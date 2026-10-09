//! `Cluster` (the whole server instance) and `DatabaseHandle`
//! (`m2.md` §4.8, `m3.md` §4.8, §5.5). Replaces M1's `Database`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::backend::{BackendGuard, BackendId, BackendInfo, BackendRegistry};
use crate::catalog::cache::CatalogCache;
use crate::catalog::rows;
use crate::catalog::store::{CatalogStore, DatabaseRow, RoleRow, SharedCatalogStore};
use crate::checkpoint::{self, CheckpointKind, CheckpointParts, CheckpointerHandle};
use crate::control::{ControlFileHandle, DbState};
use crate::datadir::{self, OidAllocator, PidFile};
use crate::dbase::{self, SessionLocks};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::interrupt::InterruptFlag;
use crate::recovery::{self, log};
use crate::storage::stack::{StackConfig, StorageStack};
use crate::storage::vfs::Vfs;
use crate::storage::{IndexStore, SequenceStore, TableStore};
use crate::txn::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtl};
use crate::txn::multixact::MultiXactTable;
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
    /// 簡易 autovacuum（03 §6.8）。既定は無効。
    pub autovacuum: crate::vacuum::stats::AutovacuumConfig,
    /// 末尾の切り詰めの `AccessExclusive` を待つ上限（03 VC-D18）。既定 5 秒。
    pub vacuum_truncate_lock_timeout: Duration,
    /// CREATE / DROP DATABASE が他のセッションの退出を待つ上限（09 DB-D5）。既定 5 秒。
    pub database_busy_wait: Duration,
    /// 認証の期限（08 C10）。既定 60 秒。`SHOW` するだけで、サーバが使う値は `Config` 側。
    pub authentication_timeout: Duration,
    /// 既定の `TimeZone`（06 TD-D16）。既定 `UTC`。
    pub timezone: String,
    /// tz データベースのディレクトリ（06 §11-2）。既定 `/usr/share/zoneinfo`。
    pub timezone_dir: PathBuf,
}

impl ClusterOptions {
    /// 既定値の一式（`data_dir` 以外）。構造体リテラルは `..ClusterOptions::new(dir)` で書く。
    /// `Clock`（06 §11-2 の `clock` 項目）は TD が `util/clock.rs` と一緒に足す。
    pub fn new(data_dir: PathBuf) -> ClusterOptions {
        ClusterOptions {
            data_dir,
            shared_buffers: 16384,
            max_connections: 100,
            checkpoint_timeout: Duration::from_secs(300),
            max_wal_size: DEFAULT_MAX_WAL_SIZE,
            background_checkpointer: true,
            knobs: DebugKnobs::default(),
            autovacuum: crate::vacuum::stats::AutovacuumConfig::default(),
            vacuum_truncate_lock_timeout: Duration::from_secs(5),
            database_busy_wait: Duration::from_secs(5),
            authentication_timeout: Duration::from_secs(60),
            timezone: "UTC".to_owned(),
            timezone_dir: PathBuf::from("/usr/share/zoneinfo"),
        }
    }
}

/// The whole cluster: storage, transactions, catalogs of every database.
#[derive(Debug)]
pub struct Cluster {
    stack: StorageStack,
    storage: Arc<dyn TableStore>,
    pid_file: Mutex<Option<PidFile>>,
    control: Arc<ControlFileHandle>,
    txn: Arc<TxnManager>,
    /// 重量ロック（リレーション・XID・タプルなど。`TxnManager` と `HeapStore` も同じものを持つ）。
    locks: Arc<LockManager>,
    /// 接続中のバックエンドの登録簿（`LockManager` は知らない。01 §4.5）。
    backends: Arc<BackendRegistry>,
    oids: OidAllocator,
    shared: Arc<SharedCatalogStore>,
    checkpoint_lock: Mutex<()>,
    checkpointer: Mutex<Option<CheckpointerHandle>>,
    /// When the last checkpoint (or the start) happened, for `checkpoint_timeout`.
    last_checkpoint: Mutex<Instant>,
    checkpoint_timeout: Duration,
    max_wal_size: u64,
    max_connections: u32,
    database_busy_wait: Duration,
    /// 変異テスト用のスイッチ（DB は `skip_pre_copy_checkpoint` などを読む）。
    knobs: DebugKnobs,
    databases: Mutex<HashMap<Oid, Arc<DatabaseHandle>>>,
    poisoned: AtomicBool,
    next_session_id: AtomicU64,
    /// Read-only values for `SHOW` (`shared_buffers`, `data_directory`, ...).
    server_settings: Vec<(&'static str, String)>,
    /// The time zone database (`m4/09` §5.1): one per cluster, read lazily.
    zones: yuzhu_datetime::ZoneDb,
}

fn fatal(state: crate::SqlState, message: String) -> Error {
    Error::new(state, message).with_severity(Severity::Fatal)
}

fn no_such_database(name: &str) -> Error {
    fatal(
        sqlstate::INVALID_CATALOG_NAME,
        format!("database \"{name}\" does not exist"),
    )
}

/// `Cluster::connect` の入力（認証の後。09 章 §4.1、00 §4.8）。
#[derive(Debug)]
pub struct ConnectRequest<'a> {
    pub database: &'a str,
    pub user: &'a str,
    pub client_addr: Option<std::net::IpAddr>,
    pub session_id: u64,
    pub pid: i32,
    pub interrupts: Arc<InterruptFlag>,
}

