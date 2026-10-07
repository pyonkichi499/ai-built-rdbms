//! `CREATE TABLE` / `DROP TABLE`（`m4/07-catalog-ddl.md` §5.1、§5.8）。
//!
//! PRIMARY KEY / UNIQUE が所有する索引と SERIAL / IDENTITY のシーケンスを含む。表は空なので、
//! 索引は `init_index` だけで `build` はしない。

use std::collections::HashSet;

use super::depend::default_value_depends;
use super::index::{default_key_parts, new_index_columns};
use super::sequence;
use super::{DdlCtx, RelOptTarget, validate_reloptions};
use crate::analyzer::query::{
    BoundCreateSequence, BoundCreateTable, BoundDropTable, IndexConstraintKind, SeqOwner,
};
use crate::catalog::depend::{ObjectAddress, cascade_notice, plan_drop};
use crate::catalog::naming::{choose_index_column_names, choose_index_name};
use crate::catalog::store::{
    EMPTY_INDEX_STATS, NewConstraint, NewIndex, NewTable, TableOidRequest,
};
use crate::catalog::{BoundExprSource, ColumnDef, IndexConstraintRef, IndexDef, RelKind, TableDef};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::IndexHandle;
use crate::types::Oid;

/// `CREATE TABLE` が SERIAL / IDENTITY のシーケンスについて知っておくこと。
struct SeqPlan {
    attnum: i16,
    /// SERIAL（既定値に `nextval` を入れる）なら真、IDENTITY なら偽。
    serial_default: bool,
    oid: Oid,
}

