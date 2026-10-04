//! Storage manager: relation files, segments, forks (`m2.md` §4.3, §6.2).
//!
//! One relation fork is a sequence of segment files of `rel_seg_blocks`
//! blocks each. The block count is cached in memory (one process, so it is
//! always right). File handles are kept open and shared (`Arc<dyn
//! VfsFile>`); I/O never happens under the `rels` lock.
//!
//! Lock order (all leaves): `rels` -> `RelEntry::segs` / `pending_*`;
//! `RelEntry::ext` is taken before `segs`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::BLCKSZ;
use super::vfs::{OpenMode, Vfs, VfsFile};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::types::Oid;
use crate::util::sync::lock;

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

const ALL_FORKS: [ForkNumber; 4] = [
    ForkNumber::Main,
    ForkNumber::Fsm,
    ForkNumber::VisibilityMap,
    ForkNumber::Init,
];

pub type BlockNumber = u32;
pub const INVALID_BLOCK_NUMBER: BlockNumber = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BufferTag {
    pub rel: RelFileLocator,
    pub fork: ForkNumber,
    pub block: BlockNumber,
}

/// Path relative to the data directory, e.g. `base/5/16384.1`
/// (PostgreSQL's `relpath()`; the fork suffix comes before the segment
/// number). `debug_assert`s that a relation in the global tablespace has
/// `db_oid == 0`.
pub fn relpath(rel: RelFileLocator, fork: ForkNumber, segno: u32) -> PathBuf {
    let mut name = match rel.spc_oid {
        GLOBALTABLESPACE_OID => {
            debug_assert_eq!(rel.db_oid, 0, "shared relations have db_oid 0");
            format!("global/{}", rel.rel_number.0)
        }
        _ => format!("base/{}/{}", rel.db_oid, rel.rel_number.0),
    };
    match fork {
        ForkNumber::Main => {}
        ForkNumber::Fsm => name.push_str("_fsm"),
        ForkNumber::VisibilityMap => name.push_str("_vm"),
        ForkNumber::Init => name.push_str("_init"),
    }
    if segno > 0 {
        name.push('.');
        name.push_str(&segno.to_string());
    }
    PathBuf::from(name)
}

type SyncKey = (RelFileLocator, ForkNumber, u32);
type RelKey = (RelFileLocator, ForkNumber);

/// In-memory state of one relation fork.
#[derive(Debug)]
struct RelEntry {
    nblocks: AtomicU32,
    /// Open segment files, index = segment number. Always contiguous.
    segs: Mutex<Vec<Arc<dyn VfsFile>>>,
    /// The relation extension lock; held across the whole `extend`.
    ext: Mutex<()>,
}

#[derive(Debug)]
pub struct StorageManager {
    vfs: Arc<dyn Vfs>,
    rel_seg_blocks: u32,
    rels: Mutex<HashMap<RelKey, Arc<RelEntry>>>,
    pending_sync: Mutex<HashMap<SyncKey, Arc<dyn VfsFile>>>,
    pending_unlink: Mutex<Vec<RelFileLocator>>,
    warnings: Mutex<Vec<String>>,
    recovery: AtomicBool,
    broken: AtomicBool,
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn io_error(e: &io::Error, what: &str, path: &Path) -> Error {
    let err = Error::from_io(e, format!("{what} \"{}\"", display(path)));
    if e.kind() == io::ErrorKind::StorageFull {
        err.with_hint("Check free disk space.")
    } else {
        err
    }
}

impl StorageManager {
    pub fn new(vfs: Arc<dyn Vfs>, rel_seg_blocks: u32) -> Self {
        assert!(rel_seg_blocks > 0, "rel_seg_blocks must be positive");
        StorageManager {
            vfs,
            rel_seg_blocks,
            rels: Mutex::new(HashMap::new()),
            pending_sync: Mutex::new(HashMap::new()),
            pending_unlink: Mutex::new(Vec::new()),
            warnings: Mutex::new(Vec::new()),
            recovery: AtomicBool::new(false),
            broken: AtomicBool::new(false),
        }
    }

    pub fn rel_seg_blocks(&self) -> u32 {
        self.rel_seg_blocks
    }

    /// Drains the WARNING messages produced while opening relations (e.g. a
    /// truncated partial block at the end of a file).
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default()
    }

    fn seg_bytes(&self) -> u64 {
        u64::from(self.rel_seg_blocks) * BLCKSZ as u64
    }

