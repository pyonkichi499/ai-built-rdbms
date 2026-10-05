//! XLOG rmgr: チェックポイントレコードなどのエンコードと REDO
//! （`m3.md` §3.6、§4.4、§6.4.4）。
//!
//! `CheckpointRecord` のエンコード・デコードは A が置いた単純な実装。REDO は
//! 担当 W2 が実装する。

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_pass_by_value
)]

use super::{DecodedRecord, Lsn, RecordBuilder, RedoCtx, RmgrId};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::txn::Xid;
use crate::types::Oid;

pub const XLOG_CHECKPOINT_SHUTDOWN: u8 = 0x00;
pub const XLOG_CHECKPOINT_ONLINE: u8 = 0x10;
pub const XLOG_CHECKPOINT_REDO: u8 = 0x20;
pub const XLOG_SWITCH: u8 = 0x30;
pub const XLOG_FPI: u8 = 0x40;
pub const XLOG_NOOP: u8 = 0x50;
/// 予約（M5）。
pub const XLOG_FPI_FOR_HINT: u8 = 0x60;

/// チェックポイントレコードのメインデータの長さ。
pub const CHECKPOINT_RECORD_LEN: usize = 48;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointRecordKind {
    Shutdown = 0,
    Online = 1,
    EndOfRecovery = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointRecord {
    pub redo: Lsn,
    pub next_xid: Xid,
    pub oldest_xid: Xid,
    pub next_oid: Oid,
    pub kind: CheckpointRecordKind,
    pub full_page_writes: bool,
    /// UNIX 秒。
    pub time: i64,
}

fn corrupt(msg: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid checkpoint record: {msg}"),
    )
    .with_severity(Severity::Panic)
}

impl CheckpointRecord {
    /// `kind` が `Shutdown` / `EndOfRecovery` なら `CHECKPOINT_SHUTDOWN`、`Online` なら
    /// `CHECKPOINT_ONLINE`。
    pub fn info(&self) -> u8 {
        match self.kind {
            CheckpointRecordKind::Online => XLOG_CHECKPOINT_ONLINE,
            _ => XLOG_CHECKPOINT_SHUTDOWN,
        }
    }

    pub fn encode(&self) -> [u8; CHECKPOINT_RECORD_LEN] {
        let mut b = [0u8; CHECKPOINT_RECORD_LEN];
        b[0..8].copy_from_slice(&self.redo.0.to_le_bytes());
        b[8..16].copy_from_slice(&self.next_xid.0.to_le_bytes());
        b[16..24].copy_from_slice(&self.oldest_xid.0.to_le_bytes());
        b[24..28].copy_from_slice(&self.next_oid.to_le_bytes());
        b[28] = self.kind as u8;
        b[29] = u8::from(self.full_page_writes);
        b[32..40].copy_from_slice(&self.time.to_le_bytes());
        b
    }

    /// rmgr・info・長さ・kind の整合を検査する。
    pub fn decode(rec: &DecodedRecord) -> Result<CheckpointRecord> {
        let b = rec.main.as_slice();
        if rec.rmgr != RmgrId::Xlog || !rec.blocks.is_empty() {
            return Err(corrupt("not an XLOG record without blocks"));
        }
        if b.len() != CHECKPOINT_RECORD_LEN {
            return Err(corrupt("bad length"));
        }
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]));
        let kind = match b[28] {
            0 => CheckpointRecordKind::Shutdown,
            1 => CheckpointRecordKind::Online,
            2 => CheckpointRecordKind::EndOfRecovery,
            _ => return Err(corrupt("bad kind")),
        };
        let out = CheckpointRecord {
            redo: Lsn(u64_at(0)),
            next_xid: Xid(u64_at(8)),
            oldest_xid: Xid(u64_at(16)),
            next_oid: u32::from_le_bytes(b[24..28].try_into().unwrap_or([0; 4])),
            kind,
            full_page_writes: b[29] & 1 != 0,
            time: i64::from_le_bytes(b[32..40].try_into().unwrap_or([0; 8])),
        };
        if rec.info != out.info() {
            return Err(corrupt("info does not match kind"));
        }
        Ok(out)
    }

    pub fn builder(&self) -> RecordBuilder<'static> {
        let mut b = RecordBuilder::new(RmgrId::Xlog, self.info(), Xid::INVALID);
        b.main_data(&self.encode());
        b
    }
}

/// XLOG rmgr の REDO（§6.4.4）。担当 W2 が実装する。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let _ = (ctx, rec);
    Err(Error::internal("xlog::redo: 担当 W2 が実装"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kind: CheckpointRecordKind) -> CheckpointRecord {
        CheckpointRecord {
            redo: Lsn(0x0100_0020),
            next_xid: Xid(1234),
            oldest_xid: Xid(3),
            next_oid: 24_576,
            kind,
            full_page_writes: true,
            time: 1_760_000_000,
        }
    }

    fn as_decoded(c: &CheckpointRecord) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(0x0100_0100),
            end: Lsn(0x0100_0150),
            xid: Xid::INVALID,
            rmgr: RmgrId::Xlog,
            info: c.info(),
            blocks: vec![],
            main: c.encode().to_vec(),
        }
    }

    #[test]
    fn encode_decode_round_trip_for_all_kinds() {
        for k in [
            CheckpointRecordKind::Shutdown,
            CheckpointRecordKind::Online,
            CheckpointRecordKind::EndOfRecovery,
        ] {
            let c = sample(k);
            assert_eq!(CheckpointRecord::decode(&as_decoded(&c)).unwrap(), c);
        }
    }

    #[test]
    fn layout_matches_the_spec() {
        let b = sample(CheckpointRecordKind::Online).encode();
        assert_eq!(b.len(), 48);
        assert_eq!(&b[0..8], &0x0100_0020u64.to_le_bytes());
        assert_eq!(b[28], 1);
        assert_eq!(b[29], 1);
        assert_eq!(&b[40..48], &[0u8; 8]);
    }

    #[test]
    fn decode_rejects_inconsistent_records() {
        let c = sample(CheckpointRecordKind::Online);
        let mut r = as_decoded(&c);
        r.info = XLOG_CHECKPOINT_SHUTDOWN;
        assert!(CheckpointRecord::decode(&r).is_err());
        let mut r = as_decoded(&c);
        r.main.pop();
        assert!(CheckpointRecord::decode(&r).is_err());
        let mut r = as_decoded(&c);
        r.main[28] = 9;
        assert!(CheckpointRecord::decode(&r).is_err());
        let mut r = as_decoded(&c);
        r.rmgr = RmgrId::Heap;
        assert!(CheckpointRecord::decode(&r).is_err());
    }

    #[test]
    fn builder_carries_info_and_main_data() {
        let c = sample(CheckpointRecordKind::EndOfRecovery);
        let b = c.builder();
        assert_eq!(b.rmgr, RmgrId::Xlog);
        assert_eq!(b.info, XLOG_CHECKPOINT_SHUTDOWN);
        assert_eq!(b.main, c.encode().to_vec());
    }
}
