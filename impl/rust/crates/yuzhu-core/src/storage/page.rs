//! Page accessors, line pointers and page checks (`m2.md` §3.3, §3.4, §6.4).
//!
//! 担当 C が実装する。

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
#![allow(clippy::unimplemented)]

use super::BLCKSZ;
use super::smgr::BlockNumber;

/// One 8KB page. Frames hold `Box<Page>`.
#[derive(Clone, Debug)]
pub struct Page(pub [u8; BLCKSZ]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LpFlags {
    Unused = 0,
    Normal = 1,
    Redirect = 2,
    Dead = 3,
}

/// A decoded line pointer (§3.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemId {
    pub off: u16,
    pub flags: LpFlags,
    pub len: u16,
}

/// Page-level damage; converted to `XX001` by the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageError {
    BadHeader(&'static str),
    NotAllZero,
    ChecksumMismatch { stored: u16, computed: u16 },
    BadItem { off: u16 },
}

impl Page {
    /// An all-zero page.
    pub fn zeroed() -> Page {
        Page([0; BLCKSZ])
    }

    pub fn init_heap(&mut self) {
        unimplemented!("担当 C が実装")
    }

    /// `pd_upper == 0`.
    pub fn is_new(&self) -> bool {
        unimplemented!("担当 C が実装")
    }

    pub fn is_all_zero(&self) -> bool {
        unimplemented!("担当 C が実装")
    }

    pub fn lsn(&self) -> u64 {
        unimplemented!("担当 C が実装")
    }

    pub fn lower(&self) -> u16 {
        unimplemented!("担当 C が実装")
    }

    pub fn upper(&self) -> u16 {
        unimplemented!("担当 C が実装")
    }

    pub fn special(&self) -> u16 {
        unimplemented!("担当 C が実装")
    }

    /// Number of line pointers.
    pub fn max_offset(&self) -> u16 {
        unimplemented!("担当 C が実装")
    }

    /// `off` is 1-based. Out-of-range offsets give `PageError::BadItem`.
    pub fn item_id(&self, _off: u16) -> std::result::Result<ItemId, PageError> {
        unimplemented!("担当 C が実装")
    }

    /// The bytes of an `LP_NORMAL` tuple (range-checked like `item_id`).
    pub fn item(&self, _off: u16) -> std::result::Result<&[u8], PageError> {
        unimplemented!("担当 C が実装")
    }

    pub fn item_mut(&mut self, _off: u16) -> std::result::Result<&mut [u8], PageError> {
        unimplemented!("担当 C が実装")
    }

    /// `None` if it does not fit (or the page already has
    /// `MAX_HEAP_TUPLES_PER_PAGE` items).
    pub fn add_item(&mut self, _data: &[u8]) -> Option<u16> {
        unimplemented!("担当 C が実装")
    }

    /// Free bytes minus one new line pointer (`PageGetHeapFreeSpace`).
    pub fn free_space(&self) -> usize {
        unimplemented!("担当 C が実装")
    }

    /// Header invariants + the all-zero check (D12) + the checksum.
    pub fn verify(&self, _blkno: BlockNumber) -> std::result::Result<(), PageError> {
        unimplemented!("担当 C が実装")
    }
}
