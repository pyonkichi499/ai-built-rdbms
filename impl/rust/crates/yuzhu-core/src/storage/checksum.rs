//! Port of PostgreSQL's `pg_checksum_page` (`m2.md` §6.4).
//!
//! `PG:src/include/storage/checksum_impl.h`. The page is read as a
//! `[[u32; 32]; 64]` (little-endian words), 32 FNV-1a-like sums run in
//! parallel, and the result is mixed with the block number and folded to
//! 16 bits (never 0).

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use super::BLCKSZ;
use super::smgr::BlockNumber;

const N_SUMS: usize = 32;
const FNV_PRIME: u32 = 16_777_619;
/// Offset of `pd_checksum` in the page header.
const CHECKSUM_OFFSET: usize = 8;

/// `checksumBaseOffsets` (copied from the PostgreSQL source).
const BASE_OFFSETS: [u32; N_SUMS] = [
    0x5B1F_36E9,
    0xB852_5960,
    0x02AB_50AA,
    0x1DE6_6D2A,
    0x79FF_467A,
    0x9BB9_F8A3,
    0x217E_7CD2,
    0x83E1_3D2C,
    0xF8D4_474F,
    0xE39E_B970,
    0x42C6_AE16,
    0x9932_16FA,
    0x7B09_3B5D,
    0x98DA_FF3C,
    0xF718_902A,
    0x0B1C_9CDB,
    0xE58F_764B,
    0x1876_36BC,
    0x5D7B_3BB1,
    0xE73D_E7DE,
    0x92BE_C979,
    0xCCA6_C0B2,
    0x304A_0979,
    0x85AA_43D4,
    0x7831_25BB,
    0x6CA8_EAA2,
    0xE407_EAC6,
    0x4B5C_FC3E,
    0x9FBF_8C76,
    0x15CA_20BE,
    0xF2CA_9FD3,
    0x959B_D756,
];

#[inline]
fn comp(sum: u32, value: u32) -> u32 {
    let tmp = sum ^ value;
    tmp.wrapping_mul(FNV_PRIME) ^ (tmp >> 17)
}

#[allow(clippy::chunks_exact_to_as_chunks)]
/// `pg_checksum_block` with the checksum field read as 0.
fn checksum_block(page: &[u8; BLCKSZ]) -> u32 {
    let mut sums = BASE_OFFSETS;
    for (i, row) in page.chunks_exact(N_SUMS * 4).enumerate() {
        for (j, (sum, word)) in sums.iter_mut().zip(row.chunks_exact(4)).enumerate() {
            let mut w = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            // pd_checksum is the low half of word 2 of the first row.
            if i == 0 && j == CHECKSUM_OFFSET / 4 {
                w &= 0xFFFF_0000;
            }
            *sum = comp(*sum, w);
        }
    }
    for _ in 0..2 {
        for sum in &mut sums {
            *sum = comp(*sum, 0);
        }
    }
    sums.iter().fold(0, |acc, s| acc ^ s)
}

/// Checksum of `page` as block `blkno` of its relation (`pd_checksum` is
/// treated as 0). `blkno` is the block number within the relation, not
/// within the segment.
pub fn page_checksum(page: &[u8; BLCKSZ], blkno: BlockNumber) -> u16 {
    let checksum = checksum_block(page) ^ blkno;
    ((checksum % 65535) + 1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG_PAGES: &[u8] = include_bytes!("../../testdata/pg_pages.bin");
    /// (block number within the relation, page index in the fixture).
    /// Pages come from `initdb --data-checksums` of PostgreSQL 17:
    /// `pg_proc` (1255) blocks 0, 7, 98 and `pg_class` (1259) block 2.
    const FIXTURE: [(u32, usize); 4] = [(0, 0), (7, 1), (98, 2), (2, 3)];

    fn fixture_page(idx: usize) -> [u8; BLCKSZ] {
        PG_PAGES[idx * BLCKSZ..(idx + 1) * BLCKSZ]
            .try_into()
            .unwrap()
    }

    #[test]
    fn matches_postgresql_on_real_pages() {
        for (blkno, idx) in FIXTURE {
            let page = fixture_page(idx);
            let stored = u16::from_le_bytes([page[8], page[9]]);
            assert_ne!(stored, 0);
            assert_eq!(page_checksum(&page, blkno), stored, "block {blkno}");
        }
    }

    #[test]
    fn ignores_the_stored_checksum_field() {
        let (blkno, idx) = FIXTURE[1];
        let mut page = fixture_page(idx);
        let expected = page_checksum(&page, blkno);
        page[8] = 0xAB;
        page[9] = 0xCD;
        assert_eq!(page_checksum(&page, blkno), expected);
    }

    #[test]
    fn depends_on_block_number_and_content() {
        let (blkno, idx) = FIXTURE[0];
        let mut page = fixture_page(idx);
        let base = page_checksum(&page, blkno);
        assert_ne!(page_checksum(&page, blkno + 1), base);
        page[4000] ^= 1;
        assert_ne!(page_checksum(&page, blkno), base);
    }

    #[test]
    fn never_returns_zero() {
        let page = [0u8; BLCKSZ];
        for blk in 0..2000 {
            assert_ne!(page_checksum(&page, blk), 0);
        }
    }
}
