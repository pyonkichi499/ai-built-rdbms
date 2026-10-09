//! `TxnManager` (XID assignment, snapshots, commit / abort WAL, commit gate,
//! storage barrier), `XidGuard` and `Transaction` (`m2.md` §4.5, §6.7;
//! `m3.md` §4.6, §5.3, §6.6; `m5/01-lock-txn.md` §4.4, §5.6 to §5.8).
//!
//! M5: the single writer lock is gone (D5). A transaction takes an XID on its
//! first write ([`TxnManager::assign_xid`]); the XID, the running list and the
//! snapshot registry live in [`ProcArray`].
//!
//! Lock order (`00` §5.3): heavy locks (`LockManager`, waited for with no
//! other lock held) -> `checkpoint_lock` -> storage barrier -> commit gate
//! (shared) -> `ProcArray` mutex (leaf) -> control file / `Clog` internals /
//! `LockManager` internal mutex (the registration of the XID lock, D37).
//!
//! Commit order (D38): WAL insert and flush -> clog and running list
//! ([`ProcArray::finish`]) -> (caller) catalog cache invalidation -> file
//! removal -> release of the transaction-scope locks. `commit` / `abort` do
//! only the first two; releasing the locks is the caller's last step.

use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::clog::{Clog, XidStatus};
use super::lock::{LockManager, LockScope};
use super::multixact::MultiXactTable;
use super::proc_array::ProcArray;
use super::snapshot::RegisteredSnapshot;
use super::xact_wal::{XACT_ABORT, XACT_COMMIT, XactRecord};
use super::{CommandId, FIRST_COMMAND_ID, IsolationLevel, Snapshot, Xid};
use crate::backend::BackendId;
use crate::control::ControlFileHandle;
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::WriteCtx;
use crate::storage::buffer::track;
use crate::storage::smgr::RelFileLocator;
use crate::util::sync::{lock_ignore_poison, read, write};
use crate::wal::{Lsn, Wal};

#[derive(Debug)]
pub struct TxnManager {
    procs: Arc<ProcArray>,
    /// Only the checkpoint's buffer flush (shared) and the removal of
    /// dropped / rolled-back files (exclusive) take this (D10, LK-D23). A
    /// statement takes nothing: the relation lock protects the files.
    barrier: RwLock<()>,
    /// Files of dropped / rolled-back relations that could not be removed yet
    /// because a statement was running (see `try_exclusive_barrier`).
    deferred_unlinks: std::sync::Mutex<Vec<RelFileLocator>>,
    /// Shared by commit / abort from the WAL insert to the removal from the
    /// running list; exclusive while a checkpoint fixes its REDO point.
    commit_gate: RwLock<()>,
    clog: Arc<Clog>,
    control: Arc<ControlFileHandle>,
    wal: Arc<Wal>,
    locks: Arc<LockManager>,
    multixact: Arc<MultiXactTable>,
    knobs: DebugKnobs,
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
    /// `procs` carries `next_xid` (from recovery, or the control file after a
    /// clean stop) and the pre-allocation state (M3 §6.7.3); the XIDs
    /// pre-allocated before the last stop are discarded.
    pub fn new(
        procs: Arc<ProcArray>,
        clog: Arc<Clog>,
        control: Arc<ControlFileHandle>,
        wal: Arc<Wal>,
        locks: Arc<LockManager>,
        multixact: Arc<MultiXactTable>,
        knobs: DebugKnobs,
    ) -> Arc<Self> {
        if knobs.vacuum_ignore_registered_snapshots {
            procs.set_ignore_registered_snapshots(true);
        }
        Arc::new(TxnManager {
            procs,
            barrier: RwLock::new(()),
            deferred_unlinks: std::sync::Mutex::new(Vec::new()),
            commit_gate: RwLock::new(()),
            clog,
            control,
            wal,
            locks,
            multixact,
            knobs,
        })
    }

