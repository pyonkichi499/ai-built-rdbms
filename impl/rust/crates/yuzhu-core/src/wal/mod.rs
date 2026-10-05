//! WAL（REDO のみ）の型と定数（`m3.md` §3、§4.2）。
//!
//! この `mod.rs` はロジックを持たない。形式のエンコードとデコードは
//! [`record`]、セグメントは [`segment`]、書き込みは [`writer`]、読み取りは
//! [`reader`]、REDO の骨格は [`redo`]、XLOG rmgr は [`xlog`]、表示は [`dump`]。

pub mod dump;
pub mod reader;
pub mod record;
pub mod redo;
pub mod segment;
pub mod writer;
pub mod xlog;

use crate::debug_knobs::DebugKnobs;
use crate::storage::page::Page;
use crate::storage::smgr::BufferTag;
use crate::txn::Xid;

pub use self::reader::{EndReason, WalReader};
pub use self::record::{RecordBuilder, RecordError, decode_record};
pub use self::redo::{
    InvalidPages, RedoBuffer, RedoCtx, RedoStats, read_buffer_for_redo, run_redo,
};
pub use self::writer::Wal;

// ----- 定数（§3.3） ------------------------------------------------------------

pub const SEG_HEADER_SIZE: u64 = 32;
pub const RECORD_HEADER_SIZE: usize = 32;
pub const BLOCK_REF_HEADER_SIZE: usize = 24;
/// D4。PostgreSQL の `XLR_MAX_BLOCK_ID + 1` と同じ。
pub const MAX_BLOCK_REFS: usize = 32;
/// 1 MiB。
pub const MAX_RECORD_LEN: usize = 1 << 20;
/// 2 MiB = 2 × `MAX_RECORD_LEN`。
pub const MIN_WAL_SEGMENT_SIZE: u32 = 2 << 20;
pub const MAX_WAL_SEGMENT_SIZE: u32 = 1 << 30;
pub const DEFAULT_WAL_SEGMENT_SIZE: u32 = 16 << 20;
/// 溜まったら挿入側で flush する。
pub const WAL_BUFFER_FLUSH_THRESHOLD: usize = 4 << 20;
pub const WAL_FORMAT_VERSION: u16 = 1;

// ----- LSN ------------------------------------------------------------------

/// WAL ストリーム上のバイト位置。0 は無効（`m3.md` §3.2）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Lsn(pub u64);

impl Lsn {
    pub const INVALID: Lsn = Lsn(0);

    /// セグメント番号（`lsn / seg_size`）。
    pub fn segno(self, seg_size: u32) -> u64 {
        self.0 / u64::from(seg_size)
    }

    /// セグメント内の位置（`lsn % seg_size`）。
    pub fn seg_offset(self, seg_size: u32) -> u64 {
        self.0 % u64::from(seg_size)
    }
}

/// `%X/%X`（上位 32 ビット / 下位 32 ビット）。
impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xFFFF_FFFF)
    }
}

// ----- rmgr とブロック参照のフラグ -------------------------------------------------

/// リソースマネージャ ID（§3.5）。4 = Btree、5 = Seq は M4 で足す。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RmgrId {
    Xlog = 0,
    Xact = 1,
    Smgr = 2,
    Heap = 3,
}

impl RmgrId {
    /// 未知の ID（予約を含む）は `None`。読み手はそのレコードを不正とみなす。
    pub fn from_u8(v: u8) -> Option<RmgrId> {
        match v {
            0 => Some(RmgrId::Xlog),
            1 => Some(RmgrId::Xact),
            2 => Some(RmgrId::Smgr),
            3 => Some(RmgrId::Heap),
            _ => None,
        }
    }
}

/// `RecordBuilder::register_block` に渡すフラグ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegFlags(pub u8);

