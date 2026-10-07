//! 起動時のクラッシュリカバリの全手順と rmgr の振り分け（`m3.md` §4.7、§5.5、§6.8）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::control::{ControlFileHandle, DbState, FLAG_FULL_PAGE_WRITES};
use crate::datadir;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::btree;
use crate::storage::heap;
use crate::storage::sequence;
use crate::storage::smgr_wal;
use crate::storage::stack::{StackConfig, StorageStack};
use crate::storage::vfs::Vfs;
use crate::txn::{Xid, xact_wal};
use crate::types::Oid;
use crate::wal::xlog::{
    CheckpointRecord, CheckpointRecordKind, XLOG_CHECKPOINT_ONLINE, XLOG_CHECKPOINT_SHUTDOWN,
};
use crate::wal::{
    self, DecodedRecord, EndReason, Lsn, RedoCtx, RedoStats, RmgrId, Wal, WalConfig, WalReader,
};

#[derive(Debug)]
pub struct StartupOutcome {
    pub stack: StorageStack,
    pub next_xid: Xid,
    pub next_oid: Oid,
    pub did_redo: bool,
    pub redo_stats: Option<RedoStats>,
}

/// `Cluster::open` の手順 4（§5.5）。制御ファイルは開いて検査済みのものを受け取る。
///
/// 不正な制御ファイル・チェックポイントレコードは FATAL（XX001）、REDO の失敗は
/// `Severity::Panic` のまま返す（どちらも `Cluster::open` の失敗になる）。
#[allow(clippy::similar_names)]
pub fn startup(
    vfs: Arc<dyn Vfs>,
    control: &Arc<ControlFileHandle>,
    cfg: &StackConfig,
) -> Result<StartupOutcome> {
    let data = control.get();
    let wal_cfg = WalConfig {
        segment_size: data.wal_segment_size,
        system_identifier: data.system_identifier,
        full_page_writes: data.flags & FLAG_FULL_PAGE_WRITES != 0,
        knobs: cfg.knobs,
    };
    let checkpoint_lsn = Lsn(data.checkpoint_lsn);

    // a. チェックポイントレコードを読む。
    let mut reader = WalReader::open(Arc::clone(&vfs), &wal_cfg, checkpoint_lsn);
    let (ckpt_rec, ckpt) = read_checkpoint(&mut reader, checkpoint_lsn)?;
    if ckpt.redo.0 != data.redo_lsn {
        return Err(fatal(
            "checkpoint record is inconsistent with control file".to_string(),
        ));
    }

    // b. 正常停止か（制御ファイルが ShutDown で、停止系のチェックポイントの後ろに WAL がない）。
    let clean = data.state == DbState::ShutDown
        && matches!(
            ckpt.kind,
            CheckpointRecordKind::Shutdown | CheckpointRecordKind::EndOfRecovery
        )
        && reader.next()?.is_none();

    if clean {
        // c.
        log(&format!(
            "database system was shut down at {}",
            format_unix_time(data.time)
        ));
        let wal = Wal::open_at(Arc::clone(&vfs), wal_cfg, ckpt_rec.end, ckpt_rec.start)?;
        let next_xid = Xid(data.next_xid);
        let stack = StorageStack::new(vfs, cfg, wal, next_xid)?;
        return Ok(StartupOutcome {
            stack,
            next_xid,
            next_oid: data.next_oid,
            did_redo: false,
            redo_stats: None,
        });
    }

    let from = Checkpoint {
        rec: ckpt_rec,
        body: ckpt,
    };
    crash_recovery(&vfs, control, cfg, &wal_cfg, &from)
}

/// 制御ファイルが指すチェックポイント。
struct Checkpoint {
    rec: DecodedRecord,
    body: CheckpointRecord,
}

