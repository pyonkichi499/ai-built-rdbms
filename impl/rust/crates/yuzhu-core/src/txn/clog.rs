//! Commit log `pg_xact/` (`m2.md` §3.8, §6.7.2; `m5/01` LK-D10).
//!
//! Two bits per XID, 32768 XIDs per 8KB page, 32 pages per segment file
//! `pg_xact/<segment as 12 upper-case hex digits>`. Missing files and pages
//! past the end of a file read as zeros (`IN_PROGRESS`).
//!
//! Pages are kept in memory and never evicted. The page table is an
//! `RwLock`, but the status bytes are `AtomicU8`: `status` takes only the
//! read lock and one atomic load, `set_status` only the read lock and a
//! `compare_exchange` loop (`0 -> Committed | Aborted`). `fetch_or` is not
//! used: a double update (Committed and Aborted) would produce the reserved
//! value `0b11` instead of being detected (D13). No disk I/O happens while
//! the page table lock is held.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::Xid;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::vfs::{OpenMode, Vfs};
use crate::util::sync::{lock, read, write};

const PAGE_SIZE: usize = 8192;
const XIDS_PER_PAGE: u64 = (PAGE_SIZE as u64) * 4;
const PAGES_PER_SEGMENT: u64 = 32;
const CLOG_DIR: &str = "pg_xact";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XidStatus {
    InProgress = 0,
    Committed = 1,
    Aborted = 2,
}

#[derive(Debug)]
struct ClogPage {
    data: Box<[AtomicU8]>,
    dirty: AtomicBool,
}

impl ClogPage {
    fn zeroed() -> ClogPage {
        ClogPage {
            data: (0..PAGE_SIZE).map(|_| AtomicU8::new(0)).collect(),
            dirty: AtomicBool::new(false),
        }
    }

    fn from_bytes(bytes: &[u8; PAGE_SIZE]) -> ClogPage {
        ClogPage {
            data: bytes.iter().map(|b| AtomicU8::new(*b)).collect(),
            dirty: AtomicBool::new(false),
        }
    }

    fn bits(&self, byte: usize, shift: u32) -> u8 {
        (self.data[byte].load(Ordering::Acquire) >> shift) & 3
    }

    /// `InProgress -> s` with a compare-and-swap. Returns the 2-bit value
    /// found when it was not 0 (nothing is written then).
    fn try_set(&self, byte: usize, shift: u32, s: XidStatus) -> std::result::Result<(), u8> {
        let cell = &self.data[byte];
        let mut cur = cell.load(Ordering::Acquire);
        loop {
            let found = (cur >> shift) & 3;
            if found != 0 {
                return Err(found);
            }
            let new = cur | ((s as u8) << shift);
            match cell.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.dirty.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(actual) => cur = actual,
            }
        }
    }

    fn snapshot(&self) -> Box<[u8; PAGE_SIZE]> {
        let mut out = Box::new([0u8; PAGE_SIZE]);
        for (o, a) in out.iter_mut().zip(self.data.iter()) {
            *o = a.load(Ordering::Acquire);
        }
        out
    }
}

#[derive(Debug)]
pub struct Clog {
    vfs: Arc<dyn Vfs>,
    /// Page number -> page. Reads and `set_status` take only the read lock;
    /// the write lock is for adding a page.
    pages: RwLock<HashMap<u64, ClogPage>>,
    /// Serializes `flush` so that a second flush cannot return before the
    /// first one has written the pages whose dirty flags it cleared.
    flush_lock: Mutex<()>,
    /// The horizon below which the log is truncated (control file
    /// `oldest_xid`). `truncate_before` and the `XX001` check on older XIDs
    /// are added by VACUUM (`m5/03` C6).
    oldest: AtomicU64,
}

fn page_of(xid: Xid) -> u64 {
    xid.0 / XIDS_PER_PAGE
}

/// Byte index in the page and bit shift of the XID.
#[allow(clippy::cast_possible_truncation)]
fn slot_of(xid: Xid) -> (usize, u32) {
    let in_page = xid.0 % XIDS_PER_PAGE;
    ((in_page / 4) as usize, ((in_page % 4) * 2) as u32)
}

fn segment_path(segment: u64) -> PathBuf {
    Path::new(CLOG_DIR).join(format!("{segment:012X}"))
}

