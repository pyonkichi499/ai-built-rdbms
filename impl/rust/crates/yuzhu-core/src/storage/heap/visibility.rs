//! MVCC visibility (`HeapTupleSatisfiesMVCC` subset), `satisfies_update`,
//! `satisfies_dirty` and tuple classification (`m2.md` §6.5.3, §6.6;
//! `m5/02-row-lock-rr.md` §5.1, §6.3).
//!
//! Two generations live side by side until RW-1c switches `HeapStore` over:
//! the `*_env` functions follow the M5 rules (`ProcArray` decides "running",
//! `LOCK_ONLY` xmax is ignored); `satisfies_update`, `satisfies_dirty` and
//! `classify` are the M4 versions that `HeapStore` still calls.

use super::prune::RunningXids;
use super::tuple::TupleHeader;
use super::xmax::{XmaxState, decode_xmax, xmax_is_lock_only, xmin_is_frozen};
use crate::error::Result;
use crate::storage::{DirtyResult, TmResult, TupleState};
use crate::txn::clog::{Clog, XidStatus};
use crate::txn::multixact::{MultiXactState, MultiXactTable};
use crate::txn::{Snapshot, Xid};
use crate::types::Tid;

/// Whether `x` had committed as of `snap`. The clog is consulted only after
/// the snapshot says the XID is old enough.
pub fn committed_in_snapshot(clog: &Clog, x: Xid, snap: &Snapshot) -> Result<bool> {
    if x == Xid::BOOTSTRAP || x == Xid::FROZEN {
        return Ok(true);
    }
    if x >= snap.xmax || snap.xip.binary_search(&x).is_ok() {
        return Ok(false);
    }
    // An XID left IN_PROGRESS by a crash counts as aborted.
    Ok(clog.status(x)? == XidStatus::Committed)
}

/// `HeapTupleSatisfiesMVCC` without hint bits and subtransactions. A
/// `LOCK_ONLY` xmax (single or `MultiXact`) is ignored; a frozen xmin is
/// committed and visible to everyone (`m5/02` §6.3).
pub fn visible(clog: &Clog, t: &TupleHeader, snap: &Snapshot) -> Result<bool> {
    if snap.is_any() {
        return Ok(true);
    }
    if xmin_is_frozen(t) {
        // committed for everyone; cmin is meaningless
    } else if snap.is_own(t.xmin) {
        if t.cmin >= snap.curcid {
            return Ok(false);
        }
        if t.xmax_invalid() || xmax_is_lock_only(t) {
            return Ok(true);
        }
        if snap.is_own(t.xmax) {
            return Ok(t.cmax >= snap.curcid);
        }
        return Ok(true);
    } else if !committed_in_snapshot(clog, t.xmin, snap)? {
        return Ok(false);
    }
    if t.xmax_invalid() || xmax_is_lock_only(t) {
        return Ok(true);
    }
    if snap.is_own(t.xmax) {
        return Ok(t.cmax >= snap.curcid);
    }
    Ok(!committed_in_snapshot(clog, t.xmax, snap)?)
}

/// What an XID is right now (not as of a snapshot).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XidState {
    Running,
    Committed,
    Aborted,
}

/// The state `satisfies_update_env` and friends decide from.
#[allow(missing_debug_implementations)]
#[derive(Clone, Copy)]
pub struct VisEnv<'a> {
    pub clog: &'a Clog,
    pub procs: &'a dyn RunningXids,
    pub multi: &'a MultiXactTable,
}

impl VisEnv<'_> {
    /// `xid_state` (`m5/02` §5.1). The `ProcArray` is consulted first: commit
    /// writes the clog before leaving the `ProcArray`, so "not in the
    /// `ProcArray`" makes the clog final. `IN_PROGRESS` in the clog without a
    /// `ProcArray` entry is a crash leftover, i.e. aborted.
    pub fn xid_state(&self, x: Xid) -> Result<XidState> {
        if x == Xid::BOOTSTRAP || x == Xid::FROZEN {
            return Ok(XidState::Committed);
        }
        if self.procs.is_in_progress(x) {
            return Ok(XidState::Running);
        }
        Ok(match self.clog.status(x)? {
            XidStatus::Committed => XidState::Committed,
            XidStatus::Aborted | XidStatus::InProgress => XidState::Aborted,
        })
    }

    fn is_running(&self, x: Xid) -> bool {
        self.procs.is_in_progress(x)
    }
}

