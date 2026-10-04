//! File I/O abstraction (`m2.md` §4.3, §6.1). All file operations of the
//! engine go through [`Vfs`] so that faults can be injected ([`sim::SimVfs`]).
//! Paths are relative to the data directory.

pub mod local;
pub mod sim;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use self::local::LocalVfs;
pub use self::sim::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimStats, SimVfs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    ReadWrite,
    CreateNew,
    ReadOnly,
}

pub trait Vfs: Send + Sync + std::fmt::Debug {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Arc<dyn VfsFile>>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    /// `fsync` of a directory (makes creations, removals and renames durable).
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
    /// Exclusive advisory lock (for `yuzhu.pid`); released when the guard is
    /// dropped.
    fn lock_file(&self, path: &Path) -> io::Result<Box<dyn VfsLock>>;
}

/// A file without a position (`pread` / `pwrite`); usable concurrently
/// through `&self`.
pub trait VfsFile: Send + Sync + std::fmt::Debug {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// The file size (named `size` to avoid clippy's `len_without_is_empty`).
    fn size(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;
    fn sync_data(&self) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
}

pub trait VfsLock: Send + std::fmt::Debug {}

/// Normalizes a data-directory-relative path: drops `.` components and
/// rejects absolute paths and `..` (so that nothing can escape the root).
/// The root itself is the empty path.
pub(crate) fn normalize(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "path must be relative and must not contain \"..\": {}",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(out)
}
