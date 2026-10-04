//! `TxnManager` (XID assignment, running list, snapshots, single-writer
//! lock, storage barrier), `Transaction` and `WriterGuard`
//! (`m2.md` §4.5, §6.7).
//!
//! `TxnManager` と `WriterGuard` は担当 E が実装する。`Transaction` は A が
//! 仕様どおりに実装済み（単純なので）。

#![allow(clippy::unimplemented)]

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use super::clog::Clog;
use super::{CommandId, FIRST_COMMAND_ID, Snapshot, Xid};
use crate::control::ControlFileHandle;
use crate::error::{Error, Result, sqlstate};
use crate::storage::WriteCtx;
use crate::storage::smgr::RelFileLocator;

fn pending() -> Error {
    Error::not_supported("transaction manager is not implemented yet")
}

#[derive(Debug)]
struct ProcState {
    #[allow(dead_code)]
    next_xid: Xid,
    #[allow(dead_code)]
    xid_limit: Xid,
    #[allow(dead_code)]
    running: BTreeSet<Xid>,
}

#[derive(Debug)]
pub struct TxnManager {
    #[allow(dead_code)]
    proc: Mutex<ProcState>,
    /// Session ID of the writer, if any.
    #[allow(dead_code)]
    writer: Mutex<Option<u64>>,
    #[allow(dead_code)]
    writer_released: Condvar,
    #[allow(dead_code)]
    barrier: RwLock<()>,
    clog: Arc<Clog>,
    #[allow(dead_code)]
    control: Arc<ControlFileHandle>,
}

impl TxnManager {
    /// Takes the writer lock, assigns an XID and adds it to the running list
    /// (§6.7.1). A timeout is `55P03 canceling statement due to lock
    /// timeout`.
    pub fn begin_write(
        self: &Arc<Self>,
        _session_id: u64,
        _timeout: Option<Duration>,
    ) -> Result<(Xid, WriterGuard)> {
        Err(pending())
    }

    pub fn snapshot(&self, _own_xid: Option<Xid>, _curcid: CommandId) -> Snapshot {
        unimplemented!("担当 E が実装")
    }

    /// Whether `xid` is in the raw running list (`TransactionIdIsInProgress`).
    pub fn is_in_progress(&self, _xid: Xid) -> bool {
        unimplemented!("担当 E が実装")
    }

    /// Writes COMMITTED to the clog and removes `xid` from the running list.
    /// Failure is `Severity::Panic`.
    pub fn commit(&self, _xid: Xid) -> Result<()> {
        Err(pending())
    }

    pub fn abort(&self, _xid: Xid) -> Result<()> {
        Err(pending())
    }

    /// Shared guard held while a statement runs. Never store it in a struct.
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>> {
        Err(pending())
    }

    /// Exclusive guard for DROP cleanup and checkpoints.
    pub fn exclusive_barrier(&self) -> Result<BarrierWrite<'_>> {
        Err(pending())
    }

    pub fn clog(&self) -> &Arc<Clog> {
        &self.clog
    }

    pub fn next_xid(&self) -> Xid {
        unimplemented!("担当 E が実装")
    }

    /// `(next_xid, xid_limit)` read inside the `proc` mutex (checkpoint).
    pub fn xid_counters(&self) -> (Xid, Xid) {
        unimplemented!("担当 E が実装")
    }
}

/// Thin wrapper of `RwLockReadGuard` (re-entry is detected in
/// `storage/buffer/track.rs`).
#[derive(Debug)]
pub struct BarrierRead<'a> {
    #[allow(dead_code)]
    guard: RwLockReadGuard<'a, ()>,
}

#[derive(Debug)]
pub struct BarrierWrite<'a> {
    #[allow(dead_code)]
    guard: RwLockWriteGuard<'a, ()>,
}

/// Token for owning the writer lock (not the lock guard itself). As a safety
/// net on drop, an unfinished transaction is aborted in memory (clog
/// ABORTED, removed from `running`; no I/O) and the writer lock is released.
/// Files in `pending_creates` cannot be removed on drop (no I/O in `Drop`):
/// a WARNING is logged and they stay as orphans.
#[derive(Debug)]
pub struct WriterGuard {
    #[allow(dead_code)]
    mgr: Arc<TxnManager>,
    #[allow(dead_code)]
    session_id: u64,
    #[allow(dead_code)]
    xid: Xid,
    #[allow(dead_code)]
    finished: bool,
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
}
