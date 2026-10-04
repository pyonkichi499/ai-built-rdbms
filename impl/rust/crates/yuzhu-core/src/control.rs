//! Control file `global/yuzhu_control` with two alternating slots

//! (`m2.md` §3.2, §4.3a).
//!
//! 担当 B が実装する。

#![allow(clippy::unimplemented)]

use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::storage::vfs::{Vfs, VfsFile};

fn pending() -> Error {
    Error::not_supported("control file is not implemented yet")
}

/// The fields of `m2.md` §3.2 (without magic, generation and crc32c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlData {
    pub format_version: u32,
    pub catalog_version: u32,
    pub system_identifier: u64,
    pub state: DbState,
    pub page_size: u32,
    pub wal_segment_size: u32,
    pub flags: u32,
    pub time: i64,
    pub checkpoint_lsn: u64,
    pub redo_lsn: u64,
    pub next_xid: u64,
    pub oldest_xid: u64,
    pub next_oid: u32,
    pub timeline: u32,
    pub min_recovery_lsn: u64,
    pub rel_seg_blocks: u32,
    pub data_layout_version: u32,
    pub builtin_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbState {
    Startup = 0,
    ShutDown = 1,
    ShuttingDown = 2,
    InCrashRecovery = 3,
    InProduction = 4,
}

#[derive(Debug)]
#[allow(dead_code)]
struct ControlInner {
    data: ControlData,
    generation: u64,
    last_slot: usize,
}

#[derive(Debug)]
pub struct ControlFileHandle {
    #[allow(dead_code)]
    vfs: Arc<dyn Vfs>,
    #[allow(dead_code)]
    file: Arc<dyn VfsFile>,
    #[allow(dead_code)]
    inner: Mutex<ControlInner>,
}

impl ControlFileHandle {
    /// For initdb: writes both slots, then `sync_all` and `sync_dir(global)`.
    pub fn create(_vfs: &Arc<dyn Vfs>, _data: &ControlData) -> Result<ControlFileHandle> {
        Err(pending())
    }

    /// Reads both slots and takes the valid one with the larger generation.
    pub fn open(_vfs: &Arc<dyn Vfs>) -> Result<ControlFileHandle> {
        Err(pending())
    }

    /// Start-up checks of the constants (FATAL on mismatch).
    pub fn check_compatible(&self, _expected_builtin_hash: u64) -> Result<()> {
        Err(pending())
    }

    /// A copy of the current contents.
    pub fn get(&self) -> ControlData {
        unimplemented!("担当 B が実装")
    }

    /// Read-modify-write by the rules of §3.2. Failure is `Severity::Panic`.
    pub fn update(&self, _f: impl FnOnce(&mut ControlData)) -> Result<()> {
        Err(pending())
    }

    /// Only for the shutdown checkpoint: may lower `next_xid` (§3.2).
    pub fn update_for_shutdown(&self, _f: impl FnOnce(&mut ControlData)) -> Result<()> {
        Err(pending())
    }
}
