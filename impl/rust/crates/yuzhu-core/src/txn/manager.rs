//! `TxnManager` (XID assignment, running list, snapshots, single-writer
//! lock with a FIFO queue, commit gate, storage barrier, commit / abort
//! WAL), `Transaction` and `WriterGuard` (`m2.md` §4.5, §6.7; `m3.md` §4.6,
//! §5.3, §6.6).
//!
//! Lock order (§5.9): writer ownership -> `checkpoint lock` -> storage
//! barrier -> commit gate (shared) -> `proc` (leaf) -> control file / `Clog`
//! internals. The `writer` mutex is only used for the queue and for changing
//! the owner, and is released before `proc` is taken.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::clog::{Clog, XidStatus};
use super::xact_wal::{XACT_ABORT, XACT_COMMIT, XactRecord};
use super::{CommandId, FIRST_COMMAND_ID, Snapshot, Xid};
use crate::control::ControlFileHandle;
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::interrupt::InterruptFlag;
use crate::storage::WriteCtx;
use crate::storage::XID_PREFETCH;
use crate::storage::smgr::RelFileLocator;
use crate::util::sync::{lock, lock_ignore_poison, read, wait_timeout, write};
use crate::wal::{Lsn, Wal};

/// How often a waiter for the writer lock checks for interrupts (§6.6.3).
const WAIT_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct ProcState {
    next_xid: Xid,
    /// XIDs below this are covered by the value recorded in the control file.
    xid_limit: Xid,
    /// Raw list of running XIDs.
    running: BTreeSet<Xid>,
}

/// Owner of the writer lock and the waiters in arrival order (§6.6.3).
#[derive(Debug, Default)]
struct WriterState {
    owner: Option<u64>,
    queue: VecDeque<u64>,
}

/// What a wait for the writer lock honors besides the lock itself.
#[derive(Debug, Clone, Copy)]
pub struct WaitCtl<'a> {
    /// `None` and a zero duration wait forever.
    pub lock_timeout: Option<Duration>,
    pub interrupts: &'a InterruptFlag,
}

#[derive(Debug)]
pub struct TxnManager {
    /// Leaf lock (apart from `Clog` internals and the control file mutex).
    proc: Mutex<ProcState>,
    writer: Mutex<WriterState>,
    writer_released: Condvar,
    barrier: RwLock<()>,
    /// Files of dropped / rolled-back relations that could not be removed yet
    /// because a statement was running (see `try_exclusive_barrier`).
    deferred_unlinks: Mutex<Vec<RelFileLocator>>,
    /// Shared by commit / abort from the WAL insert to the removal from the
    /// running list; exclusive while a checkpoint fixes its REDO point.
    commit_gate: RwLock<()>,
    clog: Arc<Clog>,
    control: Arc<ControlFileHandle>,
    wal: Arc<Wal>,
    knobs: DebugKnobs,
}

/// `proc` for the infallible accessors: a poisoned lock means another
/// thread panicked while updating the XID state, so we must not go on.
fn proc_or_panic(m: &Mutex<ProcState>) -> MutexGuard<'_, ProcState> {
    m.lock()
        .expect("transaction manager state is poisoned (another thread panicked)")
}

fn panic_err(e: Error) -> Error {
    e.with_severity(Severity::Panic)
}

#[allow(clippy::cast_possible_truncation)]
fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

impl TxnManager {
    /// `next_xid` comes from recovery (or the control file after a clean
    /// stop). Pre-allocation continues from the value recorded in the control
    /// file; the XIDs pre-allocated before the last stop are discarded
    /// (§6.7.3).
    pub fn new(
        clog: Arc<Clog>,
        control: Arc<ControlFileHandle>,
        wal: Arc<Wal>,
        next_xid: Xid,
        knobs: DebugKnobs,
    ) -> Arc<Self> {
        let next = Xid(next_xid.0.max(Xid::FIRST_NORMAL.0));
        let limit = Xid(control.get().next_xid.max(Xid::FIRST_NORMAL.0));
        Arc::new(TxnManager {
            proc: Mutex::new(ProcState {
                next_xid: next,
                xid_limit: limit,
                running: BTreeSet::new(),
            }),
            writer: Mutex::new(WriterState::default()),
            writer_released: Condvar::new(),
            barrier: RwLock::new(()),
            deferred_unlinks: Mutex::new(Vec::new()),
            commit_gate: RwLock::new(()),
            clog,
            control,
            wal,
            knobs,
        })
    }

    /// Takes the writer lock (FIFO), assigns an XID and adds it to the
    /// running list (§6.7.1, §6.6.3). While waiting, every 50ms (and at the
    /// lock timeout) it checks `wait.interrupts`; a timeout is `55P03
    /// canceling statement due to lock timeout`.
    pub fn begin_write(
        self: &Arc<Self>,
        session_id: u64,
        wait: &WaitCtl<'_>,
    ) -> Result<(Xid, WriterGuard)> {
        self.acquire_writer(session_id, wait)?;
        match self.assign_xid() {
            Ok(xid) => Ok((
                xid,
                WriterGuard {
                    mgr: Arc::clone(self),
                    session_id,
                    xid,
                },
            )),
            Err(e) => {
                self.release_writer(session_id);
                Err(e)
            }
        }
    }

