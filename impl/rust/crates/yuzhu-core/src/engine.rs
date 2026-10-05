//! `Cluster` (the whole server instance) and `DatabaseHandle`
//! (`m2.md` §4.8, `m3.md` §4.8, §5.5). Replaces M1's `Database`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::catalog::cache::CatalogCache;
use crate::catalog::rows;
use crate::catalog::store::{CatalogStore, RoleRow, SharedCatalogStore};
use crate::checkpoint::{self, CheckpointKind, CheckpointParts, CheckpointerHandle};
use crate::control::{ControlFileHandle, DbState};
use crate::datadir::{self, OidAllocator, PidFile};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::recovery::{self, log};
use crate::storage::TableStore;
use crate::storage::stack::{StackConfig, StorageStack};
use crate::storage::vfs::Vfs;
use crate::txn::{FIRST_COMMAND_ID, TxnManager};
use crate::types::Oid;
use crate::util::sync::lock;
use crate::wal::{Wal, segment};

/// Default `max_wal_size` (1 GiB).
pub const DEFAULT_MAX_WAL_SIZE: u64 = 1 << 30;

/// How often the checkpointer thread looks at the clock and the WAL volume.
const CHECKPOINTER_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct ClusterOptions {
    pub data_dir: PathBuf,
    /// Number of buffer frames. Default 16384 (128MB).
    pub shared_buffers: usize,
    pub max_connections: u32,
    /// Default 300 seconds.
    pub checkpoint_timeout: Duration,
    /// A checkpoint starts when this much WAL has accumulated since the
    /// REDO point. Default [`DEFAULT_MAX_WAL_SIZE`].
    pub max_wal_size: u64,
    /// Default true. The crash tests turn it off and call
    /// [`Cluster::checkpoint`] by hand.
    pub background_checkpointer: bool,
    /// Mutation-testing switches; all off by default.
    pub knobs: DebugKnobs,
}

/// The whole cluster: storage, transactions, catalogs of every database.
#[derive(Debug)]
pub struct Cluster {
    stack: StorageStack,
    storage: Arc<dyn TableStore>,
    pid_file: Mutex<Option<PidFile>>,
    control: Arc<ControlFileHandle>,
    txn: Arc<TxnManager>,
    oids: OidAllocator,
    shared: Arc<SharedCatalogStore>,
    checkpoint_lock: Mutex<()>,
    checkpointer: Mutex<Option<CheckpointerHandle>>,
    /// When the last checkpoint (or the start) happened, for `checkpoint_timeout`.
    last_checkpoint: Mutex<Instant>,
    checkpoint_timeout: Duration,
    max_wal_size: u64,
    databases: Mutex<HashMap<Oid, Arc<DatabaseHandle>>>,
    poisoned: AtomicBool,
    next_session_id: AtomicU64,
    /// Read-only values for `SHOW` (`shared_buffers`, `data_directory`, ...).
    server_settings: Vec<(&'static str, String)>,
}

fn fatal(state: crate::SqlState, message: String) -> Error {
    Error::new(state, message).with_severity(Severity::Fatal)
}

impl Cluster {
    /// Starts the cluster, including crash recovery (`m3.md` §5.5). The
    /// checkpointer thread gets a `Weak<Cluster>` and a stop channel (no
    /// reference cycle).
    #[allow(clippy::needless_pass_by_value)]
    pub fn open(vfs: Arc<dyn Vfs>, opts: ClusterOptions) -> Result<Arc<Cluster>> {
        // 1. The pid file lock (a second server on the same directory fails).
        let display = opts.data_dir.display().to_string();
        let pid = PidFile::acquire(&*vfs, &display, 0)?;
        Cluster::open_locked(&vfs, &opts, pid)
    }

