//! `ProcArray`: running XIDs, XID assignment and the registry of snapshots
//! (`m5/01-lock-txn.md` §4.4, §5.6; D11, D37).
//!
//! One mutex (`P`) guards the state. It is a leaf lock: nothing is waited
//! for inside it, and the only I/O is the control file update that
//! pre-allocates XIDs (M3 §6.7.3). The one allowed nesting besides the
//! control file and `Clog` internals is `P` -> `LockManager` internal mutex
//! (registering the XID lock in [`ProcArray::assign`], D37; it never waits).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::clog::{Clog, XidStatus};
use super::lock::{LockManager, LockMode, LockScope, LockTag};
use super::snapshot::RegisteredSnapshot;
use super::{CommandId, Snapshot, Xid};
use crate::backend::BackendId;
use crate::control::ControlFileHandle;
use crate::error::{Error, Result, Severity};
use crate::storage::XID_PREFETCH;
use crate::util::sync::{lock, lock_ignore_poison};

#[derive(Debug)]
struct ProcState {
    next_xid: Xid,
    /// XIDs below this are covered by the value recorded in the control file.
    xid_limit: Xid,
    /// Running transactions (those that hold an XID).
    running: BTreeSet<Xid>,
    /// `xmin` of each registered snapshot -> how many are registered.
    snap_xmins: BTreeMap<Xid, u32>,
    /// Mutation switch (`DebugKnobs::vacuum_ignore_registered_snapshots`).
    ignore_registered: bool,
}

impl ProcState {
    /// `min(running, registered snapshots, next_xid)` (D11).
    fn oldest_xmin(&self) -> Xid {
        let running = self.running.first().copied().unwrap_or(self.next_xid);
        let reg = if self.ignore_registered {
            self.next_xid
        } else {
            self.snap_xmins
                .keys()
                .next()
                .copied()
                .unwrap_or(self.next_xid)
        };
        running.min(reg).min(self.next_xid)
    }

    /// The `xmin` a snapshot taken now would get.
    fn snapshot_xmin(&self) -> Xid {
        self.running
            .first()
            .copied()
            .map_or(self.next_xid, |f| f.min(self.next_xid))
    }
}

#[derive(Debug)]
pub struct ProcArray {
    inner: Mutex<ProcState>,
    /// A lower bound of `oldest_xmin`, readable without the mutex. It only
    /// grows (a new snapshot's `xmin` is never below the current horizon).
    hint: AtomicU64,
}

impl ProcArray {
    /// `next_xid` comes from recovery (or the control file after a clean
    /// shutdown). `xid_limit` starts at `next_xid`, so the first
    /// [`assign`](Self::assign) records a new limit in the control file.
    pub fn new(next_xid: Xid) -> Arc<ProcArray> {
        let next = Xid(next_xid.0.max(Xid::FIRST_NORMAL.0));
        Arc::new(ProcArray::with_limit(next, next))
    }

    /// Like [`new`](Self::new) with an explicit `xid_limit` (the value
    /// already recorded in the control file).
    pub fn with_limit(next_xid: Xid, xid_limit: Xid) -> ProcArray {
        let next = Xid(next_xid.0.max(Xid::FIRST_NORMAL.0));
        ProcArray {
            inner: Mutex::new(ProcState {
                next_xid: next,
                xid_limit: xid_limit.max(next),
                running: BTreeSet::new(),
                snap_xmins: BTreeMap::new(),
                ignore_registered: false,
            }),
            hint: AtomicU64::new(next.0),
        }
    }

