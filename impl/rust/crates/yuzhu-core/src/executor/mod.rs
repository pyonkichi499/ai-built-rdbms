//! Volcano-style executor. (Owned by the executor implementer; `build` is
//! in `build.rs`.)

pub mod build;
pub mod eval;
pub mod nodes;

pub use build::build;
pub use eval::eval;

use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::interrupt::InterruptFlag;
use crate::storage::{TableStore, WriteCtx};
use crate::txn::{Snapshot, Transaction};
use crate::types::Row;

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
    /// `InterruptFlag::check` に委ねる（`pg_sleep` が 10ms ごとに呼ぶ）。
    fn check_interrupts(&self) -> Result<()>;
    /// 現在のトランザクションに割り当て済みの XID（未割り当てなら `None`）。
    /// `txid_current_if_assigned` が使う。
    fn current_xid(&self) -> Option<u64> {
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

impl ExecCtx<'_> {
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
    fn write_ctx_and_interrupts() {
        let cat = FakeCatalog::new("postgres");
        let store = FakeStore::default();
        let mut txn = Transaction::new();
        let info = session();
        let snap = snapshot();
        let flag = InterruptFlag::default();
        let mut ctx = ExecCtx {
            catalog: &cat,
            storage: &store,
            txn: &mut txn,
            snapshot: &snap,
            session: &info,
            interrupts: &flag,
            runtime: &NullRuntime,
        };
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
