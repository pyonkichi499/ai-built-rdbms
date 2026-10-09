//! Page accessors, line pointers and page checks (`m2.md` §3.3, §3.4, §6.4).
//!
//! All fields are little-endian and read through `from_le_bytes`; nothing is
//! overlaid on the byte array.

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use std::fmt;

use super::checksum::page_checksum;
use super::smgr::BlockNumber;
use super::{BLCKSZ, MAX_HEAP_TUPLES_PER_PAGE, MAXALIGN, PAGE_LAYOUT_VERSION, SIZE_OF_PAGE_HEADER};
use crate::error::Error;
use crate::txn::Xid;

const OFF_LSN: usize = 0;
const OFF_CHECKSUM: usize = 8;
const OFF_FLAGS: usize = 10;
const OFF_LOWER: usize = 12;
const OFF_UPPER: usize = 14;
const OFF_SPECIAL: usize = 16;
const OFF_VERSION: usize = 18;
const OFF_PRUNE_XID: usize = 20;

/// Size of one line pointer.
const ITEM_ID_SIZE: usize = 4;
/// `pd_pagesize_version`: page size in the high byte (`8192 >> 8 << 8`),
/// layout version in the low byte.
const PAGESIZE_VERSION: u16 = (BLCKSZ as u16) | (PAGE_LAYOUT_VERSION as u16);
/// `PD_HAS_FREE_LINES | PD_PAGE_FULL | PD_ALL_VISIBLE`.
const PD_VALID_FLAG_BITS: u16 = 0x0007;
/// `pd_flags` bit0: at least one `LP_UNUSED` line pointer exists. Kept equal to
/// "an `LP_UNUSED` exists" after every operation that changes line pointer states
/// (`m5/03` VC-D10).
pub const PD_HAS_FREE_LINES: u16 = 0x0001;

/// Offset of `kind` in an FSM page (`m5/03` §3.3).
const OFF_FSM_KIND: usize = 24;
/// Start of `data[8160]` in an FSM page.
const FSM_DATA_START: usize = 32;

/// `kind` byte of an FSM page (`m5/03` §3.3). The FSM module (VC-2) re-exports this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsmPageKind {
    Root = 1,
    Leaf = 2,
}

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

impl LpFlags {
    fn from_bits(bits: u32) -> LpFlags {
        match bits & 3 {
            0 => LpFlags::Unused,
            1 => LpFlags::Normal,
            2 => LpFlags::Redirect,
            _ => LpFlags::Dead,
        }
    }
}

/// A decoded line pointer (§3.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemId {
    pub off: u16,
    pub flags: LpFlags,
    pub len: u16,
}

impl ItemId {
    /// `lp_off | (lp_flags << 15) | (lp_len << 17)`.
    fn encode(self) -> u32 {
        u32::from(self.off) | ((self.flags as u32) << 15) | (u32::from(self.len) << 17)
    }

    fn decode(raw: u32) -> ItemId {
        ItemId {
            off: (raw & 0x7FFF) as u16,
            flags: LpFlags::from_bits(raw >> 15),
            len: (raw >> 17) as u16,
        }
    }
}

/// Page-level damage; converted to `XX001` by the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageError {
    BadHeader(&'static str),
    NotAllZero,
    ChecksumMismatch { stored: u16, computed: u16 },
    BadItem { off: u16 },
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageError::BadHeader(what) => write!(f, "invalid page header: {what}"),
            PageError::NotAllZero => f.write_str("page header is zero but the page is not"),
            PageError::ChecksumMismatch { stored, computed } => {
                write!(
                    f,
                    "page checksum mismatch: stored {stored}, computed {computed}"
                )
            }
            PageError::BadItem { off } => write!(f, "invalid line pointer {off}"),
        }
    }
}

impl std::error::Error for PageError {}

impl PageError {
    /// `XX001 invalid page in block N of relation PATH`, with the cause as
    /// DETAIL.
    pub fn to_error(self, blkno: BlockNumber, relation: &str) -> Error {
        Error::corrupted(format!(
            "invalid page in block {blkno} of relation {relation}"
        ))
        .with_detail(self.to_string())
    }
}

#[inline]
fn align_up(n: usize) -> usize {
    (n + MAXALIGN - 1) & !(MAXALIGN - 1)
}

impl Page {
    /// An all-zero page.
    pub fn zeroed() -> Page {
        Page([0; BLCKSZ])
    }

