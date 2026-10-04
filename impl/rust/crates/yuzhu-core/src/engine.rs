//! `Cluster` (the whole server instance) and `DatabaseHandle`
//! (`m2.md` §4.8). Replaces M1's `Database`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::catalog::cache::CatalogCache;
use crate::catalog::rows;
use crate::catalog::store::{CatalogStore, RoleRow, SharedCatalogStore};
use crate::checkpoint::{self, CheckpointKind, CheckpointParts, CheckpointerHandle};
use crate::control::{ControlFileHandle, DbState};
use crate::datadir::{self, OidAllocator, PidFile};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::TableStore;
use crate::storage::stack::StorageStack;
use crate::storage::vfs::Vfs;
use crate::txn::{FIRST_COMMAND_ID, TxnManager, Xid};
use crate::types::Oid;
use crate::util::sync::lock;

#[derive(Debug, Clone)]
pub struct ClusterOptions {
    pub data_dir: PathBuf,
    /// Number of buffer frames. Default 16384 (128MB).
    pub shared_buffers: usize,
    pub max_connections: u32,
    /// Default 300 seconds.
    pub checkpoint_timeout: Duration,
    pub ignore_unclean_shutdown: bool,
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
    /// Starts the cluster (`m2.md` §5.1). The checkpointer thread gets a
    /// `Weak<Cluster>` and a stop channel (no reference cycle).
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
        match Cluster::prepare(vfs, opts) {
            Ok(parts) => {
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
                    databases: Mutex::new(HashMap::new()),
                    poisoned: AtomicBool::new(false),
                    next_session_id: AtomicU64::new(1),
                    server_settings: server_settings(opts, parts.rel_seg_blocks),
                });
                // 8. The checkpointer.
                let weak = Arc::downgrade(&cluster);
                let handle = checkpoint::spawn_checkpointer(
                    opts.checkpoint_timeout,
                    Box::new(move || Cluster::tick(&weak)),
                );
                match cluster.checkpointer.lock() {
                    Ok(mut g) => *g = Some(handle),
                    Err(p) => *p.into_inner() = Some(handle),
                }
                Ok(cluster)
            }
            Err(e) => {
                // Do not leave a stale file behind a refused start.
                let _ = pid.release(&**vfs);
                Err(e)
            }
        }
    }

    /// Steps 2 to 7 of §5.1.
    fn prepare(vfs: &Arc<dyn Vfs>, opts: &ClusterOptions) -> Result<Prepared> {
        // 2. YUZHU_VERSION
        datadir::check_version_file(&**vfs, Path::new(""))?;
        // 3. The control file and its constants.
        let control = Arc::new(ControlFileHandle::open(vfs)?);
        control.check_compatible(rows::builtin_hash())?;
        let data = control.get();
        // 4. Was the last shutdown clean?
        if data.state != DbState::ShutDown {
            if !opts.ignore_unclean_shutdown {
                return Err(Error::new(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "database system was not properly shut down",
                )
                .with_severity(Severity::Fatal)
                .with_hint(
                    "yuzhu M2 has no crash recovery. Start with --ignore-unclean-shutdown \
                     to start anyway (data may be inconsistent).",
                ));
            }
            warn("database system was not properly shut down; continuing without recovery");
        }
        // 5. Storage, transactions and OIDs.
        let stack = StorageStack::new(
            Arc::clone(vfs),
            data.rel_seg_blocks,
            opts.shared_buffers,
            Xid(data.next_xid),
        )?;
        let txn = Arc::new(TxnManager::new(
            Arc::clone(&stack.clog),
            Arc::clone(&control),
        ));
        let oids = OidAllocator::new(Arc::clone(&control));
        let storage: Arc<dyn TableStore> = stack.heap.clone();
        let shared = Arc::new(SharedCatalogStore::new(Arc::clone(&storage)));
        // 6. The shared catalogs must be readable.
        let snap = txn.snapshot(None, FIRST_COMMAND_ID);
        if shared.database_by_name(&snap, "postgres")?.is_none() {
            return Err(Error::corrupted("pg_database has no \"postgres\" row")
                .with_severity(Severity::Fatal));
        }
        // 7. In production from now on.
        control.update(|c| c.state = DbState::InProduction)?;
        Ok(Prepared {
            stack,
            storage,
            control,
            txn,
            oids,
            shared,
            rel_seg_blocks: data.rel_seg_blocks,
        })
    }

    /// One periodic checkpoint; false ends the thread.
    fn tick(weak: &Weak<Cluster>) -> bool {
        let Some(cluster) = weak.upgrade() else {
            return false;
        };
        if cluster.is_poisoned() {
            return true;
        }
        if let Err(e) = cluster.run_checkpoint(CheckpointKind::Periodic) {
            warn(&format!("checkpoint failed: {}", e.message));
        }
        true
    }

    fn run_checkpoint(&self, kind: CheckpointKind) -> Result<()> {
        let parts = CheckpointParts {
            pool: &self.stack.pool,
            smgr: &self.stack.smgr,
            clog: &self.stack.clog,
            txn: &self.txn,
            control: &self.control,
            oids: &self.oids,
            checkpoint_lock: &self.checkpoint_lock,
        };
        checkpoint::run(&parts, kind).inspect_err(|e| {
            if e.severity == Severity::Panic {
                self.poison();
            }
        })
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
        self.run_checkpoint(CheckpointKind::Explicit)
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
            .and_then(|()| self.run_checkpoint(CheckpointKind::Shutdown));
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
fn server_settings(opts: &ClusterOptions, rel_seg_blocks: u32) -> Vec<(&'static str, String)> {
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
    use crate::control::DbState;
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};
    use crate::testing::{TestCluster, TestClusterOptions, cluster_options};

    fn opts(ignore: bool) -> ClusterOptions {
        cluster_options(&TestClusterOptions::default(), ignore)
    }

    fn fresh() -> SimVfs {
        let sim = SimVfs::new(1);
        initdb(
            Arc::new(sim.clone()),
            &InitdbOptions {
                superuser: "postgres".into(),
                no_sync: false,
                rel_seg_blocks: 131_072,
            },
        )
        .unwrap();
        sim
    }

    fn state(sim: &SimVfs) -> DbState {
        let v: Arc<dyn Vfs> = Arc::new(sim.clone());
        ControlFileHandle::open(&v).unwrap().get().state
    }

    #[test]
    fn open_marks_in_production_and_shutdown_marks_shut_down() {
        let sim = fresh();
        assert_eq!(state(&sim), DbState::ShutDown);
        let c = Cluster::open(Arc::new(sim.clone()), opts(false)).unwrap();
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
        let c = Cluster::open(Arc::new(sim.clone()), opts(false)).unwrap();
        let e = Cluster::open(Arc::new(sim.clone()), opts(false)).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.hint.unwrap().contains("another yuzhu-server"));
        c.shutdown().unwrap();
        Cluster::open(Arc::new(sim), opts(false))
            .unwrap()
            .shutdown()
            .unwrap();
    }

    #[test]
    fn uninitialized_directory_is_refused() {
        let sim = SimVfs::new(1);
        let e = Cluster::open(Arc::new(sim.clone()), opts(false)).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        // The refused start leaves no lock behind.
        assert!(sim.file_contents(Path::new("yuzhu.pid")).is_none());
    }

    #[test]
    fn unclean_shutdown_is_refused_unless_ignored() {
        let tc = TestCluster::new();
        let (disk, options) = tc.crash(CrashMode::DropUnsynced);
        let e = TestCluster::start_on(disk.clone(), options.clone(), false).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.message.contains("not properly shut down"));
        assert!(e.hint.unwrap().contains("ignore-unclean-shutdown"));
        // The refused start did not change the state.
        let tc = TestCluster::start_on(disk, options, true).unwrap();
        tc.cluster.connect("postgres", "postgres").unwrap();
    }

    #[test]
    fn explicit_checkpoint_keeps_state() {
        let tc = TestCluster::new();
        tc.cluster.checkpoint().unwrap();
        assert_eq!(tc.cluster.control().get().state, DbState::InProduction);
        assert_eq!(tc.cluster.stack().pool.pinned_frames(), 0);
    }

    #[test]
    fn failed_shutdown_sync_leaves_state_unclean() {
        let tc = TestCluster::new();
        // Fail the control file write of the shutdown checkpoint.
        tc.vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: Some("global/yuzhu_control".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        assert!(tc.cluster.shutdown().is_err());
        tc.vfs.set_faults(FaultPlan::default());
        assert_ne!(state(&tc.vfs), DbState::ShutDown);
        let (disk, options) = tc.crash(CrashMode::KeepAll);
        let e = TestCluster::start_on(disk, options, false).unwrap_err();
        assert!(e.message.contains("not properly shut down"));
    }

    #[test]
    fn periodic_checkpointer_runs_and_stops() {
        let sim = fresh();
        let mut o = opts(false);
        o.checkpoint_timeout = Duration::from_millis(20);
        let c = Cluster::open(Arc::new(sim.clone()), o).unwrap();
        let g0 = c.control().generation();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while c.control().generation() == g0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(c.control().generation() > g0, "no periodic checkpoint ran");
        c.shutdown().unwrap();
        assert_eq!(state(&sim), DbState::ShutDown);
    }

    #[test]
    fn dropping_the_last_reference_ends_the_checkpointer() {
        let sim = fresh();
        let mut o = opts(false);
        o.checkpoint_timeout = Duration::from_millis(10);
        let c = Cluster::open(Arc::new(sim.clone()), o).unwrap();
        let weak = Arc::downgrade(&c);
        drop(c);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while weak.strong_count() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(weak.strong_count(), 0);
    }

    #[test]
    fn poison_is_visible() {
        let tc = TestCluster::new();
        assert!(!tc.cluster.is_poisoned());
        tc.cluster.poison();
        assert!(tc.cluster.is_poisoned());
        assert!(tc.cluster.stack().pool.is_poisoned());
    }

    #[test]
    fn session_ids_increase_and_caches_invalidate() {
        let tc = TestCluster::new();
        let a = tc.cluster.next_session_id();
        assert!(tc.cluster.next_session_id() > a);
        let (h, _) = tc.cluster.connect("postgres", "postgres").unwrap();
        let g = h.cache.generation();
        tc.cluster.invalidate_all_catalog_caches();
        assert!(h.cache.generation() > g);
        // The same handle is returned for the same database.
        let (h2, _) = tc.cluster.connect("postgres", "postgres").unwrap();
        assert!(Arc::ptr_eq(&h, &h2));
    }

    #[test]
    fn restart_keeps_counters_and_never_reuses_xids() {
        let tc = TestCluster::new();
        let xid = {
            let t = tc.cluster.txn_manager();
            let (x, g) = t.begin_write(1, None).unwrap();
            t.commit(x).unwrap();
            drop(g);
            x
        };
        let oid = tc.cluster.oid_allocator().next_raw().unwrap();
        let tc = tc.restart().unwrap();
        let (x2, g) = tc.cluster.txn_manager().begin_write(1, None).unwrap();
        assert!(x2 > xid);
        tc.cluster.txn_manager().abort(x2).unwrap();
        drop(g);
        assert!(tc.cluster.oid_allocator().next_raw().unwrap() > oid);
    }
}
