//! Data directory layout, `YUZHU_VERSION`, the pid file, OID allocation and
//! database directory copy (`m2.md` §3.9, §4.3a, §6.9). Uses `Vfs` only.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::control::{ControlFileHandle, FIRST_NORMAL_OID};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::OID_PREFETCH;
use crate::storage::vfs::{OpenMode, Vfs, VfsLock};
use crate::types::Oid;
use crate::util::sync::lock;

/// Version of the data directory format (`YUZHU_VERSION` contains `"1\n"`).
pub const DATA_FORMAT_VERSION: u32 = 1;
pub const VERSION_FILE: &str = "YUZHU_VERSION";
pub const PID_FILE: &str = "yuzhu.pid";

fn io_error(e: &std::io::Error, what: &str, path: &Path) -> Error {
    Error::from_io(e, format!("could not {what} \"{}\"", path.display()))
}

fn read_to_string(vfs: &dyn Vfs, path: &Path) -> std::io::Result<String> {
    let f = vfs.open(path, OpenMode::ReadOnly)?;
    let mut buf = vec![0u8; usize::try_from(f.size()?).unwrap_or(0)];
    f.read_exact_at(&mut buf, 0)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Checks `YUZHU_VERSION` in `dir` (§3.9). `FATAL` if it is missing or differs.
pub fn check_version_file(vfs: &dyn Vfs, dir: &Path) -> Result<()> {
    let path = dir.join(VERSION_FILE);
    let content = match read_to_string(vfs, &path) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(Error::new(
                sqlstate::UNDEFINED_FILE,
                format!("\"{}\" is not a valid data directory", dir.display()),
            )
            .with_severity(Severity::Fatal)
            .with_detail(format!("File \"{VERSION_FILE}\" is missing.")));
        }
        Err(e) => return Err(io_error(&e, "read file", &path).with_severity(Severity::Fatal)),
    };
    if content.trim() == DATA_FORMAT_VERSION.to_string() {
        return Ok(());
    }
    Err(
        Error::internal("database files are incompatible with server")
            .with_severity(Severity::Fatal)
            .with_detail(format!(
                "The data directory has format version \"{}\", but this server expects \"{DATA_FORMAT_VERSION}\".",
                content.trim()
            ))
            .with_hint("It looks like you need to initdb."),
    )
}

/// Writes `YUZHU_VERSION` (`"1\n"`), replacing an existing file. With `sync`,
/// the file and `dir` are fsynced.
pub fn write_version_file(vfs: &dyn Vfs, dir: &Path, sync: bool) -> Result<()> {
    let path = dir.join(VERSION_FILE);
    let write = || -> std::io::Result<()> {
        let f = match vfs.open(&path, OpenMode::CreateNew) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let f = vfs.open(&path, OpenMode::ReadWrite)?;
                f.set_len(0)?;
                f
            }
            Err(e) => return Err(e),
        };
        f.write_all_at(format!("{DATA_FORMAT_VERSION}\n").as_bytes(), 0)?;
        if sync {
            f.sync_all()?;
            vfs.sync_dir(dir)?;
        }
        Ok(())
    };
    write().map_err(|e| io_error(&e, "write file", &path))
}

/// `yuzhu.pid`: locked while the server runs (§3.9). Dropping releases the
/// lock; the file is removed explicitly by [`PidFile::release`].
#[derive(Debug)]
pub struct PidFile {
    lock: Box<dyn VfsLock>,
}

impl PidFile {
    /// Locks `yuzhu.pid` and writes four lines: PID, data directory, start
    /// time (UNIX seconds) and port. A file left behind by a crashed server
    /// (no lock held) is overwritten.
    pub fn acquire(vfs: &dyn Vfs, data_dir_display: &str, port: u16) -> Result<PidFile> {
        let path = Path::new(PID_FILE);
        let lock = match vfs.lock_file(path) {
            Ok(l) => l,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                let pid = read_to_string(vfs, path)
                    .ok()
                    .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                return Err(Error::new(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("lock file \"{PID_FILE}\" already exists"),
                )
                .with_severity(Severity::Fatal)
                .with_hint(format!(
                    "Is another yuzhu-server (PID {pid}) running in data directory \"{data_dir_display}\"?"
                )));
            }
            Err(e) => return Err(io_error(&e, "lock file", path).with_severity(Severity::Fatal)),
        };
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let content = format!(
            "{}\n{data_dir_display}\n{started}\n{port}\n",
            std::process::id()
        );
        let write = || -> std::io::Result<()> {
            let f = vfs.open(path, OpenMode::ReadWrite)?;
            f.set_len(0)?;
            f.write_all_at(content.as_bytes(), 0)
        };
        write().map_err(|e| io_error(&e, "write file", path).with_severity(Severity::Fatal))?;
        Ok(PidFile { lock })
    }

    /// Removes the file, then releases the lock.
    pub fn release(self, vfs: &dyn Vfs) -> Result<()> {
        let r = match vfs.remove_file(Path::new(PID_FILE)) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(io_error(&e, "remove file", Path::new(PID_FILE)))
            }
            _ => Ok(()),
        };
        drop(self.lock);
        r
    }
}

