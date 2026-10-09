//! 文ごとのロック取得・スナップショット・文の実行環境（LK が持つ）。

#![allow(clippy::wildcard_imports)]
// `mod locking` は非公開なので pub は到達不能に見える。結線（exec_data_statement・Bind。LK-4c・XQ）までは未使用。
#![allow(unreachable_pub, dead_code)]

use super::*;

impl Session {
    /// Steps 3 to 6 of `m2.md` §5.2.
    pub(super) fn run_under_barrier(
        &mut self,
        stmt: &Statement,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        read_only_write: Option<&'static str>,
        out: &mut Output,
    ) -> Result<ExecOutcome> {
        let span = stmt.span();
        self.with_stmt_env(cluster, db, |txn, env| {
            Self::run_bound(txn, env, stmt, span, read_only_write, out)
        })
    }

    /// Builds the per-statement environment (snapshot, catalog, `TypeEnv`,
    /// planner settings, the `RuntimeInfo`) and calls `f` with it and the
    /// transaction. What `set_config` changed in `f` is kept when it succeeds.
    pub(super) fn with_stmt_env<R>(
        &mut self,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        f: impl FnOnce(&mut Transaction, &StmtEnv<'_>) -> Result<R>,
    ) -> Result<R> {
        // 3. The generation is read before the snapshot.
        let generation = db.cache.generation();
        // 4.
        // Any statement but SET / SHOW / BEGIN / COMMIT / CHECKPOINT counts as
        // having used a snapshot (PostgreSQL 17, also for `SELECT 1`).
        self.txn_snapshot_taken = true;
        let snap = cluster.txn_manager().snapshot(self.txn.xid, self.txn.cid);
        // 5.
        let search_path = self.settings.search_path();
        let catalog = StatementCatalog {
            db,
            snapshot: &snap,
            gen_at_snapshot: generation,
            bypass_cache: self.txn.catalog_dirty,
            search_path: &search_path,
        };
        let info = self.session_info();
        let interrupts = Arc::clone(&self.interrupt);
        let zones = cluster.zones();
        let datetime = self.settings.datetime_settings(zones);
        let names = CatalogNames(&catalog);
        let type_env = self
            .settings
            .type_env(&datetime, zones, self.txn.started_at, Some(&names));
        let planner_settings = self.settings.planner_settings();
        let runtime = SessionRuntime {
            pid: self.backend_pid(),
            mgr: Some(Arc::clone(cluster.txn_manager())),
            interrupts: Arc::clone(&interrupts),
            xid: self.txn.xid.map(|x| x.0),
            stmt_started_at: self.stmt_started_at,
            base: &self.settings,
            changed: RefCell::new(None),
            chars: self.chars,
            seq: &self.seq_state,
            seq_store: &**cluster.sequences(),
            catalog: &catalog,
            locks: Some(Arc::clone(cluster.locks())),
            backend: BackendId(self.id),
            db_oid: db.oid,
        };
        let wait = lock::WaitCtl {
            lock_timeout: self.settings.lock_timeout(),
            deadlock_timeout: self.settings.deadlock_timeout(),
            interrupts: &interrupts,
        };
        let env = StmtEnv {
            cluster,
            db,
            catalog: &catalog,
            snapshot: &snap,
            info: &info,
            runtime: &runtime,
            type_env: &type_env,
            interrupts: &interrupts,
            planner: &planner_settings,
            role_oid: self.role_oid,
            in_transaction_block: self.in_block_for_set(),
            backend: BackendId(self.id),
            wait: &wait,
            role: &self.role,
            password: self.settings.password_policy(),
        };
        let r = f(&mut self.txn, &env)?;
        let changed = runtime.changed.into_inner();
        if let Some(s) = changed {
            // Keep what `set_config` changed.
            self.settings = s;
        }
        Ok(r)
    }
}

/// Microseconds since 2000-01-01 (PostgreSQL's epoch), for `Transaction.started_at`.
pub(super) fn pg_now_micros() -> i64 {
    const UNIX_TO_PG_SECS: i64 = 946_684_800;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .map_or(0, |us| us - UNIX_TO_PG_SECS * 1_000_000)
}

/// Everything a statement needs besides the transaction (`m4/02` §4.1).
pub(super) struct StmtEnv<'a> {
    pub(super) cluster: &'a Cluster,
    pub(super) db: &'a Arc<DatabaseHandle>,
    pub(super) catalog: &'a dyn CatalogReader,
    pub(super) snapshot: &'a Snapshot,
    pub(super) info: &'a SessionInfo,
    pub(super) runtime: &'a SessionRuntime<'a>,
    pub(super) type_env: &'a TypeEnv<'a>,
    pub(super) interrupts: &'a InterruptFlag,
    pub(super) planner: &'a PlannerSettings,
    pub(super) role_oid: Oid,
    pub(super) in_transaction_block: bool,
    // ----- M5（`DdlCtx` に渡す。F0b-2）-----
    pub(super) backend: BackendId,
    pub(super) wait: &'a lock::WaitCtl<'a>,
    pub(super) role: &'a RoleRow,
    pub(super) password: PasswordPolicy,
}

/// Rows (as text) and the affected-row count of one run.
pub(super) struct RunResult {
    pub(super) rows: Vec<Vec<Option<String>>>,
    pub(super) affected: u64,
}

impl StmtEnv<'_> {
    pub(super) fn fold_constants(&self, bound: &mut BoundStatement) -> Result<()> {
        planner::check_constant_exprs(
            bound,
            &executor::EvalCtx {
                session: self.info,
                catalog: self.catalog,
                runtime: self.runtime,
                type_env: self.type_env,
            },
        )
    }

    pub(super) fn plan(
        &self,
        bound: &BoundStatement,
        want_explain: bool,
        explain_verbose: bool,
    ) -> Result<PhysicalQuery> {
        let env = planner::PlanEnv {
            catalog: self.catalog,
            storage: &**self.cluster.storage(),
            settings: self.planner,
            type_env: self.type_env,
            want_explain,
            explain_verbose,
        };
        planner::plan(bound, &env)
    }

    pub(super) fn exec_env<'e>(
        &'e self,
        instr: Option<&'e Rc<Instrumentation>>,
    ) -> executor::ExecEnv<'e> {
        executor::ExecEnv {
            catalog: self.catalog,
            storage: &**self.cluster.storage(),
            indexes: &**self.cluster.indexes(),
            snapshot: self.snapshot,
            session: self.info,
            runtime: self.runtime,
            interrupts: self.interrupts,
            type_env: self.type_env,
            mem_limit: self.planner.query_mem_limit,
            instr,
        }
    }

    /// Runs `query` to the end. Rows are converted to text only when `returns_rows`.
    pub(super) fn run(
        &self,
        txn: &mut Transaction,
        query: &PhysicalQuery,
        returns_rows: bool,
        instr: Option<&Rc<Instrumentation>>,
    ) -> Result<RunResult> {
        // A leftover `unknown` is reported (and rendered) as text (PostgreSQL ≥ 10).
        let types: Vec<SqlType> = query
            .output
            .iter()
            .map(|c| {
                if c.ty.oid == oid::UNKNOWN {
                    SqlType::TEXT
                } else {
                    c.ty
                }
            })
            .collect();
        let mut exec = build_executor(query, instr);
        let mut ctx = ExecCtx::new(self.exec_env(instr), txn, query);
        let mut rows = Vec::new();
        while let Some(row) = exec.next(&mut ctx)? {
            if returns_rows {
                rows.push(row_text(&row, &types, self.type_env)?);
            }
        }
        Ok(RunResult {
            rows,
            affected: exec.rows_affected(),
        })
    }
}

/// Builds the executor tree; EXPLAIN ANALYZE wraps every node with its counters.
pub(super) fn build_executor(
    query: &PhysicalQuery,
    instr: Option<&Rc<Instrumentation>>,
) -> executor::BoxedExecutor {
    match instr {
        Some(i) => executor::instrument::build_instrumented(query, i),
        None => executor::build_query(query),
    }
}

// ----- TxnManager calls ----------------------------------------------------
// Every call into the transaction manager that `m3.md` §4.6 changes goes
// through these three functions.

/// Assigns an XID (no writer lock since M5; 01 LK-3b).
pub(super) fn mgr_begin_write(mgr: &Arc<TxnManager>, session_id: u64) -> Result<(Xid, XidGuard)> {
    mgr.assign_xid(BackendId(session_id))
}

pub(super) fn mgr_is_blocked_by(mgr: &TxnManager, session_id: u64, among: &[u64]) -> bool {
    let among: Vec<BackendId> = among.iter().map(|a| BackendId(*a)).collect();
    mgr.locks().is_blocked_by(BackendId(session_id), &among)
}

