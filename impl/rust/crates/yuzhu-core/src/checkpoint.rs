//! The checkpoint procedure and the checkpointer thread (`m2.md` §4.8,
//! §5.6). 手順の持ち主は E だけ。`Cluster` は `run` を呼ぶだけ。
//!
//! 担当 E が実装する。

#![allow(clippy::unimplemented)]

use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::control::ControlFileHandle;
use crate::datadir::OidAllocator;
use crate::error::{Error, Result};
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::StorageManager;
use crate::txn::TxnManager;
use crate::txn::clog::Clog;

/// Borrowed pieces a checkpoint needs.
#[derive(Debug)]
pub struct CheckpointParts<'a> {
    pub pool: &'a BufferPool,
    pub smgr: &'a StorageManager,
    pub clog: &'a Clog,
    pub txn: &'a TxnManager,
    pub control: &'a ControlFileHandle,
    pub oids: &'a OidAllocator,
    pub checkpoint_lock: &'a Mutex<()>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointKind {
    Periodic,
    Explicit,
    /// Writes the exact `next_xid` and sets `state` to `ShutDown` (§6.7.3).
    /// The others write the look-ahead limit and leave `state` alone.
    Shutdown,
}

pub fn run(_parts: &CheckpointParts<'_>, _kind: CheckpointKind) -> Result<()> {
    Err(Error::not_supported("checkpoint is not implemented yet"))
}

/// Starts the checkpointer thread. `tick` borrows the parts for each run
/// (it upgrades a `Weak<Cluster>`) and returns false when the cluster is
/// gone, which ends the thread.
pub fn spawn_checkpointer(
    _interval: Duration,
    _tick: Box<dyn Fn() -> bool + Send>,
) -> CheckpointerHandle {
    unimplemented!("担当 E が実装")
}

#[derive(Debug)]
pub struct CheckpointerHandle {
    #[allow(dead_code)]
    stop: mpsc::Sender<()>,
    #[allow(dead_code)]
    join: JoinHandle<()>,
}

impl CheckpointerHandle {
    pub fn stop_and_join(self) {
        unimplemented!("担当 E が実装")
    }
}