    /// Mutation switch: `oldest_xmin` ignores the registered snapshots.
    /// Set once while the cluster is built.
    pub fn set_ignore_registered_snapshots(&self, on: bool) {
        self.state().ignore_registered = on;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProcState> {
        lock_ignore_poison(&self.inner)
    }

    /// Recomputes the horizon and raises the hint. Call with `P` held.
    fn advance_hint(&self, p: &ProcState) -> Xid {
        let o = p.oldest_xmin();
        self.hint.fetch_max(o.0, Ordering::AcqRel);
        o
    }

    /// `TransactionIdIsInProgress` on the raw running list.
    pub fn is_in_progress(&self, xid: Xid) -> bool {
        self.state().running.contains(&xid)
    }

    /// `min(running, registered snapshots, next_xid)` (D11). Also advances
    /// [`oldest_xmin_hint`](Self::oldest_xmin_hint).
    pub fn oldest_xmin(&self) -> Xid {
        let p = self.state();
        self.advance_hint(&p)
    }

    /// A lock-free lower bound of [`oldest_xmin`](Self::oldest_xmin) that
    /// never decreases (`03` C5). Updated whenever a transaction finishes or
    /// a registered snapshot is dropped.
    pub fn oldest_xmin_hint(&self) -> Xid {
        Xid(self.hint.load(Ordering::Acquire))
    }

    /// An unregistered snapshot. Only for unit tests and diagnostics
    /// (LK-D9); everything else uses [`take_snapshot`](Self::take_snapshot).
    pub fn snapshot(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot {
        let p = self.state();
        Self::build_snapshot(&p, own, curcid)
    }

    fn build_snapshot(p: &ProcState, own: Option<Xid>, curcid: CommandId) -> Snapshot {
        Snapshot {
            xmin: p.snapshot_xmin(),
            xmax: p.next_xid,
            xip: p
                .running
                .iter()
                .copied()
                .filter(|x| Some(*x) != own)
                .collect(),
            curcid,
            own_xid: own,
        }
    }

    /// Computes a snapshot and registers it in one step under `P` (LK-D8).
    /// The registration ends when the result is dropped.
    pub fn take_snapshot(
        self: &Arc<Self>,
        own: Option<Xid>,
        curcid: CommandId,
    ) -> RegisteredSnapshot {
        let mut p = self.state();
        let snap = Self::build_snapshot(&p, own, curcid);
        let reg_xmin = snap.xmin;
        *p.snap_xmins.entry(reg_xmin).or_insert(0) += 1;
        drop(p);
        RegisteredSnapshot::new(snap, Arc::clone(self), reg_xmin)
    }

    /// Called by `RegisteredSnapshot::drop`. Never fails (poison is ignored).
    pub(super) fn unregister_snapshot(&self, reg_xmin: Xid) {
        let mut p = self.state();
        match p.snap_xmins.get_mut(&reg_xmin) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                p.snap_xmins.remove(&reg_xmin);
            }
            None => debug_assert!(
                std::thread::panicking(),
                "snapshot with xmin {} is not registered",
                reg_xmin.0
            ),
        }
        self.advance_hint(&p);
    }

    /// Number of registered snapshots (tests and diagnostics; `04` C12).
    pub fn registered_snapshot_count(&self) -> usize {
        self.state().snap_xmins.values().map(|n| *n as usize).sum()
    }

    pub fn next_xid(&self) -> Xid {
        self.state().next_xid
    }

    /// `(next_xid, xid_limit)` read inside `P` (checkpoint).
    pub fn xid_counters(&self) -> (Xid, Xid) {
        let p = self.state();
        (p.next_xid, p.xid_limit)
    }

    /// Startup only (before any XID is assigned): raises `next_xid` when REDO
    /// saw XIDs beyond the value the stack was built with. The limit is left
    /// alone, so the next [`assign`](Self::assign) extends it through the
    /// control file when needed. A lower value is ignored.
    pub(crate) fn raise_next_xid(&self, next: Xid) {
        let mut p = self.state();
        if next > p.next_xid {
            p.next_xid = next;
            self.advance_hint(&p);
        }
    }

    /// The running XIDs in ascending order (tests and diagnostics).
    pub fn running_xids(&self) -> Vec<Xid> {
        self.state().running.iter().copied().collect()
    }

