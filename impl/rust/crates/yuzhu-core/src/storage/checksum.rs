//! Port of PostgreSQL's `pg_checksum_page` (`m2.md` §6.4).
//!
//! 担当 C が実装する。

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]
#![allow(clippy::unimplemented)]

use super::BLCKSZ;
use super::smgr::BlockNumber;

/// Checksum of `page` as block `blkno` of its relation (`pd_checksum` is
/// treated as 0).
pub fn page_checksum(_page: &[u8; BLCKSZ], _blkno: BlockNumber) -> u16 {
    unimplemented!("担当 C が実装")
}
