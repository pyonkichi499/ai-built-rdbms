//! `RegisteredSnapshot`: a snapshot registered in the [`ProcArray`] that
//! holds back the horizon until it is dropped (`m5/01-lock-txn.md` §4.4,
//! D11).

use std::ops::Deref;
use std::sync::Arc;

use super::proc_array::ProcArray;
use super::{CommandId, Snapshot, Xid};

/// Not `Clone`: one value is exactly one registration. Dropping it removes
/// the registration (no I/O, cannot fail).
#[derive(Debug)]
pub struct RegisteredSnapshot {
    snap: Snapshot,
    procs: Arc<ProcArray>,
    reg_xmin: Xid,
}

impl RegisteredSnapshot {
    pub(super) fn new(snap: Snapshot, procs: Arc<ProcArray>, reg_xmin: Xid) -> RegisteredSnapshot {
        RegisteredSnapshot {
            snap,
            procs,
            reg_xmin,
        }
    }

    /// The statement snapshot of a Repeatable Read transaction: `xmin`,
    /// `xmax` and `xip` are kept, only `own_xid` and `curcid` are replaced.
    /// The copy is not registered; this snapshot keeps the registration.
    pub fn for_statement(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot {
        let mut s = self.snap.clone();
        s.curcid = curcid;
        s.own_xid = own;
        s
    }
}

impl Deref for RegisteredSnapshot {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.snap
    }
}

impl Drop for RegisteredSnapshot {
    fn drop(&mut self) {
        self.procs.unregister_snapshot(self.reg_xmin);
    }
}
