//! Buffer pool: frames, pins, latches, clock-sweep replacement
//! (`m2.md` §4.3, §6.3).
//!
//! 担当 C が実装する。`BufferPool` の本体と `PinnedBuffer` は未実装。

#![allow(clippy::unimplemented)]

pub mod clock;
pub mod frame;
pub mod guard;
pub mod table;
pub mod track;

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use self::guard::{PageReadGuard, PageWriteGuard};
use super::smgr::{BufferTag, ForkNumber, RelFileLocator, StorageManager};
use crate::error::{Error, Result, Severity};

fn pending() -> Error {
    Error::not_supported("buffer pool is not implemented yet")
}

/// WAL flush hook for WAL-before-data. M2 uses [`NoWal`].
pub trait WalFlush: Send + Sync + std::fmt::Debug {
    fn flush_to(&self, lsn: u64) -> Result<()>;
    fn redo_ptr(&self) -> u64;
}

/// M2: no WAL, nothing to flush.
#[derive(Debug)]
pub struct NoWal;

impl WalFlush for NoWal {
    fn flush_to(&self, _lsn: u64) -> Result<()> {
        Ok(())
    }

    fn redo_ptr(&self) -> u64 {
        0
    }
}

/// Set when the cluster must stop: a panic or error inside a critical
/// section, a poisoned latch, a failed fsync (`m2.md` §6.3.4).
#[derive(Debug, Default)]
pub struct PoisonFlag(AtomicBool);

impl PoisonFlag {
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub struct BufferPool {
    #[allow(dead_code)]
    smgr: Arc<StorageManager>,
    #[allow(dead_code)]
    wal: Arc<dyn WalFlush>,
    #[allow(dead_code)]
    nframes: usize,
    poison: Arc<PoisonFlag>,
}

impl BufferPool {
    pub fn new(nframes: usize, smgr: Arc<StorageManager>, wal: Arc<dyn WalFlush>) -> Arc<Self> {
        Arc::new(BufferPool {
            smgr,
            wal,
            nframes,
            poison: Arc::new(PoisonFlag::default()),
        })
    }

    pub fn read_buffer(self: &Arc<Self>, _tag: BufferTag) -> Result<PinnedBuffer> {
        Err(pending())
    }

    /// M3 REDO: extends the file to `tag.block` and pins a zero page
    /// without reading. Unused in M2.
    pub fn read_buffer_zeroed(self: &Arc<Self>, _tag: BufferTag) -> Result<PinnedBuffer> {
        Err(pending())
    }

    /// Extends the relation by one block and pins the zero page. The caller
    /// must hold no page latch (§6.3 item 10).
    pub fn extend(
        self: &Arc<Self>,
        _rel: RelFileLocator,
        _fork: ForkNumber,
    ) -> Result<PinnedBuffer> {
        Err(pending())
    }

    pub fn nblocks(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<u32> {
        Err(pending())
    }

    pub fn flush_all_for_checkpoint(&self) -> Result<FlushStats> {
        Err(pending())
    }

    /// Discards a relation's buffers without writing (DROP). Pinned buffers
    /// are an internal error.
    pub fn drop_relation_buffers(&self, _rel: RelFileLocator) -> Result<()> {
        Err(pending())
    }

    /// Writes a relation's dirty buffers (before copying a database).
    pub fn flush_relation_buffers(&self, _rel: RelFileLocator) -> Result<()> {
        Err(pending())
    }

    pub fn pinned_frames(&self) -> usize {
        unimplemented!("担当 C が実装")
    }

    pub fn stats(&self) -> BufferStatsSnapshot {
        unimplemented!("担当 C が実装")
    }

    /// The flag shared with [`CriticalSection`].
    pub fn poison_flag(&self) -> &Arc<PoisonFlag> {
        &self.poison
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FlushStats {
    pub written: u64,
    pub skipped_clean: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BufferStatsSnapshot {
    pub hits: u64,
    pub reads: u64,
    pub writes: u64,
    pub evictions: u64,
    pub write_errors: u64,
}

/// A pin. `'static`, released on drop, and `!Send` so that the thread-local
/// pin counts stay correct.
#[derive(Debug)]
pub struct PinnedBuffer {
    #[allow(dead_code)]
    pool: Arc<BufferPool>,
    #[allow(dead_code)]
    frame: usize,
    tag: BufferTag,
    _not_send: PhantomData<*const ()>,
}

impl PinnedBuffer {
    pub fn tag(&self) -> BufferTag {
        self.tag
    }

    /// Shared latch. A poisoned latch is a `Severity::Panic` error.
    pub fn read(&self) -> Result<PageReadGuard<'_>> {
        Err(pending())
    }

    /// Exclusive latch. A poisoned latch is a `Severity::Panic` error.
    pub fn write(&self) -> Result<PageWriteGuard<'_>> {
        Err(pending())
    }

    pub fn try_write(&self) -> Result<Option<PageWriteGuard<'_>>> {
        Err(pending())
    }
}

impl Clone for PinnedBuffer {
    /// Takes another pin.
    fn clone(&self) -> Self {
        unimplemented!("担当 C が実装")
    }
}

/// Critical section around page modifications (D14). An error inside is
/// escalated to `Severity::Panic`; a panic inside poisons the cluster.
#[derive(Debug)]
pub struct CriticalSection {
    poison: Arc<PoisonFlag>,
}

impl CriticalSection {
    pub fn enter(pool: &BufferPool) -> CriticalSection {
        CriticalSection {
            poison: Arc::clone(&pool.poison),
        }
    }

    /// Turns `e` into a `Severity::Panic` error and poisons the cluster.
    pub fn escalate(&self, mut e: Error) -> Error {
        e.severity = Severity::Panic;
        self.poison.set();
        e
    }
}

impl Drop for CriticalSection {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.poison.set();
        }
    }
}

/// Called at the end of a statement. If this thread still holds pins:
/// panic in debug builds, WARNING "buffer refcount leak" in release builds.
pub fn assert_no_pins() {
    // 担当 C が実装する（スレッドローカルのピン表。track.rs）。
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::storage::vfs::SimVfs;

    fn pool() -> Arc<BufferPool> {
        let smgr = Arc::new(StorageManager::new(Arc::new(SimVfs::new(1)), 131_072));
        BufferPool::new(16, smgr, Arc::new(NoWal))
    }

    #[test]
    fn critical_section_escalates_errors_to_panic() {
        let p = pool();
        let cs = CriticalSection::enter(&p);
        assert!(!p.poison_flag().is_set());
        let e = cs.escalate(Error::internal("boom"));
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(p.poison_flag().is_set());
    }

    #[test]
    fn critical_section_poisons_on_panic_only() {
        let p = pool();
        drop(CriticalSection::enter(&p));
        assert!(!p.poison_flag().is_set());
        let p2 = Arc::clone(&p);
        let r = std::thread::spawn(move || {
            let _cs = CriticalSection::enter(&p2);
            panic!("inside the critical section");
        })
        .join();
        assert!(r.is_err());
        assert!(p.poison_flag().is_set());
    }

    #[test]
    fn nowal_is_a_no_op() {
        let w = NoWal;
        assert!(w.flush_to(100).is_ok());
        assert_eq!(w.redo_ptr(), 0);
    }
}
