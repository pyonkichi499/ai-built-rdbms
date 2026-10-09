//! Volcano-style executor. (Owned by the executor implementer; `build` is
//! in `build.rs`.)

pub mod agg;
pub mod build;
pub mod dml;
pub mod eval;
pub mod instrument;
pub mod mem;
pub mod nodes;
pub mod ri;
pub mod seq;
pub mod subplan;
pub mod virtual_scan;

pub use build::{build, build_query};
pub use eval::eval;

use std::rc::Rc;

use self::mem::{DEFAULT_QUERY_MEM_LIMIT, MemBudget};
use self::subplan::{CteStates, SubPlanStates};
use crate::catalog::CatalogReader;
use crate::error::{Error, Result};
use crate::expr::ParamId;
use crate::interrupt::InterruptFlag;
use crate::planner::physical::PhysicalQuery;
use crate::storage::{
    BuildStats, BuildUnique, IndexHandle, IndexScan, IndexStore, ResolvedScanKeys, ScanDirection,
    TableStore, UniqueCheck, WriteCtx,
};
use crate::txn::lock::WaitCtx;
use crate::txn::{Snapshot, Transaction};
use crate::types::{Datum, Row, Tid, TypeEnv};

/// Per-session values visible to expressions (`current_user`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// `current_user` / `user` / `current_role`.
    pub current_user: String,
    /// `session_user`.
    pub session_user: String,
    /// `current_database()` / `current_catalog`.
    pub database: String,
    /// `current_schema()` / `current_schema`; `None` gives NULL.
    pub current_schema: Option<String>,
}

/// クラスタやセッションの実行時の情報。session が実装する（`m3.md` §4.9）。
pub trait RuntimeInfo: std::fmt::Debug {
    fn backend_pid(&self) -> i32;
    /// `TxnManager::is_blocked_by` に委ねる。
    fn is_blocked_by(&self, pid: i32, among: &[i32]) -> bool;
    /// `pg_blocking_pids(pid)`: `pid` を止めているバックエンドの pid（`pid` → `BackendId` →
    /// `LockManager::blocking_backends` → pid。M5 01 §4.9）。
    fn blocking_pids(&self, _pid: i32) -> Vec<i32> {
        Vec::new()
    }
    /// `pg_locks` の元になるロック表のスナップショット（`LockManager::lock_status`。M5 01 §6.5）。
    fn lock_status(&self) -> Vec<crate::txn::lock::LockStatusRow> {
        Vec::new()
    }
    /// `pg_roles` の元になる `pg_authid` の全行（最新のカタログ用スナップショット。M5 08 §5.10）。
    fn authid_rows(&self) -> Result<Vec<crate::catalog::roles::AuthidRow>> {
        Ok(Vec::new())
    }
    /// 他のトランザクション `xid` の終了を待つ（一意検査の `InsertOutcome::WaitFor` の待ち。M5 02 §5.9）。
    /// 実装は `LockManager::wait_for_xact(backend, xid, still_running, wait_ctl)` に委ねる。
    /// 呼ぶときはページのラッチもピンも持っていない。session が実装するまでの既定は内部エラー。
    fn wait_for_xact(&self, _xid: crate::txn::Xid) -> Result<()> {
        Err(Error::internal(
            "waiting for another transaction is not wired into this runtime",
        ))
    }
    /// `InterruptFlag::check` に委ねる（`pg_sleep` が 10ms ごとに呼ぶ）。
    fn check_interrupts(&self) -> Result<()>;
    /// 現在のトランザクションに割り当て済みの XID（未割り当てなら `None`）。
    /// `txid_current_if_assigned` が使う。
    fn current_xid(&self) -> Option<u64> {
        None
    }
    /// 実行中の文の開始時刻（2000-01-01 からのマイクロ秒）。`statement_timestamp()` が使う。未追跡なら `None`。
    fn statement_timestamp(&self) -> Option<i64> {
        None
    }
    /// `current_setting` の値。未知のパラメータは `None`。
    fn get_setting(&self, _name: &str) -> Result<Option<String>> {
        Ok(None)
    }
    /// `set_config`。`value = None` は既定値に戻す。設定後の値を返す。
    fn set_setting(&self, _name: &str, _value: Option<&str>, _local: bool) -> Result<String> {
        Err(crate::error::Error::new(
            crate::error::sqlstate::FEATURE_NOT_SUPPORTED,
            "set_config is not supported here",
        ))
    }
    /// シーケンス関数（`m4/08` §5.1、§5.2）。25006（READ ONLY）、55000（`currval` / `lastval` が未定義）、
    /// 2200H はここで返す。`SeqRuntime` に委ねる。
    fn nextval(&self, _seq: crate::types::Oid) -> Result<i64> {
        Err(crate::error::Error::not_supported(
            "nextval is not supported here",
        ))
    }
    fn currval(&self, _seq: crate::types::Oid) -> Result<i64> {
        Err(crate::error::Error::not_supported(
            "currval is not supported here",
        ))
    }
    fn lastval(&self) -> Result<i64> {
        Err(crate::error::Error::not_supported(
            "lastval is not supported here",
        ))
    }
    fn setval(&self, _seq: crate::types::Oid, _value: i64, _is_called: bool) -> Result<i64> {
        Err(crate::error::Error::not_supported(
            "setval is not supported here",
        ))
    }
}

