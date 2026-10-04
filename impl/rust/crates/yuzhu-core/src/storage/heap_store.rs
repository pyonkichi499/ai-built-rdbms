//! `HeapStore`: the `TableStore` implementation over the buffer pool and
//! the heap (`m2.md` §4.4).
//!
//! 担当 D が実装する。

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::RelFileLocator;
use crate::storage::{
    HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
};
use crate::txn::Snapshot;
use crate::txn::clog::Clog;
use crate::types::{Datum, Tid};

fn pending() -> Error {
    Error::not_supported("heap store is not implemented yet")
}

#[derive(Debug)]
pub struct HeapStore {
    #[allow(dead_code)]
    pool: Arc<BufferPool>,
    #[allow(dead_code)]
    clog: Arc<Clog>,
}

impl HeapStore {
    pub fn new(pool: Arc<BufferPool>, clog: Arc<Clog>) -> HeapStore {
        HeapStore { pool, clog }
    }
}

impl TableStore for HeapStore {
    fn create_storage(&self, _rel: RelFileLocator) -> Result<()> {
        Err(pending())
    }

    fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
        Err(pending())
    }

    fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
        Err(pending())
    }

    fn insert(&self, _rel: &RelHandle, _w: &WriteCtx, _row: &[Datum]) -> Result<Tid> {
        Err(pending())
    }

    fn delete(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
    ) -> Result<TmResult> {
        Err(pending())
    }

    fn update(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
        _new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        Err(pending())
    }

    fn begin_scan(&self, _rel: &RelHandle, _snap: &Snapshot) -> Result<HeapScan> {
        Err(pending())
    }

    fn scan_next(&self, _scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        Err(pending())
    }

    fn fetch(&self, _rel: &RelHandle, _snap: &Snapshot, _tid: Tid) -> Result<Option<HeapTuple>> {
        Err(pending())
    }
}
