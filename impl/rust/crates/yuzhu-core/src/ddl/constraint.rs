//! `ALTER TABLE ... ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE` と `OWNER TO`
//! （`m4/07-catalog-ddl.md` §5.4、§5.5）。

use std::collections::HashSet;

use super::index::{
    build_from_heap, default_key_parts, new_index_columns, system_catalog_error, table_owner,
};
use super::{DdlCtx, RelOptTarget, validate_reloptions};
use crate::analyzer::query::{
    AlterTarget, BoundAlterTableAddCheck, BoundAlterTableAddConstraint, BoundAlterTableOwner,
    IndexConstraintKind, OwnerSpec,
};
use crate::catalog::naming::{choose_index_column_names, choose_index_name};
use crate::catalog::schema::oids;
use crate::catalog::store::{ClassPatch, NewConstraint, NewIndex};
use crate::catalog::{CheckDef, IndexConstraintRef, IndexDef};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::RelHandle;

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