#[allow(clippy::too_many_lines, clippy::single_match_else)]
pub fn create_table(ctx: &mut DdlCtx<'_>, b: &BoundCreateTable) -> Result<String> {
    // 1.
    validate_reloptions(RelOptTarget::Table, &b.options)?;
    for c in &b.constraints {
        validate_reloptions(RelOptTarget::Index, &c.options)?;
    }
    // 2.
    let snap = ctx.snapshot;
    let nsp = ctx
        .db
        .catalog
        .namespace_oid(snap, &b.schema)?
        .ok_or_else(|| {
            Error::new(
                sqlstate::INVALID_SCHEMA_NAME,
                format!("schema \"{}\" does not exist", b.schema),
            )
        })?;
    // 3.
    if ctx
        .catalog
        .relation_kind(Some(&b.schema), &b.name)?
        .is_some()
    {
        if b.if_not_exists {
            ctx.notice(
                Severity::Notice,
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists, skipping", b.name),
                None,
            );
            return Ok("CREATE TABLE".into());
        }
        return Err(Error::new(
            sqlstate::DUPLICATE_TABLE,
            format!("relation \"{}\" already exists", b.name),
        ));
    }
    // 4.
    let alloc = ctx.cluster.oid_allocator();
    let serial_attnums: Vec<i16> = b
        .sequences
        .iter()
        .filter_map(|s| match &s.owner {
            SeqOwner::NewTableColumn {
                attnum,
                serial_default: true,
            } => Some(*attnum),
            _ => None,
        })
        .collect();
    let n_attrdefs = b
        .columns
        .iter()
        .filter(|c| c.default.is_some() || serial_attnums.contains(&c.attnum))
        .count();
    let oids = ctx.db.catalog.allocate_table_oids(
        alloc,
        &TableOidRequest {
            n_attrdefs,
            n_checks: b.checks.len(),
            n_index_constraints: b.constraints.len(),
        },
    )?;
    // 4b. シーケンスの OID と、列の補正（SERIAL の既定値、NOT NULL）。
    let seq_oids = b
        .sequences
        .iter()
        .map(|_| ctx.db.catalog.get_new_relation_oid(alloc))
        .collect::<Result<Vec<_>>>()?;
    let mut columns = b.columns.clone();
    for col in &mut columns {
        fold_datetime_default(ctx, col)?;
    }
    let seq_plans = apply_sequence_owners(&mut columns, &b.sequences, &seq_oids)?;
    // 5.
    let w = ctx.write_ctx()?;
    // 6.
    for (s, plan) in b.sequences.iter().zip(&seq_plans) {
        sequence::create_with_oid(ctx, s, plan.oid, Some((oids.table, plan.attnum)))?;
    }
    // 7.
    let table_locator = ctx.locator_for(oids.table);
    ctx.create_file(&w, table_locator)?;
    // 8.
    let provisional = TableDef {
        oid: oids.table,
        namespace: nsp,
        schema: b.schema.clone(),
        name: b.name.clone(),
        kind: RelKind::Table,
        locator: table_locator,
        columns: columns.clone(),
        checks: b.checks.clone(),
        indexes: Vec::new(),
        sequence: None,
        identity_seqs: Vec::new(),
    };
    // 9.
    let mut taken: HashSet<String> = HashSet::new();
    taken.insert(b.name.clone());
    taken.extend(b.sequences.iter().map(|s| s.name.clone()));
    taken.extend(b.checks.iter().map(|c| c.name.clone()));
    let mut new_indexes = Vec::with_capacity(b.constraints.len());
    for (con, &(index_oid, constraint_oid)) in b.constraints.iter().zip(&oids.index_constraints) {
        let primary = con.kind == IndexConstraintKind::PrimaryKey;
        let parts = default_key_parts(&columns, &con.columns)?;
        let columns_new = new_index_columns(&parts);
        let name = match &con.name {
            Some(n) => {
                if taken.contains(n) || ctx.catalog.relation_kind(Some(&b.schema), n)?.is_some() {
                    return Err(Error::new(
                        sqlstate::DUPLICATE_TABLE,
                        format!("relation \"{n}\" already exists"),
                    ));
                }
                n.clone()
            }
            None => {
                let key_names: Vec<String> = parts.iter().map(|p| p.colname.clone()).collect();
                let key_names: Vec<&str> = key_names.iter().map(String::as_str).collect();
                choose_index_name(
                    &b.name,
                    nsp,
                    &choose_index_column_names(&key_names),
                    primary,
                    true,
                    &ctx.name_lookup(),
                    &taken,
                )?
            }
        };
        taken.insert(name.clone());
        let def = IndexDef {
            oid: index_oid,
            name: name.clone(),
            namespace: nsp,
            table_oid: oids.table,
            locator: ctx.locator_for(index_oid),
            columns: parts.iter().map(|p| p.column.clone()).collect(),
            unique: true,
            primary,
            constraint: Some(IndexConstraintRef {
                oid: constraint_oid,
                name: name.clone(),
            }),
        };
        ctx.create_file(&w, def.locator)?;
        ctx.index_store()
            .init_index(&w, &IndexHandle::from_def(&def, &provisional))?;
        new_indexes.push(NewIndex {
            oid: index_oid,
            name: name.clone(),
            namespace: nsp,
            owner: ctx.role_oid,
            table_oid: oids.table,
            relfilenode: index_oid,
            columns: columns_new,
            unique: true,
            primary,
            constraint: Some(NewConstraint {
                oid: constraint_oid,
                name,
            }),
            stats: EMPTY_INDEX_STATS,
        });
    }
    // 10.
    let attrdef_pairs: Vec<(i16, Oid)> = columns
        .iter()
        .filter(|c| c.default.is_some())
        .map(|c| c.attnum)
        .zip(oids.attrdefs.iter().copied())
        .collect();
    let serial_pairs: Vec<(i16, Oid)> = seq_plans
        .iter()
        .filter(|p| p.serial_default)
        .map(|p| (p.attnum, p.oid))
        .collect();
    let extra_depends = default_value_depends(&attrdef_pairs, &serial_pairs, &b.default_refs);
    // 11.
    ctx.db.catalog.create_table(
        &w,
        snap,
        &NewTable {
            oid: oids.table,
            namespace: nsp,
            name: b.name.clone(),
            owner: ctx.role_oid,
            columns,
            checks: b.checks.clone(),
            attrdef_oids: oids.attrdefs,
            constraint_oids: oids.checks,
            indexes: new_indexes,
            extra_depends,
        },
    )?;
    // 12.
    ctx.mark_catalog_dirty();
    Ok("CREATE TABLE".into())
}

