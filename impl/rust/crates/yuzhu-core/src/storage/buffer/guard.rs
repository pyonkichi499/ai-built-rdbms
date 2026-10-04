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

/// Exclusive latch on a page. When `page_mut` (or `page_mut_hint`) was
/// called, the buffer is marked dirty on drop, before the latch is released.
/// Not marked when the thread is panicking: the latch is poisoned instead
/// and the page is never written.
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

    /// Mutable access; the buffer becomes dirty when the guard is dropped.
    /// Nothing that can fail may follow this call (D14).
    pub fn page_mut(&mut self) -> &mut Page {
        self.dirtied = true;
        &mut self.latch
    }

    /// Hint-bit-only changes (M3 and later). In M2 this dirties the buffer
    /// like `page_mut`; the difference only matters for the M3 check that
    /// every `page_mut` is followed by `set_lsn`.
    pub fn page_mut_hint(&mut self) -> &mut Page {
        self.hint_only = true;
        &mut self.latch
    }

    /// Records the page LSN after a WAL record was written (M3).
    pub fn set_lsn(&mut self, lsn: u64) {
        self.lsn_set = true;
        self.latch.set_lsn(lsn);
    }

    /// Whether `page_mut` was called without `set_lsn` (for the M3 check).
    pub fn lsn_missing(&self) -> bool {
        self.dirtied && !self.lsn_set
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        if (self.dirtied || self.hint_only) && !std::thread::panicking() {
            let pool = &self.pin.pool;
            let mut h = lock_ignore_poison(&pool.frame(self.pin.frame).header);
            h.dirty = true;
            h.just_dirtied = true;
        }
        track::latch_released(self.pin.pool.id, self.pin.frame);
        // The field `latch` is dropped after this body: the dirty mark is
        // already set while the latch is still held.
    }
}
