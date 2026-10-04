//! `LocalVfs`: the real file system, rooted at the data directory
//! (`m2.md` §6.1). The only module allowed to use `std::fs`.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{OpenMode, Vfs, VfsFile, VfsLock, normalize};

/// Permissions of files and directories created by the engine (as PostgreSQL).
const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;

#[derive(Debug)]
pub struct LocalVfs {
    root: PathBuf,
}

impl LocalVfs {
    /// `root` is the data directory (made absolute here; it need not exist yet).
    pub fn new(root: PathBuf) -> LocalVfs {
        let root = std::path::absolute(&root).unwrap_or(root);
        LocalVfs { root }
    }

    /// The absolute data directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(self.root.join(normalize(path)?))
    }
}

#[derive(Debug)]
struct LocalFile {
    file: File,
}

impl VfsFile for LocalFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// Holds the open file; the OS releases the lock when it is closed.
#[derive(Debug)]
struct LocalLock {
    _file: File,
}

impl VfsLock for LocalLock {}

impl Vfs for LocalVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Arc<dyn VfsFile>> {
        let full = self.resolve(path)?;
        let mut opts = OpenOptions::new();
        match mode {
            OpenMode::ReadOnly => opts.read(true),
            OpenMode::ReadWrite => opts.read(true).write(true),
            OpenMode::CreateNew => opts.read(true).write(true).create_new(true),
        };
        opts.mode(FILE_MODE);
        Ok(Arc::new(LocalFile {
            file: opts.open(full)?,
        }))
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        fs::exists(self.resolve(path)?)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(self.resolve(path)?)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(self.resolve(from)?, self.resolve(to)?)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::DirBuilder::new()
            .mode(DIR_MODE)
            .create(self.resolve(path)?)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(self.resolve(path)?)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(self.resolve(path)?)
    }

    /// Returns the entries as paths relative to the root (`path` joined with
    /// the entry name), sorted by name.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let rel = normalize(path)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(&rel))? {
            out.push(rel.join(entry?.file_name()));
        }
        out.sort();
        Ok(out)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        File::open(self.resolve(path)?)?.sync_all()
    }

    /// Opens (creating if needed, never truncating) and locks `path`. Fails
    /// with `WouldBlock` if another open file description holds the lock.
    fn lock_file(&self, path: &Path) -> io::Result<Box<dyn VfsLock>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(self.resolve(path)?)?;
        match file.try_lock() {
            Ok(()) => Ok(Box::new(LocalLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yuzhu-localvfs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_roundtrip_and_dirs() {
        let root = tmp_root("rt");
        let vfs = LocalVfs::new(root.clone());
        vfs.create_dir(Path::new("a")).unwrap();
        vfs.create_dir_all(Path::new("a/b/c")).unwrap();
        let f = vfs.open(Path::new("a/f"), OpenMode::CreateNew).unwrap();
        assert!(vfs.open(Path::new("a/f"), OpenMode::CreateNew).is_err());
        f.write_all_at(b"hello", 3).unwrap();
        assert_eq!(f.size().unwrap(), 8);
        let mut buf = [9u8; 8];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"\0\0\0hello");
        assert_eq!(
            f.read_exact_at(&mut [0u8; 4], 6).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        f.set_len(2).unwrap();
        f.sync_data().unwrap();
        f.sync_all().unwrap();
        vfs.sync_dir(Path::new("a")).unwrap();
        assert!(vfs.exists(Path::new("a/f")).unwrap());
        assert_eq!(
            vfs.read_dir(Path::new("a")).unwrap(),
            vec![PathBuf::from("a/b"), PathBuf::from("a/f")]
        );
        vfs.rename(Path::new("a/f"), Path::new("a/g")).unwrap();
        assert!(!vfs.exists(Path::new("a/f")).unwrap());
        // An open handle survives unlink.
        let g = vfs.open(Path::new("a/g"), OpenMode::ReadWrite).unwrap();
        vfs.remove_file(Path::new("a/g")).unwrap();
        g.write_all_at(b"x", 0).unwrap();
        assert_eq!(
            vfs.open(Path::new("a/g"), OpenMode::ReadOnly)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let ro = vfs.open(Path::new("a/f2"), OpenMode::CreateNew).unwrap();
        drop(ro);
        let ro = vfs.open(Path::new("a/f2"), OpenMode::ReadOnly).unwrap();
        assert!(ro.write_all_at(b"x", 0).is_err());
        vfs.remove_dir_all(Path::new("a")).unwrap();
        assert!(!vfs.exists(Path::new("a")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_escaping_paths() {
        let root = tmp_root("esc");
        let vfs = LocalVfs::new(root.clone());
        for p in ["../x", "/etc/passwd", "a/../../x"] {
            assert_eq!(
                vfs.exists(Path::new(p)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{p}"
            );
        }
        assert!(vfs.exists(Path::new("./")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let root = tmp_root("lock");
        let vfs = LocalVfs::new(root.clone());
        let l1 = vfs.lock_file(Path::new("yuzhu.pid")).unwrap();
        assert_eq!(
            vfs.lock_file(Path::new("yuzhu.pid")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(l1);
        let _l2 = vfs.lock_file(Path::new("yuzhu.pid")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