fn decode(bits: u8) -> Result<XidStatus> {
    match bits {
        0 => Ok(XidStatus::InProgress),
        1 => Ok(XidStatus::Committed),
        2 => Ok(XidStatus::Aborted),
        _ => Err(Error::corrupted(
            "commit log holds the reserved status SUB_COMMITTED",
        )),
    }
}

impl Clog {
    /// Loads the page containing `next_xid`; pages of XIDs assigned later
    /// are new, so they need no I/O. `oldest` is the control file's
    /// `oldest_xid` (values below `Xid::FIRST_NORMAL` are raised to it).
    pub fn open(vfs: Arc<dyn Vfs>, next_xid: Xid, oldest: Xid) -> Result<Clog> {
        let clog = Clog {
            vfs,
            pages: RwLock::new(HashMap::new()),
            flush_lock: Mutex::new(()),
            oldest: AtomicU64::new(oldest.0.max(Xid::FIRST_NORMAL.0)),
        };
        let pageno = page_of(next_xid);
        let page = clog.read_page(pageno)?;
        write(&clog.pages)?.insert(pageno, page);
        Ok(clog)
    }

    /// The truncation horizon given to [`open`](Self::open) (VACUUM advances
    /// it later).
    pub fn oldest(&self) -> Xid {
        Xid(self.oldest.load(Ordering::Acquire))
    }

