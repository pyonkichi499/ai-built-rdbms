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
use crate::control::{ControlData, ControlFileHandle, generate_system_identifier};
use crate::datadir::{copy_database_dir, write_version_file};
use crate::error::{Error, Result, sqlstate};
use crate::storage::stack::StorageStack;
use crate::storage::vfs::Vfs;
use crate::storage::{TableStore, WriteCtx};
use crate::txn::Xid;
use crate::types::Oid;

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

    // 3. template1: the catalogs are written straight into the heap with
    // xmin = BOOTSTRAP (visible to every snapshot).
    let stack = StorageStack::new(
        Arc::clone(vfs),
        opts.rel_seg_blocks,
        BOOTSTRAP_FRAMES,
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

    // 4. Write every page, then make the files durable.
    stack.pool.flush_all_for_checkpoint()?;
    if sync {
        stack.smgr.sync_pending()?;
    }
    if stack.pool.pinned_frames() != 0 {
        return Err(Error::internal("buffers are still pinned after bootstrap"));
    }
    drop(stack);
    if sync {
        for dir in ["global", "base/1", "base", ""] {
            vfs.sync_dir(Path::new(dir))
                .map_err(|e| Error::from_io(&e, format!("could not fsync directory \"{dir}\"")))?;
        }
    }

    // 5. base/1/YUZHU_VERSION
    write_version_file(&**vfs, Path::new("base/1"), sync)?;

    // 6. template0 and postgres are copies of template1.
    copy_database_dir(&**vfs, TEMPLATE1_OID, TEMPLATE0_OID, sync)?;
    copy_database_dir(&**vfs, TEMPLATE1_OID, POSTGRES_OID, sync)?;

    // 7. pg_xact/ stays empty. 8. The control file marks the end.
    let data = ControlData::initial(
        generate_system_identifier(),
        rows::builtin_hash(),
        opts.rel_seg_blocks,
    );
    ControlFileHandle::create(vfs, &data)?;
    Ok(())
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
    use crate::storage::vfs::{
        CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, OpenMode, SimVfs,
    };

    fn opts() -> InitdbOptions {
        InitdbOptions {
            superuser: "postgres".into(),
            no_sync: false,
            rel_seg_blocks: 131_072,
        }
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
}
