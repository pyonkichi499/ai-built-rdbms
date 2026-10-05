//! Test scaffolding: a `StorageStack` over `SimVfs`, used by the unit tests
//! of the heap, the catalog and the executor (`m2.md` §2).
//!
//! ```ignore
//! let ts = TestStorage::new();                 // 64 frames, 1GB segments
//! let rel = test_rel(16384);
//! ts.smgr().create(rel, ForkNumber::Main)?;
//! // ... use ts.pool(), ts.clog(), ts.heap() ...
//! ts.assert_clean();                           // no pins left
//!
//! let ts2 = ts.crash_and_reopen(CrashMode::DropUnsynced)?;   // simulated crash
//! ```
//!
//! Everything lives in memory; `SimVfs` can inject faults
//! (`ts.vfs.set_faults(..)`) and simulate crashes.

use std::path::Path;
use std::sync::Arc;

use super::DEFAULT_RELSEG_SIZE;
use super::buffer::{BufferPool, PageWriteGuard, PinnedBuffer};
use super::heap_store::HeapStore;
use super::smgr::{
    BlockNumber, BufferTag, DEFAULTTABLESPACE_OID, ForkNumber, RelFileLocator, RelFileNumber,
    StorageManager,
};
use super::smgr_wal::log_and_create;
use super::stack::{StackConfig, StorageStack};
use super::vfs::{CrashMode, SimVfs, Vfs};
use crate::debug_knobs::DebugKnobs;
use crate::error::Result;
use crate::txn::Xid;
use crate::txn::clog::Clog;
use crate::wal::xlog::XLOG_NOOP;
use crate::wal::{
    Lsn, MIN_WAL_SEGMENT_SIZE, RecordBuilder, RegFlags, RmgrId, Wal, WalConfig, WalReader,
};

/// The database OID used by [`test_rel`].
pub const TEST_DB_OID: u32 = 5;

/// A relation in the default tablespace of the test database.
pub fn test_rel(rel_number: u32) -> RelFileLocator {
    RelFileLocator {
        spc_oid: DEFAULTTABLESPACE_OID,
        db_oid: TEST_DB_OID,
        rel_number: RelFileNumber(rel_number),
    }
}

/// The tag of block `block` in the main fork of `rel`.
pub fn test_tag(rel: RelFileLocator, block: BlockNumber) -> BufferTag {
    BufferTag {
        rel,
        fork: ForkNumber::Main,
        block,
    }
}

/// Settings of a [`TestStorage`].
#[derive(Debug, Clone, Copy)]
pub struct TestStorageOptions {
    pub seed: u64,
    pub nframes: usize,
    pub rel_seg_blocks: u32,
    pub next_xid: Xid,
    /// Mutation switches. `assert_wal_before_data` is on by default in tests.
    pub knobs: DebugKnobs,
    pub wal_segment_size: u32,
}

impl Default for TestStorageOptions {
    fn default() -> Self {
        TestStorageOptions {
            seed: 1,
            nframes: 64,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            next_xid: Xid::FIRST_NORMAL,
            knobs: DebugKnobs {
                assert_wal_before_data: true,
                ..DebugKnobs::default()
            },
            wal_segment_size: MIN_WAL_SEGMENT_SIZE,
        }
    }
}

fn wal_config(options: &TestStorageOptions) -> WalConfig {
    WalConfig {
        segment_size: options.wal_segment_size,
        system_identifier: 1,
        full_page_writes: true,
        knobs: options.knobs,
    }
}

/// A `StorageStack` over a fresh `SimVfs`.
#[derive(Debug)]
pub struct TestStorage {
    /// The simulated disk (the same object the stack uses).
    pub vfs: SimVfs,
    pub stack: StorageStack,
    pub options: TestStorageOptions,
}

impl TestStorage {
    pub fn new() -> TestStorage {
        TestStorage::with_options(TestStorageOptions::default())
            .expect("the default test storage must build")
    }

    /// A small pool (to force evictions) and small segments.
    pub fn small(nframes: usize, rel_seg_blocks: u32) -> TestStorage {
        TestStorage::with_options(TestStorageOptions {
            nframes,
            rel_seg_blocks,
            ..TestStorageOptions::default()
        })
        .expect("the test storage must build")
    }

    pub fn with_options(options: TestStorageOptions) -> Result<TestStorage> {
        let vfs = SimVfs::new(options.seed);
        TestStorage::over(vfs, options)
    }

    /// Builds a stack on an existing `SimVfs`. A fresh `SimVfs` gets a new
    /// WAL; one that already holds WAL segments (after a crash) is brought
    /// to a writable state the way recovery does: the end of the valid WAL
    /// is found, the tail is zeroed and writing continues there. No REDO is
    /// performed (the callers of this scaffolding replay by hand if needed).
    pub fn over(vfs: SimVfs, options: TestStorageOptions) -> Result<TestStorage> {
        let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
        let cfg = wal_config(&options);
        let seg_dir = Path::new(crate::wal::segment::WAL_DIR);
        let wal = if shared.exists(seg_dir).unwrap_or(false) {
            let start = Lsn(u64::from(cfg.segment_size) + crate::wal::SEG_HEADER_SIZE);
            let mut reader = WalReader::open(Arc::clone(&shared), &cfg, start);
            while reader.next()?.is_some() {}
            let (end, _) = reader.end_of_wal().expect("the reader reached the end");
            let last = reader.last_record_start().unwrap_or(Lsn::INVALID);
            let wal = Wal::open_for_recovery(Arc::clone(&shared), cfg);
            wal.finish_recovery(end, last)?;
            wal
        } else {
            Wal::initialize(Arc::clone(&shared), cfg)?
        };
        let stack = StorageStack::new(
            shared,
            &StackConfig {
                rel_seg_blocks: options.rel_seg_blocks,
                nframes: options.nframes,
                knobs: options.knobs,
            },
            wal,
            options.next_xid,
        )?;
        Ok(TestStorage {
            vfs,
            stack,
            options,
        })
    }

