//! The checkpoint procedure and the checkpointer thread (`m3.md` §4.7,
//! §5.6, §6.6.5). This module is the only owner of the procedure; `Cluster`
//! just calls [`run`].
//!
//! It is a fuzzy checkpoint: the REDO point is fixed under the exclusive
//! commit gate, then the data written before that point is made durable
//! while writers go on.

use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::control::{ControlFileHandle, DbState};
use crate::datadir::OidAllocator;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::StorageManager;
use crate::txn::TxnManager;
use crate::txn::clog::Clog;
use crate::util::sync::lock;
use crate::wal::record::encode_record;
use crate::wal::segment::{align8, normalize};
use crate::wal::xlog::{CheckpointRecord, CheckpointRecordKind};
use crate::wal::{Lsn, Wal};

/// The oldest XID that may still appear in the heap (no freezing in M3).
const OLDEST_XID: crate::txn::Xid = crate::txn::Xid::FIRST_NORMAL;

/// Borrowed pieces a checkpoint needs.
#[derive(Debug)]
pub struct CheckpointParts<'a> {
    pub pool: &'a BufferPool,
    pub smgr: &'a StorageManager,
    pub clog: &'a Clog,
    pub wal: &'a Wal,
    pub txn: &'a TxnManager,
    pub control: &'a ControlFileHandle,
    pub oids: &'a OidAllocator,
    pub checkpoint_lock: &'a Mutex<()>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointKind {
    /// `checkpoint_timeout` elapsed. Skipped when nothing was logged since
    /// the previous checkpoint.
    Periodic,
    /// The `CHECKPOINT` statement.
    Explicit,
    /// `max_wal_size` was reached.
    WalVolume,
    /// Writes the exact `next_xid` and sets `state` to `ShutDown` (§6.7.3).
    /// There must be no other writer.
    Shutdown,
    /// The checkpoint after crash recovery. There must be no other writer.
    EndOfRecovery,
}

impl CheckpointKind {
    /// Whether other transactions may be running (REDO point under the gate).
    fn is_online(self) -> bool {
        !matches!(
            self,
            CheckpointKind::Shutdown | CheckpointKind::EndOfRecovery
        )
    }