    fn acquire_writer(&self, session_id: u64, wait: &WaitCtl<'_>) -> Result<()> {
        let started = Instant::now();
        let limit = wait.lock_timeout.filter(|d| !d.is_zero());
        let mut st = lock(&self.writer)?;
        if st.owner == Some(session_id) || st.queue.contains(&session_id) {
            return Err(Error::internal(
                "session already holds or awaits the writer lock (begin_write called twice)",
            ));
        }
        st.queue.push_back(session_id);
        loop {
            if st.owner.is_none() && st.queue.front() == Some(&session_id) {
                st.queue.pop_front();
                st.owner = Some(session_id);
                return Ok(());
            }
            let mut slice = WAIT_POLL;
            let failure = if let Err(e) = wait.interrupts.check() {
                Some(e)
            } else if let Some(t) = limit {
                let left = t.saturating_sub(started.elapsed());
                if left.is_zero() {
                    Some(lock_timeout())
                } else {
                    slice = slice.min(left);
                    None
                }
            } else {
                None
            };
            if let Some(e) = failure {
                st.queue.retain(|s| *s != session_id);
                // The next waiter may now be at the front of an idle lock.
                self.writer_released.notify_all();
                return Err(e);
            }
            st = wait_timeout(&self.writer_released, st, slice)?.0;
        }
    }

    /// Clears the owner (if it is `session_id`) and wakes the waiters. Safe
    /// in `Drop`.
    fn release_writer(&self, session_id: u64) {
        let mut st = lock_ignore_poison(&self.writer);
        if st.owner == Some(session_id) {
            st.owner = None;
            self.writer_released.notify_all();
        }
    }

    /// The session that owns the writer lock, if any.
    pub fn writer_owner(&self) -> Option<u64> {
        lock_ignore_poison(&self.writer).owner
    }

    /// Whether `session_id` is waiting for the writer lock and its owner is
    /// one of `among` (`pg_isolation_test_session_is_blocked`).
    pub fn is_blocked_by(&self, session_id: u64, among: &[u64]) -> bool {
        let st = lock_ignore_poison(&self.writer);
        st.queue.contains(&session_id) && st.owner.is_some_and(|o| among.contains(&o))
    }

    /// Allocates the next XID, pre-allocating through the control file when
    /// the limit is reached (§6.7.3).
    fn assign_xid(&self) -> Result<Xid> {
        let mut p = lock(&self.proc)?;
        if p.next_xid >= p.xid_limit {
            let new_limit = p.next_xid.0 + XID_PREFETCH;
            // proc -> control file is an allowed nesting (§5.9). A failure
            // is a Panic and leaves `xid_limit` unchanged.
            self.control
                .update(|c| c.next_xid = c.next_xid.max(new_limit))?;
            p.xid_limit = Xid(new_limit);
        }
        let xid = p.next_xid;
        self.clog.ensure_page_for(xid);
        p.next_xid = Xid(xid.0 + 1);
        p.running.insert(xid);
        Ok(xid)
    }

    pub fn snapshot(&self, own_xid: Option<Xid>, curcid: CommandId) -> Snapshot {
        let p = proc_or_panic(&self.proc);
        let xmin = p
            .running
            .first()
            .copied()
            .map_or(p.next_xid, |f| f.min(p.next_xid));
        Snapshot {
            xmin,
            xmax: p.next_xid,
            xip: p
                .running
                .iter()
                .copied()
                .filter(|x| Some(*x) != own_xid)
                .collect(),
            curcid,
            own_xid,
        }
    }

    /// Whether `xid` is in the raw running list (`TransactionIdIsInProgress`).
    pub fn is_in_progress(&self, xid: Xid) -> bool {
        proc_or_panic(&self.proc).running.contains(&xid)
    }

    /// Commit steps 1 of §5.3: COMMIT record (`dropped` = `pending_unlinks`),
    /// WAL flush (unless `skip_commit_flush`), then the clog and the running
    /// list inside the `proc` mutex. Every failure is `Severity::Panic`; the
    /// running list is not touched in that case.
    pub fn commit(&self, xid: Xid, dropped: &[RelFileLocator]) -> Result<()> {
        self.finish(xid, XidStatus::Committed, dropped)
    }

    /// Abort step 1 of §5.3: ABORT record (`created` = `pending_creates`).
    /// The WAL is flushed only if `created` is not empty (the files are
    /// removed afterwards, so the record must not be lost).
    pub fn abort(&self, xid: Xid, created: &[RelFileLocator]) -> Result<()> {
        self.finish(xid, XidStatus::Aborted, created)
    }