/// The `RuntimeInfo` a statement's `EvalCtx` gets (`m3.md` §6.11.6).
pub(super) struct SessionRuntime<'a> {
    pub(super) pid: i32,
    pub(super) mgr: Option<Arc<TxnManager>>,
    pub(super) interrupts: Arc<InterruptFlag>,
    pub(super) stmt_started_at: i64,
    pub(super) xid: Option<u64>,
    /// The session's parameters. `set_config` copies them into `changed` and
    /// the session takes that back when the statement succeeds.
    pub(super) base: &'a Settings,
    pub(super) changed: RefCell<Option<Settings>>,
    pub(super) chars: TxnCharacteristics,
    pub(super) seq: &'a RefCell<SeqSession>,
    pub(super) seq_store: &'a dyn SequenceStore,
    pub(super) catalog: &'a dyn CatalogReader,
    /// シーケンス関数の実行時ロック（LK-D18）。`None` ならロックしない。
    pub(super) locks: Option<Arc<LockManager>>,
    pub(super) backend: BackendId,
    pub(super) db_oid: Oid,
}

impl std::fmt::Debug for SessionRuntime<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuntime")
            .field("pid", &self.pid)
            .field("xid", &self.xid)
            .finish_non_exhaustive()
    }
}

impl SessionRuntime<'_> {
    /// LK-D18: `nextval` / `currval` / `setval` の先頭でシーケンスを `RowExclusive`
    /// （トランザクションスコープ）で取る。保持済みなら即座に戻る。
    ///
    /// 式の評価中は共有ストレージバリアを持っており、規約 1 により `acquire`（待ちうる呼び出し）は
    /// 使えないので `try_acquire` を使う。競合する保持者（`ALTER` / `DROP SEQUENCE` の未完了）がいるときは
    /// 待たずに 55P03 を返す（既知の制限。待つには文の前にシーケンスを取る必要がある）。
    fn lock_sequence(&self, seq: Oid) -> Result<()> {
        let Some(locks) = &self.locks else {
            return Ok(());
        };
        let tag = LockTag::Relation {
            db: self.db_oid,
            rel: seq,
        };
        if locks.try_acquire(
            self.backend,
            tag,
            LockMode::RowExclusive,
            LockScope::Transaction,
        ) {
            return Ok(());
        }
        Err(Error::new(
            sqlstate::LOCK_NOT_AVAILABLE,
            format!("could not obtain lock on sequence with OID {seq}"),
        ))
    }

    pub(super) fn seq_runtime(&self) -> SeqRuntime<'_> {
        SeqRuntime {
            state: self.seq,
            store: self.seq_store,
            catalog: self.catalog,
            read_only: self.chars.read_only,
        }
    }
}

impl RuntimeInfo for SessionRuntime<'_> {
    fn backend_pid(&self) -> i32 {
        self.pid
    }

    fn is_blocked_by(&self, pid: i32, among: &[i32]) -> bool {
        let Some(mgr) = &self.mgr else {
            return false;
        };
        let to_id = |p: &i32| u64::try_from(*p).ok();
        let (Some(id), among) = (
            to_id(&pid),
            among.iter().filter_map(to_id).collect::<Vec<_>>(),
        ) else {
            return false;
        };
        mgr_is_blocked_by(mgr, id, &among)
    }

    fn check_interrupts(&self) -> Result<()> {
        self.interrupts.check()
    }

    fn current_xid(&self) -> Option<u64> {
        self.xid
    }

    fn statement_timestamp(&self) -> Option<i64> {
        (self.stmt_started_at != 0).then_some(self.stmt_started_at)
    }

    fn get_setting(&self, name: &str) -> Result<Option<String>> {
        match name.to_ascii_lowercase().as_str() {
            "transaction_isolation" => return Ok(Some(self.chars.isolation.as_str().to_owned())),
            "transaction_read_only" => return Ok(Some(on_off(self.chars.read_only).to_owned())),
            "transaction_deferrable" => return Ok(Some(on_off(self.chars.deferrable).to_owned())),
            _ => {}
        }
        let changed = self.changed.borrow();
        let settings = changed.as_ref().unwrap_or(self.base);
        match settings.show(name) {
            Ok((_, v)) => Ok(Some(v)),
            Err(e) if e.sqlstate == sqlstate::UNDEFINED_OBJECT => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String> {
        if is_characteristic(name) {
            return Err(Error::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format!("set_config of \"{name}\" is not supported; use SET TRANSACTION"),
            ));
        }
        let mut changed = self.changed.borrow_mut();
        let settings = changed.get_or_insert_with(|| self.base.clone());
        let args = value.map(|v| [v.to_owned()]);
        // is_local lasts until the end of the (implicit) transaction, which
        // `Settings::commit` / `rollback` take care of.
        settings.set(name, args.as_ref().map(<[String; 1]>::as_slice), local)?;
        Ok(settings.get(name).to_owned())
    }

    fn nextval(&self, seq: Oid) -> Result<i64> {
        self.lock_sequence(seq)?;
        self.seq_runtime().nextval(seq)
    }

    fn currval(&self, seq: Oid) -> Result<i64> {
        self.lock_sequence(seq)?;
        self.seq_runtime().currval(seq)
    }

    fn lastval(&self) -> Result<i64> {
        self.seq_runtime().lastval()
    }

    fn setval(&self, seq: Oid, value: i64, is_called: bool) -> Result<i64> {
        self.lock_sequence(seq)?;
        self.seq_runtime().setval(seq, value, is_called)
    }
}

// ===== M5 LK-4a: 文ごとのリレーションロックの取得（`m5/01` §4.6・§5.9）=======================
//
// 呼び出す側（`exec_data_statement` と Extended Query の Bind。LK-4c・XQ・RW が結ぶ）の順序:
//   0'. Repeatable Read の最初のスナップショットは**ロックの前**（00 §5.1、LK-D27。RW の持ち物）
//   1.  `collect_lock_requests(stmt)`
//   2.  `acquire_statement_locks(&reqs)`（RC のスナップショットはこの後で取る）
//   3.  書く文なら XID の割り当て（ロックの後。LK-D24）
//   4.  `analyze` → `missing_locks(&bound)` が空でなければ `acquire_oids` してやり直す
//       （最大 `MAX_VERIFY_ATTEMPTS` 回。尽きたら `verification_exhausted()`）

use crate::analyzer::bound::BoundExpr;
#[allow(unused_imports)]
use crate::catalog::RelKind;
use crate::catalog::schema;
use crate::error::Span;
use crate::sql::ast;
use crate::txn::CommandId;
use crate::txn::lock::relation::{self as rel_lock, RelLockOpts};
use crate::txn::lock::{LockManager, LockMode, LockTag};

/// LK-D12 の検証の輪の上限（解析 → 足りないロックを取る → やり直し）。
pub(super) const MAX_VERIFY_ATTEMPTS: usize = 4;

/// 検証の輪が尽きたときのエラー。
pub(super) fn verification_exhausted() -> Error {
    Error::internal("could not lock all relations of the statement")
}

/// 生のパース木から作る 1 件のロック要求（§5.9）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockRequest {
    pub target: LockTarget,
    pub mode: LockMode,
    pub nowait: bool,
    pub lock_table: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockTarget {
    /// 既存のリレーション（表・シーケンス）。名前は検索パスで解決する。見つからなければ飛ばす（LK-D13）。
    Relation {
        schema: Option<String>,
        name: String,
        span: Span,
    },
    /// DROP INDEX: インデックスの名前を引き、持ち主の表をロックする。
    IndexOwner {
        schema: Option<String>,
        name: String,
        span: Span,
    },
    /// 名前予約ロック（CREATE が作る名前。mode は Exclusive）。
    NewName {
        schema: Option<String>,
        name: String,
    },
    /// 第 2 段（LK-D16）を取る印。直前の `Relation` / `IndexOwner` の解決結果に対して行う。
    /// `None`: DROP TABLE（その表の全 FK の相手と、列が所有するシーケンス）。
    /// `Some(name)`: `ALTER TABLE ... DROP CONSTRAINT name`（name が FK のときだけ、その相手）。
    DropPeers { constraint: Option<String> },
}

impl LockTarget {
    /// `span` を無視した同一判定（同じ要求の畳み込み用）。`DropPeers` は位置が意味を持つので常に別物。
    fn same_object(&self, other: &LockTarget) -> bool {
        match (self, other) {
            (
                LockTarget::Relation {
                    schema: s1,
                    name: n1,
                    ..
                },
                LockTarget::Relation {
                    schema: s2,
                    name: n2,
                    ..
                },
            )
            | (
                LockTarget::IndexOwner {
                    schema: s1,
                    name: n1,
                    ..
                },
                LockTarget::IndexOwner {
                    schema: s2,
                    name: n2,
                    ..
                },
            )
            | (
                LockTarget::NewName {
                    schema: s1,
                    name: n1,
                },
                LockTarget::NewName {
                    schema: s2,
                    name: n2,
                },
            ) => s1 == s2 && n1 == n2,
            _ => false,
        }
    }
}

