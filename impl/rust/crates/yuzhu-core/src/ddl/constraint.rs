//! `ALTER TABLE ... ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE` と `OWNER TO`
//! （`m4/07-catalog-ddl.md` §5.4、§5.5）。

use std::collections::HashSet;

use super::index::{
    build_from_heap, default_key_parts, new_index_columns, system_catalog_error, table_owner,
};
use super::{DdlCtx, RelOptTarget, validate_reloptions};
use crate::analyzer::ddl_ext::foreign_key::{BoundFkParent, BoundForeignKey};
use crate::analyzer::query::{
    AlterTarget, BoundAlterTableAddCheck, BoundAlterTableAddConstraint, BoundAlterTableOwner,
    IndexConstraintKind, OwnerSpec,
};
use crate::catalog::fk::ForeignKeyDef;
use crate::catalog::naming::{choose_index_column_names, choose_index_name};
use crate::catalog::schema::oids;
use crate::catalog::store::{ClassPatch, NewConstraint, NewIndex};
use crate::catalog::{CatalogReader, CheckDef, IndexConstraintRef, IndexDef, TableDef};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::RelHandle;
use crate::types::Oid;
use std::sync::Arc;

fn missing_table_notice(ctx: &mut DdlCtx<'_>, name: &str) {
    ctx.notice(
        Severity::Notice,
        sqlstate::UNDEFINED_TABLE,
        format!("relation \"{name}\" does not exist, skipping"),
        None,
    );
}

#[allow(clippy::too_many_lines, clippy::single_match_else)]
pub fn add_constraint(ctx: &mut DdlCtx<'_>, b: &BoundAlterTableAddConstraint) -> Result<String> {
    // 0.
    let t = match &b.target {
        AlterTarget::Missing(name) => {
            missing_table_notice(ctx, name);
            return Ok("ALTER TABLE".into());
        }
        AlterTarget::Found(t) => t,
    };
    // 1.
    if t.is_system_catalog() {
        return Err(system_catalog_error(&t.name));
    }
    // 2.
    let c = &b.constraint;
    validate_reloptions(RelOptTarget::Index, &c.options)?;
    // 3.
    let primary = c.kind == IndexConstraintKind::PrimaryKey;
    if primary && t.primary_key().is_some() {
        return Err(Error::new(
            sqlstate::INVALID_TABLE_DEFINITION,
            format!(
                "multiple primary keys for table \"{}\" are not allowed",
                t.name
            ),
        ));
    }
    // 4.
    let key_names: Vec<&str> = c
        .columns
        .iter()
        .map(|a| {
            t.columns
                .iter()
                .find(|col| col.attnum == *a)
                .map(|col| col.name.as_str())
                .ok_or_else(|| Error::internal(format!("constraint column {a} not found")))
        })
        .collect::<Result<_>>()?;
    let name = match &c.name {
        Some(n) => {
            if ctx.catalog.relation_kind(Some(&t.schema), n)?.is_some() {
                return Err(Error::new(
                    sqlstate::DUPLICATE_TABLE,
                    format!("relation \"{n}\" already exists"),
                ));
            }
            if ctx
                .db
                .catalog
                .constraint_names_of(ctx.snapshot, t.oid)?
                .iter()
                .any(|x| x == n)
            {
                return Err(Error::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!(
                        "constraint \"{n}\" for relation \"{}\" already exists",
                        t.name
                    ),
                ));
            }
            n.clone()
        }
        None => choose_index_name(
            &t.name,
            t.namespace,
            &choose_index_column_names(&key_names),
            primary,
            true,
            &ctx.name_lookup(),
            &HashSet::new(),
        )?,
    };
    // 5.
    let parts = default_key_parts(&t.columns, &c.columns)?;
    // 6.
    let alloc = ctx.cluster.oid_allocator();
    let index_oid = ctx.db.catalog.get_new_relation_oid(alloc)?;
    let constraint_oid = ctx.db.catalog.get_new_oid(alloc, oids::PG_CONSTRAINT)?;
    let locator = ctx.locator_for(index_oid);
    let def = IndexDef {
        oid: index_oid,
        name: name.clone(),
        namespace: t.namespace,
        table_oid: t.oid,
        locator,
        columns: parts.iter().map(|p| p.column.clone()).collect(),
        unique: true,
        primary,
        constraint: Some(IndexConstraintRef {
            oid: constraint_oid,
            name: name.clone(),
        }),
    };
    let handle = crate::storage::IndexHandle::from_def(&def, t);
    let owner = table_owner(ctx, t)?;
    // 7.
    let w = ctx.write_ctx()?;
    ctx.create_file(&w, locator)?;
    ctx.index_store().init_index(&w, &handle)?;
    // 8.
    let stats = build_from_heap(ctx, &w, t, &def, &handle)?;
    // 9.
    if primary {
        check_not_null(ctx, t, &c.columns)?;
    }
    // 10.
    ctx.db.catalog.add_constraint(
        &w,
        ctx.snapshot,
        &NewIndex {
            oid: index_oid,
            name: name.clone(),
            namespace: t.namespace,
            owner,
            table_oid: t.oid,
            relfilenode: index_oid,
            columns: new_index_columns(&parts),
            unique: true,
            primary,
            constraint: Some(NewConstraint {
                oid: constraint_oid,
                name,
            }),
            stats,
        },
        t,
    )?;
    // 11.
    ctx.mark_catalog_dirty();
    Ok("ALTER TABLE".into())
}

