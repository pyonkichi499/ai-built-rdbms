//! Buffer frames: header (tag, flags, pin/usage counts), content latch and
//! the I/O condition variable (`m2.md` §6.3 items 1, 4, 5).

use std::sync::{Condvar, Mutex, RwLock};

use crate::storage::page::Page;
use crate::storage::smgr::BufferTag;

/// Index of a frame in the pool.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct FrameId(pub u32);

impl FrameId {
    pub(super) fn index(self) -> usize {
        self.0 as usize
    }
}

/// Per-frame state, protected by the frame's mutex (a leaf lock: nothing
/// else is taken and no I/O is done while holding it).
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct FrameHeader {
    /// `None` = not mapped (`BM_TAG_VALID` cleared). A frame with no tag and
    /// no pins is on the free list.
    pub(super) tag: Option<BufferTag>,
    /// The content has been loaded (`BM_VALID`).
    pub(super) valid: bool,
    pub(super) dirty: bool,
    /// Set whenever the page is modified; cleared when a write starts
    /// (`BM_JUST_DIRTIED`). The write clears `dirty` only if it is still unset.
    pub(super) just_dirtied: bool,
    pub(super) io_in_progress: bool,
    /// The last write failed (`BM_IO_ERROR`); the frame stays valid and dirty.
    pub(super) io_error: bool,
    pub(super) checkpoint_needed: bool,
    pub(super) pin_count: u32,
    pub(super) usage_count: u8,
}

impl FrameHeader {
    /// Forgets the page: the frame is no longer mapped.
    pub(super) fn clear_mapping(&mut self) {
        self.tag = None;
        self.valid = false;
        self.dirty = false;
        self.just_dirtied = false;
        self.io_in_progress = false;
        self.io_error = false;
        self.checkpoint_needed = false;
        self.usage_count = 0;
    }
}

#[derive(Debug)]
pub(super) struct Frame {
    pub(super) header: Mutex<FrameHeader>,
    /// Signalled when `io_in_progress` is cleared.
    pub(super) io_done: Condvar,
    /// The content latch.
    pub(super) content: RwLock<Box<Page>>,
}

impl Frame {
    pub(super) fn new() -> Frame {
        Frame {
            header: Mutex::new(FrameHeader::default()),
            io_done: Condvar::new(),
            content: RwLock::new(Box::new(Page::zeroed())),
        }
    }
}
