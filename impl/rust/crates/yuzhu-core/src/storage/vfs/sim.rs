//! `SimVfs`: an in-memory file system with fault injection and crash
//! simulation (`m2.md` §4.3). Always built (not `cfg(test)`) because the
//! server's integration tests use it too.
//!
//! Model:
//! - Every file is an inode with `current` (what reads see), `durable` (the
//!   content as of the last `sync_data` / `sync_all`) and the byte ranges
//!   written since then (`unsynced`).
//! - The namespace has a `live` view and a `durable` view. Creating,
//!   removing and renaming only change the live view; `sync_dir(parent)`
//!   copies the entries directly under `parent` to the durable view.
//! - `crash` throws the live state away according to [`CrashMode`] and
//!   returns a new instance over the same disk. The old instance and every
//!   handle / lock obtained from it fail with EIO afterwards. Fault rules and
//!   locks are reset by a crash (a new "boot"); statistics are kept.
//! - Unlinked-but-open files stay readable and writable (Linux semantics).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::{OpenMode, Vfs, VfsFile, VfsLock, normalize};

/// Deterministic RNG (`SplitMix64`).
#[derive(Debug)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Debug, Default)]
struct Inode {
    current: Vec<u8>,
    durable: Vec<u8>,
    /// Ranges `(offset, len)` written since the last sync.
    unsynced: Vec<(u64, u64)>,
    /// The length changed since the last sync.
    len_dirty: bool,
}

impl Inode {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let off = usize::try_from(offset).map_err(|_| io::Error::other("offset too large"))?;
        let end = off + buf.len();
        if end > self.current.len() {
            self.current.resize(end, 0);
            self.len_dirty = true;
        }
        self.current[off..end].copy_from_slice(buf);
        self.unsynced.push((offset, buf.len() as u64));
        Ok(())
    }

    fn set_len(&mut self, len: usize) {
        let old = self.current.len();
        self.current.resize(len, 0);
        if len > old {
            self.unsynced.push((old as u64, (len - old) as u64));
        }
        self.len_dirty = true;
    }

    fn sync(&mut self) {
        let cur_len = self.current.len();
        let target = if self.len_dirty {
            cur_len
        } else {
            self.durable.len().min(cur_len)
        };
        self.durable.resize(target, 0);
        for (o, l) in self.unsynced.drain(..) {
            let s = usize::try_from(o).unwrap_or(usize::MAX).min(target);
            let e = usize::try_from(o + l).unwrap_or(usize::MAX).min(target);
            self.durable[s..e].copy_from_slice(&self.current[s..e]);
        }
        self.len_dirty = false;
    }
}

#[derive(Debug)]
struct SimState {
    /// Incremented by every crash; handles of older epochs are dead.
    epoch: u64,
    next_ino: u64,
    inodes: BTreeMap<u64, Inode>,
    live_files: BTreeMap<PathBuf, u64>,
    live_dirs: BTreeSet<PathBuf>,
    durable_files: BTreeMap<PathBuf, u64>,
    durable_dirs: BTreeSet<PathBuf>,
    locks: BTreeSet<PathBuf>,
    /// Rules with the number of operations that matched them so far.
    faults: Vec<(FaultRule, u64)>,
    rng: SplitMix64,
    stats: SimStats,
}

fn eio() -> io::Error {
    io::Error::from_raw_os_error(5)
}

fn injected(kind: io::ErrorKind) -> io::Error {
    io::Error::new(kind, "injected fault")
}

fn parent_of(p: &Path) -> PathBuf {
    p.parent().map(Path::to_path_buf).unwrap_or_default()
}

impl SimState {
    fn dir_exists(&self, p: &Path) -> bool {
        p.as_os_str().is_empty() || self.live_dirs.contains(p)
    }