    fn check_writable(&self) -> Result<()> {
        if self.is_broken() {
            return Err(Error::new(
                sqlstate::IO_ERROR,
                "refusing to write: a previous fsync failed",
            )
            .with_severity(Severity::Panic));
        }
        Ok(())
    }

    fn fsync_failed(&self, e: &io::Error, path: &Path) -> Error {
        self.broken.store(true, Ordering::SeqCst);
        Error::new(
            sqlstate::IO_ERROR,
            format!("could not fsync file \"{}\": {e}", display(path)),
        )
        .with_severity(Severity::Panic)
    }

    fn parent_dir(path: &Path) -> &Path {
        path.parent().unwrap_or_else(|| Path::new(""))
    }

    fn sync_parent(&self, path: &Path) -> Result<()> {
        let dir = Self::parent_dir(path);
        self.vfs.sync_dir(dir).map_err(|e| {
            Error::from_io(
                &e,
                format!("could not fsync directory \"{}\"", display(dir)),
            )
        })
    }

    /// Returns the cached entry or opens the relation's files.
    fn entry(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<Arc<RelEntry>> {
        if let Some(e) = lock(&self.rels)?.get(&(rel, fork)) {
            return Ok(Arc::clone(e));
        }
        let opened = Arc::new(self.open_entry(rel, fork)?);
        let mut rels = lock(&self.rels)?;
        Ok(Arc::clone(rels.entry((rel, fork)).or_insert(opened)))
    }

    fn open_entry(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<RelEntry> {
        let first = relpath(rel, fork, 0);
        let exists = self
            .vfs
            .exists(&first)
            .map_err(|e| io_error(&e, "could not open file", &first))?;
        if !exists {
            return Err(Error::new(
                sqlstate::UNDEFINED_FILE,
                format!(
                    "could not open file \"{}\": No such file or directory",
                    display(&first)
                ),
            ));
        }
        let seg_bytes = self.seg_bytes();
        let recovery = self.recovery.load(Ordering::SeqCst);
        let mut segs: Vec<Arc<dyn VfsFile>> = Vec::new();
        let mut sizes: Vec<u64> = Vec::new();
        for segno in 0u32.. {
            let path = relpath(rel, fork, segno);
            if segno > 0
                && !self
                    .vfs
                    .exists(&path)
                    .map_err(|e| io_error(&e, "could not open file", &path))?
            {
                break;
            }
            let file = self
                .vfs
                .open(&path, OpenMode::ReadWrite)
                .map_err(|e| io_error(&e, "could not open file", &path))?;
            sizes.push(
                file.size()
                    .map_err(|e| io_error(&e, "could not stat file", &path))?,
            );
            segs.push(file);
        }
        let last = segs.len() - 1;
        for k in 0..last {
            if sizes[k] != seg_bytes {
                let path = relpath(rel, fork, segno_of(k));
                if !recovery {
                    return Err(Error::corrupted(format!(
                        "segment {k} of relation file \"{}\" has {} bytes, expected {seg_bytes}",
                        display(&path),
                        sizes[k]
                    )));
                }
                segs[k]
                    .set_len(seg_bytes)
                    .map_err(|e| io_error(&e, "could not extend file", &path))?;
                sizes[k] = seg_bytes;
            }
        }
        let last_path = relpath(rel, fork, segno_of(last));
        if sizes[last] > seg_bytes {
            return Err(Error::corrupted(format!(
                "file \"{}\" has {} bytes, more than a segment ({seg_bytes})",
                display(&last_path),
                sizes[last]
            )));
        }
        let tail = sizes[last] % BLCKSZ as u64;
        if tail != 0 {
            let msg = format!(
                "file \"{}\" has a partial block at the end ({tail} bytes); ignoring it",
                display(&last_path)
            );
            if let Ok(mut w) = self.warnings.lock() {
                w.push(msg);
            }
        }
        let blocks = (last as u64) * u64::from(self.rel_seg_blocks) + sizes[last] / BLCKSZ as u64;
        let nblocks = u32::try_from(blocks).map_err(|_| {
            Error::corrupted(format!(
                "relation file \"{}\" is too large",
                display(&first)
            ))
        })?;
        Ok(RelEntry {
            nblocks: AtomicU32::new(nblocks),
            segs: Mutex::new(segs),
            ext: Mutex::new(()),
        })
    }

    fn segment(entry: &RelEntry, segno: u32, path: &Path) -> Result<Arc<dyn VfsFile>> {
        let segs = lock(&entry.segs)?;
        segs.get(segno as usize).cloned().ok_or_else(|| {
            Error::internal(format!(
                "segment {segno} of \"{}\" is not open",
                display(path)
            ))
        })
    }

    fn locate(&self, block: BlockNumber) -> (u32, u64) {
        (
            block / self.rel_seg_blocks,
            u64::from(block % self.rel_seg_blocks) * BLCKSZ as u64,
        )
    }

    fn register_sync(&self, key: SyncKey, file: &Arc<dyn VfsFile>) -> Result<()> {
        lock(&self.pending_sync)?
            .entry(key)
            .or_insert_with(|| Arc::clone(file));
        Ok(())
    }

    /// Internal error if the file already exists (including a D13 leftover).
    pub fn create(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<()> {
        self.check_writable()?;
        let path = relpath(rel, fork, 0);
        let exists = self
            .vfs
            .exists(&path)
            .map_err(|e| io_error(&e, "could not create file", &path))?;
        if exists {
            return Err(Error::internal(format!(
                "could not create file \"{}\": File exists",
                display(&path)
            )));
        }
        let dir = Self::parent_dir(&path);
        // Directories that do not exist yet (outermost last), so that their
        // creation can be made durable through their parents.
        let mut missing = Vec::new();
        for d in dir.ancestors().take_while(|d| !d.as_os_str().is_empty()) {
            if self
                .vfs
                .exists(d)
                .map_err(|e| io_error(&e, "could not stat directory", d))?
            {
                break;
            }
            missing.push(d);
        }
        self.vfs
            .create_dir_all(dir)
            .map_err(|e| io_error(&e, "could not create directory", dir))?;
        for d in missing.into_iter().rev() {
            let parent = Self::parent_dir(d);
            self.vfs.sync_dir(parent).map_err(|e| {
                Error::from_io(
                    &e,
                    format!("could not fsync directory \"{}\"", display(parent)),
                )
            })?;
        }
        let file = self
            .vfs
            .open(&path, OpenMode::CreateNew)
            .map_err(|e| io_error(&e, "could not create file", &path))?;
        self.sync_parent(&path)?;
        let entry = Arc::new(RelEntry {
            nblocks: AtomicU32::new(0),
            segs: Mutex::new(vec![file]),
            ext: Mutex::new(()),
        });
        lock(&self.rels)?.insert((rel, fork), entry);
        Ok(())
    }

    /// Whether the first segment exists (a 0-byte leftover counts).
    pub fn exists(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<bool> {
        let path = relpath(rel, fork, 0);
        self.vfs
            .exists(&path)
            .map_err(|e| io_error(&e, "could not stat file", &path))
    }

    pub fn nblocks(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<BlockNumber> {
        Ok(self.entry(rel, fork)?.nblocks.load(Ordering::Acquire))
    }

    pub fn read_block(&self, tag: BufferTag, buf: &mut [u8; BLCKSZ]) -> Result<()> {
        let entry = self.entry(tag.rel, tag.fork)?;
        let (segno, off) = self.locate(tag.block);
        let path = relpath(tag.rel, tag.fork, segno);
        let n = entry.nblocks.load(Ordering::Acquire);
        if tag.block >= n {
            return Err(short_read(tag.block, &path, 0));
        }
        let file = Self::segment(&entry, segno, &path)?;
        match file.read_exact_at(buf, off) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                let got = file
                    .size()
                    .unwrap_or(0)
                    .saturating_sub(off)
                    .min(BLCKSZ as u64);
                Err(short_read(tag.block, &path, got))
            }
            Err(e) => Err(Error::from_io(
                &e,
                format!(
                    "could not read blocks {0}..{0} in file \"{1}\"",
                    tag.block,
                    display(&path)
                ),
            )),
        }
    }

    pub fn write_block(&self, tag: BufferTag, buf: &[u8; BLCKSZ]) -> Result<()> {
        self.check_writable()?;
        let entry = self.entry(tag.rel, tag.fork)?;
        let (segno, off) = self.locate(tag.block);
        let path = relpath(tag.rel, tag.fork, segno);
        if tag.block >= entry.nblocks.load(Ordering::Acquire) {
            return Err(Error::internal(format!(
                "could not write block {} in file \"{}\": beyond the end of the relation",
                tag.block,
                display(&path)
            )));
        }
        let file = Self::segment(&entry, segno, &path)?;
        file.write_all_at(buf, off).map_err(|e| {
            Error::from_io(
                &e,
                format!(
                    "could not write block {} in file \"{}\"",
                    tag.block,
                    display(&path)
                ),
            )
        })?;
        self.register_sync((tag.rel, tag.fork, segno), &file)
    }

    /// Appends one zero-filled block and returns its number. Takes the
    /// per-relation extension lock internally; only `BufferPool::extend`
    /// calls this.
    pub fn extend(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<BlockNumber> {
        self.check_writable()?;
        let entry = self.entry(rel, fork)?;
        let _ext = lock(&entry.ext)?;
        self.extend_locked(&entry, rel, fork)
    }

    fn extend_locked(
        &self,
        entry: &RelEntry,
        rel: RelFileLocator,
        fork: ForkNumber,
    ) -> Result<BlockNumber> {
        let n = entry.nblocks.load(Ordering::Acquire);
        let (segno, off) = self.locate(n);
        let path = relpath(rel, fork, segno);
        if n == INVALID_BLOCK_NUMBER {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "cannot extend file \"{}\" beyond {INVALID_BLOCK_NUMBER} blocks",
                    display(&path)
                ),
            ));
        }
        let mut created = false;
        let file = {
            let mut segs = lock(&entry.segs)?;
            if let Some(f) = segs.get(segno as usize) {
                Arc::clone(f)
            } else {
                let f = match self.vfs.open(&path, OpenMode::CreateNew) {
                    Ok(f) => f,
                    // A leftover from a crash (recovery) or a failed earlier extend.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self
                        .vfs
                        .open(&path, OpenMode::ReadWrite)
                        .map_err(|e| io_error(&e, "could not open file", &path))?,
                    Err(e) => return Err(io_error(&e, "could not create file", &path)),
                };
                created = true;
                segs.push(Arc::clone(&f));
                f
            }
        };
        let zero = [0u8; BLCKSZ];
        if let Err(e) = file.write_all_at(&zero, off) {
            // Put the length back (best effort), so that a failed extend (ENOSPC) leaves no partial block.
            let _ = file.set_len(off);
            return Err(io_error(&e, "could not extend file", &path));
        }
        entry.nblocks.store(n + 1, Ordering::Release);
        self.register_sync((rel, fork, segno), &file)?;
        if created {
            self.sync_parent(&path)?;
        }
        Ok(n)
    }

    /// Zero-fills up to and including `blk` (M3 REDO; unused in M2).
    pub fn extend_to(&self, rel: RelFileLocator, fork: ForkNumber, blk: BlockNumber) -> Result<()> {
        self.check_writable()?;
        let entry = self.entry(rel, fork)?;
        let _ext = lock(&entry.ext)?;
        while entry.nblocks.load(Ordering::Acquire) <= blk {
            self.extend_locked(&entry, rel, fork)?;
        }
        Ok(())
    }

    /// D13: removes later segments and non-main forks, truncates the first
    /// segment to 0 bytes and queues it for removal at the next checkpoint.
    pub fn unlink(&self, rel: RelFileLocator) -> Result<()> {
        self.check_writable()?;
        {
            let mut rels = lock(&self.rels)?;
            rels.retain(|(r, _), _| *r != rel);
        }
        lock(&self.pending_sync)?.retain(|(r, _, _), _| *r != rel);
        let mut queue = false;
        for fork in ALL_FORKS {
            let first = relpath(rel, fork, 0);
            let first_exists = self
                .vfs
                .exists(&first)
                .map_err(|e| io_error(&e, "could not stat file", &first))?;
            if !first_exists {
                continue;
            }
            let mut last = 0u32;
            while self
                .vfs
                .exists(&relpath(rel, fork, last + 1))
                .map_err(|e| io_error(&e, "could not stat file", &first))?
            {
                last += 1;
            }
            for segno in (1..=last).rev() {
                let path = relpath(rel, fork, segno);
                self.vfs
                    .remove_file(&path)
                    .map_err(|e| io_error(&e, "could not remove file", &path))?;
            }
            if fork == ForkNumber::Main {
                let file = self
                    .vfs
                    .open(&first, OpenMode::ReadWrite)
                    .map_err(|e| io_error(&e, "could not open file", &first))?;
                file.set_len(0)
                    .map_err(|e| io_error(&e, "could not truncate file", &first))?;
                file.sync_data()
                    .map_err(|e| self.fsync_failed(&e, &first))?;
                queue = true;
            } else {
                self.vfs
                    .remove_file(&first)
                    .map_err(|e| io_error(&e, "could not remove file", &first))?;
            }
        }
        self.sync_parent(&relpath(rel, ForkNumber::Main, 0))?;
        if queue {
            let mut q = lock(&self.pending_unlink)?;
            if !q.contains(&rel) {
                q.push(rel);
            }
        }
        Ok(())
    }

    pub fn immedsync(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<()> {
        let entry = self.entry(rel, fork)?;
        let segs: Vec<_> = lock(&entry.segs)?.clone();
        for (segno, file) in segs.iter().enumerate() {
            file.sync_data()
                .map_err(|e| self.fsync_failed(&e, &relpath(rel, fork, segno_of(segno))))?;
        }
        Ok(())
    }

    /// Checkpoint: fsyncs every segment written since the last call. The
    /// set is taken out whole; segments written afterwards are picked up by
    /// the next call. A failure is a Panic and is never retried.
    pub fn sync_pending(&self) -> Result<()> {
        let taken = std::mem::take(&mut *lock(&self.pending_sync)?);
        let mut keys: Vec<_> = taken.keys().copied().collect();
        keys.sort();
        for key in keys {
            let (rel, fork, segno) = key;
            let file = &taken[&key];
            file.sync_data()
                .map_err(|e| self.fsync_failed(&e, &relpath(rel, fork, segno)))?;
        }
        Ok(())
    }

    /// End of a checkpoint: removes the leftovers queued by `unlink`.
    pub fn finish_pending_unlinks(&self) -> Result<()> {
        let mut queue = std::mem::take(&mut *lock(&self.pending_unlink)?);
        while let Some(&rel) = queue.last() {
            let path = relpath(rel, ForkNumber::Main, 0);
            let result = match self.vfs.remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_error(&e, "could not remove file", &path)),
            }
            .and_then(|()| self.sync_parent(&path));
            if let Err(e) = result {
                // Keep the rest (including this one) for the next checkpoint.
                lock(&self.pending_unlink)?.extend(queue);
                return Err(e);
            }
            queue.pop();
        }
        Ok(())
    }