/// OID allocation with look-ahead recorded in the control file (§6.9.2).
///
/// `(next, limit)`: OIDs in `next..limit` are covered by the value already
/// written to the control file. OIDs start at 16384 and wrap there after
/// `u32::MAX - 1`.
#[derive(Debug)]
pub struct OidAllocator {
    state: Mutex<(Oid, Oid)>,
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
    /// `CatalogStore::get_new_oid`). When the reservation is used up, the new
    /// limit is written to the control file (and synced) first.
    pub fn next_raw(&self) -> Result<Oid> {
        let mut st = lock(&self.state)?;
        let (mut next, mut limit) = *st;
        if next < FIRST_NORMAL_OID {
            next = FIRST_NORMAL_OID;
            limit = next;
        }
        if next == limit {
            if next == u32::MAX {
                next = FIRST_NORMAL_OID;
            }
            let new_limit = next.saturating_add(OID_PREFETCH);
            self.control.update(|c| c.next_oid = new_limit)?;
            limit = new_limit;
        }
        let oid = next;
        *st = (oid + 1, limit);
        Ok(oid)
    }

    /// The look-ahead limit the checkpoint writes to the control file. Note:
    /// the limit may advance between reading it and writing it, so callers
    /// must write `max(control.next_oid, limit)`, not the bare value.
    pub fn limit(&self) -> Oid {
        crate::util::sync::lock_ignore_poison(&self.state).1
    }
}

/// Copies `base/<src>/` to `base/<dst>/` (all entries are treated as
/// regular files). `base/<dst>` must not exist. With `sync`, every file and
/// the directories are fsynced. On failure the partial copy is removed.
/// Flushing buffers first is the caller's job (§6.9.3).
pub fn copy_database_dir(vfs: &dyn Vfs, src: Oid, dst: Oid, sync: bool) -> Result<()> {
    let base = PathBuf::from("base");
    let src_dir = base.join(src.to_string());
    let dst_dir = base.join(dst.to_string());
    vfs.create_dir(&dst_dir)
        .map_err(|e| io_error(&e, "create directory", &dst_dir))?;
    let result = copy_files(vfs, &src_dir, &dst_dir, sync).and_then(|()| {
        if sync {
            vfs.sync_dir(&dst_dir)
                .and_then(|()| vfs.sync_dir(&base))
                .map_err(|e| io_error(&e, "fsync directory", &dst_dir))?;
        }
        Ok(())
    });
    if result.is_err() {
        let _ = vfs.remove_dir_all(&dst_dir);
    }
    result
}

