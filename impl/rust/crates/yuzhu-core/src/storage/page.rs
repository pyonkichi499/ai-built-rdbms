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
        if id.flags == LpFlags::Normal {
            let start = usize::from(id.off);
            let end = start + usize::from(id.len);
            if start < usize::from(self.upper()) || end > usize::from(self.special()) {
                return Err(bad);
            }
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

    /// Appends a tuple and returns its 1-based offset number. `None` if it
    /// does not fit or the page already has `MAX_HEAP_TUPLES_PER_PAGE` line
    /// pointers. Unused line pointers are never reused (§3.4).
    pub fn add_item(&mut self, data: &[u8]) -> Option<u16> {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper < lower || lower < SIZE_OF_PAGE_HEADER {
            return None;
        }
        let n = usize::from(self.max_offset());
        if n >= MAX_HEAP_TUPLES_PER_PAGE {
            return None;
        }
        let size = align_up(data.len());
        if size + ITEM_ID_SIZE > upper - lower || data.len() > 0x7FFF {
            return None;
        }
        let new_upper = upper - size;
        self.0[new_upper..upper].fill(0);
        self.0[new_upper..new_upper + data.len()].copy_from_slice(data);
        let id = ItemId {
            off: new_upper as u16,
            flags: LpFlags::Normal,
            len: data.len() as u16,
        };
        let pos = lower;
        self.0[pos..pos + ITEM_ID_SIZE].copy_from_slice(&id.encode().to_le_bytes());
        self.put_u16(OFF_LOWER, (lower + ITEM_ID_SIZE) as u16);
        self.put_u16(OFF_UPPER, new_upper as u16);
        Some((n + 1) as u16)
    }

    /// Free bytes minus one new line pointer (`PageGetHeapFreeSpace`); 0 when
    /// no more line pointers may be added.
    pub fn free_space(&self) -> usize {
        let lower = usize::from(self.lower());
        let upper = usize::from(self.upper());
        if upper <= lower || usize::from(self.max_offset()) >= MAX_HEAP_TUPLES_PER_PAGE {
            return 0;
        }
        (upper - lower).saturating_sub(ITEM_ID_SIZE)
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
}
