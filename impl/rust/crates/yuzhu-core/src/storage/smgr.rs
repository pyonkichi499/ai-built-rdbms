//! Storage manager: relation files, segments, forks (`m2.md` §4.3, §6.2).
//!
//! 担当 C が実装する。

#![allow(clippy::unimplemented)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::BLCKSZ;
use super::vfs::{Vfs, VfsFile};
use crate::error::{Error, Result};
use crate::types::Oid;

fn pending() -> Error {
    Error::not_supported("storage manager is not implemented yet")
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RelFileNumber(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RelFileLocator {
    pub spc_oid: Oid,
    pub db_oid: Oid,
    pub rel_number: RelFileNumber,
}

pub const DEFAULTTABLESPACE_OID: Oid = 1663;
pub const GLOBALTABLESPACE_OID: Oid = 1664;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum ForkNumber {
    Main = 0,
    Fsm = 1,
    VisibilityMap = 2,
    Init = 3,
}

pub type BlockNumber = u32;
pub const INVALID_BLOCK_NUMBER: BlockNumber = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BufferTag {
    pub rel: RelFileLocator,
    pub fork: ForkNumber,
    pub block: BlockNumber,
}

/// Path relative to the data directory, e.g. `base/5/16384.1`.
/// `debug_assert`s that a relation in the global tablespace has `db_oid == 0`.
pub fn relpath(_rel: RelFileLocator, _fork: ForkNumber, _segno: u32) -> PathBuf {
    unimplemented!("担当 C が実装")
}

type SyncKey = (RelFileLocator, ForkNumber, u32);

#[derive(Debug)]
pub struct StorageManager {
    #[allow(dead_code)]
    vfs: Arc<dyn Vfs>,
    #[allow(dead_code)]
    rel_seg_blocks: u32,
    #[allow(dead_code)]
    pending_sync: Mutex<HashMap<SyncKey, Arc<dyn VfsFile>>>,
    #[allow(dead_code)]
    pending_unlink: Mutex<Vec<RelFileLocator>>,
    #[allow(dead_code)]
    recovery: AtomicBool,
    #[allow(dead_code)]
    broken: AtomicBool,
}

impl StorageManager {
    pub fn new(vfs: Arc<dyn Vfs>, rel_seg_blocks: u32) -> Self {
        StorageManager {
            vfs,
            rel_seg_blocks,
            pending_sync: Mutex::new(HashMap::new()),
            pending_unlink: Mutex::new(Vec::new()),
            recovery: AtomicBool::new(false),
            broken: AtomicBool::new(false),
        }
    }

    /// Internal error if the file already exists (including a D13 leftover).
    pub fn create(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<()> {
        Err(pending())
    }

    /// Whether the first segment exists (a 0-byte leftover counts).
    pub fn exists(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<bool> {
        Err(pending())
    }

    pub fn nblocks(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<BlockNumber> {
        Err(pending())
    }

    pub fn read_block(&self, _tag: BufferTag, _buf: &mut [u8; BLCKSZ]) -> Result<()> {
        Err(pending())
    }

    pub fn write_block(&self, _tag: BufferTag, _buf: &[u8; BLCKSZ]) -> Result<()> {
        Err(pending())
    }

    /// Appends one zero-filled block and returns its number. Takes the
    /// per-relation extension lock internally; only `BufferPool::extend`
    /// calls this.
    pub fn extend(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<BlockNumber> {
        Err(pending())
    }

    /// Zero-fills up to and including `blk` (M3 REDO; unused in M2).
    pub fn extend_to(
        &self,
        _rel: RelFileLocator,
        _fork: ForkNumber,
        _blk: BlockNumber,
    ) -> Result<()> {
        Err(pending())
    }

    /// D13: removes later segments and non-main forks, truncates the first
    /// segment to 0 bytes and queues it for removal at the next checkpoint.
    pub fn unlink(&self, _rel: RelFileLocator) -> Result<()> {
        Err(pending())
    }

    pub fn immedsync(&self, _rel: RelFileLocator, _fork: ForkNumber) -> Result<()> {
        Err(pending())
    }

    /// Checkpoint: fsyncs every segment written since the last call.
    pub fn sync_pending(&self) -> Result<()> {
        Err(pending())
    }

    /// End of a checkpoint: removes the leftovers queued by `unlink`.
    pub fn finish_pending_unlinks(&self) -> Result<()> {
        Err(pending())
    }

    /// M3 recovery only; always off in M2.
    pub fn set_recovery_mode(&self, _on: bool) {
        unimplemented!("担当 C が実装")
    }

    pub fn is_broken(&self) -> bool {
        unimplemented!("担当 C が実装")
    }
}