    fn finish(&self, xid: Xid, status: XidStatus, rels: &[RelFileLocator]) -> Result<()> {
        if !self.is_in_progress(xid) {
            return Err(Error::internal(format!(
                "transaction {} is not running, cannot mark it {status:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        let committing = status == XidStatus::Committed;
        let info = if committing { XACT_COMMIT } else { XACT_ABORT };
        // Held until the end: the checkpoint's REDO point must not fall
        // between the record and the clog update (D10).
        let _gate = read(&self.commit_gate).map_err(panic_err)?;
        let record = XactRecord {
            time_us: now_us(),
            rels: rels.to_vec(),
        };
        let ins = self
            .wal
            .insert(record.builder(info, xid))
            .map_err(panic_err)?;
        let flush = if committing {
            !self.knobs.skip_commit_flush
        } else {
            !rels.is_empty()
        };
        if flush {
            self.wal.flush(ins.end).map_err(panic_err)?;
        }
        let mut p = lock(&self.proc)?;
        if !p.running.contains(&xid) {
            return Err(Error::internal(format!(
                "transaction {} is not running, cannot mark it {status:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        self.clog.set_status(xid, status).map_err(panic_err)?;
        p.running.remove(&xid);
        Ok(())
    }

    /// XID を持たないトランザクションの COMMIT（`m4/08` §4.6）。`flush_upto` が `Lsn(0)` でなければ
    /// `wal.flush(flush_upto)`。コミットゲートは取らない（clog を触らない）。失敗は `Severity::Panic`。
    /// `DebugKnobs::skip_commit_flush` のときは何もしない（変異試験）。
    pub fn finish_without_xid(&self, flush_upto: Lsn) -> Result<()> {
        if flush_upto == Lsn::INVALID || self.knobs.skip_commit_flush {
            return Ok(());
        }
        self.wal.flush(flush_upto).map_err(panic_err)
    }

    /// Held by the checkpoint only while it fixes its REDO point (D10). It
    /// waits for the commits and aborts in progress and blocks new ones.
    pub fn commit_gate_exclusive(&self) -> Result<GateWrite<'_>> {
        Ok(GateWrite {
            _guard: write(&self.commit_gate).map_err(panic_err)?,
        })
    }

    /// Shared guard held while a statement runs. Never store it in a struct.
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>> {
        Ok(BarrierRead {
            _guard: read(&self.barrier)?,
        })
    }

    /// Exclusive guard for DROP cleanup. The caller must not hold a shared
    /// barrier. It polls with `try_write` instead of queueing: std's `RwLock`
    /// prefers a queued writer, which would block every new statement
    /// (even `SELECT 1`) behind a long-running one.
    pub fn exclusive_barrier(&self) -> Result<BarrierWrite<'_>> {
        let mut pause = Duration::from_micros(50);
        loop {
            match self.barrier.try_write() {
                Ok(g) => return Ok(BarrierWrite { _guard: g }),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(Error::new(sqlstate::INTERNAL_ERROR, "lock poisoned")
                        .with_severity(Severity::Panic));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(5));
                }
            }
        }
    }

    /// Non-blocking variant of `exclusive_barrier`: `None` while any
    /// statement runs. DROP cleanup must not wait for other sessions' (possibly
    /// very long) statements, so the files are queued with `defer_unlinks`.
    pub fn try_exclusive_barrier(&self) -> Result<Option<BarrierWrite<'_>>> {
        match self.barrier.try_write() {
            Ok(g) => Ok(Some(BarrierWrite { _guard: g })),
            Err(std::sync::TryLockError::WouldBlock) => Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err(Error::new(sqlstate::INTERNAL_ERROR, "lock poisoned")
                    .with_severity(Severity::Panic))
            }
        }
    }

    /// Queues relation files for removal under the exclusive barrier.
    pub fn defer_unlinks(&self, rels: &[RelFileLocator]) {
        lock_ignore_poison(&self.deferred_unlinks).extend_from_slice(rels);
    }

    /// Takes the queued files. The caller must hold the exclusive barrier.
    pub fn take_deferred_unlinks(&self) -> Vec<RelFileLocator> {
        std::mem::take(&mut *lock_ignore_poison(&self.deferred_unlinks))
    }

    pub fn has_deferred_unlinks(&self) -> bool {
        !lock_ignore_poison(&self.deferred_unlinks).is_empty()
    }

    pub fn clog(&self) -> &Arc<Clog> {
        &self.clog
    }

    pub fn wal(&self) -> &Arc<Wal> {
        &self.wal
    }

    pub fn next_xid(&self) -> Xid {
        proc_or_panic(&self.proc).next_xid
    }

    /// `(next_xid, xid_limit)` read inside the `proc` mutex (checkpoint).
    pub fn xid_counters(&self) -> (Xid, Xid) {
        let p = proc_or_panic(&self.proc);
        (p.next_xid, p.xid_limit)
    }
}

fn lock_timeout() -> Error {
    Error::new(
        sqlstate::LOCK_NOT_AVAILABLE,
        "canceling statement due to lock timeout",
    )
}

/// Thin wrapper of `RwLockReadGuard` (re-entry is detected in
/// `storage/buffer/track.rs`).
#[derive(Debug)]
pub struct BarrierRead<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

#[derive(Debug)]
pub struct BarrierWrite<'a> {
    _guard: RwLockWriteGuard<'a, ()>,
}

/// The commit gate held exclusively (see `commit_gate_exclusive`).
#[derive(Debug)]
pub struct GateWrite<'a> {
    _guard: RwLockWriteGuard<'a, ()>,
}

/// Token for owning the writer lock (not the lock guard itself). As a safety
/// net on drop, an unfinished transaction is aborted in memory (clog
/// ABORTED, removed from `running`; no I/O, no WAL) and the writer lock is
/// released. This is the same as the implicit abort of a crash. Files in
/// `pending_creates` cannot be removed on drop (no I/O in `Drop`): a WARNING
/// is logged and they stay as orphans.
///
/// "Unfinished" means the XID is still in the running list, so `commit` /
/// `abort` need no handshake with the guard.
#[derive(Debug)]
pub struct WriterGuard {
    mgr: Arc<TxnManager>,
    session_id: u64,
    xid: Xid,
}

