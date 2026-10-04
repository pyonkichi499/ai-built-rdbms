//! `SimVfs`: an in-memory file system with fault injection and crash
//! simulation (`m2.md` §4.3). Always built (not `cfg(test)`) because the
//! server's integration tests use it too.
//!
//! 担当 B が実装する。ここにあるのは API だけ（データ型は仕様どおり、
//! メソッドの本体は未実装）。

#![allow(clippy::unimplemented)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{OpenMode, Vfs, VfsFile, VfsLock, unsupported};

#[derive(Debug, Default)]
struct SimState {}

#[derive(Debug, Clone)]
pub struct SimVfs {
    #[allow(dead_code)]
    state: Arc<Mutex<SimState>>,
    #[allow(dead_code)]
    generation: u64,
}

impl SimVfs {
    /// `seed` drives the fault probabilities (`SplitMix64`).
    pub fn new(_seed: u64) -> SimVfs {
        SimVfs {
            state: Arc::new(Mutex::new(SimState::default())),
            generation: 0,
        }
    }

    pub fn set_faults(&self, _plan: FaultPlan) {
        unimplemented!("担当 B が実装")
    }

    /// Simulates a crash and returns a new instance over the same "disk".
    /// The old instance and everything opened through it fail with EIO
    /// afterwards.
    #[must_use]
    pub fn crash(&self, _mode: CrashMode) -> SimVfs {
        unimplemented!("担当 B が実装")
    }

    pub fn stats(&self) -> SimStats {
        unimplemented!("担当 B が実装")
    }
}

#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    pub rules: Vec<FaultRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOp {
    Read,
    Write,
    Sync,
    SyncDir,
    Open,
    Remove,
    Rename,
    SetLen,
}

#[derive(Debug, Clone)]
pub struct FaultRule {
    pub op: FaultOp,
    /// `None` matches every file.
    pub path_prefix: Option<PathBuf>,
    /// Only the n-th matching operation (1-based). `None` = every time.
    pub nth: Option<u64>,
    /// Exclusive with `nth`; decided by the seeded RNG.
    pub probability: Option<f64>,
    pub effect: FaultEffect,
}

#[derive(Debug, Clone)]
pub enum FaultEffect {
    Error(io::ErrorKind),
    ShortWrite,
    FsyncFailAndForget,
    BitFlipOnRead,
    Delay(Duration),
}

#[derive(Debug, Clone, Copy)]
pub enum CrashMode {
    DropUnsynced,
    TornSectors {
        sector: usize,
        keep_probability: f64,
    },
    KeepAll,
}

#[derive(Debug, Clone, Default)]
pub struct SimStats {
    pub reads: u64,
    pub writes: u64,
    pub syncs: u64,
    pub sync_dirs: u64,
    pub faults_fired: u64,
}

impl Vfs for SimVfs {
    fn open(&self, _path: &Path, _mode: OpenMode) -> io::Result<Arc<dyn VfsFile>> {
        Err(unsupported("SimVfs::open"))
    }
    fn exists(&self, _path: &Path) -> io::Result<bool> {
        Err(unsupported("SimVfs::exists"))
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::remove_file"))
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::rename"))
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::create_dir"))
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::create_dir_all"))
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::remove_dir_all"))
    }
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        Err(unsupported("SimVfs::read_dir"))
    }
    fn sync_dir(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("SimVfs::sync_dir"))
    }
    fn lock_file(&self, _path: &Path) -> io::Result<Box<dyn VfsLock>> {
        Err(unsupported("SimVfs::lock_file"))
    }
}
