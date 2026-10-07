//! `TRUNCATE`（`m4/07-catalog-ddl.md` §5.6）。

use std::collections::HashSet;
use std::sync::Arc;

use super::DdlCtx;
use super::index::system_catalog_error;
use super::sequence;
use crate::analyzer::query::BoundTruncate;
use crate::catalog::{RelKind, TableDef};
use crate::error::{Error, Result, sqlstate};
use crate::storage::IndexHandle;
use crate::storage::smgr::{RelFileLocator, RelFileNumber};
use crate::types::Oid;

pub fn truncate(ctx: &mut DdlCtx<'_>, b: &BoundTruncate) -> Result<String> {
    // 1. 重複を除き（最初の出現順）、すべての表を検査してから書き始める。
    let mut seen = HashSet::new();
    let tables: Vec<&Arc<TableDef>> = b.tables.iter().filter(|t| seen.insert(t.oid)).collect();
    for t in &tables {
        if t.kind != RelKind::Table {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a table", t.name),
            ));
        }
        if t.is_system_catalog() {
            return Err(system_catalog_error(&t.name));
        }
    }
    // 2.
    let w = ctx.write_ctx()?;
    // 3.
    for t in &tables {
        let (new_table_oid, _) = new_file(ctx, &w, t.locator)?;
        ctx.db
            .catalog
            .truncate_relation(&w, ctx.snapshot, t.oid, new_table_oid)?;
        for i in &t.indexes {
            let (new_oid, new_loc) = new_file(ctx, &w, i.locator)?;
            let handle = IndexHandle {
                locator: new_loc,
                ..IndexHandle::from_def(i, t)
            };
            ctx.index_store().init_index(&w, &handle)?;
            ctx.db
                .catalog
                .truncate_relation(&w, ctx.snapshot, i.oid, new_oid)?;
        }
    }
    // 4.
    if b.restart_identity {
        for t in &tables {
            sequence::restart_owned_by_table(ctx, t.oid)?;
        }
    }
    // 5. `b.cascade` は無視する（外部キーがない）。
    // 6.
    ctx.mark_catalog_dirty();
    Ok("TRUNCATE TABLE".into())
}

