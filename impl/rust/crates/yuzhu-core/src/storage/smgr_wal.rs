//! SMGR rmgr: リレーションファイルの作成・切り詰めの WAL と REDO
//! （`m3.md` §3.8、§4.5、§6.5.2）。
//!
//! M3 で TRUNCATE を出す箇所はない（TRUNCATE 文と VACUUM の切り詰めは M5）。形式・REDO・
//! `smgr.truncate` は M5 で WAL 側に手を入れずに済むよう、ここで作ってテストする。

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use super::buffer::{BufferPool, CriticalSection};
use super::smgr::{BlockNumber, ForkNumber, RelFileLocator, RelFileNumber, StorageManager};
use crate::error::{Error, Result, Severity};
use crate::txn::Xid;
use crate::util::sync::lock;
use crate::wal::{DecodedRecord, RecordBuilder, RedoCtx, RmgrId, Wal};

pub const SMGR_CREATE: u8 = 0x00;
pub const SMGR_TRUNCATE: u8 = 0x10;

/// `CREATE` のメインデータの長さ。
pub const CREATE_LEN: usize = 16;
/// `TRUNCATE` のメインデータの長さ。
pub const TRUNCATE_LEN: usize = 20;

/// SMGR レコードのメインデータ（`TRUNCATE` だけ `nblocks` を持つ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmgrMain {
    pub rel: RelFileLocator,
    pub fork: ForkNumber,
    /// 切り詰めた後のブロック数（`TRUNCATE` のときだけ）。
    pub nblocks: Option<BlockNumber>,
}

impl SmgrMain {
    /// `spc u32, db u32, rel u32, fork u8, 予約 [u8; 3]`（+ `nblocks u32`）。リトルエンディアン。
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(TRUNCATE_LEN);
        b.extend_from_slice(&self.rel.spc_oid.to_le_bytes());
        b.extend_from_slice(&self.rel.db_oid.to_le_bytes());
        b.extend_from_slice(&self.rel.rel_number.0.to_le_bytes());
        b.push(self.fork as u8);
        b.extend_from_slice(&[0; 3]);
        if let Some(n) = self.nblocks {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b
    }

    /// `info` ごとの長さと fork の値を検査する。
    pub fn decode(info: u8, b: &[u8]) -> Result<SmgrMain> {
        let want = match info {
            SMGR_CREATE => CREATE_LEN,
            SMGR_TRUNCATE => TRUNCATE_LEN,
            _ => return Err(bad(format!("unknown SMGR record type {info:#04x}"))),
        };
        if b.len() != want {
            return Err(bad(format!(
                "SMGR record type {info:#04x} has {} bytes of main data, expected {want}",
                b.len()
            )));
        }
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let fork = ForkNumber::from_u8(b[12])
            .ok_or_else(|| bad(format!("SMGR record has an invalid fork number {}", b[12])))?;
        Ok(SmgrMain {
            rel: RelFileLocator {
                spc_oid: u32_at(0),
                db_oid: u32_at(4),
                rel_number: RelFileNumber(u32_at(8)),
            },
            fork,
            nblocks: (info == SMGR_TRUNCATE).then(|| u32_at(16)),
        })
    }
}

fn bad(msg: String) -> Error {
    Error::corrupted(msg).with_severity(Severity::Panic)
}

/// 作成を WAL に記録してからファイルを作る（WAL の flush はしない。§6.5.2）。
///
/// レコードの LSN はそのリレーションのページの LSN より小さいので、ページが書かれる前に
/// WAL が flush される（WAL-before-data）限り、作成の記録なしにページだけが残ることはない。
pub fn log_and_create(
    wal: &Wal,
    smgr: &StorageManager,
    xid: Xid,
    rel: RelFileLocator,
    fork: ForkNumber,
) -> Result<()> {
    let main = SmgrMain {
        rel,
        fork,
        nblocks: None,
    };
    let mut rec = RecordBuilder::new(RmgrId::Smgr, SMGR_CREATE, xid);
    rec.main_data(&main.encode());
    wal.insert(rec)?;
    smgr.create(rel, fork)
}

