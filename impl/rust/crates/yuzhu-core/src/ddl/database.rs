//! `CREATE DATABASE`（M5 DB-2。`m5/09` §5.3）。`DROP DATABASE` / `ALTER DATABASE` は DB-3 が足す。
//!
//! 手順: チェックポイント → ディレクトリのコピー → `DBASE_CREATE_FILE_COPY` を WAL → 行の挿入 →
//! コミット → チェックポイント。文の途中でトランザクションを閉じるので、`TxnControl` を通して実行する。
//! 名前予約・テンプレート・新しい DB のロックは `LockScope::Session` で最後まで持つ（DB-D6）。

use std::sync::Arc;

use super::{DdlCtx, TxnControl};
use crate::analyzer::ddl_ext::database::{
    BoundAlterDatabase, BoundCreateDatabase, BoundDropDatabase,
};
use crate::catalog::roles::lock_role_shared;
use crate::catalog::store_db::{DatabaseFlagUpdate, DatabaseInfo, NewDatabase};
use crate::dbase::{
    BusyKind, CreatedDirGuard, DbaseParts, HeldLock, lock_database_exclusive, log_create_file_copy,
    log_drop, name_lock_tag, remove_database_dir,
};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::vfs::Vfs;
use crate::txn::lock::{LockMode, LockScope, LockTag};
use crate::txn::{FIRST_COMMAND_ID, Xid};
use crate::types::Oid;

/// フェーズ 1 の結果。ロックはここが持つ（Drop の順序: `new_lock` → `tpl_lock` → `name_lock`）。
struct Plan {
    tpl: DatabaseInfo,
    owner: Oid,
    new_oid: Oid,
    frozen8: Xid,
    vfs: Arc<dyn Vfs>,
    _new_lock: HeldLock,
    _tpl_lock: HeldLock,
    _name_lock: HeldLock,
}

fn permission_denied(message: String) -> Error {
    Error::new(sqlstate::INSUFFICIENT_PRIVILEGE, message)
}

fn template_missing(name: &str) -> Error {
    Error::new(
        sqlstate::INVALID_CATALOG_NAME,
        format!("template database \"{name}\" does not exist"),
    )
}

fn check_template_permission(ctx: &DdlCtx<'_>, tpl: &DatabaseInfo) -> Result<()> {
    let allowed = tpl.row.is_template || ctx.role.superuser || tpl.owner == ctx.role.oid;
    if !allowed {
        return Err(permission_denied(format!(
            "permission denied to copy database \"{}\"",
            tpl.row.name
        )));
    }
    Ok(())
}

/// 手順 1: 所有者のロール（`DROP ROLE` との競合を塞ぐ）と、実行したロールの権限。所有者の OID を返す。
fn resolve_owner(ctx: &DdlCtx<'_>, b: &BoundCreateDatabase) -> Result<Oid> {
    let cluster = ctx.cluster;
    let shared = &ctx.db.shared;
    let take = || cluster.txn_manager().take_snapshot(None, FIRST_COMMAND_ID);
    let snap = take();
    let owner = match &b.owner {
        None => ctx.role.oid,
        Some(name) => {
            let missing = || {
                Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("role \"{name}\" does not exist"),
                )
            };
            let (_, row) = shared.authid_by_name(&snap, name)?.ok_or_else(missing)?;
            lock_role_shared(cluster.locks(), ctx.backend, row.oid, ctx.wait)?;
            // ロックを取った後で、存在を確かめ直す。
            if shared.authid_by_oid(&take(), row.oid)?.is_none() {
                return Err(missing());
            }
            row.oid
        }
    };
    if !ctx.role.superuser && owner != ctx.role.oid {
        let name = b.owner.as_deref().unwrap_or_default();
        return Err(permission_denied(format!(
            "must be able to SET ROLE \"{name}\""
        )));
    }
    let can_create = ctx.role.superuser
        || shared
            .authid_by_oid(&snap, ctx.role.oid)?
            .is_some_and(|(_, r)| r.create_db);
    if !can_create {
        return Err(permission_denied(
            "permission denied to create database".to_owned(),
        ));
    }
    Ok(owner)
}