/// 何も持たない `RuntimeInfo`（pid 0、ブロックされない、中断なし）。
/// 単体テストと、本物の実装に置き換わるまでの仮置き。
#[derive(Debug, Clone, Copy, Default)]
pub struct NullRuntime;

impl RuntimeInfo for NullRuntime {
    fn backend_pid(&self) -> i32 {
        0
    }

    fn is_blocked_by(&self, _pid: i32, _among: &[i32]) -> bool {
        false
    }

    fn check_interrupts(&self) -> Result<()> {
        Ok(())
    }
}

/// What expression evaluation may look at: the session and the catalog
/// (`FnKind::Context` functions such as `pg_get_userbyid` need the latter).
/// 担当 H2 が `eval_expr` をこれを受け取る形にする（`m2.md` §4.6）。
#[derive(Clone, Copy)]
pub struct EvalCtx<'a> {
    pub session: &'a SessionInfo,
    pub catalog: &'a dyn CatalogReader,
    /// `FnKind::Runtime` の関数（`pg_backend_pid` など）が使う。
    pub runtime: &'a dyn RuntimeInfo,
    /// 日時・数値の入出力と `CastMethod::Env` が使う（`m4/02` §3.7.2）。
    pub type_env: &'a TypeEnv<'a>,
}

impl std::fmt::Debug for EvalCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalCtx")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Everything an executor needs at run time. Passed to every `next` call
/// so executor structs carry no lifetimes.
pub struct ExecCtx<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub txn: &'a mut Transaction,
    /// Taken at the start of the statement.
    pub snapshot: &'a Snapshot,
    pub session: &'a SessionInfo,
    /// Shutdown request (`m2.md` §6.11).
    pub interrupts: &'a InterruptFlag,
    /// クラスタやセッションの実行時の情報（`m3.md` §4.9）。
    pub runtime: &'a dyn RuntimeInfo,
    /// 索引（M4）。
    pub indexes: &'a dyn IndexStore,
    /// `subplans` / `ctes` を引く（M4）。
    pub query: &'a PhysicalQuery,
    /// `n_params` 個。添字は `ParamId`（M4）。
    pub params: Vec<Datum>,
    /// クエリ単位のメモリ予算（M4）。
    pub mem: MemBudget,
    /// `SubPlan` / `InitPlan` の Executor と結果（M4）。
    pub subplans: SubPlanStates,
    /// CTE の行（M4）。
    pub ctes: CteStates,
    pub type_env: &'a TypeEnv<'a>,
    /// EXPLAIN ANALYZE の計測（`None` なら計測しない。`m4/10` §3.10、11 §7.1 の C-2）。
    pub instr: Option<Rc<instrument::Instrumentation>>,
    /// 行ロック・更新の待ち（誰が待つか + 中断条件。`m5/02` §4.4）。`None` の間は待たない（`BeingModified` を返す）。
    /// session が配線するまでの既定。`LockRows` は `None` だと内部エラー。
    pub wait: Option<WaitCtx<'a>>,
    /// Repeatable Read のときだけ `Some`（`snapshot` と同じもの。40001 の判定に使う。`m5/02` §5.6）。
    pub xact_snapshot: Option<&'a Snapshot>,
}

