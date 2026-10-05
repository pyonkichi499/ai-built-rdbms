//! initdb: bootstrap of template1, copy to template0 / postgres
//! (`m2.md` §5.8).
//!
//! The `Vfs` is rooted at the data directory. The control file is written
//! last: its presence marks a finished initdb. On failure everything under
//! the root is removed again.

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use crate::catalog::rows::{self, InitParams};
use crate::catalog::store::{CatalogStore, SharedCatalogStore};
use crate::control::{
    ControlData, ControlFileHandle, FIRST_NORMAL_OID, generate_system_identifier,
};
use crate::datadir::{copy_database_dir, write_version_file};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, sqlstate};
use crate::storage::stack::{StackConfig, StorageStack};
use crate::storage::vfs::Vfs;
use crate::storage::{TableStore, WriteCtx};
use crate::txn::Xid;
use crate::types::Oid;
use crate::wal::xlog::{CheckpointRecord, CheckpointRecordKind};
use crate::wal::{
    DEFAULT_WAL_SEGMENT_SIZE, MAX_WAL_SEGMENT_SIZE, MIN_WAL_SEGMENT_SIZE, Wal, WalConfig,
};

/// OIDs of the three databases created by initdb (PostgreSQL's values).
pub const TEMPLATE1_OID: Oid = 1;
pub const TEMPLATE0_OID: Oid = 4;
pub const POSTGRES_OID: Oid = 5;

/// Frames used while bootstrapping (the catalogs are a few hundred KB).
const BOOTSTRAP_FRAMES: usize = 256;

#[derive(Debug, Clone)]
pub struct InitdbOptions {
    pub superuser: String,
    pub no_sync: bool,
    pub rel_seg_blocks: u32,
    /// WAL segment size in bytes (a power of two in 2 MiB..=1 GiB).
    pub wal_segment_size: u32,
}

impl InitdbOptions {
    /// Defaults for everything but the superuser name.
    pub fn new(superuser: impl Into<String>) -> Self {
        InitdbOptions {
            superuser: superuser.into(),
            no_sync: false,
            rel_seg_blocks: crate::storage::DEFAULT_RELSEG_SIZE,
            wal_segment_size: DEFAULT_WAL_SEGMENT_SIZE,
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn initdb(vfs: Arc<dyn Vfs>, opts: &InitdbOptions) -> Result<()> {
    if opts.superuser.is_empty() {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "the superuser name must not be empty",
        ));
    }
    if opts.rel_seg_blocks == 0 {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "rel_seg_blocks must be positive",
        ));
    }
    if !opts.wal_segment_size.is_power_of_two()
        || !(MIN_WAL_SEGMENT_SIZE..=MAX_WAL_SEGMENT_SIZE).contains(&opts.wal_segment_size)
    {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "WAL segment size must be a power of two between {MIN_WAL_SEGMENT_SIZE} and {MAX_WAL_SEGMENT_SIZE} bytes"
            ),
        ));
    }
    // 1. The directory must not exist or must be empty. Nothing is removed
    // when this check fails.
    match vfs.read_dir(Path::new("")) {
        Ok(entries) if !entries.is_empty() => {
            return Err(Error::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "data directory exists but is not empty",
            )
            .with_hint("Remove the directory or choose another one."));
        }
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(Error::from_io(&e, "could not read the data directory")),
    }
    let result = run(&vfs, opts);
    if result.is_err() {
        remove_contents(&*vfs);
    }
    result
}

fn io_ctx(what: &'static str, path: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::from_io(&e, format!("could not {what} \"{path}\""))
}