/// フェーズ 1（検査とロック。WAL は書かない）。最後にコピー前のチェックポイントまで行う。
fn prepare(ctx: &mut DdlCtx<'_>, b: &BoundCreateDatabase) -> Result<Plan> {
    if !ctx.outside_block {
        return Err(Error::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "CREATE DATABASE cannot run inside a transaction block",
        ));
    }
    let cluster = ctx.cluster;
    let shared = Arc::clone(&ctx.db.shared);
    let locks = cluster.locks();
    let me = ctx.backend;
    let take = || cluster.txn_manager().take_snapshot(None, FIRST_COMMAND_ID);

    // 1. 所有者のロールとその同時削除の防止、権限。
    let owner = resolve_owner(ctx, b)?;

    // 2. 同名の CREATE / DROP を直列化する（待つ）。
    let name_lock = HeldLock::acquire(
        locks,
        me,
        name_lock_tag(&b.name),
        LockMode::Exclusive,
        ctx.wait,
    )?;

    // 3. ロックの後でスナップショットを取り、存在と権限を検査する。
    let snap = take();
    let tpl = shared
        .database_info_by_name(&snap, &b.template)?
        .ok_or_else(|| template_missing(&b.template))?;
    check_template_permission(ctx, &tpl)?;
    if shared.database_info_by_name(&snap, &b.name)?.is_some() {
        return Err(Error::new(
            sqlstate::DUPLICATE_DATABASE,
            format!("database \"{}\" already exists", b.name),
        ));
    }
    drop(snap);

    // 4. テンプレートに他のセッションが居ないこと（最長 database_busy_wait 待つ）。
    let tpl_lock = lock_database_exclusive(
        locks,
        cluster.backends(),
        me,
        ctx.db.oid,
        tpl.row.oid,
        &tpl.row.name,
        BusyKind::Source,
        cluster.database_busy_wait(),
        ctx.interrupts,
    )?;

    // 5. 待っている間に DROP / 更新された可能性があるので取り直す。
    let snap = take();
    let tpl = shared
        .database_info_by_oid(&snap, tpl.row.oid)?
        .ok_or_else(|| template_missing(&b.template))?;
    check_template_permission(ctx, &tpl)?;

    // 6. 新しい OID（行との重複と base/<oid> の孤児を避ける）と、新しい DB のロック。
    let vfs = Arc::clone(&cluster.stack().vfs);
    let new_oid = shared.get_new_database_oid(cluster.oid_allocator(), &*vfs)?;
    let new_lock = HeldLock::try_acquire(
        locks,
        me,
        LockTag::Database(new_oid),
        LockMode::AccessExclusive,
    )
    .ok_or_else(|| Error::internal(format!("could not lock the new database {new_oid}")))?;

    // 7. 凍結境界。番兵（template0）は oldest_xmin() になる。
    let src8 = shared.datxid(&snap, tpl.row.oid)?.ok_or_else(|| {
        Error::new(
            sqlstate::DATA_CORRUPTED,
            format!("yz_datxid has no row for database {}", tpl.row.oid),
        )
    })?;
    let frozen8 = src8.min(cluster.txn_manager().oldest_xmin());
    drop(snap);

    // 8. テンプレートの dirty ページをすべてディスクに書く（以後、テンプレートはロックで変更されない）。
    if !cluster.knobs().skip_pre_copy_checkpoint {
        crate::storage::buffer::assert_no_pins();
        cluster.checkpoint()?;
    }
    Ok(Plan {
        tpl,
        owner,
        new_oid,
        frozen8,
        vfs,
        _new_lock: new_lock,
        _tpl_lock: tpl_lock,
        _name_lock: name_lock,
    })
}

/// フェーズ 2 の手順 9〜12（コピー、WAL、行の挿入）。
fn copy_and_insert(ctx: &mut DdlCtx<'_>, b: &BoundCreateDatabase, plan: &Plan) -> Result<()> {
    let cluster = ctx.cluster;
    // 9. XID（なければ割り当てる）。
    if ctx.txn.guard.is_none() {
        let (xid, guard) = cluster.txn_manager().assign_xid(ctx.backend)?;
        ctx.txn.xid = Some(xid);
        ctx.txn.guard = Some(guard);
    }
    let w = ctx.write_ctx()?;
    let knobs = cluster.knobs();
    // 10. コピー（各ファイルとディレクトリを fsync。キャンセルは 57014）。
    let interrupts = ctx.interrupts;
    crate::datadir::copy_database_dir_checked(
        &*plan.vfs,
        plan.tpl.row.oid,
        plan.new_oid,
        !knobs.skip_copy_fsync,
        &mut || interrupts.check(),
    )?;
    // 11. DBASE_CREATE_FILE_COPY（flush しない。コミットの flush に含まれる）。
    if !knobs.skip_dbase_wal {
        let stack = cluster.stack();
        let parts = DbaseParts {
            pool: &stack.pool,
            smgr: &stack.smgr,
            wal: &stack.wal,
            txn: cluster.txn_manager(),
        };
        log_create_file_copy(&parts, w.xid, plan.tpl.row.oid, plan.new_oid)?;
    }
    // 12. pg_database と yz_datxid の行。
    let frozen_low = u32::try_from(plan.frozen8.0 & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    ctx.db.shared.insert_database(
        &w,
        &NewDatabase {
            oid: plan.new_oid,
            name: b.name.clone(),
            owner: plan.owner,
            is_template: b.is_template,
            allow_conn: b.allow_conn,
            conn_limit: b.conn_limit,
            frozen_xid: frozen_low,
            min_mxid: plan.tpl.min_mxid,
            frozen8: plan.frozen8,
        },
    )
}

/// `CREATE DATABASE` を実行する。コマンドタグを返す。
#[allow(
    clippy::needless_pass_by_value,
    reason = "execute_standalone の契約（BoundDdl の所有権を渡す）"
)]
pub fn create_database(ctl: &mut dyn TxnControl, b: BoundCreateDatabase) -> Result<String> {
    let plan = {
        let mut ctx = ctl.ddl_ctx()?;
        prepare(&mut ctx, &b)?
    };
    let mut guard = CreatedDirGuard::new(Arc::clone(&plan.vfs), plan.new_oid);
    {
        let mut ctx = ctl.ddl_ctx()?;
        if let Err(e) = copy_and_insert(&mut ctx, &b, &plan) {
            guard.remove_now();
            return Err(e);
        }
    }
    // コミットの結果が分からなくなる前に無効化する（コミット済みの DB のディレクトリを消さない）。
    guard.disarm();
    ctl.commit_and_restart()?;
    // 14. REDO の範囲を DBASE_CREATE より後ろに進める。失敗しても DB は存在する（M5-DB-Q3）。
    crate::storage::buffer::assert_no_pins();
    ctl.ddl_ctx()?.cluster.checkpoint()?;
    Ok("CREATE DATABASE".to_owned())
}

