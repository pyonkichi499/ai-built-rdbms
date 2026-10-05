//! XACT rmgr: COMMIT / ABORT レコードと REDO（`m3.md` §3.7、§4.6、§6.6.1）。

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;

use super::Xid;
use super::clog::{Clog, XidStatus};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::smgr::{RelFileLocator, RelFileNumber};
use crate::wal::{DecodedRecord, MAX_RECORD_LEN, RecordBuilder, RedoCtx, RmgrId};

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

/// 1 レコードに載せられるリレーション数の上限（ヘッダ分の余裕を見る）。
pub const MAX_RELS_PER_RECORD: usize = (MAX_RECORD_LEN - 256) / REL_LEN;

impl XactRecord {
    /// `info` は `XACT_COMMIT` か `XACT_ABORT`。
    pub fn builder(&self, info: u8, xid: Xid) -> RecordBuilder<'static> {
        let mut b = RecordBuilder::new(RmgrId::Xact, info, xid);
        b.main_data(&self.encode());
        b
    }

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

/// XACT rmgr の REDO（§6.4.4）。`ctx.ext` から `Arc<Clog>` を取り出して使う。
///
/// COMMIT / ABORT とも clog を更新し（同じ値の再適用は許す）、`rels` のリレーションを捨てる。
/// 形式の不正や矛盾は `Severity::Panic`。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let status = match rec.info {
        XACT_COMMIT => XidStatus::Committed,
        XACT_ABORT => XidStatus::Aborted,
        other => return Err(corrupt(&format!("unknown info {other:#04x}"))),
    };
    if rec.rmgr != RmgrId::Xact || !rec.blocks.is_empty() {
        return Err(corrupt("not an XACT record without blocks"));
    }
    if !rec.xid.is_normal() {
        return Err(corrupt("record has no regular transaction ID"));
    }
    let body = XactRecord::decode(&rec.main)?;
    let clog = ctx.ext.downcast_ref::<Arc<Clog>>().ok_or_else(|| {
        Error::internal("REDO context does not hold the commit log").with_severity(Severity::Panic)
    })?;
    clog.set_status_redo(rec.xid, status)?;
    for &rel in &body.rels {
        ctx.pool
            .drop_relation_buffers(rel)
            .map_err(|e| e.with_severity(Severity::Panic))?;
        ctx.smgr
            .unlink(rel)
            .map_err(|e| e.with_severity(Severity::Panic))?;
        ctx.invalid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forget_relation(rel);
    }
    Ok(())
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

#[cfg(test)]
mod redo_tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::buffer::BufferPool;
    use crate::storage::buffer::NoWal;
    use crate::storage::smgr::{ForkNumber, StorageManager};
    use crate::storage::vfs::{SimVfs, Vfs};
    use crate::wal::{InvalidPages, Lsn};
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    struct Env {
        ctx: RedoCtx,
        clog: Arc<Clog>,
    }

    fn env() -> Env {
        let sim = SimVfs::new(5);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        sim.create_dir_all(Path::new("base/5")).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), 131_072));
        smgr.set_recovery_mode(true);
        let pool = BufferPool::new(8, Arc::clone(&smgr), Arc::new(NoWal), DebugKnobs::default());
        let clog = Arc::new(Clog::open(vfs, Xid(3)).unwrap());
        let ctx = RedoCtx {
            pool,
            smgr,
            ext: Box::new(Arc::clone(&clog)),
            invalid: Mutex::new(InvalidPages::default()),
            next_oid: AtomicU32::new(100),
            knobs: DebugKnobs::default(),
        };
        Env { ctx, clog }
    }

    fn record(info: u8, xid: Xid, rels: Vec<RelFileLocator>) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(100),
            end: Lsn(200),
            xid,
            rmgr: RmgrId::Xact,
            info,
            blocks: Vec::new(),
            main: XactRecord { time_us: 1, rels }.encode(),
        }
    }

    #[test]
    fn commit_and_abort_set_the_commit_log() {
        let e = env();
        redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![])).unwrap();
        redo(&e.ctx, &record(XACT_ABORT, Xid(6), vec![])).unwrap();
        assert_eq!(e.clog.status(Xid(5)).unwrap(), XidStatus::Committed);
        assert_eq!(e.clog.status(Xid(6)).unwrap(), XidStatus::Aborted);
        // REDO twice gives the same result.
        redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![])).unwrap();
        assert_eq!(e.clog.status(Xid(5)).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn contradicting_records_are_panic() {
        let e = env();
        redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![])).unwrap();
        let err = redo(&e.ctx, &record(XACT_ABORT, Xid(5), vec![])).unwrap_err();
        assert_eq!(err.severity, Severity::Panic);
    }

    #[test]
    fn malformed_records_are_panic() {
        let e = env();
        let bad_info = record(0x30, Xid(5), vec![]);
        assert_eq!(
            redo(&e.ctx, &bad_info).unwrap_err().severity,
            Severity::Panic
        );
        let bad_xid = record(XACT_COMMIT, Xid::BOOTSTRAP, vec![]);
        assert_eq!(
            redo(&e.ctx, &bad_xid).unwrap_err().severity,
            Severity::Panic
        );
        let mut bad_main = record(XACT_COMMIT, Xid(5), vec![]);
        bad_main.main.pop();
        assert_eq!(
            redo(&e.ctx, &bad_main).unwrap_err().severity,
            Severity::Panic
        );
        let mut bad_rmgr = record(XACT_COMMIT, Xid(5), vec![]);
        bad_rmgr.rmgr = RmgrId::Heap;
        assert!(redo(&e.ctx, &bad_rmgr).is_err());
        assert_eq!(e.clog.status(Xid(5)).unwrap(), XidStatus::InProgress);
    }

    #[test]
    fn missing_clog_in_the_context_is_an_error() {
        let mut e = env();
        e.ctx.ext = Box::new(());
        assert!(redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![])).is_err());
    }

    #[test]
    fn listed_relations_are_removed_and_forgotten() {
        let e = env();
        let (a, b) = (rel(16384), rel(16385));
        e.ctx.smgr.create(a, ForkNumber::Main).unwrap();
        e.ctx.smgr.extend(a, ForkNumber::Main).unwrap();
        e.ctx.invalid.lock().unwrap().record(a, ForkNumber::Main, 7);
        // `b` was never created (it was dropped again before the crash).
        redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![a, b])).unwrap();
        assert_eq!(e.ctx.smgr.nblocks(a, ForkNumber::Main).unwrap_or(0), 0);
        assert!(e.ctx.invalid.lock().unwrap().is_empty());
        // The same list again (REDO after REDO) is fine.
        redo(&e.ctx, &record(XACT_COMMIT, Xid(5), vec![a, b])).unwrap();
        // ABORT removes the files the transaction created.
        let c = rel(16386);
        e.ctx.smgr.create(c, ForkNumber::Main).unwrap();
        redo(&e.ctx, &record(XACT_ABORT, Xid(6), vec![c])).unwrap();
        assert_eq!(e.ctx.smgr.nblocks(c, ForkNumber::Main).unwrap_or(0), 0);
    }
}