    /// Allocates the next XID, pre-allocating through the control file when
    /// the limit is reached, registers `TransactionId(xid)` as an exclusive
    /// transaction-scope lock of `backend` and puts the XID on the running
    /// list, all inside `P` (D37). Only `TxnManager` calls this.
    ///
    /// A failed pre-allocation is a `Severity::Panic` error and leaves the
    /// limit and the counter unchanged.
    pub(crate) fn assign(
        &self,
        backend: BackendId,
        locks: &LockManager,
        clog: &Clog,
        control: &ControlFileHandle,
    ) -> Result<Xid> {
        let mut p = lock(&self.inner)?;
        if p.next_xid >= p.xid_limit {
            let new_limit = p.xid_limit.0.max(p.next_xid.0) + XID_PREFETCH;
            control.update(|c| c.next_xid = c.next_xid.max(new_limit))?;
            p.xid_limit = Xid(new_limit);
        }
        let xid = p.next_xid;
        clog.ensure_page_for(xid);
        // P -> LockManager's mutex is the allowed exception (D37). Nobody
        // else knows this XID, so the request never waits.
        if !locks.try_acquire(
            backend,
            LockTag::TransactionId(xid),
            LockMode::Exclusive,
            LockScope::Transaction,
        ) {
            return Err(Error::internal(format!(
                "transaction ID lock for {} could not be registered",
                xid.0
            )));
        }
        p.next_xid = Xid(xid.0 + 1);
        p.running.insert(xid);
        Ok(xid)
    }