/// `Cluster::connect` の成功。フィールドの並びが Drop の順序
/// （`db`、`role`、`session_locks`、`backend`）なので、並べ替えない。
#[derive(Debug)]
pub struct ConnectGrant {
    pub db: Arc<DatabaseHandle>,
    pub role: RoleRow,
    pub session_locks: SessionLocks,
    pub backend: BackendGuard,
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
        // Convention 1 (debug builds): waiting for a heavy lock while holding a
        // latch, pins, the gate etc. panics (01 §6.6).
        crate::txn::lock::set_wait_assertion(crate::storage::buffer::track::assert_may_wait);
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
            locks: parts.locks,
            backends: BackendRegistry::new(),
            oids: parts.oids,
            shared: parts.shared,
            checkpoint_lock: Mutex::new(()),
            checkpointer: Mutex::new(None),
            last_checkpoint: Mutex::new(Instant::now()),
            checkpoint_timeout: opts.checkpoint_timeout,
            max_wal_size: opts.max_wal_size,
            max_connections: opts.max_connections,
            database_busy_wait: opts.database_busy_wait,
            knobs: opts.knobs,
            databases: Mutex::new(HashMap::new()),
            poisoned: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
            server_settings: server_settings(opts, parts.rel_seg_blocks, parts.wal_segment_size),
            zones: yuzhu_datetime::ZoneDb::system(),
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
            self.check_shared_catalog()?;
            self.warn_orphans();
            Ok(())
        } else {
            // 7. A clean stop: in production from now on.
            self.check_shared_catalog()?;
            self.warn_orphans();
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
        let locks = LockManager::with_knobs(opts.knobs);
        let outcome = recovery::startup(Arc::clone(vfs), &control, &{
            let mut cfg = StackConfig::new(data.rel_seg_blocks, opts.shared_buffers, opts.knobs);
            cfg.oldest_xid = crate::txn::Xid(data.oldest_xid.max(3));
            cfg.locks = Arc::clone(&locks);
            cfg
        })?;
        // 5. Transactions, OIDs and the shared catalog.
        let stack = outcome.stack;
        // REDO may have seen XIDs past the value the stack was built with.
        stack.procs.raise_next_xid(outcome.next_xid);
        let multixact = Arc::new(MultiXactTable::new(Arc::clone(&control)));
        let txn = TxnManager::new(
            Arc::clone(&stack.procs),
            Arc::clone(&stack.clog),
            Arc::clone(&control),
            Arc::clone(&stack.wal),
            Arc::clone(&locks),
            Arc::clone(&multixact),
            opts.knobs,
        );
        // ヒープに、行ロック・XID 待ち用のロックマネージャと、可視性判定（VACUUM・剪定）の環境を渡す。
        stack.heap.set_lock_env(Arc::clone(&locks));
        stack.heap.set_vacuum_env(stack.procs.clone(), multixact);
        let oids = OidAllocator::new(Arc::clone(&control));
        let storage: Arc<dyn TableStore> = stack.heap.clone();
        let shared = Arc::new(SharedCatalogStore::new(Arc::clone(&storage)));
        Ok(Prepared {
            stack,
            storage,
            control,
            txn,
            locks,
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

    /// 認証の後に呼ぶ（09 章 §5.1。検査順は DB-D1）。失敗はすべて FATAL（`Severity::Panic` を除く）。
    ///
    /// 成功すると `Database(oid)` を `AccessShare` で `LockScope::Session` に取り、
    /// `BackendRegistry` と `LockManager` に登録済みになる。
    pub fn connect(&self, req: &ConnectRequest<'_>) -> Result<ConnectGrant> {
        self.connect_inner(req).map_err(|e| {
            if e.severity == Severity::Error {
                e.with_severity(Severity::Fatal)
            } else {
                e
            }
        })
    }

    /// 検査 1〜2: ロールの存在と `rolcanlogin`、データベース行（無くてもここでは失敗しない）。
    fn lookup_role_and_db(
        &self,
        req: &ConnectRequest<'_>,
    ) -> Result<(RoleRow, Option<DatabaseRow>, i32)> {
        let snap = self.txn.take_snapshot(None, FIRST_COMMAND_ID);
        let Some(role) = self.shared.role_by_name(&snap, req.user)? else {
            return Err(fatal(
                sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{}\" does not exist", req.user),
            ));
        };
        if !role.can_login {
            return Err(fatal(
                sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{}\" is not permitted to log in", req.user),
            ));
        }
        let row_opt = self.shared.database_by_name(&snap, req.database)?;
        let role_limit = self.shared.role_conn_limit(&snap, role.oid)?.unwrap_or(-1);
        Ok((role, row_opt, role_limit))
    }

    fn connect_inner(&self, req: &ConnectRequest<'_>) -> Result<ConnectGrant> {
        let id = BackendId(req.session_id);
        // 1〜2. ロールとデータベース行（ロールの接続数が先）。
        let (role, row_opt, role_limit) = self.lookup_role_and_db(req)?;
        // 3. 自分を登録してから数える（DB-D3）。ローカル変数の Drop は宣言の逆順なので、
        //    session_locks が backend より先に外れる。
        let backend = self.backends.register(BackendInfo {
            id,
            pid: req.pid,
            db_oid: row_opt.as_ref().map_or(0, |r| r.oid),
            user_oid: role.oid,
            client_addr: req.client_addr,
            interrupts: Arc::clone(&req.interrupts),
        });
        let session_locks = SessionLocks::register(&self.locks, id, req.pid);
        let max = usize::try_from(self.max_connections).unwrap_or(usize::MAX);
        if self.backends.all().len() > max {
            return Err(fatal(
                sqlstate::TOO_MANY_CONNECTIONS,
                "sorry, too many clients already".to_owned(),
            ));
        }
        if role_limit >= 0
            && !role.superuser
            && i64::try_from(self.backends.count_of_role(role.oid)).unwrap_or(i64::MAX)
                > i64::from(role_limit)
        {
            return Err(fatal(
                sqlstate::TOO_MANY_CONNECTIONS,
                format!("too many connections for role \"{}\"", role.name),
            ));
        }
        // 4. データベースの存在と Database(oid) の AccessShare。
        let Some(row) = row_opt else {
            return Err(no_such_database(req.database));
        };
        let wait = WaitCtl {
            lock_timeout: None,
            deadlock_timeout: Duration::from_secs(1),
            interrupts: &req.interrupts,
        };
        // 待つ間は、スナップショットも他のロックも持たない（規約 1）。
        self.locks.acquire(
            id,
            LockTag::Database(row.oid),
            LockMode::AccessShare,
            LockScope::Session,
            &wait,
        )?;
        // 5. 待った間のコミットを見るため、スナップショットを取り直す。
        let (row2, db_limit) = {
            let snap = self.txn.take_snapshot(None, FIRST_COMMAND_ID);
            let row2 = self.shared.database_by_oid(&snap, row.oid)?;
            let limit = self
                .shared
                .database_conn_limit(&snap, row.oid)?
                .unwrap_or(-1);
            (row2, limit)
        };
        let row2 = match row2 {
            Some(r) if r.name == row.name => r,
            _ => {
                return Err(no_such_database(req.database)
                    .with_detail("It seems to have just been dropped or renamed."));
            }
        };
        // 6. datallowconn（superuser でも拒否）。
        if !row2.allow_conn {
            return Err(fatal(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "database \"{}\" is not currently accepting connections",
                    row2.name
                ),
            ));
        }
        // 7. データベースの接続数。
        if db_limit >= 0
            && !role.superuser
            && i64::try_from(self.backends.count_in_db(row2.oid)).unwrap_or(i64::MAX)
                > i64::from(db_limit)
        {
            return Err(fatal(
                sqlstate::TOO_MANY_CONNECTIONS,
                format!("too many connections for database \"{}\"", row2.name),
            ));
        }
        // 8. ハンドル。
        let db = self.handle_for(&row2)?;
        Ok(ConnectGrant {
            db,
            role,
            session_locks,
            backend,
        })
    }

    /// 移行用: `ConnectGrant` を捨てて `(db, role)` だけ返す（ロックはすぐ外れる）。
    /// テストと、`ConnectGrant` に移っていない呼び出し元が使う。
    pub fn connect_simple(
        &self,
        database: &str,
        user: &str,
    ) -> Result<(Arc<DatabaseHandle>, RoleRow)> {
        let id = self.next_session_id();
        let grant = self.connect(&ConnectRequest {
            database,
            user,
            client_addr: None,
            session_id: id,
            pid: i32::try_from(id).unwrap_or(i32::MAX),
            interrupts: Arc::new(InterruptFlag::default()),
        })?;
        Ok((Arc::clone(&grant.db), grant.role.clone()))
    }

    /// `databases` のキャッシュから引き、無ければ作る。Mutex の中では挿入だけする。
    fn handle_for(&self, row: &DatabaseRow) -> Result<Arc<DatabaseHandle>> {
        if let Some(h) = lock(&self.databases)?.get(&row.oid) {
            return Ok(Arc::clone(h));
        }
        let fresh = Arc::new(DatabaseHandle {
            oid: row.oid,
            name: row.name.clone(),
            catalog: CatalogStore::new(row.oid, Arc::clone(&self.storage)),
            shared: Arc::clone(&self.shared),
            cache: CatalogCache::default(),
        });
        let mut dbs = lock(&self.databases)?;
        Ok(Arc::clone(dbs.entry(row.oid).or_insert(fresh)))
    }

    /// `oid` のデータベースの `DatabaseHandle`。行が無ければ 3D000（FATAL ではない）。
    /// ロックは取らない（接続以外の呼び出し元用）。
    pub fn database_handle(&self, oid: Oid) -> Result<Arc<DatabaseHandle>> {
        if let Some(h) = lock(&self.databases)?.get(&oid) {
            return Ok(Arc::clone(h));
        }
        let row = {
            let snap = self.txn.take_snapshot(None, FIRST_COMMAND_ID);
            self.shared.database_by_oid(&snap, oid)?
        };
        let Some(row) = row else {
            return Err(Error::new(
                sqlstate::INVALID_CATALOG_NAME,
                format!("database with OID {oid} does not exist"),
            ));
        };
        self.handle_for(&row)
    }

    /// DROP DATABASE の後始末。キャッシュから外す。
    pub fn forget_database(&self, oid: Oid) {
        crate::util::sync::lock_ignore_poison(&self.databases).remove(&oid);
    }

    /// `ClusterOptions.knobs`。
    pub fn knobs(&self) -> DebugKnobs {
        self.knobs
    }

    /// `ClusterOptions.database_busy_wait`（`dbase::lock_database_exclusive` に渡す）。
    pub fn database_busy_wait(&self) -> Duration {
        self.database_busy_wait
    }

    /// `ClusterOptions.max_connections`。
    pub fn max_connections(&self) -> u32 {
        self.max_connections
    }

    /// `base/` のディレクトリと `pg_database` の行の食い違い（09 §5.6）。
    pub fn orphan_report(&self) -> Result<dbase::OrphanReport> {
        let rows = {
            let snap = self.txn.take_snapshot(None, FIRST_COMMAND_ID);
            self.shared.databases(&snap)?
        };
        let dirs = dbase::list_database_dirs(&*self.stack.vfs)?;
        Ok(dbase::find_orphans(&dirs, &rows))
    }

    /// 起動時の孤児検査。診断だけで、起動は止めない（D31、DB-D13）。
    fn warn_orphans(&self) {
        match self.orphan_report() {
            Ok(report) => {
                for oid in report.dirs_without_row {
                    warn(&format!(
                        "database directory \"base/{oid}\" has no row in pg_database (left over from an interrupted CREATE or DROP DATABASE); it is not removed"
                    ));
                }
                for (oid, name) in report.rows_without_dir {
                    warn(&format!(
                        "database \"{name}\" (OID {oid}) has a row in pg_database but its directory \"base/{oid}\" does not exist"
                    ));
                }
            }
            Err(e) => warn(&format!(
                "could not check database directories: {}",
                e.message
            )),
        }
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

    /// `StorageStack::index`（B+Tree。`m4/02-pipeline-refactor.md` §5.2 の P0-b、提案 14）。
    pub fn indexes(&self) -> &Arc<dyn IndexStore> {
        &self.stack.index
    }

    /// `StorageStack::seq`（シーケンス。同上）。
    pub fn sequences(&self) -> &Arc<dyn SequenceStore> {
        &self.stack.seq
    }

    /// The time zone database (`yuzhu_datetime::ZoneDb`), shared by every session.
    pub fn zones(&self) -> &yuzhu_datetime::ZoneDb {
        &self.zones
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

    /// 重量ロックの管理者（01 §4.2）。
    pub fn locks(&self) -> &Arc<LockManager> {
        &self.locks
    }

    /// 接続中のバックエンドの登録簿（01 §4.5）。
    pub fn backends(&self) -> &Arc<BackendRegistry> {
        &self.backends
    }

    /// `MultiXact` の表（行ロックのメンバー集合。`TxnManager` が持つ）。
    pub fn multixact(&self) -> &Arc<MultiXactTable> {
        self.txn.multixact()
    }

    /// Values of the read-only parameters (`shared_buffers`, ...), applied
    /// to each new session's `Settings`.
    pub fn server_settings(&self) -> &[(&'static str, String)] {
        &self.server_settings
    }

    /// Allocates a session ID (the `BackendId` and the pid); called once by
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
    locks: Arc<LockManager>,
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
        (
            "authentication_timeout",
            format_timeout(opts.authentication_timeout.as_secs()),
        ),
    ]
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

/// 認証が使うロールの情報（00 §4.8。`pg_authid` から作る）。
#[derive(Clone, PartialEq, Eq)]
pub struct RoleAuthInfo {
    pub oid: Oid,
    pub name: String,
    /// `rolpassword`（SCRAM の保存形式）。
    pub password: Option<String>,
    /// UNIX エポックからのマイクロ秒。`None` は期限なし。
    pub valid_until: Option<i64>,
    pub can_login: bool,
    pub conn_limit: i32,
    pub superuser: bool,
}

impl std::fmt::Debug for RoleAuthInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoleAuthInfo")
            .field("oid", &self.oid)
            .field("name", &self.name)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("valid_until", &self.valid_until)
            .field("can_login", &self.can_login)
            .field("conn_limit", &self.conn_limit)
            .field("superuser", &self.superuser)
            .finish()
    }
}

