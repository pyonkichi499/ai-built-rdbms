//! `StorageStack`: smgr + buffer pool + clog + heap assembled in one place,
//! shared by `Cluster::open`, bootstrap and the test scaffolding
//! (`m2.md` §4.8).
//!
//! 担当 C が実装する。

use std::sync::Arc;

use super::buffer::BufferPool;
use super::heap_store::HeapStore;
use super::smgr::StorageManager;
use super::vfs::Vfs;
use crate::error::{Error, Result};
use crate::txn::Xid;
use crate::txn::clog::Clog;

#[derive(Debug)]
pub struct StorageStack {
    pub vfs: Arc<dyn Vfs>,
    pub smgr: Arc<StorageManager>,
    pub pool: Arc<BufferPool>,
    pub clog: Arc<Clog>,
    pub heap: Arc<HeapStore>,
}

impl StorageStack {
    pub fn new(
        _vfs: Arc<dyn Vfs>,
        _rel_seg_blocks: u32,
        _nframes: usize,
        _next_xid: Xid,
    ) -> Result<StorageStack> {
        Err(Error::not_supported("storage stack is not implemented yet"))
    }
}