impl WriterGuard {
    pub fn xid(&self) -> Xid {
        self.xid
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        let unfinished = {
            let mut p = lock_ignore_poison(&self.mgr.proc);
            if p.running.remove(&self.xid) {
                // Memory only; ignore a failure (nothing more can be done).
                let _ = self.mgr.clog.set_status(self.xid, XidStatus::Aborted);
                true
            } else {
                false
            }
        };
        if unfinished {
            warn(&format!(
                "transaction {} was dropped without commit or abort; aborted in memory (files created by it may remain as orphans)",
                self.xid.0
            ));
        }
        self.mgr.release_writer(self.session_id);
    }
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

#[derive(Debug, Default)]
pub struct Transaction {
    /// Assigned when the writer lock is taken.
    pub xid: Option<Xid>,
    pub cid: CommandId,
    /// Whether the current statement wrote something.
    pub cid_used: bool,
    /// Single-writer lock; the drop is a safety net.
    pub writer: Option<WriterGuard>,
    /// Files to remove on abort.
    pub pending_creates: Vec<RelFileLocator>,
    /// Files to remove on commit.
    pub pending_unlinks: Vec<RelFileLocator>,
    /// The transaction changed the catalog.
    pub catalog_dirty: bool,
    /// 非トランザクション的に書いた WAL（`SEQ_LOG`）のうち、このトランザクションが払い出した値を覆うものの
    /// 終端 LSN の最大値。コミット時にここまで flush する。`Lsn(0)` = なし（`m4/08` §4.6）。
    pub wal_flush_upto: Lsn,
    /// トランザクションの開始時刻（2000-01-01 からのマイクロ秒。PostgreSQL のエポック）。
    /// `Transaction::new()` は 0。session が開始時に設定する。
    pub started_at: i64,
}

impl Transaction {
    pub fn new() -> Self {
        Transaction {
            cid: FIRST_COMMAND_ID,
            ..Transaction::default()
        }
    }

    /// `wal_flush_upto` を大きい方に更新する。
    pub fn note_wal(&mut self, lsn: Lsn) {
        if lsn > self.wal_flush_upto {
            self.wal_flush_upto = lsn;
        }
    }

    /// The only entry point for writes. An internal error if no XID was
    /// assigned (the writer lock was not taken). Sets `cid_used`.
    pub fn write_ctx(&mut self) -> Result<WriteCtx> {
        let xid = self.xid.ok_or_else(|| {
            Error::internal("write without a transaction ID (writer lock not taken)")
        })?;
        self.cid_used = true;
        Ok(WriteCtx { xid, cid: self.cid })
    }

    /// Called when a statement succeeds: advances `cid` if the statement
    /// wrote (`CommandCounterIncrement`). Beyond 2^32-2 commands: `54000`.
    pub fn command_counter_increment(&mut self) -> Result<()> {
        if !self.cid_used {
            return Ok(());
        }
        if self.cid >= u32::MAX - 1 {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cannot have more than 2^32-2 commands in a transaction",
            ));
        }
        self.cid += 1;
        self.cid_used = false;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use crate::wal::{Lsn, MIN_WAL_SEGMENT_SIZE, WalConfig, WalReader};

    #[test]
    fn write_ctx_requires_an_xid() {
        let mut t = Transaction::new();
        let e = t.write_ctx().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(!t.cid_used);

        t.xid = Some(Xid(7));
        let w = t.write_ctx().unwrap();
        assert_eq!((w.xid, w.cid), (Xid(7), 0));
        assert!(t.cid_used);
    }

    #[test]
    fn command_counter_advances_only_after_a_write() {
        let mut t = Transaction::new();
        t.xid = Some(Xid(7));
        t.command_counter_increment().unwrap();
        assert_eq!(t.cid, 0);
        t.write_ctx().unwrap();
        t.command_counter_increment().unwrap();
        assert_eq!(t.cid, 1);
        assert!(!t.cid_used);
    }

    #[test]
    fn command_counter_limit_is_54000() {
        let mut t = Transaction::new();
        t.xid = Some(Xid(7));
        t.cid = u32::MAX - 1;
        t.write_ctx().unwrap();
        let e = t.command_counter_increment().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(t.cid, u32::MAX - 1);
    }

    // ---- TxnManager ----