/// 新しい relfilenode のファイルを作り（`pending_creates`）、旧いファイルを `pending_unlinks` に積む。
fn new_file(
    ctx: &mut DdlCtx<'_>,
    w: &crate::storage::WriteCtx,
    old: RelFileLocator,
) -> Result<(Oid, RelFileLocator)> {
    let new_oid = ctx
        .db
        .catalog
        .get_new_relfilenumber(ctx.cluster.oid_allocator())?;
    let new_loc = RelFileLocator {
        spc_oid: old.spc_oid,
        db_oid: ctx.db.oid,
        rel_number: RelFileNumber(new_oid),
    };
    ctx.create_file(w, new_loc)?;
    ctx.schedule_unlinks(&[old])?;
    Ok((new_oid, new_loc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::{BoundDdl, IndexConstraintKind};
    use crate::catalog::CatalogReader;
    use crate::ddl::testkit::{Harness, col, create_table_ddl, key};
    use crate::storage::RelHandle;
    use crate::types::{Datum, SqlType};

    fn trunc(tables: Vec<Arc<TableDef>>) -> BoundDdl {
        BoundDdl::Truncate(BoundTruncate {
            tables,
            restart_identity: false,
            cascade: false,
        })
    }

    fn count_rows(h: &mut Harness, t: &TableDef) -> usize {
        let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
        let rel = RelHandle::from_table(t);
        let heap = h.tc.cluster.storage();
        let mut scan = heap.begin_scan(&rel, &snap).unwrap();
        let mut n = 0;
        while heap.scan_next(&mut scan).unwrap().is_some() {
            n += 1;
        }
        n
    }

    fn setup(h: &mut Harness) -> Arc<TableDef> {
        let mut ct = create_table_ddl(
            "tr1",
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::TEXT)],
        );
        ct.constraints = vec![key(IndexConstraintKind::PrimaryKey, None, &[1])];
        h.exec_w(BoundDdl::CreateTable(ct)).unwrap();
        let t = h.table("tr1").unwrap();
        h.run(|ctx| {
            let w = ctx.write_ctx()?;
            let rel = RelHandle::from_table(&t);
            for i in 0..5 {
                ctx.heap()
                    .insert(&rel, &w, &[Datum::Int4(i), Datum::Text("x".into())])?;
            }
            Ok(())
        })
        .unwrap();
        h.commit();
        h.table("tr1").unwrap()
    }

    #[test]
    fn truncate_swaps_files_and_resets_statistics() {
        let mut h = Harness::new();
        let t = setup(&mut h);
        assert_eq!(count_rows(&mut h, &t), 5);
        h.begin();
        assert_eq!(
            h.exec(trunc(vec![Arc::clone(&t)])).unwrap(),
            "TRUNCATE TABLE"
        );
        let new_t = h.table("tr1").unwrap();
        assert_ne!(new_t.locator, t.locator);
        assert_ne!(new_t.indexes[0].locator, t.indexes[0].locator);
        // 旧ファイルはコミットまで残り、新しいファイルが先にある。
        assert!(h.file_exists(t.locator) && h.file_exists(new_t.locator));
        assert!(h.file_exists(new_t.indexes[0].locator));
        assert_eq!(count_rows(&mut h, &new_t), 0);
        assert_eq!(
            count_rows(&mut h, &t),
            5,
            "old readers still see the old file"
        );
        // 空の索引（メタページと空のルート）。
        let handle = crate::storage::IndexHandle::from_def(&new_t.indexes[0], &new_t);
        assert_eq!(h.tc.cluster.stack().index.nblocks(&handle).unwrap(), 2);
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        assert_eq!(
            h.db.catalog
                .relation_row(&snap, t.oid)
                .unwrap()
                .unwrap()
                .locator,
            t.locator
        );
        h.commit();
        assert!(h.file_removed(t.locator));
        assert!(h.file_removed(t.indexes[0].locator));
        assert!(h.file_exists(new_t.locator));
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        let row = h.db.catalog.relation_row(&snap, t.oid).unwrap().unwrap();
        assert_eq!(row.locator, new_t.locator);
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn truncate_rollback_removes_the_new_files() {
        let mut h = Harness::new();
        let t = setup(&mut h);
        h.begin();
        h.exec(trunc(vec![Arc::clone(&t)])).unwrap();
        let created = h.txn.pending_creates.clone();
        assert_eq!(created.len(), 2, "table + index");
        h.rollback();
        assert!(created.iter().all(|l| h.file_removed(*l)));
        assert!(h.file_exists(t.locator));
        let t2 = h.table("tr1").unwrap();
        assert_eq!(t2.locator, t.locator);
        assert_eq!(count_rows(&mut h, &t2), 5);
    }

    #[test]
    fn duplicates_are_truncated_once_and_errors_come_first() {
        let mut h = Harness::new();
        let t = setup(&mut h);
        h.begin();
        h.exec(trunc(vec![Arc::clone(&t), Arc::clone(&t)])).unwrap();
        assert_eq!(h.txn.pending_creates.len(), 2, "one new file per relation");
        h.rollback();

        h.begin();
        let pg_class = h.reader(|c| c.table(Some("pg_catalog"), "pg_class").unwrap().unwrap());
        let e = h.exec(trunc(vec![Arc::clone(&t), pg_class])).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert!(h.txn.pending_creates.is_empty(), "nothing was written");
        // 索引はテーブルではない。
        let idx_def = TableDef {
            kind: RelKind::Index,
            ..(*t).clone()
        };
        let e = h.exec(trunc(vec![Arc::new(idx_def)])).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(e.message, "\"tr1\" is not a table");
        h.rollback();
    }

    #[test]
    fn a_table_created_in_the_same_transaction_is_truncated_without_double_unlink() {
        let mut h = Harness::new();
        h.begin();
        h.exec(BoundDdl::CreateTable(create_table_ddl(
            "tr2",
            vec![col("a", 1, SqlType::INT4)],
        )))
        .unwrap();
        let t = h.table("tr2").unwrap();
        h.exec(trunc(vec![t])).unwrap();
        h.commit();
        assert!(h.check().is_empty());
    }
}
