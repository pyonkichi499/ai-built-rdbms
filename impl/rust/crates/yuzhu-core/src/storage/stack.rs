//! `StorageStack`: smgr + buffer pool + clog + heap + WAL assembled in one
//! place, shared by `Cluster::open`, bootstrap, recovery and the test
//! scaffolding (`m2.md` §4.8, `m3.md` §4.5).

use std::sync::Arc;

use super::buffer::{BufferPool, WalFlush};
use super::heap_store::HeapStore;
use super::smgr::StorageManager;
use super::vfs::Vfs;
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result};
use crate::txn::Xid;
use crate::txn::clog::Clog;
use crate::wal::Wal;

/// Settings of a [`StorageStack`].
#[derive(Clone, Copy, Debug)]
pub struct StackConfig {
    pub rel_seg_blocks: u32,
    pub nframes: usize,
    pub knobs: DebugKnobs,
}

/// The pool's view of the [`Wal`]: flushes through it and reports the durable end
/// for the `assert_wal_before_data` check.
#[derive(Debug)]
pub struct WalFlusher(pub Arc<Wal>);

impl WalFlush for WalFlusher {
    fn flush_to(&self, lsn: u64) -> Result<()> {
        self.0.flush_to(lsn)
    }

    fn redo_ptr(&self) -> u64 {
        self.0.redo_ptr()
    }

    fn flushed_ptr(&self) -> u64 {
        self.0.durable_lsn().0
    }
}

#[derive(Debug)]
pub struct StorageStack {
    pub vfs: Arc<dyn Vfs>,
    pub smgr: Arc<StorageManager>,
    pub pool: Arc<BufferPool>,
    pub clog: Arc<Clog>,
    pub heap: Arc<HeapStore>,
    pub wal: Arc<Wal>,
}

impl StorageStack {
    /// Builds the stack over `vfs`. `next_xid` is the control file's next
    /// XID (the clog loads the page that contains it). The caller decides
    /// the mode of `wal` (writing, or recovery) before passing it in; the
    /// same `Wal` object is used during and after recovery, so the buffer
    /// pool never has its WAL swapped.
    pub fn new(
        vfs: Arc<dyn Vfs>,
        cfg: &StackConfig,
        wal: Arc<Wal>,
        next_xid: Xid,
    ) -> Result<StorageStack> {
        if cfg.rel_seg_blocks == 0 {
            return Err(Error::internal("rel_seg_blocks must be positive"));
        }
        if cfg.nframes == 0 {
            return Err(Error::internal("the buffer pool needs at least one frame"));
        }
        let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), cfg.rel_seg_blocks));
        let pool = BufferPool::new(
            cfg.nframes,
            Arc::clone(&smgr),
            Arc::new(WalFlusher(Arc::clone(&wal))),
            cfg.knobs,
        );
        let clog = Arc::new(Clog::open(Arc::clone(&vfs), next_xid)?);
        let heap = Arc::new(HeapStore::new(
            Arc::clone(&pool),
            Arc::clone(&clog),
            Arc::clone(&wal),
        ));
        Ok(StorageStack {
            vfs,
            smgr,
            pool,
            clog,
            heap,
            wal,
        })
    }
}