/// 接続時のロールの検査（08 章 §5.1 手順 9 の a・b・e。AU-3）。`Cluster::connect` の本体は DB-1 が持つ。
impl Cluster {
    /// 認証の前に呼ぶ。存在しなければ `None`。カタログ用スナップショットで `pg_authid` を引く。
    pub fn lookup_role(&self, user: &str) -> Result<Option<RoleAuthInfo>> {
        Ok(self.authid_by_name_now(user)?.map(|r| r.to_auth_info()))
    }

    fn authid_by_name_now(&self, user: &str) -> Result<Option<crate::catalog::roles::AuthidRow>> {
        let snap = self.txn.take_snapshot(None, FIRST_COMMAND_ID);
        Ok(self.shared.authid_by_name(&snap, user)?.map(|(_, r)| r))
    }

    /// 手順 9 の a・b。ロールが無ければ FATAL 28000 `role "x" does not exist`、LOGIN でなければ
    /// FATAL 28000 `role "x" is not permitted to log in`。最新の行を返す。
    pub fn check_role_can_connect(&self, user: &str) -> Result<crate::catalog::roles::AuthidRow> {
        let Some(role) = self.authid_by_name_now(user)? else {
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
        Ok(role)
    }

    /// 手順 9 の e。自分を `BackendRegistry` に登録した後で呼ぶ（自分を数えて超えたら拒否）。
    /// スーパーユーザーと `conn_limit < 0` は対象外。`conn_limit = 0` は（スーパーユーザー以外の）すべてを拒否する。
    pub fn check_role_conn_limit(&self, role: &crate::catalog::roles::AuthidRow) -> Result<()> {
        if role.superuser || role.conn_limit < 0 {
            return Ok(());
        }
        let n = i64::try_from(self.backends.count_of_role(role.oid)).unwrap_or(i64::MAX);
        if n > i64::from(role.conn_limit) {
            return Err(fatal(
                sqlstate::TOO_MANY_CONNECTIONS,
                format!("too many connections for role \"{}\"", role.name),
            ));
        }
        Ok(())
    }
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
            ..ClusterOptions::new(PathBuf::from("/sim/data"))
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
        let (db, _) = c.connect_simple("postgres", "postgres").unwrap();
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
            identity: None,
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
                    indexes: vec![],
                    extra_depends: vec![],
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
        let (db, _) = c.connect_simple("postgres", "postgres").unwrap();
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

    /// A fresh backend (registered with the lock manager) takes an XID.
    fn begin(t: &Arc<TxnManager>) -> (crate::txn::Xid, crate::txn::XidGuard) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1000);
        let id = crate::backend::BackendId(NEXT.fetch_add(1, Ordering::Relaxed));
        t.locks().register_backend(id, 1);
        t.assign_xid(id).unwrap()
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