    /// Assigns an XID on the first write of a transaction (`Transaction.xid`
    /// is `None`). There is no writer lock. Inside `P` the XID is taken,
    /// put on the running list and `TransactionId(xid)` is registered
    /// exclusively for `backend` (D37), so a waiter never sees a running XID
    /// without its lock. `backend` must be registered with the
    /// `LockManager`.
    ///
    /// A failed pre-allocation is `Severity::Panic`; no XID is consumed.
    pub fn assign_xid(self: &Arc<Self>, backend: BackendId) -> Result<(Xid, XidGuard)> {
        let xid = self
            .procs
            .assign(backend, &self.locks, &self.clog, &self.control)?;
        Ok((
            xid,
            XidGuard {
                mgr: Arc::clone(self),
                backend,
                xid,
            },
        ))
    }

    /// A snapshot registered in the `ProcArray`; the registration ends when
    /// the result is dropped (LK-D8).
    pub fn take_snapshot(
        self: &Arc<Self>,
        own: Option<Xid>,
        curcid: CommandId,
    ) -> RegisteredSnapshot {
        self.procs.take_snapshot(own, curcid)
    }

    /// An unregistered snapshot, for unit tests and diagnostics only (LK-D9).
    pub fn snapshot(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot {
        self.procs.snapshot(own, curcid)
    }

    /// `min(running XIDs, registered snapshots' xmin, next_xid)` (D11).
    pub fn oldest_xmin(&self) -> Xid {
        self.procs.oldest_xmin()
    }

    /// Whether `xid` is in the raw running list (`TransactionIdIsInProgress`).
    pub fn is_in_progress(&self, xid: Xid) -> bool {
        self.procs.is_in_progress(xid)
    }

    /// Number of registered snapshots (tests and diagnostics; `04` C12).
    pub fn registered_snapshot_count(&self) -> usize {
        self.procs.registered_snapshot_count()
    }

    /// Commit step 1 (§5.8): COMMIT record (`dropped` = `pending_unlinks`),
    /// WAL flush (unless `skip_commit_flush`), then the clog and the running
    /// list inside `P`, then `MultiXactTable::on_xact_end`. The locks are
    /// **not** released (the caller does it last, D38). Every failure is
    /// `Severity::Panic`; the running list is not touched in that case.
    pub fn commit(&self, xid: Xid, dropped: &[RelFileLocator]) -> Result<()> {
        self.finish(xid, XidStatus::Committed, dropped)
    }

    /// Abort step 1: ABORT record (`created` = `pending_creates`). The WAL is
    /// flushed only if `created` is not empty (the files are removed
    /// afterwards, so the record must not be lost).
    pub fn abort(&self, xid: Xid, created: &[RelFileLocator]) -> Result<()> {
        self.finish(xid, XidStatus::Aborted, created)
    }

    fn finish(&self, xid: Xid, status: XidStatus, rels: &[RelFileLocator]) -> Result<()> {
        if !self.procs.is_in_progress(xid) {
            return Err(Error::internal(format!(
                "transaction {} is not running, cannot mark it {status:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        let committing = status == XidStatus::Committed;
        if self.knobs.release_locks_before_commit_record {
            // Mutation (D38 broken on purpose): waiters wake up before the
            // record and the clog say the transaction is over.
            self.release_xact_locks_early(xid);
        }
        let info = if committing { XACT_COMMIT } else { XACT_ABORT };
        {
            // Held until the clog and the running list are updated: the
            // checkpoint's REDO point must not fall between the record and
            // the clog update (D10). No latch is held here (convention 1).
            let _gate = ReadGate::acquire(&self.commit_gate)?;
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
            self.procs.finish(xid, status, &self.clog)?;
        }
        self.multixact.on_xact_end(xid);
        Ok(())
    }

    /// Mutation only: releases the transaction-scope locks of the backend
    /// that owns `xid` (found through its `TransactionId` lock).
    fn release_xact_locks_early(&self, xid: Xid) {
        let owner = self
            .locks
            .lock_status()
            .into_iter()
            .find(|r| r.granted && r.transactionid == Some(xid))
            .map(|r| r.backend);
        if let Some(b) = owner {
            self.locks.release_all(b, LockScope::Transaction);
        }
    }

    /// The end of a transaction without an XID (D48, M4 §14.1). When
    /// `flush_upto` is not `Lsn(0)`, the WAL is flushed up to it (sequence
    /// records the transaction depends on) before the caller releases its
    /// locks. Neither the clog nor the running list is touched and the gate
    /// is not taken. A failure is `Severity::Panic`.
    /// `DebugKnobs::skip_commit_flush` turns the flush off (mutation test).
    pub fn finish_without_xid(&self, flush_upto: Lsn) -> Result<()> {
        if flush_upto == Lsn::INVALID || self.knobs.skip_commit_flush {
            return Ok(());
        }
        self.wal.flush(flush_upto).map_err(panic_err)
    }

    /// Held by the checkpoint only while it fixes its REDO point (D10). It
    /// waits for the commits and aborts in progress and blocks new ones.
    pub fn commit_gate_exclusive(&self) -> Result<GateWrite<'_>> {
        let guard = write(&self.commit_gate).map_err(panic_err)?;
        track::gate_acquired();
        Ok(GateWrite { _guard: guard })
    }

    /// Shared guard for the checkpoint's buffer flush (LK-D23). Statements do
    /// not take it. The checkpoint thread reports it with
    /// `track::barrier_acquired` / `barrier_released`.
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>> {
        Ok(BarrierRead {
            _guard: read(&self.barrier)?,
        })
    }

    /// Exclusive guard for removing files (commit of DROP / TRUNCATE, abort of
    /// CREATE). The caller must not hold a shared barrier. It polls with
    /// `try_write` instead of queueing: std's `RwLock` prefers a queued
    /// writer, which would block every new shared taker behind a long flush.
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

    /// Non-blocking variant of `exclusive_barrier`: `None` while the shared
    /// barrier is held. Files that cannot be removed now are queued with
    /// `defer_unlinks`.
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

    pub fn procs(&self) -> &Arc<ProcArray> {
        &self.procs
    }

    pub fn clog(&self) -> &Arc<Clog> {
        &self.clog
    }

    pub fn wal(&self) -> &Arc<Wal> {
        &self.wal
    }

    pub fn locks(&self) -> &Arc<LockManager> {
        &self.locks
    }

    pub fn multixact(&self) -> &Arc<MultiXactTable> {
        &self.multixact
    }

    pub fn next_xid(&self) -> Xid {
        self.procs.next_xid()
    }

    /// `(next_xid, xid_limit)` read inside `P` (checkpoint).
    pub fn xid_counters(&self) -> (Xid, Xid) {
        self.procs.xid_counters()
    }
}

/// The shared commit gate; reports itself to `track` so that waiting for a
/// heavy lock while holding it is detected (convention 1).
struct ReadGate<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

impl<'a> ReadGate<'a> {
    fn acquire(gate: &'a RwLock<()>) -> Result<ReadGate<'a>> {
        let guard = read(gate).map_err(panic_err)?;
        track::gate_acquired();
        Ok(ReadGate { _guard: guard })
    }
}

impl Drop for ReadGate<'_> {
    fn drop(&mut self) {
        track::gate_released();
    }
}

/// Thin wrapper of `RwLockReadGuard`. The thread that holds it reports it
/// with `track::barrier_acquired` / `barrier_released` (re-entry detection).
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

impl Drop for GateWrite<'_> {
    fn drop(&mut self) {
        track::gate_released();
    }
}

/// Token for the XID of a transaction (the successor of `WriterGuard`; not a
/// lock). As a safety net on drop, a transaction that is still on the running
/// list is aborted in memory: the clog gets ABORTED, the XID leaves the
/// running list, and the transaction-scope locks (including the XID lock) are
/// released. No I/O, no WAL: it is the same as the implicit abort of a
/// crash. Files in `pending_creates` cannot be removed (no I/O in `Drop`):
/// a WARNING is logged and they stay as orphans.
///
/// "Unfinished" means the XID is still on the running list, so `commit` /
/// `abort` need no handshake with the guard; after a normal end the drop does
/// nothing (the session released the locks itself, D38).
#[derive(Debug)]
pub struct XidGuard {
    mgr: Arc<TxnManager>,
    backend: BackendId,
    xid: Xid,
}

impl XidGuard {
    pub fn xid(&self) -> Xid {
        self.xid
    }

    pub fn backend(&self) -> BackendId {
        self.backend
    }
}

impl Drop for XidGuard {
    fn drop(&mut self) {
        // Only the owner of the XID ends it, so checking and then finishing
        // cannot race with a commit of the same XID.
        if !self.mgr.procs.is_in_progress(self.xid) {
            return;
        }
        // Memory only; a failure (poisoned lock) leaves nothing more to do.
        let _ = self
            .mgr
            .procs
            .finish(self.xid, XidStatus::Aborted, &self.mgr.clog);
        self.mgr.multixact.on_xact_end(self.xid);
        self.mgr
            .locks
            .release_all(self.backend, LockScope::Transaction);
        warn(&format!(
            "transaction {} was dropped without commit or abort; aborted in memory (files created by it may remain as orphans)",
            self.xid.0
        ));
    }
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

#[derive(Debug, Default)]
pub struct Transaction {
    /// Assigned on the first write.
    pub xid: Option<Xid>,
    pub cid: CommandId,
    /// Whether the current statement wrote something.
    pub cid_used: bool,
    /// The token of `xid`; the drop is a safety net.
    pub guard: Option<XidGuard>,
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
    /// The isolation level of this transaction.
    pub isolation: IsolationLevel,
    /// Repeatable Read: the snapshot of the first statement, kept (and
    /// registered, so it holds the horizon) until the transaction ends.
    pub xact_snapshot: Option<RegisteredSnapshot>,
}

impl Transaction {
    pub fn new() -> Self {
        Transaction {
            cid: FIRST_COMMAND_ID,
            ..Transaction::default()
        }
    }

    /// Whether `x` is this transaction's XID. Visibility and locking code
    /// uses this instead of comparing with `xid` (D2: subtransactions would
    /// extend it).
    pub fn owns_xid(&self, x: Xid) -> bool {
        self.xid == Some(x)
    }

    /// `wal_flush_upto` を大きい方に更新する。
    pub fn note_wal(&mut self, lsn: Lsn) {
        if lsn > self.wal_flush_upto {
            self.wal_flush_upto = lsn;
        }
    }

    /// The only entry point for writes. An internal error if no XID was
    /// assigned. Sets `cid_used`.
    pub fn write_ctx(&mut self) -> Result<WriteCtx> {
        let xid = self
            .xid
            .ok_or_else(|| Error::internal("write without a transaction ID (no XID assigned)"))?;
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
    use crate::wal::{MIN_WAL_SEGMENT_SIZE, WalConfig, WalReader};

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
    use crate::storage::XID_PREFETCH;
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs, Vfs};
    use crate::txn::lock::{LockMode, LockTag};
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    pub(crate) fn control_data(next_xid: u64) -> ControlData {
        ControlData {
            format_version: crate::control::CONTROL_FORMAT_VERSION,
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
            next_multi: 1,
            mock_auth_nonce: [0; 32],
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
        locks: Arc<LockManager>,
        mgr: Arc<TxnManager>,
        registered: Mutex<HashSet<u64>>,
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
            let wal = Wal::initialize(Arc::clone(&vfs), wal_cfg(knobs)).unwrap();
            vfs.sync_dir(Path::new("")).unwrap();
            let locks = LockManager::with_knobs(knobs);
            let mgr = Rig::manager(&vfs, &control, &wal, &locks, Xid(3), knobs);
            Rig {
                sim,
                vfs,
                control,
                wal,
                locks,
                mgr,
                registered: Mutex::new(HashSet::new()),
            }
        }

        fn manager(
            vfs: &Arc<dyn Vfs>,
            control: &Arc<ControlFileHandle>,
            wal: &Arc<Wal>,
            locks: &Arc<LockManager>,
            next_xid: Xid,
            knobs: DebugKnobs,
        ) -> Arc<TxnManager> {
            let clog = Arc::new(Clog::open(Arc::clone(vfs), next_xid, Xid(3)).unwrap());
            TxnManager::new(
                ProcArray::new(next_xid),
                clog,
                Arc::clone(control),
                Arc::clone(wal),
                Arc::clone(locks),
                Arc::new(MultiXactTable::new(Arc::clone(control))),
                knobs,
            )
        }

        /// Registers the backend with the lock manager on first use.
        fn backend(&self, session: u64) -> BackendId {
            let id = BackendId(session);
            if self.registered.lock().unwrap().insert(session) {
                self.locks
                    .register_backend(id, i32::try_from(session).unwrap());
            }
            id
        }

        fn begin(&self, session: u64) -> (Xid, XidGuard) {
            self.mgr.assign_xid(self.backend(session)).unwrap()
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
    fn transaction_defaults_and_owns_xid() {
        let mut t = Transaction::new();
        assert_eq!(t.isolation, IsolationLevel::ReadCommitted);
        assert!(t.xact_snapshot.is_none() && t.guard.is_none());
        assert!(!t.owns_xid(Xid(7)));
        t.xid = Some(Xid(7));
        assert!(t.owns_xid(Xid(7)));
        assert!(!t.owns_xid(Xid(8)));
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
    fn recovered_next_xid_beyond_the_control_file_extends_the_limit() {
        let r = Rig::new();
        let ahead = Xid(r.control.get().next_xid + 10);
        let m2 = Rig::manager(
            &r.vfs,
            &r.control,
            &r.wal,
            &r.locks,
            ahead,
            DebugKnobs::default(),
        );
        let (x, g) = m2.assign_xid(r.backend(1)).unwrap();
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

    // ---- M5 (LK-3b): tests that replace the writer-lock tests ----

    #[test]
    fn several_writers_hold_xids_at_the_same_time() {
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let (x2, g2) = r.begin(2);
        let (x3, g3) = r.begin(3);
        assert_eq!((x1, x2, x3), (Xid(3), Xid(4), Xid(5)));
        assert_eq!(r.mgr.procs().running_xids(), vec![x1, x2, x3]);
        assert_eq!((g1.xid(), g1.backend()), (x1, BackendId(1)));
        // Finishing in another order than assigning is fine.
        r.mgr.commit(x2, &[]).unwrap();
        r.mgr.abort(x3, &[]).unwrap();
        assert!(r.mgr.is_in_progress(x1));
        assert_eq!(r.mgr.oldest_xmin(), x1);
        r.mgr.commit(x1, &[]).unwrap();
        for g in [g1, g2, g3] {
            drop(g);
        }
        assert_eq!(r.mgr.clog().status(x2).unwrap(), XidStatus::Committed);
        assert_eq!(r.mgr.clog().status(x3).unwrap(), XidStatus::Aborted);
        assert_eq!(r.mgr.oldest_xmin(), Xid(6));
    }

    #[test]
    fn assign_registers_the_xid_lock_exclusively() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        let tag = LockTag::TransactionId(x);
        assert!(r.locks.holds(BackendId(1), tag, LockMode::Exclusive));
        // Another backend cannot take it without waiting.
        r.backend(2);
        assert!(
            !r.locks
                .try_acquire(BackendId(2), tag, LockMode::Share, LockScope::Transaction)
        );
        r.mgr.commit(x, &[]).unwrap();
        // D38: commit does not release the locks; the caller does, last.
        assert!(r.locks.holds(BackendId(1), tag, LockMode::Exclusive));
        r.locks.release_all(BackendId(1), LockScope::Transaction);
        assert!(
            r.locks
                .try_acquire(BackendId(2), tag, LockMode::Share, LockScope::Transaction)
        );
        drop(g);
        // A finished guard does not touch the locks again.
        assert!(r.locks.holds(BackendId(2), tag, LockMode::Share));
        r.locks.release_all(BackendId(2), LockScope::Transaction);
    }

    #[test]
    fn assign_for_an_unregistered_backend_fails_without_consuming_an_xid() {
        let r = Rig::new();
        let e = r.mgr.assign_xid(BackendId(77)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(r.mgr.procs().running_xids().is_empty());
        let (x, _g) = r.begin(1);
        assert_eq!(x, Xid(3));
    }

    #[test]
    fn dropping_an_unfinished_guard_releases_the_transaction_locks() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        let before = r.wal.insert_lsn();
        let tag = LockTag::TransactionId(x);
        // A session-scope lock stays.
        let other = LockTag::TransactionId(Xid(9999));
        assert!(
            r.locks
                .try_acquire(BackendId(1), other, LockMode::Exclusive, LockScope::Session)
        );
        drop(g);
        assert!(!r.mgr.is_in_progress(x));
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Aborted);
        assert_eq!(r.wal.insert_lsn(), before, "the safety net writes no WAL");
        assert!(!r.locks.holds(BackendId(1), tag, LockMode::Exclusive));
        assert!(r.locks.holds(BackendId(1), other, LockMode::Exclusive));
        r.locks.release_all(BackendId(1), LockScope::Session);
    }

    #[test]
    fn a_waiter_on_the_xid_lock_wakes_only_after_commit() {
        // The order of D38 seen from another backend: while the XID is still
        // running its lock is held; after `commit` the status is final.
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.backend(2);
        assert!(!r.mgr.locks().try_acquire(
            BackendId(2),
            LockTag::TransactionId(x),
            LockMode::Share,
            LockScope::Transaction
        ));
        assert!(r.mgr.is_in_progress(x));
        r.mgr.commit(x, &[]).unwrap();
        assert!(!r.mgr.is_in_progress(x));
        assert_eq!(r.mgr.clog().status(x).unwrap(), XidStatus::Committed);
        r.locks.release_all(BackendId(1), LockScope::Transaction);
        assert!(r.mgr.locks().try_acquire(
            BackendId(2),
            LockTag::TransactionId(x),
            LockMode::Share,
            LockScope::Transaction
        ));
        r.locks.release_all(BackendId(2), LockScope::Transaction);
        drop(g);
    }

    #[test]
    fn control_failure_during_prefetch_consumes_no_xid_and_recovers() {
        let r = Rig::new();
        let b = r.backend(1);
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("global".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = r.mgr.assign_xid(b).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.mgr.xid_counters(), (Xid(3), Xid(3)));
        assert!(r.mgr.procs().running_xids().is_empty());
        r.sim.set_faults(FaultPlan::default());
        let (x, _g) = r.mgr.assign_xid(b).unwrap();
        assert_eq!(x, Xid(3));
    }

    #[test]
    fn take_snapshot_registers_and_holds_the_horizon() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        let reg = r.mgr.take_snapshot(Some(x), 2);
        assert_eq!(r.mgr.registered_snapshot_count(), 1);
        assert_eq!((reg.xmin, reg.xmax, reg.own_xid), (x, Xid(4), Some(x)));
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        // The registered snapshot (xmin = x) keeps the horizon back.
        assert_eq!(r.mgr.oldest_xmin(), x);
        drop(reg);
        assert_eq!(r.mgr.registered_snapshot_count(), 0);
        assert_eq!(r.mgr.oldest_xmin(), Xid(4));
    }

    #[test]
    fn finish_without_xid_flushes_only_when_asked() {
        let r = Rig::new();
        r.mgr.finish_without_xid(Lsn::INVALID).unwrap();
        let ins = r
            .wal
            .insert(
                XactRecord {
                    time_us: 1,
                    rels: vec![],
                }
                .builder(XACT_COMMIT, Xid(3)),
            )
            .unwrap();
        assert!(r.wal.flushed_lsn() < r.wal.insert_lsn());
        r.mgr.finish_without_xid(ins.end).unwrap();
        assert_eq!(r.wal.flushed_lsn(), r.wal.insert_lsn());
        // No XID was involved: nothing in the running list or the clog.
        assert!(r.mgr.procs().running_xids().is_empty());
    }

    #[test]
    fn finish_without_xid_respects_the_skip_flush_knob() {
        let r = Rig::with_knobs(DebugKnobs {
            skip_commit_flush: true,
            ..DebugKnobs::default()
        });
        let ins = r
            .wal
            .insert(
                XactRecord {
                    time_us: 1,
                    rels: vec![],
                }
                .builder(XACT_COMMIT, Xid(3)),
            )
            .unwrap();
        r.mgr.finish_without_xid(ins.end).unwrap();
        assert!(r.wal.flushed_lsn() < r.wal.insert_lsn());
    }

    #[test]
    fn failed_flush_in_finish_without_xid_is_panic() {
        let r = Rig::new();
        let ins = r
            .wal
            .insert(
                XactRecord {
                    time_us: 1,
                    rels: vec![],
                }
                .builder(XACT_COMMIT, Xid(3)),
            )
            .unwrap();
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_wal".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = r.mgr.finish_without_xid(ins.end).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        r.sim.set_faults(FaultPlan::default());
    }

    #[test]
    fn commit_and_abort_end_the_multixact_membership() {
        use crate::txn::multixact::{LockTupleMode, MultiMember};
        let r = Rig::new();
        let (x1, g1) = r.begin(1);
        let (x2, g2) = r.begin(2);
        let mx = r.mgr.multixact();
        mx.ensure_reserved().unwrap();
        mx.create(&[
            MultiMember {
                xid: x1,
                mode: LockTupleMode::Share,
            },
            MultiMember {
                xid: x2,
                mode: LockTupleMode::KeyShare,
            },
        ])
        .unwrap();
        assert_eq!(mx.len(), 1);
        r.mgr.commit(x1, &[]).unwrap();
        assert_eq!(mx.len(), 1, "one member is still running");
        r.mgr.abort(x2, &[]).unwrap();
        assert_eq!(mx.len(), 0, "all members ended");
        drop((g1, g2));
    }

    #[test]
    fn concurrent_writers_commit_in_parallel() {
        let r = Rig::new();
        let n = Arc::new(AtomicUsize::new(0));
        let ids: Vec<BackendId> = (0..4).map(|s| r.backend(s)).collect();
        let hs: Vec<_> = ids
            .into_iter()
            .map(|b| {
                let m = Arc::clone(&r.mgr);
                let n = Arc::clone(&n);
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let (x, g) = m.assign_xid(b).unwrap();
                        m.commit(x, &[]).unwrap();
                        m.locks().release_all(b, LockScope::Transaction);
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
        assert!(r.mgr.procs().running_xids().is_empty());
        assert_eq!(r.mgr.next_xid(), Xid(83));
    }

    #[test]
    fn gate_and_barrier_state_is_tracked_for_rule_one() {
        let r = Rig::new();
        assert!(!track::gate_held());
        let gate = r.mgr.commit_gate_exclusive().unwrap();
        assert!(track::gate_held());
        drop(gate);
        assert!(!track::gate_held());
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        assert!(!track::gate_held(), "commit released the shared gate");
        // A failed commit releases it too.
        let e = r.mgr.commit(Xid(99), &[]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert!(!track::gate_held());
    }

    #[test]
    fn restart_discards_prefetched_xids_with_new_constructor() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        drop(g);
        let next = Xid(r.control.get().next_xid);
        let m2 = Rig::manager(
            &r.vfs,
            &r.control,
            &r.wal,
            &r.locks,
            next,
            DebugKnobs::default(),
        );
        assert_eq!(m2.next_xid(), Xid(3 + XID_PREFETCH));
        assert_eq!(m2.xid_counters().1, next, "the limit restarts at next_xid");
    }

    /// Whether backend 2 can take a `Share` lock on the XID lock of `x`
    /// (the lock a waiter for the transaction asks for).
    fn waiter_gets_xid_lock(r: &Rig, x: Xid) -> bool {
        r.backend(2);
        let got = r.locks.try_acquire(
            BackendId(2),
            LockTag::TransactionId(x),
            LockMode::Share,
            LockScope::Transaction,
        );
        if got {
            r.locks.release_all(BackendId(2), LockScope::Transaction);
        }
        got
    }

    #[test]
    fn commit_keeps_the_xid_lock_until_the_caller_releases_it() {
        let r = Rig::new();
        let (x, g) = r.begin(1);
        r.mgr.commit(x, &[]).unwrap();
        assert!(
            !waiter_gets_xid_lock(&r, x),
            "D38: the caller releases last"
        );
        r.locks.release_all(BackendId(1), LockScope::Transaction);
        assert!(waiter_gets_xid_lock(&r, x));
        drop(g);
    }

    #[test]
    fn mutation_release_locks_before_commit_record_breaks_the_order() {
        let r = Rig::with_knobs(DebugKnobs {
            release_locks_before_commit_record: true,
            ..DebugKnobs::default()
        });
        let (x, g) = r.begin(1);
        let before = r.wal.insert_lsn();
        // Make the commit fail at the WAL flush by an unregistered XID check
        // is not possible here; instead observe the lock right after commit.
        r.mgr.commit(x, &[]).unwrap();
        assert!(
            waiter_gets_xid_lock(&r, x),
            "the mutation released the locks inside commit, without the caller"
        );
        assert!(r.wal.insert_lsn() > before);
        drop(g);
    }

    #[test]
    fn mutation_release_locks_before_commit_record_frees_the_waiter_while_running() {
        // The waiter can get the XID lock while the clog still says
        // "in progress" for the moment the release happens: the release is
        // done before the record, so a commit that fails leaves no lock.
        let r = Rig::with_knobs(DebugKnobs {
            release_locks_before_commit_record: true,
            ..DebugKnobs::default()
        });
        let (x, g) = r.begin(1);
        // abort goes through the same path
        r.mgr.abort(x, &[]).unwrap();
        assert!(waiter_gets_xid_lock(&r, x));
        drop(g);
    }

    #[test]
    fn mutation_vacuum_ignore_registered_snapshots_lets_the_horizon_pass_them() {
        for ignore in [false, true] {
            let r = Rig::with_knobs(DebugKnobs {
                vacuum_ignore_registered_snapshots: ignore,
                ..DebugKnobs::default()
            });
            let snap = r.mgr.take_snapshot(None, 0); // xmin = next_xid (3)
            let (x, g) = r.begin(1);
            r.mgr.commit(x, &[]).unwrap();
            r.locks.release_all(BackendId(1), LockScope::Transaction);
            drop(g);
            let horizon = r.mgr.oldest_xmin();
            if ignore {
                assert_eq!(horizon, r.mgr.next_xid(), "the snapshot is ignored");
            } else {
                assert_eq!(horizon, Xid(3), "the snapshot holds the horizon back");
            }
            drop(snap);
        }
    }
}