    fn open_locked(
        vfs: &Arc<dyn Vfs>,
        opts: &ClusterOptions,
        pid: PidFile,
    ) -> Result<Arc<Cluster>> {
        let parts = match Cluster::prepare(vfs, opts) {
            Ok(parts) => parts,
            Err(e) => {
                // Do not leave a stale file behind a refused start.
                let _ = pid.release(&**vfs);
                return Err(e);
            }
        };
        let did_redo = parts.did_redo;
        let cluster = Arc::new(Cluster {
            stack: parts.stack,
            storage: parts.storage,
            pid_file: Mutex::new(Some(pid)),
            control: parts.control,
            txn: parts.txn,
            oids: parts.oids,
            shared: parts.shared,
            checkpoint_lock: Mutex::new(()),
            checkpointer: Mutex::new(None),
            last_checkpoint: Mutex::new(Instant::now()),
            checkpoint_timeout: opts.checkpoint_timeout,
            max_wal_size: opts.max_wal_size,
            databases: Mutex::new(HashMap::new()),
            poisoned: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
            server_settings: server_settings(opts, parts.rel_seg_blocks, parts.wal_segment_size),
        });
        if let Err(e) = cluster.finish_start(did_redo) {
            // The data directory is left as it is (for inspection); only the
            // lock goes.
            let pid = lock(&cluster.pid_file).ok().and_then(|mut g| g.take());
            if let Some(pid) = pid {
                let _ = pid.release(&*cluster.stack.vfs);
            }
            return Err(e);
        }
        // 8. The checkpointer.
        if opts.background_checkpointer {
            let weak = Arc::downgrade(&cluster);
            let poll = CHECKPOINTER_POLL
                .min(opts.checkpoint_timeout)
                .max(Duration::from_millis(1));
            let handle =
                checkpoint::spawn_checkpointer(poll, Box::new(move || Cluster::tick(&weak)));
            match cluster.checkpointer.lock() {
                Ok(mut g) => *g = Some(handle),
                Err(p) => *p.into_inner() = Some(handle),
            }
        }
        log("database system is ready to accept connections");
        Ok(cluster)
    }

    /// Steps 6 to 8 of §5.5 (without the checkpointer thread).
    fn finish_start(&self, did_redo: bool) -> Result<()> {
        if did_redo {
            // 6. The end-of-recovery checkpoint writes what REDO dirtied and
            // moves `state` to `InProduction`.
            log("checkpoint starting: end-of-recovery immediate wait");
            let r = self.run_checkpoint(CheckpointKind::EndOfRecovery)?;
            log(&format!(
                "checkpoint complete: wrote {} buffers; redo at {}",
                r.buffers_written, r.redo
            ));
            self.check_shared_catalog()
        } else {
            // 7. A clean stop: in production from now on.
            self.check_shared_catalog()?;
            self.control.update(|c| c.state = DbState::InProduction)
        }
    }

    /// The shared catalogs must be readable.
    fn check_shared_catalog(&self) -> Result<()> {
        let snap = self.txn.snapshot(None, FIRST_COMMAND_ID);
        if self.shared.database_by_name(&snap, "postgres")?.is_none() {
            return Err(Error::corrupted("pg_database has no \"postgres\" row")
                .with_severity(Severity::Fatal));
        }
        Ok(())
    }

