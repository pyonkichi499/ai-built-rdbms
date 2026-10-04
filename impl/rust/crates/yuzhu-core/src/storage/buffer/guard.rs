//! Page latch guards (`m2.md` §4.3, §6.3 item 5).
//!
//! 担当 C が実装する。ここにあるのは型と単純なアクセサだけで、Drop での
//! dirty の記録、ラッチの追跡は未実装。

use std::ops::Deref;
use std::sync::{RwLockReadGuard, RwLockWriteGuard};

use super::PinnedBuffer;
use crate::storage::page::Page;

/// Shared latch on a page. Derefs to [`Page`].
#[derive(Debug)]
pub struct PageReadGuard<'a> {
    #[allow(dead_code)]
    pub(super) pin: &'a PinnedBuffer,
    pub(super) latch: RwLockReadGuard<'a, Box<Page>>,
}

impl Deref for PageReadGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.latch
    }
}

/// Exclusive latch on a page. Marks the buffer dirty on drop when
/// `page_mut` was called (C implements the `Drop`).
#[derive(Debug)]
pub struct PageWriteGuard<'a> {
    #[allow(dead_code)]
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

    /// Hint-bit-only changes (M3 and later).
    pub fn page_mut_hint(&mut self) -> &mut Page {
        self.hint_only = true;
        &mut self.latch
    }

    /// Records the page LSN after a WAL record was written (M3).
    pub fn set_lsn(&mut self, _lsn: u64) {
        self.lsn_set = true;
    }
}