    /// M3 / M4 のデータディレクトリ（制御ファイルの `format_version` 2、`catalog_version` は M4 の値）を
    /// M5 のサーバは起動を拒否する（00 D35）。拒否は FATAL で、pid ファイルを残さない。
    #[test]
    fn m4_data_directory_is_refused_clearly() {
        use crate::storage::vfs::{OpenMode, Vfs};
        use crate::util::crc32c::crc32c;
        let sim = fresh();
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        let file = vfs
            .open(
                Path::new(crate::control::CONTROL_FILE_PATH),
                OpenMode::ReadWrite,
            )
            .unwrap();
        // M4 の形に書き換える: format_version 2、catalog_version は M4 の値、オフセット 120 以降は 0。
        for slot_off in [0u64, 4096] {
            let mut slot = vec![0u8; 512];
            file.read_exact_at(&mut slot, slot_off).unwrap();
            if &slot[0..8] != b"YUZHUCTL" {
                continue;
            }
            slot[8..12].copy_from_slice(&2u32.to_le_bytes());
            slot[12..16].copy_from_slice(&2_026_100_601u32.to_le_bytes());
            slot[120..508].fill(0);
            let crc = crc32c(&slot[..508]);
            slot[508..512].copy_from_slice(&crc.to_le_bytes());
            file.write_all_at(&slot, slot_off).unwrap();
        }
        file.sync_all().unwrap();
        let e = open(&sim, opts()).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.message, "database files are incompatible with server");
        let detail = e.detail.as_deref().unwrap();
        assert!(detail.contains("format version 2"), "{detail}");
        assert!(detail.contains("format version 3"), "{detail}");
        assert_eq!(e.hint.as_deref(), Some("Re-run yuzhu-initdb."));
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
        c.connect_simple("postgres", "postgres").unwrap();
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
    fn cluster_shares_one_lock_manager_and_a_backend_registry() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        // The transaction manager and the cluster use the same lock manager.
        assert!(Arc::ptr_eq(c.locks(), c.txn_manager().locks()));
        assert!(Arc::ptr_eq(c.multixact(), c.txn_manager().multixact()));
        // The stack's ProcArray is the one the transaction manager uses.
        assert!(Arc::ptr_eq(&c.stack().procs, c.txn_manager().procs()));
        assert!(c.backends().all().is_empty());
        c.shutdown().unwrap();
    }

    #[test]
    fn open_registers_the_rule_one_assertion() {
        use crate::txn::lock::{LockMode, LockScope, LockTag, WaitCtl};
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let id = crate::backend::BackendId(4242);
        c.locks().register_backend(id, 1);
        let irq = crate::interrupt::InterruptFlag::default();
        let ctl = WaitCtl {
            lock_timeout: None,
            deadlock_timeout: Duration::from_secs(1),
            interrupts: &irq,
        };
        let tag = LockTag::TransactionId(crate::txn::Xid(777));
        // Nothing held: waiting is allowed (the lock is free here).
        c.locks()
            .acquire(id, tag, LockMode::Share, LockScope::Transaction, &ctl)
            .unwrap();
        c.locks().release_all(id, LockScope::Transaction);
        if cfg!(debug_assertions) {
            // Holding the commit gate: waiting is a bug, even if no wait is needed.
            let gate = c.txn_manager().commit_gate_exclusive().unwrap();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = c
                    .locks()
                    .acquire(id, tag, LockMode::Share, LockScope::Transaction, &ctl);
            }));
            assert!(r.is_err(), "acquire under the commit gate must panic");
            drop(gate);
        }
        c.locks().release_all(id, LockScope::Session);
        c.locks().unregister_backend(id);
        c.shutdown().unwrap();
    }
    #[test]
    fn session_ids_increase_and_caches_invalidate() {
        let sim = fresh();
        let c = open(&sim, opts()).unwrap();
        let a = c.next_session_id();
        assert!(c.next_session_id() > a);
        let (h, _) = c.connect_simple("postgres", "postgres").unwrap();
        let g = h.cache.generation();
        c.invalidate_all_catalog_caches();
        assert!(h.cache.generation() > g);
        // The same handle is returned for the same database.
        let (h2, _) = c.connect_simple("postgres", "postgres").unwrap();
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
        let e = c.connect_simple("nope", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_CATALOG_NAME);
        let e = c.connect_simple("template0", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        let e = c.connect_simple("postgres", "nobody").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        c.shutdown().unwrap();
    }
}