/// `ExecCtx` が所有する実行時の状態の束。中断中のポータルが持ち越すために
/// `ExecCtx::take_state` / `ExecCtx::restore_state` で出し入れする（M5 04 C6）。
#[derive(Debug, Default)]
pub struct ExecState {
    pub params: Vec<Datum>,
    pub mem: MemBudget,
    pub subplans: SubPlanStates,
    pub ctes: CteStates,
}

/// `ExecCtx::new` に渡す、文ごとに session が作る借用の束（`m4/02` §3.7.2）。
#[derive(Clone, Copy)]
pub struct ExecEnv<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub indexes: &'a dyn IndexStore,
    pub snapshot: &'a Snapshot,
    pub session: &'a SessionInfo,
    pub runtime: &'a dyn RuntimeInfo,
    pub interrupts: &'a InterruptFlag,
    pub type_env: &'a TypeEnv<'a>,
    /// `yuzhu.query_mem_limit`。
    pub mem_limit: usize,
    /// EXPLAIN ANALYZE の計測（C-10: `ExecEnv` が持つ）。
    pub instr: Option<&'a Rc<instrument::Instrumentation>>,
    /// 行ロック・更新の待ち（`ExecCtx::wait` になる）。`None` なら待たない。
    pub wait: Option<WaitCtx<'a>>,
}

impl std::fmt::Debug for ExecEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecEnv")
            .field("mem_limit", &self.mem_limit)
            .finish_non_exhaustive()
    }
}

impl<'a> ExecEnv<'a> {
    /// 索引を持たない環境（索引のない単体テストと、本物の `IndexStore` が配線されるまでの session 用）。
    pub fn without_indexes(
        catalog: &'a dyn CatalogReader,
        storage: &'a dyn TableStore,
        snapshot: &'a Snapshot,
        session: &'a SessionInfo,
        runtime: &'a dyn RuntimeInfo,
        interrupts: &'a InterruptFlag,
        type_env: &'a TypeEnv<'a>,
    ) -> ExecEnv<'a> {
        ExecEnv {
            catalog,
            storage,
            indexes: &NoIndexStore,
            snapshot,
            session,
            runtime,
            interrupts,
            type_env,
            mem_limit: DEFAULT_QUERY_MEM_LIMIT,
            instr: None,
            wait: None,
        }
    }
}

/// すべての操作が `0A000` になる `IndexStore`（`RelHandle.indexes` が空の間は呼ばれない）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIndexStore;

fn no_indexes() -> Error {
    Error::not_supported("B+Tree indexes are not supported yet")
}

impl IndexStore for NoIndexStore {
    fn init_index(&self, _w: &WriteCtx, _index: &IndexHandle) -> Result<()> {
        Err(no_indexes())
    }

    fn insert(
        &self,
        _w: &WriteCtx,
        _index: &IndexHandle,
        _key: &[Datum],
        _tid: Tid,
        _check: UniqueCheck<'_>,
    ) -> Result<crate::storage::InsertOutcome> {
        Err(no_indexes())
    }

    fn build(
        &self,
        _w: &WriteCtx,
        _index: &IndexHandle,
        _entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>,
        _unique: BuildUnique,
    ) -> Result<BuildStats> {
        Err(no_indexes())
    }

    fn begin_scan(
        &self,
        _index: &IndexHandle,
        _keys: &ResolvedScanKeys,
        _dir: ScanDirection,
    ) -> Result<IndexScan> {
        Err(no_indexes())
    }

    fn scan_next(&self, _scan: &mut IndexScan) -> Result<Option<Tid>> {
        Err(no_indexes())
    }

    fn nblocks(&self, _index: &IndexHandle) -> Result<u32> {
        Err(no_indexes())
    }

    fn unlink_storage(&self, _index: &IndexHandle) -> Result<()> {
        Err(no_indexes())
    }
}