fn must_be_owner(name: &str) -> Error {
    permission_denied(format!("must be owner of database {name}"))
}

fn database_missing(name: &str) -> Error {
    Error::new(
        sqlstate::INVALID_CATALOG_NAME,
        format!("database \"{name}\" does not exist"),
    )
}

/// DROP のフェーズ 1 の結果。ロックはここが持つ（Drop の順序: `_lock` → `_name_lock`）。
struct DropPlan {
    info: DatabaseInfo,
    _lock: HeldLock,
    _name_lock: HeldLock,
}

/// フェーズ 1（手順 2〜8）。`None` は `IF EXISTS` で対象が無いこと（NOTICE 済み）。
fn prepare_drop(ctx: &mut DdlCtx<'_>, b: &BoundDropDatabase) -> Result<Option<DropPlan>> {
    if !ctx.outside_block {
        return Err(Error::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "DROP DATABASE cannot run inside a transaction block",
        ));
    }
    let cluster = ctx.cluster;
    let shared = Arc::clone(&ctx.db.shared);
    let locks = cluster.locks();
    let me = ctx.backend;
    let take = || cluster.txn_manager().take_snapshot(None, FIRST_COMMAND_ID);

    // 2. 同名の CREATE / DROP / ALTER を直列化する。
    let name_lock = HeldLock::acquire(
        locks,
        me,
        name_lock_tag(&b.name),
        LockMode::Exclusive,
        ctx.wait,
    )?;
    // 3. ロックの後でスナップショットを取り、存在を確かめる。
    let found = shared.database_info_by_name(&take(), &b.name)?;
    let Some(info) = found else {
        if b.if_exists {
            ctx.notice(
                Severity::Notice,
                sqlstate::SUCCESSFUL_COMPLETION,
                format!("database \"{}\" does not exist, skipping", b.name),
                None,
            );
            return Ok(None);
        }
        return Err(database_missing(&b.name));
    };
    // 4〜6. 所有者、テンプレート、現在の DB（PostgreSQL の検査順）。
    if !(ctx.role.superuser || info.owner == ctx.role.oid) {
        return Err(must_be_owner(&b.name));
    }
    if info.row.is_template {
        return Err(Error::new(
            sqlstate::WRONG_OBJECT_TYPE,
            "cannot drop a template database",
        ));
    }
    if info.row.oid == ctx.db.oid {
        return Err(Error::new(
            sqlstate::OBJECT_IN_USE,
            "cannot drop the currently open database",
        ));
    }
    // 7. 他のセッションが居なくなるのを待って AccessExclusive を取る。
    let lock = lock_database_exclusive(
        locks,
        cluster.backends(),
        me,
        ctx.db.oid,
        info.row.oid,
        &info.row.name,
        BusyKind::Target,
        cluster.database_busy_wait(),
        ctx.interrupts,
    )?;
    // 8. 待っている間の変化に備えて取り直す（名前予約ロックがあるので通常は起きない）。
    let info = shared
        .database_info_by_oid(&take(), info.row.oid)?
        .ok_or_else(|| database_missing(&b.name))?;
    Ok(Some(DropPlan {
        info,
        _lock: lock,
        _name_lock: name_lock,
    }))
}

/// フェーズ 3 の手順 11: バッファを捨て、`DBASE_DROP` を WAL に入れて flush し、ディレクトリを消す。
/// チェックポイントの書き出しとはバリアで排他する（D10）。
fn discard_files(cluster: &crate::engine::Cluster, oid: Oid) -> Result<()> {
    let stack = cluster.stack();
    let _barrier = cluster.txn_manager().exclusive_barrier()?;
    stack.pool.drop_database_buffers(oid)?;
    stack.smgr.forget_database(oid)?;
    let parts = DbaseParts {
        pool: &stack.pool,
        smgr: &stack.smgr,
        wal: &stack.wal,
        txn: cluster.txn_manager(),
    };
    log_drop(&parts, oid)?;
    remove_database_dir(&*stack.vfs, oid)
}

/// `discard_files` の失敗の扱い。Panic はそのまま返し、ほかは行が消えているので WARNING にする。
fn finish_discard(ctx: &mut DdlCtx<'_>, oid: Oid) -> Result<()> {
    match discard_files(ctx.cluster, oid) {
        Ok(()) => Ok(()),
        Err(e) if e.severity == Severity::Panic => Err(e),
        Err(e) => {
            ctx.notice(
                Severity::Warning,
                sqlstate::WARNING,
                format!(
                    "could not remove database directory \"base/{oid}\": {}",
                    e.message
                ),
                None,
            );
            Ok(())
        }
    }
}