    /// Steps 2 to 5 of §5.5.
    fn prepare(vfs: &Arc<dyn Vfs>, opts: &ClusterOptions) -> Result<Prepared> {
        // 2. YUZHU_VERSION, the control file and its constants.
        datadir::check_version_file(&**vfs, Path::new(""))?;
        let control = Arc::new(ControlFileHandle::open(vfs)?);
        control.check_compatible(rows::builtin_hash())?;
        control.set_single_slot(opts.knobs.single_slot_control_file);
        let data = control.get();
        // 3. A WAL segment that was being created when the server stopped.
        segment::remove_temp_files(&**vfs)?;
        // 4. Crash recovery when the last stop was not clean.
        let outcome = recovery::startup(
            Arc::clone(vfs),
            &control,
            &StackConfig {
                rel_seg_blocks: data.rel_seg_blocks,
                nframes: opts.shared_buffers,
                knobs: opts.knobs,
            },
        )?;
        // 5. Transactions, OIDs and the shared catalog.
        let stack = outcome.stack;
        let txn = TxnManager::new(
            Arc::clone(&stack.clog),
            Arc::clone(&control),
            Arc::clone(&stack.wal),
            outcome.next_xid,
            opts.knobs,
        );
        let oids = OidAllocator::new(Arc::clone(&control));
        let storage: Arc<dyn TableStore> = stack.heap.clone();
        let shared = Arc::new(SharedCatalogStore::new(Arc::clone(&storage)));
        Ok(Prepared {
            stack,
            storage,
            control,
            txn,
            oids,
            shared,
            rel_seg_blocks: data.rel_seg_blocks,
            wal_segment_size: data.wal_segment_size,
            did_redo: outcome.did_redo,
        })
    }

