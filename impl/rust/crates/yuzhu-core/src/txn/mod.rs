//! Transaction types: XID, command ID and snapshot (`m2.md` §4.5). This
//! module holds no logic; the commit log is in [`clog`] and the manager in
//! [`manager`].

pub mod clog;
pub mod manager;
pub mod xact_wal;

pub use self::manager::{
    BarrierRead, BarrierWrite, GateWrite, Transaction, TxnManager, WaitCtl, WriterGuard,
};

/// A 64-bit transaction ID (wraparound is not handled; `m2.md` D2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, Default)]
pub struct Xid(pub u64);

impl Xid {
    pub const INVALID: Xid = Xid(0);
    /// Rows created by initdb.
    pub const BOOTSTRAP: Xid = Xid(1);
    /// Reserved (unused).
    pub const FROZEN: Xid = Xid(2);
    pub const FIRST_NORMAL: Xid = Xid(3);

    pub fn is_normal(self) -> bool {
        self >= Xid::FIRST_NORMAL
    }

    /// The lower 32 bits, as shown by the `xid` type.
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_external(self) -> u32 {
        self.0 as u32
    }
}

pub type CommandId = u32;
pub const FIRST_COMMAND_ID: CommandId = 0;

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Every XID below this has finished.
    pub xmin: Xid,
    /// Every XID at or above this is in the future (invisible).
    pub xmax: Xid,
    /// XIDs running when the snapshot was taken (own XID excluded, sorted).
    pub xip: Vec<Xid>,
    /// Changes of the own transaction made by this command ID or later are
    /// invisible.
    pub curcid: CommandId,
    pub own_xid: Option<Xid>,
}

impl Snapshot {
    /// The `SnapshotAny` convention (`xmin == xmax == INVALID`): every tuple
    /// version is visible, committed or not.
    pub fn is_any(&self) -> bool {
        self.xmin == Xid::INVALID && self.xmax == Xid::INVALID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xid_classes() {
        assert!(!Xid::INVALID.is_normal());
        assert!(!Xid::BOOTSTRAP.is_normal());
        assert!(!Xid::FROZEN.is_normal());
        assert!(Xid::FIRST_NORMAL.is_normal());
        assert_eq!(Xid(0x1_0000_0005).to_external(), 5);
        assert!(Xid(3) < Xid(4));
    }
}
