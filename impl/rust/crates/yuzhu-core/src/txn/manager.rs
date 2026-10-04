//! `TxnManager` (XID assignment, running list, snapshots, single-writer
//! lock, storage barrier), `Transaction` and `WriterGuard`
//! (`m2.md` §4.5, §6.7).
//!
//! Lock order (§5.9): writer ownership -> storage barrier -> `proc` (leaf)
//! -> control file / `Clog` internals. The `writer` mutex is only used for
//! the condvar wait and for changing the owner, and is released before
//! `proc` is taken.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use super::clog::{Clog, XidStatus};
use super::{CommandId, FIRST_COMMAND_ID, Snapshot, Xid};
use crate::control::ControlFileHandle;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::WriteCtx;
use crate::storage::XID_PREFETCH;
use crate::storage::smgr::RelFileLocator;
use crate::util::sync::{lock, lock_ignore_poison, read, wait, wait_timeout, write};

#[derive(Debug)]
struct ProcState {
    next_xid: Xid,
    /// XIDs below this are covered by the value recorded in the control file.
    xid_limit: Xid,
    /// Raw list of running XIDs.
    running: BTreeSet<Xid>,
}

#[derive(Debug)]
pub struct TxnManager {
    /// Leaf lock (apart from `Clog` internals and the control file mutex).
    proc: Mutex<ProcState>,
    /// Session ID of the writer, if any.
    writer: Mutex<Option<u64>>,
    writer_released: Condvar,
    barrier: RwLock<()>,
    clog: Arc<Clog>,
    control: Arc<ControlFileHandle>,
}

/// `proc` for the infallible accessors: a poisoned lock means another
/// thread panicked while updating the XID state, so we must not go on.
fn proc_or_panic(m: &Mutex<ProcState>) -> MutexGuard<'_, ProcState> {
    m.lock()
        .expect("transaction manager state is poisoned (another thread panicked)")
}

impl TxnManager {
    /// Starts with `next_xid = xid_limit = control.next_xid`: the XIDs
    /// pre-allocated before the last stop are discarded (§6.7.3).
    pub fn new(clog: Arc<Clog>, control: Arc<ControlFileHandle>) -> TxnManager {
        let next = Xid(control.get().next_xid.max(Xid::FIRST_NORMAL.0));
        TxnManager {
            proc: Mutex::new(ProcState {
                next_xid: next,
                xid_limit: next,
                running: BTreeSet::new(),
            }),
            writer: Mutex::new(None),
            writer_released: Condvar::new(),
            barrier: RwLock::new(()),
            clog,
            control,
        }
    }