#[cfg(test)]
mod connect_tests {
    //! `connect` の検査順・接続数・`Database(oid)` ロック・孤児検査（09 章 §5.1、§5.6）。
    use super::*;
    use crate::bootstrap::{InitdbOptions, initdb};
    use crate::catalog::schema::oids;
    use crate::dbase::{BusyKind, HeldLock, lock_database_exclusive};
    use crate::storage::WriteCtx;
    use crate::storage::vfs::SimVfs;
    use crate::types::Datum;

    const POSTGRES_DB: Oid = 5;
    const ALICE: Oid = 20_001;
    const LIMITED_DB: Oid = 20_002;

    fn cluster_with(max_connections: u32, busy: Duration) -> (SimVfs, Arc<Cluster>) {
        let sim = SimVfs::new(1);
        initdb(Arc::new(sim.clone()), &InitdbOptions::new("postgres")).unwrap();
        let opts = ClusterOptions {
            shared_buffers: 256,
            max_connections,
            background_checkpointer: false,
            database_busy_wait: busy,
            ..ClusterOptions::new(PathBuf::from("/sim/data"))
        };
        let c = Cluster::open(Arc::new(sim.clone()), opts).unwrap();
        (sim, c)
    }

    fn cluster() -> Arc<Cluster> {
        cluster_with(16, Duration::from_millis(300)).1
    }

