//! Data directory layout, `YUZHU_VERSION`, the pid file, OID allocation and
//! database directory copy (`m2.md` §3.9, §4.3a, §6.9). Uses `Vfs` only.
//!
//! 担当 B が実装する。

#![allow(clippy::unimplemented)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::control::ControlFileHandle;
use crate::error::{Error, Result};
use crate::storage::vfs::{Vfs, VfsLock};
use crate::types::Oid;

fn pending() -> Error {
    Error::not_supported("data directory handling is not implemented yet")
}

/// Checks `YUZHU_VERSION` in `dir` (§3.9).
pub fn check_version_file(_vfs: &dyn Vfs, _dir: &Path) -> Result<()> {
    Err(pending())
}

pub fn write_version_file(_vfs: &dyn Vfs, _dir: &Path, _sync: bool) -> Result<()> {
    Err(pending())
}

/// `yuzhu.pid`: locked while the server runs (§3.9). Dropping releases the
/// lock; the file is removed explicitly by [`PidFile::release`].
#[derive(Debug)]
pub struct PidFile {
    #[allow(dead_code)]
    lock: Box<dyn VfsLock>,
}

impl PidFile {
    pub fn acquire(_vfs: &dyn Vfs, _data_dir_display: &str, _port: u16) -> Result<PidFile> {
        Err(pending())
    }

    pub fn release(self, _vfs: &dyn Vfs) -> Result<()> {
        Err(pending())
    }
}

/// OID allocation with look-ahead recorded in the control file (§6.9.2).
#[derive(Debug)]
pub struct OidAllocator {
    #[allow(dead_code)]
    state: Mutex<(Oid, Oid)>,
    #[allow(dead_code)]
    control: Arc<ControlFileHandle>,
}

impl OidAllocator {
    /// `next = limit = control.next_oid`.
    pub fn new(control: Arc<ControlFileHandle>) -> OidAllocator {
        let next = control.get().next_oid;
        OidAllocator {
            state: Mutex::new((next, next)),
            control,
        }
    }

    /// Takes one OID from the counter (no duplicate check; that is
    /// `CatalogStore::get_new_oid`).
    pub fn next_raw(&self) -> Result<Oid> {
        Err(pending())
    }

    /// The look-ahead limit the checkpoint writes to the control file.
    pub fn limit(&self) -> Oid {
        unimplemented!("担当 B が実装")
    }
}

/// Copies `base/<src>/` to `base/<dst>/`. Flushing buffers first is the
/// caller's job (§6.9.3).
pub fn copy_database_dir(_vfs: &dyn Vfs, _src: Oid, _dst: Oid, _sync: bool) -> Result<()> {
    Err(pending())
}
