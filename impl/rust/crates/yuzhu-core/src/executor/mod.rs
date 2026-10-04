//! Volcano-style executor. (Owned by the executor implementer; `build` is a
//! stub that keeps the crate building.)

pub mod eval;
pub mod nodes;

pub use eval::eval;

use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::planner::PhysicalPlan;
use crate::storage::TableStore;
use crate::txn::Transaction;
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

/// Everything an executor needs at run time. Passed to every `next` call
/// so executor structs carry no lifetimes.
pub struct ExecCtx<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub txn: &'a mut Transaction,
    pub session: &'a SessionInfo,
}

impl std::fmt::Debug for ExecCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("txn", &self.txn)
            .field("session", &self.session)
            .finish_non_exhaustive()
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

/// Builds an executor tree for `plan`.
pub fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    use nodes::{
        DistinctExec, FilterExec, InsertExec, LimitExec, ProjectExec, ResultExec, SeqScanExec,
        SortExec, ValuesExec,
    };
    match plan {
        PhysicalPlan::Result { exprs } => Box::new(ResultExec::new(exprs.clone())),
        PhysicalPlan::Values { rows } => Box::new(ValuesExec::new(rows.clone())),
        PhysicalPlan::SeqScan { table_oid, .. } => Box::new(SeqScanExec::new(*table_oid)),
        PhysicalPlan::Filter { input, predicate } => {
            Box::new(FilterExec::new(build(input), predicate.clone()))
        }
        PhysicalPlan::Project { input, exprs } => {
            Box::new(ProjectExec::new(build(input), exprs.clone()))
        }
        PhysicalPlan::Sort { input, keys } => Box::new(SortExec::new(build(input), keys.clone())),
        PhysicalPlan::Distinct { input } => Box::new(DistinctExec::new(build(input))),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => Box::new(LimitExec::new(build(input), limit.clone(), offset.clone())),
        PhysicalPlan::Insert {
            table_oid,
            input,
            column_map,
            defaults,
            checks,
            not_null,
        } => Box::new(InsertExec::new(
            *table_oid,
            build(input),
            column_map.clone(),
            defaults.clone(),
            checks.clone(),
            not_null.clone(),
        )),
    }
}