/// `HeapTupleSatisfiesUpdate` on the current state (`m5/02` §5.1).
/// `self_tid` is the tuple's own TID (to tell an UPDATE from a DELETE by
/// `ctid`). Errors: damaged xmax bits (XX001) and clog read failures.
pub fn satisfies_update_env(
    env: &VisEnv<'_>,
    t: &TupleHeader,
    snap: &Snapshot,
    self_tid: Tid,
) -> Result<TmResult> {
    if !xmin_is_frozen(t) {
        if snap.is_own(t.xmin) {
            if t.cmin >= snap.curcid {
                return Ok(TmResult::Invisible);
            }
        } else if env.xid_state(t.xmin)? != XidState::Committed {
            return Ok(TmResult::Invisible);
        }
    }
    Ok(match decode_xmax(t)? {
        XmaxState::None => TmResult::Ok,
        XmaxState::Updater { xid, .. } => {
            if snap.is_own(xid) {
                if t.cmax >= snap.curcid {
                    TmResult::SelfModified { cmax: t.cmax }
                } else {
                    TmResult::Invisible
                }
            } else {
                match env.xid_state(xid)? {
                    XidState::Running => TmResult::BeingModified { xmax: xid },
                    XidState::Committed => deleted_or_updated(t, self_tid),
                    XidState::Aborted => TmResult::Ok,
                }
            }
        }
        XmaxState::Locker { xid, .. } => {
            if snap.is_own(xid) || env.xid_state(xid)? == XidState::Running {
                TmResult::BeingModified { xmax: xid }
            } else {
                TmResult::Ok
            }
        }
        XmaxState::LockerMulti(id) => {
            match env
                .multi
                .expand(id, &|x| snap.is_own(x) || env.is_running(x))
            {
                MultiXactState::Live(members) => match members.first() {
                    Some(m) => TmResult::BeingModified { xmax: m.xid },
                    None => TmResult::Ok,
                },
                MultiXactState::Dead => TmResult::Ok,
            }
        }
    })
}

/// `HeapTupleSatisfiesDirty` for `TableStore::fetch_dirty` (`m5/02` §6.3).
/// The caller has already ruled out a line pointer that is not `NORMAL`.
/// Command IDs are not consulted. A running inserter or deleter gives
/// `WaitFor`; the inserter takes priority.
pub fn satisfies_dirty_env(
    env: &VisEnv<'_>,
    t: &TupleHeader,
    own: Option<Xid>,
) -> Result<DirtyResult> {
    if !xmin_is_frozen(t) && own != Some(t.xmin) {
        match env.xid_state(t.xmin)? {
            XidState::Committed => {}
            XidState::Aborted => return Ok(DirtyResult::Invisible),
            XidState::Running => return Ok(DirtyResult::WaitFor(t.xmin)),
        }
    }
    if t.xmax_invalid() || xmax_is_lock_only(t) {
        return Ok(DirtyResult::Visible);
    }
    if own == Some(t.xmax) {
        return Ok(DirtyResult::Invisible);
    }
    Ok(match env.xid_state(t.xmax)? {
        XidState::Running => DirtyResult::WaitFor(t.xmax),
        XidState::Committed => DirtyResult::Invisible,
        XidState::Aborted => DirtyResult::Visible,
    })
}

/// `TableStore::tuple_state` (`m5/02` §6.3). `xmax` is `Xid::INVALID` when
/// there is no deleter; `xmax_lock_only` is `LOCK_ONLY` (a lock is not a
/// delete). Command IDs are not consulted.
pub fn classify_env(
    env: &VisEnv<'_>,
    xmin: Xid,
    xmax: Xid,
    xmax_lock_only: bool,
    own: Option<Xid>,
) -> Result<TupleState> {
    if own != Some(xmin) {
        match env.xid_state(xmin)? {
            XidState::Committed => {}
            XidState::Aborted => return Ok(TupleState::InsertAborted),
            XidState::Running => return Ok(TupleState::InsertInProgress(xmin)),
        }
    }
    if xmax == Xid::INVALID || xmax_lock_only {
        return Ok(TupleState::Live);
    }
    if own == Some(xmax) {
        return Ok(TupleState::DeletedBySelf);
    }
    Ok(match env.xid_state(xmax)? {
        XidState::Running => TupleState::DeleteInProgress(xmax),
        XidState::Committed => TupleState::DeadCommitted,
        XidState::Aborted => TupleState::Live,
    })
}

