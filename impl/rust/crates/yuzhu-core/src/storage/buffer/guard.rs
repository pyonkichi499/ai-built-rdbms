//! Page latch guards (`m2.md` §4.3, §6.3 items 5, 7, 8).
//!
//! The guards borrow the [`PinnedBuffer`], so a latch cannot outlive its
//! pin. Releasing a latch is bookkeeping only (no I/O).

use std::ops::Deref;
use std::sync::{RwLockReadGuard, RwLockWriteGuard};

use super::PinnedBuffer;
use super::track;
use crate::storage::page::Page;
use crate::util::sync::lock_ignore_poison;

/// Shared latch on a page. Derefs to [`Page`].
#[derive(Debug)]
pub struct PageReadGuard<'a> {
    pub(super) pin: &'a PinnedBuffer,
    pub(super) latch: RwLockReadGuard<'a, Box<Page>>,
}

impl Deref for PageReadGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.latch
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        track::latch_released(self.pin.pool.id, self.pin.frame);
    }
}

/// Exclusive latch on a page. `page_mut` marks the buffer dirty at once, in
/// the frame header's mutex while the latch is held (D11): a checkpoint that
/// scans the pool after the WAL insert therefore always sees the page dirty.
/// A guard on which `page_mut` was called must get `set_lsn` before it is
/// dropped (checked in debug builds). If the thread panics while the guard is
/// alive, std poisons the latch and the pool is poisoned: such a page is
/// never written.
#[derive(Debug)]
pub struct PageWriteGuard<'a> {
    pub(super) pin: &'a PinnedBuffer,
    pub(super) latch: RwLockWriteGuard<'a, Box<Page>>,
    pub(super) dirtied: bool,
    pub(super) hint_only: bool,
    pub(super) lsn_set: bool,
}

impl PageWriteGuard<'_> {
    pub fn page(&self) -> &Page {
        &self.latch
    }

    fn mark_dirty(&self) {
        let pool = &self.pin.pool;
        let mut h = lock_ignore_poison(&pool.frame(self.pin.frame).header);
        h.dirty = true;
        h.just_dirtied = true;
    }

    /// Mutable access. The first call marks the buffer dirty. Nothing that
    /// can fail may follow this call except inside a `CriticalSection`
    /// (`m3.md` §3 rule 1), and `set_lsn` must follow before the drop.
    pub fn page_mut(&mut self) -> &mut Page {
        if !self.dirtied && !self.hint_only {
            self.mark_dirty();
        }
        self.dirtied = true;
        &mut self.latch
    }

    /// Hint-bit-only changes (no caller in M3, D7): dirties the buffer but
    /// needs no `set_lsn`.
    pub fn page_mut_hint(&mut self) -> &mut Page {
        if !self.dirtied && !self.hint_only {
            self.mark_dirty();
        }
        self.hint_only = true;
        &mut self.latch
    }

    /// Records the page LSN after a WAL record was written (or, in REDO,
    /// the end of the record being replayed).
    pub fn set_lsn(&mut self, lsn: u64) {
        self.lsn_set = true;
        self.latch.set_lsn(lsn);
    }

    /// Whether `page_mut` was called without `set_lsn`.
    pub fn lsn_missing(&self) -> bool {
        self.dirtied && !self.lsn_set
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        let panicking = std::thread::panicking();
        if panicking && (self.dirtied || self.hint_only) {
            // std poisons the latch; the page may be half-changed.
            self.pin.pool.poison.set();
        }
        track::latch_released(self.pin.pool.id, self.pin.frame);
        debug_assert!(
            panicking || !self.lsn_missing(),
            "page_mut() without set_lsn() on block {} of {:?}",
            self.pin.tag.block,
            self.pin.tag.rel
        );
    }
}
