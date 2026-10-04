//! Where `TZif` files come from. All file access of the time zone code goes
//! through [`ZoneSource`], so tests can inject a fake zoneinfo tree (or
//! I/O failures) without touching the real file system.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// Read-only access to a zoneinfo directory tree. Paths are relative to the
/// tree's root, use `/` as separator, and `""` names the root itself.
pub trait ZoneSource: Send + Sync + fmt::Debug {
    /// Names of the entries of a directory.
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>>;
    /// Contents of a regular file.
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>>;
}

/// A zoneinfo tree on the local file system (e.g. `/usr/share/zoneinfo`).
#[derive(Debug, Clone)]
pub struct FsZoneSource {
    root: PathBuf,
}

impl FsZoneSource {
    /// Use the tree rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &str) -> io::Result<PathBuf> {
        // Callers only pass names found by listing directories, but refuse
        // anything that could escape the root regardless.
        if path.starts_with('/') || path.split('/').any(|c| c == ".." || c == ".") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad zone path"));
        }
        Ok(if path.is_empty() {
            self.root.clone()
        } else {
            self.root.join(path)
        })
    }
}

impl ZoneSource for FsZoneSource {
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let dir = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if let Ok(name) = entry.file_name().into_string() {
                out.push(name);
            }
        }
        Ok(out)
    }

    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.resolve(path)?)
    }
}

/// An in-memory zoneinfo tree (tests and fault injection).
#[derive(Debug, Clone, Default)]
pub struct MemZoneSource {
    files: BTreeMap<String, Vec<u8>>,
}

impl MemZoneSource {
    /// An empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file at `path` (directories are implied).
    pub fn insert(&mut self, path: impl Into<String>, data: Vec<u8>) {
        self.files.insert(path.into(), data);
    }
}

impl ZoneSource for MemZoneSource {
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut out: Vec<String> = Vec::new();
        for key in self.files.keys() {
            if let Some(rest) = key.strip_prefix(&prefix) {
                let first = rest.split('/').next().unwrap_or("");
                if !first.is_empty() && !out.iter().any(|e| e == first) {
                    out.push(first.to_owned());
                }
            }
        }
        if out.is_empty() && !path.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"));
        }
        Ok(out)
    }

    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }
}