/// §5.5 の手順 d（クラッシュリカバリ）。
#[allow(clippy::similar_names)]
fn crash_recovery(
    vfs: &Arc<dyn Vfs>,
    control: &Arc<ControlFileHandle>,
    cfg: &StackConfig,
    wal_cfg: &WalConfig,
    from: &Checkpoint,
) -> Result<StartupOutcome> {
    let data = control.get();
    let (ckpt_rec, ckpt) = (&from.rec, &from.body);
    let wal_cfg = *wal_cfg;
    let checkpoint_lsn = ckpt_rec.start;
    log(&format!(
        "database system was interrupted; last known up at {}",
        format_unix_time(data.time)
    ));
    if data.state == DbState::ShutDown {
        warn("database system was shut down but the WAL continues after the last checkpoint");
    }
    control.update(|c| c.state = DbState::InCrashRecovery)?;
    datadir::sync_data_directory(vfs.as_ref())?;

    let wal = Wal::open_for_recovery(Arc::clone(vfs), wal_cfg);
    let start_xid = Xid(data.next_xid.max(ckpt.next_xid.0));
    let stack = StorageStack::new(Arc::clone(vfs), cfg, Arc::clone(&wal), start_xid)?;
    stack.smgr.set_recovery_mode(true);

    let ctx = RedoCtx {
        pool: Arc::clone(&stack.pool),
        smgr: Arc::clone(&stack.smgr),
        ext: Box::new(Arc::clone(&stack.clog)),
        invalid: Mutex::new(wal::InvalidPages::default()),
        next_oid: AtomicU32::new(data.next_oid.max(ckpt.next_oid)),
        knobs: cfg.knobs,
    };
    let mut reader = WalReader::open(Arc::clone(vfs), &wal_cfg, ckpt.redo);
    log(&format!("redo starts at {}", ckpt.redo));
    let mut max_xid = Xid::INVALID;
    let stats = wal::redo::run_redo(&ctx, &wal, &mut reader, &dispatch, &mut |rec| {
        max_xid = max_xid.max(rec.xid);
    })?;
    ctx.invalid
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check_empty()?;

    let Some((end, reason)) = reader.end_of_wal() else {
        return Err(
            Error::internal("the WAL reader did not report the end of the WAL")
                .with_severity(Severity::Panic),
        );
    };
    // チェックポイントレコード自体は読めているので、それより手前で終わるのは WAL の欠落。
    if end < ckpt_rec.end {
        return Err(fatal(format!(
            "WAL ends at {end}, before the end of the checkpoint record at {checkpoint_lsn}"
        )));
    }
    let last_start = reader.last_record_start().unwrap_or(ckpt_rec.start);
    log(&describe_end(end, &reason));
    log(&format!("redo done at {last_start}"));

    wal.finish_recovery(end, last_start)?;
    stack.smgr.set_recovery_mode(false);

    let next_xid = Xid(data
        .next_xid
        .max(ckpt.next_xid.0)
        .max(max_xid.0.saturating_add(1)));
    let next_oid = ctx.next_oid.load(Ordering::Acquire);
    control.update(|c| {
        c.next_xid = c.next_xid.max(next_xid.0);
        c.next_oid = c.next_oid.max(next_oid);
    })?;
    stack.clog.load_page_for(next_xid)?;
    Ok(StartupOutcome {
        stack,
        next_xid,
        next_oid,
        did_redo: true,
        redo_stats: Some(stats),
    })
}

fn fatal(message: String) -> Error {
    Error::new(sqlstate::DATA_CORRUPTED, message).with_severity(Severity::Fatal)
}

/// 制御ファイルが指すチェックポイントレコードを読む。読めない・形が違うなら FATAL XX001。
fn read_checkpoint(reader: &mut WalReader, lsn: Lsn) -> Result<(DecodedRecord, CheckpointRecord)> {
    let not_found = || {
        fatal(format!(
            "could not locate a valid checkpoint record at {lsn}"
        ))
    };
    if lsn == Lsn::INVALID {
        return Err(not_found());
    }
    let rec = reader.next()?.ok_or_else(not_found)?;
    let is_checkpoint = rec.start == lsn
        && rec.rmgr == RmgrId::Xlog
        && matches!(rec.info, XLOG_CHECKPOINT_SHUTDOWN | XLOG_CHECKPOINT_ONLINE);
    if !is_checkpoint {
        return Err(not_found());
    }
    let ckpt = CheckpointRecord::decode(&rec).map_err(|_| not_found())?;
    Ok((rec, ckpt))
}