    /// 既存の行を写して、`edit` で直した行を共有カタログに足す（コミット済み）。
    fn insert_clone(
        c: &Arc<Cluster>,
        catalog: Oid,
        name_col: &str,
        name: &str,
        edit: &dyn Fn(&mut Vec<Datum>),
    ) {
        let (_, rel) = c.shared.rel(catalog).unwrap().clone();
        let idx = crate::catalog::rows::column_index(catalog, name_col);
        let t = c.txn_manager();
        let snap = t.snapshot(None, 0);
        let mut scan = c.storage().begin_scan(&rel, &snap).unwrap();
        let mut found = None;
        while let Some(tuple) = c.storage().scan_next(&mut scan).unwrap() {
            if tuple.row[idx] == Datum::Text(name.to_owned()) {
                found = Some(tuple.row.clone());
            }
        }
        drop(scan);
        let mut row = found.unwrap();
        edit(&mut row);
        let id = BackendId(c.next_session_id());
        t.locks().register_backend(id, 1);
        let (xid, guard) = t.assign_xid(id).unwrap();
        c.storage()
            .insert(&rel, &WriteCtx { xid, cid: 0 }, &row)
            .unwrap();
        t.commit(xid, &[]).unwrap();
        drop(guard);
        t.locks().release_all(id, LockScope::Session);
        t.locks().unregister_backend(id);
    }

    fn add_role(c: &Arc<Cluster>, oid: Oid, name: &str, superuser: bool, limit: i32) {
        let su = crate::catalog::rows::column_index(oids::PG_AUTHID, "rolsuper");
        let nm = crate::catalog::rows::column_index(oids::PG_AUTHID, "rolname");
        let cl = crate::catalog::rows::column_index(oids::PG_AUTHID, "rolconnlimit");
        insert_clone(c, oids::PG_AUTHID, "rolname", "postgres", &|r| {
            r[0] = Datum::Oid(oid);
            r[nm] = Datum::Text(name.to_owned());
            r[su] = Datum::Bool(superuser);
            r[cl] = Datum::Int4(limit);
        });
    }

    fn add_db(c: &Arc<Cluster>, oid: Oid, name: &str, limit: i32) {
        let nm = crate::catalog::rows::column_index(oids::PG_DATABASE, "datname");
        let cl = crate::catalog::rows::column_index(oids::PG_DATABASE, "datconnlimit");
        insert_clone(c, oids::PG_DATABASE, "datname", "postgres", &|r| {
            r[0] = Datum::Oid(oid);
            r[nm] = Datum::Text(name.to_owned());
            r[cl] = Datum::Int4(limit);
        });
    }