    use crate::control::{ControlData, DbState};
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs, Vfs};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub(crate) fn control_data(next_xid: u64) -> ControlData {
        ControlData {
            format_version: 2,
            catalog_version: 1,
            system_identifier: 1,
            state: DbState::InProduction,
            page_size: 8192,
            wal_segment_size: MIN_WAL_SEGMENT_SIZE,
            flags: 0,
            time: 0,
            checkpoint_lsn: 0,
            redo_lsn: 0,
            next_xid,
            oldest_xid: 3,
            next_oid: 16384,
            timeline: 1,
            min_recovery_lsn: 0,
            rel_seg_blocks: 131_072,
            data_layout_version: 1,
            builtin_hash: 0,
        }
    }

    fn wal_cfg(knobs: DebugKnobs) -> WalConfig {
        WalConfig {
            segment_size: MIN_WAL_SEGMENT_SIZE,
            system_identifier: 1,
            full_page_writes: true,
            knobs,
        }
    }

    struct Rig {
        sim: SimVfs,
        vfs: Arc<dyn Vfs>,
        control: Arc<ControlFileHandle>,
        wal: Arc<Wal>,
        mgr: Arc<TxnManager>,
        irq: InterruptFlag,
    }

    impl Rig {
        fn new() -> Rig {
            Rig::with_knobs(DebugKnobs::default())
        }

        fn with_knobs(knobs: DebugKnobs) -> Rig {
            let sim = SimVfs::new(3);
            sim.create_dir_all(Path::new("pg_xact")).unwrap();
            sim.create_dir_all(Path::new("global")).unwrap();
            let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
            let control = Arc::new(ControlFileHandle::create(&vfs, &control_data(3)).unwrap());
            let clog = Arc::new(Clog::open(Arc::clone(&vfs), Xid(3)).unwrap());
            let wal = Wal::initialize(Arc::clone(&vfs), wal_cfg(knobs)).unwrap();
            vfs.sync_dir(Path::new("")).unwrap();
            let mgr = TxnManager::new(clog, Arc::clone(&control), Arc::clone(&wal), Xid(3), knobs);
            Rig {
                sim,
                vfs,
                control,
                wal,
                mgr,
                irq: InterruptFlag::default(),
            }
        }

        fn wc(&self) -> WaitCtl<'_> {
            WaitCtl {
                lock_timeout: None,
                interrupts: &self.irq,
            }
        }

        fn begin(&self, session: u64) -> (Xid, WriterGuard) {
            self.mgr.begin_write(session, &self.wc()).unwrap()
        }

        /// The records readable from the start of the WAL on `vfs`.
        fn records(vfs: &Arc<dyn Vfs>) -> Vec<(u8, u8, Xid, XactRecord)> {
            let cfg = wal_cfg(DebugKnobs::default());
            let start = Lsn(u64::from(MIN_WAL_SEGMENT_SIZE) + 32);
            let mut r = WalReader::open(Arc::clone(vfs), &cfg, start);
            let mut out = Vec::new();
            while let Some(rec) = r.next().unwrap() {
                out.push((
                    rec.rmgr as u8,
                    rec.info,
                    rec.xid,
                    XactRecord::decode(&rec.main).unwrap(),
                ));
            }
            out
        }
    }

    fn rel(n: u32) -> RelFileLocator {
        use crate::storage::smgr::RelFileNumber;
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    #[test]
    fn xids_are_assigned_in_order_and_run() {
        let r = Rig::new();
        let m = &r.mgr;
        let (x1, g1) = r.begin(1);
        assert_eq!(x1, Xid(3));
        assert!(m.is_in_progress(x1));
        m.commit(x1, &[]).unwrap();
        assert!(!m.is_in_progress(x1));
        drop(g1);
        let (x2, g2) = r.begin(2);
        assert_eq!(x2, Xid(4));
        m.abort(x2, &[]).unwrap();
        drop(g2);
        assert_eq!(m.clog().status(x1).unwrap(), XidStatus::Committed);
        assert_eq!(m.clog().status(x2).unwrap(), XidStatus::Aborted);
        assert_eq!(m.next_xid(), Xid(5));
    }

    #[test]
    fn xid_prefetch_is_written_to_control_file() {
        let r = Rig::new();
        let m = &r.mgr;
        let (x, g) = r.begin(1);
        assert_eq!(m.xid_counters(), (Xid(4), Xid(3 + XID_PREFETCH)));
        assert_eq!(r.control.get().next_xid, 3 + XID_PREFETCH);
        m.commit(x, &[]).unwrap();
        drop(g);
        // Crossing the limit extends it again.
        for i in 0..XID_PREFETCH {
            let (x, g) = r.begin(1);
            m.commit(x, &[]).unwrap();
            drop(g);
            assert_eq!(x.0, 4 + i);
        }
        let (next, limit) = m.xid_counters();
        assert_eq!(next, Xid(4 + XID_PREFETCH));
        assert_eq!(limit, Xid(3 + 2 * XID_PREFETCH));
        assert_eq!(r.control.get().next_xid, 3 + 2 * XID_PREFETCH);
    }

    #[test]
    fn restart_discards_prefetched_xids() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        let clog = Arc::new(Clog::open(Arc::clone(&r.vfs), Xid(r.control.get().next_xid)).unwrap());
        let m2 = TxnManager::new(
            clog,
            Arc::clone(&r.control),
            Arc::clone(&r.wal),
            Xid(r.control.get().next_xid),
            DebugKnobs::default(),
        );
        assert_eq!(m2.next_xid(), Xid(3 + XID_PREFETCH));
    }

    #[test]
    fn recovered_next_xid_beyond_the_control_file_extends_the_limit() {
        let r = Rig::new();
        let clog = Arc::new(Clog::open(Arc::clone(&r.vfs), Xid(3)).unwrap());
        let ahead = Xid(r.control.get().next_xid + 10);
        let m2 = TxnManager::new(
            clog,
            Arc::clone(&r.control),
            Arc::clone(&r.wal),
            ahead,
            DebugKnobs::default(),
        );
        let (x, g) = m2.begin_write(1, &r.wc()).unwrap();
        assert_eq!(x, ahead);
        assert_eq!(m2.xid_counters().1, Xid(ahead.0 + XID_PREFETCH));
        assert_eq!(r.control.get().next_xid, ahead.0 + XID_PREFETCH);
        m2.abort(x, &[]).unwrap();
        drop(g);
    }

    #[test]
    fn snapshot_contents() {
        let r = Rig::new();
        let m = &r.mgr;
        let s0 = m.snapshot(None, 0);
        assert_eq!((s0.xmin, s0.xmax), (Xid(3), Xid(3)));
        assert!(s0.xip.is_empty());

        let (x, g) = r.begin(1);
        let other = m.snapshot(None, 0);
        assert_eq!(other.xmin, x);
        assert_eq!(other.xmax, Xid(x.0 + 1));
        assert_eq!(other.xip, vec![x]);
        let own = m.snapshot(Some(x), 5);
        assert!(own.xip.is_empty());
        assert_eq!(own.xmin, x);
        assert_eq!((own.curcid, own.own_xid), (5, Some(x)));
        m.commit(x, &[]).unwrap();
        drop(g);
        let after = m.snapshot(None, 0);
        assert_eq!((after.xmin, after.xmax), (Xid(4), Xid(4)));
        assert!(after.xip.is_empty());
    }

    #[test]
    fn writer_lock_blocks_then_hands_over() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let m2 = Arc::clone(&r.mgr);
        let got = Arc::new(AtomicBool::new(false));
        let got2 = Arc::clone(&got);
        let t = std::thread::spawn(move || {
            let irq = InterruptFlag::default();
            let (x, g) = m2
                .begin_write(
                    2,
                    &WaitCtl {
                        lock_timeout: None,
                        interrupts: &irq,
                    },
                )
                .unwrap();
            got2.store(true, Ordering::SeqCst);
            m2.commit(x, &[]).unwrap();
            drop(g);
            x
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!got.load(Ordering::SeqCst));
        assert!(r.mgr.is_blocked_by(2, &[1]));
        assert!(!r.mgr.is_blocked_by(2, &[3]));
        assert!(!r.mgr.is_blocked_by(1, &[1]));
        assert_eq!(r.mgr.writer_owner(), Some(1));
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
        let x2 = t.join().unwrap();
        assert!(got.load(Ordering::SeqCst));
        assert!(x2 > x1);
        assert_eq!(r.mgr.writer_owner(), None);
        assert!(!r.mgr.is_blocked_by(2, &[1]));
    }

    #[test]
    fn waiters_get_the_lock_in_arrival_order() {
        let r = Rig::new();
        let (x0, g0) = r.begin(100);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for sid in 1..=4u64 {
            let m = Arc::clone(&r.mgr);
            let order = Arc::clone(&order);
            handles.push(std::thread::spawn(move || {
                let irq = InterruptFlag::default();
                let (x, g) = m
                    .begin_write(
                        sid,
                        &WaitCtl {
                            lock_timeout: None,
                            interrupts: &irq,
                        },
                    )
                    .unwrap();
                order.lock().unwrap().push(sid);
                m.commit(x, &[]).unwrap();
                drop(g);
            }));
            // Make sure this waiter is queued before the next one starts.
            while !r.mgr.is_blocked_by(sid, &[100]) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        r.mgr.commit(x0, &[]).unwrap();
        drop(g0);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn writer_lock_timeout_is_55p03() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let start = Instant::now();
        let wc = WaitCtl {
            lock_timeout: Some(Duration::from_millis(50)),
            interrupts: &r.irq,
        };
        let e = r.mgr.begin_write(2, &wc).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
        assert_eq!(e.message, "canceling statement due to lock timeout");
        assert!(start.elapsed() >= Duration::from_millis(50));
        // The loser consumed no XID, left the queue and did not disturb the owner.
        assert_eq!(r.mgr.next_xid(), Xid(x1.0 + 1));
        assert!(!r.mgr.is_blocked_by(2, &[1]));
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
        r.mgr.begin_write(2, &wc).unwrap();
    }

    #[test]
    fn zero_lock_timeout_waits_forever() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let m2 = Arc::clone(&r.mgr);
        let t = std::thread::spawn(move || {
            let irq = InterruptFlag::default();
            let wc = WaitCtl {
                lock_timeout: Some(Duration::ZERO),
                interrupts: &irq,
            };
            let (x, g) = m2.begin_write(2, &wc).unwrap();
            m2.abort(x, &[]).unwrap();
            drop(g);
        });
        std::thread::sleep(Duration::from_millis(150));
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
        t.join().unwrap();
    }

    #[test]
    fn cancel_while_waiting_leaves_the_queue() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let m2 = Arc::clone(&r.mgr);
        let irq2 = Arc::new(InterruptFlag::default());
        let irq2c = Arc::clone(&irq2);
        let t = std::thread::spawn(move || {
            m2.begin_write(
                2,
                &WaitCtl {
                    lock_timeout: None,
                    interrupts: &irq2c,
                },
            )
            .map(|_| ())
        });
        while !r.mgr.is_blocked_by(2, &[1]) {
            std::thread::sleep(Duration::from_millis(2));
        }
        irq2.request_cancel();
        let e = t.join().unwrap().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(!r.mgr.is_blocked_by(2, &[1]));
        // A later session still gets the lock.
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
        let (x, _g) = r.mgr.begin_write(3, &r.wc()).unwrap();
        assert!(x > x1);
    }

    #[test]
    fn cancelled_front_waiter_does_not_block_the_next_one() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let irq2 = Arc::new(InterruptFlag::default());
        let m2 = Arc::clone(&r.mgr);
        let irq2c = Arc::clone(&irq2);
        let t2 = std::thread::spawn(move || {
            m2.begin_write(
                2,
                &WaitCtl {
                    lock_timeout: None,
                    interrupts: &irq2c,
                },
            )
            .map(|_| ())
        });
        while !r.mgr.is_blocked_by(2, &[1]) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let m3 = Arc::clone(&r.mgr);
        let t3 = std::thread::spawn(move || {
            let irq = InterruptFlag::default();
            let (x, g) = m3
                .begin_write(
                    3,
                    &WaitCtl {
                        lock_timeout: None,
                        interrupts: &irq,
                    },
                )
                .unwrap();
            m3.abort(x, &[]).unwrap();
            drop(g);
        });
        while !r.mgr.is_blocked_by(3, &[1]) {
            std::thread::sleep(Duration::from_millis(2));
        }
        // The owner finishes first; session 2 is cancelled in the same moment.
        irq2.request_cancel();
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
        // Either 2 got the lock before noticing (then it ends with an XID it
        // drops) or it was cancelled; 3 must get the lock in both cases.
        let _ = t2.join().unwrap();
        t3.join().unwrap();
    }

    #[test]
    fn statement_deadline_and_terminate_end_the_wait() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let irq = InterruptFlag::default();
        irq.set_statement_deadline(Some(Instant::now() + Duration::from_millis(30)));
        let e = r
            .mgr
            .begin_write(
                2,
                &WaitCtl {
                    lock_timeout: None,
                    interrupts: &irq,
                },
            )
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(e.message.contains("statement timeout"));
        irq.request_terminate();
        let e = r
            .mgr
            .begin_write(
                2,
                &WaitCtl {
                    lock_timeout: None,
                    interrupts: &irq,
                },
            )
            .unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        r.mgr.commit(x1, &[]).unwrap();
        drop(g1);
    }

    #[test]
    fn double_begin_by_the_same_session_is_an_error() {
        let r = Rig::new();
        let (_x, _g) = r.begin(1);
        let e = r.mgr.begin_write(1, &r.wc()).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn dropping_a_writer_guard_aborts_in_memory_without_wal() {
        let r = Rig::new();
        let before = r.wal.insert_lsn();
        let (x, g) = r.begin(1);
        drop(g);
        assert!(!r.mgr.is_in_progress(x));
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Aborted);
        assert_eq!(r.wal.insert_lsn(), before);
        // The writer lock was released.
        let (x2, g2) = r.begin(2);
        assert!(x2 > x);
        r.mgr.commit(x2, &[]).unwrap();
        drop(g2);
        assert_eq!(r.mgr.clog().status(x2).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn dropping_a_finished_guard_keeps_the_status() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn commit_of_unknown_xid_is_panic_error() {
        let r = Rig::new();
        let before = r.wal.insert_lsn();
        let e = r.mgr.commit(Xid(99), &[]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        // No record was logged for the unknown XID.
        assert_eq!(r.wal.insert_lsn(), before);
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        assert_eq!(r.mgr.abort(x, &[]).unwrap_err().severity, Severity::Panic);
        drop(g);
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn commit_and_abort_write_records_and_flush() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        r.mgr.commit(x1, &[rel(16384), rel(16385)]).unwrap();
        drop(g1);
        let (x2, g2) = r.begin(1);
        r.mgr.abort(x2, &[rel(20000)]).unwrap();
        drop(g2);
        assert_eq!(r.wal.flushed_lsn(), r.wal.insert_lsn());
        // Only what was flushed survives a crash.
        let crashed: Arc<dyn Vfs> = Arc::new(r.sim.crash(CrashMode::DropUnsynced));
        let recs = Rig::records(&crashed);
        assert_eq!(recs.len(), 2);
        assert_eq!((recs[0].0, recs[0].1, recs[0].2), (1, XACT_COMMIT, x1));
        assert_eq!(recs[0].3.rels, vec![rel(16384), rel(16385)]);
        assert!(recs[0].3.time_us > 0);
        assert_eq!((recs[1].1, recs[1].2), (XACT_ABORT, x2));
        assert_eq!(recs[1].3.rels, vec![rel(20000)]);
    }

    #[test]
    fn abort_without_created_files_does_not_flush() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        let flushed = r.wal.flushed_lsn();
        r.mgr.abort(x, &[]).unwrap();
        drop(g);
        assert_eq!(r.wal.flushed_lsn(), flushed);
        assert!(r.wal.insert_lsn() > flushed);
        let (x, g) = r.begin(1);
        r.mgr.abort(x, &[rel(1)]).unwrap();
        drop(g);
        assert_eq!(r.wal.flushed_lsn(), r.wal.insert_lsn());
    }

    #[test]
    fn skip_commit_flush_knob_leaves_the_record_unflushed() {
        let r = Rig::with_knobs(DebugKnobs {
            skip_commit_flush: true,
            ..DebugKnobs::default()
        });
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        assert!(r.wal.flushed_lsn() < r.wal.insert_lsn());
        let crashed: Arc<dyn Vfs> = Arc::new(r.sim.crash(CrashMode::DropUnsynced));
        assert!(Rig::records(&crashed).is_empty());
    }

    #[test]
    fn failed_wal_flush_is_panic_and_the_commit_stays_invisible() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_wal".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = r.mgr.commit(x, &[]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        // The order is WAL flush -> clog -> running list: nothing happened yet.
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::InProgress);
        assert!(r.mgr.is_in_progress(x));
        r.sim.set_faults(FaultPlan::default());
        drop(g);
    }

    #[test]
    fn crash_before_the_flush_loses_the_commit_and_after_keeps_it() {
        let r = Rig::new();
        // Crash freezes the disk at the first WAL sync: the record is not durable.
        let (x, g) = r.begin(1);
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_wal".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::CrashFreeze,
            }],
        });
        assert!(r.mgr.commit(x, &[]).is_err());
        let crashed: Arc<dyn Vfs> = Arc::new(r.sim.crash(CrashMode::DropUnsynced));
        assert!(Rig::records(&crashed).is_empty());
        drop(g);

        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        let crashed: Arc<dyn Vfs> = Arc::new(r.sim.crash(CrashMode::DropUnsynced));
        assert_eq!(Rig::records(&crashed).len(), 1);
    }

    #[test]
    fn checkpoint_gate_waits_for_commits_and_blocks_new_ones() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        let gate = r.mgr.commit_gate_exclusive().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let (m2, done2) = (Arc::clone(&r.mgr), Arc::clone(&done));
        let t = std::thread::spawn(move || {
            m2.commit(x, &[]).unwrap();
            done2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!done.load(Ordering::SeqCst));
        // Nothing was logged while the gate is held.
        assert_eq!(r.wal.flushed_lsn(), r.wal.insert_lsn());
        drop(gate);
        t.join().unwrap();
        drop(g);
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn exclusive_gate_waits_for_a_commit_in_progress() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        // Slow down the WAL sync so that the commit is inside the gate.
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_wal".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Delay(Duration::from_millis(300)),
            }],
        });
        let m2 = Arc::clone(&r.mgr);
        let t = std::thread::spawn(move || m2.commit(x, &[]).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        let gate = r.mgr.commit_gate_exclusive().unwrap();
        // The commit completed fully (clog and running list) before the gate was ours.
        assert!(!r.mgr.is_in_progress(x));
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Committed);
        drop(gate);
        t.join().unwrap();
        drop(g);
    }

    #[test]
    fn concurrent_commits_all_finish() {
        let r = Rig::new();
        let n = Arc::new(AtomicUsize::new(0));
        let hs: Vec<_> = (0..4u64)
            .map(|sid| {
                let m = Arc::clone(&r.mgr);
                let n = Arc::clone(&n);
                std::thread::spawn(move || {
                    let irq = InterruptFlag::default();
                    for _ in 0..20 {
                        let (x, g) = m
                            .begin_write(
                                sid,
                                &WaitCtl {
                                    lock_timeout: None,
                                    interrupts: &irq,
                                },
                            )
                            .unwrap();
                        m.commit(x, &[]).unwrap();
                        drop(g);
                        n.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(n.load(Ordering::SeqCst), 80);
        assert_eq!(Rig::records(&r.vfs).len(), 80);
    }

    #[test]
    fn barrier_shared_and_exclusive() {
        let r = Rig::new();
        let m = &r.mgr;
        {
            let _a = m.statement_barrier().unwrap();
            // Another thread can share it.
            let m2 = Arc::clone(m);
            std::thread::spawn(move || {
                let _b = m2.statement_barrier().unwrap();
            })
            .join()
            .unwrap();
        }
        let _x = m.exclusive_barrier().unwrap();
    }

    #[test]
    fn pending_exclusive_barrier_does_not_block_new_statements() {
        let r = Rig::new();
        let shared = r.mgr.statement_barrier().unwrap();
        let m2 = Arc::clone(&r.mgr);
        let t = std::thread::spawn(move || {
            let _g = m2.exclusive_barrier().unwrap();
        });
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        drop(r.mgr.statement_barrier().unwrap());
        assert!(started.elapsed() < Duration::from_millis(50));
        drop(shared);
        t.join().unwrap();
    }

    #[test]
    fn try_exclusive_barrier_does_not_wait() {
        let r = Rig::new();
        let shared = r.mgr.statement_barrier().unwrap();
        assert!(r.mgr.try_exclusive_barrier().unwrap().is_none());
        drop(shared);
        assert!(r.mgr.try_exclusive_barrier().unwrap().is_some());
        assert!(!r.mgr.has_deferred_unlinks());
    }

    #[test]
    fn exclusive_barrier_waits_for_statements() {
        let r = Rig::new();
        let shared = r.mgr.statement_barrier().unwrap();
        let m2 = Arc::clone(&r.mgr);
        let done = Arc::new(AtomicBool::new(false));
        let done2 = Arc::clone(&done);
        let t = std::thread::spawn(move || {
            let _g = m2.exclusive_barrier().unwrap();
            done2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!done.load(Ordering::SeqCst));
        drop(shared);
        t.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn control_failure_during_prefetch_releases_the_writer() {
        let r = Rig::new();
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("global".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = r.mgr.begin_write(1, &r.wc()).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.mgr.xid_counters(), (Xid(3), Xid(3)));
        assert_eq!(r.mgr.writer_owner(), None);
        r.sim.set_faults(FaultPlan::default());
        let wc = WaitCtl {
            lock_timeout: Some(Duration::from_millis(10)),
            interrupts: &r.irq,
        };
        let (x, _g) = r.mgr.begin_write(2, &wc).unwrap();
        assert_eq!(x, Xid(3));
    }
}
