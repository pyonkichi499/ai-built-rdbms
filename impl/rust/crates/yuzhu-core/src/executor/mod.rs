//! Volcano-style executor. (Owned by the executor implementer; `build` is
//! in `build.rs`.)

pub mod build;
pub mod eval;
pub mod nodes;

pub use build::build;
pub use eval::eval;

use crate::catalog::CatalogReader;
use crate::error::{Error, Result, Severity, sqlstate};
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

/// What expression evaluation may look at: the session and the catalog
/// (`FnKind::Context` functions such as `pg_get_userbyid` need the latter).
/// 担当 H2 が `eval_expr` をこれを受け取る形にする（`m2.md` §4.6）。
#[derive(Clone, Copy)]
pub struct EvalCtx<'a> {
    pub session: &'a SessionInfo,
    pub catalog: &'a dyn CatalogReader,
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

    /// `57P01` (FATAL) if a shutdown was requested. Call once per row.
    pub fn check_interrupts(&self) -> Result<()> {
        if self.interrupts.is_terminate_requested() {
            return Err(Error::new(
                sqlstate::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            )
            .with_severity(Severity::Fatal));
        }
        Ok(())
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