    /// One wake-up of the checkpointer: a checkpoint if `checkpoint_timeout`
    /// has passed or `max_wal_size` has accumulated; false ends the thread.
    fn tick(weak: &Weak<Cluster>) -> bool {
        let Some(cluster) = weak.upgrade() else {
            return false;
        };
        if cluster.is_poisoned() {
            return true;
        }
        let last = *crate::util::sync::lock_ignore_poison(&cluster.last_checkpoint);
        let due = checkpoint::due(
            last,
            cluster.checkpoint_timeout,
            &cluster.stack.wal,
            cluster.max_wal_size,
        );
        if let Some(kind) = due {
            // A panic here would silently kill the thread and poison the
            // checkpoint lock; treat it like a panic in a connection thread.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cluster.run_checkpoint(kind)
            })) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn(&format!("checkpoint failed: {}", e.message)),
                Err(_) => {
                    warn("checkpointer panicked; shutting the cluster down");
                    cluster.poison();
                }
            }
        }
        true
    }

    fn run_checkpoint(&self, kind: CheckpointKind) -> Result<checkpoint::CheckpointResult> {
        let parts = CheckpointParts {
            pool: &self.stack.pool,
            smgr: &self.stack.smgr,
            clog: &self.stack.clog,
            wal: &self.stack.wal,
            txn: &self.txn,
            control: &self.control,
            oids: &self.oids,
            checkpoint_lock: &self.checkpoint_lock,
        };
        let result = checkpoint::run(&parts, kind).inspect_err(|e| {
            if e.severity == Severity::Panic {
                self.poison();
            }
        })?;
        *crate::util::sync::lock_ignore_poison(&self.last_checkpoint) = Instant::now();
        Ok(result)
    }

    /// `3D000` / `55000` / `28000`, all FATAL.
    pub fn connect(&self, database: &str, user: &str) -> Result<(Arc<DatabaseHandle>, RoleRow)> {
        // Own-less snapshot; no storage barrier (§6.9.1).
        let snap = self.txn.snapshot(None, FIRST_COMMAND_ID);
        let Some(db) = self.shared.database_by_name(&snap, database)? else {
            return Err(fatal(
                sqlstate::INVALID_CATALOG_NAME,
                format!("database \"{database}\" does not exist"),
            ));
        };
        if !db.allow_conn {
            return Err(fatal(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("database \"{database}\" is not currently accepting connections"),
            ));
        }
        let Some(role) = self.shared.role_by_name(&snap, user)? else {
            return Err(fatal(
                sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{user}\" does not exist"),
            ));
        };
        if !role.can_login {
            return Err(fatal(
                sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{user}\" is not permitted to log in"),
            ));
        }
        let handle = {
            let mut dbs = lock(&self.databases)?;
            Arc::clone(dbs.entry(db.oid).or_insert_with(|| {
                Arc::new(DatabaseHandle {
                    oid: db.oid,
                    name: db.name.clone(),
                    catalog: CatalogStore::new(db.oid, Arc::clone(&self.storage)),
                    shared: Arc::clone(&self.shared),
                    cache: CatalogCache::default(),
                })
            }))
        };
        Ok((handle, role))
    }

    /// Calls `checkpoint::run(.., CheckpointKind::Explicit)` (§5.6).
    pub fn checkpoint(&self) -> Result<()> {
        self.run_checkpoint(CheckpointKind::Explicit).map(|_| ())
    }

    /// Called after new connections are refused and all sessions have
    /// ended. Stops and joins the checkpointer first (§5.7). If anything
    /// fails, `state` is not set to `ShutDown`.
    pub fn shutdown(&self) -> Result<()> {
        if let Some(h) = lock(&self.checkpointer)?.take() {
            h.stop_and_join();
        }
        let Some(pid) = lock(&self.pid_file)?.take() else {
            return Ok(()); // already shut down
        };
        let result = self
            .control
            .update(|c| c.state = DbState::ShuttingDown)
            .and_then(|()| self.run_checkpoint(CheckpointKind::Shutdown).map(|_| ()));
        match result {
            Ok(()) => pid.release(&*self.stack.vfs),
            Err(e) => {
                // Keep the lock until the cluster is dropped: the process
                // is going down with an unclean state.
                if let Ok(mut g) = self.pid_file.lock() {
                    *g = Some(pid);
                }
                Err(e)
            }
        }
    }

    /// Tests only: stops the checkpointer without writing anything and drops
    /// the cluster (pairs with `SimVfs::crash`).
    pub fn abandon(self: Arc<Self>) {
        let handle = lock(&self.checkpointer).ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            h.stop_and_join();
        }
        // Release the pid lock even if other references to the cluster
        // are still alive.
        let pid = lock(&self.pid_file).ok().and_then(|mut g| g.take());
        drop(pid);
    }

    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
        self.stack.pool.poison_flag().set();
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    pub fn txn_manager(&self) -> &Arc<TxnManager> {
        &self.txn
    }

    pub fn oid_allocator(&self) -> &OidAllocator {
        &self.oids
    }

    pub fn storage(&self) -> &Arc<dyn TableStore> {
        &self.storage
    }

    pub fn control(&self) -> &Arc<ControlFileHandle> {
        &self.control
    }

    pub fn stack(&self) -> &StorageStack {
        &self.stack
    }

    pub fn wal(&self) -> &Arc<Wal> {
        &self.stack.wal
    }

    /// Values of the read-only parameters (`shared_buffers`, ...), applied
    /// to each new session's `Settings`.
    pub fn server_settings(&self) -> &[(&'static str, String)] {
        &self.server_settings
    }

    /// Allocates a session ID (passed to `begin_write`); called once by
    /// `Session::new`.
    pub fn next_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    /// `invalidate_all` on every database's `CatalogCache` (§5.3).
    pub fn invalidate_all_catalog_caches(&self) {
        let dbs = self
            .databases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for db in dbs.values() {
            db.cache.invalidate_all();
        }
    }
}

struct Prepared {
    stack: StorageStack,
    storage: Arc<dyn TableStore>,
    control: Arc<ControlFileHandle>,
    txn: Arc<TxnManager>,
    oids: OidAllocator,
    shared: Arc<SharedCatalogStore>,
    rel_seg_blocks: u32,
    wal_segment_size: u32,
    did_redo: bool,
}

/// `8192 * blocks` bytes in the unit PostgreSQL's `SHOW` uses (`128MB`).
fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10)];
    for (name, unit) in UNITS {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{}{name}", bytes / unit);
        }
    }
    format!("{bytes}B")
}

fn format_timeout(secs: u64) -> String {
    for (unit, name) in [(86_400, "d"), (3_600, "h"), (60, "min")] {
        if secs >= unit && secs.is_multiple_of(unit) {
            return format!("{}{name}", secs / unit);
        }
    }
    format!("{secs}s")
}

