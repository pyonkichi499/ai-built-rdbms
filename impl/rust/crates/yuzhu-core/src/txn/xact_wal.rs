//! XACT rmgr: COMMIT / ABORT レコードと REDO（`m3.md` §3.7、§4.6、§6.6.1）。
//!
//! 担当 E が実装する。`XactRecord` のエンコード・デコードは A が置いた実装、
//! `redo` はスタブ。

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_pass_by_value
)]

use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::smgr::{RelFileLocator, RelFileNumber};
use crate::wal::{DecodedRecord, RedoCtx};

pub const XACT_COMMIT: u8 = 0x00;
pub const XACT_ABORT: u8 = 0x10;

/// メインデータの固定部の長さ。
const FIXED_LEN: usize = 16;
const REL_LEN: usize = 12;

/// COMMIT / ABORT のメインデータ。COMMIT の `rels` は削除するもの（`pending_unlinks`）、
/// ABORT は作って消すもの（`pending_creates`）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct XactRecord {
    /// UNIX エポックからのマイクロ秒。
    pub time_us: i64,
    pub rels: Vec<RelFileLocator>,
}

fn corrupt(what: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid transaction WAL record: {what}"),
    )
    .with_severity(Severity::Panic)
}

impl XactRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(FIXED_LEN + REL_LEN * self.rels.len());
        b.extend_from_slice(&self.time_us.to_le_bytes());
        b.extend_from_slice(&(self.rels.len() as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        for r in &self.rels {
            b.extend_from_slice(&r.spc_oid.to_le_bytes());
            b.extend_from_slice(&r.db_oid.to_le_bytes());
            b.extend_from_slice(&r.rel_number.0.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < FIXED_LEN {
            return Err(corrupt("too short"));
        }
        let time_us = i64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8]));
        let nrels = u32::from_le_bytes(b[8..12].try_into().unwrap_or([0; 4])) as usize;
        if nrels
            .checked_mul(REL_LEN)
            .and_then(|n| n.checked_add(FIXED_LEN))
            != Some(b.len())
        {
            return Err(corrupt("length does not match nrels"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or([0; 4]));
        let rels = (0..nrels)
            .map(|i| {
                let o = FIXED_LEN + i * REL_LEN;
                RelFileLocator {
                    spc_oid: u32_at(o),
                    db_oid: u32_at(o + 4),
                    rel_number: RelFileNumber(u32_at(o + 8)),
                }
            })
            .collect();
        Ok(XactRecord { time_us, rels })
    }
}

/// XACT rmgr の REDO。`ctx.ext` から `Arc<Clog>` を取り出して使う。担当 E が実装する。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let _ = (ctx, rec);
    Err(Error::internal("xact_wal::redo: 担当 E が実装"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    #[test]
    fn round_trip_with_and_without_relations() {
        for rels in [vec![], vec![rel(16384)], vec![rel(1), rel(2), rel(3)]] {
            let r = XactRecord {
                time_us: 1_760_000_000_123_456,
                rels,
            };
            let b = r.encode();
            assert_eq!(b.len(), 16 + 12 * r.rels.len());
            assert_eq!(XactRecord::decode(&b).unwrap(), r);
        }
    }

    #[test]
    fn layout_matches_the_spec() {
        let b = XactRecord {
            time_us: 7,
            rels: vec![rel(16384)],
        }
        .encode();
        assert_eq!(&b[0..8], &7i64.to_le_bytes());
        assert_eq!(&b[8..12], &1u32.to_le_bytes());
        assert_eq!(&b[12..16], &[0; 4]);
        assert_eq!(&b[16..20], &1663u32.to_le_bytes());
        assert_eq!(&b[24..28], &16384u32.to_le_bytes());
    }

    #[test]
    fn decode_rejects_bad_lengths() {
        assert!(XactRecord::decode(&[0; 15]).is_err());
        let mut b = XactRecord {
            time_us: 0,
            rels: vec![rel(1)],
        }
        .encode();
        b.pop();
        assert_eq!(
            XactRecord::decode(&b).unwrap_err().severity,
            Severity::Panic
        );
        // nrels が大きすぎる
        let mut b = XactRecord {
            time_us: 0,
            rels: vec![],
        }
        .encode();
        b[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(XactRecord::decode(&b).is_err());
    }
}