    fn require_parent(&self, p: &Path) -> io::Result<()> {
        if self.dir_exists(&parent_of(p)) {
            Ok(())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    /// Finds the first rule that fires for this operation.
    fn check_fault(&mut self, op: FaultOp, path: &Path) -> Option<FaultEffect> {
        for (rule, count) in &mut self.faults {
            if rule.op != op {
                continue;
            }
            if let Some(prefix) = &rule.path_prefix
                && !path.starts_with(prefix)
            {
                continue;
            }
            *count += 1;
            let fire = match (rule.nth, rule.probability) {
                (Some(n), _) => *count == n,
                (None, Some(p)) => self.rng.next_f64() < p,
                (None, None) => true,
            };
            if fire {
                self.stats.faults_fired += 1;
                return Some(rule.effect.clone());
            }
        }
        None
    }

    fn new_inode(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, Inode::default());
        ino
    }

    fn create_file(&mut self, p: &Path) -> u64 {
        let ino = self.new_inode();
        self.live_files.insert(p.to_path_buf(), ino);
        ino
    }

    fn do_crash(&mut self, mode: CrashMode) {
        self.epoch += 1;
        self.faults.clear();
        self.locks.clear();
        if matches!(mode, CrashMode::KeepAll) {
            self.durable_files = self.live_files.clone();
            self.durable_dirs = self.live_dirs.clone();
        } else {
            // Keep only entries whose parent directory is itself durable
            // (BTreeSet order puts parents before children).
            let mut dirs = BTreeSet::new();
            for d in &self.durable_dirs {
                if parent_of(d).as_os_str().is_empty() || dirs.contains(&parent_of(d)) {
                    dirs.insert(d.clone());
                }
            }
            self.durable_files
                .retain(|f, _| parent_of(f).as_os_str().is_empty() || dirs.contains(&parent_of(f)));
            self.durable_dirs = dirs;
            self.live_files = self.durable_files.clone();
            self.live_dirs = self.durable_dirs.clone();
        }
        let referenced: BTreeSet<u64> = self.live_files.values().copied().collect();
        self.inodes.retain(|ino, _| referenced.contains(ino));
        let rng = &mut self.rng;
        for node in self.inodes.values_mut() {
            match mode {
                CrashMode::KeepAll => {}
                CrashMode::DropUnsynced => node.current = node.durable.clone(),
                CrashMode::TornSectors {
                    sector,
                    keep_probability,
                } => {
                    node.current = torn(node, sector.max(1), keep_probability, rng);
                }
            }
            node.durable = node.current.clone();
            node.unsynced.clear();
            node.len_dirty = false;
        }
    }
}

/// The content after a crash that keeps each unsynced sector with the given
/// probability.
fn torn(node: &Inode, sector: usize, keep: f64, rng: &mut SplitMix64) -> Vec<u8> {
    let mut out = node.durable.clone();
    if node.len_dirty && rng.next_f64() < keep {
        out.resize(node.current.len(), 0);
    }
    let mut sectors = BTreeSet::new();
    for &(o, l) in &node.unsynced {
        if l == 0 {
            continue;
        }
        let s = usize::try_from(o).unwrap_or(usize::MAX) / sector;
        let e = usize::try_from(o + l - 1).unwrap_or(usize::MAX) / sector;
        sectors.extend(s..=e);
    }
    for s in sectors {
        if rng.next_f64() >= keep {
            continue;
        }
        let start = s * sector;
        let end = ((s + 1) * sector).min(node.current.len());
        if start >= end {
            continue;
        }
        if out.len() < end {
            out.resize(end, 0);
        }
        out[start..end].copy_from_slice(&node.current[start..end]);
    }
    out
}

#[derive(Debug, Clone)]
pub struct SimVfs {
    state: Arc<Mutex<SimState>>,
    generation: u64,
}

fn lock_state(m: &Mutex<SimState>) -> MutexGuard<'_, SimState> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Locks the state, failing with EIO if this instance is from before a crash.
fn live_state(m: &Mutex<SimState>, generation: u64) -> io::Result<MutexGuard<'_, SimState>> {
    let st = lock_state(m);
    if st.epoch == generation {
        Ok(st)
    } else {
        Err(eio())
    }
}

impl SimVfs {
    /// `seed` drives the fault probabilities and torn-write choices (`SplitMix64`).
    pub fn new(seed: u64) -> SimVfs {
        SimVfs {
            state: Arc::new(Mutex::new(SimState {
                epoch: 0,
                next_ino: 1,
                inodes: BTreeMap::new(),
                live_files: BTreeMap::new(),
                live_dirs: BTreeSet::new(),
                durable_files: BTreeMap::new(),
                durable_dirs: BTreeSet::new(),
                locks: BTreeSet::new(),
                faults: Vec::new(),
                rng: SplitMix64(seed),
                stats: SimStats::default(),
            })),
            generation: 0,
        }
    }