fn copy_files(vfs: &dyn Vfs, src_dir: &Path, dst_dir: &Path, sync: bool) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    let entries = vfs
        .read_dir(src_dir)
        .map_err(|e| io_error(&e, "read directory", src_dir))?;
    let mut buf = vec![0u8; CHUNK];
    for entry in entries {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let to = dst_dir.join(name);
        let copy = |buf: &mut Vec<u8>| -> std::io::Result<()> {
            let from = vfs.open(&entry, OpenMode::ReadOnly)?;
            let out = vfs.open(&to, OpenMode::CreateNew)?;
            let size = from.size()?;
            let mut off = 0u64;
            while off < size {
                let n = usize::try_from(size - off).map_or(CHUNK, |r| r.min(CHUNK));
                from.read_exact_at(&mut buf[..n], off)?;
                out.write_all_at(&buf[..n], off)?;
                off += n as u64;
            }
            if sync {
                out.sync_all()?;
            }
            Ok(())
        };
        copy(&mut buf).map_err(|e| io_error(&e, "copy file", &entry))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlData;
    use crate::storage::vfs::sim::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};

    fn sim() -> (SimVfs, Arc<dyn Vfs>) {
        let s = SimVfs::new(1);
        let v: Arc<dyn Vfs> = Arc::new(s.clone());
        (s, v)
    }

    fn control(vfs: &Arc<dyn Vfs>) -> Arc<ControlFileHandle> {
        Arc::new(ControlFileHandle::create(vfs, &ControlData::initial(7, 9, 131_072)).unwrap())
    }

    #[test]
    fn version_file() {
        let (s, vfs) = sim();
        let dir = Path::new("");
        let e = check_version_file(&*vfs, dir).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.message.contains("not a valid data directory"));
        write_version_file(&*vfs, dir, true).unwrap();
        check_version_file(&*vfs, dir).unwrap();
        assert_eq!(s.file_contents(Path::new(VERSION_FILE)).unwrap(), b"1\n");
        // Overwrite with a different version.
        let f = vfs
            .open(Path::new(VERSION_FILE), OpenMode::ReadWrite)
            .unwrap();
        f.write_all_at(b"2\n", 0).unwrap();
        let e = check_version_file(&*vfs, dir).unwrap_err();
        assert!(e.message.contains("incompatible"));
        assert!(e.hint.unwrap().contains("initdb"));
        // Rewriting restores it (and truncates).
        write_version_file(&*vfs, dir, false).unwrap();
        check_version_file(&*vfs, dir).unwrap();
        // Per-database directory.
        vfs.create_dir_all(Path::new("base/5")).unwrap();
        write_version_file(&*vfs, Path::new("base/5"), true).unwrap();
        check_version_file(&*vfs, Path::new("base/5")).unwrap();
        // A synced version file survives a crash.
        let after = s.crash(CrashMode::DropUnsynced);
        check_version_file(&after, Path::new("base/5")).unwrap_err(); // dir "base" never synced
        let (s2, v2) = sim();
        v2.create_dir(Path::new("base")).unwrap();
        v2.sync_dir(Path::new("")).unwrap();
        write_version_file(&*v2, Path::new("base"), true).unwrap();
        check_version_file(&s2.crash(CrashMode::DropUnsynced), Path::new("base")).unwrap();
    }

    #[test]
    fn pid_file_lock_and_release() {
        let (s, vfs) = sim();
        let pid = PidFile::acquire(&*vfs, "/data", 5432).unwrap();
        let text = String::from_utf8(s.file_contents(Path::new(PID_FILE)).unwrap()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], std::process::id().to_string());
        assert_eq!(lines[1], "/data");
        assert!(lines[2].parse::<u64>().is_ok());
        assert_eq!(lines[3], "5432");
        let e = PidFile::acquire(&*vfs, "/data", 5432).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.message, "lock file \"yuzhu.pid\" already exists");
        let hint = e.hint.unwrap();
        assert!(
            hint.contains(&format!("PID {}", std::process::id())),
            "{hint}"
        );
        assert!(hint.contains("\"/data\""));
        // The failed attempt did not disturb the content.
        assert!(
            s.file_contents(Path::new(PID_FILE))
                .unwrap()
                .starts_with(lines[0].as_bytes())
        );
        pid.release(&*vfs).unwrap();
        assert!(!vfs.exists(Path::new(PID_FILE)).unwrap());
        PidFile::acquire(&*vfs, "/data", 1).unwrap();
    }

    #[test]
    fn stale_pid_file_is_overwritten() {
        let (s, vfs) = sim();
        let pid = PidFile::acquire(&*vfs, "/old-and-long-directory-name", 1).unwrap();
        drop(pid); // lock gone, file left behind (like a killed server)
        assert!(vfs.exists(Path::new(PID_FILE)).unwrap());
        PidFile::acquire(&*vfs, "/d", 2).unwrap();
        let text = String::from_utf8(s.file_contents(Path::new(PID_FILE)).unwrap()).unwrap();
        assert_eq!(text.lines().nth(1), Some("/d"));
        assert_eq!(text.lines().count(), 4);
    }

    #[test]
    fn oid_allocator_reserves_in_batches() {
        let (s, vfs) = sim();
        let ctl = control(&vfs);
        let oids = OidAllocator::new(Arc::clone(&ctl));
        assert_eq!(oids.limit(), 16384);
        let before = s.stats().syncs;
        assert_eq!(oids.next_raw().unwrap(), 16384);
        assert_eq!(ctl.get().next_oid, 16384 + OID_PREFETCH);
        assert_eq!(oids.limit(), 16384 + OID_PREFETCH);
        for i in 1..OID_PREFETCH {
            assert_eq!(oids.next_raw().unwrap(), 16384 + i);
        }
        assert_eq!(s.stats().syncs, before + 1, "one control write per batch");
        assert_eq!(oids.next_raw().unwrap(), 16384 + OID_PREFETCH);
        assert_eq!(ctl.get().next_oid, 16384 + 2 * OID_PREFETCH);
        // After a restart the reserved-but-unused OIDs are skipped.
        let ctl2 = Arc::new(ControlFileHandle::open(&vfs).unwrap());
        let oids2 = OidAllocator::new(ctl2);
        assert_eq!(oids2.next_raw().unwrap(), 16384 + 2 * OID_PREFETCH);
    }

    #[test]
    fn oid_allocator_wraps_to_first_normal_oid() {
        let (_s, vfs) = sim();
        let mut d = ControlData::initial(7, 9, 131_072);
        d.next_oid = u32::MAX - 2;
        let ctl = Arc::new(ControlFileHandle::create(&vfs, &d).unwrap());
        let oids = OidAllocator::new(Arc::clone(&ctl));
        assert_eq!(oids.next_raw().unwrap(), u32::MAX - 2);
        assert_eq!(oids.next_raw().unwrap(), u32::MAX - 1);
        // u32::MAX is never handed out.
        assert_eq!(oids.next_raw().unwrap(), FIRST_NORMAL_OID);
        assert_eq!(ctl.get().next_oid, FIRST_NORMAL_OID + OID_PREFETCH);
        assert_eq!(oids.next_raw().unwrap(), FIRST_NORMAL_OID + 1);
    }

    #[test]
    fn oid_allocator_never_hands_out_reserved_oids_below_normal() {
        let (_s, vfs) = sim();
        let mut d = ControlData::initial(7, 9, 131_072);
        d.next_oid = 100;
        let ctl = Arc::new(ControlFileHandle::create(&vfs, &d).unwrap());
        let oids = OidAllocator::new(ctl);
        assert_eq!(oids.next_raw().unwrap(), FIRST_NORMAL_OID);
    }

    #[test]
    fn oid_allocator_failure_does_not_advance() {
        let (s, vfs) = sim();
        let ctl = control(&vfs);
        let oids = OidAllocator::new(Arc::clone(&ctl));
        s.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::Error(ErrorKind::Other),
            }],
        });
        let e = oids.next_raw().unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(oids.next_raw().unwrap(), 16384);
    }

    #[test]
    fn oid_allocator_is_unique_across_threads() {
        let (_s, vfs) = sim();
        let oids = Arc::new(OidAllocator::new(control(&vfs)));
        let mut all: Vec<Oid> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..4)
                .map(|_| {
                    let o = Arc::clone(&oids);
                    sc.spawn(move || (0..3000).map(|_| o.next_raw().unwrap()).collect::<Vec<_>>())
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 12_000);
    }

    fn populate(vfs: &Arc<dyn Vfs>) {
        vfs.create_dir_all(Path::new("base/1")).unwrap();
        for (name, len, byte) in [
            ("1259", 8192usize * 3, 1u8),
            ("16384", 10, 2),
            ("16384.1", (1 << 20) + 5, 3),
            ("empty", 0, 0),
        ] {
            let f = vfs
                .open(&Path::new("base/1").join(name), OpenMode::CreateNew)
                .unwrap();
            f.write_all_at(&vec![byte; len], 0).unwrap();
        }
    }

    #[test]
    fn copy_database_dir_copies_all_files() {
        let (s, vfs) = sim();
        populate(&vfs);
        vfs.sync_dir(Path::new("")).unwrap();
        vfs.sync_dir(Path::new("base")).unwrap();
        copy_database_dir(&*vfs, 1, 6, true).unwrap();
        let src = vfs.read_dir(Path::new("base/1")).unwrap();
        let dst = vfs.read_dir(Path::new("base/6")).unwrap();
        assert_eq!(src.len(), 4);
        for (a, b) in src.iter().zip(&dst) {
            assert_eq!(a.file_name(), b.file_name());
            assert_eq!(s.file_contents(a), s.file_contents(b), "{}", a.display());
        }
        // Durable after a crash (sync = true).
        let after = s.crash(CrashMode::DropUnsynced);
        // base/1 itself was never synced in this test, but base/6 was
        // (with its files) via sync_dir(base).
        assert_eq!(
            after
                .file_contents(Path::new("base/6/16384.1"))
                .unwrap()
                .len(),
            (1 << 20) + 5
        );
        // Destination exists: refuse and leave it alone.
        assert!(copy_database_dir(&after, 1, 6, true).is_err());
        assert!(after.exists(Path::new("base/6/1259")).unwrap());
    }

    #[test]
    fn copy_database_dir_without_sync_is_not_durable() {
        let (s, vfs) = sim();
        populate(&vfs);
        copy_database_dir(&*vfs, 1, 6, false).unwrap();
        let after = s.crash(CrashMode::DropUnsynced);
        assert!(!after.exists(Path::new("base/6")).unwrap());
    }

    #[test]
    fn copy_database_dir_cleans_up_on_failure() {
        let (s, vfs) = sim();
        populate(&vfs);
        s.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some(PathBuf::from("base/6")),
                nth: Some(2),
                probability: None,
                effect: FaultEffect::Error(ErrorKind::StorageFull),
            }],
        });
        let e = copy_database_dir(&*vfs, 1, 6, true).unwrap_err();
        assert_eq!(e.sqlstate.code(), "53100");
        assert!(!vfs.exists(Path::new("base/6")).unwrap());
        // A missing source is reported too.
        assert!(copy_database_dir(&*vfs, 99, 7, true).is_err());
        assert!(!vfs.exists(Path::new("base/7")).unwrap());
    }
}