/// M4 version of `HeapTupleSatisfiesUpdate` (superseded by [`satisfies_update_env`]; RW-1c removes it). `self_tid` is the tuple's own TID
/// (to tell an UPDATE from a DELETE by `ctid`).
///
/// "In progress" is decided from the snapshot (`xip`, `xmax`) and the clog:
/// an XID that the clog still shows as `IN_PROGRESS` but that the snapshot
/// does not list is a leftover of a crash, i.e. aborted. With the single
/// writer of M2 this equals `TxnManager::is_in_progress`.
pub fn satisfies_update(
    clog: &Clog,
    t: &TupleHeader,
    snap: &Snapshot,
    self_tid: Tid,
) -> Result<TmResult> {
    // xmin visibility (only the xmin half of `visible`).
    if Some(t.xmin) == snap.own_xid {
        if t.cmin >= snap.curcid {
            return Ok(TmResult::Invisible);
        }
    } else if !committed_in_snapshot(clog, t.xmin, snap)? {
        return Ok(TmResult::Invisible);
    }
    if t.xmax_invalid() {
        return Ok(TmResult::Ok);
    }
    if Some(t.xmax) == snap.own_xid {
        return Ok(if t.cmax >= snap.curcid {
            TmResult::SelfModified { cmax: t.cmax }
        } else {
            TmResult::Invisible
        });
    }
    if t.xmax == Xid::BOOTSTRAP || t.xmax == Xid::FROZEN {
        return Ok(deleted_or_updated(t, self_tid));
    }
    match clog.status(t.xmax)? {
        XidStatus::Committed => Ok(deleted_or_updated(t, self_tid)),
        XidStatus::Aborted => Ok(TmResult::Ok),
        XidStatus::InProgress => {
            if t.xmax >= snap.xmax || snap.xip.binary_search(&t.xmax).is_ok() {
                Ok(TmResult::BeingModified { xmax: t.xmax })
            } else {
                Ok(TmResult::Ok)
            }
        }
    }
}

/// Who an XID is from the caller's point of view (`m4/06` §4.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum XidKind {
    Own,
    Committed,
    /// Aborted, or still `IN_PROGRESS` in the clog (a crash leftover; M4 has
    /// a single writer, so no other transaction can be running).
    Aborted,
}

fn xid_kind(clog: &Clog, x: Xid, own: Option<Xid>) -> Result<XidKind> {
    if x == Xid::BOOTSTRAP || x == Xid::FROZEN {
        return Ok(XidKind::Committed);
    }
    if Some(x) == own {
        return Ok(XidKind::Own);
    }
    Ok(match clog.status(x)? {
        XidStatus::Committed => XidKind::Committed,
        XidStatus::Aborted | XidStatus::InProgress => XidKind::Aborted,
    })
}

/// `TableStore::tuple_state`. `xmax` is `Xid::INVALID` when there is no
/// deleter. Command IDs are not consulted.
pub fn classify(clog: &Clog, xmin: Xid, xmax: Xid, own: Option<Xid>) -> Result<TupleState> {
    if xid_kind(clog, xmin, own)? == XidKind::Aborted {
        return Ok(TupleState::InsertAborted);
    }
    if xmax == Xid::INVALID {
        return Ok(TupleState::Live);
    }
    Ok(match xid_kind(clog, xmax, own)? {
        XidKind::Aborted => TupleState::Live,
        XidKind::Own => TupleState::DeletedBySelf,
        XidKind::Committed => TupleState::DeadCommitted,
    })
}

/// `HeapTupleSatisfiesDirty` subset for `TableStore::fetch_dirty`. Never
/// returns `WaitFor` in M4. Command IDs are not consulted.
pub fn satisfies_dirty(clog: &Clog, t: &TupleHeader, own: Option<Xid>) -> Result<DirtyResult> {
    if xid_kind(clog, t.xmin, own)? == XidKind::Aborted {
        return Ok(DirtyResult::Invisible);
    }
    if t.xmax_invalid() || t.xmax == Xid::INVALID {
        return Ok(DirtyResult::Visible);
    }
    Ok(match xid_kind(clog, t.xmax, own)? {
        XidKind::Aborted => DirtyResult::Visible,
        XidKind::Own | XidKind::Committed => DirtyResult::Invisible,
    })
}