/// `DROP DATABASE` を実行する。コマンドタグを返す。順序は D32（行の削除をコミット → ファイルの破棄）。
#[allow(
    clippy::needless_pass_by_value,
    reason = "execute_standalone の契約（BoundDdl の所有権を渡す）"
)]
pub fn drop_database(ctl: &mut dyn TxnControl, b: BoundDropDatabase) -> Result<String> {
    let plan = {
        let mut ctx = ctl.ddl_ctx()?;
        match prepare_drop(&mut ctx, &b)? {
            Some(p) => p,
            None => return Ok("DROP DATABASE".to_owned()),
        }
    };
    let oid = plan.info.row.oid;
    // 9. 行の削除（pg_database と yz_datxid）。
    {
        let mut ctx = ctl.ddl_ctx()?;
        let cluster = ctx.cluster;
        if ctx.txn.guard.is_none() {
            let (xid, guard) = cluster.txn_manager().assign_xid(ctx.backend)?;
            ctx.txn.xid = Some(xid);
            ctx.txn.guard = Some(guard);
        }
        let w = ctx.write_ctx()?;
        let snap = cluster
            .txn_manager()
            .take_snapshot(ctx.txn.xid, ctx.txn.cid);
        ctx.db.shared.delete_database(&w, &snap, oid)?;
        if cluster.knobs().drop_dir_before_commit {
            // 変異試験用: D32 を破る。
            finish_discard(&mut ctx, oid)?;
        }
    }
    // 10. コミット。ここで DB は論理的に消える。
    ctl.commit_and_restart()?;
    // 11. ファイルの破棄（コミット済み）。
    let mut ctx = ctl.ddl_ctx()?;
    if !ctx.cluster.knobs().drop_dir_before_commit {
        crate::storage::buffer::assert_no_pins();
        finish_discard(&mut ctx, oid)?;
    }
    // 12. 接続のキャッシュから外す。
    ctx.cluster.forget_database(oid);
    drop(ctx);
    // 13. ロックを解放する（lock → name_lock の順）。
    drop(plan);
    Ok("DROP DATABASE".to_owned())
}