    /// Takes the writer lock, assigns an XID and adds it to the running list
    /// (§6.7.1). A timeout is `55P03 canceling statement due to lock
    /// timeout`; `None` and a zero duration wait forever.
    pub fn begin_write(
        self: &Arc<Self>,
        session_id: u64,
        timeout: Option<Duration>,
    ) -> Result<(Xid, WriterGuard)> {
        self.acquire_writer(session_id, timeout)?;
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

    fn acquire_writer(&self, session_id: u64, timeout: Option<Duration>) -> Result<()> {
        let deadline = timeout.filter(|d| !d.is_zero()).map(|d| Instant::now() + d);
        let mut owner = lock(&self.writer)?;
        loop {
            match *owner {
                None => break,
                Some(s) if s == session_id => {
                    return Err(Error::internal(
                        "session already holds the writer lock (begin_write called twice)",
                    ));
                }
                Some(_) => {}
            }
            match deadline {
                None => owner = wait(&self.writer_released, owner)?,
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(lock_timeout());
                    }
                    let (g, _) = wait_timeout(&self.writer_released, owner, left)?;
                    owner = g;
                }
            }
        }
        *owner = Some(session_id);
        Ok(())
    }

    /// Clears the owner (if it is `session_id`) and wakes the waiters. Safe
    /// in `Drop`.
    fn release_writer(&self, session_id: u64) {
        let mut owner = lock_ignore_poison(&self.writer);
        if *owner == Some(session_id) {
            *owner = None;
            self.writer_released.notify_all();
        }
    }

    /// Allocates the next XID, pre-allocating through the control file when
    /// the limit is reached (§6.7.3).
    fn assign_xid(&self) -> Result<Xid> {
        let mut p = lock(&self.proc)?;
        if p.next_xid >= p.xid_limit {
            let new_limit = p.xid_limit.0 + XID_PREFETCH;
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

    /// Writes COMMITTED to the clog and removes `xid` from the running list,
    /// both inside the `proc` mutex (no I/O). Failure is `Severity::Panic`;
    /// the running list is not touched in that case.
    pub fn commit(&self, xid: Xid) -> Result<()> {
        self.finish(xid, XidStatus::Committed)
    }

    pub fn abort(&self, xid: Xid) -> Result<()> {
        self.finish(xid, XidStatus::Aborted)
    }

    fn finish(&self, xid: Xid, status: XidStatus) -> Result<()> {
        let mut p = lock(&self.proc)?;
        if !p.running.contains(&xid) {
            return Err(Error::internal(format!(
                "transaction {} is not running, cannot mark it {status:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        self.clog
            .set_status(xid, status)
            .map_err(|e| e.with_severity(Severity::Panic))?;
        p.running.remove(&xid);
        Ok(())
    }

    /// Shared guard held while a statement runs. Never store it in a struct.
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>> {
        Ok(BarrierRead {
            _guard: read(&self.barrier)?,
        })
    }

    /// Exclusive guard for DROP cleanup and checkpoints. The caller must not
    /// hold a shared barrier.
    pub fn exclusive_barrier(&self) -> Result<BarrierWrite<'_>> {
        Ok(BarrierWrite {
            _guard: write(&self.barrier)?,
        })
    }

    pub fn clog(&self) -> &Arc<Clog> {
        &self.clog
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

/// Token for owning the writer lock (not the lock guard itself). As a safety
/// net on drop, an unfinished transaction is aborted in memory (clog
/// ABORTED, removed from `running`; no I/O) and the writer lock is released.
/// Files in `pending_creates` cannot be removed on drop (no I/O in `Drop`):
/// a WARNING is logged and they stay as orphans.
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
}

impl Transaction {
    pub fn new() -> Self {
        Transaction {
            cid: FIRST_COMMAND_ID,
            ..Transaction::default()
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
    use crate::storage::vfs::{SimVfs, Vfs};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) fn control_data(next_xid: u64) -> ControlData {
        ControlData {
            format_version: 1,
            catalog_version: 1,
            system_identifier: 1,
            state: DbState::InProduction,
            page_size: 8192,
            wal_segment_size: 0,
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

    fn setup() -> (Arc<TxnManager>, Arc<ControlFileHandle>, Arc<dyn Vfs>) {
        let sim = SimVfs::new(3);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        sim.create_dir_all(Path::new("global")).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let control = Arc::new(ControlFileHandle::create(&vfs, &control_data(3)).unwrap());
        let clog = Arc::new(Clog::open(Arc::clone(&vfs), Xid(3)).unwrap());
        let mgr = Arc::new(TxnManager::new(clog, Arc::clone(&control)));
        (mgr, control, vfs)
    }

    #[test]
    fn xids_are_assigned_in_order_and_run() {
        let (m, _c, _v) = setup();
        let (x1, g1) = m.begin_write(1, None).unwrap();
        assert_eq!(x1, Xid(3));
        assert!(m.is_in_progress(x1));
        m.commit(x1).unwrap();
        assert!(!m.is_in_progress(x1));
        drop(g1);
        let (x2, g2) = m.begin_write(2, None).unwrap();
        assert_eq!(x2, Xid(4));
        m.abort(x2).unwrap();
        drop(g2);
        assert_eq!(m.clog().status(x1).unwrap(), XidStatus::Committed);
        assert_eq!(m.clog().status(x2).unwrap(), XidStatus::Aborted);
        assert_eq!(m.next_xid(), Xid(5));
    }

    #[test]
    fn xid_prefetch_is_written_to_control_file() {
        let (m, c, _v) = setup();
        let (x, g) = m.begin_write(1, None).unwrap();
        assert_eq!(m.xid_counters(), (Xid(4), Xid(3 + XID_PREFETCH)));
        assert_eq!(c.get().next_xid, 3 + XID_PREFETCH);
        m.commit(x).unwrap();
        drop(g);
        // Crossing the limit extends it again.
        for i in 0..XID_PREFETCH {
            let (x, g) = m.begin_write(1, None).unwrap();
            m.commit(x).unwrap();
            drop(g);
            assert_eq!(x.0, 4 + i);
        }
        let (next, limit) = m.xid_counters();
        assert_eq!(next, Xid(4 + XID_PREFETCH));
        assert_eq!(limit, Xid(3 + 2 * XID_PREFETCH));
        assert_eq!(c.get().next_xid, 3 + 2 * XID_PREFETCH);
    }

    #[test]
    fn restart_discards_prefetched_xids() {
        let (m, c, v) = setup();
        let (x, g) = m.begin_write(1, None).unwrap();
        m.commit(x).unwrap();
        drop(g);
        let clog = Arc::new(Clog::open(v, Xid(c.get().next_xid)).unwrap());
        let m2 = TxnManager::new(clog, c);
        assert_eq!(m2.next_xid(), Xid(3 + XID_PREFETCH));
    }

    #[test]
    fn snapshot_contents() {
        let (m, _c, _v) = setup();
        let s0 = m.snapshot(None, 0);
        assert_eq!((s0.xmin, s0.xmax), (Xid(3), Xid(3)));
        assert!(s0.xip.is_empty());

        let (x, g) = m.begin_write(1, None).unwrap();
        let other = m.snapshot(None, 0);
        assert_eq!(other.xmin, x);
        assert_eq!(other.xmax, Xid(x.0 + 1));
        assert_eq!(other.xip, vec![x]);
        let own = m.snapshot(Some(x), 5);
        assert!(own.xip.is_empty());
        assert_eq!(own.xmin, x);
        assert_eq!((own.curcid, own.own_xid), (5, Some(x)));
        m.commit(x).unwrap();
        drop(g);
        let after = m.snapshot(None, 0);
        assert_eq!((after.xmin, after.xmax), (Xid(4), Xid(4)));
        assert!(after.xip.is_empty());
    }

    #[test]
    fn writer_lock_blocks_then_hands_over() {
        let (m, _c, _v) = setup();
        let (x1, g1) = m.begin_write(1, None).unwrap();
        let m2 = Arc::clone(&m);
        let got = Arc::new(AtomicBool::new(false));
        let got2 = Arc::clone(&got);
        let t = std::thread::spawn(move || {
            let (x, g) = m2.begin_write(2, None).unwrap();
            got2.store(true, Ordering::SeqCst);
            m2.commit(x).unwrap();
            drop(g);
            x
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(!got.load(Ordering::SeqCst));
        m.commit(x1).unwrap();
        drop(g1);
        let x2 = t.join().unwrap();
        assert!(got.load(Ordering::SeqCst));
        assert!(x2 > x1);
    }

    #[test]
    fn writer_lock_timeout_is_55p03() {
        let (m, _c, _v) = setup();
        let (x1, g1) = m.begin_write(1, None).unwrap();
        let start = Instant::now();
        let e = m
            .begin_write(2, Some(Duration::from_millis(50)))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
        assert_eq!(e.message, "canceling statement due to lock timeout");
        assert!(start.elapsed() >= Duration::from_millis(50));
        // The loser consumed no XID and did not disturb the owner.
        assert_eq!(m.next_xid(), Xid(x1.0 + 1));
        m.commit(x1).unwrap();
        drop(g1);
        m.begin_write(2, Some(Duration::from_millis(50))).unwrap();
    }

    #[test]
    fn double_begin_by_the_same_session_is_an_error() {
        let (m, _c, _v) = setup();
        let (_x, _g) = m.begin_write(1, None).unwrap();
        let e = m
            .begin_write(1, Some(Duration::from_millis(10)))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn dropping_a_writer_guard_aborts_in_memory() {
        let (m, _c, _v) = setup();
        let (x, g) = m.begin_write(1, None).unwrap();
        drop(g);
        assert!(!m.is_in_progress(x));
        assert_eq!(m.clog().status(x).unwrap(), XidStatus::Aborted);
        // The writer lock was released.
        let (x2, g2) = m.begin_write(2, Some(Duration::from_millis(10))).unwrap();
        assert!(x2 > x);
        m.commit(x2).unwrap();
        drop(g2);
        assert_eq!(m.clog().status(x2).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn dropping_a_finished_guard_keeps_the_status() {
        let (m, _c, _v) = setup();
        let (x, g) = m.begin_write(1, None).unwrap();
        m.commit(x).unwrap();
        drop(g);
        assert_eq!(m.clog().status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn commit_of_unknown_xid_is_panic_error() {
        let (m, _c, _v) = setup();
        let e = m.commit(Xid(99)).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        let (x, g) = m.begin_write(1, None).unwrap();
        m.commit(x).unwrap();
        assert_eq!(m.abort(x).unwrap_err().severity, Severity::Panic);
        drop(g);
        assert_eq!(m.clog().status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn barrier_shared_and_exclusive() {
        let (m, _c, _v) = setup();
        {
            let _a = m.statement_barrier().unwrap();
            // Another thread can share it.
            let m2 = Arc::clone(&m);
            std::thread::spawn(move || {
                let _b = m2.statement_barrier().unwrap();
            })
            .join()
            .unwrap();
        }
        let _x = m.exclusive_barrier().unwrap();
    }

    #[test]
    fn exclusive_barrier_waits_for_statements() {
        let (m, _c, _v) = setup();
        let shared = m.statement_barrier().unwrap();
        let m2 = Arc::clone(&m);
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
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let sim = SimVfs::new(5);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        sim.create_dir_all(Path::new("global")).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        let control = Arc::new(ControlFileHandle::create(&vfs, &control_data(3)).unwrap());
        let clog = Arc::new(Clog::open(Arc::clone(&vfs), Xid(3)).unwrap());
        let m = Arc::new(TxnManager::new(clog, control));
        sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("global".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = m.begin_write(1, None).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(m.xid_counters(), (Xid(3), Xid(3)));
        sim.set_faults(FaultPlan::default());
        let (x, _g) = m.begin_write(2, Some(Duration::from_millis(10))).unwrap();
        assert_eq!(x, Xid(3));
    }
}
