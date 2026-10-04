//! MVCC visibility (`HeapTupleSatisfiesMVCC` subset) and
//! `satisfies_update` (`m2.md` §6.5.3, §6.6).

use super::tuple::TupleHeader;
use crate::error::Result;
use crate::storage::TmResult;
use crate::txn::clog::{Clog, XidStatus};
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

/// `HeapTupleSatisfiesMVCC` without hint bits, locks and subtransactions.
pub fn visible(clog: &Clog, t: &TupleHeader, snap: &Snapshot) -> Result<bool> {
    if Some(t.xmin) == snap.own_xid {
        if t.cmin >= snap.curcid {
            return Ok(false);
        }
        if t.xmax_invalid() {
            return Ok(true);
        }
        if Some(t.xmax) == snap.own_xid {
            return Ok(t.cmax >= snap.curcid);
        }
        return Ok(true);
    }
    if !committed_in_snapshot(clog, t.xmin, snap)? {
        return Ok(false);
    }
    if t.xmax_invalid() {
        return Ok(true);
    }
    if Some(t.xmax) == snap.own_xid {
        return Ok(t.cmax >= snap.curcid);
    }
    Ok(!committed_in_snapshot(clog, t.xmax, snap)?)
}

/// Reduced `HeapTupleSatisfiesUpdate`. `self_tid` is the tuple's own TID
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
        let c = Clog::open(Arc::new(SimVfs::new(1)), Xid(3)).unwrap();
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
}