    fn record_kind(self) -> CheckpointRecordKind {
        match self {
            CheckpointKind::Shutdown => CheckpointRecordKind::Shutdown,
            CheckpointKind::EndOfRecovery => CheckpointRecordKind::EndOfRecovery,
            _ => CheckpointRecordKind::Online,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CheckpointResult {
    /// A `Periodic` checkpoint that found nothing to do.
    pub skipped: bool,
    pub redo: Lsn,
    pub checkpoint_lsn: Lsn,
    pub buffers_written: u64,
}

/// What the checkpointer thread should run now, if anything: `WalVolume`
/// when `bytes_since_redo >= max_wal_size`, else `Periodic` when
/// `checkpoint_timeout` has passed since `last`.
pub fn due(
    last: Instant,
    checkpoint_timeout: Duration,
    wal: &Wal,
    max_wal_size: u64,
) -> Option<CheckpointKind> {
    if wal.bytes_since_redo() >= max_wal_size {
        Some(CheckpointKind::WalVolume)
    } else if last.elapsed() >= checkpoint_timeout {
        Some(CheckpointKind::Periodic)
    } else {
        None
    }
}

/// Runs one checkpoint (§5.6). One checkpoint runs at a time.
///
/// The caller must not hold the storage barrier (shared or exclusive) nor
/// the `proc` / control file mutexes; the writer lock does not matter.
///
/// An error from the buffer flush, `sync_pending` or the WAL is already
/// `Severity::Panic` (they own the fsync policy); this function returns it
/// as is. The control file is not touched in that case, so a failed
/// shutdown checkpoint never marks the cluster `ShutDown`.
pub fn run(parts: &CheckpointParts<'_>, kind: CheckpointKind) -> Result<CheckpointResult> {
    let _serial = lock(parts.checkpoint_lock)?;

    // After a failed write or fsync the dirty/pending sets were already
    // discarded, so a retry would find nothing to write and look durable.
    // Refuse (also for Shutdown, so the state never becomes ShutDown).
    if parts.pool.is_poisoned() || parts.smgr.is_broken() || parts.wal.is_poisoned() {
        return Err(Error::new(
            sqlstate::IO_ERROR,
            "refusing to checkpoint: a previous write or fsync failed",
        )
        .with_severity(Severity::Panic));
    }
    // 0. Nothing but the previous checkpoint is in the WAL.
    if kind == CheckpointKind::Periodic && nothing_logged_since_last(parts)? {
        let c = parts.control.get();
        return Ok(CheckpointResult {
            skipped: true,
            redo: Lsn(c.redo_lsn),
            checkpoint_lsn: Lsn(c.checkpoint_lsn),
            buffers_written: 0,
        });
    }
    run_locked(parts, kind).inspect_err(|e| {
        if e.severity == Severity::Panic {
            parts.pool.poison_flag().set();
        }
    })
}

/// Whether the insert position is the end of the last checkpoint record
/// recorded in the control file, i.e. only checkpoint records were logged.
fn nothing_logged_since_last(parts: &CheckpointParts<'_>) -> Result<bool> {
    let last = Lsn(parts.control.get().checkpoint_lsn);
    if last == Lsn::INVALID {
        return Ok(false);
    }
    // The record has no blocks, so its length does not depend on its values.
    let sample = CheckpointRecord {
        redo: Lsn::INVALID,
        next_xid: OLDEST_XID,
        oldest_xid: OLDEST_XID,
        next_oid: 0,
        kind: CheckpointRecordKind::Online,
        full_page_writes: true,
        time: 0,
    };
    let len = encode_record(&sample.builder(), Lsn::INVALID, false)?.len() as u64;
    let end = normalize(Lsn(last.0 + align8(len)), parts.wal.config().segment_size);
    Ok(parts.wal.insert_lsn() == end)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[allow(clippy::similar_names)]
fn run_locked(parts: &CheckpointParts<'_>, kind: CheckpointKind) -> Result<CheckpointResult> {
    let knobs = parts.wal.config().knobs;
    // 1. Fix the REDO point. Online: under the exclusive commit gate, so
    // that every commit record before it has its clog bit set in memory.
    let (redo, cycle) = if kind.is_online() {
        let _gate = parts.txn.commit_gate_exclusive()?;
        let redo = parts.wal.begin_checkpoint_online()?;
        (redo, parts.smgr.begin_unlink_cycle())
    } else {
        let redo = parts.wal.begin_checkpoint_quiet()?;
        (redo, parts.smgr.begin_unlink_cycle())
    };

    // 2. Read the counters after the REDO point (the `proc` and OID mutexes
    // are released before the control file mutex is taken, §5.9): every XID
    // in a record before the REDO point is below `next_xid`.
    let (next_xid, xid_limit) = parts.txn.xid_counters();
    let next_oid = parts.oids.limit();

    // 3. The commit log.
    if !knobs.skip_clog_flush_at_checkpoint {
        parts.clog.flush()?;
    }

    // 4. The pages that are dirty now. The shared barrier keeps a DROP
    // cleanup from removing files under the flush.
    let stats = {
        let _barrier = parts.txn.statement_barrier()?;
        parts.pool.flush_all_for_checkpoint()?
    };

    // 5. The data files.
    parts.smgr.sync_pending()?;

    // 6. The checkpoint record.
    let rec = CheckpointRecord {
        redo,
        next_xid,
        oldest_xid: OLDEST_XID,
        next_oid,
        kind: kind.record_kind(),
        full_page_writes: parts.wal.config().full_page_writes && !knobs.disable_full_page_writes,
        time: unix_seconds(),
    };
    let ins = parts.wal.insert(rec.builder())?;
    parts.wal.flush(ins.end)?;

    // 7. Record it in the control file; this is the completion point.
    // `max` keeps the control file from moving backwards if a
    // pre-allocation raced with us (Shutdown may lower `next_xid` on
    // purpose).
    if kind == CheckpointKind::Shutdown {
        parts.control.update_for_shutdown(|c| {
            c.checkpoint_lsn = ins.start.0;
            c.redo_lsn = redo.0;
            c.next_xid = next_xid.0;
            c.next_oid = next_oid;
            c.state = DbState::ShutDown;
        })?;
    } else {
        parts.control.update(|c| {
            c.checkpoint_lsn = ins.start.0;
            c.redo_lsn = redo.0;
            c.next_xid = c.next_xid.max(xid_limit.0);
            c.next_oid = c.next_oid.max(next_oid);
            c.state = DbState::InProduction;
        })?;
    }

    // 8-9. Leftovers: the checkpoint is complete, so a failure is only a
    // warning.
    if let Err(e) = parts.smgr.finish_pending_unlinks(cycle) {
        warn(&format!(
            "checkpoint could not remove files of dropped relations: {}",
            e.message
        ));
    }
    if let Err(e) = parts.wal.remove_segments_before(redo) {
        warn(&format!(
            "checkpoint could not remove old WAL segments: {}",
            e.message
        ));
    }
    Ok(CheckpointResult {
        skipped: false,
        redo,
        checkpoint_lsn: ins.start,
        buffers_written: stats.written,
    })
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

/// Starts the checkpointer thread; it wakes every `poll` and calls `tick`.
/// `tick` borrows the parts for each run (it upgrades a `Weak<Cluster>`),
/// decides with [`due`] whether a checkpoint is needed, and returns false
/// when the cluster is gone, which ends the thread. Errors are `tick`'s to
/// log (a non-Panic error is retried at the next wake-up).
pub fn spawn_checkpointer(
    poll: Duration,
    tick: Box<dyn Fn() -> bool + Send>,
) -> CheckpointerHandle {
    let (stop, rx) = mpsc::channel::<()>();
    let join = std::thread::Builder::new()
        .name("checkpointer".to_string())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(poll) {
                if !tick() {
                    break;
                }
            }
        })
        .expect("could not spawn the checkpointer thread");
    CheckpointerHandle { stop, join }
}

#[derive(Debug)]
pub struct CheckpointerHandle {
    stop: mpsc::Sender<()>,
    join: JoinHandle<()>,
}

impl CheckpointerHandle {
    /// Asks the thread to stop and waits for it (a checkpoint in progress
    /// finishes first). A thread that panicked is ignored.
    pub fn stop_and_join(self) {
        let _ = self.stop.send(());
        let _ = self.join.join();
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[test]
    fn checkpointer_ticks_and_stops() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let h = spawn_checkpointer(
            Duration::from_millis(10),
            Box::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
                true
            }),
        );
        let start = Instant::now();
        while n.load(Ordering::SeqCst) < 3 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(n.load(Ordering::SeqCst) >= 3);
        h.stop_and_join();
        let after = n.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(n.load(Ordering::SeqCst), after);
    }

    #[test]
    fn stop_does_not_wait_for_the_interval() {
        let h = spawn_checkpointer(Duration::from_secs(3600), Box::new(|| true));
        let start = Instant::now();
        h.stop_and_join();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn thread_ends_when_tick_returns_false() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let h = spawn_checkpointer(
            Duration::from_millis(5),
            Box::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
                false
            }),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        h.stop_and_join(); // must not hang
    }

