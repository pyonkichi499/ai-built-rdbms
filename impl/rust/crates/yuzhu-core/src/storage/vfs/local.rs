//! `LocalVfs`: the real file system, rooted at the data directory
//! (`m2.md` §6.1). The only module allowed to use `std::fs`.
//!
//! 担当 B が実装する。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{OpenMode, Vfs, VfsFile, VfsLock, unsupported};

#[derive(Debug)]
pub struct LocalVfs {
    #[allow(dead_code)]
    root: PathBuf,
}

impl LocalVfs {
    /// `root` is the data directory (made absolute by the real implementation).
    pub fn new(root: PathBuf) -> LocalVfs {
        LocalVfs { root }
    }
}

impl Vfs for LocalVfs {
    fn open(&self, _path: &Path, _mode: OpenMode) -> io::Result<Arc<dyn VfsFile>> {
        Err(unsupported("LocalVfs::open"))
    }
    fn exists(&self, _path: &Path) -> io::Result<bool> {
        Err(unsupported("LocalVfs::exists"))
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::remove_file"))
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::rename"))
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::create_dir"))
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::create_dir_all"))
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::remove_dir_all"))
    }
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        Err(unsupported("LocalVfs::read_dir"))
    }
    fn sync_dir(&self, _path: &Path) -> io::Result<()> {
        Err(unsupported("LocalVfs::sync_dir"))
    }
    fn lock_file(&self, _path: &Path) -> io::Result<Box<dyn VfsLock>> {
        Err(unsupported("LocalVfs::lock_file"))
    }
}