impl RegFlags {
    pub const NONE: RegFlags = RegFlags(0);
    /// 標準のページ。穴を省いてよい。
    pub const STANDARD: RegFlags = RegFlags(0x01);
    /// REDO がページを初期化する。画像を付けない。
    pub const WILL_INIT: RegFlags = RegFlags(0x02);
    /// 必ず画像を付ける（FPI レコード用）。
    pub const FORCE_IMAGE: RegFlags = RegFlags(0x04);
    /// 画像を付けない。
    pub const NO_IMAGE: RegFlags = RegFlags(0x08);
    /// 画像を付けても差分データを残す（M4 の B+Tree 用）。
    pub const KEEP_DATA: RegFlags = RegFlags(0x10);

    pub fn contains(self, other: RegFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for RegFlags {
    type Output = RegFlags;

    fn bitor(self, rhs: RegFlags) -> RegFlags {
        RegFlags(self.0 | rhs.0)
    }
}

// ----- 挿入とデコードの結果 -------------------------------------------------------

/// `Wal::insert` の戻り値。
#[derive(Clone, Copy, Debug)]
pub struct Inserted {
    pub start: Lsn,
    pub end: Lsn,
}

#[derive(Clone, Debug)]
pub struct DecodedBlock {
    pub id: u8,
    pub tag: BufferTag,
    pub will_init: bool,
    /// `HAS_IMAGE` のとき、穴を 0 で埋めた 8192 バイト。
    pub image: Option<Box<Page>>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct DecodedRecord {
    pub start: Lsn,
    /// `start + align8(tot_len)`。
    pub end: Lsn,
    pub xid: Xid,
    pub rmgr: RmgrId,
    pub info: u8,
    pub blocks: Vec<DecodedBlock>,
    pub main: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct WalConfig {
    pub segment_size: u32,
    pub system_identifier: u64,
    /// 本番は常に true。`DebugKnobs` で false にできる。
    pub full_page_writes: bool,
    pub knobs: DebugKnobs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_segment_arithmetic() {
        let seg = DEFAULT_WAL_SEGMENT_SIZE;
        let lsn = Lsn(u64::from(seg) + SEG_HEADER_SIZE);
        assert_eq!(lsn.segno(seg), 1);
        assert_eq!(lsn.seg_offset(seg), 32);
        assert_eq!(Lsn(0x3FF_FFFF).segno(seg), 3);
        assert_eq!(Lsn::INVALID, Lsn::default());
        assert!(Lsn(1) > Lsn::INVALID);
    }

    #[test]
    fn lsn_display_is_hex_hi_slash_lo() {
        assert_eq!(Lsn(0x0100_0020).to_string(), "0/1000020");
        assert_eq!(Lsn(0x1_0000_00AB).to_string(), "1/AB");
        assert_eq!(Lsn(0).to_string(), "0/0");
    }

    #[test]
    fn reg_flags_combine_and_contain() {
        let f = RegFlags::STANDARD | RegFlags::KEEP_DATA;
        assert!(f.contains(RegFlags::STANDARD));
        assert!(f.contains(RegFlags::KEEP_DATA));
        assert!(!f.contains(RegFlags::WILL_INIT));
        assert!(f.contains(RegFlags::NONE));
        assert_eq!(f.0, 0x11);
    }

    #[test]
    fn rmgr_ids_round_trip_and_reject_unknown() {
        for r in [RmgrId::Xlog, RmgrId::Xact, RmgrId::Smgr, RmgrId::Heap] {
            assert_eq!(RmgrId::from_u8(r as u8), Some(r));
        }
        assert_eq!(RmgrId::from_u8(4), None);
        assert_eq!(RmgrId::from_u8(255), None);
    }

    #[test]
    fn constants_are_consistent() {
        assert_eq!(MIN_WAL_SEGMENT_SIZE as usize, 2 * MAX_RECORD_LEN);
        assert!(DEFAULT_WAL_SEGMENT_SIZE.is_power_of_two());
        assert!(MAX_WAL_SEGMENT_SIZE.is_power_of_two());
        assert!(u8::try_from(MAX_BLOCK_REFS).is_ok());
    }
}
