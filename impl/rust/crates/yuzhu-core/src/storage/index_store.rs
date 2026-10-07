//! `BtreeStore`: `IndexStore` の実装（`m4/00-contracts.md` §13.2、`m4/06-btree.md` §4.6）。
//!
//! すべてのメソッドが配線済み（`init_index`・`insert` は B1b、`build`・`begin_scan`・`scan_next` は B2b）。
//! リレーションごとのメモリ上の状態は持たない。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::btree::{BtCtx, build, insert as bt_insert, meta, scan};
use super::buffer::BufferPool;
use super::smgr::ForkNumber;
use super::{
    BuildStats, BuildUnique, IndexHandle, IndexScan, IndexStore, ResolvedScanKeys, ScanDirection,
    UniqueCheck, WriteCtx,
};
use crate::debug_knobs::DebugKnobs;
use crate::error::Result;
use crate::types::{Datum, Tid};
use crate::wal::Wal;

#[derive(Debug)]
pub struct BtreeStore {
    pool: Arc<BufferPool>,
    wal: Arc<Wal>,
    knobs: DebugKnobs,
    inserts: AtomicU64,
}

impl BtreeStore {
    pub fn new(pool: Arc<BufferPool>, wal: Arc<Wal>) -> BtreeStore {
        BtreeStore {
            pool,
            wal,
            knobs: DebugKnobs::default(),
            inserts: AtomicU64::new(0),
        }
    }

    /// 変異テスト用のスイッチを渡す（`btree_split_in_two_records`・`btree_lossy_insert_every`）。
    #[must_use]
    pub fn with_knobs(mut self, knobs: DebugKnobs) -> BtreeStore {
        self.knobs = knobs;
        self
    }
}

impl IndexStore for BtreeStore {
    fn init_index(&self, w: &WriteCtx, index: &IndexHandle) -> Result<()> {
        meta::init_index(&self.pool, &self.wal, w, index)
    }

    fn insert(
        &self,
        w: &WriteCtx,
        index: &IndexHandle,
        key: &[Datum],
        tid: Tid,
        check: UniqueCheck<'_>,
    ) -> Result<()> {
        let ctx = BtCtx {
            pool: &self.pool,
            wal: &self.wal,
            index,
            knobs: self.knobs,
        };
        if self.knobs.btree_lossy_insert_every > 0
            && (self.inserts.fetch_add(1, Ordering::Relaxed) + 1)
                .is_multiple_of(self.knobs.btree_lossy_insert_every)
        {
            return Ok(());
        }
        bt_insert::insert(&ctx, w, key, tid, check)
    }

    fn build(
        &self,
        w: &WriteCtx,
        index: &IndexHandle,
        entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>,
        unique: BuildUnique,
    ) -> Result<BuildStats> {
        let ctx = BtCtx {
            pool: &self.pool,
            wal: &self.wal,
            index,
            knobs: self.knobs,
        };
        build::build_index(&ctx, w, entries, unique)
    }

    fn begin_scan(
        &self,
        index: &IndexHandle,
        keys: &ResolvedScanKeys,
        dir: ScanDirection,
    ) -> Result<IndexScan> {
        scan::begin(index, keys, dir)
    }

    fn scan_next(&self, scan: &mut IndexScan) -> Result<Option<Tid>> {
        scan::next(&self.pool, &self.wal, scan)
    }

    fn nblocks(&self, index: &IndexHandle) -> Result<u32> {
        self.pool.nblocks(index.locator, ForkNumber::Main)
    }

    fn unlink_storage(&self, index: &IndexHandle) -> Result<()> {
        self.pool.drop_relation_buffers(index.locator)?;
        self.pool.smgr().unlink(index.locator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::storage::btree::testing::{index_handle, test_index};
    use crate::txn::Xid;
    use crate::types::SqlType;

    fn tid(n: u16) -> Tid {
        Tid {
            block: 0,
            offset: n,
        }
    }

    #[test]
    fn init_insert_and_nblocks_go_through_the_store() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let store = BtreeStore::new(Arc::clone(t.pool()), Arc::clone(t.wal()));
        let w = WriteCtx {
            xid: Xid(3),
            cid: 0,
        };
        store.init_index(&w, &t.handle).unwrap();
        assert_eq!(store.nblocks(&t.handle).unwrap(), 2);
        for i in 1..=500u16 {
            store
                .insert(
                    &w,
                    &t.handle,
                    &[Datum::Int4(i32::from(i))],
                    tid(i),
                    UniqueCheck::Skip,
                )
                .unwrap();
        }
        // 408 件目で最初の分割（左 1、右 2、新ルート 3）。
        assert!(store.nblocks(&t.handle).unwrap() >= 4);
        t.assert_clean();
    }

    #[test]
    fn insert_rejects_a_key_of_the_wrong_length() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let store = BtreeStore::new(Arc::clone(t.pool()), Arc::clone(t.wal()));
        let w = t.write_ctx();
        store.init_index(&w, &t.handle).unwrap();
        let e = store
            .insert(&w, &t.handle, &[], tid(1), UniqueCheck::Skip)
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn unlink_storage_truncates_the_file() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let store = BtreeStore::new(Arc::clone(t.pool()), Arc::clone(t.wal()));
        store.init_index(&t.write_ctx(), &t.handle).unwrap();
        store.unlink_storage(&t.handle).unwrap();
        assert_eq!(
            t.pool()
                .nblocks(t.handle.locator, ForkNumber::Main)
                .unwrap(),
            0
        );
        let _ = index_handle;
    }

    #[test]
    fn build_and_scan_go_through_the_store() {
        use crate::storage::{ResolvedScanKeys, ScanDirection};
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let store = BtreeStore::new(Arc::clone(t.pool()), Arc::clone(t.wal()));
        let w = t.write_ctx();
        store.init_index(&w, &t.handle).unwrap();
        let mut it = (1..=2000u16).map(|i| (vec![Datum::Int4(i32::from(i))], tid(i)));
        let stats = store
            .build(&w, &t.handle, &mut it, BuildUnique::No)
            .unwrap();
        assert_eq!(stats.tuples, 2000);
        assert_eq!(stats.pages, store.nblocks(&t.handle).unwrap());
        let keys = ResolvedScanKeys {
            eq: vec![],
            lower: Some((Datum::Int4(1995), true)),
            upper: None,
        };
        let mut scan = store
            .begin_scan(&t.handle, &keys, ScanDirection::Backward)
            .unwrap();
        let mut got = Vec::new();
        while let Some(x) = store.scan_next(&mut scan).unwrap() {
            got.push(x.offset);
        }
        assert_eq!(got, vec![2000, 1999, 1998, 1997, 1996, 1995]);
        t.assert_clean();
    }
}