impl std::fmt::Debug for ExecCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("txn", &self.txn)
            .field("snapshot", &self.snapshot)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl<'a> ExecCtx<'a> {
    /// `params` を `n_params` 個の NULL で、`mem` を `mem_limit` で、`subplans` / `ctes` を `query` から作る。
    pub fn new(
        env: ExecEnv<'a>,
        txn: &'a mut Transaction,
        query: &'a PhysicalQuery,
    ) -> ExecCtx<'a> {
        ExecCtx {
            catalog: env.catalog,
            storage: env.storage,
            txn,
            snapshot: env.snapshot,
            session: env.session,
            interrupts: env.interrupts,
            runtime: env.runtime,
            indexes: env.indexes,
            query,
            params: vec![Datum::Null; query.n_params],
            mem: MemBudget::new(env.mem_limit),
            subplans: SubPlanStates::new(query),
            ctes: CteStates::new(query),
            type_env: env.type_env,
            instr: env.instr.cloned(),
            wait: env.wait,
            xact_snapshot: None,
        }
    }

    /// 実行時パラメータの値。範囲外は `Error::internal`。
    pub fn param(&self, id: ParamId) -> Result<&Datum> {
        self.params
            .get(usize::from(id.0))
            .ok_or_else(|| Error::internal(format!("parameter {} out of range", id.0)))
    }

    pub fn set_param(&mut self, id: ParamId, v: Datum) -> Result<()> {
        let slot = self
            .params
            .get_mut(usize::from(id.0))
            .ok_or_else(|| Error::internal(format!("parameter {} out of range", id.0)))?;
        *slot = v;
        Ok(())
    }

    /// 状態を取り出し、`ExecCtx` には空の状態を残す。
    pub fn take_state(&mut self) -> ExecState {
        ExecState {
            params: std::mem::take(&mut self.params),
            mem: std::mem::take(&mut self.mem),
            subplans: std::mem::take(&mut self.subplans),
            ctes: std::mem::take(&mut self.ctes),
        }
    }

    /// `take_state` で取り出した状態を戻す。
    pub fn restore_state(&mut self, state: ExecState) {
        self.params = state.params;
        self.mem = state.mem;
        self.subplans = state.subplans;
        self.ctes = state.ctes;
    }
}

impl<'a> ExecCtx<'a> {
    /// 関数・演算子・キャストの呼び出しに渡す文脈。借用を `'a` で返すので `&mut self` の呼び出しと衝突しない。
    pub fn eval_ctx(&self) -> EvalCtx<'a> {
        EvalCtx {
            session: self.session,
            catalog: self.catalog,
            runtime: self.runtime,
            type_env: self.type_env,
        }
    }

    /// The only way to get a [`WriteCtx`]: delegates to
    /// [`Transaction::write_ctx`].
    pub fn write_ctx(&mut self) -> Result<WriteCtx> {
        self.txn.write_ctx()
    }

    /// 停止（FATAL 57P01）・キャンセル・文の期限（57014）を検査する。1 行ごとに呼ぶ。
    pub fn check_interrupts(&self) -> Result<()> {
        self.interrupts.check()
    }
}

pub trait Executor {
    /// The next row, or `None` when exhausted. INSERT returns no rows; its
    /// count is available from the node (see `rows_affected`).
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>>;

    /// 初期状態に戻す（必須。既定実装なし）。`ctx.params` が前回から変わっていることがある。未開始のノードにも
    /// 呼べ、その場合は何もしなくてよい（`m4/02` §3.7.1、02-D16）。`rewind` 自体は行を読まない。
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()>;

    /// EXPLAIN ANALYZE の計測の受け口（`m4/10` §3.10、C-2）。既定は何もしない。述語が false の行を数える
    /// ノード（`Filter`・`SeqScan` など）が上書きして `instr.add_removed` を呼ぶ。
    fn set_counters(&mut self, _id: usize, _instr: &Rc<instrument::Instrumentation>) {}

    /// Number of rows inserted/updated/deleted so far (DML nodes only).
    fn rows_affected(&self) -> u64 {
        0
    }
}

