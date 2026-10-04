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

use std::sync::Arc;

use super::DEFAULT_RELSEG_SIZE;
use super::buffer::BufferPool;
use super::heap_store::HeapStore;
use super::smgr::{
    BlockNumber, BufferTag, DEFAULTTABLESPACE_OID, ForkNumber, RelFileLocator, RelFileNumber,
    StorageManager,
};
use super::stack::StorageStack;
use super::vfs::{CrashMode, SimVfs};
use crate::error::Result;
use crate::txn::Xid;
use crate::txn::clog::Clog;

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
}

impl Default for TestStorageOptions {
    fn default() -> Self {
        TestStorageOptions {
            seed: 1,
            nframes: 64,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            next_xid: Xid::FIRST_NORMAL,
        }
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

    /// Builds a stack on an existing `SimVfs` (e.g. after a crash).
    pub fn over(vfs: SimVfs, options: TestStorageOptions) -> Result<TestStorage> {
        let stack = StorageStack::new(
            Arc::new(vfs.clone()),
            options.rel_seg_blocks,
            options.nframes,
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

    /// Creates the main fork of `rel`.
    pub fn create_rel(&self, rel: RelFileLocator) -> Result<()> {
        self.stack.smgr.create(rel, ForkNumber::Main)
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
}
