//! Commit log `pg_xact/` (`m2.md` §3.8, §6.7.2).
//!
//! Two bits per XID, 32768 XIDs per 8KB page, 32 pages per segment file
//! `pg_xact/<segment as 12 upper-case hex digits>`. Missing files and pages
//! past the end of a file read as zeros (`IN_PROGRESS`).
//!
//! Pages are kept in memory and never evicted (M2). The internal mutex is a
//! leaf lock: no I/O happens while it is held.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::Xid;
use crate::error::{Error, Result, Severity};
use crate::storage::vfs::{OpenMode, Vfs};
use crate::util::sync::lock;

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
    data: Box<[u8; PAGE_SIZE]>,
    dirty: bool,
}

impl ClogPage {
    fn zeroed() -> ClogPage {
        ClogPage {
            data: Box::new([0; PAGE_SIZE]),
            dirty: false,
        }
    }
}

#[derive(Debug)]
pub struct Clog {
    vfs: Arc<dyn Vfs>,
    /// Page number -> page (with a dirty flag). Leaf lock.
    pages: Mutex<HashMap<u64, ClogPage>>,
    /// Serializes `flush` so that a second flush cannot return before the
    /// first one has written the pages whose dirty flags it cleared.
    flush_lock: Mutex<()>,
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
    /// are new, so they need no I/O.
    pub fn open(vfs: Arc<dyn Vfs>, next_xid: Xid) -> Result<Clog> {
        let clog = Clog {
            vfs,
            pages: Mutex::new(HashMap::new()),
            flush_lock: Mutex::new(()),
        };
        let pageno = page_of(next_xid);
        let page = clog.read_page(pageno)?;
        lock(&clog.pages)?.insert(pageno, page);
        Ok(clog)
    }

    /// Reads a page from disk (zeros for a missing file or a short file).
    fn read_page(&self, pageno: u64) -> Result<ClogPage> {
        let mut page = ClogPage::zeroed();
        let path = segment_path(pageno / PAGES_PER_SEGMENT);
        let io_err = |e: &io::Error| {
            Error::from_io(e, format!("could not read file \"{}\"", path.display()))
        };
        if !self.vfs.exists(&path).map_err(|e| io_err(&e))? {
            return Ok(page);
        }
        let file = match self.vfs.open(&path, OpenMode::ReadOnly) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(page),
            Err(e) => return Err(io_err(&e)),
        };
        let offset = (pageno % PAGES_PER_SEGMENT) * PAGE_SIZE as u64;
        let size = file.size().map_err(|e| io_err(&e))?;
        if size > offset {
            // A torn or short last page reads as zeros past the end.
            #[allow(clippy::cast_possible_truncation)]
            let n = (size - offset).min(PAGE_SIZE as u64) as usize;
            file.read_exact_at(&mut page.data[..n], offset)
                .map_err(|e| io_err(&e))?;
        }
        Ok(page)
    }

    /// The status of `xid`. `Xid::INVALID` is an internal error; the
    /// bootstrap and frozen XIDs are committed without consulting the log.
    /// May read a page from disk, so never call it inside the `proc` mutex.
    pub fn status(&self, xid: Xid) -> Result<XidStatus> {
        if xid == Xid::INVALID {
            return Err(Error::internal("clog lookup of the invalid transaction ID"));
        }
        if !xid.is_normal() {
            return Ok(XidStatus::Committed);
        }
        let pageno = page_of(xid);
        let (byte, shift) = slot_of(xid);
        {
            let pages = lock(&self.pages)?;
            if let Some(p) = pages.get(&pageno) {
                return decode((p.data[byte] >> shift) & 3);
            }
        }
        // Not in memory: read without holding the lock, then keep the first
        // copy inserted (a concurrent `ensure_page_for` / `set_status` wins).
        let loaded = self.read_page(pageno)?;
        let mut pages = lock(&self.pages)?;
        let p = pages.entry(pageno).or_insert(loaded);
        decode((p.data[byte] >> shift) & 3)
    }

    /// Called by `TxnManager` inside the `proc` mutex when assigning an
    /// XID (`ExtendCLOG`). Creates a zero page if absent; no I/O.
    ///
    /// A poisoned lock is ignored here: the failure then surfaces in
    /// `set_status` (which reports it as an error).
    pub fn ensure_page_for(&self, xid: Xid) {
        if let Ok(mut pages) = self.pages.lock() {
            pages.entry(page_of(xid)).or_insert_with(ClogPage::zeroed);
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
        let mut pages = lock(&self.pages)?;
        let page = pages.get_mut(&page_of(xid)).ok_or_else(|| {
            Error::internal(format!(
                "commit log page for transaction {} is not in memory",
                xid.0
            ))
        })?;
        let current = decode((page.data[byte] >> shift) & 3)?;
        if current != XidStatus::InProgress {
            return Err(Error::internal(format!(
                "transaction {} is already {current:?}, cannot become {s:?}",
                xid.0
            )));
        }
        page.data[byte] |= (s as u8) << shift;
        page.dirty = true;
        Ok(())
    }

    /// Writes dirty pages and `sync_data`s them (checkpoint and shutdown).
    /// Any I/O failure is `Severity::Panic` (no retry after a failed fsync).
    pub fn flush(&self) -> Result<()> {
        let _serial = lock(&self.flush_lock)?;
        // Copy the dirty pages and clear their flags in one critical
        // section: a `set_status` after the copy sets the flag again.
        let mut dirty: BTreeMap<u64, Box<[u8; PAGE_SIZE]>> = BTreeMap::new();
        {
            let mut pages = lock(&self.pages)?;
            for (&no, p) in pages.iter_mut().filter(|(_, p)| p.dirty) {
                dirty.insert(no, p.data.clone());
                p.dirty = false;
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
        lock(&self.pages).unwrap().values().any(|p| p.dirty)
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
        let c = Clog::open(vfs(), Xid(3)).unwrap();
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
        let c = Clog::open(vfs(), Xid(3)).unwrap();
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
        let c = Clog::open(vfs(), Xid(3)).unwrap();
        c.set_status(Xid(3), XidStatus::Committed).unwrap();
        assert!(c.set_status(Xid(3), XidStatus::Aborted).is_err());
        assert!(c.set_status(Xid(3), XidStatus::Committed).is_err());
        assert!(c.set_status(Xid(3), XidStatus::InProgress).is_err());
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn set_status_without_page_is_internal_error() {
        let c = Clog::open(vfs(), Xid(3)).unwrap();
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
            let c = Clog::open(Arc::clone(&v), Xid(3)).unwrap();
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
        let c = Clog::open(v, Xid(5)).unwrap();
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
        let c = Clog::open(vfs(), Xid(3)).unwrap();
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
        let c = Clog::open(v, Xid(3)).unwrap();
        assert_eq!(c.status(Xid(3)).unwrap(), XidStatus::Committed); // bits 6..7 of byte 0
        assert_eq!(c.status(Xid(7)).unwrap(), XidStatus::Aborted);
        assert_eq!(c.status(Xid(9)).unwrap(), XidStatus::InProgress);
    }

    #[test]
    fn flush_failure_is_panic() {
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let sim = SimVfs::new(7);
        sim.create_dir_all(Path::new("pg_xact")).unwrap();
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        let c = Clog::open(v, Xid(3)).unwrap();
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
}