/// PRIMARY KEY のキー列のうち、まだ `attnotnull` でない列に NULL がないかを 1 回の走査で調べる（D07-10）。
fn check_not_null(ctx: &DdlCtx<'_>, t: &crate::catalog::TableDef, key: &[i16]) -> Result<()> {
    let pending: Vec<(usize, &str)> = key
        .iter()
        .filter_map(|a| t.columns.iter().find(|c| c.attnum == *a && !c.not_null))
        .map(|c| (usize::try_from(c.attnum - 1).unwrap_or(0), c.name.as_str()))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let rel = RelHandle::from_table(t);
    let heap = ctx.heap();
    let mut scan = heap.begin_scan(&rel, ctx.snapshot)?;
    while let Some(tuple) = heap.scan_next(&mut scan)? {
        ctx.check_interrupts()?;
        for (i, name) in &pending {
            if tuple.row.get(*i).is_none_or(crate::types::Datum::is_null) {
                return Err(Error::new(
                    sqlstate::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{name}\" of relation \"{}\" contains null values",
                        t.name
                    ),
                )
                .with_table(t.schema.clone(), t.name.clone())
                .with_column((*name).to_owned()));
            }
        }
    }
    Ok(())
}

/// `ALTER TABLE ... ADD CHECK`: 既存の行をすべて検査してから `pg_constraint` に書く。
pub fn add_check(ctx: &mut DdlCtx<'_>, b: &BoundAlterTableAddCheck) -> Result<String> {
    let t = match &b.target {
        AlterTarget::Missing(name) => {
            missing_table_notice(ctx, name);
            return Ok("ALTER TABLE".into());
        }
        AlterTarget::Found(t) => t,
    };
    if t.is_system_catalog() {
        return Err(system_catalog_error(&t.name));
    }
    let expr = b
        .expr
        .as_ref()
        .ok_or_else(|| Error::internal("ADD CHECK without an analyzed expression"))?;
    let phys = crate::expr::walk::lower_single_rel(expr)?;
    let violated = || {
        Error::new(
            sqlstate::CHECK_VIOLATION,
            format!(
                "check constraint \"{}\" of relation \"{}\" is violated by some row",
                b.name, t.name
            ),
        )
        .with_table(t.schema.clone(), t.name.clone())
        .with_constraint(b.name.clone())
    };
    {
        let session = crate::executor::SessionInfo {
            current_user: "postgres".into(),
            session_user: "postgres".into(),
            database: "postgres".into(),
            current_schema: Some(t.schema.clone()),
        };
        let runtime = crate::executor::NullRuntime;
        let ectx = crate::executor::EvalCtx {
            session: &session,
            catalog: ctx.catalog,
            runtime: &runtime,
            type_env: ctx.type_env,
        };
        let rel = RelHandle::from_table(t);
        let heap = ctx.heap();
        let mut scan = heap.begin_scan(&rel, ctx.snapshot)?;
        while let Some(tuple) = heap.scan_next(&mut scan)? {
            ctx.check_interrupts()?;
            // NULL は違反ではない（CHECK は false だけを拒否する）。
            if crate::executor::eval::eval_const_pred(&phys, &tuple.row, &ectx)? == Some(false) {
                return Err(violated());
            }
        }
    }
    let alloc = ctx.cluster.oid_allocator();
    let oid = ctx.db.catalog.get_new_oid(alloc, oids::PG_CONSTRAINT)?;
    let w = ctx.write_ctx()?;
    ctx.db.catalog.add_check(
        &w,
        ctx.snapshot,
        t,
        oid,
        &CheckDef {
            name: b.name.clone(),
            expr_sql: b.expr_sql.clone(),
            no_inherit: b.no_inherit,
        },
    )?;
    ctx.mark_catalog_dirty();
    Ok("ALTER TABLE".into())
}

