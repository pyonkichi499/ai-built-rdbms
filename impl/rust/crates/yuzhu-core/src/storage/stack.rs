//! `StorageStack`: smgr + buffer pool + clog + heap + WAL assembled in one
//! place, shared by `Cluster::open`, bootstrap, recovery and the test
//! scaffolding (`m2.md` §4.8, `m3.md` §4.5).

use std::sync::Arc;

use super::buffer::{BufferPool, WalFlush};
use super::heap_store::HeapStore;
use super::index_store::BtreeStore;
use super::sequence::SeqStore;
use super::smgr::StorageManager;
use super::vfs::Vfs;
use super::{IndexStore, SequenceStore};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result};
use crate::txn::ProcArray;
use crate::txn::Xid;
use crate::txn::clog::Clog;
use crate::txn::lock::LockManager;
use crate::wal::Wal;

/// Settings of a [`StorageStack`].
///
/// M5（01 §5.12、00 §4.5。R-18）: `HeapStore` が要る `LockManager` と、`Clog::open` に渡す `oldest_xid` は
/// ここで渡す（`StorageStack::new` の署名は変えない）。`multixact: Arc<MultiXactTable>` は RW-2 が
/// `MultiXactTable` を作る時に足す（型がまだ無い）。
#[derive(Clone, Debug)]
pub struct StackConfig {
    pub rel_seg_blocks: u32,
    pub nframes: usize,
    pub knobs: DebugKnobs,
    /// ロックマネージャ（`Cluster` が 1 つ作って共有する）。
    pub locks: Arc<LockManager>,
    /// 制御ファイルの `oldest_xid`（`Clog::open` の掃除の下限。0 のときは `Xid(3)` とみなす）。
    pub oldest_xid: Xid,
}

impl StackConfig {
    /// 既定値の設定（新しい `LockManager`、`oldest_xid` = `Xid::FIRST_NORMAL`）。
    /// `Cluster::prepare` は `locks` と `oldest_xid` を実際の値で置き換える。
    pub fn new(rel_seg_blocks: u32, nframes: usize, knobs: DebugKnobs) -> StackConfig {
        StackConfig {
            rel_seg_blocks,
            nframes,
            knobs,
            locks: LockManager::with_knobs(knobs),
            oldest_xid: Xid::FIRST_NORMAL,
        }
    }
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
    /// XID の採番・実行中一覧・スナップショットの登録簿（`TxnManager` が使う。01 §4.4）。
    pub procs: Arc<ProcArray>,
    pub heap: Arc<HeapStore>,
    /// B+Tree（M4）。B1・B2 が実体を作るまでは `not_supported` を返すスタブ。
    pub index: Arc<dyn IndexStore>,
    /// シーケンス（M4）。Q1 が実体を作るまでは `not_supported` を返すスタブ。
    pub seq: Arc<dyn SequenceStore>,
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
        let clog = Arc::new(Clog::open(Arc::clone(&vfs), next_xid, cfg.oldest_xid)?);
        let procs = ProcArray::new(next_xid);
        let heap = Arc::new(HeapStore::new(
            Arc::clone(&pool),
            Arc::clone(&clog),
            Arc::clone(&wal),
        ));
        let index =
            Arc::new(BtreeStore::new(Arc::clone(&pool), Arc::clone(&wal)).with_knobs(cfg.knobs));
        let seq = Arc::new(
            SeqStore::new(Arc::clone(&pool), Arc::clone(&smgr), Arc::clone(&wal))
                .with_knobs(cfg.knobs),
        );
        Ok(StorageStack {
            vfs,
            smgr,
            pool,
            clog,
            procs,
            heap,
            index,
            seq,
            wal,
        })
    }
}