/// 切り詰めを WAL に記録して flush してから、バッファを捨てて切り詰める。
///
/// 切り詰めは取り消せないので、flush を先にする（PostgreSQL の `RelationTruncate` と同じ）。
/// flush の後の失敗は REDO が完了させられる状態なので Panic にする。
pub fn log_and_truncate(
    wal: &Wal,
    pool: &BufferPool,
    smgr: &StorageManager,
    xid: Xid,
    rel: RelFileLocator,
    fork: ForkNumber,
    nblocks: BlockNumber,
) -> Result<()> {
    let main = SmgrMain {
        rel,
        fork,
        nblocks: Some(nblocks),
    };
    let mut rec = RecordBuilder::new(RmgrId::Smgr, SMGR_TRUNCATE, xid);
    rec.main_data(&main.encode());
    let end = wal.insert(rec)?.end;
    wal.flush(end)?;
    let cs = CriticalSection::enter(pool);
    pool.drop_relation_buffers_from(rel, fork, nblocks)
        .map_err(|e| cs.escalate(e))?;
    smgr.truncate(rel, fork, nblocks)
        .map_err(|e| cs.escalate(e))
}

/// SMGR rmgr の REDO（§6.4.4）。失敗を飲み込まない。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    if rec.rmgr != RmgrId::Smgr {
        return Err(bad(format!(
            "SMGR redo called for a record of another rmgr at {}",
            rec.start
        )));
    }
    let main = SmgrMain::decode(rec.info, &rec.main).map_err(|e| {
        e.with_detail(format!("in the WAL record at {}", rec.start))
            .with_severity(Severity::Panic)
    })?;
    let panic = |e: Error| e.with_severity(Severity::Panic);
    if rec.info == SMGR_CREATE {
        // 0 バイトの残骸（D13）やクラッシュ前に作られたファイルがあれば何もしない。
        if !ctx.smgr.exists(main.rel, main.fork).map_err(panic)? {
            ctx.smgr.create(main.rel, main.fork).map_err(panic)?;
        }
    } else {
        let n = main.nblocks.unwrap_or(0);
        ctx.pool
            .drop_relation_buffers_from(main.rel, main.fork, n)
            .map_err(panic)?;
        if ctx.smgr.exists(main.rel, main.fork).map_err(panic)? {
            ctx.smgr.truncate(main.rel, main.fork, n).map_err(panic)?;
        }
        lock(&ctx.invalid)
            .map_err(panic)?
            .forget_from(main.rel, main.fork, n);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::testing::{TestStorage, test_rel, test_tag};
    use crate::storage::vfs::CrashMode;
    use crate::wal::{Lsn, WalReader};

    fn ctx_for(ts: &TestStorage) -> RedoCtx {
        RedoCtx {
            pool: Arc::clone(ts.pool()),
            smgr: Arc::clone(ts.smgr()),
            ext: Box::new(()),
            invalid: Mutex::new(crate::wal::InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs: DebugKnobs::default(),
        }
    }

    fn read_all(ts: &TestStorage) -> Vec<DecodedRecord> {
        ts.wal().flush(ts.wal().insert_lsn()).unwrap();
        let cfg = *ts.wal().config();
        let start = Lsn(u64::from(cfg.segment_size) + crate::wal::SEG_HEADER_SIZE);
        let mut r = WalReader::open(Arc::new(ts.vfs.clone()), &cfg, start);
        let mut out = Vec::new();
        while let Some(rec) = r.next().unwrap() {
            out.push(rec);
        }
        out
    }

    fn fill(ts: &TestStorage, rel: RelFileLocator, nblocks: u32) {
        for _ in 0..nblocks {
            let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
            let mut g = buf.write().unwrap();
            g.page_mut().init_heap();
            g.page_mut().add_item(b"data").unwrap();
            ts.log_change(&buf, &mut g).unwrap();
        }
    }

    #[test]
    fn info_codes_are_distinct_rmgr_types() {
        assert_ne!(SMGR_CREATE, SMGR_TRUNCATE);
        // 上位 4 ビットだけを使う（§3.3）。
        assert_eq!(SMGR_CREATE & 0x0F, 0);
        assert_eq!(SMGR_TRUNCATE & 0x0F, 0);
    }

    #[test]
    fn main_data_round_trips_with_the_documented_layout() {
        let m = SmgrMain {
            rel: test_rel(0x0102_0304),
            fork: ForkNumber::Fsm,
            nblocks: None,
        };
        let b = m.encode();
        assert_eq!(b.len(), 16);
        assert_eq!(&b[0..4], &1663u32.to_le_bytes());
        assert_eq!(&b[4..8], &5u32.to_le_bytes());
        assert_eq!(&b[8..12], &[4, 3, 2, 1]);
        assert_eq!(&b[12..16], &[1, 0, 0, 0]);
        assert_eq!(SmgrMain::decode(SMGR_CREATE, &b).unwrap(), m);
        let t = SmgrMain {
            nblocks: Some(77),
            ..m
        };
        let b = t.encode();
        assert_eq!(b.len(), 20);
        assert_eq!(&b[16..], &77u32.to_le_bytes());
        assert_eq!(SmgrMain::decode(SMGR_TRUNCATE, &b).unwrap(), t);
    }

    #[test]
    fn decode_rejects_bad_lengths_forks_and_infos() {
        let ok = SmgrMain {
            rel: test_rel(1),
            fork: ForkNumber::Main,
            nblocks: Some(1),
        }
        .encode();
        assert!(SmgrMain::decode(SMGR_CREATE, &ok).is_err());
        assert!(SmgrMain::decode(SMGR_TRUNCATE, &ok[..16]).is_err());
        assert!(SmgrMain::decode(0x20, &ok).is_err());
        let mut b = ok.clone();
        b[12] = 9;
        let e = SmgrMain::decode(SMGR_TRUNCATE, &b).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
    }

    #[test]
    fn log_and_create_writes_a_record_and_creates_the_file() {
        let ts = TestStorage::new();
        let rel = test_rel(5000);
        let before = ts.wal().insert_lsn();
        ts.create_rel_logged(rel).unwrap();
        assert!(ts.smgr().exists(rel, ForkNumber::Main).unwrap());
        assert_eq!(
            ts.wal().flushed_lsn(),
            before,
            "creating must not flush the WAL"
        );
        let recs = read_all(&ts);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].rmgr, RmgrId::Smgr);
        assert_eq!(recs[0].info, SMGR_CREATE);
        assert_eq!(recs[0].xid, Xid::BOOTSTRAP);
        let m = SmgrMain::decode(recs[0].info, &recs[0].main).unwrap();
        assert_eq!((m.rel, m.fork, m.nblocks), (rel, ForkNumber::Main, None));
    }

    #[test]
    fn redo_create_makes_a_missing_file_and_is_idempotent() {
        let src = TestStorage::new();
        let rel = test_rel(5001);
        src.create_rel_logged(rel).unwrap();
        let recs = read_all(&src);
        let dst = TestStorage::new();
        dst.smgr().set_recovery_mode(true);
        let ctx = ctx_for(&dst);
        assert!(!dst.smgr().exists(rel, ForkNumber::Main).unwrap());
        redo(&ctx, &recs[0]).unwrap();
        assert!(dst.smgr().exists(rel, ForkNumber::Main).unwrap());
        redo(&ctx, &recs[0]).unwrap();
        // A D13 leftover also counts as existing.
        dst.smgr().unlink(rel).unwrap();
        redo(&ctx, &recs[0]).unwrap();
        assert_eq!(dst.smgr().nblocks(rel, ForkNumber::Main).unwrap(), 0);
    }

    #[test]
    fn log_and_truncate_flushes_first_then_cuts() {
        let ts = TestStorage::small(16, 4);
        let rel = test_rel(5002);
        ts.create_rel_logged(rel).unwrap();
        fill(&ts, rel, 10);
        ts.pool().flush_all_for_checkpoint().unwrap();
        // Dirty a block that will be cut: it must be discarded unwritten.
        {
            let buf = ts.pool().read_buffer(test_tag(rel, 9)).unwrap();
            let mut g = buf.write().unwrap();
            g.page_mut().add_item(b"zz").unwrap();
            ts.log_change(&buf, &mut g).unwrap();
        }
        let writes = ts.pool().stats().writes;
        log_and_truncate(
            ts.wal(),
            ts.pool(),
            ts.smgr(),
            Xid::FIRST_NORMAL,
            rel,
            ForkNumber::Main,
            6,
        )
        .unwrap();
        assert_eq!(ts.pool().stats().writes, writes);
        assert_eq!(ts.wal().flushed_lsn(), ts.wal().insert_lsn());
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Main).unwrap(), 6);
        let len = |p: &str| ts.vfs.file_contents(Path::new(p)).map(|c| c.len());
        assert_eq!(len("base/5/5002"), Some(4 * 8192));
        assert_eq!(len("base/5/5002.1"), Some(2 * 8192));
        assert_eq!(len("base/5/5002.2"), None);
        let recs = read_all(&ts);
        let last = recs.last().unwrap();
        assert_eq!((last.rmgr, last.info), (RmgrId::Smgr, SMGR_TRUNCATE));
        let m = SmgrMain::decode(last.info, &last.main).unwrap();
        assert_eq!(m.nblocks, Some(6));
        ts.assert_clean();
    }

    #[test]
    fn truncate_survives_a_crash_and_redo_is_idempotent() {
        let ts = TestStorage::small(16, 4);
        let rel = test_rel(5003);
        ts.create_rel_logged(rel).unwrap();
        fill(&ts, rel, 9);
        ts.pool().flush_all_for_checkpoint().unwrap();
        ts.smgr().sync_pending().unwrap();
        log_and_truncate(
            ts.wal(),
            ts.pool(),
            ts.smgr(),
            Xid::FIRST_NORMAL,
            rel,
            ForkNumber::Main,
            4,
        )
        .unwrap();
        let recs = read_all(&ts);
        let ts2 = ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        assert_eq!(ts2.smgr().nblocks(rel, ForkNumber::Main).unwrap(), 4);
        // Replaying the record on the already-short file changes nothing.
        let ctx = ctx_for(&ts2);
        ts2.smgr().set_recovery_mode(true);
        redo(&ctx, recs.last().unwrap()).unwrap();
        assert_eq!(ts2.smgr().nblocks(rel, ForkNumber::Main).unwrap(), 4);
    }

    #[test]
    fn redo_truncate_cuts_a_longer_file_and_forgets_invalid_pages() {
        let ts = TestStorage::small(16, 4);
        let rel = test_rel(5004);
        ts.create_rel(rel).unwrap();
        fill(&ts, rel, 8);
        ts.pool().flush_all_for_checkpoint().unwrap();
        let ctx = ctx_for(&ts);
        ctx.invalid.lock().unwrap().record(rel, ForkNumber::Main, 7);
        let rec = DecodedRecord {
            start: Lsn(1),
            end: Lsn(2),
            xid: Xid::FIRST_NORMAL,
            rmgr: RmgrId::Smgr,
            info: SMGR_TRUNCATE,
            blocks: Vec::new(),
            main: SmgrMain {
                rel,
                fork: ForkNumber::Main,
                nblocks: Some(5),
            }
            .encode(),
        };
        redo(&ctx, &rec).unwrap();
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Main).unwrap(), 5);
        ctx.invalid.lock().unwrap().check_empty().unwrap();
        // A missing file is ignored.
        let gone = DecodedRecord {
            main: SmgrMain {
                rel: test_rel(9999),
                fork: ForkNumber::Main,
                nblocks: Some(0),
            }
            .encode(),
            ..rec.clone()
        };
        redo(&ctx, &gone).unwrap();
        // Garbage is a Panic.
        let broken = DecodedRecord {
            main: vec![0; 3],
            ..rec
        };
        assert_eq!(redo(&ctx, &broken).unwrap_err().severity, Severity::Panic);
        ts.assert_clean();
    }
}