pub fn alter_owner(ctx: &mut DdlCtx<'_>, b: &BoundAlterTableOwner) -> Result<String> {
    // 0.
    let t = match &b.target {
        AlterTarget::Missing(name) => {
            missing_table_notice(ctx, name);
            return Ok("ALTER TABLE".into());
        }
        AlterTarget::Found(t) => t,
    };
    // 1.
    let new_owner = match &b.new_owner {
        OwnerSpec::Name(n) => {
            ctx.db
                .shared
                .role_by_name(ctx.snapshot, n)?
                .ok_or_else(|| {
                    Error::new(
                        sqlstate::UNDEFINED_OBJECT,
                        format!("role \"{n}\" does not exist"),
                    )
                })?
                .oid
        }
        OwnerSpec::CurrentUser | OwnerSpec::SessionUser => ctx.role_oid,
    };
    // 2.
    if t.is_system_catalog() {
        return Err(system_catalog_error(&t.name));
    }
    // 3. 表、索引、表が所有するシーケンス。
    let mut targets = vec![t.oid];
    targets.extend(t.indexes.iter().map(|i| i.oid));
    for seq in ctx.db.catalog.owned_sequences(ctx.snapshot, t.oid)? {
        if !targets.contains(&seq) {
            targets.push(seq);
        }
    }
    let mut changed = false;
    for oid in targets {
        let Some(row) = ctx.db.catalog.relation_row(ctx.snapshot, oid)? else {
            return Err(Error::internal(format!(
                "relation {oid} has no pg_class row"
            )));
        };
        if row.owner == new_owner {
            continue;
        }
        let w = ctx.write_ctx()?;
        ctx.db.catalog.update_class_row(
            &w,
            ctx.snapshot,
            oid,
            &ClassPatch {
                relowner: Some(new_owner),
                ..ClassPatch::default()
            },
        )?;
        changed = true;
    }
    // 4.
    if changed {
        ctx.mark_catalog_dirty();
    }
    Ok("ALTER TABLE".into())
}

/// `create_foreign_key` の親の解決結果（07 §4.6）。
#[derive(Clone, Copy, Debug)]
pub enum ResolvedParent<'a> {
    Existing(&'a TableDef),
    /// 作成中の表自身（`child` の `indexes` に作成済みの PK / UNIQUE の索引が入っていること）。
    SelfTable,
}

/// `parent_cols` の attnum の集合に一致する一意インデックス（部分・式でない。OID 昇順で最初）。
fn matching_unique_index(parent: &TableDef, parent_cols: &[i16]) -> Option<Oid> {
    let want: std::collections::BTreeSet<i16> = parent_cols.iter().copied().collect();
    parent
        .indexes
        .iter()
        .filter(|i| i.unique && i.columns.len() == want.len())
        .filter(|i| {
            i.columns
                .iter()
                .map(|c| c.attnum)
                .collect::<std::collections::BTreeSet<_>>()
                == want
        })
        .map(|i| i.oid)
        .min()
}

/// `CREATE TABLE` と `ALTER TABLE ADD FOREIGN KEY` の共通部分。`pg_constraint` と `pg_depend` の行を書き、
/// カタログのキャッシュの無効化を予約する（07 §6.6）。
///
/// `check_existing`（既存の行の検査。`initial_check`）は RI エンジン（FK-2）の完成まで未対応で、真なら 0A000。
pub fn create_foreign_key(
    ctx: &mut DdlCtx<'_>,
    child: &TableDef,
    fk: &BoundForeignKey,
    resolved_parent: ResolvedParent<'_>,
    check_existing: bool,
) -> Result<ForeignKeyDef> {
    if check_existing {
        return Err(Error::not_supported(
            "checking existing rows when adding a foreign key is not supported yet",
        ));
    }
    if fk.child_cols.is_empty() || fk.child_cols.len() != fk.parent_cols.len() {
        return Err(Error::new(
            sqlstate::INVALID_FOREIGN_KEY,
            "number of referencing and referenced columns for foreign key disagree",
        ));
    }
    if fk.eq_ops.len() != fk.child_cols.len() {
        return Err(Error::internal("foreign key without equality operators"));
    }
    let (parent_oid, parent_index) = match (resolved_parent, &fk.parent) {
        (ResolvedParent::Existing(p), BoundFkParent::Existing { index, .. }) => (p.oid, *index),
        (ResolvedParent::Existing(p), BoundFkParent::SelfTable { .. }) => (
            p.oid,
            matching_unique_index(p, &fk.parent_cols).ok_or_else(no_matching_unique(&p.name))?,
        ),
        (ResolvedParent::SelfTable, _) => (
            child.oid,
            matching_unique_index(child, &fk.parent_cols)
                .ok_or_else(no_matching_unique(&child.name))?,
        ),
    };
    let oid = ctx
        .db
        .catalog
        .get_new_oid(ctx.cluster.oid_allocator(), oids::PG_CONSTRAINT)?;
    let def = ForeignKeyDef {
        oid,
        name: fk.name.clone(),
        child: child.oid,
        parent: parent_oid,
        parent_index,
        child_cols: fk.child_cols.clone(),
        parent_cols: fk.parent_cols.clone(),
        match_type: fk.match_type,
        on_update: fk.on_update,
        on_delete: fk.on_delete,
        set_cols: fk.set_cols.clone(),
        eq_ops: fk.eq_ops.clone(),
    };
    let w = ctx.write_ctx()?;
    ctx.db
        .catalog
        .insert_foreign_key(&w, ctx.snapshot, &def, child.namespace)?;
    ctx.mark_catalog_dirty();
    Ok(def)
}

