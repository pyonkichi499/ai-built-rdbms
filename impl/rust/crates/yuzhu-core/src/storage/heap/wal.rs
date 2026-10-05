//! HEAP rmgr: INSERT / DELETE / UPDATE の WAL と REDO（`m3.md` §3.9、§4.5、§5.1）。
//!
//! 担当 D が実装する。メインデータの `encode` / `decode` は A が置いた実装、
//! `redo` はスタブ。

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_pass_by_value
)]

use crate::error::{Error, Result, Severity, sqlstate};
use crate::txn::{CommandId, Xid};
use crate::wal::{DecodedRecord, RedoCtx};

pub const HEAP_INSERT: u8 = 0x00;
pub const HEAP_DELETE: u8 = 0x10;
pub const HEAP_UPDATE: u8 = 0x20;
/// `info` に OR するフラグ。
pub const HEAP_INIT_PAGE: u8 = 0x80;

/// メインデータ（DELETE / UPDATE）の長さ。
pub const HEAP_MAIN_LEN: usize = 24;

fn corrupt(what: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid heap WAL record: {what}"),
    )
    .with_severity(Severity::Panic)
}

/// DELETE のメインデータ（変更後のヘッダの値をそのまま載せる）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapDeleteMain {
    pub offnum: u16,
    pub infomask: u16,
    pub infomask2: u16,
    pub xmax: Xid,
    pub cmax: CommandId,
}

/// UPDATE のメインデータ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapUpdateMain {
    pub old_offnum: u16,
    pub new_offnum: u16,
    pub old_infomask: u16,
    pub old_infomask2: u16,
    pub old_xmax: Xid,
    pub old_cmax: CommandId,
    /// blk1 がない（新旧が同じページ）。
    pub same_page: bool,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]))
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or([0; 4]))
}

impl HeapDeleteMain {
    pub fn encode(&self) -> [u8; HEAP_MAIN_LEN] {
        let mut b = [0u8; HEAP_MAIN_LEN];
        b[0..2].copy_from_slice(&self.offnum.to_le_bytes());
        b[2..4].copy_from_slice(&self.infomask.to_le_bytes());
        b[4..6].copy_from_slice(&self.infomask2.to_le_bytes());
        b[8..16].copy_from_slice(&self.xmax.0.to_le_bytes());
        b[16..20].copy_from_slice(&self.cmax.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != HEAP_MAIN_LEN {
            return Err(corrupt("DELETE main data has a bad length"));
        }
        Ok(HeapDeleteMain {
            offnum: u16_at(b, 0),
            infomask: u16_at(b, 2),
            infomask2: u16_at(b, 4),
            xmax: Xid(u64_at(b, 8)),
            cmax: u32_at(b, 16),
        })
    }
}

impl HeapUpdateMain {
    pub fn encode(&self) -> [u8; HEAP_MAIN_LEN] {
        let mut b = [0u8; HEAP_MAIN_LEN];
        b[0..2].copy_from_slice(&self.old_offnum.to_le_bytes());
        b[2..4].copy_from_slice(&self.new_offnum.to_le_bytes());
        b[4..6].copy_from_slice(&self.old_infomask.to_le_bytes());
        b[6..8].copy_from_slice(&self.old_infomask2.to_le_bytes());
        b[8..16].copy_from_slice(&self.old_xmax.0.to_le_bytes());
        b[16..20].copy_from_slice(&self.old_cmax.to_le_bytes());
        b[20] = u8::from(self.same_page);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != HEAP_MAIN_LEN {
            return Err(corrupt("UPDATE main data has a bad length"));
        }
        Ok(HeapUpdateMain {
            old_offnum: u16_at(b, 0),
            new_offnum: u16_at(b, 2),
            old_infomask: u16_at(b, 4),
            old_infomask2: u16_at(b, 6),
            old_xmax: Xid(u64_at(b, 8)),
            old_cmax: u32_at(b, 16),
            same_page: b[20] & 1 != 0,
        })
    }
}

/// HEAP rmgr の REDO。担当 D が実装する。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let _ = (ctx, rec);
    Err(Error::internal("heap::wal::redo: 担当 D が実装"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_main_round_trips_and_layout_matches() {
        let m = HeapDeleteMain {
            offnum: 5,
            infomask: 0x0800,
            infomask2: 0x0001,
            xmax: Xid(0x1_0000_0001),
            cmax: 7,
        };
        let b = m.encode();
        assert_eq!(&b[0..2], &[5, 0]);
        assert_eq!(&b[8..16], &0x1_0000_0001u64.to_le_bytes());
        assert_eq!(&b[20..24], &[0; 4]);
        assert_eq!(HeapDeleteMain::decode(&b).unwrap(), m);
    }

    #[test]
    fn update_main_round_trips_and_layout_matches() {
        let m = HeapUpdateMain {
            old_offnum: 2,
            new_offnum: 9,
            old_infomask: 1,
            old_infomask2: 2,
            old_xmax: Xid(44),
            old_cmax: 3,
            same_page: true,
        };
        let b = m.encode();
        assert_eq!(b[20], 1);
        assert_eq!(&b[21..24], &[0; 3]);
        assert_eq!(HeapUpdateMain::decode(&b).unwrap(), m);
        let mut m2 = m;
        m2.same_page = false;
        assert!(!HeapUpdateMain::decode(&m2.encode()).unwrap().same_page);
    }

    #[test]
    fn bad_length_is_a_panic_level_error() {
        let e = HeapDeleteMain::decode(&[0; 23]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert!(HeapUpdateMain::decode(&[0; 25]).is_err());
    }

    #[test]
    fn init_page_flag_leaves_the_kind_nibble_alone() {
        for k in [HEAP_INSERT, HEAP_DELETE, HEAP_UPDATE] {
            assert_eq!((k | HEAP_INIT_PAGE) & 0x70, k & 0x70);
        }
    }
}