    /// M3 recovery only; always off in M2.
    pub fn set_recovery_mode(&self, on: bool) {
        self.recovery.store(on, Ordering::SeqCst);
    }

    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }
}

/// Segment index to segment number (a relation never has 2^32 segments).
fn segno_of(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

fn short_read(block: BlockNumber, path: &Path, got: u64) -> Error {
    Error::new(
        sqlstate::IO_ERROR,
        format!(
            "could not read blocks {block}..{block} in file \"{}\": read only {got} of {BLCKSZ} bytes",
            display(path)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    fn tag(r: RelFileLocator, block: u32) -> BufferTag {
        BufferTag {
            rel: r,
            fork: ForkNumber::Main,
            block,
        }
    }

    fn page(b: u8) -> [u8; BLCKSZ] {
        [b; BLCKSZ]
    }

    fn setup(seg_blocks: u32) -> (SimVfs, StorageManager) {
        let vfs = SimVfs::new(7);
        let mgr = StorageManager::new(Arc::new(vfs.clone()), seg_blocks);
        (vfs, mgr)
    }

    fn fault(op: FaultOp, nth: u64, effect: FaultEffect) -> FaultPlan {
        FaultPlan {
            rules: vec![FaultRule {
                op,
                path_prefix: None,
                nth: Some(nth),
                probability: None,
                effect,
            }],
        }
    }

    fn file_len(vfs: &SimVfs, path: &str) -> Option<usize> {
        vfs.file_contents(Path::new(path)).map(|c| c.len())
    }

    #[test]
    fn relpath_follows_postgresql() {
        let shared = RelFileLocator {
            spc_oid: GLOBALTABLESPACE_OID,
            db_oid: 0,
            rel_number: RelFileNumber(1262),
        };
        assert_eq!(
            relpath(shared, ForkNumber::Main, 0),
            PathBuf::from("global/1262")
        );
        let r = rel(16384);
        assert_eq!(
            relpath(r, ForkNumber::Main, 0),
            PathBuf::from("base/5/16384")
        );
        assert_eq!(
            relpath(r, ForkNumber::Main, 1),
            PathBuf::from("base/5/16384.1")
        );
        assert_eq!(
            relpath(r, ForkNumber::Fsm, 0),
            PathBuf::from("base/5/16384_fsm")
        );
        assert_eq!(
            relpath(r, ForkNumber::VisibilityMap, 2),
            PathBuf::from("base/5/16384_vm.2")
        );
        assert_eq!(
            relpath(r, ForkNumber::Init, 0),
            PathBuf::from("base/5/16384_init")
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "db_oid 0")]
    fn relpath_rejects_a_global_relation_with_a_database() {
        let bad = RelFileLocator {
            spc_oid: GLOBALTABLESPACE_OID,
            db_oid: 5,
            rel_number: RelFileNumber(1262),
        };
        let _ = relpath(bad, ForkNumber::Main, 0);
    }

    #[test]
    fn create_extend_write_read_roundtrip() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(100);
        assert!(!mgr.exists(r, ForkNumber::Main).unwrap());
        mgr.create(r, ForkNumber::Main).unwrap();
        assert!(mgr.exists(r, ForkNumber::Main).unwrap());
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 0);
        assert_eq!(mgr.extend(r, ForkNumber::Main).unwrap(), 0);
        assert_eq!(mgr.extend(r, ForkNumber::Main).unwrap(), 1);
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 2);
        let mut buf = page(1);
        mgr.read_block(tag(r, 1), &mut buf).unwrap();
        assert_eq!(buf, page(0), "an extended block is zero-filled");
        mgr.write_block(tag(r, 1), &page(0xAB)).unwrap();
        mgr.read_block(tag(r, 1), &mut buf).unwrap();
        assert_eq!(buf, page(0xAB));
        mgr.read_block(tag(r, 0), &mut buf).unwrap();
        assert_eq!(buf, page(0));
    }

    const DEFAULT_SEG: u32 = 131_072;

    #[test]
    fn create_twice_is_an_internal_error() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(1);
        mgr.create(r, ForkNumber::Main).unwrap();
        let e = mgr.create(r, ForkNumber::Main).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn read_past_the_end_reports_a_short_read() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(1);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        let mut buf = page(0);
        let e = mgr.read_block(tag(r, 1), &mut buf).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
        assert_eq!(
            e.message,
            "could not read blocks 1..1 in file \"base/5/1\": read only 0 of 8192 bytes"
        );
    }

    #[test]
    fn write_beyond_the_end_is_refused() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(1);
        mgr.create(r, ForkNumber::Main).unwrap();
        let e = mgr.write_block(tag(r, 0), &page(1)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn opening_a_missing_relation_is_58p01() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let e = mgr.nblocks(rel(9), ForkNumber::Main).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FILE);
        let mut buf = page(0);
        let e = mgr.read_block(tag(rel(9), 0), &mut buf).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FILE);
    }

    #[test]
    fn segments_split_at_the_boundary_and_survive_reopening() {
        let (vfs, mgr) = setup(4);
        let r = rel(200);
        mgr.create(r, ForkNumber::Main).unwrap();
        for i in 0..10u32 {
            assert_eq!(mgr.extend(r, ForkNumber::Main).unwrap(), i);
            mgr.write_block(tag(r, i), &page(u8::try_from(i).unwrap() + 1))
                .unwrap();
        }
        assert_eq!(file_len(&vfs, "base/5/200"), Some(4 * BLCKSZ));
        assert_eq!(file_len(&vfs, "base/5/200.1"), Some(4 * BLCKSZ));
        assert_eq!(file_len(&vfs, "base/5/200.2"), Some(2 * BLCKSZ));
        assert_eq!(file_len(&vfs, "base/5/200.3"), None);
        let mut buf = page(0);
        for i in 0..10u32 {
            mgr.read_block(tag(r, i), &mut buf).unwrap();
            assert_eq!(buf, page(u8::try_from(i).unwrap() + 1), "block {i}");
        }
        // A fresh manager finds the same block count and contents.
        let mgr2 = StorageManager::new(Arc::new(vfs.clone()), 4);
        assert_eq!(mgr2.nblocks(r, ForkNumber::Main).unwrap(), 10);
        mgr2.read_block(tag(r, 9), &mut buf).unwrap();
        assert_eq!(buf, page(10));
        // Filling the last segment exactly, then extending, opens a new one.
        mgr2.extend(r, ForkNumber::Main).unwrap();
        mgr2.extend(r, ForkNumber::Main).unwrap();
        assert_eq!(mgr2.extend(r, ForkNumber::Main).unwrap(), 12);
        assert_eq!(file_len(&vfs, "base/5/200.3"), Some(BLCKSZ));
    }

    #[test]
    fn partial_block_at_the_end_is_ignored_with_a_warning() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(3);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        let f = vfs
            .open(Path::new("base/5/3"), OpenMode::ReadWrite)
            .unwrap();
        f.write_all_at(&[1u8; 100], BLCKSZ as u64).unwrap();
        let mgr2 = StorageManager::new(Arc::new(vfs.clone()), DEFAULT_SEG);
        assert_eq!(mgr2.nblocks(r, ForkNumber::Main).unwrap(), 1);
        let w = mgr2.take_warnings();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("partial block"));
        assert!(mgr2.take_warnings().is_empty());
    }

    #[test]
    fn short_middle_segment_is_corruption_unless_recovering() {
        let (vfs, mgr) = setup(4);
        let r = rel(4);
        mgr.create(r, ForkNumber::Main).unwrap();
        for _ in 0..6 {
            mgr.extend(r, ForkNumber::Main).unwrap();
        }
        // Cut segment 0 to 3 blocks while segment 1 exists.
        let f = vfs
            .open(Path::new("base/5/4"), OpenMode::ReadWrite)
            .unwrap();
        f.set_len(3 * BLCKSZ as u64).unwrap();

        let strict = StorageManager::new(Arc::new(vfs.clone()), 4);
        let e = strict.nblocks(r, ForkNumber::Main).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);

        let lenient = StorageManager::new(Arc::new(vfs.clone()), 4);
        lenient.set_recovery_mode(true);
        assert_eq!(lenient.nblocks(r, ForkNumber::Main).unwrap(), 6);
        assert_eq!(file_len(&vfs, "base/5/4"), Some(4 * BLCKSZ));
    }

    #[test]
    fn extend_to_zero_fills_up_to_the_block() {
        let (_vfs, mgr) = setup(4);
        let r = rel(5);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend_to(r, ForkNumber::Main, 5).unwrap();
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 6);
        mgr.extend_to(r, ForkNumber::Main, 2).unwrap();
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 6);
    }

    #[test]
    fn unlink_keeps_a_zero_byte_leftover_until_the_checkpoint() {
        let (vfs, mgr) = setup(2);
        let r = rel(6);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.create(r, ForkNumber::Fsm).unwrap();
        for _ in 0..5 {
            mgr.extend(r, ForkNumber::Main).unwrap();
        }
        mgr.extend(r, ForkNumber::Fsm).unwrap();
        mgr.unlink(r).unwrap();
        // D13: later segments and other forks are gone; the first segment
        // is truncated and still "exists".
        assert_eq!(file_len(&vfs, "base/5/6"), Some(0));
        assert_eq!(file_len(&vfs, "base/5/6.1"), None);
        assert_eq!(file_len(&vfs, "base/5/6.2"), None);
        assert_eq!(file_len(&vfs, "base/5/6_fsm"), None);
        assert!(mgr.exists(r, ForkNumber::Main).unwrap());
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 0);
        assert!(
            mgr.create(r, ForkNumber::Main).is_err(),
            "no reuse before the checkpoint"
        );
        mgr.unlink(r).unwrap();
        mgr.finish_pending_unlinks().unwrap();
        assert!(!mgr.exists(r, ForkNumber::Main).unwrap());
        mgr.create(r, ForkNumber::Main).unwrap();
        // Nothing left to remove: the new file must not be touched.
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.finish_pending_unlinks().unwrap();
        assert!(mgr.exists(r, ForkNumber::Main).unwrap());
        // Unlinking something that does not exist is fine.
        mgr.unlink(rel(777)).unwrap();
        mgr.finish_pending_unlinks().unwrap();
    }

    #[test]
    fn unlink_is_durable_across_a_crash() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(8);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.write_block(tag(r, 0), &page(9)).unwrap();
        mgr.sync_pending().unwrap();
        mgr.unlink(r).unwrap();
        let after = vfs.crash(CrashMode::DropUnsynced);
        let mgr2 = StorageManager::new(Arc::new(after), DEFAULT_SEG);
        assert!(mgr2.exists(r, ForkNumber::Main).unwrap());
        assert_eq!(mgr2.nblocks(r, ForkNumber::Main).unwrap(), 0);
    }

    #[test]
    fn data_is_lost_in_a_crash_until_sync_pending() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(10);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.write_block(tag(r, 0), &page(5)).unwrap();
        let lost = vfs.crash(CrashMode::DropUnsynced);
        let m = StorageManager::new(Arc::new(lost), DEFAULT_SEG);
        assert_eq!(m.nblocks(r, ForkNumber::Main).unwrap(), 0);

        // The same sequence with sync_pending keeps the block.
        let (vfs, mgr) = setup(DEFAULT_SEG);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.write_block(tag(r, 0), &page(5)).unwrap();
        mgr.sync_pending().unwrap();
        assert!(vfs.stats().syncs > 0);
        // Nothing new was written: the second call has nothing to do.
        let before = vfs.stats().syncs;
        mgr.sync_pending().unwrap();
        assert_eq!(vfs.stats().syncs, before);
        let kept = vfs.crash(CrashMode::DropUnsynced);
        let m = StorageManager::new(Arc::new(kept), DEFAULT_SEG);
        let mut buf = page(0);
        m.read_block(tag(r, 0), &mut buf).unwrap();
        assert_eq!(buf, page(5));
    }

    #[test]
    fn sync_pending_ignores_unlinked_relations() {
        let (_vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(11);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.write_block(tag(r, 0), &page(1)).unwrap();
        mgr.unlink(r).unwrap();
        mgr.sync_pending().unwrap();
        mgr.immedsync(r, ForkNumber::Main).unwrap();
    }

    #[test]
    fn fsync_failure_is_a_sticky_panic() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(12);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        mgr.write_block(tag(r, 0), &page(1)).unwrap();
        vfs.set_faults(fault(
            FaultOp::Sync,
            1,
            FaultEffect::Error(io::ErrorKind::Other),
        ));
        let e = mgr.sync_pending().unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
        assert!(
            e.message.starts_with("could not fsync file \"base/5/12\""),
            "{}",
            e.message
        );
        assert!(mgr.is_broken());
        // Not retried: the set was taken, a second call has nothing to sync.
        mgr.sync_pending().unwrap();
        // And no more writes.
        for e in [
            mgr.write_block(tag(r, 0), &page(2)).unwrap_err(),
            mgr.extend(r, ForkNumber::Main).unwrap_err(),
            mgr.create(rel(13), ForkNumber::Main).unwrap_err(),
        ] {
            assert_eq!(e.severity, Severity::Panic);
        }
    }

    #[test]
    fn write_error_is_58030_and_keeps_the_manager_usable() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(14);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        vfs.set_faults(fault(
            FaultOp::Write,
            1,
            FaultEffect::Error(io::ErrorKind::Other),
        ));
        let e = mgr.write_block(tag(r, 0), &page(1)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
        assert_eq!(e.severity, Severity::Error);
        assert!(
            e.message
                .starts_with("could not write block 0 in file \"base/5/14\"")
        );
        assert!(!mgr.is_broken());
        mgr.write_block(tag(r, 0), &page(1)).unwrap();
    }

    #[test]
    fn failed_extend_restores_the_file_length() {
        let (vfs, mgr) = setup(DEFAULT_SEG);
        let r = rel(15);
        mgr.create(r, ForkNumber::Main).unwrap();
        mgr.extend(r, ForkNumber::Main).unwrap();
        // The 2nd write is the extension; half of it reaches the file and
        // the error is ENOSPC.
        vfs.set_faults(fault(FaultOp::Write, 1, FaultEffect::ShortWrite));
        let e = mgr.extend(r, ForkNumber::Main).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DISK_FULL);
        assert_eq!(e.hint.as_deref(), Some("Check free disk space."));
        assert!(e.message.starts_with("could not extend file \"base/5/15\""));
        assert_eq!(file_len(&vfs, "base/5/15"), Some(BLCKSZ));
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 1);
        assert_eq!(mgr.extend(r, ForkNumber::Main).unwrap(), 1);
        assert_eq!(file_len(&vfs, "base/5/15"), Some(2 * BLCKSZ));
    }

    #[test]
    fn concurrent_extends_hand_out_distinct_blocks() {
        let vfs = SimVfs::new(3);
        let mgr = Arc::new(StorageManager::new(Arc::new(vfs), 8));
        let r = rel(16);
        mgr.create(r, ForkNumber::Main).unwrap();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let m = Arc::clone(&mgr);
            handles.push(std::thread::spawn(move || {
                (0..25)
                    .map(|_| m.extend(r, ForkNumber::Main).unwrap())
                    .collect::<Vec<_>>()
            }));
        }
        let mut all: Vec<u32> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (0..100).collect::<Vec<_>>());
        assert_eq!(mgr.nblocks(r, ForkNumber::Main).unwrap(), 100);
    }
}