    /// Replaces the fault plan (and resets the per-rule counters).
    pub fn set_faults(&self, plan: FaultPlan) {
        lock_state(&self.state).faults = plan.rules.into_iter().map(|r| (r, 0)).collect();
    }

    /// Simulates a crash and returns a new instance over the same "disk".
    /// The old instance and everything opened through it fail with EIO
    /// afterwards. Fault rules and locks are cleared.
    #[must_use]
    pub fn crash(&self, mode: CrashMode) -> SimVfs {
        let mut st = lock_state(&self.state);
        st.do_crash(mode);
        SimVfs {
            state: Arc::clone(&self.state),
            generation: st.epoch,
        }
    }

    pub fn stats(&self) -> SimStats {
        lock_state(&self.state).stats.clone()
    }

    /// Test helper: the current content of a file (what reads see).
    pub fn file_contents(&self, path: &Path) -> Option<Vec<u8>> {
        let st = lock_state(&self.state);
        let ino = st.live_files.get(&normalize(path).ok()?)?;
        Some(st.inodes[ino].current.clone())
    }

    /// Runs the fault check for `op`. `Delay` sleeps (without holding the
    /// state lock) and is then reported as no fault; `Error` becomes `Err`;
    /// the remaining effects are returned for the caller to apply.
    fn gate(&self, op: FaultOp, path: &Path) -> io::Result<Option<FaultEffect>> {
        let effect = {
            let mut st = live_state(&self.state, self.generation)?;
            st.check_fault(op, path)
        };
        match effect {
            Some(FaultEffect::Delay(d)) => {
                std::thread::sleep(d);
                Ok(None)
            }
            Some(FaultEffect::Error(kind)) => Err(injected(kind)),
            other => Ok(other),
        }
    }