    #[test]
    fn panicking_tick_does_not_break_stop() {
        let h = spawn_checkpointer(Duration::from_millis(5), Box::new(|| panic!("boom")));
        std::thread::sleep(Duration::from_millis(50));
        h.stop_and_join();
    }

    // ---- run() ----

    use crate::control::ControlData;
    use crate::debug_knobs::DebugKnobs;
    use crate::interrupt::InterruptFlag;
    use crate::storage::buffer::WalFlush;
    use crate::storage::smgr::{
        BufferTag, DEFAULTTABLESPACE_OID, ForkNumber, RelFileLocator, RelFileNumber,
    };
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs, Vfs};
    use crate::txn::clog::XidStatus;
    use crate::txn::{WaitCtl, Xid};
    use crate::wal::xlog::{XLOG_CHECKPOINT_ONLINE, XLOG_CHECKPOINT_REDO, XLOG_NOOP};
    use crate::wal::{DecodedRecord, MIN_WAL_SEGMENT_SIZE, RecordBuilder, RmgrId, WalConfig};
    use crate::wal::{WalReader, segment};
    use std::path::Path;

    const SEG: u32 = MIN_WAL_SEGMENT_SIZE;

    fn control_data() -> ControlData {
        ControlData {
            format_version: 2,
            catalog_version: 1,
            system_identifier: 1,
            state: DbState::InProduction,
            page_size: 8192,
            wal_segment_size: SEG,
            flags: 0,
            time: 0,
            checkpoint_lsn: 0,
            redo_lsn: 0,
            next_xid: 3,
            oldest_xid: 3,
            next_oid: 16384,
            timeline: 1,
            min_recovery_lsn: 0,
            rel_seg_blocks: 131_072,
            data_layout_version: 1,
            builtin_hash: 0,
        }
    }

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    fn records_from(vfs: &Arc<dyn Vfs>, start: Lsn) -> Vec<DecodedRecord> {
        let cfg = WalConfig {
            segment_size: SEG,
            system_identifier: 1,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        };
        let mut r = WalReader::open(Arc::clone(vfs), &cfg, start);
        let mut out = Vec::new();
        while let Some(rec) = r.next().unwrap() {
            out.push(rec);
        }
        out
    }

    struct Rig {
        sim: SimVfs,
        vfs: Arc<dyn Vfs>,
        pool: Arc<BufferPool>,
        smgr: Arc<StorageManager>,
        clog: Arc<Clog>,
        wal: Arc<Wal>,
        txn: Arc<TxnManager>,
        control: Arc<ControlFileHandle>,
        oids: OidAllocator,
        lock: Mutex<()>,
        irq: InterruptFlag,
    }

    impl Rig {
        fn new() -> Rig {
            let sim = SimVfs::new(11);
            for d in ["pg_xact", "global"] {
                sim.create_dir_all(Path::new(d)).unwrap();
            }
            let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
            let control = Arc::new(ControlFileHandle::create(&vfs, &control_data()).unwrap());
            let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), 131_072));
            let knobs = DebugKnobs::default();
            let wal = Wal::initialize(
                Arc::clone(&vfs),
                WalConfig {
                    segment_size: SEG,
                    system_identifier: 1,
                    full_page_writes: true,
                    knobs,
                },
            )
            .unwrap();
            vfs.sync_dir(Path::new("")).unwrap();
            let flush: Arc<dyn WalFlush> = wal.clone();
            let pool = BufferPool::new(16, Arc::clone(&smgr), flush, knobs);
            let clog = Arc::new(Clog::open(Arc::clone(&vfs), Xid(3)).unwrap());
            let txn = TxnManager::new(
                Arc::clone(&clog),
                Arc::clone(&control),
                Arc::clone(&wal),
                Xid(3),
                knobs,
            );
            let oids = OidAllocator::new(Arc::clone(&control));
            Rig {
                sim,
                vfs,
                pool,
                smgr,
                clog,
                wal,
                txn,
                control,
                oids,
                lock: Mutex::new(()),
                irq: InterruptFlag::default(),
            }
        }

        fn parts(&self) -> CheckpointParts<'_> {
            CheckpointParts {
                pool: &self.pool,
                smgr: &self.smgr,
                clog: &self.clog,
                wal: &self.wal,
                txn: &self.txn,
                control: &self.control,
                oids: &self.oids,
                checkpoint_lock: &self.lock,
            }
        }

        fn wc(&self) -> WaitCtl<'_> {
            WaitCtl {
                lock_timeout: None,
                interrupts: &self.irq,
            }
        }

        fn commit_one(&self) -> Xid {
            let (x, g) = self.txn.begin_write(1, &self.wc()).unwrap();
            self.txn.commit(x, &[]).unwrap();
            drop(g);
            x
        }

        fn noop(&self, len: usize) -> crate::wal::Inserted {
            let mut b = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid::INVALID);
            b.main_data(&vec![0u8; len]);
            self.wal.insert(b).unwrap()
        }

        /// Changes block 0 of `rel` through the pool, with a (dummy) WAL record.
        fn dirty_block(&self, rel: RelFileLocator, byte: u8) -> BufferTag {
            if !self.smgr.exists(rel, ForkNumber::Main).unwrap() {
                self.smgr.create(rel, ForkNumber::Main).unwrap();
                self.smgr.extend(rel, ForkNumber::Main).unwrap();
            }
            let tag = BufferTag {
                rel,
                fork: ForkNumber::Main,
                block: 0,
            };
            let buf = self.pool.read_buffer(tag).unwrap();
            let mut g = buf.write().unwrap();
            g.page_mut().0[100] = byte;
            let ins = self.noop(8);
            g.set_lsn(ins.end.0);
            tag
        }

        fn fail_syncs(&self, prefix: &str) {
            self.sim.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Sync,
                    path_prefix: Some(prefix.into()),
                    nth: None,
                    probability: None,
                    effect: FaultEffect::Error(std::io::ErrorKind::Other),
                }],
            });
        }
    }

    #[test]
    fn online_checkpoint_flushes_clog_and_records_limits() {
        let r = Rig::new();
        let x = r.commit_one();
        let res = run(&r.parts(), CheckpointKind::Periodic).unwrap();
        assert!(!res.skipped);
        let c = r.control.get();
        assert_eq!(c.next_xid, r.txn.xid_counters().1.0);
        assert_eq!(c.state, DbState::InProduction);
        assert_eq!(
            (c.checkpoint_lsn, c.redo_lsn),
            (res.checkpoint_lsn.0, res.redo.0)
        );
        assert!(res.redo < res.checkpoint_lsn);
        assert!(r.sim.exists(Path::new("pg_xact/000000000000")).unwrap());
        let reopened = Clog::open(Arc::clone(&r.vfs), Xid(4)).unwrap();
        assert_eq!(reopened.status(x).unwrap(), XidStatus::Committed);
        // WAL: CHECKPOINT_REDO at the REDO point, then the checkpoint record.
        let recs = records_from(&r.vfs, res.redo);
        assert_eq!(recs[0].start, res.redo);
        assert_eq!(
            (recs[0].rmgr, recs[0].info),
            (RmgrId::Xlog, XLOG_CHECKPOINT_REDO)
        );
        let last = recs.last().unwrap();
        assert_eq!(last.start, res.checkpoint_lsn);
        assert_eq!(last.info, XLOG_CHECKPOINT_ONLINE);
        let cp = CheckpointRecord::decode(last).unwrap();
        assert_eq!(cp.redo, res.redo);
        assert_eq!(cp.kind, CheckpointRecordKind::Online);
        assert_eq!(cp.next_xid, r.txn.next_xid());
        assert_eq!(cp.next_oid, r.oids.limit());
        // Durable.
        assert_eq!(r.wal.flushed_lsn(), r.wal.insert_lsn());
    }

    #[test]
    fn shutdown_checkpoint_writes_exact_counters_and_state() {
        let r = Rig::new();
        r.commit_one();
        let res = run(&r.parts(), CheckpointKind::Shutdown).unwrap();
        let c = r.control.get();
        assert_eq!(c.next_xid, r.txn.next_xid().0);
        assert_eq!(c.state, DbState::ShutDown);
        assert_eq!(c.next_oid, r.oids.limit());
        // The REDO point is the checkpoint record itself (nothing else runs).
        assert_eq!(res.redo, res.checkpoint_lsn);
        let recs = records_from(&r.vfs, res.redo);
        assert_eq!(recs.len(), 1);
        let cp = CheckpointRecord::decode(&recs[0]).unwrap();
        assert_eq!(cp.kind, CheckpointRecordKind::Shutdown);
        assert_eq!(cp.redo, res.redo);
    }

    #[test]
    fn end_of_recovery_checkpoint_moves_state_to_production() {
        let r = Rig::new();
        r.control
            .update(|c| c.state = DbState::InCrashRecovery)
            .unwrap();
        let res = run(&r.parts(), CheckpointKind::EndOfRecovery).unwrap();
        assert_eq!(r.control.get().state, DbState::InProduction);
        let recs = records_from(&r.vfs, res.redo);
        let cp = CheckpointRecord::decode(&recs[0]).unwrap();
        assert_eq!(cp.kind, CheckpointRecordKind::EndOfRecovery);
        assert_eq!(res.redo, res.checkpoint_lsn);
    }

    #[test]
    fn periodic_is_skipped_when_only_checkpoints_were_logged() {
        let r = Rig::new();
        // The control file points to no checkpoint yet: run.
        assert!(!run(&r.parts(), CheckpointKind::Periodic).unwrap().skipped);
        let before = r.control.get();
        let wal_end = r.wal.insert_lsn();
        let res = run(&r.parts(), CheckpointKind::Periodic).unwrap();
        assert!(res.skipped);
        assert_eq!(res.checkpoint_lsn.0, before.checkpoint_lsn);
        assert_eq!(r.control.get(), before);
        assert_eq!(r.wal.insert_lsn(), wal_end);
        // The other kinds always run.
        assert!(!run(&r.parts(), CheckpointKind::Explicit).unwrap().skipped);
        assert!(!run(&r.parts(), CheckpointKind::WalVolume).unwrap().skipped);
        assert!(run(&r.parts(), CheckpointKind::Periodic).unwrap().skipped);
        // Any other record ends the skipping.
        r.commit_one();
        assert!(!run(&r.parts(), CheckpointKind::Periodic).unwrap().skipped);
        assert!(run(&r.parts(), CheckpointKind::Periodic).unwrap().skipped);
        // A shutdown checkpoint counts as well.
        run(&r.parts(), CheckpointKind::Shutdown).unwrap();
        assert!(run(&r.parts(), CheckpointKind::Periodic).unwrap().skipped);
    }

    #[test]
    fn dirty_pages_are_written_with_wal_first() {
        let r = Rig::new();
        let tag = r.dirty_block(rel(16384), 0x5A);
        assert!(r.wal.flushed_lsn() < r.wal.insert_lsn());
        let res = run(&r.parts(), CheckpointKind::Explicit).unwrap();
        assert_eq!(res.buffers_written, 1);
        let mut page = [0u8; 8192];
        r.smgr.read_block(tag, &mut page).unwrap();
        assert_eq!(page[100], 0x5A);
        // The data page is on disk only after the WAL up to its LSN was.
        assert!(r.wal.flushed_lsn().0 >= u64::from_le_bytes(page[0..8].try_into().unwrap()));
        // Surviving a crash: the page content is durable.
        let crashed = r.sim.crash(CrashMode::DropUnsynced);
        let crashed: Arc<dyn Vfs> = Arc::new(crashed);
        let smgr2 = StorageManager::new(crashed, 131_072);
        let mut page2 = [0u8; 8192];
        smgr2.read_block(tag, &mut page2).unwrap();
        assert_eq!(page2[100], 0x5A);
    }

    #[test]
    fn old_wal_segments_are_removed() {
        let r = Rig::new();
        let first = segment::segment_path(1);
        for _ in 0..4500 {
            r.noop(1000);
        }
        assert!(r.sim.exists(&first).unwrap());
        let res = run(&r.parts(), CheckpointKind::Explicit).unwrap();
        let redo_seg = res.redo.segno(SEG);
        assert!(redo_seg >= 3);
        assert!(!r.sim.exists(&first).unwrap());
        assert!(r.sim.exists(&segment::segment_path(redo_seg)).unwrap());
        // The checkpoint record is still readable.
        let recs = records_from(&r.vfs, res.redo);
        assert_eq!(recs.last().unwrap().start, res.checkpoint_lsn);
    }

    #[test]
    fn checkpoint_amid_commits_covers_every_commit_before_the_redo_point() {
        let r = Arc::new(Rig::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..3u64)
            .map(|sid| {
                let (r, stop) = (Arc::clone(&r), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let irq = InterruptFlag::default();
                    let wc = WaitCtl {
                        lock_timeout: None,
                        interrupts: &irq,
                    };
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        let (x, g) = r.txn.begin_write(sid, &wc).unwrap();
                        r.txn.commit(x, &[]).unwrap();
                        drop(g);
                    }
                })
            })
            .collect();
        for _ in 0..8 {
            run(&r.parts(), CheckpointKind::Explicit).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for w in writers {
            w.join().unwrap();
        }
        // After a crash, every commit logged before the last REDO point is
        // COMMITTED in the commit log on disk (D10).
        let crashed: Arc<dyn Vfs> = Arc::new(r.sim.crash(CrashMode::DropUnsynced));
        let c = ControlFileHandle::open(&crashed).unwrap().get();
        let start = Lsn(u64::from(SEG) + 32);
        let on_disk = Clog::open(Arc::clone(&crashed), Xid(c.next_xid)).unwrap();
        let mut checked = 0;
        for rec in records_from(&crashed, start) {
            if rec.rmgr == RmgrId::Xact && rec.end.0 <= c.redo_lsn {
                assert_eq!(on_disk.status(rec.xid).unwrap(), XidStatus::Committed);
                checked += 1;
            }
        }
        assert!(checked > 0, "no commit preceded the REDO point");
    }

    #[test]
    fn due_picks_wal_volume_before_timeout() {
        let r = Rig::new();
        let long = Duration::from_secs(3600);
        let just_now = Instant::now();
        assert_eq!(due(just_now, long, &r.wal, 1 << 20), None);
        r.noop(2000);
        assert_eq!(
            due(just_now, long, &r.wal, 1000),
            Some(CheckpointKind::WalVolume)
        );
        assert_eq!(
            due(just_now, Duration::ZERO, &r.wal, 1 << 30),
            Some(CheckpointKind::Periodic)
        );
        assert_eq!(
            due(just_now, Duration::ZERO, &r.wal, 1000),
            Some(CheckpointKind::WalVolume)
        );
        // A checkpoint resets the volume.
        run(&r.parts(), CheckpointKind::Explicit).unwrap();
        assert_eq!(due(just_now, long, &r.wal, 1000), None);
    }

    #[test]
    fn failed_clog_sync_is_panic_and_leaves_control_alone() {
        let r = Rig::new();
        r.commit_one();
        let before = r.control.get();
        r.fail_syncs("pg_xact");
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
    }

    #[test]
    fn failed_wal_sync_is_panic_and_leaves_control_alone() {
        let r = Rig::new();
        r.commit_one();
        let before = r.control.get();
        r.fail_syncs("pg_wal");
        let e = run(&r.parts(), CheckpointKind::Explicit).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
        // The WAL is unusable afterwards, so no checkpoint is attempted.
        r.sim.set_faults(FaultPlan::default());
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
    }

    #[test]
    fn checkpoint_is_refused_after_a_failed_clog_flush() {
        let r = Rig::new();
        r.commit_one();
        r.fail_syncs("pg_xact");
        assert!(run(&r.parts(), CheckpointKind::Explicit).is_err());
        r.sim.set_faults(FaultPlan::default());
        let before = r.control.get();
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
        assert_ne!(r.control.get().state, DbState::ShutDown);
    }

    #[test]
    fn checkpoint_is_refused_after_a_failed_data_fsync() {
        let r = Rig::new();
        let tag = r.dirty_block(rel(16384), 1);
        // Leave the page in the file but not in the pool's dirty set.
        r.smgr.write_block(tag, &[1u8; 8192]).unwrap();
        r.fail_syncs("base");
        assert!(run(&r.parts(), CheckpointKind::Explicit).is_err());
        r.sim.set_faults(FaultPlan::default());
        assert!(r.smgr.is_broken() || r.pool.is_poisoned());
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_ne!(r.control.get().state, DbState::ShutDown);
    }
}