fn describe_end(end: Lsn, reason: &EndReason) -> String {
    match reason {
        EndReason::ZeroLength => format!("end of WAL at {end}"),
        other => format!("invalid record at {end}: {other:?}"),
    }
}

#[allow(clippy::print_stderr)]
pub(crate) fn log(msg: &str) {
    eprintln!("LOG:  {msg}");
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

/// UNIX 秒を `YYYY-MM-DD HH:MM:SS UTC` にする（外部クレートなし）。
pub(crate) fn format_unix_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant の civil_from_days。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// rmgr の振り分け（ほかに置かない）。
pub fn dispatch(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    match rec.rmgr {
        RmgrId::Xlog => wal::xlog::redo(ctx, rec),
        RmgrId::Xact => xact_wal::redo(ctx, rec),
        RmgrId::Smgr => smgr_wal::redo(ctx, rec),
        RmgrId::Heap => heap::wal::redo(ctx, rec),
        // 呼び先は B1 / Q1 が本実装に置き換える（P0-b のスタブは 0A000）。
        RmgrId::Btree => btree::wal::redo(ctx, rec),
        RmgrId::Seq => sequence::redo(ctx, rec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{InitdbOptions, initdb};
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::buffer::{BufferPool, NoWal};
    use crate::storage::smgr::StorageManager;
    use crate::storage::vfs::{CrashMode, SimVfs};
    use crate::txn::clog::Clog;
    use crate::wal::xlog::CheckpointRecordKind as Kind;

    fn ctx() -> RedoCtx {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), 1024));
        let pool = BufferPool::new(8, Arc::clone(&smgr), Arc::new(NoWal), DebugKnobs::default());
        let clog = Arc::new(Clog::open(vfs, Xid(3)).unwrap());
        RedoCtx {
            pool,
            smgr,
            ext: Box::new(clog),
            invalid: Mutex::new(wal::InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs: DebugKnobs::default(),
        }
    }

    fn rec(rmgr: RmgrId) -> DecodedRecord {
        DecodedRecord {
            start: wal::Lsn(0x20_0020),
            end: wal::Lsn(0x20_0040),
            xid: Xid(5),
            rmgr,
            info: 0xF0,
            blocks: vec![],
            main: vec![],
        }
    }

    #[test]
    fn dispatch_routes_each_rmgr_to_its_owner() {
        let c = ctx();
        // An unknown `info` is rejected by each rmgr in its own words.
        for (rmgr, owner) in [
            (RmgrId::Xlog, "XLOG"),
            (RmgrId::Xact, "transaction WAL"),
            (RmgrId::Smgr, "SMGR"),
            (RmgrId::Heap, "heap WAL"),
        ] {
            let e = dispatch(&c, &rec(rmgr)).unwrap_err();
            assert!(e.message.contains(owner), "{rmgr:?}: {}", e.message);
        }
    }

    fn cfg() -> StackConfig {
        StackConfig {
            rel_seg_blocks: crate::storage::DEFAULT_RELSEG_SIZE,
            nframes: 64,
            knobs: DebugKnobs::default(),
        }
    }

    fn fresh() -> (SimVfs, Arc<dyn Vfs>, Arc<ControlFileHandle>) {
        let sim = SimVfs::new(11);
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        initdb(Arc::clone(&vfs), &InitdbOptions::new("postgres")).unwrap();
        let control = Arc::new(ControlFileHandle::open(&vfs).unwrap());
        (sim, vfs, control)
    }

    #[test]
    fn clean_start_does_not_redo() {
        let (_sim, vfs, control) = fresh();
        let out = startup(vfs, &control, &cfg()).unwrap();
        assert!(!out.did_redo);
        assert!(out.redo_stats.is_none());
        let d = control.get();
        assert_eq!(out.next_xid, Xid(d.next_xid));
        assert_eq!(out.next_oid, d.next_oid);
        // `startup` itself does not change `state` (the engine does).
        assert_eq!(d.state, DbState::ShutDown);
        // The WAL writes on from the end of the checkpoint record.
        let ins = out.stack.wal.insert_lsn();
        assert!(ins.0 > d.checkpoint_lsn);
    }

    #[test]
    fn unclean_state_with_nothing_to_redo_still_goes_through_recovery() {
        let (_sim, vfs, control) = fresh();
        control.update(|c| c.state = DbState::InProduction).unwrap();
        let out = startup(vfs, &control, &cfg()).unwrap();
        assert!(out.did_redo);
        let stats = out.redo_stats.unwrap();
        // Only the checkpoint record itself is read.
        assert_eq!(stats.records, 1);
        assert_eq!(control.get().state, DbState::InCrashRecovery);
        // Write mode again: an insert works and continues the stream.
        let before = out.stack.wal.insert_lsn();
        assert!(before.0 > 0);
    }

    #[test]
    fn shut_down_state_with_trailing_wal_is_recovered() {
        let (_sim, vfs, control) = fresh();
        // Log something after the checkpoint using a normal start.
        {
            let out = startup(Arc::clone(&vfs), &control, &cfg()).unwrap();
            let rec = CheckpointRecord {
                redo: out.stack.wal.insert_lsn(),
                next_xid: Xid(3),
                oldest_xid: Xid(3),
                next_oid: 16384,
                kind: Kind::Online,
                full_page_writes: true,
                time: 0,
            };
            let ins = out.stack.wal.insert(rec.builder()).unwrap();
            out.stack.wal.flush(ins.end).unwrap();
        }
        assert_eq!(control.get().state, DbState::ShutDown);
        let out = startup(vfs, &control, &cfg()).unwrap();
        assert!(out.did_redo);
        assert_eq!(out.redo_stats.unwrap().records, 2);
    }

    #[test]
    fn checkpoint_pointer_errors_are_fatal_xx001() {
        for (name, edit) in [
            (
                "zero",
                Box::new(|c: &mut crate::control::ControlData| c.checkpoint_lsn = 0)
                    as Box<dyn Fn(&mut crate::control::ControlData)>,
            ),
            ("shifted", Box::new(|c| c.checkpoint_lsn += 8)),
            ("far", Box::new(|c| c.checkpoint_lsn += 1 << 30)),
        ] {
            let (_sim, vfs, control) = fresh();
            control.update(|c| edit(c)).unwrap();
            let e = startup(vfs, &control, &cfg()).unwrap_err();
            assert_eq!(e.severity, Severity::Fatal, "{name}");
            assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED, "{name}");
            assert!(
                e.message
                    .starts_with("could not locate a valid checkpoint record at"),
                "{name}: {}",
                e.message
            );
        }
    }

    #[test]
    fn redo_point_disagreeing_with_the_control_file_is_fatal() {
        let (_sim, vfs, control) = fresh();
        control.update(|c| c.redo_lsn += 8).unwrap();
        let e = startup(vfs, &control, &cfg()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert!(e.message.contains("inconsistent with control file"));
    }

    #[test]
    fn missing_wal_segment_is_fatal() {
        let (_sim, vfs, control) = fresh();
        let lsn = Lsn(control.get().checkpoint_lsn);
        let seg = control.get().wal_segment_size;
        vfs.remove_file(&wal::segment::segment_path(lsn.segno(seg)))
            .unwrap();
        let e = startup(vfs, &control, &cfg()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.message.contains("could not locate a valid checkpoint"));
    }

    #[test]
    fn interrupted_recovery_is_repeated_from_the_same_checkpoint() {
        let (sim, vfs, control) = fresh();
        control.update(|c| c.state = DbState::InProduction).unwrap();
        let first = startup(vfs, &control, &cfg()).unwrap();
        let ckpt = control.get().checkpoint_lsn;
        drop(first);
        // Crash before the end-of-recovery checkpoint.
        let disk = sim.crash(CrashMode::DropUnsynced);
        let vfs2: Arc<dyn Vfs> = Arc::new(disk);
        let control2 = Arc::new(ControlFileHandle::open(&vfs2).unwrap());
        assert_eq!(control2.get().state, DbState::InCrashRecovery);
        assert_eq!(control2.get().checkpoint_lsn, ckpt);
        let second = startup(vfs2, &control2, &cfg()).unwrap();
        assert!(second.did_redo);
    }

    #[test]
    fn formats_unix_time() {
        assert_eq!(format_unix_time(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix_time(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(format_unix_time(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }
}