pub type BoxedExecutor = Box<dyn Executor>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::FakeCatalog;
    use crate::error::{Severity, sqlstate};
    use crate::executor::nodes::test_util::FakeStore;
    use crate::txn::Xid;

    fn session() -> SessionInfo {
        SessionInfo {
            current_user: "u".into(),
            session_user: "u".into(),
            database: "postgres".into(),
            current_schema: Some("public".into()),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(4),
            xip: vec![],
            curcid: 0,
            own_xid: None,
        }
    }

    #[test]
    fn exec_ctx_new_sizes_params_and_budget() {
        use crate::expr::ParamId;
        let cat = FakeCatalog::new("postgres");
        let store = FakeStore::default();
        let mut txn = Transaction::new();
        let (info, snap, flag) = (session(), snapshot(), InterruptFlag::default());
        let mut query = PhysicalQuery::empty();
        query.n_params = 2;
        let type_env = TypeEnv::default();
        let mut env =
            ExecEnv::without_indexes(&cat, &store, &snap, &info, &NullRuntime, &flag, &type_env);
        env.mem_limit = 10;
        let mut ctx = ExecCtx::new(env, &mut txn, &query);
        assert_eq!(ctx.params, vec![Datum::Null, Datum::Null]);
        ctx.set_param(ParamId(1), Datum::Int4(7)).unwrap();
        assert_eq!(ctx.param(ParamId(1)).unwrap(), &Datum::Int4(7));
        assert!(ctx.param(ParamId(2)).is_err());
        assert!(ctx.set_param(ParamId(2), Datum::Null).is_err());
        assert!(ctx.mem.charge(11).is_err());
        assert!(ctx.subplans.is_empty() && ctx.ctes.is_empty());
        // 索引なしの環境の IndexStore は 0A000。
        let h = crate::storage::IndexHandle {
            oid: 1,
            locator: crate::storage::smgr::RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: crate::storage::smgr::RelFileNumber(1),
            },
            schema: "public".into(),
            name: "i".into(),
            table_name: "t".into(),
            unique: false,
            primary: false,
            columns: std::sync::Arc::from([]),
        };
        assert_eq!(
            ctx.indexes.nblocks(&h).unwrap_err().sqlstate,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
    }

    #[test]
    fn exec_state_is_taken_and_restored() {
        use crate::expr::ParamId;
        let cat = FakeCatalog::new("postgres");
        let store = FakeStore::default();
        let mut txn = Transaction::new();
        let (info, snap, flag) = (session(), snapshot(), InterruptFlag::default());
        let mut query = PhysicalQuery::empty();
        query.n_params = 1;
        let type_env = TypeEnv::default();
        let env =
            ExecEnv::without_indexes(&cat, &store, &snap, &info, &NullRuntime, &flag, &type_env);
        let mut ctx = ExecCtx::new(env, &mut txn, &query);
        ctx.set_param(ParamId(0), Datum::Int4(5)).unwrap();
        let state = ctx.take_state();
        assert_eq!(state.params, vec![Datum::Int4(5)]);
        assert!(ctx.params.is_empty());
        ctx.restore_state(state);
        assert_eq!(ctx.param(ParamId(0)).unwrap(), &Datum::Int4(5));
        let d = ExecState::default();
        assert!(d.params.is_empty() && d.subplans.is_empty() && d.ctes.is_empty());
        assert_eq!(d.mem.limit(), mem::DEFAULT_QUERY_MEM_LIMIT);
    }

    #[test]
    fn write_ctx_and_interrupts() {
        let cat = FakeCatalog::new("postgres");
        let store = FakeStore::default();
        let mut txn = Transaction::new();
        let info = session();
        let snap = snapshot();
        let flag = InterruptFlag::default();
        let query = PhysicalQuery::empty();
        let type_env = TypeEnv::default();
        let env =
            ExecEnv::without_indexes(&cat, &store, &snap, &info, &NullRuntime, &flag, &type_env);
        let mut ctx = ExecCtx::new(env, &mut txn, &query);
        // No XID yet: the writer lock was not taken.
        assert!(ctx.write_ctx().is_err());
        ctx.txn.xid = Some(Xid(5));
        assert_eq!(ctx.write_ctx().unwrap().xid, Xid(5));

        assert!(ctx.check_interrupts().is_ok());
        flag.request_terminate();
        let e = ctx.check_interrupts().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ADMIN_SHUTDOWN);
        assert_eq!(e.severity, Severity::Fatal);
    }
}