    /// Simulates a crash: nothing is written (no Drop I/O), the `SimVfs`
    /// state is cut according to `mode`, and a new stack is built on the
    /// surviving "disk". The old stack must not be used afterwards (its
    /// operations fail with EIO).
    pub fn crash_and_reopen(&self, mode: CrashMode) -> Result<TestStorage> {
        TestStorage::over(self.vfs.crash(mode), self.options)
    }

    pub fn smgr(&self) -> &Arc<StorageManager> {
        &self.stack.smgr
    }

    pub fn pool(&self) -> &Arc<BufferPool> {
        &self.stack.pool
    }

    pub fn clog(&self) -> &Arc<Clog> {
        &self.stack.clog
    }

    pub fn heap(&self) -> &Arc<HeapStore> {
        &self.stack.heap
    }

    pub fn wal(&self) -> &Arc<Wal> {
        &self.stack.wal
    }

    /// Creates the main fork of `rel` (no WAL record; see `create_rel_logged`).
    pub fn create_rel(&self, rel: RelFileLocator) -> Result<()> {
        self.stack.smgr.create(rel, ForkNumber::Main)
    }

    /// Creates the main fork of `rel` through `smgr_wal::log_and_create`.
    pub fn create_rel_logged(&self, rel: RelFileLocator) -> Result<()> {
        log_and_create(
            &self.stack.wal,
            &self.stack.smgr,
            Xid::BOOTSTRAP,
            rel,
            ForkNumber::Main,
        )
    }

    /// Stands in for a rmgr's WAL record in tests: inserts an `XLOG_NOOP`
    /// record that registers the page of `g`, then stamps the page with its
    /// end LSN (`m3.md` §3 rule 1, steps 5 and 6). Call it after `page_mut()`.
    pub fn log_change(&self, buf: &PinnedBuffer, g: &mut PageWriteGuard<'_>) -> Result<Lsn> {
        let mut rec = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid::INVALID);
        rec.register_block(buf.tag(), g.page(), RegFlags::STANDARD);
        let end = self.stack.wal.insert(rec)?.end;
        g.set_lsn(end.0);
        Ok(end)
    }

    /// The end-of-test check: no pins left (and, on the calling thread, no
    /// leaked pins or latches).
    pub fn assert_clean(&self) {
        assert_eq!(self.stack.pool.pinned_frames(), 0, "buffer pins leaked");
        super::buffer::assert_no_pins();
    }
}

impl Default for TestStorage {
    fn default() -> Self {
        TestStorage::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_reopens_after_a_crash() {
        let ts = TestStorage::new();
        let rel = test_rel(16384);
        ts.create_rel(rel).unwrap();
        {
            let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
            let mut g = buf.write().unwrap();
            g.page_mut().init_heap();
            g.page_mut().add_item(b"hello").unwrap();
            ts.log_change(&buf, &mut g).unwrap();
        }
        ts.assert_clean();
        ts.pool().flush_all_for_checkpoint().unwrap();
        ts.smgr().sync_pending().unwrap();
        let ts2 = ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        let buf = ts2.pool().read_buffer(test_tag(rel, 0)).unwrap();
        assert_eq!(buf.read().unwrap().item(1).unwrap(), b"hello");
        drop(buf);
        ts2.assert_clean();
    }

    #[test]
    fn the_wal_survives_a_crash_and_continues_after_its_valid_end() {
        let ts = TestStorage::new();
        let rel = test_rel(16385);
        ts.create_rel_logged(rel).unwrap();
        let before = ts.wal().insert_lsn();
        ts.wal().flush(before).unwrap();
        // An unflushed record is lost in the crash.
        let rec = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid::INVALID);
        ts.wal().insert(rec).unwrap();
        let ts2 = ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        assert_eq!(ts2.wal().insert_lsn(), before);
        assert_eq!(ts2.wal().flushed_lsn(), before);
        let rec = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid::INVALID);
        let ins = ts2.wal().insert(rec).unwrap();
        assert_eq!(ins.start, before);
    }

    #[test]
    fn stack_rejects_zero_sizes() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let wal =
            Wal::initialize(Arc::clone(&vfs), wal_config(&TestStorageOptions::default())).unwrap();
        for (seg, frames) in [(0, 4), (4, 0)] {
            let cfg = StackConfig {
                rel_seg_blocks: seg,
                nframes: frames,
                knobs: DebugKnobs::default(),
            };
            assert!(StorageStack::new(Arc::clone(&vfs), &cfg, Arc::clone(&wal), Xid(3)).is_err());
        }
    }
}