    fn get_u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.0[off], self.0[off + 1]])
    }

    fn put_u16(&mut self, off: usize, v: u16) {
        self.0[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Initializes an empty heap page (§3.3).
    pub fn init_heap(&mut self) {
        self.0.fill(0);
        self.put_u16(OFF_LOWER, SIZE_OF_PAGE_HEADER as u16);
        self.put_u16(OFF_UPPER, BLCKSZ as u16);
        self.put_u16(OFF_SPECIAL, BLCKSZ as u16);
        self.put_u16(OFF_VERSION, PAGESIZE_VERSION);
    }

    /// Initializes an empty page with a `special_size`-byte special area
    /// (multiple of 8; `06-btree.md` §4.2). Everything else is zeroed.
    pub fn init_special(&mut self, special_size: usize) {
        assert!(
            special_size.is_multiple_of(MAXALIGN) && special_size <= BLCKSZ - SIZE_OF_PAGE_HEADER,
            "invalid special size {special_size}"
        );
        self.0.fill(0);
        let special = BLCKSZ - special_size;
        self.put_u16(OFF_LOWER, SIZE_OF_PAGE_HEADER as u16);
        self.put_u16(OFF_UPPER, special as u16);
        self.put_u16(OFF_SPECIAL, special as u16);
        self.put_u16(OFF_VERSION, PAGESIZE_VERSION);
    }

    /// `pd_special..BLCKSZ`; empty when `pd_special` is out of range.
    pub fn special_area(&self) -> &[u8] {
        let s = usize::from(self.special());
        if !(SIZE_OF_PAGE_HEADER..=BLCKSZ).contains(&s) {
            return &[];
        }
        &self.0[s..]
    }

    pub fn special_area_mut(&mut self) -> &mut [u8] {
        let s = usize::from(self.special());
        if !(SIZE_OF_PAGE_HEADER..=BLCKSZ).contains(&s) {
            return &mut [];
        }
        &mut self.0[s..]
    }

    /// Free bytes minus one new line pointer, without the
    /// `MAX_HEAP_TUPLES_PER_PAGE` limit.
    pub fn free_space_unbounded(&self) -> usize {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper <= lower {
            return 0;
        }
        (upper - lower).saturating_sub(ITEM_ID_SIZE)
    }

    /// Inserts `data` as line pointer `off` (`1..=max_offset + 1`), shifting
    /// the later line pointers back by one. The tuple body goes at the bottom
    /// of the free space; existing bodies do not move. `None` (page untouched)
    /// if it does not fit or `off` is out of range.
    pub fn insert_item_at(&mut self, off: u16, data: &[u8]) -> Option<u16> {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper < lower || lower < SIZE_OF_PAGE_HEADER || upper > usize::from(self.special()) {
            return None;
        }
        let n = self.max_offset();
        if off == 0 || off > n + 1 {
            return None;
        }
        let size = align_up(data.len());
        if size + ITEM_ID_SIZE > upper - lower || data.len() > 0x7FFF {
            return None;
        }
        let new_upper = upper - size;
        self.0[new_upper..upper].fill(0);
        self.0[new_upper..new_upper + data.len()].copy_from_slice(data);
        let pos = Self::item_id_pos(off);
        self.0.copy_within(pos..lower, pos + ITEM_ID_SIZE);
        let id = ItemId {
            off: new_upper as u16,
            flags: LpFlags::Normal,
            len: data.len() as u16,
        };
        self.0[pos..pos + ITEM_ID_SIZE].copy_from_slice(&id.encode().to_le_bytes());
        self.put_u16(OFF_LOWER, (lower + ITEM_ID_SIZE) as u16);
        self.put_u16(OFF_UPPER, new_upper as u16);
        Some(off)
    }

    /// Raw bytes from the end of the page header to `pd_lower` (metapage
    /// payload). Empty when `pd_lower` is out of range.
    pub fn body(&self) -> &[u8] {
        let l = usize::from(self.lower());
        if !(SIZE_OF_PAGE_HEADER..=BLCKSZ).contains(&l) {
            return &[];
        }
        &self.0[SIZE_OF_PAGE_HEADER..l]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        let l = usize::from(self.lower());
        if !(SIZE_OF_PAGE_HEADER..=BLCKSZ).contains(&l) {
            return &mut [];
        }
        &mut self.0[SIZE_OF_PAGE_HEADER..l]
    }

    /// Writes `pd_lower` (range is the caller's responsibility).
    pub fn set_lower(&mut self, lower: u16) {
        self.put_u16(OFF_LOWER, lower);
    }

    /// Builds a whole page from a special area and items in line-pointer
    /// order (bodies packed downward in that order). `None` if they do not
    /// fit. `pd_lsn` and `pd_checksum` are 0.
    pub fn build_with_items(special: &[u8], items: &[&[u8]]) -> Option<Box<Page>> {
        if !special.len().is_multiple_of(MAXALIGN) || special.len() > BLCKSZ - SIZE_OF_PAGE_HEADER {
            return None;
        }
        let mut page = Box::new(Page::zeroed());
        page.init_special(special.len());
        let s = usize::from(page.special());
        page.0[s..].copy_from_slice(special);
        for (i, item) in items.iter().enumerate() {
            page.insert_item_at(u16::try_from(i + 1).ok()?, item)?;
        }
        Some(page)
    }

    /// `pd_upper == 0`. Only meaningful for pages that passed `verify`.
    pub fn is_new(&self) -> bool {
        self.upper() == 0
    }

    pub fn is_all_zero(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }

    pub fn lsn(&self) -> u64 {
        u64::from_le_bytes(self.0[OFF_LSN..OFF_LSN + 8].try_into().expect("8 bytes"))
    }

    pub fn set_lsn(&mut self, lsn: u64) {
        self.0[OFF_LSN..OFF_LSN + 8].copy_from_slice(&lsn.to_le_bytes());
    }

    pub fn checksum(&self) -> u16 {
        self.get_u16(OFF_CHECKSUM)
    }

    pub fn set_checksum(&mut self, sum: u16) {
        self.put_u16(OFF_CHECKSUM, sum);
    }

    pub fn flags(&self) -> u16 {
        self.get_u16(OFF_FLAGS)
    }

    pub fn lower(&self) -> u16 {
        self.get_u16(OFF_LOWER)
    }

    pub fn upper(&self) -> u16 {
        self.get_u16(OFF_UPPER)
    }

    pub fn special(&self) -> u16 {
        self.get_u16(OFF_SPECIAL)
    }

    pub fn prune_xid(&self) -> u32 {
        u32::from_le_bytes(
            self.0[OFF_PRUNE_XID..OFF_PRUNE_XID + 4]
                .try_into()
                .expect("4 bytes"),
        )
    }

    /// Number of line pointers.
    pub fn max_offset(&self) -> u16 {
        let lower = usize::from(self.lower());
        if lower <= SIZE_OF_PAGE_HEADER {
            0
        } else {
            ((lower - SIZE_OF_PAGE_HEADER) / ITEM_ID_SIZE) as u16
        }
    }

    fn item_id_pos(off: u16) -> usize {
        SIZE_OF_PAGE_HEADER + (usize::from(off) - 1) * ITEM_ID_SIZE
    }

    /// `off` is 1-based. Offsets outside `1..=max_offset`, and `LP_NORMAL`
    /// pointers whose tuple lies outside `pd_upper..pd_special`, give
    /// `PageError::BadItem`.
    pub fn item_id(&self, off: u16) -> std::result::Result<ItemId, PageError> {
        let bad = PageError::BadItem { off };
        if off == 0 || off > self.max_offset() {
            return Err(bad);
        }
        // max_offset() derives from pd_lower, which may be garbage on an
        // unverified page; keep the read inside the page.
        let pos = Self::item_id_pos(off);
        if pos + ITEM_ID_SIZE > BLCKSZ {
            return Err(bad);
        }
        let raw = u32::from_le_bytes(self.0[pos..pos + 4].try_into().expect("4 bytes"));
        let id = ItemId::decode(raw);
        match id.flags {
            LpFlags::Normal => {
                let start = usize::from(id.off);
                let end = start + usize::from(id.len);
                if start < usize::from(self.upper()) || end > usize::from(self.special()) {
                    return Err(bad);
                }
            }
            // Canonical form (`m5/03` §3.1): no stray offset or length.
            LpFlags::Unused | LpFlags::Dead => {
                if id.off != 0 || id.len != 0 {
                    return Err(bad);
                }
            }
            LpFlags::Redirect => {}
        }
        Ok(id)
    }

    fn normal_range(&self, off: u16) -> std::result::Result<(usize, usize), PageError> {
        let id = self.item_id(off)?;
        if id.flags != LpFlags::Normal {
            return Err(PageError::BadItem { off });
        }
        let start = usize::from(id.off);
        Ok((start, start + usize::from(id.len)))
    }

    /// The bytes of an `LP_NORMAL` tuple (range-checked like `item_id`).
    pub fn item(&self, off: u16) -> std::result::Result<&[u8], PageError> {
        let (s, e) = self.normal_range(off)?;
        Ok(&self.0[s..e])
    }

    pub fn item_mut(&mut self, off: u16) -> std::result::Result<&mut [u8], PageError> {
        let (s, e) = self.normal_range(off)?;
        Ok(&mut self.0[s..e])
    }

    fn raw_item_id(&self, off: u16) -> ItemId {
        let pos = Self::item_id_pos(off);
        ItemId::decode(u32::from_le_bytes(
            self.0[pos..pos + ITEM_ID_SIZE].try_into().expect("4 bytes"),
        ))
    }

    fn put_item_id(&mut self, off: u16, id: ItemId) {
        let pos = Self::item_id_pos(off);
        self.0[pos..pos + ITEM_ID_SIZE].copy_from_slice(&id.encode().to_le_bytes());
    }

    /// `off` is valid for raw line pointer access: `1..=max_offset` and inside the
    /// page. `max_offset` derives from `pd_lower`, which is garbage on an
    /// unverified page.
    fn off_in_range(&self, off: u16) -> bool {
        off != 0 && off <= self.max_offset() && Self::item_id_pos(off) + ITEM_ID_SIZE <= BLCKSZ
    }

    /// Sets line pointer `off` to `LP_DEAD` (`lp_off = 0`, `lp_len = 0`). The caller
    /// recomputes `PD_HAS_FREE_LINES` at the end of its operation.
    pub fn mark_dead(&mut self, off: u16) {
        debug_assert!(self.off_in_range(off), "mark_dead({off}) out of range");
        if self.off_in_range(off) {
            self.put_item_id(
                off,
                ItemId {
                    off: 0,
                    flags: LpFlags::Dead,
                    len: 0,
                },
            );
        }
    }

    /// Sets line pointer `off` to `LP_UNUSED` (all zero). See [`mark_dead`](Self::mark_dead).
    pub fn mark_unused(&mut self, off: u16) {
        debug_assert!(self.off_in_range(off), "mark_unused({off}) out of range");
        if self.off_in_range(off) {
            self.put_item_id(
                off,
                ItemId {
                    off: 0,
                    flags: LpFlags::Unused,
                    len: 0,
                },
            );
        }
    }

    /// Whether any line pointer is `LP_UNUSED` (a scan over the line pointer array).
    pub fn has_unused_item(&self) -> bool {
        (1..=self.max_offset())
            .filter(|&o| self.off_in_range(o))
            .any(|o| self.raw_item_id(o).flags == LpFlags::Unused)
    }

    /// Recomputes `PD_HAS_FREE_LINES` from the line pointers.
    fn refresh_free_lines_flag(&mut self) {
        let flags = self.flags();
        let flags = if self.has_unused_item() {
            flags | PD_HAS_FREE_LINES
        } else {
            flags & !PD_HAS_FREE_LINES
        };
        self.put_u16(OFF_FLAGS, flags);
    }

    /// The first `LP_UNUSED` line pointer, if `PD_HAS_FREE_LINES` says there is one.
    fn first_unused(&self) -> Option<u16> {
        if self.flags() & PD_HAS_FREE_LINES == 0 {
            return None;
        }
        (1..=self.max_offset())
            .filter(|&o| self.off_in_range(o))
            .find(|&o| self.raw_item_id(o).flags == LpFlags::Unused)
    }

    /// Puts `data` at the bottom of the free space and points line pointer `slot`
    /// at it. `slot == max_offset + 1` appends a line pointer. The caller checked
    /// that it fits.
    fn place_item(&mut self, slot: u16, data: &[u8]) {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        let size = align_up(data.len());
        let new_upper = upper - size;
        self.0[new_upper..upper].fill(0);
        self.0[new_upper..new_upper + data.len()].copy_from_slice(data);
        let append = slot > self.max_offset();
        self.put_item_id(
            slot,
            ItemId {
                off: new_upper as u16,
                flags: LpFlags::Normal,
                len: data.len() as u16,
            },
        );
        if append {
            self.put_u16(OFF_LOWER, (lower + ITEM_ID_SIZE) as u16);
        }
        self.put_u16(OFF_UPPER, new_upper as u16);
        self.refresh_free_lines_flag();
    }

    /// Whether a tuple of `len` bytes fits (`append`: it needs a new line pointer).
    fn item_fits(&self, len: usize, append: bool) -> bool {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper < lower || lower < SIZE_OF_PAGE_HEADER || len > 0x7FFF {
            return false;
        }
        if append && usize::from(self.max_offset()) >= MAX_HEAP_TUPLES_PER_PAGE {
            return false;
        }
        let need = align_up(len) + if append { ITEM_ID_SIZE } else { 0 };
        need <= upper - lower
    }

    /// Adds a tuple and returns its 1-based offset number: the smallest `LP_UNUSED`
    /// line pointer is reused, otherwise one is appended. `None` (page untouched) if
    /// it does not fit. `PD_HAS_FREE_LINES` is recomputed at the end (`m5/03` §6.1).
    pub fn add_item(&mut self, data: &[u8]) -> Option<u16> {
        let slot = self.first_unused();
        if !self.item_fits(data.len(), slot.is_none()) {
            return None;
        }
        let slot = slot.unwrap_or(self.max_offset() + 1);
        self.place_item(slot, data);
        Some(slot)
    }

    /// REDO counterpart of [`add_item`](Self::add_item): puts the tuple at line
    /// pointer `off`, which must be `LP_UNUSED` or `max_offset + 1`. `false` (page
    /// untouched) otherwise or if it does not fit. Given the offset `add_item`
    /// returned on an identical page, the resulting bytes are identical.
    pub fn add_item_at(&mut self, off: u16, data: &[u8]) -> bool {
        if off == 0 || self.upper() == 0 {
            return false;
        }
        let n = self.max_offset();
        let append = if off == n + 1 {
            true
        } else if off <= n
            && self.off_in_range(off)
            && self.raw_item_id(off).flags == LpFlags::Unused
        {
            false
        } else {
            return false;
        };
        if !self.item_fits(data.len(), append) {
            return false;
        }
        self.place_item(off, data);
        true
    }

    /// `PageGetHeapFreeSpace`: the largest `MAXALIGN`ed tuple that can be added. With
    /// an `LP_UNUSED` line pointer the whole `pd_upper - pd_lower` can be used,
    /// otherwise one line pointer is deducted; 0 when no line pointer may be
    /// added (`MAX_HEAP_TUPLES_PER_PAGE` and no `LP_UNUSED`).
    pub fn free_space(&self) -> usize {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper <= lower {
            return 0;
        }
        let space = upper - lower;
        if self.flags() & PD_HAS_FREE_LINES != 0 && self.has_unused_item() {
            return space;
        }
        if usize::from(self.max_offset()) >= MAX_HEAP_TUPLES_PER_PAGE {
            return 0;
        }
        space.saturating_sub(ITEM_ID_SIZE)
    }

    /// Strips the trailing `LP_UNUSED` line pointers (`pd_lower -= 4` each) and
    /// recomputes `PD_HAS_FREE_LINES`. Tuples do not move.
    pub fn finish_unused(&mut self) {
        let mut n = self.max_offset();
        while n > 0 && self.off_in_range(n) && self.raw_item_id(n).flags == LpFlags::Unused {
            n -= 1;
        }
        let new_lower = SIZE_OF_PAGE_HEADER + usize::from(n) * ITEM_ID_SIZE;
        let old_lower = usize::from(self.lower());
        if new_lower < old_lower {
            self.put_u16(OFF_LOWER, new_lower as u16);
            // The stripped line pointers are already zero (`LP_UNUSED`); keep the
            // hole zero even for a page that was not canonical.
            self.0[new_lower..old_lower.min(BLCKSZ)].fill(0);
        }
        self.refresh_free_lines_flag();
    }

    /// Defragments the page deterministically (`m5/03` §3.1): `LP_NORMAL` tuples are
    /// packed against `pd_special` in descending old-offset order, the hole and the
    /// padding are zeroed, trailing `LP_UNUSED` line pointers are dropped and
    /// `PD_HAS_FREE_LINES` is recomputed. The same input gives the same bytes, so
    /// the normal path and REDO agree.
    pub fn repair_fragmentation(&mut self) -> std::result::Result<(), PageError> {
        let n = self.max_offset();
        let special = usize::from(self.special());
        let lower = usize::from(self.lower());
        if self.upper() == 0 || lower < SIZE_OF_PAGE_HEADER || special > BLCKSZ {
            return Err(PageError::BadHeader("cannot defragment this page"));
        }
        let mut items: Vec<(u16, ItemId)> = Vec::with_capacity(usize::from(n));
        for off in 1..=n {
            let id = self.item_id(off)?;
            match id.flags {
                LpFlags::Normal => items.push((off, id)),
                LpFlags::Redirect => return Err(PageError::BadItem { off }),
                LpFlags::Unused | LpFlags::Dead => {}
            }
        }
        items.sort_by(|a, b| b.1.off.cmp(&a.1.off).then(a.0.cmp(&b.0)));
        let mut new_upper = special;
        for &(off, id) in &items {
            let size = align_up(usize::from(id.len));
            if new_upper < lower + size {
                return Err(PageError::BadItem { off });
            }
            new_upper -= size;
            let (src, len) = (usize::from(id.off), usize::from(id.len));
            if new_upper < src {
                // Packing never moves a tuple down unless tuples overlap.
                return Err(PageError::BadItem { off });
            }
            self.0.copy_within(src..src + len, new_upper);
            self.0[new_upper + len..new_upper + size].fill(0);
            self.put_item_id(
                off,
                ItemId {
                    off: new_upper as u16,
                    flags: LpFlags::Normal,
                    len: id.len,
                },
            );
        }
        self.0[lower..new_upper].fill(0);
        self.put_u16(OFF_UPPER, new_upper as u16);
        self.finish_unused();
        let (lower, upper) = (usize::from(self.lower()), usize::from(self.upper()));
        self.0[lower..upper].fill(0);
        Ok(())
    }

    /// `PageSetPrunable`: records `xid` in `pd_prune_xid` when there is no hint yet
    /// or `xid` (low 32 bits) is older than the current one (signed 32-bit difference).
    pub fn set_prunable(&mut self, xid: Xid) {
        let new = xid.to_external();
        let cur = self.prune_xid();
        if cur == 0 || new.wrapping_sub(cur).cast_signed() < 0 {
            self.set_prune_xid(new);
        }
    }

    /// Writes `pd_prune_xid` (the low 32 bits of an XID; 0 = no hint).
    pub fn set_prune_xid(&mut self, v32: u32) {
        self.0[OFF_PRUNE_XID..OFF_PRUNE_XID + 4].copy_from_slice(&v32.to_le_bytes());
    }

    /// Restores `pd_prune_xid` to 64 bits using `next_xid` (`m5/03` §3.1); `None` for 0.
    pub fn prune_xid_full(&self, next_xid: Xid) -> Option<Xid> {
        let h32 = self.prune_xid();
        if h32 == 0 {
            return None;
        }
        let mut full = (next_xid.0 & !0xFFFF_FFFF) | u64::from(h32);
        if full > next_xid.0 {
            full = full.checked_sub(1 << 32)?;
        }
        Some(Xid(full))
    }

    /// Initializes an FSM page of the given kind (`m5/03` §3.3): the standard 24-byte
    /// header with `pd_lower = 32`, `kind`, and `data` all zero.
    pub fn init_fsm(&mut self, kind: FsmPageKind) {
        self.0.fill(0);
        self.put_u16(OFF_LOWER, FSM_DATA_START as u16);
        self.put_u16(OFF_UPPER, BLCKSZ as u16);
        self.put_u16(OFF_SPECIAL, BLCKSZ as u16);
        self.put_u16(OFF_VERSION, PAGESIZE_VERSION);
        self.0[OFF_FSM_KIND] = kind as u8;
    }

    /// The `kind` of an FSM page; `None` for an all-zero page or an unknown value.
    pub fn fsm_kind(&self) -> Option<FsmPageKind> {
        match self.0[OFF_FSM_KIND] {
            1 => Some(FsmPageKind::Root),
            2 => Some(FsmPageKind::Leaf),
            _ => None,
        }
    }

    /// The 8160 bytes of an FSM page's `data`.
    pub fn fsm_data(&self) -> &[u8] {
        &self.0[FSM_DATA_START..]
    }

    pub fn fsm_data_mut(&mut self) -> &mut [u8] {
        &mut self.0[FSM_DATA_START..]
    }

    /// The hole of a standard page, `(pd_lower, pd_upper - pd_lower)`, that a
    /// WAL full-page image omits (`m3.md` §3.3). `None` when the header is
    /// not in the standard shape (`SIZE_OF_PAGE_HEADER <= lower < upper <=
    /// BLCKSZ`), which includes an empty hole.
    pub fn hole_range(&self) -> Option<(u16, u16)> {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        (SIZE_OF_PAGE_HEADER <= lower && lower < upper && upper <= BLCKSZ)
            .then(|| (lower as u16, (upper - lower) as u16))
    }

    /// Header invariants + the all-zero check (D12) + the checksum.
    pub fn verify(&self, blkno: BlockNumber) -> std::result::Result<(), PageError> {
        let upper = usize::from(self.upper());
        if upper == 0 {
            return if self.is_all_zero() {
                Ok(())
            } else {
                Err(PageError::NotAllZero)
            };
        }
        let lower = usize::from(self.lower());
        let special = usize::from(self.special());
        if self.flags() & !PD_VALID_FLAG_BITS != 0 {
            return Err(PageError::BadHeader("unknown pd_flags bits"));
        }
        if self.get_u16(OFF_VERSION) != PAGESIZE_VERSION {
            return Err(PageError::BadHeader("pd_pagesize_version"));
        }
        if lower < SIZE_OF_PAGE_HEADER {
            return Err(PageError::BadHeader("pd_lower below the header"));
        }
        if lower > upper {
            return Err(PageError::BadHeader("pd_lower > pd_upper"));
        }
        if upper > special {
            return Err(PageError::BadHeader("pd_upper > pd_special"));
        }
        if special > BLCKSZ {
            return Err(PageError::BadHeader("pd_special beyond the page"));
        }
        if !special.is_multiple_of(MAXALIGN) {
            return Err(PageError::BadHeader("pd_special not aligned"));
        }
        if !(lower - SIZE_OF_PAGE_HEADER).is_multiple_of(ITEM_ID_SIZE) {
            return Err(PageError::BadHeader(
                "pd_lower not on a line pointer boundary",
            ));
        }
        let stored = self.checksum();
        let computed = page_checksum(&self.0, blkno);
        if stored != computed {
            return Err(PageError::ChecksumMismatch { stored, computed });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heap_page() -> Page {
        let mut p = Page::zeroed();
        p.init_heap();
        p
    }

    fn sealed(mut p: Page, blk: BlockNumber) -> Page {
        let sum = page_checksum(&p.0, blk);
        p.set_checksum(sum);
        p
    }

    #[test]
    fn hole_range_of_standard_and_malformed_pages() {
        let mut p = Page::zeroed();
        assert_eq!(p.hole_range(), None, "an all-zero page has no hole");
        p.init_heap();
        assert_eq!(p.hole_range(), Some((24, 8192 - 24)));
        p.add_item(b"12345678").unwrap();
        assert_eq!(p.hole_range(), Some((p.lower(), p.upper() - p.lower())));
        // lower == upper: the hole is empty.
        let upper = p.upper();
        p.0[12..14].copy_from_slice(&upper.to_le_bytes());
        assert_eq!(p.hole_range(), None);
        // lower below the header.
        p.0[12..14].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(p.hole_range(), None);
        // upper beyond the page.
        let mut bad = Page::zeroed();
        bad.0[12..14].copy_from_slice(&100u16.to_le_bytes());
        bad.0[14..16].copy_from_slice(&9000u16.to_le_bytes());
        assert_eq!(bad.hole_range(), None);
    }

    #[test]
    fn init_heap_layout() {
        let p = heap_page();
        assert_eq!(p.lower(), 24);
        assert_eq!(p.upper(), 8192);
        assert_eq!(p.special(), 8192);
        assert_eq!(p.max_offset(), 0);
        assert_eq!(p.lsn(), 0);
        assert_eq!(p.flags(), 0);
        assert_eq!(p.prune_xid(), 0);
        assert_eq!(&p.0[18..20], &[0x01, 0x20]);
        assert!(!p.is_new());
        assert!(!p.is_all_zero());
        assert!(Page::zeroed().is_new());
        assert_eq!(p.free_space(), 8192 - 24 - 4);
    }

    #[test]
    fn add_and_read_items() {
        let mut p = heap_page();
        let a = [1u8, 2, 3];
        let b = vec![7u8; 41];
        assert_eq!(p.add_item(&a), Some(1));
        assert_eq!(p.add_item(&b), Some(2));
        assert_eq!(p.max_offset(), 2);
        assert_eq!(p.item(1).unwrap(), &a);
        assert_eq!(p.item(2).unwrap(), &b[..]);
        // MAXALIGN(3) = 8, MAXALIGN(41) = 48.
        assert_eq!(p.upper(), 8192 - 8 - 48);
        assert_eq!(p.lower(), 24 + 8);
        let id = p.item_id(2).unwrap();
        assert_eq!((id.flags, id.len), (LpFlags::Normal, 41));
        p.item_mut(1).unwrap()[0] = 9;
        assert_eq!(p.item(1).unwrap()[0], 9);
        // Padding bytes are zero.
        assert!(
            p.0[usize::from(p.item_id(1).unwrap().off) + 3..][..5]
                .iter()
                .all(|&x| x == 0)
        );
    }

    #[test]
    fn item_range_errors() {
        let mut p = heap_page();
        p.add_item(&[1, 2, 3]).unwrap();
        assert_eq!(p.item_id(0), Err(PageError::BadItem { off: 0 }));
        assert_eq!(p.item_id(2), Err(PageError::BadItem { off: 2 }));
        assert_eq!(p.item(2), Err(PageError::BadItem { off: 2 }));
        // Corrupt the line pointer to point past pd_special.
        let bad = ItemId {
            off: 8190,
            flags: LpFlags::Normal,
            len: 20,
        };
        p.0[24..28].copy_from_slice(&bad.encode().to_le_bytes());
        assert_eq!(p.item_id(1), Err(PageError::BadItem { off: 1 }));
        // And below pd_upper.
        let bad = ItemId {
            off: 30,
            flags: LpFlags::Normal,
            len: 4,
        };
        p.0[24..28].copy_from_slice(&bad.encode().to_le_bytes());
        assert!(p.item(1).is_err());
        // A Dead pointer decodes but has no tuple bytes.
        let dead = ItemId {
            off: 0,
            flags: LpFlags::Dead,
            len: 0,
        };
        p.0[24..28].copy_from_slice(&dead.encode().to_le_bytes());
        assert_eq!(p.item_id(1).unwrap().flags, LpFlags::Dead);
        assert!(p.item(1).is_err());
        // Garbage pd_lower must not read outside the page.
        let mut g = Page::zeroed();
        g.put_u16(OFF_LOWER, 0xFFFF);
        assert_eq!(g.item_id(16000), Err(PageError::BadItem { off: 16000 }));
    }

    #[test]
    fn line_pointer_bit_layout() {
        let id = ItemId {
            off: 8000,
            flags: LpFlags::Normal,
            len: 100,
        };
        let raw = id.encode();
        assert_eq!(raw, 0x1F40 | (1 << 15) | (100 << 17));
        assert_eq!(ItemId::decode(raw), id);
        let id = ItemId {
            off: 0x7FFF,
            flags: LpFlags::Dead,
            len: 0x7FFF,
        };
        assert_eq!(ItemId::decode(id.encode()), id);
    }

    #[test]
    fn fills_until_full_and_does_not_corrupt() {
        let mut p = heap_page();
        let mut n = 0u16;
        while let Some(off) = p.add_item(&[0xAA; 100]) {
            n += 1;
            assert_eq!(off, n);
        }
        // 104 + 4 bytes per tuple.
        assert_eq!(usize::from(n), (8192 - 24) / 108);
        assert!(p.free_space() < 108);
        for i in 1..=n {
            assert_eq!(p.item(i).unwrap(), &[0xAA; 100][..]);
        }
        assert!(sealed(p, 3).verify(3).is_ok());
    }

    #[test]
    fn max_tuples_per_page_is_enforced() {
        let mut p = heap_page();
        for i in 0..MAX_HEAP_TUPLES_PER_PAGE {
            assert_eq!(p.add_item(&[1]), Some((i + 1) as u16));
        }
        assert_eq!(p.free_space(), 0);
        assert_eq!(p.add_item(&[1]), None);
    }

    #[test]
    fn largest_tuple_fits_exactly() {
        let mut p = heap_page();
        let big = vec![5u8; super::super::MAX_HEAP_TUPLE_SIZE];
        assert_eq!(p.free_space(), 8164);
        assert_eq!(p.add_item(&big), Some(1));
        assert_eq!(p.free_space(), 0);
        let mut q = heap_page();
        assert_eq!(
            q.add_item(&vec![5u8; super::super::MAX_HEAP_TUPLE_SIZE + 1]),
            None
        );
        assert_eq!(q.max_offset(), 0);
        // A new (all-zero) page accepts nothing.
        assert_eq!(Page::zeroed().add_item(&[1]), None);
        assert_eq!(Page::zeroed().free_space(), 0);
    }

    #[test]
    fn verify_accepts_all_zero_and_rejects_partial_zero() {
        let z = Page::zeroed();
        assert!(z.verify(0).is_ok());
        let mut q = Page::zeroed();
        q.0[5000] = 1;
        assert_eq!(q.verify(0), Err(PageError::NotAllZero));
    }

    #[test]
    fn verify_header_invariants() {
        let ok = sealed(heap_page(), 9);
        assert!(ok.verify(9).is_ok());

        let mutate = |f: &dyn Fn(&mut Page)| {
            let mut p = heap_page();
            p.add_item(&[1, 2, 3]).unwrap();
            f(&mut p);
            sealed(p, 9).verify(9)
        };
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_FLAGS, 0x0008)),
            Err(PageError::BadHeader(_))
        ));
        assert!(mutate(&|p| p.put_u16(OFF_FLAGS, 0x0007)).is_ok());
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_VERSION, 0x2000)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_LOWER, 23)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_LOWER, 29)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_LOWER, 9000)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_SPECIAL, 8190)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_SPECIAL, 8200)),
            Err(PageError::BadHeader(_))
        ));
        assert!(matches!(
            mutate(&|p| p.put_u16(OFF_SPECIAL, 8000)),
            Err(PageError::BadHeader(_))
        ));
    }

    #[test]
    fn verify_detects_bit_flips_and_wrong_block() {
        let mut p = heap_page();
        p.add_item(&[1, 2, 3, 4]).unwrap();
        let p = sealed(p, 5);
        assert!(p.verify(5).is_ok());
        assert!(matches!(
            p.verify(6),
            Err(PageError::ChecksumMismatch { .. })
        ));
        let mut q = p.clone();
        q.0[8000] ^= 0x10;
        assert!(matches!(
            q.verify(5),
            Err(PageError::ChecksumMismatch { .. })
        ));
        // An unsealed page (checksum 0) is rejected too.
        assert!(matches!(
            heap_page().verify(0),
            Err(PageError::ChecksumMismatch { stored: 0, .. })
        ));
    }

    #[test]
    fn page_error_converts_to_xx001() {
        let e = PageError::NotAllZero.to_error(7, "base/5/16384");
        assert_eq!(e.sqlstate, crate::error::sqlstate::DATA_CORRUPTED);
        assert_eq!(
            e.message,
            "invalid page in block 7 of relation base/5/16384"
        );
        assert!(e.detail.is_some());
    }

    #[test]
    fn lsn_roundtrip() {
        let mut p = heap_page();
        p.set_lsn(0x0102_0304_0506_0708);
        assert_eq!(p.lsn(), 0x0102_0304_0506_0708);
        assert_eq!(p.0[0], 0x08);
    }

    fn bt_page() -> Page {
        let mut p = Page::zeroed();
        p.init_special(16);
        p
    }

    #[test]
    fn init_special_layout() {
        let p = bt_page();
        assert_eq!((p.lower(), p.upper(), p.special()), (24, 8176, 8176));
        assert_eq!(p.0[18..20], 0x2001u16.to_le_bytes());
        assert_eq!(p.special_area().len(), 16);
        assert_eq!(p.free_space_unbounded(), 8152 - 4);
        assert!(sealed(p, 1).verify(1).is_ok());
        let mut q = Page::zeroed();
        q.init_special(8);
        q.special_area_mut().copy_from_slice(&[7; 8]);
        assert_eq!(q.special(), 8184);
        assert_eq!(&q.0[8184..], &[7; 8]);
    }

    #[test]
    fn insert_item_at_front_middle_end_and_full() {
        let mut p = bt_page();
        assert_eq!(p.insert_item_at(1, b"bbbb"), Some(1));
        assert_eq!(p.insert_item_at(1, b"aaaaa"), Some(1));
        assert_eq!(p.insert_item_at(3, b"dd"), Some(3));
        assert_eq!(p.insert_item_at(3, b"c"), Some(3));
        assert_eq!(p.insert_item_at(0, b"x"), None);
        assert_eq!(p.insert_item_at(6, b"x"), None);
        let got: Vec<&[u8]> = (1..=4).map(|i| p.item(i).unwrap()).collect();
        assert_eq!(got, [&b"aaaaa"[..], b"bbbb", b"c", b"dd"]);
        // Bodies are packed in insertion order: "bbbb" is highest.
        let off = |i| p.item_id(i).unwrap().off;
        assert!(off(2) > off(1) && off(1) > off(4) && off(4) > off(3));
        assert!(sealed(p, 1).verify(1).is_ok());
    }

    #[test]
    fn insert_item_at_fills_exactly_and_rejects_without_change() {
        let mut p = bt_page();
        // 8152 usable = 2 * (4000 + 4) + 144
        assert!(p.insert_item_at(1, &[1; 4000]).is_some());
        assert!(p.insert_item_at(2, &[2; 4000]).is_some());
        let before = p.clone();
        assert_eq!(p.insert_item_at(1, &[3; 141]), None);
        assert_eq!(p.0[..], before.0[..]);
        assert!(p.insert_item_at(3, &[3; 136]).is_some());
        assert_eq!(p.free_space_unbounded(), 0);
        assert_eq!(p.insert_item_at(1, &[1]), None);
    }

    #[test]
    fn insert_item_at_ignores_heap_tuple_limit() {
        let mut p = bt_page();
        for _ in 0..MAX_HEAP_TUPLES_PER_PAGE + 10 {
            p.insert_item_at(1, &[1]).unwrap();
        }
        assert!(usize::from(p.max_offset()) > MAX_HEAP_TUPLES_PER_PAGE);
        assert_eq!(p.free_space(), 0);
    }

    #[test]
    fn build_with_items_matches_sequential_insert() {
        let items: Vec<Vec<u8>> = (0..20).map(|i| vec![i as u8; 5 + i * 7]).collect();
        let refs: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
        let special: Vec<u8> = (0..16).collect();
        let built = Page::build_with_items(&special, &refs).unwrap();
        let mut p = bt_page();
        p.special_area_mut().copy_from_slice(&special);
        for (i, it) in refs.iter().enumerate() {
            p.insert_item_at((i + 1) as u16, it).unwrap();
        }
        assert_eq!(built.0[..], p.0[..]);
        assert_eq!(built.lsn(), 0);
        assert_eq!(built.checksum(), 0);
        assert!(sealed(*built, 2).verify(2).is_ok());
        let big = vec![0u8; 8200];
        assert!(Page::build_with_items(&special, &[&big]).is_none());
        assert!(Page::build_with_items(&[0; 7], &[]).is_none());
    }

    #[test]
    fn body_and_set_lower() {
        let mut p = Page::zeroed();
        p.init_special(16);
        assert!(p.body().is_empty());
        p.set_lower(56);
        assert_eq!(p.body().len(), 32);
        p.body_mut()[..4].copy_from_slice(&0x0005_3162_u32.to_le_bytes());
        assert_eq!(p.0[24..28], 0x0005_3162_u32.to_le_bytes());
        assert_eq!(p.max_offset(), 8);
        assert!(sealed(p, 0).verify(0).is_ok());
        let mut bad = Page::zeroed();
        bad.set_lower(0xFFFF);
        assert!(bad.body().is_empty());
    }

    #[test]
    fn special_area_out_of_range_is_empty() {
        let mut p = Page::zeroed();
        assert!(p.special_area().is_empty());
        assert!(p.special_area_mut().is_empty());
    }

    // ----- M5 VC-1: reuse of LP_UNUSED, defragmentation, prune hint, FSM page -----

    fn tup(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    fn lp_bytes(p: &Page, off: u16) -> [u8; 4] {
        let pos = 24 + (usize::from(off) - 1) * 4;
        p.0[pos..pos + 4].try_into().unwrap()
    }

    #[test]
    fn spec_example_three_states_are_fixed_values() {
        // m5/03 §3.1: three 51-byte tuples (56 bytes each with MAXALIGN).
        let mut p = heap_page();
        for i in 1..=3u8 {
            assert_eq!(p.add_item(&tup(51, i)), Some(u16::from(i)));
        }
        assert_eq!((p.lower(), p.upper()), (36, 8024));
        assert_eq!(lp_bytes(&p, 1), [0xC8, 0x9F, 0x66, 0x00]);
        assert_eq!(lp_bytes(&p, 2), [0x90, 0x9F, 0x66, 0x00]);
        assert_eq!(lp_bytes(&p, 3), [0x58, 0x9F, 0x66, 0x00]);

        // State 2: pruning kills tuple 2; tuple 3 slides up to 8080.
        p.mark_dead(2);
        assert_eq!(lp_bytes(&p, 2), [0x00, 0x80, 0x01, 0x00]);
        p.repair_fragmentation().unwrap();
        assert_eq!(lp_bytes(&p, 1), [0xC8, 0x9F, 0x66, 0x00]);
        assert_eq!(lp_bytes(&p, 2), [0x00, 0x80, 0x01, 0x00]);
        assert_eq!(lp_bytes(&p, 3), [0x90, 0x9F, 0x66, 0x00]);
        assert_eq!((p.lower(), p.upper()), (36, 8080));
        assert_eq!(&p.0[12..16], &[0x24, 0x00, 0x90, 0x1F]);
        assert_eq!(p.flags(), 0);
        assert_eq!(p.item(3).unwrap(), &tup(51, 3)[..]);

        // State 3: the third stage turns line pointer 2 into LP_UNUSED; it is not
        // the last one, so it stays in the array.
        p.mark_unused(2);
        p.finish_unused();
        assert_eq!(lp_bytes(&p, 2), [0, 0, 0, 0]);
        assert_eq!(p.lower(), 36);
        assert_eq!(p.flags(), 0x0001);
    }

    #[test]
    fn add_item_reuses_the_smallest_unused_line_pointer() {
        let mut p = heap_page();
        for i in 0..5u8 {
            p.add_item(&tup(20, i)).unwrap();
        }
        assert_eq!(p.flags() & PD_HAS_FREE_LINES, 0);
        p.mark_dead(2);
        p.mark_unused(4);
        p.mark_unused(3);
        p.finish_unused();
        assert_eq!(p.flags() & PD_HAS_FREE_LINES, PD_HAS_FREE_LINES);
        assert_eq!(p.add_item(&tup(20, 9)), Some(3));
        assert_eq!(p.add_item(&tup(20, 8)), Some(4));
        // The flag follows "an LP_UNUSED exists" (only LP_DEAD at 2 is left: no).
        assert_eq!(p.flags() & PD_HAS_FREE_LINES, 0);
        assert_eq!(p.add_item(&tup(20, 7)), Some(6));
        assert_eq!(p.max_offset(), 6);
    }

    #[test]
    fn max_tuples_with_unused_slots_can_still_be_filled() {
        let mut p = heap_page();
        for _ in 0..MAX_HEAP_TUPLES_PER_PAGE {
            p.add_item(&[1]).unwrap();
        }
        assert_eq!(p.add_item(&[1]), None);
        assert_eq!(p.free_space(), 0);
        p.mark_unused(50);
        p.finish_unused();
        assert!(p.free_space() > 0);
        assert_eq!(p.add_item(&[2]), Some(50));
        assert_eq!(p.free_space(), 0);
        assert_eq!(p.add_item(&[3]), None);
    }

    #[test]
    fn add_item_at_places_only_on_unused_or_next() {
        let mut p = heap_page();
        for i in 0..4u8 {
            p.add_item(&tup(10, i)).unwrap();
        }
        p.mark_unused(2);
        p.mark_dead(3);
        p.refresh_free_lines_flag();
        let before = p.clone();
        assert!(!p.add_item_at(0, b"x"));
        assert!(!p.add_item_at(1, b"x"), "LP_NORMAL");
        assert!(!p.add_item_at(3, b"x"), "LP_DEAD");
        assert!(!p.add_item_at(6, b"x"), "beyond max_offset + 1");
        assert!(!Page::zeroed().add_item_at(1, b"x"), "new page");
        assert_eq!(
            p.0[..],
            before.0[..],
            "a refused add_item_at leaves the page alone"
        );
        assert!(p.add_item_at(2, b"two"));
        assert_eq!(p.item(2).unwrap(), b"two");
        assert!(p.add_item_at(5, b"five"));
        assert_eq!(p.max_offset(), 5);
        assert_eq!(p.flags() & PD_HAS_FREE_LINES, 0);
        // Does not fit: nothing changes.
        let before = p.clone();
        assert!(!p.add_item_at(6, &vec![0; 9000]));
        assert_eq!(p.0[..], before.0[..]);
    }

    #[test]
    fn free_space_has_three_cases() {
        let mut p = heap_page();
        // No LP_UNUSED: one line pointer is deducted.
        assert_eq!(p.free_space(), 8164);
        p.add_item(&tup(100, 1)).unwrap();
        p.add_item(&tup(100, 1)).unwrap();
        let hole = usize::from(p.upper() - p.lower());
        assert_eq!(p.free_space(), hole - 4);
        // With an LP_UNUSED in the middle: the whole hole.
        p.mark_unused(1);
        p.refresh_free_lines_flag();
        assert_eq!(p.free_space(), hole);
        // 185 line pointers and no LP_UNUSED: 0.
        let mut q = heap_page();
        for _ in 0..MAX_HEAP_TUPLES_PER_PAGE {
            q.add_item(&[1]).unwrap();
        }
        assert_eq!(q.free_space(), 0);
        // A stale PD_HAS_FREE_LINES (no LP_UNUSED actually) does not inflate the answer.
        let hole = usize::from(p.upper() - p.lower());
        p.add_item(&tup(8, 2)).unwrap();
        p.0[10] |= 1;
        let hole2 = usize::from(p.upper() - p.lower());
        assert!(hole2 < hole);
        assert_eq!(p.free_space(), hole2 - 4);
    }

    #[test]
    fn item_id_requires_the_canonical_form() {
        let mut p = heap_page();
        p.add_item(&[1, 2, 3]).unwrap();
        p.add_item(&[1, 2, 3]).unwrap();
        for (flags, ok) in [(LpFlags::Dead, true), (LpFlags::Unused, true)] {
            let id = ItemId {
                off: 0,
                flags,
                len: 0,
            };
            p.put_item_id(1, id);
            assert_eq!(p.item_id(1).map(|i| i.flags), Ok(flags), "{ok}");
            for bad in [
                ItemId {
                    off: 40,
                    flags,
                    len: 0,
                },
                ItemId {
                    off: 0,
                    flags,
                    len: 3,
                },
            ] {
                p.put_item_id(1, bad);
                assert_eq!(p.item_id(1), Err(PageError::BadItem { off: 1 }));
            }
        }
        // mark_dead / mark_unused write the canonical form.
        p.mark_dead(2);
        assert_eq!(p.item_id(2).unwrap().flags, LpFlags::Dead);
        p.mark_unused(2);
        assert_eq!(lp_bytes(&p, 2), [0; 4]);
    }

    #[test]
    fn set_prunable_keeps_the_oldest_in_wraparound_order() {
        let mut p = heap_page();
        assert_eq!(p.prune_xid(), 0);
        p.set_prunable(Xid(500));
        assert_eq!(p.prune_xid(), 500);
        p.set_prunable(Xid(600));
        assert_eq!(p.prune_xid(), 500, "a newer XID does not replace it");
        p.set_prunable(Xid(400));
        assert_eq!(p.prune_xid(), 400);
        // Signed 32-bit comparison: just before the wrap counts as older.
        p.set_prune_xid(5);
        p.set_prunable(Xid(0xFFFF_FFF0));
        assert_eq!(p.prune_xid(), 0xFFFF_FFF0);
        // The 64-bit XID uses its low 32 bits.
        let mut q = heap_page();
        q.set_prunable(Xid(0x1_0000_0007));
        assert_eq!(q.prune_xid(), 7);
    }

    #[test]
    fn prune_xid_full_restores_the_high_bits() {
        let mut p = heap_page();
        assert_eq!(p.prune_xid_full(Xid(100)), None);
        p.set_prune_xid(50);
        assert_eq!(p.prune_xid_full(Xid(100)), Some(Xid(50)));
        // next_xid just past a 2^32 boundary, hint from before it.
        p.set_prune_xid(0xFFFF_FFF0);
        assert_eq!(p.prune_xid_full(Xid(0x1_0000_0010)), Some(Xid(0xFFFF_FFF0)));
        // Same epoch.
        p.set_prune_xid(0x10);
        assert_eq!(
            p.prune_xid_full(Xid(0x1_0000_0100)),
            Some(Xid(0x1_0000_0010))
        );
        // A hint above next_xid in epoch 0 cannot be restored.
        p.set_prune_xid(500);
        assert_eq!(p.prune_xid_full(Xid(100)), None);
    }

    #[test]
    fn fsm_page_header_matches_the_documented_layout() {
        let mut p = Page::zeroed();
        assert_eq!(p.fsm_kind(), None);
        p.init_fsm(FsmPageKind::Leaf);
        assert_eq!(p.fsm_kind(), Some(FsmPageKind::Leaf));
        p.fsm_data_mut()[..4].copy_from_slice(&[0xFF, 0x00, 0x03, 0x00]);
        assert_eq!(p.fsm_data().len(), 8160);
        assert_eq!(
            &p.0[12..20],
            &[0x20, 0x00, 0x00, 0x20, 0x00, 0x20, 0x01, 0x20]
        );
        assert_eq!(&p.0[24..36], &[2, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0, 3, 0]);
        assert!(sealed(p.clone(), 4).verify(4).is_ok());
        p.init_fsm(FsmPageKind::Root);
        assert_eq!(p.fsm_kind(), Some(FsmPageKind::Root));
        assert!(p.fsm_data().iter().all(|&b| b == 0));
    }

    #[test]
    fn repair_fragmentation_rejects_a_corrupt_page() {
        let mut p = heap_page();
        p.add_item(&tup(20, 1)).unwrap();
        // A redirect line pointer is never made in M5.
        p.put_item_id(
            1,
            ItemId {
                off: 5,
                flags: LpFlags::Redirect,
                len: 0,
            },
        );
        assert_eq!(p.repair_fragmentation(), Err(PageError::BadItem { off: 1 }));
        assert_eq!(
            Page::zeroed().repair_fragmentation(),
            Err(PageError::BadHeader("cannot defragment this page"))
        );
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        #[derive(Clone, Debug)]
        enum Op {
            Add(usize),
            Dead(usize),
            Unused(usize),
        }

        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                (1usize..300).prop_map(Op::Add),
                (0usize..64).prop_map(Op::Dead),
                (0usize..64).prop_map(Op::Unused),
            ]
        }

        /// Runs `ops` on a fresh page through the public operations.
        fn build(ops: &[Op]) -> Page {
            let mut p = heap_page();
            for (i, o) in ops.iter().enumerate() {
                let n = usize::from(p.max_offset());
                match *o {
                    Op::Add(len) => {
                        let _ = p.add_item(&tup(len, (i % 250) as u8 + 1));
                    }
                    Op::Dead(k) if n > 0 => p.mark_dead((k % n) as u16 + 1),
                    Op::Unused(k) if n > 0 => p.mark_unused((k % n) as u16 + 1),
                    _ => {}
                }
            }
            p.refresh_free_lines_flag();
            p
        }

        proptest! {
            /// `add_item_at(add_item's offset)` on an identical page gives identical bytes.
            #[test]
            fn add_item_at_agrees_with_add_item(
                ops in proptest::collection::vec(op(), 0..60),
                len in 1usize..400,
            ) {
                let base = build(&ops);
                let data = tup(len, 0xEE);
                let mut a = base.clone();
                let mut b = base;
                if let Some(off) = a.add_item(&data) {
                    prop_assert!(b.add_item_at(off, &data));
                    prop_assert_eq!(&a.0[..], &b.0[..]);
                } else {
                    // add_item found no room: appending (which needs 4 more bytes) cannot fit either.
                    let n = b.max_offset();
                    prop_assert!(!b.add_item_at(n + 1, &data));
                }
            }

            /// `repair_fragmentation` keeps tuple bytes and line pointer numbers, packs
            /// the page, zeroes holes and padding, and is idempotent.
            #[test]
            fn repair_fragmentation_properties(ops in proptest::collection::vec(op(), 0..80)) {
                let before = build(&ops);
                let mut after = before.clone();
                after.repair_fragmentation().unwrap();

                let n0 = before.max_offset();
                let mut used = 0usize;
                let mut last_kept = 0u16;
                for off in 1..=n0 {
                    let b = before.item_id(off).unwrap();
                    if b.flags != LpFlags::Unused {
                        last_kept = off;
                    }
                    if off > after.max_offset() {
                        prop_assert_eq!(b.flags, LpFlags::Unused);
                        continue;
                    }
                    let a = after.item_id(off).unwrap();
                    prop_assert_eq!(a.flags, b.flags);
                    prop_assert_eq!(a.len, b.len);
                    if b.flags == LpFlags::Normal {
                        prop_assert_eq!(after.item(off).unwrap(), before.item(off).unwrap());
                        used += align_up(usize::from(b.len));
                    }
                }
                // Trailing LP_UNUSED are gone; LP_DEAD are not.
                prop_assert_eq!(after.max_offset(), last_kept);
                // Packed: pd_upper is as high as it can be.
                prop_assert_eq!(usize::from(after.upper()), BLCKSZ - used);
                // Hole and padding are zero.
                let (lo, up) = (usize::from(after.lower()), usize::from(after.upper()));
                prop_assert!(after.0[lo..up].iter().all(|&x| x == 0));
                for off in 1..=after.max_offset() {
                    let id = after.item_id(off).unwrap();
                    if id.flags == LpFlags::Normal {
                        let s = usize::from(id.off) + usize::from(id.len);
                        let e = usize::from(id.off) + align_up(usize::from(id.len));
                        prop_assert!(after.0[s..e].iter().all(|&x| x == 0));
                    }
                }
                prop_assert_eq!(
                    after.flags() & PD_HAS_FREE_LINES != 0,
                    after.has_unused_item()
                );
                prop_assert!(sealed(after.clone(), 1).verify(1).is_ok());
                // Idempotent.
                let mut twice = after.clone();
                twice.repair_fragmentation().unwrap();
                prop_assert_eq!(&twice.0[..], &after.0[..]);
            }
        }
    }
}