/// SERIAL / IDENTITY の列の補正（08 §5.7 の C）: 所有される列を NOT NULL にし、SERIAL の列の既定値に
/// `nextval('<シーケンスの OID>'::regclass)` を入れる。
fn apply_sequence_owners(
    columns: &mut [ColumnDef],
    sequences: &[BoundCreateSequence],
    seq_oids: &[Oid],
) -> Result<Vec<SeqPlan>> {
    let mut plans = Vec::with_capacity(sequences.len());
    for (s, &oid) in sequences.iter().zip(seq_oids) {
        let SeqOwner::NewTableColumn {
            attnum,
            serial_default,
        } = s.owner
        else {
            return Err(Error::internal(
                "a sequence of CREATE TABLE must belong to a column of the new table",
            ));
        };
        let col = columns
            .iter_mut()
            .find(|c| c.attnum == attnum)
            .ok_or_else(|| Error::internal(format!("sequence owner column {attnum} not found")))?;
        col.not_null = true;
        if serial_default {
            col.default = Some(BoundExprSource {
                expr_sql: format!("nextval('{oid}'::regclass)"),
            });
        }
        plans.push(SeqPlan {
            attnum,
            serial_default,
            oid,
        });
    }
    Ok(plans)
}

pub fn drop_table(ctx: &mut DdlCtx<'_>, b: &BoundDropTable) -> Result<String> {
    // 1.
    for name in &b.missing {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            format!("table \"{name}\" does not exist, skipping"),
            None,
        );
    }
    // 2.
    for t in &b.tables {
        if t.is_system_catalog() {
            return Err(super::index::system_catalog_error(&t.name));
        }
    }
    if b.tables.is_empty() {
        return Ok("DROP TABLE".into());
    }
    // 3.
    let roots: Vec<ObjectAddress> = b
        .tables
        .iter()
        .map(|t| ObjectAddress::relation(t.oid))
        .collect();
    let visible = ctx.catalog.visible_namespaces()?;
    let plan = plan_drop(&ctx.db.catalog, ctx.snapshot, &roots, b.behavior, &visible)?;
    // 4.
    if let Some((message, detail)) = cascade_notice(&plan) {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            message,
            detail,
        );
    }
    // 5.
    let w = ctx.write_ctx()?;
    let locators = ctx.db.catalog.drop_objects(&w, ctx.snapshot, &plan)?;
    // 6.
    ctx.schedule_unlinks(&locators)?;
    // 7.
    ctx.mark_catalog_dirty();
    Ok("DROP TABLE".into())
}