fn deleted_or_updated(t: &TupleHeader, self_tid: Tid) -> TmResult {
    if t.ctid == self_tid {
        TmResult::Deleted { xmax: t.xmax }
    } else {
        TmResult::Updated {
            ctid: t.ctid,
            xmax: t.xmax,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::heap::tuple::HEAP_XMAX_INVALID;
    use crate::storage::vfs::sim::SimVfs;

    fn clog() -> Clog {
        let c = Clog::open(Arc::new(SimVfs::new(1)), Xid(3), Xid(3)).unwrap();
        for x in 3..=12 {
            c.ensure_page_for(Xid(x));
        }
        c.set_status(Xid(5), XidStatus::Committed).unwrap();
        c.set_status(Xid(6), XidStatus::Aborted).unwrap();
        c.set_status(Xid(8), XidStatus::Committed).unwrap();
        c
    }

    fn snap(own: Option<u64>, xmax: u64, xip: &[u64], curcid: u32) -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(xmax),
            xip: xip.iter().map(|x| Xid(*x)).collect(),
            curcid,
            own_xid: own.map(Xid),
        }
    }

    fn tup(xmin: u64, xmax: u64, cmin: u32, cmax: u32) -> TupleHeader {
        TupleHeader {
            xmin: Xid(xmin),
            xmax: Xid(xmax),
            cmin,
            cmax,
            ctid: Tid {
                block: 0,
                offset: 1,
            },
            infomask2: 1,
            infomask: if xmax == 0 { HEAP_XMAX_INVALID } else { 0 },
            hoff: 40,
        }
    }

    #[test]
    fn snapshot_any_sees_every_version() {
        let c = clog();
        let any = Snapshot {
            xmin: Xid::INVALID,
            xmax: Xid::INVALID,
            xip: Vec::new(),
            curcid: u32::MAX,
            own_xid: None,
        };
        assert!(visible(&c, &tup(5, 0, 0, 0), &any).unwrap());
        assert!(visible(&c, &tup(6, 0, 0, 0), &any).unwrap());
        assert!(visible(&c, &tup(11, 0, 0, 0), &any).unwrap());
        assert!(visible(&c, &tup(5, 8, 0, 0), &any).unwrap());
    }

    #[test]
    fn committed_insert_is_visible_others_are_not() {
        let c = clog();
        let s = snap(None, 10, &[7], 0);
        assert!(visible(&c, &tup(5, 0, 0, 0), &s).unwrap());
        assert!(!visible(&c, &tup(6, 0, 0, 0), &s).unwrap()); // aborted
        assert!(!visible(&c, &tup(7, 0, 0, 0), &s).unwrap()); // in xip
        assert!(!visible(&c, &tup(9, 0, 0, 0), &s).unwrap()); // crash leftover
        assert!(!visible(&c, &tup(11, 0, 0, 0), &s).unwrap()); // future
        assert!(visible(&c, &tup(1, 0, 0, 0), &s).unwrap()); // bootstrap
    }

    #[test]
    fn deleter_visibility() {
        let c = clog();
        let s = snap(None, 10, &[7], 0);
        assert!(!visible(&c, &tup(5, 8, 0, 0), &s).unwrap()); // committed delete
        assert!(visible(&c, &tup(5, 6, 0, 0), &s).unwrap()); // aborted delete
        assert!(visible(&c, &tup(5, 7, 0, 0), &s).unwrap()); // running delete
        assert!(visible(&c, &tup(5, 11, 0, 0), &s).unwrap()); // future delete
        // Committed after the snapshot was taken.
        let old = snap(None, 8, &[], 0);
        assert!(visible(&c, &tup(5, 8, 0, 0), &old).unwrap());
    }

    #[test]
    fn own_changes_follow_command_ids() {
        let c = clog();
        let s = snap(Some(9), 10, &[], 2);
        assert!(visible(&c, &tup(9, 0, 1, 0), &s).unwrap());
        assert!(!visible(&c, &tup(9, 0, 2, 0), &s).unwrap());
        assert!(!visible(&c, &tup(9, 9, 0, 1), &s).unwrap()); // deleted earlier
        assert!(visible(&c, &tup(9, 9, 0, 2), &s).unwrap()); // deleted by this command
        assert!(!visible(&c, &tup(5, 9, 0, 0), &s).unwrap());
        assert!(visible(&c, &tup(5, 9, 0, 3), &s).unwrap());
    }

    #[test]
    fn classify_all_combinations() {
        let c = clog();
        let own = Some(Xid(9));
        let x = Xid;
        // xmin: 5 committed, 9 own, 1 bootstrap, 6 aborted, 10 leftover
        for (xmin, live) in [(5, true), (9, true), (1, true), (6, false), (10, false)] {
            let st = classify(&c, x(xmin), Xid::INVALID, own).unwrap();
            let want = if live {
                TupleState::Live
            } else {
                TupleState::InsertAborted
            };
            assert_eq!(st, want);
            if !live {
                assert_eq!(
                    classify(&c, x(xmin), x(8), own).unwrap(),
                    TupleState::InsertAborted
                );
            }
        }
        for xmin in [5, 9] {
            let st = |xmax| classify(&c, x(xmin), x(xmax), own).unwrap();
            assert_eq!(st(9), TupleState::DeletedBySelf);
            assert_eq!(st(8), TupleState::DeadCommitted);
            assert_eq!(st(6), TupleState::Live);
            assert_eq!(st(10), TupleState::Live);
        }
        assert_eq!(
            classify(&c, x(5), x(8), None).unwrap(),
            TupleState::DeadCommitted
        );
        assert_eq!(
            classify(&c, x(9), Xid::INVALID, None).unwrap(),
            TupleState::InsertAborted
        );
        assert!(classify(&c, Xid::INVALID, Xid::INVALID, own).is_err());
    }

    #[test]
    fn dirty_all_combinations() {
        let c = clog();
        let own = Some(Xid(9));
        for (xmin, live) in [(5, true), (9, true), (6, false), (10, false)] {
            let r = satisfies_dirty(&c, &tup(xmin, 0, 0, 0), own).unwrap();
            let want = if live {
                DirtyResult::Visible
            } else {
                DirtyResult::Invisible
            };
            assert_eq!(r, want);
            for (xmax, vis) in [(9, false), (8, false), (6, true), (10, true)] {
                let r = satisfies_dirty(&c, &tup(xmin, xmax, 0, 0), own).unwrap();
                let want = if live && vis {
                    DirtyResult::Visible
                } else {
                    DirtyResult::Invisible
                };
                assert_eq!(r, want, "xmin {xmin} xmax {xmax}");
            }
        }
    }

    #[test]
    fn update_results() {
        let c = clog();
        let me = Tid {
            block: 0,
            offset: 1,
        };
        let s = snap(Some(9), 10, &[7], 1);
        assert_eq!(
            satisfies_update(&c, &tup(5, 0, 0, 0), &s, me).unwrap(),
            TmResult::Ok
        );
        assert_eq!(
            satisfies_update(&c, &tup(6, 0, 0, 0), &s, me).unwrap(),
            TmResult::Invisible
        );
        assert_eq!(
            satisfies_update(&c, &tup(9, 0, 1, 0), &s, me).unwrap(),
            TmResult::Invisible
        );
        assert_eq!(
            satisfies_update(&c, &tup(5, 9, 0, 1), &s, me).unwrap(),
            TmResult::SelfModified { cmax: 1 }
        );
        assert_eq!(
            satisfies_update(&c, &tup(5, 7, 0, 0), &s, me).unwrap(),
            TmResult::BeingModified { xmax: Xid(7) }
        );
        // aborted and crash-leftover deleters are ignored
        assert_eq!(
            satisfies_update(&c, &tup(5, 6, 0, 0), &s, me).unwrap(),
            TmResult::Ok
        );
        assert_eq!(
            satisfies_update(&c, &tup(5, 4, 0, 0), &s, me).unwrap(),
            TmResult::Ok
        );
        assert_eq!(
            satisfies_update(&c, &tup(5, 8, 0, 0), &s, me).unwrap(),
            TmResult::Deleted { xmax: Xid(8) }
        );
        let mut t = tup(5, 8, 0, 0);
        t.ctid = Tid {
            block: 1,
            offset: 2,
        };
        assert_eq!(
            satisfies_update(&c, &t, &s, me).unwrap(),
            TmResult::Updated {
                ctid: t.ctid,
                xmax: Xid(8)
            }
        );
    }

    // ----- M5 (`*_env`, LOCK_ONLY, XMIN_FROZEN) ---------------------------------

    use crate::control::{ControlData, ControlFileHandle};
    use crate::storage::heap::tuple::{
        HEAP_KEYS_UPDATED, HEAP_XMAX_COMMITTED, HEAP_XMAX_EXCL_LOCK, HEAP_XMAX_IS_MULTI,
        HEAP_XMAX_KEYSHR_LOCK, HEAP_XMAX_LOCK_ONLY,
    };
    use crate::storage::heap::xmax::HEAP_XMIN_FROZEN;
    use crate::storage::vfs::Vfs;
    use crate::txn::multixact::{LockTupleMode, MultiMember};

    #[derive(Debug)]
    struct Procs(Vec<Xid>);

    impl RunningXids for Procs {
        fn is_in_progress(&self, x: Xid) -> bool {
            self.0.contains(&x)
        }
        fn next_xid(&self) -> Xid {
            Xid(100)
        }
        fn oldest_xmin(&self) -> Xid {
            Xid(3)
        }
        fn oldest_xmin_hint(&self) -> Xid {
            Xid(3)
        }
    }

    fn multi_table() -> MultiXactTable {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let ctl = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(1, 2, 131_072)).unwrap(),
        );
        let t = MultiXactTable::new(ctl);
        t.ensure_reserved().unwrap();
        t
    }

    /// clog: 5 committed, 6 aborted, 8 committed, 7 / 9 / 10 in progress in the
    /// clog; `procs` lists the ones that really run.
    fn with_env<R>(running: &[u64], f: impl FnOnce(&VisEnv<'_>) -> R) -> R {
        let c = clog();
        let procs = Procs(running.iter().map(|x| Xid(*x)).collect());
        let multi = multi_table();
        f(&VisEnv {
            clog: &c,
            procs: &procs,
            multi: &multi,
        })
    }

    fn lock_only(mut t: TupleHeader, xid: u64, bits: u16) -> TupleHeader {
        t.xmax = Xid(xid);
        t.infomask = (t.infomask & !HEAP_XMAX_INVALID) | HEAP_XMAX_LOCK_ONLY | bits;
        t
    }

    fn updater(mut t: TupleHeader, xid: u64, keys: bool) -> TupleHeader {
        t.xmax = Xid(xid);
        t.infomask &= !HEAP_XMAX_INVALID;
        if keys {
            t.infomask2 |= HEAP_KEYS_UPDATED;
        }
        t
    }

    #[test]
    fn visible_ignores_lock_only_xmax() {
        let c = clog();
        let s = snap(None, 10, &[7], 0);
        // a committed locker does not hide the row, nor does a running one
        for locker in [5, 6, 7, 8, 11] {
            let t = lock_only(tup(5, 0, 0, 0), locker, HEAP_XMAX_KEYSHR_LOCK);
            assert!(visible(&c, &t, &s).unwrap(), "locker {locker}");
        }
        // a committed UPDATER still hides it
        assert!(!visible(&c, &updater(tup(5, 0, 0, 0), 8, true), &s).unwrap());
        // MultiXact lockers are not expanded
        let m = lock_only(
            tup(5, 0, 0, 0),
            77,
            HEAP_XMAX_IS_MULTI | HEAP_XMAX_KEYSHR_LOCK,
        );
        assert!(visible(&c, &m, &s).unwrap());
        // an aborted insert stays invisible whatever the lock says
        let t = lock_only(tup(6, 0, 0, 0), 5, HEAP_XMAX_KEYSHR_LOCK);
        assert!(!visible(&c, &t, &s).unwrap());
    }

    #[test]
    fn own_insert_with_lock_only_xmax_is_visible() {
        let c = clog();
        let s = snap(Some(9), 10, &[], 2);
        let own_lock = lock_only(tup(9, 0, 1, 0), 9, HEAP_XMAX_EXCL_LOCK);
        assert!(visible(&c, &own_lock, &s).unwrap());
        // the own updater at an earlier command hides it
        assert!(!visible(&c, &updater(tup(9, 0, 0, 1), 9, false), &s).unwrap());
        // a lock by someone else on an own insert is ignored
        let other = lock_only(tup(9, 0, 1, 0), 8, HEAP_XMAX_KEYSHR_LOCK);
        assert!(visible(&c, &other, &s).unwrap());
    }

    #[test]
    fn frozen_xmin_is_visible_to_everyone() {
        let c = clog();
        let mut t = tup(11, 0, 99, 0); // future xmin, huge cmin
        t.infomask |= HEAP_XMIN_FROZEN;
        let s = snap(None, 10, &[], 0);
        assert!(visible(&c, &t, &s).unwrap());
        // also for an own-looking snapshot and when xmin is aborted / own
        t.xmin = Xid(6);
        assert!(visible(&c, &t, &s).unwrap());
        t.xmin = Xid(9);
        assert!(visible(&c, &t, &snap(Some(9), 10, &[], 0)).unwrap());
        // the xmax half still applies
        let mut deleted = tup(5, 8, 0, 0);
        deleted.infomask |= HEAP_XMIN_FROZEN;
        assert!(!visible(&c, &deleted, &s).unwrap());
        // the frozen / bootstrap XID values
        let mut bootstrap = tup(5, 0, 0, 0);
        bootstrap.xmin = Xid::FROZEN;
        assert!(visible(&c, &bootstrap, &s).unwrap());
    }

    #[test]
    fn xid_state_asks_the_proc_array_first() {
        with_env(&[7, 8], |e| {
            assert_eq!(e.xid_state(Xid::BOOTSTRAP).unwrap(), XidState::Committed);
            assert_eq!(e.xid_state(Xid::FROZEN).unwrap(), XidState::Committed);
            assert_eq!(e.xid_state(Xid(5)).unwrap(), XidState::Committed);
            assert_eq!(e.xid_state(Xid(6)).unwrap(), XidState::Aborted);
            // in the ProcArray: running even if the clog already says committed
            assert_eq!(e.xid_state(Xid(8)).unwrap(), XidState::Running);
            assert_eq!(e.xid_state(Xid(7)).unwrap(), XidState::Running);
            // clog IN_PROGRESS but not in the ProcArray: a crash leftover
            assert_eq!(e.xid_state(Xid(9)).unwrap(), XidState::Aborted);
        });
    }

    #[test]
    fn update_env_xmin_rules() {
        let me = Tid {
            block: 0,
            offset: 1,
        };
        let s = snap(Some(9), 10, &[], 1);
        with_env(&[7, 9], |e| {
            let u = |t: &TupleHeader| satisfies_update_env(e, t, &s, me).unwrap();
            assert_eq!(u(&tup(5, 0, 0, 0)), TmResult::Ok);
            assert_eq!(u(&tup(6, 0, 0, 0)), TmResult::Invisible); // aborted
            assert_eq!(u(&tup(7, 0, 0, 0)), TmResult::Invisible); // running
            assert_eq!(u(&tup(10, 0, 0, 0)), TmResult::Invisible); // leftover
            assert_eq!(u(&tup(9, 0, 0, 0)), TmResult::Ok); // own, earlier command
            assert_eq!(u(&tup(9, 0, 1, 0)), TmResult::Invisible); // own, this command
            let mut f = tup(11, 0, 0, 0);
            f.infomask |= HEAP_XMIN_FROZEN;
            assert_eq!(u(&f), TmResult::Ok);
        });
    }

    #[test]
    fn update_env_updater_rules() {
        let me = Tid {
            block: 0,
            offset: 1,
        };
        let moved = Tid {
            block: 1,
            offset: 2,
        };
        let s = snap(Some(9), 10, &[], 1);
        with_env(&[7, 9], |e| {
            let u = |t: &TupleHeader| satisfies_update_env(e, t, &s, me).unwrap();
            assert_eq!(
                u(&updater(tup(5, 0, 0, 0), 7, false)),
                TmResult::BeingModified { xmax: Xid(7) }
            );
            assert_eq!(u(&updater(tup(5, 0, 0, 0), 6, false)), TmResult::Ok);
            assert_eq!(u(&updater(tup(5, 0, 0, 0), 10, false)), TmResult::Ok); // leftover
            assert_eq!(
                u(&updater(tup(5, 0, 0, 0), 8, false)),
                TmResult::Deleted { xmax: Xid(8) }
            );
            let mut t = updater(tup(5, 0, 0, 0), 8, true);
            t.ctid = moved;
            assert_eq!(
                u(&t),
                TmResult::Updated {
                    ctid: moved,
                    xmax: Xid(8)
                }
            );
            // own updater: this command -> SelfModified, earlier command -> Invisible
            assert_eq!(
                u(&updater(tup(5, 0, 0, 1), 9, false)),
                TmResult::SelfModified { cmax: 1 }
            );
            assert_eq!(u(&updater(tup(5, 0, 0, 0), 9, false)), TmResult::Invisible);
        });
    }

    #[test]
    fn update_env_locker_rules() {
        let me = Tid {
            block: 0,
            offset: 1,
        };
        let s = snap(Some(9), 10, &[], 1);
        with_env(&[7, 9], |e| {
            let u = |t: &TupleHeader| satisfies_update_env(e, t, &s, me).unwrap();
            let l = |x| lock_only(tup(5, 0, 0, 0), x, HEAP_XMAX_KEYSHR_LOCK);
            assert_eq!(u(&l(7)), TmResult::BeingModified { xmax: Xid(7) });
            assert_eq!(u(&l(9)), TmResult::BeingModified { xmax: Xid(9) }); // own lock
            assert_eq!(u(&l(5)), TmResult::Ok); // committed locker
            assert_eq!(u(&l(6)), TmResult::Ok); // aborted locker
            assert_eq!(u(&l(10)), TmResult::Ok); // leftover
            // damaged bits are XX001, not a panic
            let mut bad = tup(5, 0, 0, 0);
            bad.xmax = Xid(7);
            bad.infomask = (bad.infomask & !HEAP_XMAX_INVALID) | HEAP_XMAX_IS_MULTI;
            assert!(satisfies_update_env(e, &bad, &s, me).is_err());
        });
    }

    #[test]
    fn update_env_multixact_rules() {
        let me = Tid {
            block: 0,
            offset: 1,
        };
        let s = snap(Some(9), 10, &[], 1);
        let c = clog();
        let procs = Procs(vec![Xid(7), Xid(9)]);
        let multi = multi_table();
        let env = VisEnv {
            clog: &c,
            procs: &procs,
            multi: &multi,
        };
        let mk = |members: &[u64]| {
            let ms: Vec<MultiMember> = members
                .iter()
                .map(|&x| MultiMember {
                    xid: Xid(x),
                    mode: LockTupleMode::KeyShare,
                })
                .collect();
            let id = multi.create(&ms).unwrap();
            lock_only(
                tup(5, 0, 0, 0),
                id.0,
                HEAP_XMAX_IS_MULTI | HEAP_XMAX_KEYSHR_LOCK,
            )
        };
        let u = |t: &TupleHeader| satisfies_update_env(&env, t, &s, me).unwrap();
        // finished members are skipped; the first live one is reported
        assert_eq!(u(&mk(&[5, 7, 9])), TmResult::BeingModified { xmax: Xid(7) });
        assert_eq!(u(&mk(&[9])), TmResult::BeingModified { xmax: Xid(9) });
        // all finished -> Ok
        assert_eq!(u(&mk(&[5, 6])), TmResult::Ok);
        // an ID the table does not know (older than this start) -> Ok
        let mut gone = mk(&[7]);
        gone.xmax = Xid(9_999_999);
        assert_eq!(u(&gone), TmResult::Ok);
        // MULTI + committed updater bit is not a locker (decode: Updater needs !MULTI)
        let mut t = tup(5, 0, 0, 0);
        t.xmax = Xid(8);
        t.infomask = (t.infomask & !HEAP_XMAX_INVALID) | HEAP_XMAX_COMMITTED;
        assert_eq!(u(&t), TmResult::Deleted { xmax: Xid(8) });
    }

    #[test]
    fn dirty_env_rules() {
        let own = Some(Xid(9));
        with_env(&[7, 9, 8], |e| {
            let d = |t: &TupleHeader| satisfies_dirty_env(e, t, own).unwrap();
            // xmin
            assert_eq!(d(&tup(5, 0, 0, 0)), DirtyResult::Visible);
            assert_eq!(d(&tup(9, 0, 0, 0)), DirtyResult::Visible); // own
            assert_eq!(d(&tup(6, 0, 0, 0)), DirtyResult::Invisible);
            assert_eq!(d(&tup(10, 0, 0, 0)), DirtyResult::Invisible); // leftover
            assert_eq!(d(&tup(7, 0, 0, 0)), DirtyResult::WaitFor(Xid(7)));
            let mut f = tup(6, 0, 0, 0);
            f.infomask |= HEAP_XMIN_FROZEN;
            assert_eq!(d(&f), DirtyResult::Visible);
            // inserter wait takes priority over the deleter
            assert_eq!(d(&tup(7, 8, 0, 0)), DirtyResult::WaitFor(Xid(7)));
            // xmax
            assert_eq!(d(&tup(5, 9, 0, 0)), DirtyResult::Invisible); // own delete
            assert_eq!(d(&tup(5, 7, 0, 0)), DirtyResult::WaitFor(Xid(7)));
            assert_eq!(d(&tup(5, 6, 0, 0)), DirtyResult::Visible); // aborted
            assert_eq!(d(&tup(5, 10, 0, 0)), DirtyResult::Visible); // leftover
            // a running updater (non-key update) also makes us wait
            assert_eq!(
                d(&updater(tup(5, 0, 0, 0), 7, false)),
                DirtyResult::WaitFor(Xid(7))
            );
            // locks are never looked at
            for x in [5, 7, 9, 10] {
                let t = lock_only(tup(5, 0, 0, 0), x, HEAP_XMAX_EXCL_LOCK);
                assert_eq!(d(&t), DirtyResult::Visible, "locker {x}");
            }
        });
        with_env(&[], |e| {
            assert_eq!(
                satisfies_dirty_env(e, &tup(5, 8, 0, 0), None).unwrap(),
                DirtyResult::Invisible
            );
        });
    }

    #[test]
    fn classify_env_rules() {
        let own = Some(Xid(9));
        with_env(&[7, 9], |e| {
            let k = |xmin, xmax, lock, o| classify_env(e, Xid(xmin), Xid(xmax), lock, o).unwrap();
            assert_eq!(k(5, 0, false, own), TupleState::Live);
            assert_eq!(k(9, 0, false, own), TupleState::Live);
            assert_eq!(k(1, 0, false, own), TupleState::Live);
            assert_eq!(k(6, 0, false, own), TupleState::InsertAborted);
            assert_eq!(k(10, 8, false, own), TupleState::InsertAborted); // leftover
            assert_eq!(k(7, 0, false, own), TupleState::InsertInProgress(Xid(7)));
            assert_eq!(k(7, 8, false, own), TupleState::InsertInProgress(Xid(7)));
            assert_eq!(k(5, 9, false, own), TupleState::DeletedBySelf);
            assert_eq!(k(5, 8, false, own), TupleState::DeadCommitted);
            assert_eq!(k(5, 6, false, own), TupleState::Live);
            assert_eq!(k(5, 10, false, own), TupleState::Live);
            assert_eq!(k(5, 7, false, own), TupleState::DeleteInProgress(Xid(7)));
            // LOCK_ONLY xmax counts as no xmax, whoever holds it
            for x in [5, 7, 9, 10] {
                assert_eq!(k(5, x, true, own), TupleState::Live, "locker {x}");
            }
            // without an own XID a running inserter is not "own"
            assert_eq!(k(9, 0, false, None), TupleState::InsertInProgress(Xid(9)));
        });
    }
}
