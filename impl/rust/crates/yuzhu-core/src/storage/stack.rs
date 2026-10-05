//! `StorageStack`: smgr + buffer pool + clog + heap assembled in one place,
//! shared by `Cluster::open`, bootstrap and the test scaffolding
//! (`m2.md` §4.8).

use std::sync::Arc;

use super::buffer::{BufferPool, NoWal};
use super::heap_store::HeapStore;
use super::smgr::StorageManager;
use super::vfs::Vfs;
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result};
use crate::txn::Xid;
use crate::txn::clog::Clog;

/// `StorageStack::new` に渡す設定（`m3.md` §4.5）。担当 C が `StorageStack::new` をこれを
/// 受け取る形にする。
#[derive(Clone, Copy, Debug)]
pub struct StackConfig {
    pub rel_seg_blocks: u32,
    pub nframes: usize,
    pub knobs: DebugKnobs,
}

#[derive(Debug)]
pub struct StorageStack {
    pub vfs: Arc<dyn Vfs>,
    pub smgr: Arc<StorageManager>,
    pub pool: Arc<BufferPool>,
    pub clog: Arc<Clog>,
    pub heap: Arc<HeapStore>,
}

impl StorageStack {
    /// Builds the stack over `vfs`. `next_xid` is the control file's next
    /// XID (the clog loads the page that contains it). M2 has no WAL, so
    /// the pool gets [`NoWal`].
    pub fn new(
        vfs: Arc<dyn Vfs>,
        rel_seg_blocks: u32,
        nframes: usize,
        next_xid: Xid,
    ) -> Result<StorageStack> {
        if rel_seg_blocks == 0 {
            return Err(Error::internal("rel_seg_blocks must be positive"));
        }
        if nframes == 0 {
            return Err(Error::internal("the buffer pool needs at least one frame"));
        }
        let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), rel_seg_blocks));
        let pool = BufferPool::new(nframes, Arc::clone(&smgr), Arc::new(NoWal));
        let clog = Arc::new(Clog::open(Arc::clone(&vfs), next_xid)?);
        let heap = Arc::new(HeapStore::new(Arc::clone(&pool), Arc::clone(&clog)));
        Ok(StorageStack {
            vfs,
            smgr,
            pool,
            clog,
            heap,
        })
    }
}