fn no_matching_unique(table: &str) -> impl FnOnce() -> Error + '_ {
    move || {
        Error::new(
            sqlstate::INVALID_FOREIGN_KEY,
            format!(
                "there is no unique constraint matching given keys for referenced table \"{table}\""
            ),
        )
    }
}

/// FK を 1 つ消す（`pg_constraint` と `pg_depend` の行を消し、`catalog_dirty` を立てる）。
/// CASCADE の NOTICE は呼び出し側が出す。
pub fn drop_foreign_key(ctx: &mut DdlCtx<'_>, fk: &ForeignKeyDef) -> Result<()> {
    let w = ctx.write_ctx()?;
    ctx.db
        .catalog
        .delete_foreign_key(&w, ctx.snapshot, fk.oid)?;
    ctx.mark_catalog_dirty();
    Ok(())
}

/// `dropping` を参照している FK のうち、子の表が `dropping` に含まれないもの（OID 昇順、重複なし）。
/// 空でなければ DROP TABLE は 2BP01、CASCADE なら `drop_foreign_key` で消す（07 §6.7）。
pub fn fks_blocking_drop_tables(
    ctx: &DdlCtx<'_>,
    dropping: &[Oid],
) -> Result<Vec<Arc<ForeignKeyDef>>> {
    let mut out: Vec<Arc<ForeignKeyDef>> = Vec::new();
    for &t in dropping {
        for fk in ctx.catalog.foreign_keys_referencing(t)? {
            if !dropping.contains(&fk.child) && !out.iter().any(|o| o.oid == fk.oid) {
                out.push(fk);
            }
        }
    }
    out.sort_by_key(|f| f.oid);
    Ok(out)
}

/// 一意インデックス `index`（`conindid`）に依存する FK。DROP INDEX と DROP CONSTRAINT（PK / UNIQUE）が使う。
pub fn fks_depending_on_index(ctx: &DdlCtx<'_>, index: Oid) -> Result<Vec<Arc<ForeignKeyDef>>> {
    let Some(idx) = ctx.catalog.index_by_oid(index)? else {
        return Ok(Vec::new());
    };
    Ok(ctx
        .catalog
        .foreign_keys_referencing(idx.table_oid)?
        .into_iter()
        .filter(|f| f.parent_index == index)
        .collect())
}

/// DROP の DETAIL の 1 行の前半: `constraint c_pid_fkey on table c`（`depends on ...` は呼び出し側が付ける）。
pub fn describe_dependent(fk: &ForeignKeyDef, child_name: &str) -> String {
    format!("constraint {} on table {child_name}", fk.name)
}