/// `ALTER DATABASE`（通常のトランザクションの中。ブロック内でも可）。
#[allow(
    clippy::needless_pass_by_value,
    reason = "ddl::execute の契約（BoundDdl の所有権を渡す）"
)]
pub fn alter_database(ctx: &mut DdlCtx<'_>, b: BoundAlterDatabase) -> Result<String> {
    let cluster = ctx.cluster;
    // 1. CREATE / DROP / ALTER と直列にする。コミットかアボートで外れる。
    cluster.locks().acquire(
        ctx.backend,
        name_lock_tag(&b.name),
        LockMode::Exclusive,
        LockScope::Transaction,
        ctx.wait,
    )?;
    // 2. ロックの後の新しいスナップショット（このトランザクション自身の変更も見える）。
    let snap = cluster
        .txn_manager()
        .take_snapshot(ctx.txn.xid, ctx.txn.cid);
    let info = ctx
        .db
        .shared
        .database_info_by_name(&snap, &b.name)?
        .ok_or_else(|| database_missing(&b.name))?;
    // 3. 権限。
    if !(ctx.role.superuser || info.owner == ctx.role.oid) {
        return Err(must_be_owner(&b.name));
    }
    // 4. MVCC の更新。
    if ctx.txn.guard.is_none() {
        let (xid, guard) = cluster.txn_manager().assign_xid(ctx.backend)?;
        ctx.txn.xid = Some(xid);
        ctx.txn.guard = Some(guard);
    }
    let w = ctx.write_ctx()?;
    ctx.db
        .shared
        .update_database_flags(&w, &snap, info.row.oid, &b.flags)?;
    ctx.txn.catalog_dirty = true;
    Ok("ALTER DATABASE".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendId;
    use crate::bootstrap::{InitdbOptions, initdb};
    use crate::catalog::reader::StatementCatalog;
    use crate::catalog::store::RoleRow;
    use crate::ddl::role::PasswordPolicy;
    use crate::debug_knobs::DebugKnobs;
    use crate::engine::{Cluster, ClusterOptions, DatabaseHandle};
    use crate::executor::SessionInfo;
    use crate::interrupt::InterruptFlag;
    use crate::session::Notice;
    use crate::storage::vfs::{CrashMode, SimVfs};
    use crate::testing::{TestClusterOptions, cluster_options};
    use crate::txn::Transaction;
    use crate::txn::lock::{LockScope, WaitCtl};
    use crate::types::TypeEnv;
    use std::path::Path;
    use std::time::Duration;

    /// 実際にコミットする `TxnControl`（`commit_and_restart` の直後のディスクの像を残す）。
    struct Ctl {
        vfs: SimVfs,
        opts: ClusterOptions,
        cluster: Arc<Cluster>,
        db: Arc<DatabaseHandle>,
        role_row: RoleRow,
        info: SessionInfo,
        txn: Transaction,
        notices: Vec<Notice>,
        intr: InterruptFlag,
        session_id: u64,
        image_at_commit: Option<SimVfs>,
        /// 真なら、コミットの直後に「クラッシュ」して文を打ち切る（元のディスクはもう使えない）。
        crash_after_commit: bool,
    }

    fn options(knobs: DebugKnobs, busy: Duration) -> ClusterOptions {
        ClusterOptions {
            database_busy_wait: busy,
            ..cluster_options(&TestClusterOptions {
                knobs,
                ..TestClusterOptions::default()
            })
        }
    }

    impl Ctl {
        fn new(knobs: DebugKnobs, busy: Duration) -> Ctl {
            let vfs = SimVfs::new(1);
            initdb(
                Arc::new(vfs.clone()),
                &InitdbOptions {
                    superuser: "postgres".into(),
                    ..InitdbOptions::default()
                },
            )
            .unwrap();
            Ctl::open(vfs, options(knobs, busy))
        }

        fn open(vfs: SimVfs, opts: ClusterOptions) -> Ctl {
            let cluster = Cluster::open(Arc::new(vfs.clone()), opts.clone()).unwrap();
            let (db, role) = cluster.connect_simple("postgres", "postgres").unwrap();
            let session_id = cluster.next_session_id();
            cluster.locks().register_backend(BackendId(session_id), 1);
            Ctl {
                vfs,
                opts,
                cluster,
                db,
                info: SessionInfo {
                    current_user: role.name.clone(),
                    session_user: role.name.clone(),
                    database: "postgres".to_owned(),
                    current_schema: Some("public".to_owned()),
                },
                role_row: role,
                txn: Transaction::new(),
                notices: Vec::new(),
                intr: InterruptFlag::default(),
                session_id,
                image_at_commit: None,
                crash_after_commit: false,
            }
        }

        fn me(&self) -> BackendId {
            BackendId(self.session_id)
        }

        fn run(&mut self, b: BoundCreateDatabase) -> Result<String> {
            create_database(self, b)
        }

        fn info_of(&self, name: &str) -> Option<DatabaseInfo> {
            let snap = self
                .cluster
                .txn_manager()
                .take_snapshot(None, FIRST_COMMAND_ID);
            self.db.shared.database_info_by_name(&snap, name).unwrap()
        }

        fn datxid(&self, oid: Oid) -> Option<Xid> {
            let snap = self
                .cluster
                .txn_manager()
                .take_snapshot(None, FIRST_COMMAND_ID);
            self.db.shared.datxid(&snap, oid).unwrap()
        }

        fn files(vfs: &SimVfs, db: Oid) -> Vec<(String, Vec<u8>)> {
            use crate::storage::vfs::OpenMode;
            let mut v: Vec<_> = vfs
                .read_dir(&crate::dbase::database_dir(db))
                .unwrap()
                .iter()
                .map(|p| {
                    let f = vfs.open(p, OpenMode::ReadOnly).unwrap();
                    let mut buf = vec![0u8; usize::try_from(f.size().unwrap()).unwrap()];
                    f.read_exact_at(&mut buf, 0).unwrap();
                    (p.file_name().unwrap().to_string_lossy().into_owned(), buf)
                })
                .collect();
            v.sort();
            v
        }
    }

    impl TxnControl for Ctl {
        fn ddl_ctx(&mut self) -> Result<DdlCtx<'_>> {
            let snap = Box::leak(Box::new(
                self.cluster
                    .txn_manager()
                    .snapshot(self.txn.xid, self.txn.cid),
            ));
            let search_path = Box::leak(Box::new(vec!["public".to_owned()]));
            let catalog = Box::leak(Box::new(StatementCatalog {
                db: &self.db,
                snapshot: snap,
                gen_at_snapshot: self.db.cache.generation(),
                bypass_cache: true,
                search_path,
            }));
            let wait = Box::leak(Box::new(WaitCtl {
                lock_timeout: None,
                deadlock_timeout: Duration::from_secs(1),
                interrupts: &self.intr,
            }));
            let env = Box::leak(Box::new(TypeEnv::default()));
            Ok(DdlCtx {
                cluster: &self.cluster,
                db: &self.db,
                snapshot: snap,
                catalog,
                txn: &mut self.txn,
                role_oid: self.role_row.oid,
                notices: &mut self.notices,
                type_env: env,
                in_transaction_block: false,
                interrupts: &self.intr,
                backend: BackendId(self.session_id),
                wait,
                session: &self.info,
                role: &self.role_row,
                password: PasswordPolicy::default(),
                outside_block: true,
            })
        }

        fn commit_and_restart(&mut self) -> Result<()> {
            let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
            if let Some(xid) = txn.xid {
                self.cluster
                    .txn_manager()
                    .commit(xid, &txn.pending_unlinks)?;
            }
            txn.guard = None;
            self.cluster
                .locks()
                .release_all(self.me(), LockScope::Transaction);
            if self.crash_after_commit {
                self.image_at_commit = Some(self.vfs.crash(CrashMode::DropUnsynced));
                return Err(Error::new(
                    sqlstate::IO_ERROR,
                    "simulated crash after the commit",
                ));
            }
            Ok(())
        }
    }

    fn bound(name: &str, template: &str) -> BoundCreateDatabase {
        BoundCreateDatabase {
            name: name.to_owned(),
            template: template.to_owned(),
            owner: None,
            conn_limit: -1,
            is_template: false,
            allow_conn: true,
        }
    }

    fn quick() -> Ctl {
        Ctl::new(DebugKnobs::default(), Duration::from_millis(150))
    }

    fn code(r: Result<String>) -> String {
        r.unwrap_err().sqlstate.code().to_owned()
    }

    #[test]
    fn creates_a_copy_of_template1() {
        let mut c = quick();
        let mut b = bound("m5db_a", "template1");
        b.conn_limit = 5;
        b.allow_conn = false;
        assert_eq!(c.run(b).unwrap(), "CREATE DATABASE");
        let new = c.info_of("m5db_a").expect("the row exists");
        let tpl = c.info_of("template1").unwrap();
        assert!(new.row.oid > tpl.row.oid && new.row.oid >= 16384);
        assert_eq!(new.owner, c.role_row.oid);
        assert_eq!(
            (new.conn_limit, new.row.allow_conn, new.row.is_template),
            (5, false, false)
        );
        assert_eq!(new.min_mxid, tpl.min_mxid);
        assert_eq!(c.datxid(new.row.oid), c.datxid(tpl.row.oid));
        assert_eq!(
            Ctl::files(&c.vfs, new.row.oid),
            Ctl::files(&c.vfs, tpl.row.oid)
        );
        // ロックは文の最後に外れている。
        let locks = c.cluster.locks();
        assert!(!locks.holds(
            c.me(),
            LockTag::Database(new.row.oid),
            LockMode::AccessExclusive
        ));
        assert!(!locks.holds(c.me(), LockTag::Database(1), LockMode::AccessExclusive));
        assert!(!locks.holds(c.me(), name_lock_tag("m5db_a"), LockMode::Exclusive));
        // ALLOW_CONNECTIONS false は接続時に効き、true の DB には接続できる。
        let e = c.cluster.connect_simple("m5db_a", "postgres").unwrap_err();
        assert_eq!(e.sqlstate.code(), "55000");
        c.run(bound("m5db_c", "template1")).unwrap();
        c.cluster.connect_simple("m5db_c", "postgres").unwrap();
    }

    #[test]
    fn template0_gives_a_real_frozen_boundary_not_the_sentinel() {
        let mut c = quick();
        c.run(bound("m5db_b", "template0")).unwrap();
        let oid = c.info_of("m5db_b").unwrap().row.oid;
        let v = c.datxid(oid).unwrap();
        assert!(v >= Xid::FIRST_NORMAL);
        assert!(v <= c.cluster.txn_manager().oldest_xmin());
        let row = c.info_of("m5db_b").unwrap();
        assert_eq!(u64::from(row.row.oid), u64::from(oid));
    }

    #[test]
    fn rejects_duplicates_and_missing_objects() {
        let mut c = quick();
        assert_eq!(code(c.run(bound("postgres", "template1"))), "42P04");
        assert_eq!(code(c.run(bound("m5db_x", "nonexistent"))), "3D000");
        let mut b = bound("m5db_x", "template1");
        b.owner = Some("nobody".to_owned());
        assert_eq!(code(c.run(b)), "42704");
        assert!(c.info_of("m5db_x").is_none());
        // 失敗した文はロックを残さない。
        assert!(
            !c.cluster
                .locks()
                .holds(c.me(), name_lock_tag("m5db_x"), LockMode::Exclusive)
        );
    }

    #[test]
    fn checks_the_callers_privileges() {
        let mut c = quick();
        c.role_row = RoleRow {
            oid: 99_999,
            name: "plain".into(),
            can_login: true,
            superuser: false,
        };
        assert_eq!(code(c.run(bound("m5db_x", "template1"))), "42501");
        let mut b = bound("m5db_x", "template1");
        b.owner = Some("postgres".into());
        let e = c.run(b).unwrap_err();
        assert_eq!(e.message, "must be able to SET ROLE \"postgres\"");
        assert!(c.info_of("m5db_x").is_none());
    }

    #[test]
    fn a_template_in_use_is_refused_with_55006() {
        let mut c = quick();
        let other = BackendId(9_999);
        c.cluster.locks().register_backend(other, 2);
        assert!(c.cluster.locks().try_acquire(
            other,
            LockTag::Database(1),
            LockMode::AccessShare,
            LockScope::Session
        ));
        let e = c.run(bound("m5db_x", "template1")).unwrap_err();
        assert_eq!(e.sqlstate.code(), "55006");
        assert_eq!(
            e.message,
            "source database \"template1\" is being accessed by other users"
        );
        assert!(c.info_of("m5db_x").is_none());
        assert!(
            !c.cluster
                .locks()
                .holds(c.me(), name_lock_tag("m5db_x"), LockMode::Exclusive)
        );
        // 相手が居なくなれば成功する。
        c.cluster.locks().release_all(other, LockScope::Session);
        c.run(bound("m5db_x", "template1")).unwrap();
    }

    #[test]
    fn a_cancel_during_the_copy_leaves_nothing_behind() {
        let mut c = quick();
        c.intr.request_cancel();
        assert_eq!(code(c.run(bound("m5db_x", "template1"))), "57014");
        assert!(c.info_of("m5db_x").is_none());
        let dirs = crate::dbase::list_database_dirs(&c.vfs).unwrap();
        assert!(dirs.iter().all(|d| *d < 16384), "{dirs:?}");
        c.intr.clear_cancel();
        c.run(bound("m5db_x", "template1")).unwrap();
        assert!(c.info_of("m5db_x").is_some());
    }

    #[test]
    fn redo_rebuilds_the_directory_after_a_crash_before_the_last_checkpoint() {
        let knobs = DebugKnobs {
            skip_copy_fsync: true,
            ..DebugKnobs::default()
        };
        let mut c = Ctl::new(knobs, Duration::from_millis(150));
        let tpl_files = Ctl::files(&c.vfs, 1);
        c.crash_after_commit = true;
        c.run(bound("m5db_a", "template1")).unwrap_err();
        // コミットの直後（最後のチェックポイントの前）のディスク。コピーは fsync されていない。
        let image = c.image_at_commit.take().unwrap();
        let restarted = Ctl::open(image, options(knobs, Duration::from_millis(150)));
        let row = restarted
            .info_of("m5db_a")
            .expect("the committed row survives");
        let oid = row.row.oid;
        assert_eq!(Ctl::files(&restarted.vfs, oid), tpl_files);
        assert!(restarted.datxid(oid).is_some());
    }

    #[test]
    fn without_the_dbase_record_the_unsynced_copy_is_lost() {
        let knobs = DebugKnobs {
            skip_copy_fsync: true,
            skip_dbase_wal: true,
            ..DebugKnobs::default()
        };
        let mut c = Ctl::new(knobs, Duration::from_millis(150));
        let tpl_files = Ctl::files(&c.vfs, 1);
        c.crash_after_commit = true;
        c.run(bound("m5db_a", "template1")).unwrap_err();
        let image = c.image_at_commit.take().unwrap();
        let restarted = Ctl::open(image, options(knobs, Duration::from_millis(150)));
        let oid = restarted.info_of("m5db_a").unwrap().row.oid;
        // 変異が見える: 行はあるのに、ディレクトリが無いか不完全。
        let dir = crate::dbase::database_dir(oid);
        assert!(
            !restarted.vfs.exists(&dir).unwrap() || Ctl::files(&restarted.vfs, oid) != tpl_files,
            "the mutation must be visible"
        );
    }

    #[test]
    fn the_pre_copy_checkpoint_can_be_skipped() {
        let knobs = DebugKnobs {
            skip_pre_copy_checkpoint: true,
            ..DebugKnobs::default()
        };
        let mut c = Ctl::new(knobs, Duration::from_millis(150));
        c.run(bound("m5db_a", "template1")).unwrap();
        assert!(c.info_of("m5db_a").is_some());
        let _ = (&c.opts, Path::new(""));
    }

    fn drop_b(name: &str, if_exists: bool) -> BoundDropDatabase {
        BoundDropDatabase {
            name: name.to_owned(),
            if_exists,
        }
    }

    fn alter(c: &mut Ctl, name: &str, flags: DatabaseFlagUpdate) -> Result<String> {
        let r = {
            let mut ctx = c.ddl_ctx()?;
            alter_database(
                &mut ctx,
                BoundAlterDatabase {
                    name: name.to_owned(),
                    flags,
                },
            )
        };
        if r.is_ok() {
            c.commit_and_restart()?;
        }
        r
    }

    fn plain_role() -> RoleRow {
        RoleRow {
            oid: 99_999,
            name: "plain".into(),
            can_login: true,
            superuser: false,
        }
    }

    #[test]
    fn drop_removes_rows_directory_and_locks() {
        let mut c = quick();
        c.run(bound("m5db_d", "template1")).unwrap();
        let oid = c.info_of("m5db_d").unwrap().row.oid;
        let dir = crate::dbase::database_dir(oid);
        assert!(c.vfs.exists(&dir).unwrap());
        assert_eq!(
            drop_database(&mut c, drop_b("m5db_d", false)).unwrap(),
            "DROP DATABASE"
        );
        assert!(c.info_of("m5db_d").is_none());
        assert!(c.datxid(oid).is_none());
        assert!(!c.vfs.exists(&dir).unwrap());
        let locks = c.cluster.locks();
        assert!(!locks.holds(c.me(), LockTag::Database(oid), LockMode::AccessExclusive));
        assert!(!locks.holds(c.me(), name_lock_tag("m5db_d"), LockMode::Exclusive));
        // 同じ名前で作り直せる。
        c.run(bound("m5db_d", "template1")).unwrap();
        assert!(c.info_of("m5db_d").is_some());
    }

    #[test]
    fn drop_of_a_missing_database() {
        let mut c = quick();
        assert_eq!(code(drop_database(&mut c, drop_b("nope", false))), "3D000");
        assert_eq!(
            drop_database(&mut c, drop_b("nope", true)).unwrap(),
            "DROP DATABASE"
        );
        let n = c.notices.last().expect("a notice");
        assert_eq!(n.message, "database \"nope\" does not exist, skipping");
        assert_eq!(n.severity, Severity::Notice);
        assert!(
            !c.cluster
                .locks()
                .holds(c.me(), name_lock_tag("nope"), LockMode::Exclusive)
        );
    }

    #[test]
    fn drop_refuses_templates_the_current_database_and_non_owners() {
        let mut c = quick();
        assert_eq!(
            code(drop_database(&mut c, drop_b("template1", false))),
            "42809"
        );
        assert_eq!(
            code(drop_database(&mut c, drop_b("template0", false))),
            "42809"
        );
        let e = drop_database(&mut c, drop_b("postgres", false)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "55006");
        assert_eq!(e.message, "cannot drop the currently open database");
        c.run(bound("m5db_o", "template1")).unwrap();
        let admin = c.role_row.clone();
        c.role_row = plain_role();
        let e = drop_database(&mut c, drop_b("m5db_o", false)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42501");
        assert_eq!(e.message, "must be owner of database m5db_o");
        c.role_row = admin;
        assert!(c.info_of("m5db_o").is_some());
    }

    #[test]
    fn drop_of_a_database_in_use_is_55006_and_leaves_it_intact() {
        let mut c = quick();
        c.run(bound("m5db_u", "template1")).unwrap();
        let oid = c.info_of("m5db_u").unwrap().row.oid;
        let other = BackendId(9_998);
        c.cluster.locks().register_backend(other, 2);
        assert!(c.cluster.locks().try_acquire(
            other,
            LockTag::Database(oid),
            LockMode::AccessShare,
            LockScope::Session
        ));
        let e = drop_database(&mut c, drop_b("m5db_u", false)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "55006");
        assert_eq!(
            e.message,
            "database \"m5db_u\" is being accessed by other users"
        );
        assert!(c.info_of("m5db_u").is_some());
        assert!(c.vfs.exists(&crate::dbase::database_dir(oid)).unwrap());
        assert!(
            !c.cluster
                .locks()
                .holds(c.me(), name_lock_tag("m5db_u"), LockMode::Exclusive)
        );
        c.cluster.locks().release_all(other, LockScope::Session);
        drop_database(&mut c, drop_b("m5db_u", false)).unwrap();
        assert!(c.info_of("m5db_u").is_none());
    }

    #[test]
    fn a_crash_right_after_the_commit_leaves_an_orphan_directory_only() {
        let mut c = quick();
        c.run(bound("m5db_k", "template1")).unwrap();
        let oid = c.info_of("m5db_k").unwrap().row.oid;
        c.crash_after_commit = true;
        drop_database(&mut c, drop_b("m5db_k", false)).unwrap_err();
        let image = c.image_at_commit.take().unwrap();
        let restarted = Ctl::open(
            image,
            options(DebugKnobs::default(), Duration::from_millis(150)),
        );
        assert!(restarted.info_of("m5db_k").is_none());
        assert!(restarted.datxid(oid).is_none());
        assert!(
            restarted
                .vfs
                .exists(&crate::dbase::database_dir(oid))
                .unwrap()
        );
    }

    #[test]
    fn drop_dir_before_commit_knob_removes_the_files_first() {
        let knobs = DebugKnobs {
            drop_dir_before_commit: true,
            ..DebugKnobs::default()
        };
        let mut c = Ctl::new(knobs, Duration::from_millis(150));
        c.run(bound("m5db_m", "template1")).unwrap();
        let oid = c.info_of("m5db_m").unwrap().row.oid;
        c.crash_after_commit = true;
        drop_database(&mut c, drop_b("m5db_m", false)).unwrap_err();
        // コミット時点では既にディレクトリが無い（D32 を破っている）。
        assert!(!c.vfs.exists(&crate::dbase::database_dir(oid)).unwrap());
    }

    #[test]
    fn alter_updates_the_three_flags_and_template_can_be_dropped_afterwards() {
        let mut c = quick();
        c.run(bound("m5db_t", "template1")).unwrap();
        let flags = DatabaseFlagUpdate {
            allow_conn: Some(false),
            is_template: Some(true),
            conn_limit: Some(7),
        };
        assert_eq!(alter(&mut c, "m5db_t", flags).unwrap(), "ALTER DATABASE");
        let i = c.info_of("m5db_t").unwrap();
        assert_eq!(
            (i.row.allow_conn, i.row.is_template, i.conn_limit),
            (false, true, 7)
        );
        assert_eq!(
            code(drop_database(&mut c, drop_b("m5db_t", false))),
            "42809"
        );
        // 指定しなかった項目は変わらない。
        let only = DatabaseFlagUpdate {
            is_template: Some(false),
            ..DatabaseFlagUpdate::default()
        };
        alter(&mut c, "m5db_t", only).unwrap();
        let i = c.info_of("m5db_t").unwrap();
        assert_eq!(
            (i.row.allow_conn, i.row.is_template, i.conn_limit),
            (false, false, 7)
        );
        drop_database(&mut c, drop_b("m5db_t", false)).unwrap();
        assert!(c.info_of("m5db_t").is_none());
        assert!(
            !c.cluster
                .locks()
                .holds(c.me(), name_lock_tag("m5db_t"), LockMode::Exclusive)
        );
    }

    #[test]
    fn alter_checks_existence_and_ownership() {
        let mut c = quick();
        let f = DatabaseFlagUpdate {
            conn_limit: Some(1),
            ..DatabaseFlagUpdate::default()
        };
        assert_eq!(code(alter(&mut c, "nope", f)), "3D000");
        let admin = c.role_row.clone();
        c.role_row = plain_role();
        let e = alter(&mut c, "template1", f).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42501");
        assert_eq!(e.message, "must be owner of database template1");
        c.role_row = admin;
        // 失敗した文のロックはトランザクションの終わりに外れる。
        c.cluster
            .locks()
            .release_all(c.me(), LockScope::Transaction);
        assert_eq!(c.info_of("template1").unwrap().conn_limit, -1);
    }

    #[test]
    fn alter_on_the_current_database_twice_in_one_transaction() {
        let mut c = quick();
        let a = DatabaseFlagUpdate {
            conn_limit: Some(3),
            ..DatabaseFlagUpdate::default()
        };
        let b = DatabaseFlagUpdate {
            conn_limit: Some(4),
            allow_conn: Some(true),
            ..DatabaseFlagUpdate::default()
        };
        {
            let mut ctx = c.ddl_ctx().unwrap();
            for f in [a, b] {
                alter_database(
                    &mut ctx,
                    BoundAlterDatabase {
                        name: "postgres".into(),
                        flags: f,
                    },
                )
                .unwrap();
                ctx.txn.command_counter_increment().unwrap();
            }
        }
        c.commit_and_restart().unwrap();
        assert_eq!(c.info_of("postgres").unwrap().conn_limit, 4);
    }
}