    /// Marks `xid` finished: the clog gets `status`, then the XID leaves the
    /// running list, both inside `P`. Locks are released by the caller
    /// afterwards (D38). A failure is `Severity::Panic` and leaves the
    /// running list untouched.
    pub(crate) fn finish(&self, xid: Xid, status: XidStatus, clog: &Clog) -> Result<()> {
        let mut p = lock(&self.inner)?;
        if !p.running.contains(&xid) {
            return Err(Error::internal(format!(
                "transaction {} is not running, cannot mark it {status:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        clog.set_status(xid, status)
            .map_err(|e| e.with_severity(Severity::Panic))?;
        p.running.remove(&xid);
        self.advance_hint(&p);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CONTROL_FORMAT_VERSION, ControlData, DbState};
    use crate::storage::vfs::{SimVfs, Vfs};
    use crate::wal::MIN_WAL_SEGMENT_SIZE;
    use std::path::Path;

    struct Rig {
        procs: Arc<ProcArray>,
        locks: Arc<LockManager>,
        clog: Clog,
        control: ControlFileHandle,
    }

    fn rig(next: u64) -> Rig {
        let sim = SimVfs::new(3);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        sim.create_dir_all(Path::new("global")).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let data = ControlData {
            format_version: CONTROL_FORMAT_VERSION,
            catalog_version: 1,
            system_identifier: 1,
            state: DbState::InProduction,
            page_size: 8192,
            wal_segment_size: MIN_WAL_SEGMENT_SIZE,
            flags: 0,
            time: 0,
            checkpoint_lsn: 0,
            redo_lsn: 0,
            next_xid: next,
            oldest_xid: 3,
            next_oid: 16384,
            timeline: 1,
            min_recovery_lsn: 0,
            rel_seg_blocks: 131_072,
            data_layout_version: 1,
            builtin_hash: 0,
            next_multi: 1,
            mock_auth_nonce: [0; 32],
        };
        let control = ControlFileHandle::create(&vfs, &data).unwrap();
        let clog = Clog::open(Arc::clone(&vfs), Xid(next), Xid(3)).unwrap();
        Rig {
            procs: ProcArray::new(Xid(next)),
            locks: LockManager::new(),
            clog,
            control,
        }
    }

    impl Rig {
        fn assign(&self, b: u64) -> Xid {
            let id = BackendId(b);
            self.locks.register_backend(id, 100);
            self.procs
                .assign(id, &self.locks, &self.clog, &self.control)
                .unwrap()
        }

        fn finish(&self, x: Xid, s: XidStatus) {
            self.procs.finish(x, s, &self.clog).unwrap();
        }
    }

    #[test]
    fn raise_next_xid_only_moves_forward_and_extends_the_limit_on_assign() {
        let r = rig(3);
        r.procs.raise_next_xid(Xid(10));
        assert_eq!(r.procs.next_xid(), Xid(10));
        r.procs.raise_next_xid(Xid(5));
        assert_eq!(r.procs.next_xid(), Xid(10));
        assert_eq!(r.assign(1), Xid(10));
        assert!(r.procs.xid_counters().1 > Xid(10));
        assert_eq!(r.procs.oldest_xmin_hint(), Xid(10));
    }
    #[test]
    fn assigns_increasing_xids_and_tracks_running() {
        let r = rig(3);
        assert_eq!(r.procs.next_xid(), Xid(3));
        let a = r.assign(1);
        let b = r.assign(2);
        assert_eq!((a, b), (Xid(3), Xid(4)));
        assert_eq!(r.procs.running_xids(), vec![a, b]);
        assert!(r.procs.is_in_progress(a));
        r.finish(a, XidStatus::Committed);
        assert!(!r.procs.is_in_progress(a));
        assert_eq!(r.clog.status(a).unwrap(), XidStatus::Committed);
        r.finish(b, XidStatus::Aborted);
        assert_eq!(r.clog.status(b).unwrap(), XidStatus::Aborted);
        assert!(r.procs.running_xids().is_empty());
        assert!(!r.procs.is_in_progress(Xid(99)));
    }

    #[test]
    fn assign_registers_the_xid_lock_before_it_is_visible() {
        let r = rig(3);
        let x = r.assign(1);
        assert!(
            r.locks
                .holds(BackendId(1), LockTag::TransactionId(x), LockMode::Exclusive)
        );
    }

    #[test]
    fn assign_fails_for_an_unregistered_backend_without_consuming_an_xid() {
        let r = rig(3);
        let e = r
            .procs
            .assign(BackendId(9), &r.locks, &r.clog, &r.control)
            .unwrap_err();
        assert!(e.message.contains("transaction ID lock"));
        assert_eq!(r.procs.next_xid(), Xid(3));
        assert!(r.procs.running_xids().is_empty());
    }

    #[test]
    fn prefetch_extends_the_control_file_limit() {
        let r = rig(3);
        assert_eq!(r.procs.xid_counters(), (Xid(3), Xid(3)));
        r.assign(1);
        assert_eq!(r.procs.xid_counters(), (Xid(4), Xid(3 + XID_PREFETCH)));
        assert_eq!(r.control.get().next_xid, 3 + XID_PREFETCH);
    }

    #[test]
    fn first_xid_is_at_least_first_normal() {
        let p = ProcArray::new(Xid(0));
        assert_eq!(p.next_xid(), Xid::FIRST_NORMAL);
    }

    #[test]
    fn finish_of_an_unknown_xid_is_a_panic_error() {
        let r = rig(3);
        let e = r
            .procs
            .finish(Xid(3), XidStatus::Committed, &r.clog)
            .unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
    }

    #[test]
    fn snapshot_contents() {
        let r = rig(3);
        let s0 = r.procs.snapshot(None, 0);
        assert_eq!((s0.xmin, s0.xmax), (Xid(3), Xid(3)));
        assert!(s0.xip.is_empty());
        let a = r.assign(1);
        let b = r.assign(2);
        let other = r.procs.snapshot(None, 4);
        assert_eq!((other.xmin, other.xmax, other.curcid), (a, Xid(5), 4));
        assert_eq!(other.xip, vec![a, b]);
        let own = r.procs.snapshot(Some(a), 0);
        assert_eq!(own.xip, vec![b]);
        assert_eq!(own.xmin, a);
        assert!(own.is_own(a));
        r.finish(a, XidStatus::Committed);
        let later = r.procs.snapshot(None, 0);
        assert_eq!((later.xmin, later.xip), (b, vec![b]));
    }

    #[test]
    fn unregistered_snapshots_do_not_hold_the_horizon() {
        let r = rig(3);
        let _s = r.procs.snapshot(None, 0);
        assert_eq!(r.procs.registered_snapshot_count(), 0);
    }

    #[test]
    fn registered_snapshot_holds_the_horizon_until_dropped() {
        let r = rig(3);
        let a = r.assign(1);
        let snap = r.procs.take_snapshot(None, 0);
        assert_eq!(snap.xmin, a);
        assert_eq!(r.procs.registered_snapshot_count(), 1);
        r.finish(a, XidStatus::Committed);
        // The XID finished, but the snapshot still sees it as running.
        assert_eq!(r.procs.oldest_xmin(), a);
        assert_eq!(r.procs.oldest_xmin_hint(), a);
        drop(snap);
        assert_eq!(r.procs.registered_snapshot_count(), 0);
        assert_eq!(r.procs.oldest_xmin(), Xid(4));
        assert_eq!(r.procs.oldest_xmin_hint(), Xid(4));
    }

    #[test]
    fn same_xmin_is_counted() {
        let r = rig(3);
        let s1 = r.procs.take_snapshot(None, 0);
        let s2 = r.procs.take_snapshot(None, 1);
        assert_eq!(r.procs.registered_snapshot_count(), 2);
        let a = r.assign(1);
        assert_eq!(r.procs.oldest_xmin(), Xid(3));
        drop(s1);
        assert_eq!(r.procs.registered_snapshot_count(), 1);
        assert_eq!(r.procs.oldest_xmin(), Xid(3));
        drop(s2);
        assert_eq!(r.procs.oldest_xmin(), a);
    }

    #[test]
    fn oldest_xmin_is_min_of_running_registered_and_next() {
        let r = rig(3);
        assert_eq!(r.procs.oldest_xmin(), Xid(3));
        let a = r.assign(1);
        let b = r.assign(2);
        assert_eq!(r.procs.oldest_xmin(), a);
        r.finish(a, XidStatus::Committed);
        assert_eq!(r.procs.oldest_xmin(), b);
        let snap = r.procs.take_snapshot(None, 0); // xmin = b
        r.finish(b, XidStatus::Committed);
        assert_eq!(r.procs.oldest_xmin(), b);
        drop(snap);
        assert_eq!(r.procs.oldest_xmin(), Xid(5));
    }

    #[test]
    fn hint_never_decreases() {
        let r = rig(3);
        let a = r.assign(1);
        let s = r.procs.take_snapshot(None, 0);
        r.finish(a, XidStatus::Committed);
        let h1 = r.procs.oldest_xmin_hint();
        drop(s);
        let h2 = r.procs.oldest_xmin_hint();
        assert!(h2 >= h1);
        let b = r.assign(2);
        assert!(r.procs.oldest_xmin_hint() >= h2);
        assert!(r.procs.oldest_xmin() >= h2);
        r.finish(b, XidStatus::Aborted);
    }

    #[test]
    fn for_statement_keeps_the_view_and_replaces_own_and_curcid() {
        let r = rig(3);
        let a = r.assign(1);
        let rr = r.procs.take_snapshot(None, 0);
        let b = r.assign(2);
        let st = rr.for_statement(Some(b), 7);
        assert_eq!((st.xmin, st.xmax, &st.xip), (rr.xmin, rr.xmax, &rr.xip));
        assert_eq!(st.xip, vec![a]);
        assert_eq!((st.curcid, st.own_xid), (7, Some(b)));
        assert!(st.is_own(b) && !st.is_own(a));
        assert_eq!(r.procs.registered_snapshot_count(), 1);
        drop(st);
        assert_eq!(r.procs.registered_snapshot_count(), 1);
    }

    #[test]
    fn concurrent_snapshots_and_xids_leave_no_registration() {
        let r = Arc::new(rig(3));
        let handles: Vec<_> = (0..4u64)
            .map(|t| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let s = r.procs.take_snapshot(None, 0);
                        assert!(s.xmin <= s.xmax);
                        if i % 10 == 0 {
                            let x = r.assign(t * 1000 + i);
                            r.finish(x, XidStatus::Committed);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(r.procs.registered_snapshot_count(), 0);
        assert!(r.procs.running_xids().is_empty());
        assert_eq!(r.procs.oldest_xmin(), r.procs.next_xid());
    }
}