    /// `gate` for operations that do not support the data effects.
    fn gate_simple(&self, op: FaultOp, path: &Path) -> io::Result<()> {
        match self.gate(op, path)? {
            None => Ok(()),
            Some(_) => Err(injected(io::ErrorKind::Other)),
        }
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

/// What an injected fault does.
///
/// - `Error(kind)`: the operation fails with `kind` and has no effect.
/// - `ShortWrite`: (`Write`) the first half of the buffer is written, then
///   the call fails with `StorageFull`. On other operations, a plain error.
/// - `FsyncFailAndForget`: (`Sync`) the call fails and the unsynced writes
///   are forgotten (they stay readable but are never made durable), so the
///   next sync succeeds. On other operations, a plain error.
/// - `BitFlipOnRead`: (`Read`) one bit of the returned bytes is flipped; the
///   stored data is unchanged. On other operations, a plain error.
/// - `Delay(d)`: the operation sleeps for `d`, then proceeds normally.
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
    /// All unsynced writes and unsynced directory operations are lost.
    DropUnsynced,
    /// Each unsynced `sector`-sized chunk survives with `keep_probability`
    /// (torn pages). Directory operations behave as in `DropUnsynced`.
    TornSectors {
        sector: usize,
        keep_probability: f64,
    },
    /// Everything survives (only the process died).
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

#[derive(Debug)]
struct SimFile {
    state: Arc<Mutex<SimState>>,
    generation: u64,
    ino: u64,
    /// The path at open time (used for fault matching).
    path: PathBuf,
    read_only: bool,
}

impl SimFile {
    fn gate(&self, op: FaultOp) -> io::Result<Option<FaultEffect>> {
        let vfs = SimVfs {
            state: Arc::clone(&self.state),
            generation: self.generation,
        };
        vfs.gate(op, &self.path)
    }

    fn with_inode<R>(&self, f: impl FnOnce(&mut SimState, u64) -> io::Result<R>) -> io::Result<R> {
        let mut st = live_state(&self.state, self.generation)?;
        f(&mut st, self.ino)
    }

    fn writable(&self) -> io::Result<()> {
        if self.read_only {
            Err(io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        }
    }

    fn sync_impl(&self) -> io::Result<()> {
        let effect = self.gate(FaultOp::Sync)?;
        self.with_inode(|st, ino| {
            st.stats.syncs += 1;
            let node = st.inodes.get_mut(&ino).ok_or_else(eio)?;
            match effect {
                None => {
                    node.sync();
                    Ok(())
                }
                Some(FaultEffect::FsyncFailAndForget) => {
                    node.unsynced.clear();
                    node.len_dirty = false;
                    Err(injected(io::ErrorKind::Other))
                }
                Some(_) => Err(injected(io::ErrorKind::Other)),
            }
        })
    }
}

impl VfsFile for SimFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let effect = self.gate(FaultOp::Read)?;
        self.with_inode(|st, ino| {
            st.stats.reads += 1;
            let node = &st.inodes[&ino];
            let off = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
            let end = off
                .checked_add(buf.len())
                .filter(|&e| e <= node.current.len())
                .ok_or(io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(&node.current[off..end]);
            match effect {
                None => {}
                Some(FaultEffect::BitFlipOnRead) => {
                    if !buf.is_empty() {
                        let bit = st.rng.next_u64();
                        let idx = usize::try_from(bit >> 3).unwrap_or(0) % buf.len();
                        buf[idx] ^= 1 << (bit & 7);
                    }
                }
                Some(_) => return Err(injected(io::ErrorKind::Other)),
            }
            Ok(())
        })
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.writable()?;
        let effect = self.gate(FaultOp::Write)?;
        self.with_inode(|st, ino| {
            st.stats.writes += 1;
            let node = st.inodes.get_mut(&ino).ok_or_else(eio)?;
            match effect {
                None => node.write_at(offset, buf),
                Some(FaultEffect::ShortWrite) => {
                    node.write_at(offset, &buf[..buf.len() / 2])?;
                    Err(injected(io::ErrorKind::StorageFull))
                }
                Some(_) => Err(injected(io::ErrorKind::Other)),
            }
        })
    }

    fn size(&self) -> io::Result<u64> {
        self.with_inode(|st, ino| Ok(st.inodes[&ino].current.len() as u64))
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.writable()?;
        if self.gate(FaultOp::SetLen)?.is_some() {
            return Err(injected(io::ErrorKind::Other));
        }
        let len = usize::try_from(len).map_err(|_| io::Error::other("length too large"))?;
        self.with_inode(|st, ino| {
            st.inodes.get_mut(&ino).ok_or_else(eio)?.set_len(len);
            Ok(())
        })
    }

    fn sync_data(&self) -> io::Result<()> {
        self.sync_impl()
    }

    fn sync_all(&self) -> io::Result<()> {
        self.sync_impl()
    }
}

#[derive(Debug)]
struct SimLock {
    state: Arc<Mutex<SimState>>,
    generation: u64,
    path: PathBuf,
}

impl VfsLock for SimLock {}

impl Drop for SimLock {
    fn drop(&mut self) {
        let mut st = lock_state(&self.state);
        if st.epoch == self.generation {
            st.locks.remove(&self.path);
        }
    }
}

impl Vfs for SimVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Arc<dyn VfsFile>> {
        let p = normalize(path)?;
        self.gate_simple(FaultOp::Open, &p)?;
        let mut st = live_state(&self.state, self.generation)?;
        if p.as_os_str().is_empty() || st.live_dirs.contains(&p) {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        let ino = match mode {
            OpenMode::CreateNew => {
                st.require_parent(&p)?;
                if st.live_files.contains_key(&p) {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                st.create_file(&p)
            }
            OpenMode::ReadWrite | OpenMode::ReadOnly => *st
                .live_files
                .get(&p)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?,
        };
        Ok(Arc::new(SimFile {
            state: Arc::clone(&self.state),
            generation: self.generation,
            ino,
            path: p,
            read_only: mode == OpenMode::ReadOnly,
        }))
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        let p = normalize(path)?;
        let st = live_state(&self.state, self.generation)?;
        Ok(st.dir_exists(&p) || st.live_files.contains_key(&p))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        self.gate_simple(FaultOp::Remove, &p)?;
        let mut st = live_state(&self.state, self.generation)?;
        st.live_files
            .remove(&p)
            .map(|_| ())
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (normalize(from)?, normalize(to)?);
        self.gate_simple(FaultOp::Rename, &f)?;
        let mut st = live_state(&self.state, self.generation)?;
        st.require_parent(&t)?;
        if let Some(ino) = st.live_files.get(&f).copied() {
            if st.live_dirs.contains(&t) {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            st.live_files.remove(&f);
            st.live_files.insert(t, ino);
            Ok(())
        } else if st.live_dirs.contains(&f) {
            if st.live_dirs.contains(&t) || st.live_files.contains_key(&t) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            let moved_dirs: Vec<PathBuf> = st
                .live_dirs
                .iter()
                .filter(|d| d.starts_with(&f))
                .cloned()
                .collect();
            for d in moved_dirs {
                st.live_dirs.remove(&d);
                let nd = t.join(d.strip_prefix(&f).unwrap_or(&d));
                st.live_dirs.insert(nd);
            }
            let moved_files: Vec<(PathBuf, u64)> = st
                .live_files
                .iter()
                .filter(|(k, _)| k.starts_with(&f))
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            for (k, ino) in moved_files {
                st.live_files.remove(&k);
                let nk = t.join(k.strip_prefix(&f).unwrap_or(&k));
                st.live_files.insert(nk, ino);
            }
            Ok(())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut st = live_state(&self.state, self.generation)?;
        st.require_parent(&p)?;
        if st.dir_exists(&p) || st.live_files.contains_key(&p) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        st.live_dirs.insert(p);
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        let mut st = live_state(&self.state, self.generation)?;
        let mut cur = PathBuf::new();
        for c in p.components() {
            cur.push(c);
            if st.live_files.contains_key(&cur) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            st.live_dirs.insert(cur.clone());
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        self.gate_simple(FaultOp::Remove, &p)?;
        let mut st = live_state(&self.state, self.generation)?;
        if p.as_os_str().is_empty() || !st.live_dirs.contains(&p) {
            return Err(io::ErrorKind::NotFound.into());
        }
        st.live_dirs.retain(|d| !d.starts_with(&p));
        st.live_files.retain(|f, _| !f.starts_with(&p));
        Ok(())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let p = normalize(path)?;
        let st = live_state(&self.state, self.generation)?;
        if !st.dir_exists(&p) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut out: Vec<PathBuf> = st
            .live_dirs
            .iter()
            .chain(st.live_files.keys())
            .filter(|e| parent_of(e) == p)
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let p = normalize(path)?;
        self.gate_simple(FaultOp::SyncDir, &p)?;
        let mut st = live_state(&self.state, self.generation)?;
        st.stats.sync_dirs += 1;
        if !st.dir_exists(&p) {
            return Err(io::ErrorKind::NotFound.into());
        }
        st.durable_files.retain(|f, _| parent_of(f) != p);
        st.durable_dirs.retain(|d| parent_of(d) != p);
        let files: Vec<(PathBuf, u64)> = st
            .live_files
            .iter()
            .filter(|(f, _)| parent_of(f) == p)
            .map(|(f, i)| (f.clone(), *i))
            .collect();
        st.durable_files.extend(files);
        let dirs: Vec<PathBuf> = st
            .live_dirs
            .iter()
            .filter(|d| parent_of(d) == p)
            .cloned()
            .collect();
        st.durable_dirs.extend(dirs);
        Ok(())
    }

    fn lock_file(&self, path: &Path) -> io::Result<Box<dyn VfsLock>> {
        let p = normalize(path)?;
        self.gate_simple(FaultOp::Open, &p)?;
        let mut st = live_state(&self.state, self.generation)?;
        st.require_parent(&p)?;
        if st.dir_exists(&p) {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        if st.locks.contains(&p) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if !st.live_files.contains_key(&p) {
            st.create_file(&p);
        }
        st.locks.insert(p.clone());
        Ok(Box::new(SimLock {
            state: Arc::clone(&self.state),
            generation: self.generation,
            path: p,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    fn create(vfs: &SimVfs, path: &str, data: &[u8]) -> Arc<dyn VfsFile> {
        let f = vfs.open(p(path), OpenMode::CreateNew).unwrap();
        f.write_all_at(data, 0).unwrap();
        f
    }

    fn read_all(f: &Arc<dyn VfsFile>) -> Vec<u8> {
        let mut b = vec![0u8; usize::try_from(f.size().unwrap()).unwrap()];
        f.read_exact_at(&mut b, 0).unwrap();
        b
    }

    #[test]
    fn basic_file_and_dir_operations() {
        let vfs = SimVfs::new(1);
        assert!(vfs.exists(p("")).unwrap());
        assert_eq!(
            vfs.open(p("a/f"), OpenMode::CreateNew).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        vfs.create_dir(p("a")).unwrap();
        assert_eq!(
            vfs.create_dir(p("a")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        vfs.create_dir_all(p("a/b/c")).unwrap();
        let f = create(&vfs, "a/f", b"hello");
        assert_eq!(
            vfs.open(p("a/f"), OpenMode::CreateNew).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        f.write_all_at(b"XY", 8).unwrap();
        assert_eq!(read_all(&f), b"hello\0\0\0XY");
        assert_eq!(
            f.read_exact_at(&mut [0u8; 3], 9).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        f.set_len(3).unwrap();
        assert_eq!(read_all(&f), b"hel");
        assert_eq!(
            vfs.read_dir(p("a")).unwrap(),
            vec![PathBuf::from("a/b"), PathBuf::from("a/f")]
        );
        vfs.rename(p("a/f"), p("a/g")).unwrap();
        assert!(!vfs.exists(p("a/f")).unwrap());
        assert_eq!(vfs.file_contents(p("a/g")).unwrap(), b"hel");
        let ro = vfs.open(p("a/g"), OpenMode::ReadOnly).unwrap();
        assert_eq!(
            ro.write_all_at(b"x", 0).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        vfs.remove_dir_all(p("a")).unwrap();
        assert!(!vfs.exists(p("a/b/c")).unwrap());
        assert_eq!(
            vfs.remove_file(p("a/g")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            vfs.exists(p("../x")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn rename_directory_moves_subtree() {
        let vfs = SimVfs::new(1);
        vfs.create_dir_all(p("d/e")).unwrap();
        create(&vfs, "d/e/f", b"1");
        vfs.rename(p("d"), p("z")).unwrap();
        assert_eq!(vfs.file_contents(p("z/e/f")).unwrap(), b"1");
        assert!(!vfs.exists(p("d")).unwrap());
    }

    #[test]
    fn unlinked_handle_keeps_working() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"abc");
        vfs.remove_file(p("f")).unwrap();
        f.write_all_at(b"Z", 0).unwrap();
        f.sync_data().unwrap();
        assert_eq!(read_all(&f), b"Zbc");
        assert!(!vfs.exists(p("f")).unwrap());
    }

    #[test]
    fn crash_drop_unsynced_data() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"old!");
        f.sync_all().unwrap();
        vfs.sync_dir(p("")).unwrap();
        f.write_all_at(b"NEW", 0).unwrap();
        f.write_all_at(b"tail", 10).unwrap();
        let after = vfs.crash(CrashMode::DropUnsynced);
        let g = after.open(p("f"), OpenMode::ReadWrite).unwrap();
        assert_eq!(read_all(&g), b"old!");
        // The old instance and its handles are dead.
        assert_eq!(f.size().unwrap_err().raw_os_error(), Some(5));
        assert_eq!(vfs.exists(p("f")).unwrap_err().raw_os_error(), Some(5));
        // Data synced but directory entry not synced: file vanishes.
        let h = create(&after, "h", b"x");
        h.sync_all().unwrap();
        let after2 = after.crash(CrashMode::DropUnsynced);
        assert!(!after2.exists(p("h")).unwrap());
        assert!(after2.exists(p("f")).unwrap());
    }

    #[test]
    fn crash_keep_all_keeps_everything() {
        let vfs = SimVfs::new(1);
        create(&vfs, "f", b"data");
        let after = vfs.crash(CrashMode::KeepAll);
        assert_eq!(after.file_contents(p("f")).unwrap(), b"data");
        // And now it is durable.
        let after2 = after.crash(CrashMode::DropUnsynced);
        assert_eq!(after2.file_contents(p("f")).unwrap(), b"data");
    }

    #[test]
    fn crash_namespace_needs_sync_dir() {
        let vfs = SimVfs::new(1);
        vfs.create_dir(p("d")).unwrap();
        vfs.sync_dir(p("")).unwrap();
        let f = create(&vfs, "d/a", b"1");
        f.sync_all().unwrap();
        vfs.sync_dir(p("d")).unwrap();
        let g = create(&vfs, "d/b", b"2");
        g.sync_all().unwrap();
        // Removal and rename are not durable until sync_dir either.
        vfs.rename(p("d/a"), p("d/c")).unwrap();
        let after = vfs.crash(CrashMode::DropUnsynced);
        assert!(after.exists(p("d/a")).unwrap());
        assert!(!after.exists(p("d/b")).unwrap());
        assert!(!after.exists(p("d/c")).unwrap());
        // Unsynced new directory: its (synced) children vanish with it.
        let v2 = SimVfs::new(1);
        v2.create_dir(p("n")).unwrap();
        let f = create(&v2, "n/x", b"1");
        f.sync_all().unwrap();
        v2.sync_dir(p("n")).unwrap();
        let a2 = v2.crash(CrashMode::DropUnsynced);
        assert!(!a2.exists(p("n")).unwrap());
        assert!(!a2.exists(p("n/x")).unwrap());
    }

    #[test]
    fn crash_torn_sectors_is_deterministic_and_sector_granular() {
        fn run(seed: u64) -> Vec<u8> {
            let vfs = SimVfs::new(seed);
            let f = vfs.open(p("f"), OpenMode::CreateNew).unwrap();
            f.write_all_at(&[1u8; 4096], 0).unwrap();
            f.sync_all().unwrap();
            vfs.sync_dir(p("")).unwrap();
            f.write_all_at(&[2u8; 4096], 0).unwrap();
            let after = vfs.crash(CrashMode::TornSectors {
                sector: 512,
                keep_probability: 0.5,
            });
            after.file_contents(p("f")).unwrap()
        }
        let a = run(7);
        assert_eq!(a, run(7));
        assert_eq!(a.len(), 4096);
        let mut kinds = BTreeSet::new();
        for sec in a.chunks(512) {
            assert!(sec.iter().all(|&b| b == sec[0]), "sector is atomic");
            kinds.insert(sec[0]);
        }
        let differs = (0..20).any(|s| run(s) != a);
        assert!(differs);
        // Across seeds, both old and new sectors appear.
        let mut seen = BTreeSet::new();
        for s in 0..20 {
            seen.extend(run(s).iter().copied());
        }
        assert_eq!(seen, BTreeSet::from([1, 2]));
    }

    #[test]
    fn torn_extension_keeps_prefix_sectors_only() {
        let vfs = SimVfs::new(3);
        let f = vfs.open(p("f"), OpenMode::CreateNew).unwrap();
        f.write_all_at(&[5u8; 1024], 0).unwrap();
        vfs.sync_dir(p("")).unwrap();
        let after = vfs.crash(CrashMode::TornSectors {
            sector: 512,
            keep_probability: 1.0,
        });
        assert_eq!(after.file_contents(p("f")).unwrap(), vec![5u8; 1024]);
    }

    #[test]
    fn fault_error_nth_and_prefix() {
        let vfs = SimVfs::new(1);
        vfs.create_dir(p("base")).unwrap();
        let a = create(&vfs, "base/a", b"1");
        let b = create(&vfs, "other", b"1");
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some(PathBuf::from("base")),
                nth: Some(2),
                probability: None,
                effect: FaultEffect::Error(io::ErrorKind::StorageFull),
            }],
        });
        b.write_all_at(b"x", 0).unwrap(); // other path: not counted
        a.write_all_at(b"x", 0).unwrap(); // 1st
        assert_eq!(
            a.write_all_at(b"y", 0).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        a.write_all_at(b"z", 0).unwrap(); // 3rd: fine again
        assert_eq!(vfs.stats().faults_fired, 1);
        assert_eq!(read_all(&a), b"z");
    }

    #[test]
    fn fault_probability_is_seeded() {
        fn run(seed: u64) -> Vec<bool> {
            let vfs = SimVfs::new(seed);
            let f = create(&vfs, "f", b"1");
            vfs.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Sync,
                    path_prefix: None,
                    nth: None,
                    probability: Some(0.5),
                    effect: FaultEffect::Error(io::ErrorKind::Other),
                }],
            });
            (0..32).map(|_| f.sync_data().is_err()).collect()
        }
        let r = run(9);
        assert_eq!(r, run(9));
        assert!(r.iter().any(|&x| x) && r.iter().any(|&x| !x));
    }

    #[test]
    fn short_write_writes_half_then_fails() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"");
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::ShortWrite,
            }],
        });
        assert_eq!(
            f.write_all_at(b"abcdefgh", 0).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(read_all(&f), b"abcd");
    }

    #[test]
    fn fsync_fail_and_forget_loses_data_silently() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"");
        f.sync_all().unwrap();
        vfs.sync_dir(p("")).unwrap();
        f.write_all_at(b"precious", 0).unwrap();
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::FsyncFailAndForget,
            }],
        });
        assert!(f.sync_data().is_err());
        f.sync_data().unwrap(); // the retry "succeeds"
        let after = vfs.crash(CrashMode::DropUnsynced);
        assert_eq!(after.file_contents(p("f")).unwrap(), b"");
    }

    #[test]
    fn bit_flip_on_read_does_not_change_storage() {
        let vfs = SimVfs::new(5);
        let f = create(&vfs, "f", &[0u8; 64]);
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Read,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::BitFlipOnRead,
            }],
        });
        let mut buf = [0u8; 64];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(buf.iter().map(|b| b.count_ones()).sum::<u32>(), 1);
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(buf, [0u8; 64]);
    }

    #[test]
    fn faults_on_metadata_operations() {
        let vfs = SimVfs::new(1);
        create(&vfs, "f", b"1");
        for (op, name) in [
            (FaultOp::Open, "open"),
            (FaultOp::Remove, "remove"),
            (FaultOp::Rename, "rename"),
            (FaultOp::SyncDir, "syncdir"),
        ] {
            vfs.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op,
                    path_prefix: None,
                    nth: None,
                    probability: None,
                    effect: FaultEffect::Error(io::ErrorKind::Other),
                }],
            });
            let r = match name {
                "open" => vfs.open(p("f"), OpenMode::ReadOnly).map(|_| ()),
                "remove" => vfs.remove_file(p("f")),
                "rename" => vfs.rename(p("f"), p("g")),
                _ => vfs.sync_dir(p("")),
            };
            assert!(r.is_err(), "{name}");
        }
        vfs.set_faults(FaultPlan::default());
        let f = vfs.open(p("f"), OpenMode::ReadWrite).unwrap();
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::SetLen,
                path_prefix: None,
                nth: None,
                probability: None,
                effect: FaultEffect::Error(io::ErrorKind::Other),
            }],
        });
        assert!(f.set_len(0).is_err());
        assert_eq!(read_all(&f), b"1");
    }

    #[test]
    fn delay_proceeds_normally() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"1");
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: None,
                nth: None,
                probability: None,
                effect: FaultEffect::Delay(Duration::from_millis(5)),
            }],
        });
        let t = std::time::Instant::now();
        f.sync_data().unwrap();
        assert!(t.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn lock_file_semantics() {
        let vfs = SimVfs::new(1);
        let l = vfs.lock_file(p("yuzhu.pid")).unwrap();
        assert_eq!(
            vfs.lock_file(p("yuzhu.pid")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(l);
        let l2 = vfs.lock_file(p("yuzhu.pid")).unwrap();
        // A crash releases the lock; the stale guard's drop must not release
        // the new owner's lock.
        let after = vfs.crash(CrashMode::KeepAll);
        let l3 = after.lock_file(p("yuzhu.pid")).unwrap();
        drop(l2);
        assert_eq!(
            after.lock_file(p("yuzhu.pid")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(l3);
    }

    #[test]
    fn stats_count_operations() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", b"abcd");
        let mut b = [0u8; 4];
        f.read_exact_at(&mut b, 0).unwrap();
        f.sync_data().unwrap();
        f.sync_all().unwrap();
        vfs.sync_dir(p("")).unwrap();
        let s = vfs.stats();
        assert_eq!((s.reads, s.writes, s.syncs, s.sync_dirs), (1, 1, 2, 1));
    }

    #[test]
    fn concurrent_use_through_shared_handle() {
        let vfs = SimVfs::new(1);
        let f = create(&vfs, "f", &[0u8; 64]);
        std::thread::scope(|s| {
            for i in 0..4u8 {
                let f = Arc::clone(&f);
                s.spawn(move || {
                    for _ in 0..50 {
                        f.write_all_at(&[i; 16], u64::from(i) * 16).unwrap();
                    }
                });
            }
        });
        let all = read_all(&f);
        for (i, c) in all.chunks(16).enumerate() {
            assert!(c.iter().all(|&b| usize::from(b) == i));
        }
    }
}