/// 取れたロック。LK-D12 の検証と、準備済み文（D19・D21）の「参照リレーションの一覧」に使う。
#[derive(Clone, Debug, Default)]
pub struct LockSet {
    pub entries: Vec<(RelKey, LockMode)>,
}

impl LockSet {
    /// `key` について、`mode` を満たすモードを取れているか（`mode_covers`）。
    pub fn covers(&self, key: RelKey, mode: LockMode) -> bool {
        self.entries
            .iter()
            .any(|(k, m)| *k == key && rel_lock::mode_covers(*m, mode))
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = (RelKey, LockMode)>) {
        self.entries.extend(items);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelKey {
    /// 共有カタログは 0。
    pub db: Oid,
    pub rel: Oid,
}

impl RelKey {
    fn tag(self) -> LockTag {
        LockTag::Relation {
            db: self.db,
            rel: self.rel,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedRel {
    pub oid: Oid,
    pub kind: RelKind,
    pub shared: bool,
    pub schema: String,
    pub name: String,
}

/// 名前引きの窓口。`Session` の実装（[`SessionResolver`]）は「呼ぶたびに最新のカタログ用スナップショット
/// （自分の変更を含む。D12）を取り、キャッシュの世代を見て読み直す」。取ったスナップショットはこの呼び出しの
/// 間だけ登録する（LK-D9）。
pub trait RelationResolver {
    fn lookup(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>>;
    /// インデックスの名前から、持ち主の表を引く。インデックスでなければ `None`。
    fn index_owner(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>>;
    /// CREATE の名前空間。schema が明示されて存在しなければ 3F000 `schema "x" does not exist`。
    fn creation_namespace(&mut self, schema: Option<&str>) -> Result<Oid>;
    /// 第 2 段: `rel` が持つ FK の相手の表の OID（`constraint` が `Some` ならその制約が FK のときだけ）。
    /// `None` のときは、`rel` の列が所有するシーケンスの OID も含む。
    fn drop_peers(&mut self, rel: Oid, constraint: Option<&str>) -> Result<Vec<Oid>>;
    /// ロックを取る前に、解決のたびに 1 回呼ぶ（PG の `RangeVarGetRelidExtended` の callback と同じ位置）。
    /// 08 章 C5: `pg_authid`（OID 1260）はスーパーユーザー以外 42501。既定は何もしない。
    fn check_access(&mut self, _rel: &ResolvedRel, _mode: LockMode) -> Result<()> {
        Ok(())
    }
    /// カタログの世代（`DatabaseHandle.cache.generation()`）。
    fn generation(&self) -> u64;
}

// ----- A. collect_lock_requests ------------------------------------------------------------

/// 生のパース木だけから決まる純粋な関数（`Cluster` なしでテストできる）。解析順に並べ、同じ要求は
/// 1 つにまとめる（§5.9 A の表）。
///
/// 未対応（AST に無い）: `ALTER TABLE ... DROP CONSTRAINT x` の `DropPeers`（AST が制約名を持たない）。
pub fn collect_lock_requests(stmt: &Statement) -> Vec<LockRequest> {
    let mut c = Collector::default();
    c.statement(stmt);
    c.reqs
}

/// `FOR UPDATE` などの対象（PG の `transformLockingClause`）。対象の表は `RowShare`。
enum ForMode {
    No,
    /// `OF` なし: FROM の全表（副問い合わせの FROM の中も）。
    All,
    /// `OF a, b`: 別名（なければ表名）が一致する FROM の項目。
    Of(Vec<String>),
}

impl ForMode {
    fn of(clauses: &[ast::LockingClause]) -> ForMode {
        if clauses.is_empty() {
            return ForMode::No;
        }
        if clauses.iter().any(|c| c.of.is_empty()) {
            return ForMode::All;
        }
        ForMode::Of(
            clauses
                .iter()
                .flat_map(|c| c.of.iter().map(|n| n.name().value.clone()))
                .collect(),
        )
    }

    fn applies_to(&self, refname: &str) -> bool {
        match self {
            ForMode::No => false,
            ForMode::All => true,
            ForMode::Of(names) => names.iter().any(|n| n == refname),
        }
    }
}

#[derive(Default)]
struct Collector {
    reqs: Vec<LockRequest>,
    /// 見えている CTE の名前（FROM の同名の表より優先する）。
    ctes: Vec<String>,
}

fn schema_of(name: &ast::ObjectName) -> Option<String> {
    name.schema().map(|i| i.value.clone())
}

impl Collector {
    fn push(&mut self, target: LockTarget, mode: LockMode) {
        let dup = !matches!(target, LockTarget::DropPeers { .. })
            && self
                .reqs
                .iter()
                .any(|r| r.mode == mode && r.target.same_object(&target));
        if !dup {
            self.reqs.push(LockRequest {
                target,
                mode,
                nowait: false,
                lock_table: false,
            });
        }
    }

    fn relation(&mut self, name: &ast::ObjectName, mode: LockMode) {
        self.push(
            LockTarget::Relation {
                schema: schema_of(name),
                name: name.name().value.clone(),
                span: name.span,
            },
            mode,
        );
    }

    fn new_name(&mut self, schema: Option<String>, name: &str) {
        self.push(
            LockTarget::NewName {
                schema,
                name: name.to_owned(),
            },
            LockMode::Exclusive,
        );
    }

    #[allow(clippy::too_many_lines)]
    fn statement(&mut self, stmt: &Statement) {
        use LockMode::{AccessExclusive, AccessShare, RowExclusive, Share, ShareRowExclusive};
        match stmt {
            Statement::Query(q) => self.query(q),
            Statement::Insert(i) => {
                self.relation(&i.table, RowExclusive);
                if let ast::InsertSource::Query(q) = &i.source {
                    self.query(q);
                }
                self.select_items(&i.returning);
            }
            Statement::Update(u) => {
                self.relation(&u.table, RowExclusive);
                for t in &u.from {
                    self.table_ref(t, &ForMode::No);
                }
                for a in &u.assignments {
                    self.expr(&a.value);
                }
                self.opt_expr(u.selection.as_ref());
                self.select_items(&u.returning);
            }
            Statement::Delete(d) => {
                self.relation(&d.table, RowExclusive);
                for t in &d.using {
                    self.table_ref(t, &ForMode::No);
                }
                self.opt_expr(d.selection.as_ref());
                self.select_items(&d.returning);
            }
            Statement::Copy(c) => {
                let mode = match c.direction {
                    ast::CopyDirection::From => RowExclusive,
                    ast::CopyDirection::To => AccessShare,
                };
                self.relation(&c.table, mode);
                self.opt_expr(c.where_clause.as_ref());
            }
            Statement::Explain(e) => self.statement(&e.statement),
            Statement::CreateTable(ct) => self.create_table(ct),
            Statement::DropTable(d) => {
                for n in &d.names {
                    self.relation(n, AccessExclusive);
                    self.push(LockTarget::DropPeers { constraint: None }, AccessExclusive);
                }
            }
            Statement::Truncate(t) => {
                for n in &t.tables {
                    self.relation(n, AccessExclusive);
                }
            }
            Statement::CreateIndex(ci) => {
                self.relation(&ci.table, Share);
                if let Some(n) = &ci.name {
                    self.new_name(schema_of(&ci.table), &n.value);
                }
            }
            Statement::DropIndex(d) => {
                for n in &d.names {
                    self.push(
                        LockTarget::IndexOwner {
                            schema: schema_of(n),
                            name: n.name().value.clone(),
                            span: n.span,
                        },
                        AccessExclusive,
                    );
                }
            }
            Statement::CreateSequence(s) => self.new_name(schema_of(&s.name), &s.name.name().value),
            Statement::AlterSequence(s) => self.relation(&s.name, ShareRowExclusive),
            Statement::DropSequence(d) => {
                for n in &d.names {
                    self.relation(n, AccessExclusive);
                }
            }
            Statement::AlterTable(a) => self.alter_table(a),
            // 取らない（D41）: VACUUM / ANALYZE、ロール・データベースの DDL、制御系、PREPARE / EXECUTE 系。
            Statement::LockTable(l) => {
                let mode = crate::analyzer::ddl_ext::lock_table::lock_mode(l.mode);
                for t in &l.tables {
                    let before = self.reqs.len();
                    self.relation(&t.name, mode);
                    if self.reqs.len() > before {
                        let r = self.reqs.last_mut().expect("just pushed");
                        r.nowait = l.nowait;
                        r.lock_table = true;
                    }
                }
            }
            Statement::Transaction(_)
            | Statement::Set(_)
            | Statement::Reset(_)
            | Statement::Show(_)
            | Statement::Checkpoint(_)
            | Statement::Vacuum(_)
            | Statement::Analyze(_)
            | Statement::Prepare(_)
            | Statement::ExecutePrepared(_)
            | Statement::Deallocate(_)
            | Statement::Discard(_)
            | Statement::CreateRole(_)
            | Statement::AlterRole(_)
            | Statement::DropRole(_)
            | Statement::CreateDatabase(_)
            | Statement::DropDatabase(_)
            | Statement::AlterDatabase(_) => {}
        }
    }

    fn create_table(&mut self, ct: &ast::CreateTable) {
        let schema = schema_of(&ct.name);
        let table = ct.name.name().value.clone();
        self.new_name(schema.clone(), &table);
        // 明示された PRIMARY KEY / UNIQUE の制約名は索引の名前になる。
        for el in &ct.elements {
            match el {
                ast::TableElement::Column(col) => {
                    for cc in &col.constraints {
                        if let Some(n) = &cc.name
                            && matches!(
                                cc.kind,
                                ast::ColumnConstraintKind::PrimaryKey(_)
                                    | ast::ColumnConstraintKind::Unique(_)
                            )
                        {
                            self.new_name(schema.clone(), &n.value);
                        }
                    }
                }
                ast::TableElement::Constraint(tc) => {
                    if let Some(n) = &tc.name
                        && matches!(
                            tc.kind,
                            ast::TableConstraintKind::PrimaryKey(_)
                                | ast::TableConstraintKind::Unique(_)
                        )
                    {
                        self.new_name(schema.clone(), &n.value);
                    }
                }
            }
        }
        // FK の参照先（自分自身を参照するなら不要）。
        let is_self = |r: &ast::ObjectName| {
            r.name().value == table && (r.schema().is_none() || schema_of(r) == schema)
        };
        for el in &ct.elements {
            match el {
                ast::TableElement::Column(col) => {
                    for cc in &col.constraints {
                        if let ast::ColumnConstraintKind::References(fk) = &cc.kind
                            && !is_self(&fk.ref_table)
                        {
                            self.relation(&fk.ref_table, LockMode::ShareRowExclusive);
                        }
                    }
                }
                ast::TableElement::Constraint(tc) => {
                    if let ast::TableConstraintKind::ForeignKey(fk) = &tc.kind
                        && !is_self(&fk.ref_table)
                    {
                        self.relation(&fk.ref_table, LockMode::ShareRowExclusive);
                    }
                }
            }
        }
    }

    fn alter_table(&mut self, a: &ast::AlterTable) {
        use LockMode::{AccessExclusive, ShareRowExclusive};
        match &a.action {
            ast::AlterTableAction::AddConstraint(tc) => match &tc.kind {
                ast::TableConstraintKind::ForeignKey(fk) => {
                    self.relation(&a.name, ShareRowExclusive);
                    self.relation(&fk.ref_table, ShareRowExclusive);
                }
                kind => {
                    self.relation(&a.name, AccessExclusive);
                    if let Some(n) = &tc.name
                        && matches!(
                            kind,
                            ast::TableConstraintKind::PrimaryKey(_)
                                | ast::TableConstraintKind::Unique(_)
                        )
                    {
                        self.new_name(schema_of(&a.name), &n.value);
                    }
                }
            },
            ast::AlterTableAction::OwnerTo(_) | ast::AlterTableAction::Other { .. } => {
                self.relation(&a.name, AccessExclusive);
            }
        }
    }

    fn query(&mut self, q: &ast::Query) {
        self.query_pushed(q, &ForMode::No);
    }

    /// `pushed` は親の FOR 句が押し下げられたもの。自分の FOR 句があればそちらを使う。
    fn query_pushed(&mut self, q: &ast::Query, pushed: &ForMode) {
        let mark = self.ctes.len();
        if let Some(w) = &q.with {
            if w.recursive {
                self.ctes
                    .extend(w.ctes.iter().map(|c| c.name.value.clone()));
            }
            for cte in &w.ctes {
                self.query(&cte.query);
                if !w.recursive {
                    self.ctes.push(cte.name.value.clone());
                }
            }
        }
        // FOR 句は本体の SELECT の FROM に効く（集合演算には付けられない）。
        let own = ForMode::of(&q.locking);
        let for_mode = if matches!(own, ForMode::No) {
            pushed
        } else {
            &own
        };
        self.query_body(&q.body, for_mode);
        for o in &q.order_by {
            self.expr(&o.expr);
        }
        self.opt_expr(q.limit.as_ref());
        self.opt_expr(q.offset.as_ref());
        self.ctes.truncate(mark);
    }

    fn query_body(&mut self, body: &ast::QueryBody, for_mode: &ForMode) {
        match body {
            ast::QueryBody::Select(s) => self.select(s, for_mode),
            ast::QueryBody::Values(v) => {
                for e in v.rows.iter().flatten() {
                    self.expr(e);
                }
            }
            ast::QueryBody::SetOp { left, right, .. } => {
                self.query_body(left, &ForMode::No);
                self.query_body(right, &ForMode::No);
            }
            ast::QueryBody::Nested(q) => self.query(q),
        }
    }

    fn select(&mut self, s: &ast::Select, for_mode: &ForMode) {
        for t in &s.from {
            self.table_ref(t, for_mode);
        }
        self.select_items(&s.targets);
        if let Some(ast::Distinct::On(exprs)) = &s.distinct {
            for e in exprs {
                self.expr(e);
            }
        }
        self.opt_expr(s.selection.as_ref());
        for e in &s.group_by {
            self.expr(e);
        }
        self.opt_expr(s.having.as_ref());
    }

    fn select_items(&mut self, items: &[ast::SelectItem]) {
        for it in items {
            if let ast::SelectItem::Expr { expr, .. } = it {
                self.expr(expr);
            }
        }
    }

    fn table_ref(&mut self, t: &ast::TableRef, for_mode: &ForMode) {
        match t {
            ast::TableRef::Table { name, alias, .. } => {
                let shadowed = name.schema().is_none() && self.ctes.contains(&name.name().value);
                if !shadowed {
                    let refname = alias.as_ref().map_or(&name.name().value, |a| &a.name.value);
                    let mode = if for_mode.applies_to(refname) {
                        LockMode::RowShare
                    } else {
                        LockMode::AccessShare
                    };
                    self.relation(name, mode);
                }
            }
            ast::TableRef::Function { args, .. } => {
                for e in args {
                    self.expr(e);
                }
            }
            ast::TableRef::Subquery { query, alias, .. } => {
                // `FOR` は副問い合わせの FROM の中の表にも押し下げられる（`OF` が別名を指すとき、
                // または `OF` なし）。副問い合わせ自身の FOR 句はその中で足される。
                let pushed = match (for_mode, alias) {
                    (ForMode::All, _) => ForMode::All,
                    (ForMode::Of(_), Some(a)) if for_mode.applies_to(&a.name.value) => ForMode::All,
                    _ => ForMode::No,
                };
                self.query_pushed(query, &pushed);
            }
            ast::TableRef::Join {
                left,
                right,
                constraint,
                ..
            } => {
                self.table_ref(left, for_mode);
                self.table_ref(right, for_mode);
                if let ast::JoinConstraint::On(e) = constraint {
                    self.expr(e);
                }
            }
        }
    }

    fn opt_expr(&mut self, e: Option<&ast::Expr>) {
        if let Some(e) = e {
            self.expr(e);
        }
    }

    /// 式の中の副問い合わせの表を集める（AccessShare。FOR 句の影響を受けない）。
    fn expr(&mut self, e: &ast::Expr) {
        use ast::Expr as E;
        match e {
            E::Literal { .. }
            | E::Column { .. }
            | E::Parameter { .. }
            | E::SessionValue { .. }
            | E::Default { .. } => {}
            E::BinaryOp { left, right, .. }
            | E::And { left, right, .. }
            | E::Or { left, right, .. }
            | E::IsDistinctFrom { left, right, .. }
            | E::NullIf { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            E::UnaryOp { expr, .. }
            | E::Not { expr, .. }
            | E::IsNull { expr, .. }
            | E::IsBool { expr, .. }
            | E::Cast { expr, .. }
            | E::Collate { expr, .. } => self.expr(expr),
            E::Function {
                args,
                filter,
                order_by,
                ..
            } => {
                for a in args {
                    self.expr(a);
                }
                self.opt_expr(filter.as_deref());
                for o in order_by {
                    self.expr(&o.expr);
                }
            }
            E::Case {
                operand,
                whens,
                else_result,
                ..
            } => {
                self.opt_expr(operand.as_deref());
                for w in whens {
                    self.expr(&w.condition);
                    self.expr(&w.result);
                }
                self.opt_expr(else_result.as_deref());
            }
            E::Between {
                expr, low, high, ..
            } => {
                self.expr(expr);
                self.expr(low);
                self.expr(high);
            }
            E::InList { expr, list, .. } => {
                self.expr(expr);
                for l in list {
                    self.expr(l);
                }
            }
            E::InSubquery { expr, query, .. } | E::QuantifiedSubquery { expr, query, .. } => {
                self.expr(expr);
                self.query(query);
            }
            E::Row { items, .. } => {
                for i in items {
                    self.expr(i);
                }
            }
            E::Exists { query, .. } | E::Subquery { query, .. } => self.query(query),
            E::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                self.expr(expr);
                self.expr(pattern);
                self.opt_expr(escape.as_deref());
            }
            E::MinMax { args, .. } | E::Coalesce { args, .. } => {
                for a in args {
                    self.expr(a);
                }
            }
            E::ArrayConstructor { elems, .. } => {
                for a in elems {
                    self.expr(a);
                }
            }
            E::ArraySubquery { query, .. } => self.query(query),
            E::Subscript {
                expr, lower, upper, ..
            } => {
                self.expr(expr);
                self.opt_expr(lower.as_deref());
                self.opt_expr(upper.as_deref());
            }
            E::QuantifiedArray { expr, array, .. } => {
                self.expr(expr);
                self.expr(array);
            }
        }
    }
}

// ----- B. required_locks と検証 ---------------------------------------------------------------

/// `bound` が参照するリレーションと必要なモード（§5.9 B。LK-D12）。順序は出現順、重複なし。
///
/// 契約 §4.6 の署名に `db`（接続しているデータベースの OID）を足した: 共有カタログ以外の `RelKey.db` は
/// 現在のデータベースで、`BoundStatement` からは分からない。
/// 未対応（`Bound*` が空の口）: `LockTable`・FK の DDL。`SELECT ... FOR` の `RowShare` は
/// `BoundSelect.locking`（RW）が入ってから足す。
pub fn required_locks(db: Oid, bound: &BoundStatement) -> Vec<(RelKey, LockMode)> {
    let mut r = Required {
        db,
        out: Vec::new(),
    };
    r.statement(bound);
    r.out
}

struct Required {
    db: Oid,
    out: Vec<(RelKey, LockMode)>,
}

impl Required {
    fn table(&mut self, t: &crate::catalog::TableDef, mode: LockMode) {
        if matches!(t.kind, RelKind::Index | RelKind::Virtual) {
            return;
        }
        let shared = schema::catalog_def(t.oid).is_some_and(|d| d.shared);
        let key = RelKey {
            db: if shared { 0 } else { self.db },
            rel: t.oid,
        };
        if !self.out.contains(&(key, mode)) {
            self.out.push((key, mode));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn statement(&mut self, s: &BoundStatement) {
        use crate::analyzer::bound::{AlterTarget, BoundDdl};
        use LockMode::{AccessExclusive, RowExclusive, Share, ShareRowExclusive};
        match s {
            BoundStatement::Select(q) => self.query(q),
            BoundStatement::Insert(i) => {
                self.table(&i.table, RowExclusive);
                self.query(&i.source);
                if let Some(r) = &i.returning {
                    r.targets.iter().for_each(|e| self.expr(e));
                }
            }
            BoundStatement::Update(u) => {
                self.dml_rtable(&u.rtable);
                self.walk_from(&u.from);
                for (_, src) in &u.assignments {
                    if let crate::analyzer::bound::UpdateSource::Expr(e) = src {
                        self.expr(e);
                    }
                }
                self.opt_expr(u.filter.as_ref());
                if let Some(r) = &u.returning {
                    r.targets.iter().for_each(|e| self.expr(e));
                }
            }
            BoundStatement::Delete(d) => {
                self.dml_rtable(&d.rtable);
                self.walk_from(&d.from);
                self.opt_expr(d.filter.as_ref());
                if let Some(r) = &d.returning {
                    r.targets.iter().for_each(|e| self.expr(e));
                }
            }
            BoundStatement::Copy(c) => self.table(&c.table, RowExclusive),
            BoundStatement::Explain(e) => self.statement(&e.inner),
            BoundStatement::Checkpoint => {}
            BoundStatement::Ddl(d) => match d {
                BoundDdl::DropTable(x) => {
                    x.tables.iter().for_each(|t| self.table(t, AccessExclusive));
                }
                BoundDdl::Truncate(x) => {
                    x.tables.iter().for_each(|t| self.table(t, AccessExclusive));
                }
                BoundDdl::DropSequence(x) => {
                    x.targets
                        .iter()
                        .for_each(|t| self.table(t, AccessExclusive));
                }
                BoundDdl::LockTable(x) => {
                    for t in x.targets.iter().filter(|t| !t.virtual_rel) {
                        let key = RelKey {
                            db: if t.shared { 0 } else { self.db },
                            rel: t.rel,
                        };
                        if !self.out.contains(&(key, x.mode)) {
                            self.out.push((key, x.mode));
                        }
                    }
                }
                BoundDdl::CreateIndex(x) => self.table(&x.table, Share),
                BoundDdl::DropIndex(x) => {
                    // 持ち主の表は IndexDef の `table_oid` だけが分かる（`TableDef` は持たない）。
                    for ix in &x.indexes {
                        let key = RelKey {
                            db: self.db,
                            rel: ix.table_oid,
                        };
                        if !self.out.contains(&(key, AccessExclusive)) {
                            self.out.push((key, AccessExclusive));
                        }
                    }
                }
                BoundDdl::AlterSequence(x) => {
                    if let Some(t) = &x.target {
                        self.table(t, ShareRowExclusive);
                    }
                }
                BoundDdl::AlterTableAddConstraint(x) => {
                    if let AlterTarget::Found(t) = &x.target {
                        self.table(t, AccessExclusive);
                    }
                }
                BoundDdl::AlterTableAddCheck(x) => {
                    if let AlterTarget::Found(t) = &x.target {
                        self.table(t, AccessExclusive);
                    }
                }
                BoundDdl::AlterTableOwner(x) => {
                    if let AlterTarget::Found(t) = &x.target {
                        self.table(t, AccessExclusive);
                    }
                }
                BoundDdl::CreateTable(_)
                | BoundDdl::CreateSequence(_)
                | BoundDdl::Vacuum(_)
                | BoundDdl::Analyze(_)
                | BoundDdl::CreateRole(_)
                | BoundDdl::AlterRole(_)
                | BoundDdl::DropRole(_)
                | BoundDdl::CreateDatabase(_)
                | BoundDdl::DropDatabase(_)
                | BoundDdl::AlterDatabase(_)
                | BoundDdl::AlterTableAddForeignKey(_)
                | BoundDdl::AlterTableDropConstraint(_) => {}
            },
        }
    }

    /// UPDATE / DELETE の `rtable`: 0 番が対象（`RowExclusive`）、残りは `AccessShare`。
    fn dml_rtable(&mut self, rtable: &[crate::analyzer::bound::Rte]) {
        for (i, rte) in rtable.iter().enumerate() {
            let mode = if i == 0 {
                LockMode::RowExclusive
            } else {
                LockMode::AccessShare
            };
            self.rte(rte, mode);
        }
    }

    fn rte(&mut self, rte: &crate::analyzer::bound::Rte, mode: LockMode) {
        use crate::analyzer::bound::RteKind;
        match &rte.kind {
            RteKind::Table { table } => self.table(table, mode),
            RteKind::Subquery { query } => self.query(query),
            RteKind::Values { rows } => rows.iter().flatten().for_each(|e| self.expr(e)),
            RteKind::Function { call } => self.expr(call),
            RteKind::Join { .. } | RteKind::CteRef { .. } => {}
        }
    }

    fn walk_from(&mut self, items: &[crate::analyzer::bound::FromItem]) {
        use crate::analyzer::bound::FromItem;
        for it in items {
            if let FromItem::Join {
                left, right, on, ..
            } = it
            {
                self.walk_from(std::slice::from_ref(left));
                self.walk_from(std::slice::from_ref(right));
                self.opt_expr(on.as_ref());
            }
        }
    }

    fn query(&mut self, q: &crate::analyzer::bound::BoundQuery) {
        use crate::analyzer::bound::BoundSetExpr;
        for c in &q.ctes {
            self.query(&c.query);
        }
        match &q.body {
            BoundSetExpr::Select(s) => {
                for rte in &s.rtable {
                    self.rte(rte, LockMode::AccessShare);
                }
                self.walk_from(&s.from);
                for e in s
                    .filter
                    .iter()
                    .chain(&s.group_by)
                    .chain(s.having.iter())
                    .chain(&s.targets)
                {
                    self.expr(e);
                }
            }
            BoundSetExpr::Values { rows, .. } => rows.iter().flatten().for_each(|e| self.expr(e)),
            BoundSetExpr::SetOp { left, right, .. } => {
                self.query(left);
                self.query(right);
            }
        }
        self.opt_expr(q.limit.as_ref());
        self.opt_expr(q.offset.as_ref());
    }

    fn opt_expr(&mut self, e: Option<&BoundExpr>) {
        if let Some(e) = e {
            self.expr(e);
        }
    }

    /// 式の中の副問い合わせ（`walk` は `SubLink` の `query` へは降りない）。
    fn expr(&mut self, e: &BoundExpr) {
        e.walk(&mut |n| {
            if let crate::expr::ExprKind::SubLink { query, .. } = &n.kind {
                self.query(query);
            }
            true
        });
    }
}

/// `key` について `mode` が満たされているか: 同じ `key` で保持しているモード h のうち
/// `mode_covers(h, mode)` が 1 つでもあれば満たされている（`LockManager::holds` を 8 モードについて引く）。
pub fn lock_satisfied(
    locks: &LockManager,
    backend: BackendId,
    key: RelKey,
    mode: LockMode,
) -> bool {
    LockMode::ALL
        .into_iter()
        .any(|h| rel_lock::mode_covers(h, mode) && locks.holds(backend, key.tag(), h))
}

/// `required` のうち満たされていないもの。
pub fn missing_from(
    locks: &LockManager,
    backend: BackendId,
    required: Vec<(RelKey, LockMode)>,
) -> Vec<(RelKey, LockMode)> {
    required
        .into_iter()
        .filter(|(k, m)| !lock_satisfied(locks, backend, *k, *m))
        .collect()
}

// ----- C. acquire_statement_locks -----------------------------------------------------------

fn written_name(schema: Option<&str>, name: &str) -> String {
    schema.map_or_else(|| name.to_owned(), |s| format!("{s}.{name}"))
}

/// `LOCK TABLE` の対象にできるか（§6.4）。`Table` は `true`、`Virtual` は成功して何もしない（`false`）、
/// `Index` / `Sequence` は 42809（メッセージの名前はスキーマなし）。
fn check_lockable(r: &ResolvedRel, name: &str) -> Result<bool> {
    let what = match r.kind {
        RelKind::Table => return Ok(true),
        RelKind::Virtual => return Ok(false),
        RelKind::Index => "indexes",
        RelKind::Sequence => "sequences",
    };
    Err(Error::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("cannot lock relation \"{name}\""),
    )
    .with_detail(format!("This operation is not supported for {what}.")))
}

/// §5.9 C の本体。`Session` なしで試せるように、ロックマネージャ・名前引き・待ちの条件を引数で受ける。
/// 1 件ずつ**取れたものを保持したまま**次を待つ（途中の失敗はトランザクションのアボートが全部解放する）。
pub fn acquire_requests(
    locks: &LockManager,
    backend: BackendId,
    db: Oid,
    wait: &lock::WaitCtl<'_>,
    resolver: &mut dyn RelationResolver,
    reqs: &[LockRequest],
) -> Result<LockSet> {
    let resolver = RefCell::new(resolver);
    let mut set = LockSet::default();
    // `DropPeers` の対象（直前の Relation / IndexOwner の解決結果）。
    let mut last: Option<RelKey> = None;
    for req in reqs {
        match &req.target {
            LockTarget::Relation { schema, name, .. }
            | LockTarget::IndexOwner { schema, name, .. } => {
                last = None;
                let is_index = matches!(req.target, LockTarget::IndexOwner { .. });
                let label = req.nowait.then(|| written_name(schema.as_deref(), name));
                let opts = RelLockOpts {
                    mode: req.mode,
                    nowait_label: label.as_deref(),
                };
                let key = rel_lock::lock_relation(
                    locks,
                    backend,
                    wait,
                    &opts,
                    &|| resolver.borrow().generation(),
                    &mut || {
                        let mut r = resolver.borrow_mut();
                        let found = if is_index {
                            r.index_owner(schema.as_deref(), name)?
                        } else {
                            r.lookup(schema.as_deref(), name)?
                        };
                        // 見つからない → 飛ばす（LK-D13）。
                        let Some(rel) = found else {
                            return Ok(None);
                        };
                        r.check_access(&rel, req.mode)?;
                        if req.lock_table {
                            if !check_lockable(&rel, name)? {
                                return Ok(None);
                            }
                        } else if matches!(rel.kind, RelKind::Index | RelKind::Virtual) {
                            return Ok(None);
                        }
                        Ok(Some((if rel.shared { 0 } else { db }, rel.oid)))
                    },
                )?;
                if let Some((kdb, rel)) = key {
                    let k = RelKey { db: kdb, rel };
                    set.entries.push((k, req.mode));
                    last = Some(k);
                }
            }
            LockTarget::NewName { schema, name } => {
                let ns = resolver
                    .borrow_mut()
                    .creation_namespace(schema.as_deref())?;
                rel_lock::lock_name(locks, backend, wait, db, ns, name)?;
            }
            LockTarget::DropPeers { constraint } => {
                if let Some(k) = last {
                    let peers = resolver
                        .borrow_mut()
                        .drop_peers(k.rel, constraint.as_deref())?;
                    for peer in peers {
                        let key = RelKey { db, rel: peer };
                        locks.acquire(
                            backend,
                            key.tag(),
                            LockMode::AccessExclusive,
                            LockScope::Transaction,
                            wait,
                        )?;
                        set.entries.push((key, LockMode::AccessExclusive));
                    }
                }
            }
        }
    }
    Ok(set)
}

/// OID で直接取る（名前解決しない）。`missing_locks` の結果を取るのに使う。
pub fn acquire_keys(
    locks: &LockManager,
    backend: BackendId,
    wait: &lock::WaitCtl<'_>,
    items: &[(RelKey, LockMode)],
) -> Result<()> {
    for (key, mode) in items {
        wait.interrupts.check()?;
        locks.acquire(backend, key.tag(), *mode, LockScope::Transaction, wait)?;
    }
    Ok(())
}

/// `Session` の [`RelationResolver`]。呼ぶたびに最新のカタログ用スナップショット（自分の変更を含む。D12）
/// を取って（登録）、使い終えたら Drop する（LK-D9）。
pub struct SessionResolver {
    cluster: Arc<Cluster>,
    db: Arc<DatabaseHandle>,
    search_path: Vec<String>,
    own: Option<Xid>,
    cid: CommandId,
    /// このトランザクションがカタログを変えた（共有キャッシュを使わない）。
    bypass_cache: bool,
    superuser: bool,
}

impl SessionResolver {
    fn with_catalog<R>(
        &self,
        f: impl FnOnce(&StatementCatalog<'_>, &Snapshot) -> Result<R>,
    ) -> Result<R> {
        // 世代はスナップショットより先に読む（キャッシュを古い定義で埋めないため。with_stmt_env と同じ）。
        let generation = self.db.cache.generation();
        let snap = self.cluster.txn_manager().take_snapshot(self.own, self.cid);
        let cat = StatementCatalog {
            db: &self.db,
            snapshot: &snap,
            gen_at_snapshot: generation,
            bypass_cache: self.bypass_cache,
            search_path: &self.search_path,
        };
        f(&cat, &snap)
    }

    fn resolved(
        cat: &StatementCatalog<'_>,
        oid: Oid,
        kind: RelKind,
        schema: Option<&str>,
        name: &str,
    ) -> Result<ResolvedRel> {
        let shared = schema::catalog_def(oid).is_some_and(|d| d.shared);
        let schema = match (schema, kind) {
            (Some(s), _) => s.to_owned(),
            (None, RelKind::Table | RelKind::Sequence) => cat
                .table_by_oid(oid)?
                .map(|t| t.schema.clone())
                .unwrap_or_default(),
            (None, _) => String::new(),
        };
        Ok(ResolvedRel {
            oid,
            kind,
            shared,
            schema,
            name: name.to_owned(),
        })
    }
}

impl RelationResolver for SessionResolver {
    fn lookup(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>> {
        self.with_catalog(|cat, _| {
            let Some((oid, kind)) = cat.relation_kind(schema, name)? else {
                return Ok(None);
            };
            Self::resolved(cat, oid, kind, schema, name).map(Some)
        })
    }

    fn index_owner(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>> {
        self.with_catalog(|cat, _| {
            let Some(ix) = cat.index_by_name(schema, name)? else {
                return Ok(None);
            };
            let Some(owner) = cat.table_by_oid(ix.table_oid)? else {
                return Ok(None);
            };
            Self::resolved(cat, owner.oid, owner.kind, None, &owner.name).map(Some)
        })
    }

    fn creation_namespace(&mut self, schema: Option<&str>) -> Result<Oid> {
        self.with_catalog(|_, snap| match schema {
            Some("pg_catalog") => Ok(schema::oids::NAMESPACE_PG_CATALOG),
            Some(s) => self.db.catalog.namespace_oid(snap, s)?.ok_or_else(|| {
                Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{s}\" does not exist"),
                )
            }),
            None => {
                for entry in &self.search_path {
                    if entry == "pg_catalog" {
                        continue;
                    }
                    if let Some(nsp) = self.db.catalog.namespace_oid(snap, entry)? {
                        return Ok(nsp);
                    }
                }
                Err(Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "no schema has been selected to create in",
                ))
            }
        })
    }

    fn drop_peers(&mut self, rel: Oid, constraint: Option<&str>) -> Result<Vec<Oid>> {
        use crate::catalog::depend::{DependType, ObjectAddress, classes};
        let store = &self.db.catalog;
        self.with_catalog(|cat, snap| {
            let Some(def) = cat.table_by_oid(rel)? else {
                return Ok(Vec::new());
            };
            let mut peers: Vec<Oid> = Vec::new();
            if let Some(name) = constraint {
                peers.extend(
                    def.foreign_keys
                        .iter()
                        .filter(|fk| fk.name == name)
                        .map(|fk| fk.parent),
                );
            } else {
                peers.extend(def.foreign_keys.iter().map(|fk| fk.parent));
                peers.extend(def.referenced_by.iter().map(|fk| fk.child));
                peers.extend(def.identity_seqs.iter().map(|(_, seq)| *seq));
                // SERIAL の所有シーケンス（列への Auto 依存。IDENTITY の Internal は上と重複する）。
                peers.extend(
                    store
                        .dependents_of(snap, ObjectAddress::relation(rel))?
                        .into_iter()
                        .filter(|d| {
                            matches!(d.deptype, DependType::Auto | DependType::Internal)
                                && d.dependent.class_id == classes::RELATION
                                && d.referenced.obj_sub > 0
                        })
                        .map(|d| d.dependent.obj_id),
                );
            }
            let mut seen = std::collections::HashSet::new();
            peers.retain(|p| *p != rel && seen.insert(*p));
            Ok(peers)
        })
    }

    fn check_access(&mut self, rel: &ResolvedRel, _mode: LockMode) -> Result<()> {
        if rel.oid == schema::oids::PG_AUTHID && !self.superuser {
            return Err(Error::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "permission denied for table pg_authid",
            ));
        }
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.db.cache.generation()
    }
}

#[allow(dead_code)] // 結線は exec_data_statement・Bind（LK-4c・XQ・RW）。
impl Session {
    /// 待ちの中断条件（`lock_timeout`、`deadlock_timeout`、キャンセル）。
    fn lock_wait_ctl(&self) -> lock::WaitCtl<'_> {
        lock::WaitCtl {
            lock_timeout: self.settings.lock_timeout(),
            deadlock_timeout: self.settings.deadlock_timeout(),
            interrupts: &self.interrupt,
        }
    }

    pub(super) fn relation_resolver(&self) -> Result<SessionResolver> {
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        Ok(SessionResolver {
            cluster,
            db,
            search_path: self.settings.search_path(),
            own: self.txn.xid,
            cid: self.txn.cid,
            bypass_cache: self.txn.catalog_dirty,
            superuser: self.role.superuser,
        })
    }

    /// §5.9 の手順 2。待ちのエラー（55P03 / 40P01 / 57014 / 57P01）、LOCK TABLE の 42809、3F000、
    /// `check_access` の 42501 を返す。
    pub(super) fn acquire_statement_locks(&mut self, reqs: &[LockRequest]) -> Result<LockSet> {
        let mut resolver = self.relation_resolver()?;
        let cluster = Arc::clone(&resolver.cluster);
        let wait = self.lock_wait_ctl();
        acquire_requests(
            cluster.locks(),
            BackendId(self.id),
            resolver.db.oid,
            &wait,
            &mut resolver,
            reqs,
        )
    }

    /// `exec_data_statement` の手順 0〜2（§5.9）: `LOCK TABLE` のブロック外 25P01、文が使うリレーションのロック。
    /// XID の割り当て・共有バリア・スナップショットより**前**に呼ぶ（ロックを待っている間は何も持たない。規約 1）。
    pub(super) fn lock_for_statement(&mut self, stmt: &Statement) -> Result<()> {
        if matches!(stmt, Statement::LockTable(_)) {
            crate::ddl::lock_table::require_transaction_block(!self.in_block_for_set())?;
        }
        let reqs = collect_lock_requests(stmt);
        if reqs.is_empty() {
            return Ok(());
        }
        self.acquire_statement_locks(&reqs).map(drop)
    }

    /// 04 章 C7 の口:`acquire_statement_locks(&collect_lock_requests(stmt))` の結果を
    /// (リレーションの OID, モード) の一覧にしたもの。名前の解決は毎回行う（OID でロックする最適化は M6）。
    pub(super) fn lock_statement_relations(
        &mut self,
        stmt: &Statement,
    ) -> Result<Vec<(Oid, LockMode)>> {
        let set = self.acquire_statement_locks(&collect_lock_requests(stmt))?;
        Ok(set.entries.into_iter().map(|(k, m)| (k.rel, m)).collect())
    }

    /// LK-D12 の検証: `bound` が要るロックのうち、満たされていないもの。
    pub(super) fn missing_locks(&self, bound: &BoundStatement) -> Vec<(RelKey, LockMode)> {
        let (Some(cluster), Some(db)) = (&self.cluster, &self.db) else {
            return Vec::new();
        };
        missing_from(
            cluster.locks(),
            BackendId(self.id),
            required_locks(db.oid, bound),
        )
    }

    /// OID で直接取る（名前解決しない）。
    pub(super) fn acquire_oids(&mut self, items: &[(RelKey, LockMode)]) -> Result<()> {
        let cluster = self.cluster()?;
        let wait = self.lock_wait_ctl();
        acquire_keys(cluster.locks(), BackendId(self.id), &wait, items)
    }

    /// §5.8 の手順 4・5: トランザクションスコープのロックを全部解放し、`xact_snapshot` を外す。
    /// （`commit_transaction` / `rollback_transaction` は `self.txn` を取り出した後に同じ解放を行っている。
    /// 結線は RW の持ち物の `txn_ctl.rs` で 1 行置き換える。）
    pub(super) fn release_transaction_resources(&mut self) {
        self.txn.xact_snapshot = None;
        if let Some(cluster) = &self.cluster {
            cluster
                .locks()
                .release_all(BackendId(self.id), LockScope::Transaction);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::interrupt::InterruptFlag;

    fn reqs(sql: &str) -> Vec<(String, LockMode)> {
        let stmts = crate::sql::parser::parse(sql).unwrap();
        collect_lock_requests(&stmts[0])
            .into_iter()
            .map(|r| {
                let label = match r.target {
                    LockTarget::Relation { schema, name, .. } => {
                        format!("rel:{}", written_name(schema.as_deref(), &name))
                    }
                    LockTarget::IndexOwner { schema, name, .. } => {
                        format!("idx:{}", written_name(schema.as_deref(), &name))
                    }
                    LockTarget::NewName { schema, name } => {
                        format!("new:{}", written_name(schema.as_deref(), &name))
                    }
                    LockTarget::DropPeers { constraint } => format!("peers:{constraint:?}"),
                };
                (label, r.mode)
            })
            .collect()
    }

    fn r(label: &str, mode: LockMode) -> (String, LockMode) {
        (label.to_owned(), mode)
    }

    #[test]
    fn select_takes_access_share_in_analysis_order() {
        use LockMode::AccessShare as S;
        assert_eq!(
            reqs("select * from a join b on a.x = b.x where exists (select 1 from c)"),
            vec![r("rel:a", S), r("rel:b", S), r("rel:c", S)]
        );
        // 同じ要求は 1 つにまとめる。
        assert_eq!(
            reqs("select * from a, a x, s.a"),
            vec![r("rel:a", S), r("rel:s.a", S)]
        );
    }

    #[test]
    fn with_clause_comes_first_and_ctes_shadow_tables() {
        use LockMode::AccessShare as S;
        assert_eq!(
            reqs("with w as (select * from a) select * from w, b"),
            vec![r("rel:a", S), r("rel:b", S)]
        );
    }

    #[test]
    fn dml_locks_the_target_first() {
        use LockMode::{AccessShare as S, RowExclusive as RX};
        assert_eq!(
            reqs("insert into t select * from a"),
            vec![r("rel:t", RX), r("rel:a", S)]
        );
        assert_eq!(
            reqs("update t set x = (select max(y) from b) from a where t.id = a.id"),
            vec![r("rel:t", RX), r("rel:a", S), r("rel:b", S)]
        );
        assert_eq!(
            reqs("delete from t using a where t.id in (select id from b)"),
            vec![r("rel:t", RX), r("rel:a", S), r("rel:b", S)]
        );
    }

    #[test]
    fn explain_uses_the_inner_statement() {
        assert_eq!(
            reqs("explain select * from a"),
            vec![r("rel:a", LockMode::AccessShare)]
        );
    }

    #[test]
    fn for_clause_raises_the_targets_to_row_share() {
        use LockMode::{AccessShare as S, RowShare as RS};
        assert_eq!(
            reqs("select * from a, b for update"),
            vec![r("rel:a", RS), r("rel:b", RS)]
        );
        assert_eq!(
            reqs("select * from a, b for share of b"),
            vec![r("rel:a", S), r("rel:b", RS)]
        );
        // 式の中の副問い合わせは FOR の影響を受けない。
        assert_eq!(
            reqs("select * from a where exists (select 1 from b) for update"),
            vec![r("rel:a", RS), r("rel:b", S)]
        );
    }

    #[test]
    fn ddl_requests_follow_the_table() {
        use LockMode::{AccessExclusive as AX, Exclusive as X, Share, ShareRowExclusive as SRX};
        assert_eq!(
            reqs("create table t (id int primary key, p int references parent(id))"),
            vec![r("new:t", X), r("rel:parent", SRX)]
        );
        assert_eq!(
            reqs("drop table a, s.b"),
            vec![
                r("rel:a", AX),
                r("peers:None", AX),
                r("rel:s.b", AX),
                r("peers:None", AX)
            ]
        );
        assert_eq!(reqs("truncate a"), vec![r("rel:a", AX)]);
        assert_eq!(
            reqs("create index i on t (x)"),
            vec![r("rel:t", Share), r("new:i", X)]
        );
        assert_eq!(reqs("drop index s.i"), vec![r("idx:s.i", AX)]);
        assert_eq!(reqs("create sequence q"), vec![r("new:q", X)]);
        assert_eq!(reqs("alter sequence q restart"), vec![r("rel:q", SRX)]);
        assert_eq!(reqs("drop sequence q"), vec![r("rel:q", AX)]);
        assert_eq!(
            reqs("alter table t add constraint u unique (x)"),
            vec![r("rel:t", AX), r("new:u", X)]
        );
        assert_eq!(
            reqs("alter table t add foreign key (x) references p (id)"),
            vec![r("rel:t", SRX), r("rel:p", SRX)]
        );
    }

    #[test]
    fn copy_modes() {
        assert_eq!(
            reqs("copy t from stdin"),
            vec![r("rel:t", LockMode::RowExclusive)]
        );
        assert_eq!(
            reqs("copy t to stdout"),
            vec![r("rel:t", LockMode::AccessShare)]
        );
    }

    #[test]
    fn control_statements_take_nothing() {
        for sql in [
            "begin",
            "commit",
            "set search_path = x",
            "show search_path",
            "checkpoint",
            "vacuum",
            "create role r",
        ] {
            assert!(reqs(sql).is_empty(), "{sql}");
        }
    }

    // ----- acquire_requests -----

    #[derive(Default)]
    struct FakeResolver {
        rels: Vec<(String, ResolvedRel)>,
        peers: Vec<Oid>,
        generation: u64,
        lookups: usize,
    }

    impl FakeResolver {
        fn with(mut self, name: &str, oid: Oid, kind: RelKind) -> Self {
            self.rels.push((
                name.to_owned(),
                ResolvedRel {
                    oid,
                    kind,
                    shared: false,
                    schema: "public".into(),
                    name: name.to_owned(),
                },
            ));
            self
        }

        fn find(&self, name: &str) -> Option<ResolvedRel> {
            self.rels
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, r)| r.clone())
        }
    }

    impl RelationResolver for FakeResolver {
        fn lookup(&mut self, _s: Option<&str>, name: &str) -> Result<Option<ResolvedRel>> {
            self.lookups += 1;
            Ok(self.find(name))
        }
        fn index_owner(&mut self, _s: Option<&str>, name: &str) -> Result<Option<ResolvedRel>> {
            Ok(self.find(name))
        }
        fn creation_namespace(&mut self, schema: Option<&str>) -> Result<Oid> {
            match schema {
                Some("nope") => Err(Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    "schema \"nope\" does not exist",
                )),
                _ => Ok(2200),
            }
        }
        fn drop_peers(&mut self, _rel: Oid, _c: Option<&str>) -> Result<Vec<Oid>> {
            Ok(self.peers.clone())
        }
        fn generation(&self) -> u64 {
            self.generation
        }
    }

    const DB: Oid = 5;

    fn run(
        locks: &LockManager,
        backend: u64,
        res: &mut FakeResolver,
        sql: &str,
    ) -> Result<LockSet> {
        let flag = InterruptFlag::default();
        let wait = lock::WaitCtl {
            lock_timeout: Some(Duration::from_millis(100)),
            deadlock_timeout: Duration::from_secs(30),
            interrupts: &flag,
        };
        let stmts = crate::sql::parser::parse(sql).unwrap();
        acquire_requests(
            locks,
            BackendId(backend),
            DB,
            &wait,
            res,
            &collect_lock_requests(&stmts[0]),
        )
    }

    fn setup() -> Arc<LockManager> {
        let lm = LockManager::new();
        lm.register_backend(BackendId(1), 101);
        lm.register_backend(BackendId(2), 102);
        lm
    }

    fn key(rel: Oid) -> RelKey {
        RelKey { db: DB, rel }
    }

    #[test]
    fn acquire_locks_found_relations_and_skips_missing_ones() {
        let lm = setup();
        let mut res = FakeResolver::default().with("a", 100, RelKind::Table);
        let set = run(&lm, 1, &mut res, "select * from a, missing").unwrap();
        assert_eq!(set.entries, vec![(key(100), LockMode::AccessShare)]);
        assert!(lock_satisfied(
            &lm,
            BackendId(1),
            key(100),
            LockMode::AccessShare
        ));
    }

    #[test]
    fn indexes_and_virtual_relations_are_not_locked_by_a_select() {
        let lm = setup();
        let mut res = FakeResolver::default()
            .with("ix", 101, RelKind::Index)
            .with("v", 102, RelKind::Virtual);
        let set = run(&lm, 1, &mut res, "select * from ix, v").unwrap();
        assert!(set.entries.is_empty());
    }

    #[test]
    fn drop_table_takes_the_peers_after_the_table() {
        let lm = setup();
        let mut res = FakeResolver::default().with("a", 100, RelKind::Table);
        res.peers = vec![200, 201];
        let set = run(&lm, 1, &mut res, "drop table a").unwrap();
        assert_eq!(
            set.entries,
            vec![
                (key(100), LockMode::AccessExclusive),
                (key(200), LockMode::AccessExclusive),
                (key(201), LockMode::AccessExclusive),
            ]
        );
    }

    #[test]
    fn a_conflicting_lock_times_out_with_55p03() {
        let lm = setup();
        let mut res = FakeResolver::default().with("a", 100, RelKind::Table);
        run(&lm, 1, &mut res, "truncate a").unwrap();
        let err = run(&lm, 2, &mut res, "select * from a").unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
        // 解放すれば取れる。
        lm.release_all(BackendId(1), LockScope::Transaction);
        run(&lm, 2, &mut res, "select * from a").unwrap();
    }

    #[test]
    fn a_missing_schema_for_a_new_name_is_3f000() {
        let lm = setup();
        let mut res = FakeResolver::default();
        let err = run(&lm, 1, &mut res, "create sequence nope.q").unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INVALID_SCHEMA_NAME);
        run(&lm, 1, &mut res, "create sequence q").unwrap();
    }

    #[test]
    fn new_names_conflict_between_backends() {
        let lm = setup();
        let mut res = FakeResolver::default();
        run(&lm, 1, &mut res, "create sequence q").unwrap();
        let err = run(&lm, 2, &mut res, "create sequence q").unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    }

    #[test]
    fn missing_from_uses_covering_modes() {
        let lm = setup();
        let mut res = FakeResolver::default().with("t", 100, RelKind::Table);
        run(&lm, 1, &mut res, "delete from t").unwrap(); // RowExclusive
        let need = vec![
            (key(100), LockMode::AccessShare),
            (key(100), LockMode::RowExclusive),
            (key(100), LockMode::Share),
            (key(101), LockMode::AccessShare),
        ];
        let missing = missing_from(&lm, BackendId(1), need);
        assert_eq!(
            missing,
            vec![
                (key(100), LockMode::Share),
                (key(101), LockMode::AccessShare)
            ]
        );
    }

    #[test]
    fn acquire_keys_takes_what_is_missing() {
        let lm = setup();
        let flag = InterruptFlag::default();
        let wait = lock::WaitCtl {
            lock_timeout: Some(Duration::from_millis(100)),
            deadlock_timeout: Duration::from_secs(30),
            interrupts: &flag,
        };
        acquire_keys(
            &lm,
            BackendId(1),
            &wait,
            &[(key(100), LockMode::AccessExclusive)],
        )
        .unwrap();
        assert!(lock_satisfied(
            &lm,
            BackendId(1),
            key(100),
            LockMode::RowShare
        ));
        let err = acquire_keys(
            &lm,
            BackendId(2),
            &wait,
            &[(key(100), LockMode::AccessShare)],
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    }
}
