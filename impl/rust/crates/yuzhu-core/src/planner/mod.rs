//! Planner: `BoundStatement` → `PhysicalQuery`（`m4/02` §3.5・§3.6、`m4/04`）。
//!
//! 処理の流れ（`plan`）:
//!
//! 1. [`build`]: Bound → 論理プラン（`Var` → `ColId`。`LogicalQuery::validate`）
//! 2. [`rules`]: 論理プランの書き換え（P0-e は恒等。各ルールの後に `validate`）
//! 3. [`physicalize`]: 論理 → 物理（`ColId` → 位置、`PhysicalQuery::validate`）
//!
//! SELECT の段の積み方（`build`）: 行の生成 → `Filter` → `Project`（全 targets。resjunk を含む）→ `Sort`
//! → `Project`（resjunk の除去）→ `Distinct` → `Limit`。

pub mod build;
mod check_const;
pub mod explain_tree;
pub mod index_select;
pub mod logical;
pub mod physical;
pub mod physicalize;
pub mod print;
pub mod rules;
pub mod size;
pub mod util;
pub mod validate;

pub use check_const::check_constant_exprs;

/// 再帰の深さの上限（実際の制限は `check_stack_depth` のスタック予算）。超えたら 54001 `stack depth limit exceeded`。
pub const MAX_PLAN_DEPTH: usize = 100_000;
/// これを超える内部結合の島は並べ替えない（構文順のまま。R7）。
pub const MAX_JOIN_ISLAND: usize = 64;

use self::logical::LogicalQuery;
use self::physical::PhysicalQuery;
use crate::analyzer::bound::BoundStatement;
use crate::catalog::CatalogReader;
use crate::error::{Error, Result};
use crate::executor::mem::DEFAULT_QUERY_MEM_LIMIT;
use crate::storage::TableStore;
use crate::types::TypeEnv;

/// 計画の文脈（文ごとに session が作る借用の束）。
#[derive(Clone, Copy)]
pub struct PlanEnv<'a> {
    pub catalog: &'a dyn CatalogReader,
    /// `nblocks` を問い合わせる（インデックス・結合の選択）。
    pub storage: &'a dyn TableStore,
    pub settings: &'a PlannerSettings,
    /// 定数畳み込みでリテラルのキャストを評価する。
    pub type_env: &'a TypeEnv<'a>,
    pub want_explain: bool,
    /// EXPLAIN VERBOSE（`want_explain` が false なら無視）。
    pub explain_verbose: bool,
}

/// `settings.rs` の値から作る（P0 は `Default`）。
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct PlannerSettings {
    pub enable_seqscan: bool,
    pub enable_indexscan: bool,
    pub enable_hashjoin: bool,
    pub enable_nestloop: bool,
    pub enable_hashagg: bool,
    pub enable_sort: bool,
    pub enable_material: bool,
    pub query_mem_limit: usize,
    /// `build`・各ルール・`physicalize` の後に `validate` を実行する。既定はデバッグビルドだけ。
    pub validate_plans: bool,
}

impl Default for PlannerSettings {
    fn default() -> Self {
        PlannerSettings {
            enable_seqscan: true,
            enable_indexscan: true,
            enable_hashjoin: true,
            enable_nestloop: true,
            enable_hashagg: true,
            enable_sort: true,
            enable_material: true,
            query_mem_limit: DEFAULT_QUERY_MEM_LIMIT,
            validate_plans: cfg!(debug_assertions),
        }
    }
}

/// `plan_golden` とデバッグ用: 各段階の論理プランと最後の物理プランを受け取る。
pub trait PlanTrace {
    /// `stage` は `"build"` とルールの名前（`"const_fold"` など）。
    fn logical(&mut self, stage: &str, q: &LogicalQuery);
    fn physical(&mut self, q: &PhysicalQuery);
}

/// 何もしない `PlanTrace`。
#[derive(Debug)]
pub struct NoTrace;

impl PlanTrace for NoTrace {
    fn logical(&mut self, _stage: &str, _q: &LogicalQuery) {}
    fn physical(&mut self, _q: &PhysicalQuery) {}
}

/// SELECT・INSERT・UPDATE・DELETE（と、その EXPLAIN）を計画する。DDL・COPY・CHECKPOINT は計画しない。
pub fn plan(stmt: &BoundStatement, env: &PlanEnv<'_>) -> Result<PhysicalQuery> {
    plan_traced(stmt, env, &mut NoTrace)
}

/// [`plan`] と同じで、各段階を `trace` に渡す。
pub fn plan_traced(
    stmt: &BoundStatement,
    env: &PlanEnv<'_>,
    trace: &mut dyn PlanTrace,
) -> Result<PhysicalQuery> {
    match stmt {
        BoundStatement::Select(_)
        | BoundStatement::Insert(_)
        | BoundStatement::Update(_)
        | BoundStatement::Delete(_) => {
            let mut lq = build::build_statement(stmt, env)?;
            if env.settings.validate_plans {
                validate::validate_logical(&lq, "build")?;
            }
            rules::run(&mut lq, env, trace)?;
            let pq = physicalize::physicalize(lq, env)?;
            if env.settings.validate_plans {
                pq.validate()?;
            }
            trace.physical(&pq);
            Ok(pq)
        }
        // EXPLAIN は内側の文を `want_explain = true` で計画する（整形・ANALYZE の実行は `explain` と session）。
        BoundStatement::Explain(e) => {
            if matches!(e.inner, BoundStatement::Explain(_)) {
                return Err(Error::internal("nested EXPLAIN reached the planner"));
            }
            plan_traced(
                &e.inner,
                &PlanEnv {
                    want_explain: true,
                    ..*env
                },
                trace,
            )
        }
        BoundStatement::Copy(_) | BoundStatement::Ddl(_) | BoundStatement::Checkpoint => Err(
            Error::internal("utility statements are executed by the session, not planned"),
        ),
    }
}

#[cfg(test)]
mod tests;

impl std::fmt::Debug for PlanEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanEnv")
            .field("settings", &self.settings)
            .field("want_explain", &self.want_explain)
            .field("explain_verbose", &self.explain_verbose)
            .finish_non_exhaustive()
    }
}