/// 日時型の列の `DEFAULT '文字列'` を、CREATE 時の DateStyle・TimeZone・`now` で定数に畳み、
/// `'<ISO 出力>'::<型名>` の形で保存する（PG は unknown リテラルを即座に coerce して Const にする。
/// `'today'` や `'now'` が CREATE 時点の値に固定されるのもこのため）。
fn fold_datetime_default(ctx: &DdlCtx<'_>, col: &mut ColumnDef) -> Result<()> {
    use crate::sql::ast::{Expr, Literal};
    use crate::types::{TypeEnv, io, oid};
    if !matches!(col.ty.oid, oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ) {
        return Ok(());
    }
    let (Some(def), Some(dt)) = (&col.default, ctx.type_env.datetime) else {
        return Ok(());
    };
    let Ok(Expr::Literal {
        value: Literal::String(s),
        ..
    }) = crate::sql::parse_expr(&def.expr_sql)
    else {
        return Ok(());
    };
    let datum = io::input_text_typed(&s, col.ty, ctx.type_env)?;
    let iso = TypeEnv {
        datetime: Some(yuzhu_datetime::DateTimeEnv {
            date_style: yuzhu_datetime::DateStyle::Iso,
            date_order: yuzhu_datetime::DateOrder::Ymd,
            ..dt
        }),
        ..*ctx.type_env
    };
    let Some(out) = io::output_text_env(&datum, col.ty, &iso) else {
        return Ok(());
    };
    col.default = Some(BoundExprSource {
        expr_sql: format!(
            "'{}'::{}",
            out.replace('\'', "''"),
            crate::catalog::builtin::format_type_name(col.ty.oid, Some(col.ty.typmod))
        ),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::BoundDdl;
    use crate::catalog::CatalogReader;
    use crate::catalog::depend::DropBehavior;
    use crate::catalog::{ConstraintKind, TableDef};
    use crate::ddl::testkit::{Harness, check_def, col, create_table_ddl, drop_ddl, key};
    use crate::types::SqlType;
    use std::sync::Arc;

    fn pk_unique_table(name: &str) -> BoundCreateTable {
        let mut a = col("id", 1, SqlType::INT4);
        a.not_null = true;
        let mut c = col("n", 3, SqlType::INT4);
        c.default = Some(BoundExprSource {
            expr_sql: "0".into(),
        });
        let mut b = create_table_ddl(name, vec![a, col("name", 2, SqlType::TEXT), c]);
        b.checks = vec![check_def(&format!("{name}_n_check"), "n > 0")];
        b.constraints = vec![
            key(IndexConstraintKind::PrimaryKey, None, &[1]),
            key(IndexConstraintKind::Unique, None, &[2]),
        ];
        b
    }

    fn create(h: &mut Harness, b: BoundCreateTable) -> Arc<TableDef> {
        let name = b.name.clone();
        assert_eq!(h.exec_w(BoundDdl::CreateTable(b)).unwrap(), "CREATE TABLE");
        h.table(&name).unwrap()
    }

    #[test]
    fn create_table_with_keys_checks_and_defaults() {
        let mut h = Harness::new();
        create(&mut h, pk_unique_table("t"));
        h.commit();
        let t = h.table("t").unwrap();
        assert_eq!(t.columns.len(), 3);
        assert_eq!(t.columns[2].default.as_ref().unwrap().expr_sql, "0");
        assert_eq!(t.checks.len(), 1);
        let names: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["t_pkey", "t_name_key"]);
        assert!(t.indexes[0].primary && t.indexes[0].unique);
        assert!(!t.indexes[1].primary && t.indexes[1].unique);
        for i in &t.indexes {
            assert_eq!(i.constraint.as_ref().unwrap().name, i.name);
            assert!(h.file_exists(i.locator));
            assert_eq!(i.namespace, t.namespace);
        }
        assert!(h.file_exists(t.locator));
        // pg_constraint の行（p と u と c）。
        let c = |oid| h.reader(|c| c.constraint_by_oid(oid).unwrap().unwrap());
        let pk = c(t.indexes[0].constraint.as_ref().unwrap().oid);
        assert_eq!(pk.kind, ConstraintKind::PrimaryKey);
        assert_eq!(pk.columns, vec![1]);
        assert_eq!(pk.index_oid, Some(t.indexes[0].oid));
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        let row = h.db.catalog.relation_row(&snap, t.oid).unwrap().unwrap();
        assert!(row.has_index);
        assert_eq!(row.owner, h.role);
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn create_table_without_keys_has_no_index_flag() {
        let mut h = Harness::new();
        let t = create(
            &mut h,
            create_table_ddl("plain", vec![col("a", 1, SqlType::INT4)]),
        );
        h.commit();
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        assert!(
            !h.db
                .catalog
                .relation_row(&snap, t.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        assert!(t.indexes.is_empty());
    }

    #[test]
    fn rollback_removes_every_file_and_row() {
        let mut h = Harness::new();
        h.begin();
        let t = create(&mut h, pk_unique_table("t"));
        let files = h.txn.pending_creates.clone();
        assert_eq!(files.len(), 3, "table + 2 indexes");
        assert!(files.contains(&t.locator));
        h.rollback();
        assert!(files.iter().all(|l| h.file_removed(*l)));
        assert!(h.table("t").is_none());
        assert!(h.relation_kind("t_pkey").is_none());
        assert!(h.check().is_empty());
    }

    #[test]
    fn names_follow_postgresql() {
        let mut h = Harness::new();
        // 同じ名前のリレーションがすでにある: t3_a_key1。
        create(
            &mut h,
            create_table_ddl("t3_a_key", vec![col("x", 1, SqlType::INT4)]),
        );
        let mut b = create_table_ddl("t3", vec![col("a", 1, SqlType::INT4)]);
        b.constraints = vec![key(IndexConstraintKind::Unique, None, &[1])];
        let t3 = create(&mut h, b);
        assert_eq!(t3.indexes[0].name, "t3_a_key1");
        // 同じ文の CHECK の名前とも衝突しない。
        let mut b = create_table_ddl("t4", vec![col("a", 1, SqlType::INT4)]);
        b.checks = vec![check_def("t4_a_key", "a > 0")];
        b.constraints = vec![key(IndexConstraintKind::Unique, None, &[1])];
        let t4 = create(&mut h, b);
        assert_eq!(t4.indexes[0].name, "t4_a_key1");
        assert_eq!(t4.indexes[0].constraint.as_ref().unwrap().name, "t4_a_key1");
        // 複数の制約: t2_pkey、t2_b_c_key、t2_a_key。
        let mut b = create_table_ddl(
            "t2",
            vec![
                col("a", 1, SqlType::INT4),
                col("b", 2, SqlType::INT4),
                col("c", 3, SqlType::TEXT),
            ],
        );
        b.constraints = vec![
            key(IndexConstraintKind::PrimaryKey, None, &[1, 2]),
            key(IndexConstraintKind::Unique, None, &[2, 3]),
            key(IndexConstraintKind::Unique, None, &[1]),
        ];
        let t2 = create(&mut h, b);
        let names: Vec<&str> = t2.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["t2_pkey", "t2_b_c_key", "t2_a_key"]);
        assert_eq!(t2.indexes[0].columns.len(), 2);
        h.commit();
        assert!(h.check().is_empty());
    }

    #[test]
    fn explicit_constraint_names_must_be_free() {
        let mut h = Harness::new();
        let mut b = create_table_ddl("t5", vec![col("a", 1, SqlType::INT4)]);
        b.constraints = vec![key(IndexConstraintKind::Unique, Some("x"), &[1])];
        let t5 = create(&mut h, b);
        assert_eq!(t5.indexes[0].name, "x");
        let mut b = create_table_ddl("t6", vec![col("a", 1, SqlType::INT4)]);
        b.constraints = vec![key(IndexConstraintKind::Unique, Some("x"), &[1])];
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        assert_eq!(e.message, "relation \"x\" already exists");
        // 同じ文の 2 つの制約が同じ名前。
        let mut b = create_table_ddl(
            "t7",
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::INT4)],
        );
        b.constraints = vec![
            key(IndexConstraintKind::Unique, Some("c1"), &[1]),
            key(IndexConstraintKind::Unique, Some("c1"), &[2]),
        ];
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.message, "relation \"c1\" already exists");
        // 表と同じ名前の制約。
        let mut b = create_table_ddl("t8", vec![col("a", 1, SqlType::INT4)]);
        b.constraints = vec![key(IndexConstraintKind::Unique, Some("t8"), &[1])];
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        h.rollback();
    }

    #[test]
    fn existing_table_and_missing_schema() {
        let mut h = Harness::new();
        create(
            &mut h,
            create_table_ddl("e1", vec![col("a", 1, SqlType::INT4)]),
        );
        let e = h
            .exec(BoundDdl::CreateTable(create_table_ddl(
                "e1",
                vec![col("a", 1, SqlType::INT4)],
            )))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        assert_eq!(e.message, "relation \"e1\" already exists");
        let mut b = create_table_ddl("e1", vec![col("a", 1, SqlType::INT4)]);
        b.if_not_exists = true;
        let files = h.txn.pending_creates.len();
        assert_eq!(h.exec(BoundDdl::CreateTable(b)).unwrap(), "CREATE TABLE");
        assert_eq!(
            h.notices.last().unwrap().message,
            "relation \"e1\" already exists, skipping"
        );
        assert_eq!(h.txn.pending_creates.len(), files, "nothing created");
        let mut b = create_table_ddl("e2", vec![col("a", 1, SqlType::INT4)]);
        b.schema = "nosuch".into();
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_SCHEMA_NAME);
        assert_eq!(e.message, "schema \"nosuch\" does not exist");
        // 索引の名前とも衝突する。
        let mut b = create_table_ddl("e3", vec![col("a", 1, SqlType::INT4)]);
        b.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1])];
        create(&mut h, b);
        let e = h
            .exec(BoundDdl::CreateTable(create_table_ddl(
                "e3_pkey",
                vec![col("a", 1, SqlType::INT4)],
            )))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        h.rollback();
    }

    #[test]
    fn reloptions_are_validated_for_tables_and_keys() {
        let mut h = Harness::new();
        let mut b = create_table_ddl("r1", vec![col("a", 1, SqlType::INT4)]);
        b.options = vec![crate::analyzer::query::RelOption {
            namespace: None,
            name: "foo".into(),
            value: Some("1".into()),
        }];
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        let mut b = create_table_ddl("r2", vec![col("a", 1, SqlType::INT4)]);
        let mut k = key(IndexConstraintKind::Unique, None, &[1]);
        k.options = vec![crate::analyzer::query::RelOption {
            namespace: None,
            name: "fillfactor".into(),
            value: Some("5".into()),
        }];
        b.constraints = vec![k];
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.message, "value 5 out of bounds for option \"fillfactor\"");
        let mut b = create_table_ddl("r3", vec![col("a", 1, SqlType::INT4)]);
        b.options = vec![crate::analyzer::query::RelOption {
            namespace: None,
            name: "fillfactor".into(),
            value: Some("70".into()),
        }];
        create(&mut h, b);
        h.rollback();
    }

    #[test]
    fn a_key_on_a_type_without_opclass_fails_after_creating_files() {
        let mut h = Harness::new();
        let mut b = create_table_ddl("x1", vec![col("a", 1, SqlType::of(crate::types::oid::XID))]);
        b.constraints = vec![key(IndexConstraintKind::Unique, None, &[1])];
        h.begin();
        let e = h.exec(BoundDdl::CreateTable(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        let files = h.txn.pending_creates.clone();
        h.rollback();
        assert!(
            files.iter().all(|l| h.file_removed(*l)),
            "the abort removes the files"
        );
        assert!(h.table("x1").is_none());
    }

    #[test]
    fn drop_table_removes_everything_at_commit() {
        let mut h = Harness::new();
        let t = create(&mut h, pk_unique_table("t"));
        h.commit();
        let files: Vec<_> = std::iter::once(t.locator)
            .chain(t.indexes.iter().map(|i| i.locator))
            .collect();
        assert!(files.iter().all(|l| h.file_exists(*l)));
        h.begin();
        assert_eq!(
            h.exec(drop_ddl(vec![Arc::clone(&t)], DropBehavior::Restrict))
                .unwrap(),
            "DROP TABLE"
        );
        assert!(h.notices.is_empty(), "owned objects go quietly");
        // コミットまでファイルは残る。
        assert!(files.iter().all(|l| h.file_exists(*l)));
        h.commit();
        assert!(files.iter().all(|l| h.file_removed(*l)));
        assert!(h.table("t").is_none());
        assert!(h.relation_kind("t_pkey").is_none());
        // 全カタログから消える。
        for oid in std::iter::once(t.oid).chain(t.indexes.iter().map(|i| i.oid)) {
            let n = h.rows_mentioning(oid);
            assert_eq!(n, 0, "rows still mention {oid}");
        }
        assert!(h.check().is_empty());
    }

    #[test]
    fn drop_table_rollback_keeps_files() {
        let mut h = Harness::new();
        let t = create(&mut h, pk_unique_table("t"));
        h.commit();
        h.begin();
        h.exec(drop_ddl(vec![Arc::clone(&t)], DropBehavior::Restrict))
            .unwrap();
        h.rollback();
        assert!(h.file_exists(t.locator));
        assert!(h.table("t").is_some());
        assert!(h.check().is_empty());
    }

    #[test]
    fn create_truncate_drop_in_one_transaction_unlinks_each_file_once() {
        let mut h = Harness::new();
        h.begin();
        let t = create(&mut h, pk_unique_table("t"));
        h.exec(BoundDdl::Truncate(crate::analyzer::query::BoundTruncate {
            tables: vec![Arc::clone(&t)],
            restart_identity: false,
            cascade: false,
        }))
        .unwrap();
        let t2 = h.table("t").unwrap();
        h.exec(drop_ddl(vec![t2], DropBehavior::Restrict)).unwrap();
        let created = h.txn.pending_creates.clone();
        h.commit();
        // 最初のファイルは pending_creates と pending_unlinks の両方に載るが、コミットでは 1 度だけ消す。
        assert!(h.table("t").is_none());
        assert!(created.iter().all(|l| h.file_removed(*l)));
        assert!(h.check().is_empty());
    }

    #[test]
    fn drop_table_restrict_and_cascade_with_default_dependencies() {
        let mut h = Harness::new();
        let o1 = create(
            &mut h,
            create_table_ddl("o1", vec![col("id", 1, SqlType::INT4)]),
        );
        let mut id_col = col("id", 1, SqlType::INT4);
        id_col.default = Some(BoundExprSource {
            expr_sql: "nextval('o1'::regclass)".into(),
        });
        let mut b = create_table_ddl("o2", vec![id_col.clone()]);
        b.default_refs = vec![(1, o1.oid)];
        let o2 = create(&mut h, b);
        let mut b = create_table_ddl("o3", vec![id_col]);
        b.default_refs = vec![(1, o1.oid)];
        let o3 = create(&mut h, b);
        let o4 = create(
            &mut h,
            create_table_ddl("o4", vec![col("id", 1, SqlType::INT4)]),
        );
        h.commit();

        h.begin();
        let err = h
            .exec(drop_ddl(vec![Arc::clone(&o1)], DropBehavior::Restrict))
            .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            err.message,
            "cannot drop table o1 because other objects depend on it"
        );
        assert_eq!(
            err.detail.as_deref(),
            Some(
                "default value for column id of table o2 depends on table o1\n\
                 default value for column id of table o3 depends on table o1"
            )
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Use DROP ... CASCADE to drop the dependent objects too.")
        );
        let err = h
            .exec(drop_ddl(
                vec![Arc::clone(&o1), Arc::clone(&o4)],
                DropBehavior::Restrict,
            ))
            .unwrap_err();
        assert_eq!(
            err.message,
            "cannot drop desired object(s) because other objects depend on them"
        );
        // 依存元も一緒に消すなら通る。
        h.exec(drop_ddl(
            vec![Arc::clone(&o1), Arc::clone(&o2), Arc::clone(&o3)],
            DropBehavior::Restrict,
        ))
        .unwrap();
        h.rollback();

        h.begin();
        h.notices.clear();
        h.exec(drop_ddl(vec![Arc::clone(&o1)], DropBehavior::Cascade))
            .unwrap();
        assert_eq!(h.notices.len(), 1);
        assert_eq!(h.notices[0].message, "drop cascades to 2 other objects");
        assert_eq!(
            h.notices[0].detail.as_deref(),
            Some(
                "drop cascades to default value for column id of table o2\n\
                 drop cascades to default value for column id of table o3"
            )
        );
        h.commit();
        assert!(h.table("o1").is_none());
        // 残った表の既定値は消える。
        for name in ["o2", "o3"] {
            let t = h.table(name).unwrap();
            assert!(t.columns[0].default.is_none(), "{name}");
        }
        assert!(h.check().is_empty(), "{:?}", h.check());
        let _ = (o2, o3);
    }

    fn serial_sequence(attnum: i16, serial_default: bool) -> BoundCreateSequence {
        BoundCreateSequence {
            schema: "public".into(),
            namespace: 2200,
            name: format!("s{attnum}"),
            if_not_exists: false,
            params: crate::catalog::SequenceParams {
                type_oid: 23,
                start: 1,
                increment: 1,
                min: 1,
                max: i64::from(i32::MAX),
                cache: 1,
                cycle: false,
                owned_by: None,
            },
            initial: crate::storage::SeqState {
                last_value: 1,
                log_cnt: 0,
                is_called: false,
            },
            owner: SeqOwner::NewTableColumn {
                attnum,
                serial_default,
            },
            for_identity: !serial_default,
        }
    }

    #[test]
    fn sequence_owners_fix_up_the_columns() {
        let mut columns = vec![
            col("id", 1, SqlType::INT4),
            col("gen", 2, SqlType::INT4),
            col("x", 3, SqlType::INT4),
        ];
        let seqs = [serial_sequence(1, true), serial_sequence(2, false)];
        let plans = apply_sequence_owners(&mut columns, &seqs, &[16500, 16501]).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(
            (plans[0].attnum, plans[0].serial_default, plans[0].oid),
            (1, true, 16500)
        );
        assert!(!plans[1].serial_default);
        assert!(columns[0].not_null && columns[1].not_null && !columns[2].not_null);
        assert_eq!(
            columns[0].default.as_ref().unwrap().expr_sql,
            "nextval('16500'::regclass)"
        );
        assert!(columns[1].default.is_none(), "IDENTITY has no default");
        assert!(columns[2].default.is_none());
        // 存在しない列、所有者の種類が違うもの。
        let bad = [serial_sequence(9, true)];
        assert!(apply_sequence_owners(&mut columns, &bad, &[1]).is_err());
        let mut other = serial_sequence(1, true);
        other.owner = SeqOwner::None;
        assert!(apply_sequence_owners(&mut columns, &[other], &[1]).is_err());
    }

    /// SERIAL のシーケンスを（`ddl::sequence` を使わずに）カタログへ直接書いて、DROP TABLE が静かに
    /// 一緒に消すことを確かめる。
    #[test]
    fn drop_table_takes_an_owned_sequence_along() {
        use crate::catalog::depend::DependType;
        use crate::catalog::store::NewSequence;
        let mut h = Harness::new();
        let t = create(
            &mut h,
            create_table_ddl("sr1", vec![col("id", 1, SqlType::INT4)]),
        );
        let seq_loc = h
            .run(|ctx| {
                let w = ctx.write_ctx()?;
                let oid = ctx
                    .db
                    .catalog
                    .get_new_relation_oid(ctx.cluster.oid_allocator())?;
                let loc = ctx.locator_for(oid);
                ctx.create_file(&w, loc)?;
                ctx.db.catalog.create_sequence(
                    &w,
                    ctx.snapshot,
                    &NewSequence {
                        oid,
                        name: "sr1_id_seq".into(),
                        namespace: 2200,
                        owner: ctx.role_oid,
                        params: crate::catalog::SequenceParams {
                            type_oid: 23,
                            start: 1,
                            increment: 1,
                            min: 1,
                            max: i64::from(i32::MAX),
                            cache: 1,
                            cycle: false,
                            owned_by: Some((t.oid, 1)),
                        },
                        owned_by_deptype: Some(DependType::Auto),
                    },
                )?;
                ctx.mark_catalog_dirty();
                Ok(loc)
            })
            .unwrap();
        h.commit();
        let (seq_oid, kind) = h.relation_kind("sr1_id_seq").unwrap();
        assert_eq!(kind, RelKind::Sequence);
        assert!(h.file_exists(seq_loc));
        h.begin();
        h.notices.clear();
        h.exec(drop_ddl(vec![Arc::clone(&t)], DropBehavior::Restrict))
            .unwrap();
        assert!(h.notices.is_empty());
        assert!(h.txn.pending_unlinks.contains(&seq_loc));
        assert!(h.txn.pending_unlinks.contains(&t.locator));
        h.commit();
        assert!(h.relation_kind("sr1_id_seq").is_none());
        assert_eq!(h.rows_mentioning(seq_oid), 0);
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn drop_table_reports_missing_names_and_protects_system_catalogs() {
        let mut h = Harness::new();
        h.begin();
        let r = h
            .exec(BoundDdl::DropTable(
                crate::analyzer::query::BoundDropTable {
                    tables: vec![],
                    missing: vec!["nosuch".into()],
                    behavior: DropBehavior::Restrict,
                },
            ))
            .unwrap();
        assert_eq!(r, "DROP TABLE");
        assert_eq!(
            h.notices[0].message,
            "table \"nosuch\" does not exist, skipping"
        );
        assert_eq!(h.notices[0].sqlstate, sqlstate::SUCCESSFUL_COMPLETION);
        let pg_class = h.reader(|c| c.table(Some("pg_catalog"), "pg_class").unwrap().unwrap());
        let e = h
            .exec(drop_ddl(vec![pg_class], DropBehavior::Cascade))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(
            e.message,
            "permission denied: \"pg_class\" is a system catalog"
        );
        h.rollback();
    }
}
