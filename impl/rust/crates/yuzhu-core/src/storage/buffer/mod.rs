//! Buffer pool: frames, pins, latches, clock-sweep replacement
//! (`m2.md` §4.3, §6.3).
//!
//! Lock order (levels from `m2.md` §5.9): page content latch -> relation
//! extension lock (`ext_locks`) -> one mapping-table partition -> frame
//! header (leaf). No I/O is done while holding a partition or a header.

pub mod clock;
pub mod flush;
pub mod frame;
pub mod guard;
pub mod table;
pub mod track;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use self::clock::ClockHand;
use self::flush::FlushOutcome;
use self::frame::{Frame, FrameHeader, FrameId};
pub use self::guard::{PageReadGuard, PageWriteGuard};
use self::table::MappingTable;
use self::track::LatchMode;
use super::page::Page;
use super::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator, StorageManager, relpath};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity};
use crate::util::sync::{lock, lock_ignore_poison, wait};

/// `BM_MAX_USAGE_COUNT`.
pub const MAX_USAGE_COUNT: u8 = 5;

/// WAL hook for WAL-before-data (`m3.md` §6.5.3). The only real
/// implementation is `wal::Wal`; [`NoWal`] is for tests without a WAL.
pub trait WalFlush: Send + Sync + std::fmt::Debug {
    /// Makes the WAL durable up to `lsn` (a page LSN, the end of its last
    /// record).
    fn flush_to(&self, lsn: u64) -> Result<()>;
    fn redo_ptr(&self) -> u64;
    /// The durable end of the WAL, for the `assert_wal_before_data` check.
    /// The default (`u64::MAX`) means "unknown": the check passes.
    fn flushed_ptr(&self) -> u64 {
        u64::MAX
    }
}

/// No WAL: nothing to flush (unit tests of the pool and the smgr).
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

#[derive(Debug, Default)]
struct Stats {
    hits: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    evictions: AtomicU64,
    write_errors: AtomicU64,
}

static NEXT_POOL_ID: AtomicUsize = AtomicUsize::new(1);

type ExtLocks = Mutex<HashMap<(RelFileLocator, ForkNumber), Arc<Mutex<()>>>>;

pub struct BufferPool {
    id: usize,
    frames: Box<[Frame]>,
    table: MappingTable,
    free_list: Mutex<Vec<FrameId>>,
    clock: ClockHand,
    smgr: Arc<StorageManager>,
    wal: Arc<dyn WalFlush>,
    knobs: DebugKnobs,
    poison: Arc<PoisonFlag>,
    ext_locks: ExtLocks,
    stats: Stats,
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferPool")
            .field("id", &self.id)
            .field("nframes", &self.frames.len())
            .field("poisoned", &self.poison.is_set())
            .finish_non_exhaustive()
    }
}

fn unmapped_internal(what: &str, tag: BufferTag) -> Error {
    Error::internal(format!(
        "{what}: block {} of relation {}",
        tag.block,
        relpath(tag.rel, tag.fork, 0).display()
    ))
}

impl BufferPool {
    pub fn new(
        nframes: usize,
        smgr: Arc<StorageManager>,
        wal: Arc<dyn WalFlush>,
        knobs: DebugKnobs,
    ) -> Arc<Self> {
        assert!(nframes > 0, "the buffer pool needs at least one frame");
        let n = u32::try_from(nframes).expect("nframes fits u32");
        Arc::new(BufferPool {
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            frames: (0..nframes).map(|_| Frame::new()).collect(),
            table: MappingTable::new(),
            free_list: Mutex::new((0..n).rev().map(FrameId).collect()),
            clock: ClockHand::default(),
            smgr,
            wal,
            knobs,
            poison: Arc::new(PoisonFlag::default()),
            ext_locks: Mutex::new(HashMap::new()),
            stats: Stats::default(),
        })
    }

    pub fn smgr(&self) -> &Arc<StorageManager> {
        &self.smgr
    }

    pub fn nframes(&self) -> usize {
        self.frames.len()
    }

    fn frame(&self, fid: FrameId) -> &Frame {
        &self.frames[fid.index()]
    }