fn run(vfs: &Arc<dyn Vfs>, opts: &InitdbOptions) -> Result<()> {
    let sync = !opts.no_sync;
    let root = Path::new("");

    // 2. Directories and the version file.
    vfs.create_dir_all(root)
        .map_err(io_ctx("create directory", "."))?;
    for dir in ["global", "base", "base/1", "pg_xact", "pg_wal"] {
        vfs.create_dir_all(Path::new(dir))
            .map_err(|e| Error::from_io(&e, format!("could not create directory \"{dir}\"")))?;
    }
    write_version_file(&**vfs, root, sync)?;

    // 2a. The WAL (segment 1, write mode).
    let system_identifier = generate_system_identifier();
    let wal = Wal::initialize(
        Arc::clone(vfs),
        WalConfig {
            segment_size: opts.wal_segment_size,
            system_identifier,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        },
    )?;

    // 3. template1: the catalogs are written through the heap (and so its
    // WAL) with xmin = BOOTSTRAP (visible to every snapshot).
    let stack = StorageStack::new(
        Arc::clone(vfs),
        &StackConfig {
            rel_seg_blocks: opts.rel_seg_blocks,
            nframes: BOOTSTRAP_FRAMES,
            knobs: DebugKnobs::default(),
        },
        Arc::clone(&wal),
        Xid::FIRST_NORMAL,
    )?;
    let storage: Arc<dyn TableStore> = stack.heap.clone();
    let params = InitParams {
        superuser: opts.superuser.clone(),
    };
    let w = WriteCtx {
        xid: Xid::BOOTSTRAP,
        cid: 0,
    };
    SharedCatalogStore::new(Arc::clone(&storage)).bootstrap(&w, &params)?;
    CatalogStore::new(TEMPLATE1_OID, storage).bootstrap(&w, &params)?;

    // 4. The equivalent of a shutdown checkpoint: the REDO point is the
    // insert position, every page is written (WAL before data), the files
    // are made durable, then CHECKPOINT_SHUTDOWN is logged and flushed.
    // (`Wal::flush` always syncs; `no_sync` only skips the data files.)
    let redo = wal.begin_checkpoint_quiet()?;
    stack.pool.flush_all_for_checkpoint()?;
    stack.clog.flush()?;
    if sync {
        stack.smgr.sync_pending()?;
    }
    if stack.pool.pinned_frames() != 0 {
        return Err(Error::internal("buffers are still pinned after bootstrap"));
    }
    let checkpoint = CheckpointRecord {
        redo,
        next_xid: Xid::FIRST_NORMAL,
        oldest_xid: Xid::FIRST_NORMAL,
        next_oid: FIRST_NORMAL_OID,
        kind: CheckpointRecordKind::Shutdown,
        full_page_writes: true,
        time: unix_seconds(),
    };
    let inserted = wal.insert(checkpoint.builder())?;
    wal.flush(inserted.end)?;
    drop(stack);
    drop(wal);
    if sync {
        for dir in ["global", "base/1", "base", "pg_wal", ""] {
            vfs.sync_dir(Path::new(dir))
                .map_err(|e| Error::from_io(&e, format!("could not fsync directory \"{dir}\"")))?;
        }
    }

    // 5. base/1/YUZHU_VERSION
    write_version_file(&**vfs, Path::new("base/1"), sync)?;

    // 6. template0 and postgres are copies of template1. The copies are not
    // logged: their page LSNs lie before the REDO point above, so the first
    // change after start-up carries a full-page image.
    copy_database_dir(&**vfs, TEMPLATE1_OID, TEMPLATE0_OID, sync)?;
    copy_database_dir(&**vfs, TEMPLATE1_OID, POSTGRES_OID, sync)?;

    // 7. pg_xact/ stays empty. The control file marks the end.
    let mut data =
        ControlData::initial(system_identifier, rows::builtin_hash(), opts.rel_seg_blocks);
    data.wal_segment_size = opts.wal_segment_size;
    data.checkpoint_lsn = inserted.start.0;
    data.redo_lsn = redo.0;
    ControlFileHandle::create(vfs, &data)?;
    Ok(())
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Best-effort removal of everything under the root (failed initdb).
fn remove_contents(vfs: &dyn Vfs) {
    let Ok(entries) = vfs.read_dir(Path::new("")) else {
        return;
    };
    for e in entries {
        if vfs.remove_dir_all(&e).is_err() {
            let _ = vfs.remove_file(&e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::vfs::{
        CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, OpenMode, SimVfs,
    };

    fn opts() -> InitdbOptions {
        InitdbOptions::new("postgres")
    }

    #[test]
    fn creates_layout_and_three_databases() {
        let sim = SimVfs::new(1);
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        initdb(Arc::clone(&vfs), &opts()).unwrap();
        for p in [
            "YUZHU_VERSION",
            "global/yuzhu_control",
            "global/1262",
            "global/1260",
            "global/1213",
            "base/1/1259",
            "base/4/1259",
            "base/5/1259",
            "base/1/YUZHU_VERSION",
            "base/5/YUZHU_VERSION",
            "pg_xact",
            "pg_wal",
        ] {
            assert!(vfs.exists(Path::new(p)).unwrap(), "{p} is missing");
        }
        // The copies are byte-identical.
        for f in ["1259", "1247", "1255"] {
            let a = sim
                .file_contents(Path::new(&format!("base/1/{f}")))
                .unwrap();
            let b = sim
                .file_contents(Path::new(&format!("base/5/{f}")))
                .unwrap();
            assert_eq!(a, b, "{f}");
        }
        let c = ControlFileHandle::open(&vfs).unwrap().get();
        assert_eq!(c.next_xid, 3);
        assert_eq!(c.next_oid, 16384);
        assert_eq!(c.builtin_hash, rows::builtin_hash());
    }

    #[test]
    fn everything_survives_a_crash() {
        let sim = SimVfs::new(2);
        initdb(Arc::new(sim.clone()), &opts()).unwrap();
        let after: Arc<dyn Vfs> = Arc::new(sim.crash(CrashMode::DropUnsynced));
        ControlFileHandle::open(&after)
            .unwrap()
            .check_compatible(rows::builtin_hash())
            .unwrap();
        for p in ["base/1/1259", "base/4/1259", "base/5/1259", "global/1262"] {
            assert!(after.exists(Path::new(p)).unwrap(), "{p}");
            let f = after.open(Path::new(p), OpenMode::ReadOnly).unwrap();
            assert!(f.size().unwrap() > 0, "{p} is empty");
        }
    }

    #[test]
    fn non_empty_directory_is_refused_and_left_alone() {
        let sim = SimVfs::new(3);
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        vfs.create_dir(Path::new("precious")).unwrap();
        let e = initdb(Arc::clone(&vfs), &opts()).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        assert!(vfs.exists(Path::new("precious")).unwrap());
    }

    #[test]
    fn failure_leaves_no_directory_content() {
        let sim = SimVfs::new(4);
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        // Fail the copy to template0/postgres: the first write into base/4.
        sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("base/4".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = initdb(Arc::clone(&vfs), &opts()).unwrap_err();
        assert!(!e.message.is_empty());
        assert!(vfs.read_dir(Path::new("")).unwrap().is_empty());
        // A retry on the same (now clean) directory works.
        sim.set_faults(FaultPlan::default());
        initdb(vfs, &opts()).unwrap();
    }

    #[test]
    fn no_sync_still_produces_a_readable_cluster() {
        let sim = SimVfs::new(5);
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let mut o = opts();
        o.no_sync = true;
        o.superuser = "alice".into();
        initdb(Arc::clone(&vfs), &o).unwrap();
        assert!(vfs.exists(Path::new("base/5/1259")).unwrap());
    }

    #[test]
    fn wal_holds_the_bootstrap_and_ends_with_a_shutdown_checkpoint() {
        use crate::wal::xlog::{CheckpointRecord, CheckpointRecordKind};
        use crate::wal::{Lsn, RmgrId, WalConfig, WalReader};
        let sim = SimVfs::new(6);
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let mut o = opts();
        o.wal_segment_size = 4 << 20;
        initdb(Arc::clone(&vfs), &o).unwrap();
        let c = ControlFileHandle::open(&vfs).unwrap().get();
        assert_eq!(c.wal_segment_size, 4 << 20);
        assert_eq!(c.state, crate::control::DbState::ShutDown);
        let cfg = WalConfig {
            segment_size: c.wal_segment_size,
            system_identifier: c.system_identifier,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        };
        // From the first segment, every record up to the checkpoint is valid.
        let first = Lsn(u64::from(cfg.segment_size) + crate::wal::SEG_HEADER_SIZE);
        let mut r = WalReader::open(Arc::clone(&vfs), &cfg, first);
        let mut heap_inserts = 0;
        let mut creates = 0;
        let mut last = None;
        while let Some(rec) = r.next().unwrap() {
            match rec.rmgr {
                RmgrId::Heap => heap_inserts += 1,
                RmgrId::Smgr => creates += 1,
                _ => {}
            }
            last = Some(rec);
        }
        assert!(heap_inserts > 0 && creates > 0);
        let last = last.unwrap();
        assert_eq!(last.start.0, c.checkpoint_lsn);
        let ck = CheckpointRecord::decode(&last).unwrap();
        assert_eq!(ck.kind, CheckpointRecordKind::Shutdown);
        assert_eq!(ck.redo.0, c.redo_lsn);
        assert!(ck.redo <= last.start);
        assert_eq!((ck.next_xid.0, ck.next_oid), (c.next_xid, c.next_oid));
    }

    #[test]
    fn invalid_wal_segment_size_is_refused() {
        for bad in [0, 3 << 20, 1 << 20, 1 << 31] {
            let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(8));
            let mut o = opts();
            o.wal_segment_size = bad;
            let e = initdb(Arc::clone(&vfs), &o).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE, "{bad}");
            assert!(vfs.read_dir(Path::new("")).map_or(true, |v| v.is_empty()));
        }
    }
}