    fn request<'a>(c: &Cluster, database: &'a str, user: &'a str) -> ConnectRequest<'a> {
        let id = c.next_session_id();
        ConnectRequest {
            database,
            user,
            client_addr: None,
            session_id: id,
            pid: i32::try_from(id).unwrap(),
            interrupts: Arc::new(InterruptFlag::default()),
        }
    }

    fn connect(c: &Cluster, database: &str, user: &str) -> Result<ConnectGrant> {
        c.connect(&request(c, database, user))
    }

    fn assert_fatal(e: &Error, state: crate::SqlState) {
        assert_eq!(e.sqlstate, state, "{}", e.message);
        assert_eq!(e.severity, Severity::Fatal);
    }

    #[test]
    fn database_lock_is_held_for_the_session_and_released_on_drop() {
        let c = cluster();
        let g = connect(&c, "postgres", "postgres").unwrap();
        let id = g.backend.id();
        assert!(
            c.locks()
                .holds(id, LockTag::Database(POSTGRES_DB), LockMode::AccessShare)
        );
        assert_eq!(c.backends().count_in_db(POSTGRES_DB), 1);
        assert_eq!(g.db.oid, POSTGRES_DB);
        drop(g);
        assert_eq!(c.backends().count_in_db(POSTGRES_DB), 0);
        assert!(c.locks().lock_status().is_empty());
        // 同じ ID で入り直せる（登録が残っていない）。
        c.locks().register_backend(id, 1);
        c.locks().unregister_backend(id);
        c.shutdown().unwrap();
    }

    #[test]
    fn check_order_follows_postgresql() {
        let c = cluster();
        add_role(&c, ALICE, "alice", false, 0);
        // ロールが先（存在しないロール × 存在しない DB / template0）。
        let e = connect(&c, "nope", "nobody").unwrap_err();
        assert_fatal(&e, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        let e = connect(&c, "template0", "nobody").unwrap_err();
        assert_fatal(&e, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        // ロールの接続数は存在しない DB より先。
        let e = connect(&c, "nope", "alice").unwrap_err();
        assert_fatal(&e, sqlstate::TOO_MANY_CONNECTIONS);
        assert!(
            e.message
                .contains("too many connections for role \"alice\"")
        );
        // 存在しない DB。
        let e = connect(&c, "nope", "postgres").unwrap_err();
        assert_fatal(&e, sqlstate::INVALID_CATALOG_NAME);
        // template0 は superuser でも 55000。
        let e = connect(&c, "template0", "postgres").unwrap_err();
        assert_fatal(&e, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        // 失敗した接続は登録簿にもロックにも残らない。
        assert!(c.backends().all().is_empty());
        assert!(c.locks().lock_status().is_empty());
        c.shutdown().unwrap();
    }

    #[test]
    fn max_connections_counts_registered_sessions() {
        let (_sim, c) = cluster_with(2, Duration::from_millis(300));
        let g1 = connect(&c, "postgres", "postgres").unwrap();
        let _g2 = connect(&c, "postgres", "postgres").unwrap();
        let e = connect(&c, "postgres", "postgres").unwrap_err();
        assert_fatal(&e, sqlstate::TOO_MANY_CONNECTIONS);
        assert_eq!(e.message, "sorry, too many clients already");
        assert_eq!(c.backends().all().len(), 2);
        drop(g1);
        connect(&c, "postgres", "postgres").unwrap();
        assert_eq!(c.max_connections(), 2);
    }

    #[test]
    fn role_connection_limit() {
        let c = cluster();
        add_role(&c, ALICE, "alice", false, 1);
        add_role(&c, ALICE + 10, "root0", true, 0);
        let g = connect(&c, "postgres", "alice").unwrap();
        let e = connect(&c, "postgres", "alice").unwrap_err();
        assert_fatal(&e, sqlstate::TOO_MANY_CONNECTIONS);
        assert_eq!(e.message, "too many connections for role \"alice\"");
        // 拒否されたセッションは数えに残らない。
        assert_eq!(c.backends().count_of_role(ALICE), 1);
        // superuser は上限から除外される（CONNECTION LIMIT 0 でも入れる）。
        connect(&c, "postgres", "root0").unwrap();
        drop(g);
        connect(&c, "postgres", "alice").unwrap();
    }

    #[test]
    fn database_connection_limit() {
        let c = cluster();
        add_role(&c, ALICE, "alice", false, -1);
        add_db(&c, LIMITED_DB, "limited", 1);
        add_db(&c, LIMITED_DB + 1, "closed", 0);
        let g = connect(&c, "limited", "alice").unwrap();
        let e = connect(&c, "limited", "alice").unwrap_err();
        assert_fatal(&e, sqlstate::TOO_MANY_CONNECTIONS);
        assert_eq!(e.message, "too many connections for database \"limited\"");
        assert_eq!(c.backends().count_in_db(LIMITED_DB), 1);
        // 拒否されたセッションの Database ロックも残らない。
        let held = c.locks().held_locks(g.backend.id());
        assert_eq!(
            held,
            vec![(LockTag::Database(LIMITED_DB), LockMode::AccessShare)]
        );
        // CONNECTION LIMIT 0: superuser 以外は全員拒否、superuser は入れる。
        let e = connect(&c, "closed", "alice").unwrap_err();
        assert_fatal(&e, sqlstate::TOO_MANY_CONNECTIONS);
        connect(&c, "closed", "postgres").unwrap();
    }

    #[test]
    fn exclusive_lock_sees_other_sessions_and_waits_for_them() {
        let c = cluster();
        let g = connect(&c, "postgres", "postgres").unwrap();
        let me = BackendId(c.next_session_id());
        let _sl = SessionLocks::register(c.locks(), me, 1);
        let intr = InterruptFlag::default();
        let e = lock_database_exclusive(
            c.locks(),
            c.backends(),
            me,
            0,
            POSTGRES_DB,
            "postgres",
            BusyKind::Target,
            Duration::from_millis(250),
            &intr,
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_IN_USE);
        assert_eq!(
            e.message,
            "database \"postgres\" is being accessed by other users"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("There is 1 other session using the database.")
        );
        let e = lock_database_exclusive(
            c.locks(),
            c.backends(),
            me,
            0,
            POSTGRES_DB,
            "postgres",
            BusyKind::Source,
            Duration::ZERO,
            &intr,
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "source database \"postgres\" is being accessed by other users"
        );
        // 相手が出ていけば、待っている間に取れる。
        let c2 = Arc::clone(&c);
        let waiter = std::thread::spawn(move || {
            let intr = InterruptFlag::default();
            lock_database_exclusive(
                c2.locks(),
                c2.backends(),
                me,
                0,
                POSTGRES_DB,
                "postgres",
                BusyKind::Target,
                Duration::from_secs(5),
                &intr,
            )
            .map(|_| ())
        });
        std::thread::sleep(Duration::from_millis(150));
        drop(g);
        waiter.join().unwrap().unwrap();
        c.shutdown().unwrap();
    }

    #[test]
    fn connect_waits_for_exclusive_lock_then_rechecks_the_row() {
        let c = cluster();
        let owner = BackendId(c.next_session_id());
        let sl = SessionLocks::register(c.locks(), owner, 1);
        let held = HeldLock::try_acquire(
            c.locks(),
            owner,
            LockTag::Database(POSTGRES_DB),
            LockMode::AccessExclusive,
        )
        .unwrap();
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || connect(&c2, "postgres", "postgres").map(|g| g.db.oid));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!t.is_finished());
        drop(held);
        assert_eq!(t.join().unwrap().unwrap(), POSTGRES_DB);
        drop(sl);
    }

    #[test]
    fn waiting_connect_is_interrupted_by_terminate() {
        let c = cluster();
        let owner = BackendId(c.next_session_id());
        let _sl = SessionLocks::register(c.locks(), owner, 1);
        let _held = HeldLock::try_acquire(
            c.locks(),
            owner,
            LockTag::Database(POSTGRES_DB),
            LockMode::AccessExclusive,
        )
        .unwrap();
        let req = request(&c, "postgres", "postgres");
        let intr = Arc::clone(&req.interrupts);
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || c2.connect(&req).map(|_| ()));
        std::thread::sleep(Duration::from_millis(150));
        intr.request_terminate();
        let e = t.join().unwrap().unwrap_err();
        assert_fatal(&e, sqlstate::ADMIN_SHUTDOWN);
        assert_eq!(c.backends().all().len(), 0);
    }