/// The values behind the read-only parameters (`m2.md` §6.11).
fn server_settings(
    opts: &ClusterOptions,
    rel_seg_blocks: u32,
    wal_segment_size: u32,
) -> Vec<(&'static str, String)> {
    let page = u64::try_from(crate::storage::BLCKSZ).unwrap_or(8192);
    vec![
        (
            "shared_buffers",
            format_size(u64::try_from(opts.shared_buffers).unwrap_or(0) * page),
        ),
        ("data_directory", opts.data_dir.display().to_string()),
        (
            "checkpoint_timeout",
            format_timeout(opts.checkpoint_timeout.as_secs()),
        ),
        (
            "segment_size",
            format_size(u64::from(rel_seg_blocks) * page),
        ),
        ("wal_segment_size", format_size(u64::from(wal_segment_size))),
        ("max_wal_size", format_size(opts.max_wal_size)),
    ]
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

/// Per-database state.
#[derive(Debug)]
pub struct DatabaseHandle {
    pub oid: Oid,
    pub name: String,
    pub catalog: CatalogStore,
    pub shared: Arc<SharedCatalogStore>,
    pub cache: CatalogCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{InitdbOptions, initdb};
    use crate::catalog::schema::oids;
    use crate::catalog::store::NewTable;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::control::ControlData;
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};
    use crate::storage::{RelHandle, WriteCtx};
    use crate::types::{Datum, SqlType};

    fn opts() -> ClusterOptions {
        ClusterOptions {
            data_dir: PathBuf::from("/sim/data"),
            shared_buffers: 256,
            max_connections: 16,
            checkpoint_timeout: Duration::from_secs(3600),
            max_wal_size: DEFAULT_MAX_WAL_SIZE,
            background_checkpointer: false,
            knobs: DebugKnobs::default(),
        }
    }

    fn fresh_with(seed: u64, wal_segment_size: u32) -> SimVfs {
        let sim = SimVfs::new(seed);
        let mut o = InitdbOptions::new("postgres");
        o.wal_segment_size = wal_segment_size;
        initdb(Arc::new(sim.clone()), &o).unwrap();
        sim
    }

    fn fresh() -> SimVfs {
        fresh_with(1, 2 << 20)
    }

    fn open(sim: &SimVfs, o: ClusterOptions) -> Result<Arc<Cluster>> {
        Cluster::open(Arc::new(sim.clone()), o)
    }

    fn control(sim: &SimVfs) -> ControlData {
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        ControlFileHandle::open(&v).unwrap().get()
    }

    fn state(sim: &SimVfs) -> DbState {
        control(sim).state
    }

    /// Creates table `t(a int4)` with one row `42` in `postgres`; committed or aborted.
    fn create_t(c: &Arc<Cluster>, commit: bool) -> u32 {
        let (db, _) = c.connect("postgres", "postgres").unwrap();
        let t = c.txn_manager();
        let (xid, guard) = begin(t);
        let w = WriteCtx { xid, cid: 0 };
        let snap = t.snapshot(Some(xid), 0);
        let oid = db.catalog.get_new_relation_oid(c.oid_allocator()).unwrap();
        let columns = vec![ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
        }];
        db.catalog
            .create_table(
                &w,
                &snap,
                &NewTable {
                    oid,
                    namespace: oids::NAMESPACE_PUBLIC,
                    name: "t".into(),
                    owner: oids::BOOTSTRAP_SUPERUSER,
                    columns,
                    checks: vec![],
                    attrdef_oids: vec![],
                    constraint_oids: vec![],
                },
            )
            .unwrap();
        let snap = t.snapshot(Some(xid), 1);
        let def: TableDef = db.catalog.load_table_def(&snap, oid).unwrap().unwrap();
        c.storage()
            .create_storage(&WriteCtx { xid, cid: 1 }, def.locator)
            .unwrap();
        let rel = RelHandle::from_table(&def);
        c.storage()
            .insert(&rel, &WriteCtx { xid, cid: 1 }, &[Datum::Int4(42)])
            .unwrap();
        if commit {
            t.commit(xid, &[]).unwrap();
        } else {
            t.abort(xid, &[def.locator]).unwrap();
        }
        drop(guard);
        oid
    }

    fn read_t(c: &Arc<Cluster>, oid: u32) -> Option<Vec<Datum>> {
        let (db, _) = c.connect("postgres", "postgres").unwrap();
        let snap = c.txn_manager().snapshot(None, 0);
        let def = db.catalog.load_table_def(&snap, oid).unwrap()?;
        let rel = RelHandle::from_table(&def);
        let mut scan = c.storage().begin_scan(&rel, &snap).unwrap();
        let mut out = Vec::new();
        while let Some(t) = c.storage().scan_next(&mut scan).unwrap() {
            out.push(t.row[0].clone());
        }
        drop(scan);
        assert_eq!(c.stack().pool.pinned_frames(), 0);
        Some(out)
    }

    fn begin(t: &Arc<TxnManager>) -> (crate::txn::Xid, crate::txn::WriterGuard) {
        let flag = crate::interrupt::InterruptFlag::default();
        t.begin_write(
            1,
            &crate::txn::WaitCtl {
                lock_timeout: None,
                interrupts: &flag,
            },
        )
        .unwrap()
    }

    fn crash(sim: &SimVfs, c: Arc<Cluster>, mode: CrashMode) -> SimVfs {
        c.abandon();
        sim.crash(mode)
    }

    #[test]
    fn open_marks_in_production_and_shutdown_marks_shut_down() {
        let sim = fresh();
        assert_eq!(state(&sim), DbState::ShutDown);
        let c = open(&sim, opts()).unwrap();
        assert_eq!(state(&sim), DbState::InProduction);
        assert!(sim.file_contents(Path::new("yuzhu.pid")).is_some());
        c.shutdown().unwrap();
        assert_eq!(state(&sim), DbState::ShutDown);
        assert!(sim.file_contents(Path::new("yuzhu.pid")).is_none());
        // A second shutdown is harmless.
        c.shutdown().unwrap();
    }

    #[test]
    fn second_server_is_refused() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let e = open(&sim, opts()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.hint.unwrap().contains("another yuzhu-server"));
        c.shutdown().unwrap();
        open(&sim, opts()).unwrap().shutdown().unwrap();
    }

    #[test]
    fn uninitialized_directory_is_refused() {
        let sim = SimVfs::new(1);
        let e = open(&sim, opts()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        // The refused start leaves no lock behind.
        assert!(sim.file_contents(Path::new("yuzhu.pid")).is_none());
    }

    #[test]
    fn unclean_stop_is_recovered_not_refused() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let before = control(&sim);
        let disk = crash(&sim, c, CrashMode::DropUnsynced);
        assert_eq!(state(&disk), DbState::InProduction);
        let c = open(&disk, opts()).unwrap();
        c.connect("postgres", "postgres").unwrap();
        // The end-of-recovery checkpoint is the new starting point.
        let after = control(&disk);
        assert_eq!(after.state, DbState::InProduction);
        assert!(after.checkpoint_lsn > before.checkpoint_lsn);
        assert_eq!(after.wal_segment_size, 2 << 20);
        c.shutdown().unwrap();
        assert_eq!(state(&disk), DbState::ShutDown);
    }

    #[test]
    fn committed_work_survives_a_crash_and_uncommitted_work_does_not() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let kept = create_t(&c, true);
        let lost = create_t(&c, false);
        let (xid, guard) = begin(c.txn_manager());
        // An open transaction at the crash: it must end up aborted.
        let disk = crash(&sim, c, CrashMode::DropUnsynced);
        drop(guard);

        let c = open(&disk, opts()).unwrap();
        assert_eq!(read_t(&c, kept), Some(vec![Datum::Int4(42)]));
        assert_eq!(read_t(&c, lost), None);
        assert_ne!(
            c.stack().clog.status(xid).unwrap(),
            crate::txn::clog::XidStatus::Committed
        );
        let (x2, g2) = begin(c.txn_manager());
        assert!(x2 > xid, "XIDs are not reused after recovery");
        c.txn_manager().abort(x2, &[]).unwrap();
        drop(g2);
        c.shutdown().unwrap();
    }

    #[test]
    fn recovery_after_crash_is_repeatable_and_ends_clean() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let oid = create_t(&c, true);
        let disk = crash(&sim, c, CrashMode::KeepAll);
        // A crash again while recovering (nothing written by the second
        // start beyond what it syncs): the same REDO is done again.
        let c = open(&disk, opts()).unwrap();
        let disk2 = crash(&disk, c, CrashMode::DropUnsynced);
        let c = open(&disk2, opts()).unwrap();
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        c.shutdown().unwrap();
        // The next start is a clean one.
        let c = open(&disk2, opts()).unwrap();
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        c.shutdown().unwrap();
    }

    #[test]
    fn crash_after_explicit_checkpoint_keeps_data() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let oid = create_t(&c, true);
        c.checkpoint().unwrap();
        let oid2 = create_t(&c, true);
        let disk = crash(&sim, c, CrashMode::DropUnsynced);
        let c = open(&disk, opts()).unwrap();
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        assert_eq!(read_t(&c, oid2), Some(vec![Datum::Int4(42)]));
        c.shutdown().unwrap();
    }

    #[test]
    fn clean_restart_keeps_data_and_counters() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let oid = create_t(&c, true);
        let (xid, g) = begin(c.txn_manager());
        c.txn_manager().commit(xid, &[]).unwrap();
        drop(g);
        let next_oid = c.oid_allocator().next_raw().unwrap();
        c.shutdown().unwrap();
        c.abandon();
        let c = open(&sim, opts()).unwrap();
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        let (x2, g) = begin(c.txn_manager());
        assert!(x2 > xid);
        c.txn_manager().abort(x2, &[]).unwrap();
        drop(g);
        assert!(c.oid_allocator().next_raw().unwrap() > next_oid);
        c.shutdown().unwrap();
    }

    #[test]
    fn stale_wal_temp_file_is_removed_at_start() {
        let sim = fresh();
        let tmp = Path::new("pg_wal/xlogtemp.123");
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        v.open(tmp, crate::storage::vfs::OpenMode::CreateNew)
            .unwrap();
        assert!(sim.file_contents(tmp).is_some());
        let c = open(&sim, opts()).unwrap();
        assert!(sim.file_contents(tmp).is_none());
        c.shutdown().unwrap();
    }

    #[test]
    fn explicit_checkpoint_keeps_state() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        c.checkpoint().unwrap();
        assert_eq!(c.control().get().state, DbState::InProduction);
        assert_eq!(c.stack().pool.pinned_frames(), 0);
        assert!(Arc::ptr_eq(c.wal(), &c.stack().wal));
        c.shutdown().unwrap();
    }

    #[test]
    fn failed_shutdown_sync_leaves_state_unclean_and_next_start_recovers() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let oid = create_t(&c, true);
        // Fail the control file write of the shutdown checkpoint.
        sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("global/yuzhu_control".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        assert!(c.shutdown().is_err());
        sim.set_faults(FaultPlan::default());
        assert_ne!(state(&sim), DbState::ShutDown);
        let disk = crash(&sim, c, CrashMode::KeepAll);
        let c = open(&disk, opts()).unwrap();
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        c.shutdown().unwrap();
    }

    #[test]
    fn broken_checkpoint_pointer_is_fatal_and_changes_nothing() {
        let sim = fresh();
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        let h = ControlFileHandle::open(&v).unwrap();
        let good = h.get();
        h.update(|c| c.checkpoint_lsn += 8).unwrap();
        let e = open(&sim, opts()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert!(
            e.message
                .contains("could not locate a valid checkpoint record at")
        );
        assert!(sim.file_contents(Path::new("yuzhu.pid")).is_none());
        assert_eq!(state(&sim), DbState::ShutDown);
        h.update(|c| c.checkpoint_lsn = good.checkpoint_lsn)
            .unwrap();
        open(&sim, opts()).unwrap().shutdown().unwrap();
    }

    #[test]
    fn periodic_checkpointer_runs_and_stops() {
        let sim = fresh();
        let mut o = opts();
        o.checkpoint_timeout = Duration::from_millis(20);
        o.background_checkpointer = true;
        let c = open(&sim, o).unwrap();
        let oid = create_t(&c, true);
        let g0 = c.control().generation();
        let deadline = Instant::now() + Duration::from_secs(10);
        while c.control().generation() == g0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(c.control().generation() > g0, "no periodic checkpoint ran");
        assert_eq!(read_t(&c, oid), Some(vec![Datum::Int4(42)]));
        c.shutdown().unwrap();
        assert_eq!(state(&sim), DbState::ShutDown);
    }

    #[test]
    fn wal_volume_triggers_a_checkpoint() {
        let sim = fresh();
        let mut o = opts();
        o.max_wal_size = 1;
        o.background_checkpointer = true;
        o.checkpoint_timeout = Duration::from_secs(3600);
        let c = open(&sim, o).unwrap();
        let before = c.control().get().checkpoint_lsn;
        create_t(&c, true);
        // The poll interval is one second.
        let deadline = Instant::now() + Duration::from_secs(15);
        while c.control().get().checkpoint_lsn == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(c.control().get().checkpoint_lsn > before);
        c.shutdown().unwrap();
    }

    #[test]
    fn no_background_checkpointer_means_no_checkpoints() {
        let sim = fresh();
        let mut o = opts();
        o.checkpoint_timeout = Duration::from_millis(5);
        let c = open(&sim, o).unwrap();
        let g0 = c.control().generation();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(c.control().generation(), g0);
        c.shutdown().unwrap();
    }

    #[test]
    fn dropping_the_last_reference_ends_the_checkpointer() {
        let sim = fresh();
        let mut o = opts();
        o.checkpoint_timeout = Duration::from_millis(10);
        o.background_checkpointer = true;
        let c = open(&sim, o).unwrap();
        let weak = Arc::downgrade(&c);
        drop(c);
        let deadline = Instant::now() + Duration::from_secs(10);
        while weak.strong_count() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(weak.strong_count(), 0);
    }

    #[test]
    fn poison_is_visible() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        assert!(!c.is_poisoned());
        c.poison();
        assert!(c.is_poisoned());
        assert!(c.stack().pool.is_poisoned());
    }

    #[test]
    fn session_ids_increase_and_caches_invalidate() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let a = c.next_session_id();
        assert!(c.next_session_id() > a);
        let (h, _) = c.connect("postgres", "postgres").unwrap();
        let g = h.cache.generation();
        c.invalidate_all_catalog_caches();
        assert!(h.cache.generation() > g);
        // The same handle is returned for the same database.
        let (h2, _) = c.connect("postgres", "postgres").unwrap();
        assert!(Arc::ptr_eq(&h, &h2));
    }

    #[test]
    fn server_settings_include_wal_values() {
        let sim = fresh_with(7, 4 << 20);
        let c = open(&sim, opts()).unwrap();
        let get = |n: &str| {
            c.server_settings()
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("wal_segment_size").as_deref(), Some("4MB"));
        assert_eq!(get("max_wal_size").as_deref(), Some("1GB"));
        c.shutdown().unwrap();
    }

    #[test]
    fn connect_errors() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let e = c.connect("nope", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_CATALOG_NAME);
        let e = c.connect("template0", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        let e = c.connect("postgres", "nobody").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        c.shutdown().unwrap();
    }
}