    /// Reads a page from disk (zeros for a missing file or a short file).
    fn read_page(&self, pageno: u64) -> Result<ClogPage> {
        let mut bytes = Box::new([0u8; PAGE_SIZE]);
        let path = segment_path(pageno / PAGES_PER_SEGMENT);
        let io_err = |e: &io::Error| {
            Error::from_io(e, format!("could not read file \"{}\"", path.display()))
        };
        if !self.vfs.exists(&path).map_err(|e| io_err(&e))? {
            return Ok(ClogPage::zeroed());
        }
        let file = match self.vfs.open(&path, OpenMode::ReadOnly) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ClogPage::zeroed()),
            Err(e) => return Err(io_err(&e)),
        };
        let offset = (pageno % PAGES_PER_SEGMENT) * PAGE_SIZE as u64;
        let size = file.size().map_err(|e| io_err(&e))?;
        if size > offset {
            // A torn or short last page reads as zeros past the end.
            #[allow(clippy::cast_possible_truncation)]
            let n = (size - offset).min(PAGE_SIZE as u64) as usize;
            file.read_exact_at(&mut bytes[..n], offset)
                .map_err(|e| io_err(&e))?;
        }
        Ok(ClogPage::from_bytes(&bytes))
    }

    /// The status of `xid`. `Xid::INVALID` is an internal error; the
    /// bootstrap and frozen XIDs are committed without consulting the log.
    /// Takes only the read lock of the page table and one atomic load. A
    /// page that is not in memory is read from disk with no lock held.
    pub fn status(&self, xid: Xid) -> Result<XidStatus> {
        if xid == Xid::INVALID {
            return Err(Error::internal("clog lookup of the invalid transaction ID"));
        }
        if !xid.is_normal() {
            return Ok(XidStatus::Committed);
        }
        let pageno = page_of(xid);
        let (byte, shift) = slot_of(xid);
        if let Some(p) = read(&self.pages)?.get(&pageno) {
            return decode(p.bits(byte, shift));
        }
        // Not in memory: read without holding the lock, then keep the first
        // copy inserted (a concurrent `ensure_page_for` / `set_status` wins).
        let loaded = self.read_page(pageno)?;
        let mut pages = write(&self.pages)?;
        let p = pages.entry(pageno).or_insert(loaded);
        decode(p.bits(byte, shift))
    }

    /// Called by `ProcArray::assign` inside the `P` mutex when assigning an
    /// XID (`ExtendCLOG`). Creates a zero page if absent; no I/O. The read
    /// lock is enough when the page exists (the common case).
    ///
    /// A poisoned lock is ignored here: the failure then surfaces in
    /// `set_status` (which reports it as an error).
    pub fn ensure_page_for(&self, xid: Xid) {
        let pageno = page_of(xid);
        if let Ok(pages) = self.pages.read()
            && pages.contains_key(&pageno)
        {
            return;
        }
        if let Ok(mut pages) = self.pages.write() {
            pages.entry(pageno).or_insert_with(ClogPage::zeroed);
        }
    }

    /// Only `InProgress -> Committed | Aborted` is allowed. In memory only.
    /// The page must have been made by `ensure_page_for` (or `open`);
    /// otherwise this is an internal error.
    pub fn set_status(&self, xid: Xid, s: XidStatus) -> Result<()> {
        if !xid.is_normal() {
            return Err(Error::internal(format!(
                "cannot set the commit log status of reserved transaction ID {}",
                xid.0
            )));
        }
        if s == XidStatus::InProgress {
            return Err(Error::internal(format!(
                "cannot reset the status of transaction {} to in-progress",
                xid.0
            )));
        }
        let (byte, shift) = slot_of(xid);
        let pages = read(&self.pages)?;
        let page = pages.get(&page_of(xid)).ok_or_else(|| {
            Error::internal(format!(
                "commit log page for transaction {} is not in memory",
                xid.0
            ))
        })?;
        page.try_set(byte, shift, s)
            .map_err(|found| match decode(found) {
                Ok(current) => Error::internal(format!(
                    "transaction {} is already {current:?}, cannot become {s:?}",
                    xid.0
                )),
                Err(e) => e,
            })
    }

    /// Makes sure the page is in memory, reading it from disk if needed
    /// (zeros for a missing file). The first copy inserted wins.
    fn ensure_loaded(&self, pageno: u64) -> Result<()> {
        if read(&self.pages)?.contains_key(&pageno) {
            return Ok(());
        }
        let loaded = self.read_page(pageno)?;
        write(&self.pages)?.entry(pageno).or_insert(loaded);
        Ok(())
    }

    /// For REDO: like `set_status`, but the page is read from disk if it is
    /// not in memory, and setting the same value again is allowed.
    /// `Committed <-> Aborted` contradicts the WAL and is `Severity::Panic`.
    pub fn set_status_redo(&self, xid: Xid, s: XidStatus) -> Result<()> {
        if !xid.is_normal() || s == XidStatus::InProgress {
            return Err(Error::internal(format!(
                "invalid commit log update during REDO: transaction {} to {s:?}",
                xid.0
            ))
            .with_severity(Severity::Panic));
        }
        let pageno = page_of(xid);
        self.ensure_loaded(pageno)
            .map_err(|e| e.with_severity(Severity::Panic))?;
        let (byte, shift) = slot_of(xid);
        let pages = read(&self.pages)?;
        let page = pages.get(&pageno).ok_or_else(|| {
            Error::internal("commit log page vanished during REDO").with_severity(Severity::Panic)
        })?;
        match page.try_set(byte, shift, s) {
            Ok(()) => Ok(()),
            Err(found) => {
                let current = decode(found).map_err(|e| e.with_severity(Severity::Panic))?;
                if current == s {
                    return Ok(());
                }
                Err(Error::new(
                    sqlstate::DATA_CORRUPTED,
                    format!(
                        "WAL says transaction {} is {s:?} but the commit log says {current:?}",
                        xid.0
                    ),
                )
                .with_severity(Severity::Panic))
            }
        }
    }

    /// Loads the page of `xid` (from disk if it exists) so that a later
    /// `ensure_page_for` cannot replace a page that has content on disk with
    /// zeros (`m3.md` §6.6.2). Called by recovery for `next_xid`.
    pub fn load_page_for(&self, xid: Xid) -> Result<()> {
        self.ensure_loaded(page_of(xid))
    }

    /// Writes dirty pages and `sync_data`s them (checkpoint and shutdown).
    /// Any I/O failure is `Severity::Panic` (no retry after a failed fsync).
    pub fn flush(&self) -> Result<()> {
        let _serial = lock(&self.flush_lock)?;
        // Clear each page's dirty flag first, then copy its bytes: a
        // `set_status` after the swap sets the flag again (an extra write
        // at worst), one before it is in the copy.
        let mut dirty: BTreeMap<u64, Box<[u8; PAGE_SIZE]>> = BTreeMap::new();
        {
            let pages = read(&self.pages)?;
            for (&no, p) in pages.iter() {
                if p.dirty.swap(false, Ordering::AcqRel) {
                    dirty.insert(no, p.snapshot());
                }
            }
        }
        if dirty.is_empty() {
            return Ok(());
        }
        self.write_pages(&dirty).map_err(|e| {
            Error::from_io(&e.0, format!("could not write the commit log: {}", e.1))
                .with_severity(Severity::Panic)
        })
    }

    /// Returns the I/O error with the name of the failed step.
    fn write_pages(
        &self,
        dirty: &BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    ) -> std::result::Result<(), (io::Error, String)> {
        let dir = Path::new(CLOG_DIR);
        let step = |what: &str, path: &Path| format!("{what} \"{}\"", path.display());
        let mut created_any = false;
        let mut current: Option<(u64, Arc<dyn crate::storage::vfs::VfsFile>)> = None;
        let mut synced: Vec<Arc<dyn crate::storage::vfs::VfsFile>> = Vec::new();
        for (&no, data) in dirty {
            let segment = no / PAGES_PER_SEGMENT;
            if current.as_ref().is_none_or(|(s, _)| *s != segment) {
                let path = segment_path(segment);
                let exists = self
                    .vfs
                    .exists(&path)
                    .map_err(|e| (e, step("stat", &path)))?;
                let file = if exists {
                    self.vfs.open(&path, OpenMode::ReadWrite)
                } else {
                    self.vfs
                        .create_dir_all(dir)
                        .map_err(|e| (e, step("create directory", dir)))?;
                    created_any = true;
                    self.vfs.open(&path, OpenMode::CreateNew)
                }
                .map_err(|e| (e, step("open", &path)))?;
                if let Some((_, prev)) = current.take() {
                    synced.push(prev);
                }
                current = Some((segment, file));
            }
            if let Some((_, file)) = &current {
                let offset = (no % PAGES_PER_SEGMENT) * PAGE_SIZE as u64;
                file.write_all_at(&data[..], offset)
                    .map_err(|e| (e, step("write", &segment_path(segment))))?;
            }
        }
        if let Some((_, f)) = current.take() {
            synced.push(f);
        }
        for f in &synced {
            f.sync_data()
                .map_err(|e| (e, "sync of a segment file".to_string()))?;
        }
        if created_any {
            self.vfs
                .sync_dir(dir)
                .map_err(|e| (e, step("sync directory", dir)))?;
        }
        Ok(())
    }

    /// For tests and diagnostics: whether any page is dirty.
    #[cfg(test)]
    fn has_dirty(&self) -> bool {
        self.pages
            .read()
            .unwrap()
            .values()
            .any(|p| p.dirty.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::storage::vfs::SimVfs;

    fn vfs() -> Arc<dyn Vfs> {
        let v = SimVfs::new(1);
        v.create_dir_all(Path::new("pg_xact")).unwrap();
        Arc::new(v)
    }

    #[test]
    fn page_math() {
        assert_eq!(page_of(Xid(0)), 0);
        assert_eq!(page_of(Xid(32767)), 0);
        assert_eq!(page_of(Xid(32768)), 1);
        assert_eq!(slot_of(Xid(5)), (1, 2));
        assert_eq!(slot_of(Xid(32767)), (8191, 6));
        assert_eq!(segment_path(0x1A).to_str().unwrap(), "pg_xact/00000000001A");
    }

    #[test]
    fn reserved_xids() {
        let c = Clog::open(vfs(), Xid(3), Xid(3)).unwrap();
        assert_eq!(c.status(Xid::BOOTSTRAP).unwrap(), XidStatus::Committed);
        assert_eq!(c.status(Xid::FROZEN).unwrap(), XidStatus::Committed);
        assert_eq!(
            c.status(Xid::INVALID).unwrap_err().sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert!(c.set_status(Xid::BOOTSTRAP, XidStatus::Committed).is_err());
    }

    #[test]
    fn set_and_get_status() {
        let c = Clog::open(vfs(), Xid(3), Xid(3)).unwrap();
        for x in 3..8 {
            c.ensure_page_for(Xid(x));
            assert_eq!(c.status(Xid(x)).unwrap(), XidStatus::InProgress);
        }
        c.set_status(Xid(4), XidStatus::Committed).unwrap();
        c.set_status(Xid(5), XidStatus::Aborted).unwrap();
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::InProgress);
        assert_eq!(c.status(Xid(4)).unwrap(), XidStatus::Committed);
        assert_eq!(c.status(Xid(5)).unwrap(), XidStatus::Aborted);
        assert_eq!(c.status(Xid(6)).unwrap(), XidStatus::InProgress);
    }

    #[test]
    fn transitions_are_one_way() {
        let c = Clog::open(vfs(), Xid(3), Xid(3)).unwrap();
        c.set_status(Xid(3), XidStatus::Committed).unwrap();
        assert!(c.set_status(Xid(3), XidStatus::Aborted).is_err());
        assert!(c.set_status(Xid(3), XidStatus::Committed).is_err());
        assert!(c.set_status(Xid(3), XidStatus::InProgress).is_err());
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn set_status_without_page_is_internal_error() {
        let c = Clog::open(vfs(), Xid(3), Xid(3)).unwrap();
        let far = Xid(XIDS_PER_PAGE * 3 + 1);
        let e = c.set_status(far, XidStatus::Committed).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        c.ensure_page_for(far);
        c.set_status(far, XidStatus::Committed).unwrap();
    }

    #[test]
    fn flush_and_reopen() {
        let v = vfs();
        let far = Xid(XIDS_PER_PAGE * PAGES_PER_SEGMENT + 10); // second segment
        {
            let c = Clog::open(Arc::clone(&v), Xid(3), Xid(3)).unwrap();
            c.ensure_page_for(far);
            c.set_status(Xid(3), XidStatus::Committed).unwrap();
            c.set_status(Xid(4), XidStatus::Aborted).unwrap();
            c.set_status(far, XidStatus::Committed).unwrap();
            assert!(c.has_dirty());
            c.flush().unwrap();
            assert!(!c.has_dirty());
            c.flush().unwrap(); // nothing to do
        }
        assert!(v.exists(Path::new("pg_xact/000000000000")).unwrap());
        assert!(v.exists(Path::new("pg_xact/000000000001")).unwrap());
        let c = Clog::open(v, Xid(5), Xid(3)).unwrap();
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::Committed);
        assert_eq!(c.status(Xid(4)).unwrap(), XidStatus::Aborted);
        assert_eq!(c.status(Xid(5)).unwrap(), XidStatus::InProgress);
        // A page that is not in memory is read on demand.
        assert_eq!(c.status(far).unwrap(), XidStatus::Committed);
        // Past the end of the file / missing file: zeros.
        assert_eq!(
            c.status(Xid(far.0 + XIDS_PER_PAGE * 40)).unwrap(),
            XidStatus::InProgress
        );
    }

    #[test]
    fn set_after_flush_marks_dirty_again() {
        let c = Clog::open(vfs(), Xid(3), Xid(3)).unwrap();
        c.set_status(Xid(3), XidStatus::Committed).unwrap();
        c.flush().unwrap();
        assert!(!c.has_dirty());
        c.set_status(Xid(4), XidStatus::Committed).unwrap();
        assert!(c.has_dirty());
    }

    #[test]
    fn short_file_reads_as_zeros() {
        let v = vfs();
        {
            let f = v
                .open(Path::new("pg_xact/000000000000"), OpenMode::CreateNew)
                .unwrap();
            // Only the first 2 bytes of page 0: xids 0..8.
            f.write_all_at(&[0b0100_0100, 0b1000_0000], 0).unwrap();
        }
        let c = Clog::open(v, Xid(3), Xid(3)).unwrap();
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::Committed); // bits 6..7 of byte 0
        assert_eq!(c.status(Xid(7)).unwrap(), XidStatus::Aborted);
        assert_eq!(c.status(Xid(9)).unwrap(), XidStatus::InProgress);
    }

    #[test]
    fn redo_set_is_idempotent_and_loads_pages() {
        let v = vfs();
        {
            let c = Clog::open(Arc::clone(&v), Xid(3), Xid(3)).unwrap();
            c.set_status(Xid(3), XidStatus::Committed).unwrap();
            c.flush().unwrap();
        }
        let far = Xid(XIDS_PER_PAGE * 2 + 5);
        let c = Clog::open(v, far, Xid(3)).unwrap();
        // Page 0 is not in memory: it is read from disk.
        c.set_status_redo(Xid(3), XidStatus::Committed).unwrap();
        c.set_status_redo(Xid(3), XidStatus::Committed).unwrap();
        let e = c.set_status_redo(Xid(3), XidStatus::Aborted).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        c.set_status_redo(Xid(4), XidStatus::Aborted).unwrap();
        assert_eq!(c.status(Xid(4)).unwrap(), XidStatus::Aborted);
        assert!(c.set_status_redo(Xid(4), XidStatus::InProgress).is_err());
        assert!(c.set_status_redo(Xid(1), XidStatus::Committed).is_err());
        // A page past the end of the log is created as zeros.
        c.set_status_redo(Xid(XIDS_PER_PAGE * 7), XidStatus::Committed)
            .unwrap();
        assert!(c.has_dirty());
    }

    #[test]
    fn load_page_for_keeps_disk_content() {
        let v = vfs();
        {
            let c = Clog::open(Arc::clone(&v), Xid(3), Xid(3)).unwrap();
            c.set_status(Xid(10), XidStatus::Committed).unwrap();
            c.flush().unwrap();
        }
        // Opened for a later page, as after recovery advanced `next_xid`.
        let c = Clog::open(Arc::clone(&v), Xid(XIDS_PER_PAGE + 1), Xid(3)).unwrap();
        c.load_page_for(Xid(11)).unwrap();
        c.ensure_page_for(Xid(11));
        assert_eq!(c.status(Xid(10)).unwrap(), XidStatus::Committed);
        c.set_status(Xid(11), XidStatus::Aborted).unwrap();
        assert_eq!(c.status(Xid(10)).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn flush_failure_is_panic() {
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let sim = SimVfs::new(7);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        let c = Clog::open(v, Xid(3), Xid(3)).unwrap();
        c.set_status(Xid(3), XidStatus::Committed).unwrap();
        sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: None,
                nth: None,
                probability: None,
                effect: FaultEffect::Error(io::ErrorKind::Other),
            }],
        });
        let e = c.flush().unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
    }

    #[test]
    fn oldest_is_kept_and_raised_to_first_normal() {
        let c = Clog::open(vfs(), Xid(3), Xid(0)).unwrap();
        assert_eq!(c.oldest(), Xid::FIRST_NORMAL);
        let c = Clog::open(vfs(), Xid(50), Xid(42)).unwrap();
        assert_eq!(c.oldest(), Xid(42));
    }

    #[test]
    fn concurrent_set_status_on_one_byte_loses_nothing() {
        // XIDs 4..8 share one byte; four threads set them at the same time.
        let c = Arc::new(Clog::open(vfs(), Xid(3), Xid(3)).unwrap());
        for round in 0..200u64 {
            let base = 4 + round * 4;
            for x in 0..4 {
                c.ensure_page_for(Xid(base + x));
            }
            let hs: Vec<_> = (0..4u64)
                .map(|x| {
                    let c = Arc::clone(&c);
                    std::thread::spawn(move || {
                        let s = if x % 2 == 0 {
                            XidStatus::Committed
                        } else {
                            XidStatus::Aborted
                        };
                        c.set_status(Xid(base + x), s).unwrap();
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            for x in 0..4u64 {
                let want = if x % 2 == 0 {
                    XidStatus::Committed
                } else {
                    XidStatus::Aborted
                };
                assert_eq!(c.status(Xid(base + x)).unwrap(), want);
            }
        }
    }

    #[test]
    fn racing_commit_and_abort_of_one_xid_exactly_one_wins() {
        let c = Arc::new(Clog::open(vfs(), Xid(3), Xid(3)).unwrap());
        for x in 3..300u64 {
            c.ensure_page_for(Xid(x));
            let (a, b) = (Arc::clone(&c), Arc::clone(&c));
            let ha = std::thread::spawn(move || a.set_status(Xid(x), XidStatus::Committed));
            let hb = std::thread::spawn(move || b.set_status(Xid(x), XidStatus::Aborted));
            let (ra, rb) = (ha.join().unwrap(), hb.join().unwrap());
            assert!(ra.is_ok() ^ rb.is_ok(), "exactly one must win");
            let st = c.status(Xid(x)).unwrap();
            assert_eq!(st == XidStatus::Committed, ra.is_ok());
        }
    }

    #[test]
    fn flush_during_concurrent_sets_keeps_every_status() {
        let v = vfs();
        let c = Arc::new(Clog::open(Arc::clone(&v), Xid(3), Xid(3)).unwrap());
        let writer = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                for x in 3..2000u64 {
                    c.set_status(Xid(x), XidStatus::Committed).unwrap();
                }
            })
        };
        for _ in 0..20 {
            c.flush().unwrap();
        }
        writer.join().unwrap();
        c.flush().unwrap();
        let reopened = Clog::open(v, Xid(2000), Xid(3)).unwrap();
        for x in 3..2000u64 {
            assert_eq!(reopened.status(Xid(x)).unwrap(), XidStatus::Committed);
        }
    }
}