    #[test]
    fn connect_after_the_row_vanished_is_3d000() {
        let c = cluster();
        // 行を引いた後にロックを待つ間に消えた、を作れないので、取り直し側だけを確かめる:
        // `database_by_oid` が行を見つけられない OID は 3D000（FATAL でない版）。
        let e = c.database_handle(99_999).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_CATALOG_NAME);
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.message, "database with OID 99999 does not exist");
    }

    #[test]
    fn database_handle_is_cached_until_forgotten() {
        let c = cluster();
        let g = connect(&c, "postgres", "postgres").unwrap();
        let h = c.database_handle(POSTGRES_DB).unwrap();
        assert!(Arc::ptr_eq(&h, &g.db));
        c.forget_database(POSTGRES_DB);
        let h2 = c.database_handle(POSTGRES_DB).unwrap();
        assert!(!Arc::ptr_eq(&h, &h2));
        assert_eq!(h2.name, "postgres");
        let g2 = connect(&c, "postgres", "postgres").unwrap();
        assert!(Arc::ptr_eq(&h2, &g2.db));
    }

    #[test]
    fn orphan_report_lists_both_directions() {
        let c = cluster();
        assert_eq!(c.orphan_report().unwrap(), dbase::OrphanReport::default());
        let vfs = &c.stack().vfs;
        vfs.create_dir(Path::new("base/777")).unwrap();
        vfs.create_dir(Path::new("base/not_a_number")).unwrap();
        add_db(&c, LIMITED_DB, "ghost", -1);
        let r = c.orphan_report().unwrap();
        assert_eq!(r.dirs_without_row, vec![777]);
        assert_eq!(r.rows_without_dir, vec![(LIMITED_DB, "ghost".to_owned())]);
        // 起動は止まらない。
        c.warn_orphans();
    }
}