/// TRUNCATE の対象（`rels`）に対する拒否（07 §6.7）。最初の 1 件だけ報告する。表が空でも拒否する。
pub fn check_truncate_fks(ctx: &DdlCtx<'_>, rels: &[Oid], cascade: bool) -> Result<()> {
    for &rel in rels {
        for fk in ctx.catalog.foreign_keys_referencing(rel)? {
            if rels.contains(&fk.child) {
                continue;
            }
            if cascade {
                return Err(Error::not_supported(
                    "TRUNCATE ... CASCADE is not supported",
                ));
            }
            let name = |oid: Oid| -> Result<String> {
                Ok(ctx
                    .catalog
                    .table_by_oid(oid)?
                    .map_or_else(|| oid.to_string(), |t| t.name.clone()))
            };
            let (child, parent) = (name(fk.child)?, name(fk.parent)?);
            return Err(Error::not_supported(
                "cannot truncate a table referenced in a foreign key constraint",
            )
            .with_detail(format!("Table \"{child}\" references \"{parent}\"."))
            .with_hint(format!(
                "Truncate table \"{child}\" at the same time, or use TRUNCATE ... CASCADE."
            )));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::{BoundCreateTable, BoundDdl};
    use crate::catalog::CatalogReader;
    use crate::catalog::TableDef;
    use crate::ddl::testkit::{Harness, col, create_table_ddl, key};
    use crate::types::{Datum, SqlType};
    use std::sync::Arc;

    fn add(
        t: &Arc<TableDef>,
        kind: IndexConstraintKind,
        name: Option<&str>,
        cols: &[i16],
    ) -> BoundDdl {
        BoundDdl::AlterTableAddConstraint(BoundAlterTableAddConstraint {
            target: AlterTarget::Found(Arc::clone(t)),
            constraint: key(kind, name, cols),
        })
    }

    fn owner(t: &Arc<TableDef>, who: OwnerSpec) -> BoundDdl {
        BoundDdl::AlterTableOwner(BoundAlterTableOwner {
            target: AlterTarget::Found(Arc::clone(t)),
            new_owner: who,
        })
    }

    fn make(h: &mut Harness, ct: BoundCreateTable) -> Arc<TableDef> {
        let name = ct.name.clone();
        h.exec_w(BoundDdl::CreateTable(ct)).unwrap();
        h.table(&name).unwrap()
    }

    fn two_cols(name: &str) -> BoundCreateTable {
        create_table_ddl(
            name,
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::TEXT)],
        )
    }

    #[test]
    fn a_missing_target_is_a_notice() {
        let mut h = Harness::new();
        for ddl in [
            BoundDdl::AlterTableAddConstraint(BoundAlterTableAddConstraint {
                target: AlterTarget::Missing("nosuch".into()),
                constraint: key(IndexConstraintKind::Unique, None, &[1]),
            }),
            BoundDdl::AlterTableOwner(BoundAlterTableOwner {
                target: AlterTarget::Missing("nosuch".into()),
                new_owner: OwnerSpec::CurrentUser,
            }),
        ] {
            h.notices.clear();
            assert_eq!(h.exec(ddl).unwrap(), "ALTER TABLE");
            assert_eq!(
                h.notices[0].message,
                "relation \"nosuch\" does not exist, skipping"
            );
            assert_eq!(h.notices[0].sqlstate, sqlstate::UNDEFINED_TABLE);
        }
        assert!(h.txn.xid.is_none());
    }

    #[test]
    fn add_constraint_checks_before_it_builds() {
        let mut h = Harness::new();
        let mut ct = two_cols("a1");
        ct.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1])];
        let t = make(&mut h, ct);
        let e = h
            .exec(add(&t, IndexConstraintKind::PrimaryKey, None, &[2]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_TABLE_DEFINITION);
        assert_eq!(
            e.message,
            "multiple primary keys for table \"a1\" are not allowed"
        );
        // 索引の名前と衝突。
        let e = h
            .exec(add(&t, IndexConstraintKind::Unique, Some("a1_pkey"), &[2]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        assert_eq!(e.message, "relation \"a1_pkey\" already exists");
        // CHECK の名前と衝突（索引の名前とは別）。
        let mut ct = two_cols("cc4");
        ct.checks = vec![crate::ddl::testkit::check_def("c4", "a > 0")];
        let t4 = make(&mut h, ct);
        let e = h
            .exec(add(&t4, IndexConstraintKind::Unique, Some("c4"), &[1]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_OBJECT);
        assert_eq!(
            e.message,
            "constraint \"c4\" for relation \"cc4\" already exists"
        );
        // システムカタログ。
        let pg_class = h.reader(|c| c.table(Some("pg_catalog"), "pg_class").unwrap().unwrap());
        let e = h
            .exec(add(&pg_class, IndexConstraintKind::Unique, None, &[1]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        // 列の型に演算子クラスがない。
        let t5 = make(
            &mut h,
            create_table_ddl("x5", vec![col("x", 1, SqlType::of(crate::types::oid::XID))]),
        );
        let e = h
            .exec(add(&t5, IndexConstraintKind::Unique, None, &[1]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        // どの失敗も索引のカタログの行を残さない（ファイルは pending_creates でアボートが消す）。
        let files = h.txn.pending_creates.clone();
        h.rollback();
        assert!(files.iter().all(|l| h.file_removed(*l)));
        assert!(h.check().is_empty());
    }

    #[test]
    fn add_unique_and_primary_key_to_an_existing_table() {
        let mut h = Harness::new();
        let t = make(&mut h, two_cols("a2"));
        let rows = [
            vec![Datum::Int4(1), Datum::Text("x".into())],
            vec![Datum::Int4(2), Datum::Text("y".into())],
        ];
        h.run(|ctx| {
            let w = ctx.write_ctx()?;
            let rel = crate::storage::RelHandle::from_table(&t);
            for r in &rows {
                ctx.heap().insert(&rel, &w, r)?;
            }
            Ok(())
        })
        .unwrap();
        h.commit();
        let t = h.table("a2").unwrap();
        h.begin();
        assert_eq!(
            h.exec(add(&t, IndexConstraintKind::Unique, None, &[2]))
                .unwrap(),
            "ALTER TABLE"
        );
        // 同じ列の UNIQUE をもう一度: 統合せず `_key1`。
        h.exec(add(&t, IndexConstraintKind::Unique, None, &[2]))
            .unwrap();
        h.exec(add(&t, IndexConstraintKind::PrimaryKey, None, &[1]))
            .unwrap();
        h.commit();
        let t = h.table("a2").unwrap();
        let names: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["a2_b_key", "a2_b_key1", "a2_pkey"]);
        assert!(t.columns[0].not_null, "PRIMARY KEY sets attnotnull");
        assert!(!t.columns[1].not_null);
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        assert!(
            h.db.catalog
                .relation_row(&snap, t.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn add_primary_key_reports_duplicates_before_nulls() {
        let mut h = Harness::new();
        let t = make(&mut h, two_cols("a3"));
        let rows = [
            vec![Datum::Int4(3), Datum::Null],
            vec![Datum::Int4(3), Datum::Text("y".into())],
        ];
        h.run(|ctx| {
            let w = ctx.write_ctx()?;
            let rel = crate::storage::RelHandle::from_table(&t);
            for r in &rows {
                ctx.heap().insert(&rel, &w, r)?;
            }
            Ok(())
        })
        .unwrap();
        let t = h.table("a3").unwrap();
        let e = h
            .exec(add(&t, IndexConstraintKind::PrimaryKey, None, &[1]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(e.message, "could not create unique index \"a3_pkey\"");
        assert_eq!(e.detail.as_deref(), Some("Key (a)=(3) is duplicated."));
        // NULL だけの違反は 23502（SCHEMA / TABLE / COLUMN が付く）。
        let e = h
            .exec(add(&t, IndexConstraintKind::PrimaryKey, None, &[2]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(
            e.message,
            "column \"b\" of relation \"a3\" contains null values"
        );
        assert_eq!(e.schema(), Some("public"));
        assert_eq!(e.table(), Some("a3"));
        assert_eq!(e.column(), Some("b"));
        assert_eq!(e.constraint(), None);
        h.rollback();
        assert!(h.check().is_empty());
    }

    #[test]
    fn owner_to_changes_the_table_and_its_indexes_only() {
        let mut h = Harness::new();
        let mut ct = two_cols("w1");
        ct.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1])];
        let t = make(&mut h, ct);
        h.commit();
        let owner_of = |h: &Harness, oid| {
            let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
            h.db.catalog
                .relation_row(&snap, oid)
                .unwrap()
                .unwrap()
                .owner
        };
        // 同じ所有者への変更は何も書かない。
        h.begin();
        h.exec(owner(&t, OwnerSpec::CurrentUser)).unwrap();
        assert!(!h.txn.catalog_dirty);
        assert!(!h.txn.cid_used);
        h.exec(owner(&t, OwnerSpec::Name("postgres".into())))
            .unwrap();
        assert!(!h.txn.catalog_dirty);
        // 存在しないロール。
        let e = h
            .exec(owner(&t, OwnerSpec::Name("nosuch".into())))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(e.message, "role \"nosuch\" does not exist");
        // ログインできないロールにも変えられる。
        let new_owner =
            h.db.shared
                .role_by_name(
                    &h.tc.cluster.txn_manager().snapshot(None, 0),
                    "pg_database_owner",
                )
                .unwrap()
                .unwrap()
                .oid;
        h.exec(owner(&t, OwnerSpec::Name("pg_database_owner".into())))
            .unwrap();
        assert!(h.txn.catalog_dirty);
        assert_eq!(owner_of(&h, t.oid), new_owner);
        assert_eq!(owner_of(&h, t.indexes[0].oid), new_owner);
        h.commit();
        assert_eq!(owner_of(&h, t.oid), new_owner);
        assert!(h.check().is_empty());
        // システムカタログ。
        let pg_class = h.reader(|c| c.table(Some("pg_catalog"), "pg_class").unwrap().unwrap());
        h.begin();
        let e = h
            .exec(owner(&pg_class, OwnerSpec::CurrentUser))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        h.rollback();
    }
}

#[cfg(test)]
mod fk_tests {
    use super::*;
    use crate::analyzer::query::{BoundCreateTable, BoundDdl};
    use crate::catalog::fk::{FkAction, FkEqOps, FkMatch, FkNames, fk_constraint_def};
    use crate::catalog::schema::oids;
    use crate::ddl::testkit::{Harness, col, create_table_ddl, drop_ddl, key};
    use crate::types::{Datum, SqlType};
    use std::sync::Arc;

    fn make(h: &mut Harness, ct: BoundCreateTable) -> Arc<TableDef> {
        let name = ct.name.clone();
        h.exec_w(BoundDdl::CreateTable(ct)).unwrap();
        h.table(&name).unwrap()
    }

    fn parent(h: &mut Harness) -> Arc<TableDef> {
        let mut ct = create_table_ddl(
            "p",
            vec![col("id", 1, SqlType::INT4), col("k", 2, SqlType::INT4)],
        );
        ct.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1, 2])];
        make(h, ct)
    }

    fn child(h: &mut Harness) -> Arc<TableDef> {
        make(
            h,
            create_table_ddl(
                "c",
                vec![
                    col("id", 1, SqlType::INT4),
                    col("a", 2, SqlType::INT4),
                    col("b", 3, SqlType::INT4),
                ],
            ),
        )
    }

    fn bound(p: &TableDef, child_cols: &[i16], parent_cols: &[i16]) -> BoundForeignKey {
        BoundForeignKey {
            name: "c_a_b_fkey".into(),
            child_cols: child_cols.to_vec(),
            parent: BoundFkParent::Existing {
                oid: p.oid,
                schema: "public".into(),
                name: p.name.clone(),
                index: p.indexes[0].oid,
            },
            parent_cols: parent_cols.to_vec(),
            match_type: FkMatch::Simple,
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            set_cols: Vec::new(),
            eq_ops: vec![
                FkEqOps {
                    pf: 96,
                    pp: 96,
                    ff: 96
                };
                child_cols.len()
            ],
            span: crate::error::Span::default(),
        }
    }

    fn depend_rows(h: &Harness, constraint: Oid) -> Vec<(Oid, i32, char)> {
        use crate::catalog::rows::column_index;
        let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
        let (o, rid, rsub, ty) = (
            column_index(oids::PG_DEPEND, "objid"),
            column_index(oids::PG_DEPEND, "refobjid"),
            column_index(oids::PG_DEPEND, "refobjsubid"),
            column_index(oids::PG_DEPEND, "deptype"),
        );
        let mut v: Vec<_> =
            h.db.catalog
                .scan(&snap, oids::PG_DEPEND, |r| {
                    Ok(r[o] == Datum::Oid(constraint))
                })
                .unwrap()
                .iter()
                .map(|t| {
                    let (Datum::Oid(rid), Datum::Int4(sub), Datum::Char(c)) =
                        (&t.row[rid], &t.row[rsub], &t.row[ty])
                    else {
                        panic!("unexpected pg_depend row")
                    };
                    (*rid, *sub, char::from(*c))
                })
                .collect();
        v.sort_unstable();
        v
    }

    struct Names<'a>(&'a Harness);
    impl FkNames for Names<'_> {
        fn column_name(&self, table: Oid, attnum: i16) -> Option<String> {
            self.0.reader(|c| {
                c.table_by_oid(table)
                    .unwrap()?
                    .columns
                    .iter()
                    .find(|x| x.attnum == attnum)
                    .map(|x| x.name.clone())
            })
        }
        fn relation_name(&self, table: Oid) -> Option<String> {
            self.0.reader(|c| c.relation_name(table).unwrap())
        }
    }

    #[test]
    fn create_writes_constraint_and_dependency_rows() {
        let mut h = Harness::new();
        let p = parent(&mut h);
        let c = child(&mut h);
        // 親のキー (id, k) を (b, a) の順の列で参照する。
        let mut fk = bound(&p, &[3, 2], &[2, 1]);
        fk.on_update = FkAction::Cascade;
        fk.on_delete = FkAction::SetNull;
        fk.set_cols = vec![2];
        fk.match_type = FkMatch::Full;
        let def = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), false))
            .unwrap();
        assert!(h.txn.catalog_dirty);
        assert_eq!(def.parent_index, p.indexes[0].oid);
        assert_eq!(depend_rows(&h, def.oid), {
            let mut v = vec![
                (c.oid, 2, 'a'),
                (c.oid, 3, 'a'),
                (p.indexes[0].oid, 0, 'n'),
                (p.oid, 1, 'n'),
                (p.oid, 2, 'n'),
            ];
            v.sort_unstable();
            v
        });
        h.commit();

        let c2 = h.table("c").unwrap();
        let p2 = h.table("p").unwrap();
        assert_eq!(c2.foreign_keys.len(), 1);
        assert!(c2.referenced_by.is_empty());
        assert_eq!(p2.referenced_by.len(), 1);
        assert!(p2.foreign_keys.is_empty());
        assert_eq!(*c2.foreign_keys[0], def);
        assert_eq!(
            h.reader(|r| r.foreign_keys_of(c.oid).unwrap()),
            c2.foreign_keys
        );
        assert_eq!(
            h.reader(|r| r.foreign_keys_referencing(p.oid).unwrap()),
            p2.referenced_by
        );
        assert!(h.reader(|r| r.constraint_name_exists(c.namespace, "c_a_b_fkey").unwrap()));
        assert!(!h.reader(|r| r.constraint_name_exists(c.namespace, "nope").unwrap()));
        assert_eq!(
            fk_constraint_def(&def, &Names(&h)),
            "FOREIGN KEY (b, a) REFERENCES p(k, id) MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL (a)"
        );
        // pg_get_constraintdef 側の経路（ConstraintDef は contype f を読める）。
        let cd = h.reader(|r| r.constraint_by_oid(def.oid).unwrap()).unwrap();
        assert_eq!(cd.kind, crate::catalog::ConstraintKind::ForeignKey);
        assert_eq!(cd.columns, vec![3, 2]);
        assert_eq!(cd.table_oid, c.oid);
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn self_reference_lands_in_both_lists() {
        let mut h = Harness::new();
        let mut ct = create_table_ddl(
            "s",
            vec![col("id", 1, SqlType::INT4), col("parent", 2, SqlType::INT4)],
        );
        ct.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1])];
        let s = make(&mut h, ct);
        let mut fk = bound(&s, &[2], &[1]);
        fk.name = "s_parent_fkey".into();
        fk.parent = BoundFkParent::SelfTable {
            index_constraint: 0,
        };
        let def = h
            .run(|ctx| create_foreign_key(ctx, &s, &fk, ResolvedParent::SelfTable, false))
            .unwrap();
        assert!(def.is_self_referencing());
        assert_eq!(def.parent_index, s.indexes[0].oid);
        h.commit();
        let s2 = h.table("s").unwrap();
        assert_eq!(s2.foreign_keys.len(), 1);
        assert_eq!(s2.referenced_by.len(), 1);
        assert_eq!(
            depend_rows(&h, def.oid),
            vec![(s.oid, 1, 'n'), (s.oid, 2, 'a'), (s.indexes[0].oid, 0, 'n')]
        );
        // 自己参照の表の DROP は通る。
        h.begin();
        h.exec(drop_ddl(
            vec![s2],
            crate::catalog::depend::DropBehavior::Restrict,
        ))
        .unwrap();
        h.commit();
        assert!(h.table("s").is_none());
        assert_eq!(h.rows_mentioning(def.oid), 0);
        assert!(h.check().is_empty());
    }

    #[test]
    fn no_matching_unique_index_is_42830_and_existing_check_is_unsupported() {
        let mut h = Harness::new();
        let p = parent(&mut h);
        let c = child(&mut h);
        let mut fk = bound(&p, &[2], &[1]);
        fk.parent = BoundFkParent::SelfTable {
            index_constraint: 0,
        };
        let e = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), false))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_FOREIGN_KEY);
        let fk = bound(&p, &[2], &[1, 2]);
        let e = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), false))
            .unwrap_err();
        assert_eq!(
            e.message,
            "number of referencing and referenced columns for foreign key disagree"
        );
        let fk = bound(&p, &[2, 3], &[1, 2]);
        let e = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), true))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn drop_and_peers_and_truncate_checks() {
        let mut h = Harness::new();
        let p = parent(&mut h);
        let c = child(&mut h);
        let fk = bound(&p, &[2, 3], &[1, 2]);
        let def = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), false))
            .unwrap();
        h.commit();

        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        assert_eq!(
            h.db.catalog.fk_peer_tables(&snap, p.oid, None).unwrap(),
            vec![c.oid]
        );
        assert_eq!(
            h.db.catalog
                .fk_peer_tables(&snap, c.oid, Some("c_a_b_fkey"))
                .unwrap(),
            vec![p.oid]
        );
        assert!(
            h.db.catalog
                .fk_peer_tables(&snap, c.oid, Some("other"))
                .unwrap()
                .is_empty()
        );

        h.begin();
        let blockers = h
            .run(|ctx| fks_blocking_drop_tables(ctx, &[p.oid]))
            .unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(
            describe_dependent(&blockers[0], "c"),
            "constraint c_a_b_fkey on table c"
        );
        assert!(
            h.run(|ctx| fks_blocking_drop_tables(ctx, &[p.oid, c.oid]))
                .unwrap()
                .is_empty()
        );
        let by_index = h
            .run(|ctx| fks_depending_on_index(ctx, p.indexes[0].oid))
            .unwrap();
        assert_eq!(by_index.len(), 1);

        // TRUNCATE: 親だけは拒否、子も一緒なら通る、CASCADE は 0A000。
        let e = h
            .run(|ctx| check_truncate_fks(ctx, &[p.oid], false))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "cannot truncate a table referenced in a foreign key constraint"
        );
        assert_eq!(e.detail.as_deref(), Some("Table \"c\" references \"p\"."));
        assert_eq!(
            e.hint.as_deref(),
            Some("Truncate table \"c\" at the same time, or use TRUNCATE ... CASCADE.")
        );
        let e = h
            .run(|ctx| check_truncate_fks(ctx, &[p.oid], true))
            .unwrap_err();
        assert_eq!(e.message, "TRUNCATE ... CASCADE is not supported");
        h.run(|ctx| check_truncate_fks(ctx, &[p.oid, c.oid], false))
            .unwrap();
        h.run(|ctx| check_truncate_fks(ctx, &[c.oid], false))
            .unwrap();

        // DROP TABLE の親は 2BP01（依存行 n による）。FK を消せば通る。
        let p2 = h.table("p").unwrap();
        let e = h
            .exec(drop_ddl(
                vec![p2.clone()],
                crate::catalog::depend::DropBehavior::Restrict,
            ))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        h.run(|ctx| drop_foreign_key(ctx, &def)).unwrap();
        assert_eq!(h.rows_mentioning(def.oid), 0);
        h.commit();
        assert!(h.table("c").unwrap().foreign_keys.is_empty());
        assert!(h.table("p").unwrap().referenced_by.is_empty());
        assert!(h.check().is_empty());
    }

    #[test]
    fn dropping_the_child_removes_the_constraint() {
        let mut h = Harness::new();
        let p = parent(&mut h);
        let c = child(&mut h);
        let fk = bound(&p, &[2, 3], &[1, 2]);
        let def = h
            .run(|ctx| create_foreign_key(ctx, &c, &fk, ResolvedParent::Existing(&p), false))
            .unwrap();
        h.commit();
        h.begin();
        h.exec(drop_ddl(
            vec![h.table("c").unwrap()],
            crate::catalog::depend::DropBehavior::Restrict,
        ))
        .unwrap();
        h.commit();
        assert_eq!(h.rows_mentioning(def.oid), 0);
        assert!(h.table("p").unwrap().referenced_by.is_empty());
        assert!(h.check().is_empty());
    }
}