    fn header(&self, fid: FrameId) -> Result<MutexGuard<'_, FrameHeader>> {
        lock(&self.frame(fid).header)
    }

    /// Drops one pin; an unmapped frame whose last pin goes away returns to
    /// the free list. Safe to call from `Drop` (never fails, never does I/O).
    fn unpin(&self, fid: FrameId) {
        let free = {
            let mut h = lock_ignore_poison(&self.frame(fid).header);
            debug_assert!(h.pin_count > 0, "unpin of an unpinned frame");
            h.pin_count = h.pin_count.saturating_sub(1);
            h.pin_count == 0 && h.tag.is_none()
        };
        if free {
            lock_ignore_poison(&self.free_list).push(fid);
        }
    }

    fn make_pinned(self: &Arc<Self>, fid: FrameId, tag: BufferTag) -> PinnedBuffer {
        track::pin_added(self.id, fid, tag);
        PinnedBuffer {
            pool: Arc::clone(self),
            frame: fid,
            tag,
            _not_send: PhantomData,
        }
    }

    /// Looks `tag` up and pins the frame if present.
    fn lookup_and_pin(&self, tag: BufferTag) -> Result<Option<FrameId>> {
        let map = lock(self.table.part(&tag))?;
        let Some(&fid) = map.get(&tag) else {
            return Ok(None);
        };
        let mut h = self.header(fid)?;
        debug_assert_eq!(h.tag, Some(tag));
        h.pin_count += 1;
        Self::bump_usage(&mut h.usage_count);
        Ok(Some(fid))
    }

    /// Waits until a page being read has been loaded. False if the load
    /// failed (the frame was unmapped) and the caller must retry.
    fn wait_valid(&self, fid: FrameId, tag: BufferTag) -> Result<bool> {
        let frame = self.frame(fid);
        let mut h = lock(&frame.header)?;
        while !h.valid && h.io_in_progress {
            h = wait(&frame.io_done, h)?;
        }
        Ok(h.valid && h.tag == Some(tag))
    }

    /// `ReadBuffer`: pins the block, reading it from disk if needed. No
    /// latch is taken.
    pub fn read_buffer(self: &Arc<Self>, tag: BufferTag) -> Result<PinnedBuffer> {
        self.get_buffer(tag, false)
    }

    /// M3 REDO: extends the file to `tag.block` and pins a zero page
    /// without reading. Unused in M2.
    pub fn read_buffer_zeroed(self: &Arc<Self>, tag: BufferTag) -> Result<PinnedBuffer> {
        self.smgr.extend_to(tag.rel, tag.fork, tag.block)?;
        self.get_buffer(tag, true)
    }

    fn get_buffer(self: &Arc<Self>, tag: BufferTag, zero: bool) -> Result<PinnedBuffer> {
        loop {
            if let Some(fid) = self.lookup_and_pin(tag)? {
                if self.wait_valid(fid, tag)? {
                    self.stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(self.make_pinned(fid, tag));
                }
                self.unpin(fid);
                continue;
            }
            let fid = self.get_victim()?;
            if !self.register(fid, tag)? {
                // Someone else mapped the tag first; use theirs.
                self.unpin(fid);
                continue;
            }
            return match self.load_frame(fid, tag, zero) {
                Ok(()) => {
                    if !zero {
                        self.stats.reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(self.make_pinned(fid, tag))
                }
                Err(e) => {
                    self.abort_load(fid, tag);
                    Err(e)
                }
            };
        }
    }

    /// Maps `tag` to the victim `fid` (I/O in progress, not yet valid).
    /// False if the tag is already mapped.
    fn register(&self, fid: FrameId, tag: BufferTag) -> Result<bool> {
        let mut map = lock(self.table.part(&tag))?;
        if map.contains_key(&tag) {
            return Ok(false);
        }
        map.insert(tag, fid);
        let mut h = self.header(fid)?;
        h.tag = Some(tag);
        h.valid = false;
        h.dirty = false;
        h.just_dirtied = false;
        h.io_in_progress = true;
        h.io_error = false;
        h.checkpoint_needed = false;
        h.usage_count = 1;
        Ok(true)
    }

    /// Reads (or zeroes) and verifies the page of a registered frame, then
    /// marks it valid and wakes the waiters.
    fn load_frame(&self, fid: FrameId, tag: BufferTag, zero: bool) -> Result<()> {
        let frame = self.frame(fid);
        {
            let mut latch = frame.content.write().map_err(|_| self.poisoned_latch())?;
            if zero {
                latch.0.fill(0);
            } else {
                self.smgr.read_block(tag, &mut latch.0)?;
                latch.verify(tag.block).map_err(|pe| {
                    pe.to_error(
                        tag.block,
                        &relpath(tag.rel, tag.fork, 0).display().to_string(),
                    )
                })?;
            }
        }
        let mut h = self.header(fid)?;
        h.valid = true;
        h.io_in_progress = false;
        drop(h);
        frame.io_done.notify_all();
        Ok(())
    }

    /// A failed load: unmaps the frame, wakes the waiters (who retry) and
    /// drops the loader's pin (the frame returns to the free list).
    fn abort_load(&self, fid: FrameId, tag: BufferTag) {
        {
            let mut map = lock_ignore_poison(self.table.part(&tag));
            if map.get(&tag) == Some(&fid) {
                map.remove(&tag);
            }
            lock_ignore_poison(&self.frame(fid).header).clear_mapping();
        }
        self.frame(fid).io_done.notify_all();
        self.unpin(fid);
    }

    fn ext_lock(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<Arc<Mutex<()>>> {
        Ok(Arc::clone(
            lock(&self.ext_locks)?
                .entry((rel, fork))
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    /// Extends the relation by one block and pins the zero page. The caller
    /// must hold no page latch (§6.3 item 10).
    pub fn extend(self: &Arc<Self>, rel: RelFileLocator, fork: ForkNumber) -> Result<PinnedBuffer> {
        track::assert_no_latches_for_extend(self.id);
        self.extend_inner(rel, fork)
    }

    /// B+Tree variant of [`extend`](Self::extend): the caller may hold page
    /// latches (a split allocates the new page while latching the old one).
    /// The extension lock never waits for a page latch (eviction writes use
    /// `try_read`), so this cannot deadlock (`06-btree.md` §4.3).
    pub fn extend_tree(
        self: &Arc<Self>,
        rel: RelFileLocator,
        fork: ForkNumber,
    ) -> Result<PinnedBuffer> {
        self.extend_inner(rel, fork)
    }

    fn extend_inner(
        self: &Arc<Self>,
        rel: RelFileLocator,
        fork: ForkNumber,
    ) -> Result<PinnedBuffer> {
        let ext = self.ext_lock(rel, fork)?;
        let _ext = lock(&ext)?;
        let n = self.smgr.nblocks(rel, fork)?;
        let tag = BufferTag {
            rel,
            fork,
            block: n,
        };
        let fid = self.get_victim()?;
        match self.register(fid, tag) {
            Ok(true) => {}
            Ok(false) => {
                self.unpin(fid);
                return Err(unmapped_internal(
                    "buffer already mapped for a block beyond the end of the relation",
                    tag,
                ));
            }
            Err(e) => {
                self.unpin(fid);
                return Err(e);
            }
        }
        match self.finish_extend(fid, tag) {
            Ok(()) => Ok(self.make_pinned(fid, tag)),
            Err(e) => {
                self.abort_load(fid, tag);
                Err(e)
            }
        }
    }

    fn finish_extend(&self, fid: FrameId, tag: BufferTag) -> Result<()> {
        let got = self.smgr.extend(tag.rel, tag.fork)?;
        if got != tag.block {
            return Err(Error::internal(format!(
                "relation extended to block {got}, expected {}",
                tag.block
            )));
        }
        self.load_frame(fid, tag, true)
    }

    pub fn nblocks(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<BlockNumber> {
        self.smgr.nblocks(rel, fork)
    }

    /// Writes every buffer that is dirty when the call starts (`BufferSync`).
    /// Does not fsync: the checkpoint calls `smgr.sync_pending` afterwards.
    pub fn flush_all_for_checkpoint(&self) -> Result<FlushStats> {
        for i in 0..self.frames.len() {
            let mut h = self.header(FrameId(u32::try_from(i).expect("fits")))?;
            if h.valid && h.dirty && h.tag.is_some() {
                h.checkpoint_needed = true;
            }
        }
        let mut stats = FlushStats::default();
        let mut failure = None;
        for i in 0..self.frames.len() {
            let fid = FrameId(u32::try_from(i).expect("fits"));
            let tag = {
                let mut h = self.header(fid)?;
                if !h.checkpoint_needed {
                    continue;
                }
                h.checkpoint_needed = false;
                match h.tag {
                    Some(tag) if h.valid => {
                        h.pin_count += 1;
                        tag
                    }
                    _ => continue,
                }
            };
            let r = self.flush_frame(fid, true, false);
            self.unpin(fid);
            match r {
                Ok(FlushOutcome::Written) => stats.written += 1,
                Ok(_) => stats.skipped_clean += 1,
                Err(e) => {
                    failure = Some(e.with_detail(format!(
                        "while writing block {} of relation {}",
                        tag.block,
                        relpath(tag.rel, tag.fork, 0).display()
                    )));
                    break;
                }
            }
        }
        if let Some(e) = failure {
            for i in 0..self.frames.len() {
                lock_ignore_poison(&self.frames[i].header).checkpoint_needed = false;
            }
            return Err(e);
        }
        Ok(stats)
    }

    /// Discards a relation's buffers without writing (DROP). Pinned buffers
    /// are an internal error.
    pub fn drop_relation_buffers(&self, rel: RelFileLocator) -> Result<()> {
        self.drop_buffers_where(|t| t.rel == rel)
    }

    /// TRUNCATE: discards the buffers of blocks `nblocks..` of one fork
    /// without writing them. A pinned buffer among them is an internal
    /// error (the buffers before it are already gone).
    pub fn drop_relation_buffers_from(
        &self,
        rel: RelFileLocator,
        fork: ForkNumber,
        nblocks: BlockNumber,
    ) -> Result<()> {
        self.drop_buffers_where(|t| t.rel == rel && t.fork == fork && t.block >= nblocks)
    }

    fn drop_buffers_where(&self, matches: impl Fn(&BufferTag) -> bool) -> Result<()> {
        for i in 0..self.frames.len() {
            let fid = FrameId(u32::try_from(i).expect("fits"));
            let Some(tag) = self.header(fid)?.tag.filter(&matches) else {
                continue;
            };
            let mut map = lock(self.table.part(&tag))?;
            let mut h = self.header(fid)?;
            if h.tag != Some(tag) {
                continue;
            }
            if h.pin_count > 0 || h.io_in_progress {
                return Err(unmapped_internal(
                    "cannot drop a buffer that is still in use",
                    tag,
                ));
            }
            map.remove(&tag);
            h.clear_mapping();
            drop(h);
            drop(map);
            lock(&self.free_list)?.push(fid);
        }
        Ok(())
    }

    /// Writes a relation's dirty buffers (before copying a database).
    pub fn flush_relation_buffers(&self, rel: RelFileLocator) -> Result<()> {
        for i in 0..self.frames.len() {
            let fid = FrameId(u32::try_from(i).expect("fits"));
            let tag = {
                let mut h = self.header(fid)?;
                match h.tag {
                    Some(tag) if tag.rel == rel && h.valid && h.dirty => {
                        h.pin_count += 1;
                        tag
                    }
                    _ => continue,
                }
            };
            let r = self.flush_frame(fid, true, false);
            self.unpin(fid);
            r.map_err(|e| {
                e.with_detail(format!(
                    "while writing block {} of relation {}",
                    tag.block,
                    relpath(tag.rel, tag.fork, 0).display()
                ))
            })?;
        }
        Ok(())
    }

    /// Number of frames with at least one pin.
    pub fn pinned_frames(&self) -> usize {
        self.frames
            .iter()
            .filter(|f| lock_ignore_poison(&f.header).pin_count > 0)
            .count()
    }

    /// Number of dirty frames (for tests and monitoring).
    pub fn dirty_frames(&self) -> usize {
        self.frames
            .iter()
            .filter(|f| lock_ignore_poison(&f.header).dirty)
            .count()
    }

    pub fn stats(&self) -> BufferStatsSnapshot {
        BufferStatsSnapshot {
            hits: self.stats.hits.load(Ordering::Relaxed),
            reads: self.stats.reads.load(Ordering::Relaxed),
            writes: self.stats.writes.load(Ordering::Relaxed),
            evictions: self.stats.evictions.load(Ordering::Relaxed),
            write_errors: self.stats.write_errors.load(Ordering::Relaxed),
        }
    }

    /// The flag shared with [`CriticalSection`].
    pub fn poison_flag(&self) -> &Arc<PoisonFlag> {
        &self.poison
    }

    pub fn is_poisoned(&self) -> bool {
        self.poison.is_set()
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
    pool: Arc<BufferPool>,
    frame: FrameId,
    tag: BufferTag,
    _not_send: PhantomData<*const ()>,
}

impl PinnedBuffer {
    pub fn tag(&self) -> BufferTag {
        self.tag
    }

    /// Shared latch. A poisoned latch is a `Severity::Panic` error.
    pub fn read(&self) -> Result<PageReadGuard<'_>> {
        let frame = self.pool.frame(self.frame);
        track::latch_acquired(self.pool.id, self.frame, self.tag, LatchMode::Read);
        let Ok(latch) = frame.content.read() else {
            track::latch_released(self.pool.id, self.frame);
            return Err(self.pool.poisoned_latch());
        };
        Ok(PageReadGuard { pin: self, latch })
    }

    /// Shared latch for B+Tree pages: no ascending-block-order debug check
    /// (the tree order is the caller's duty); double latches still panic.
    pub fn read_tree(&self) -> Result<PageReadGuard<'_>> {
        let frame = self.pool.frame(self.frame);
        track::latch_acquired_tree(self.pool.id, self.frame, self.tag, LatchMode::Read);
        let Ok(latch) = frame.content.read() else {
            track::latch_released(self.pool.id, self.frame);
            return Err(self.pool.poisoned_latch());
        };
        Ok(PageReadGuard { pin: self, latch })
    }

    /// Exclusive latch for B+Tree pages (see [`read_tree`](Self::read_tree)).
    pub fn write_tree(&self) -> Result<PageWriteGuard<'_>> {
        let frame = self.pool.frame(self.frame);
        track::latch_acquired_tree(self.pool.id, self.frame, self.tag, LatchMode::Write);
        let Ok(latch) = frame.content.write() else {
            track::latch_released(self.pool.id, self.frame);
            return Err(self.pool.poisoned_latch());
        };
        Ok(self.write_guard(latch))
    }

    /// Exclusive latch. A poisoned latch is a `Severity::Panic` error.
    pub fn write(&self) -> Result<PageWriteGuard<'_>> {
        let frame = self.pool.frame(self.frame);
        track::latch_acquired(self.pool.id, self.frame, self.tag, LatchMode::Write);
        let Ok(latch) = frame.content.write() else {
            track::latch_released(self.pool.id, self.frame);
            return Err(self.pool.poisoned_latch());
        };
        Ok(self.write_guard(latch))
    }

    /// Exclusive latch if it is free right now (`ConditionalLockBuffer`).
    pub fn try_write(&self) -> Result<Option<PageWriteGuard<'_>>> {
        let frame = self.pool.frame(self.frame);
        match frame.content.try_write() {
            Ok(latch) => {
                track::latch_acquired_nowait(self.pool.id, self.frame, self.tag, LatchMode::Write);
                Ok(Some(self.write_guard(latch)))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Poisoned(_)) => Err(self.pool.poisoned_latch()),
        }
    }

    fn write_guard<'a>(
        &'a self,
        latch: std::sync::RwLockWriteGuard<'a, Box<Page>>,
    ) -> PageWriteGuard<'a> {
        PageWriteGuard {
            pin: self,
            latch,
            dirtied: false,
            hint_only: false,
            lsn_set: false,
        }
    }
}

impl Clone for PinnedBuffer {
    /// Takes another pin.
    fn clone(&self) -> Self {
        {
            let mut h = lock_ignore_poison(&self.pool.frame(self.frame).header);
            h.pin_count += 1;
        }
        track::pin_added(self.pool.id, self.frame, self.tag);
        PinnedBuffer {
            pool: Arc::clone(&self.pool),
            frame: self.frame,
            tag: self.tag,
            _not_send: PhantomData,
        }
    }
}

impl Drop for PinnedBuffer {
    fn drop(&mut self) {
        track::pin_removed(self.pool.id, self.frame);
        self.pool.unpin(self.frame);
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
    if let Some(desc) = track::describe_pins() {
        #[cfg(debug_assertions)]
        panic!("buffer refcount leak: {desc}");
        #[cfg(not(debug_assertions))]
        {
            // There is no logging facility in yuzhu-core yet.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("WARNING:  buffer refcount leak: {desc}");
            }
        }
    }
}
