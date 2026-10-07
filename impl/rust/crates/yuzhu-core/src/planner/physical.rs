//! 物理プラン（`m4/00-contracts.md` §9、`m4/02-pipeline-refactor.md` §3.6）。
//!
//! 旧 `planner::plan` の `PhysicalPlan`（`BoundExpr` を持つ M1〜M3 の型）と同じ名前の型を持つので、
//! P0-e まで別のモジュールに置いて共存させる。列は行内の位置（`PhysCol::Local`）か実行時パラメータ
//! （`PhysCol::Param`）で参照する。不変条件 P1〜P11 は [`PhysicalQuery::validate`] が検査する。

#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::{BTreeSet, HashSet};

use super::logical::JoinKind;
use crate::analyzer::bound::OutputColumn;
use crate::analyzer::bound::SetOpKind;
use crate::catalog::{AggKind, BuiltinFunction, SystemColumn};
use crate::error::{Error, Result};
use crate::expr::{Expr, ExprKind, ParamId, PhysCol, SubLinkKind, SubPlanId};
use crate::storage::{IndexHandle, RelHandle};
use crate::types::SqlType;

pub use crate::storage::ScanDirection;

pub type PhysExpr = Expr<PhysCol, SubPlanId>;

/// CHECK 制約（型は bool、NULL は通す）。
#[derive(Debug, Clone)]
pub struct PhysCheck {
    pub name: String,
    pub expr: PhysExpr,
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: PhysExpr,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub struct PhysAgg {
    pub kind: AggKind,
    pub arg_types: Vec<SqlType>,
    pub args: Vec<PhysExpr>,
    pub distinct: bool,
    pub filter: Option<PhysExpr>,
    /// `agg(... ORDER BY ...)`。空なら入力順。
    pub order_by: Vec<SortKey>,
    pub result: SqlType,
}

#[derive(Debug, Clone)]
pub enum IndexScanKey {
    Eq(PhysExpr),
    IsNull,
}

#[derive(Debug, Clone)]
pub struct RangeBound {
    pub expr: PhysExpr,
    pub inclusive: bool,
}

/// 先頭から `eq.len()` 個の列が等値（または IS NULL）、次の列に `lower` / `upper` の範囲。
#[derive(Debug, Clone, Default)]
pub struct IndexScanKeys {
    pub eq: Vec<IndexScanKey>,
    pub lower: Option<RangeBound>,
    pub upper: Option<RangeBound>,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum PhysicalPlan {
    Result {
        exprs: Vec<PhysExpr>,
        one_time_filter: Option<PhysExpr>,
    },
    Values {
        rows: Vec<Vec<PhysExpr>>,
    },
    /// 出力 = ユーザー列（attnum 順）++ `system_columns`。`filter` は走査中に評価する。
    SeqScan {
        rel: RelHandle,
        columns: Vec<SqlType>,
        system_columns: Vec<SystemColumn>,
        filter: Option<PhysExpr>,
    },
    /// 出力の形は `SeqScan` と同じ。`keys` で TID を集め、ヒープを引いて可視性を判定し、`filter` で再評価する。
    IndexScan {
        rel: RelHandle,
        index: IndexHandle,
        keys: IndexScanKeys,
        direction: ScanDirection,
        columns: Vec<SqlType>,
        system_columns: Vec<SystemColumn>,
        filter: Option<PhysExpr>,
    },
    FunctionScan {
        func: &'static BuiltinFunction,
        args: Vec<PhysExpr>,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: PhysExpr,
    },
    Project {
        input: Box<PhysicalPlan>,
        exprs: Vec<PhysExpr>,
    },
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<SortKey>,
    },
    /// 入力は整列済み。`key_cols`（入力の列位置。順不同）の値の組が前の行と違う最初の行だけ通す
    /// （DISTINCT ON）。
    Unique {
        input: Box<PhysicalPlan>,
        key_cols: Vec<usize>,
    },
    Distinct {
        input: Box<PhysicalPlan>,
    },
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<PhysExpr>,
        offset: Option<PhysExpr>,
    },
    Materialize {
        input: Box<PhysicalPlan>,
    },
    /// 出力は `outer ++ inner`（Semi / Anti は outer だけ）。`outer` は論理プランの left で固定。
    NestedLoopJoin {
        kind: JoinKind,
        outer: Box<PhysicalPlan>,
        inner: Box<PhysicalPlan>,
        join_filter: Option<PhysExpr>,
        outer_width: usize,
        inner_width: usize,
    },
    /// `inner` を `params` を設定して `rewind` し直す（インデックス付き NLJ）。
    NestedLoopParam {
        kind: JoinKind,
        outer: Box<PhysicalPlan>,
        inner: Box<PhysicalPlan>,
        params: Vec<(ParamId, PhysExpr)>,
        join_filter: Option<PhysExpr>,
        outer_width: usize,
        inner_width: usize,
    },
    /// `build_is_left = true` なら左がビルド側（出力の並びは変えない）。
    HashJoin {
        kind: JoinKind,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        left_keys: Vec<PhysExpr>,
        right_keys: Vec<PhysExpr>,
        key_types: Vec<SqlType>,
        residual: Option<PhysExpr>,
        build_is_left: bool,
        left_width: usize,
        right_width: usize,
    },
    Aggregate {
        input: Box<PhysicalPlan>,
        aggs: Vec<PhysAgg>,
    },
    HashAggregate {
        input: Box<PhysicalPlan>,
        keys: Vec<PhysExpr>,
        key_types: Vec<SqlType>,
        aggs: Vec<PhysAgg>,
    },
    GroupAggregate {
        input: Box<PhysicalPlan>,
        keys: Vec<PhysExpr>,
        key_types: Vec<SqlType>,
        aggs: Vec<PhysAgg>,
    },
    Append {
        inputs: Vec<PhysicalPlan>,
    },
    HashSetOp {
        op: SetOpKind,
        all: bool,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        key_types: Vec<SqlType>,
    },
    /// `PhysicalQuery.ctes[cte]` を最初の参照で全行溜め、各参照が自分のカーソルで読む。
    CteScan {
        cte: usize,
    },
    Insert {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
        column_map: Vec<Option<usize>>,
        defaults: Vec<Option<PhysExpr>>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        table_name: String,
        returning: Option<Vec<PhysExpr>>,
    },
    /// 入力の形: 対象表のユーザー列(n) ++ ctid ++ 新しい値(`assigned.len()`)。
    Update {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
        n_user_cols: usize,
        /// `(attnum - 1, 入力の列位置)`。
        assigned: Vec<(usize, usize)>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        table_name: String,
        returning: Option<Vec<PhysExpr>>,
    },
    /// 入力の形: 対象表のユーザー列 ++ ctid。
    Delete {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
        n_user_cols: usize,
        returning: Option<Vec<PhysExpr>>,
    },
}

/// 問い合わせ全体。
#[derive(Debug, Clone)]
pub struct PhysicalQuery {
    pub root: PhysicalPlan,
    pub subplans: Vec<SubPlanDef>,
    /// MATERIALIZED の CTE（および参照が複数ある CTE）。`CteScan { cte }` が添字。
    pub ctes: Vec<PhysicalPlan>,
    pub n_params: usize,
    /// 可視出力列。
    pub output: Vec<OutputColumn>,
    /// `want_explain` のときだけ。表示用の木（`root` と同形とは限らない。11 §7.1 の C-1）。
    pub explain: Option<ExplainNode>,
}

#[derive(Debug, Clone)]
pub struct SubPlanDef {
    pub plan: PhysicalPlan,
    pub kind: SubLinkKind,
    /// Any / All の比較式（`SubLinkOutput` を含む）。
    pub test: Option<PhysExpr>,
    /// 実行前に外側の行から計算して `ctx.params` に設定する値。
    pub params: Vec<(ParamId, PhysExpr)>,
    pub strategy: SubPlanStrategy,
    pub explain: Option<ExplainNode>,
}

#[derive(Debug, Clone)]
pub enum SubPlanStrategy {
    /// 外側の行ごとに `rewind` して実行（相関あり）。
    Rescan,
    /// 1 回だけ実行して結果を保持（相関なし。PostgreSQL の InitPlan）。
    InitOnce,
    /// 非相関の ANY で test が等値のとき: 1 回だけ実行してハッシュ集合にする。
    Hashed {
        probe_keys: Vec<PhysExpr>,
        build_keys: Vec<PhysExpr>,
    },
}

/// EXPLAIN の 1 ノード。表示用の木（定義の正本は `m4/10-explain-copy-compat.md` §3.2。11 §7.1 の C-1）。
#[derive(Debug, Clone)]
pub struct ExplainNode {
    /// `Seq Scan on t`、`Hash Join` など。コストと actual は含まない。
    pub title: String,
    /// 詳細行。出す順に並べる。
    pub details: Vec<ExplainDetail>,
    /// VERBOSE のときだけ作る `Output:` 行の各要素。
    pub output: Vec<String>,
    /// 通常の子とラベルつきの子（InitPlan / SubPlan / CTE）を、出力する順に並べる。
    pub children: Vec<ExplainChild>,
    /// 計測値の持ち主（`assign_exec_ids` の先行順の通し番号）。合成ノード（Hash など）は中身の番号を借りる。
    pub exec_id: usize,
    /// コスト欄の width。
    pub width: u32,
}

#[derive(Debug, Clone)]
pub struct ExplainDetail {
    /// `Filter`、`Hash Cond`、`Sort Key` など。
    pub label: &'static str,
    pub text: String,
    /// ANALYZE のとき、この行の直後に `Rows Removed by ...` を出す元。
    pub removed: Option<RemovedRows>,
}

#[derive(Debug, Clone)]
pub struct RemovedRows {
    pub label: &'static str,
    /// 合計する計測値（exec_id, どちらのカウンタか）。
    pub sources: Vec<(usize, FilterCounter)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterCounter {
    Filter,
    JoinFilter,
}

#[derive(Debug, Clone)]
pub struct ExplainChild {
    /// `InitPlan 1` など。
    pub label: Option<String>,
    pub node: ExplainNode,
}

/// `assign_exec_ids` の結果: 根は 0、続けて `subplans` の各プラン、最後に `ctes` の各プランの先頭の番号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecIds {
    pub root: usize,
    pub subplans: Vec<usize>,
    pub ctes: Vec<usize>,
    /// 番号の総数。
    pub total: usize,
}

/// 計測の通し番号を振る（先行順。根 → `subplans` の昇順 → `ctes` の昇順）。L2 と
/// `executor::build_instrumented` が共有する唯一の関数。
pub fn assign_exec_ids(q: &PhysicalQuery) -> ExecIds {
    let mut next = 0usize;
    let root = next;
    next += q.root.node_count();
    let mut subplans = Vec::with_capacity(q.subplans.len());
    for s in &q.subplans {
        subplans.push(next);
        next += s.plan.node_count();
    }
    let mut ctes = Vec::with_capacity(q.ctes.len());
    for c in &q.ctes {
        ctes.push(next);
        next += c.node_count();
    }
    ExecIds {
        root,
        subplans,
        ctes,
        total: next,
    }
}

fn some_expr_uses_params(e: &PhysExpr) -> bool {
    e.any(&mut |n| {
        matches!(
            n.kind,
            ExprKind::Column(PhysCol::Param(_)) | ExprKind::SubLink { .. }
        )
    })
}

impl PhysExpr {
    /// 部分木のどこかに `PhysCol::Param` がある、または `SubLink` が 1 つでもある（保守的。02-D7）。
    pub fn uses_params(&self) -> bool {
        some_expr_uses_params(self)
    }
}

#[allow(clippy::ref_option)]
fn opt<'a>(e: &'a Option<PhysExpr>, w: usize, out: &mut Vec<(&'a PhysExpr, usize)>) {
    if let Some(e) = e {
        out.push((e, w));
    }
}

fn agg_exprs<'a>(aggs: &'a [PhysAgg], w: usize, out: &mut Vec<(&'a PhysExpr, usize)>) {
    for a in aggs {
        out.extend(a.args.iter().map(|e| (e, w)));
        opt(&a.filter, w, out);
        out.extend(a.order_by.iter().map(|k| (&k.expr, w)));
    }
}

impl PhysicalPlan {
    /// 子のプラン。実行の呼び出し順 = EXPLAIN の表示順 = 番号付けの順。単項: `[input]`、
    /// NestedLoop*: `[outer, inner]`、HashJoin: `[probe, build]`、Append: `inputs` の順、
    /// HashSetOp: `[left, right]`、葉: `[]`。SubPlan と CTE は含めない。
    pub fn children(&self) -> Vec<&PhysicalPlan> {
        use PhysicalPlan as P;
        match self {
            P::Result { .. }
            | P::Values { .. }
            | P::SeqScan { .. }
            | P::IndexScan { .. }
            | P::FunctionScan { .. }
            | P::CteScan { .. } => vec![],
            P::Filter { input, .. }
            | P::Project { input, .. }
            | P::Sort { input, .. }
            | P::Unique { input, .. }
            | P::Distinct { input }
            | P::Limit { input, .. }
            | P::Materialize { input }
            | P::Aggregate { input, .. }
            | P::HashAggregate { input, .. }
            | P::GroupAggregate { input, .. }
            | P::Insert { input, .. }
            | P::Update { input, .. }
            | P::Delete { input, .. } => vec![&**input],
            P::NestedLoopJoin { outer, inner, .. } | P::NestedLoopParam { outer, inner, .. } => {
                vec![&**outer, &**inner]
            }
            P::HashJoin {
                left,
                right,
                build_is_left,
                ..
            } => {
                if *build_is_left {
                    vec![&**right, &**left]
                } else {
                    vec![&**left, &**right]
                }
            }
            P::Append { inputs } => inputs.iter().collect(),
            P::HashSetOp { left, right, .. } => vec![&**left, &**right],
        }
    }

    /// この木のノード数（`children()` で降りる範囲）。
    pub fn node_count(&self) -> usize {
        1 + self
            .children()
            .iter()
            .map(|c| c.node_count())
            .sum::<usize>()
    }

    /// 出力行の幅（`m4/02` §3.6.1）。`ctes` は `CteScan` の幅を引くために要る。
    pub fn width(&self, ctes: &[PhysicalPlan]) -> usize {
        use PhysicalPlan as P;
        match self {
            P::Result { exprs, .. } | P::Project { exprs, .. } => exprs.len(),
            P::Values { rows } => rows.first().map_or(0, Vec::len),
            P::SeqScan {
                columns,
                system_columns,
                ..
            }
            | P::IndexScan {
                columns,
                system_columns,
                ..
            } => columns.len() + system_columns.len(),
            P::FunctionScan { .. } => 1,
            P::Filter { input, .. }
            | P::Sort { input, .. }
            | P::Unique { input, .. }
            | P::Distinct { input }
            | P::Limit { input, .. }
            | P::Materialize { input } => input.width(ctes),
            P::NestedLoopJoin {
                kind,
                outer_width,
                inner_width,
                ..
            }
            | P::NestedLoopParam {
                kind,
                outer_width,
                inner_width,
                ..
            } => match kind {
                JoinKind::Semi | JoinKind::Anti => *outer_width,
                _ => outer_width + inner_width,
            },
            P::HashJoin {
                kind,
                left_width,
                right_width,
                ..
            } => match kind {
                JoinKind::Semi | JoinKind::Anti => *left_width,
                _ => left_width + right_width,
            },
            P::Aggregate { aggs, .. } => aggs.len(),
            P::HashAggregate { keys, aggs, .. } | P::GroupAggregate { keys, aggs, .. } => {
                keys.len() + aggs.len()
            }
            P::Append { inputs } => inputs.first().map_or(0, |p| p.width(ctes)),
            P::HashSetOp { left, .. } => left.width(ctes),
            P::CteScan { cte } => ctes.get(*cte).map_or(0, |p| p.width(ctes)),
            P::Insert { returning, .. }
            | P::Update { returning, .. }
            | P::Delete { returning, .. } => returning.as_ref().map_or(0, Vec::len),
        }
    }

    /// ノードが直接持つ式と、その文脈行の幅（`m4/02` §3.6.2）。`validate` と `uses_params` の唯一の情報源。
    /// `SubPlanDef` の式（`params` など）は含まない。
    pub fn exprs_with_width(&self, ctes: &[PhysicalPlan]) -> Vec<(&PhysExpr, usize)> {
        use PhysicalPlan as P;
        let mut out: Vec<(&PhysExpr, usize)> = Vec::new();
        match self {
            P::Result {
                exprs,
                one_time_filter,
            } => {
                out.extend(exprs.iter().map(|e| (e, 0)));
                opt(one_time_filter, 0, &mut out);
            }
            P::Values { rows } => out.extend(rows.iter().flatten().map(|e| (e, 0))),
            P::SeqScan {
                columns,
                system_columns,
                filter,
                ..
            } => opt(filter, columns.len() + system_columns.len(), &mut out),
            P::IndexScan {
                keys,
                columns,
                system_columns,
                filter,
                ..
            } => {
                for k in &keys.eq {
                    if let IndexScanKey::Eq(e) = k {
                        out.push((e, 0));
                    }
                }
                out.extend(keys.lower.iter().map(|b| (&b.expr, 0)));
                out.extend(keys.upper.iter().map(|b| (&b.expr, 0)));
                opt(filter, columns.len() + system_columns.len(), &mut out);
            }
            P::FunctionScan { args, .. } => out.extend(args.iter().map(|e| (e, 0))),
            P::Filter { input, predicate } => out.push((predicate, input.width(ctes))),
            P::Project { input, exprs } => {
                let w = input.width(ctes);
                out.extend(exprs.iter().map(|e| (e, w)));
            }
            P::Sort { input, keys } => {
                let w = input.width(ctes);
                out.extend(keys.iter().map(|k| (&k.expr, w)));
            }
            P::Limit { limit, offset, .. } => {
                opt(limit, 0, &mut out);
                opt(offset, 0, &mut out);
            }
            P::NestedLoopJoin {
                join_filter,
                outer_width,
                inner_width,
                ..
            } => opt(join_filter, outer_width + inner_width, &mut out),
            P::NestedLoopParam {
                params,
                join_filter,
                outer_width,
                inner_width,
                ..
            } => {
                out.extend(params.iter().map(|(_, e)| (e, *outer_width)));
                opt(join_filter, outer_width + inner_width, &mut out);
            }
            P::HashJoin {
                left_keys,
                right_keys,
                residual,
                left_width,
                right_width,
                ..
            } => {
                out.extend(left_keys.iter().map(|e| (e, *left_width)));
                out.extend(right_keys.iter().map(|e| (e, *right_width)));
                opt(residual, left_width + right_width, &mut out);
            }
            P::Aggregate { input, aggs } => agg_exprs(aggs, input.width(ctes), &mut out),
            P::HashAggregate {
                input, keys, aggs, ..
            }
            | P::GroupAggregate {
                input, keys, aggs, ..
            } => {
                let w = input.width(ctes);
                out.extend(keys.iter().map(|e| (e, w)));
                agg_exprs(aggs, w, &mut out);
            }
            P::Insert {
                defaults,
                checks,
                returning,
                column_map,
                ..
            } => {
                out.extend(defaults.iter().flatten().map(|e| (e, 0)));
                let w = column_map.len();
                out.extend(checks.iter().map(|c| (&c.expr, w)));
                out.extend(returning.iter().flatten().map(|e| (e, w)));
            }
            P::Update {
                checks,
                returning,
                n_user_cols,
                ..
            } => {
                out.extend(checks.iter().map(|c| (&c.expr, *n_user_cols)));
                out.extend(returning.iter().flatten().map(|e| (e, *n_user_cols)));
            }
            P::Delete {
                returning,
                n_user_cols,
                ..
            } => out.extend(returning.iter().flatten().map(|e| (e, *n_user_cols))),
            P::Unique { .. }
            | P::Distinct { .. }
            | P::Materialize { .. }
            | P::Append { .. }
            | P::HashSetOp { .. }
            | P::CteScan { .. } => {}
        }
        out
    }

    /// 木のどこかのノードの式に `PhysCol::Param` がある、または `SubLink` が 1 つでもある（02-D7。
    /// `NestedLoopParam.params` の式の中の `Param` も数える）。
    pub fn uses_params(&self) -> bool {
        // 幅は使わないので ctes は空でよい（CteScan の幅は式に影響しない）。
        self.exprs_with_width(&[])
            .iter()
            .any(|(e, _)| some_expr_uses_params(e))
            || self.children().iter().any(|c| c.uses_params())
    }
}

impl PhysicalQuery {
    /// テスト・EXPLAIN 用: 副問い合わせも CTE も持たない問い合わせ。
    pub fn single(root: PhysicalPlan, output: Vec<OutputColumn>) -> PhysicalQuery {
        PhysicalQuery {
            root,
            subplans: Vec::new(),
            ctes: Vec::new(),
            n_params: 0,
            output,
            explain: None,
        }
    }

    /// 行を出さない問い合わせ（`Result { exprs: [], one_time_filter: false }`）。
    pub fn empty() -> PhysicalQuery {
        PhysicalQuery::single(
            PhysicalPlan::Result {
                exprs: Vec::new(),
                one_time_filter: Some(PhysExpr::bool_lit(false)),
            },
            Vec::new(),
        )
    }
}

// ----- 検証（P1〜P11）---------------------------------------------------------

fn bad(rule: &str, msg: impl std::fmt::Display) -> Error {
    Error::internal(format!("invalid physical plan [{rule}] {msg}"))
}

/// `PhysicalPlan::validate` の文脈。
#[derive(Debug, Clone, Copy)]
pub struct PhysValidateCx<'a> {
    pub query: &'a PhysicalQuery,
}

struct Vstate<'a> {
    q: &'a PhysicalQuery,
    refs: Vec<u32>,
    binders: HashSet<ParamId>,
}

impl Vstate<'_> {
    fn bind(&mut self, p: ParamId) -> Result<()> {
        if usize::from(p.0) >= self.q.n_params {
            return Err(bad(
                "P2",
                format!("ParamId {} >= n_params {}", p.0, self.q.n_params),
            ));
        }
        if !self.binders.insert(p) {
            return Err(bad("P2", format!("ParamId {} is bound twice", p.0)));
        }
        Ok(())
    }

    fn expr(&mut self, e: &PhysExpr, w: usize, bound: &BTreeSet<ParamId>) -> Result<()> {
        match &e.kind {
            ExprKind::Column(PhysCol::Local(i)) => {
                if *i >= w {
                    return Err(bad(
                        "P1",
                        format!("Local({i}) but the context row has {w} columns"),
                    ));
                }
                Ok(())
            }
            ExprKind::Column(PhysCol::Param(p)) => {
                if usize::from(p.0) >= self.q.n_params {
                    return Err(bad(
                        "P2",
                        format!("Param({}) >= n_params {}", p.0, self.q.n_params),
                    ));
                }
                if !bound.contains(p) {
                    return Err(bad("P2", format!("Param({}) is not bound here", p.0)));
                }
                Ok(())
            }
            ExprKind::Aggregate(_) => Err(bad("P5", "aggregate expression in a physical plan")),
            ExprKind::SubLinkOutput(_) => Err(bad("P4", "SubLinkOutput outside SubPlanDef.test")),
            ExprKind::SubLink { kind, test, query } => {
                if test.is_some() {
                    return Err(bad("P4", "SubLink.test must be None in a physical plan"));
                }
                self.subplan(*query, *kind, w, bound)
            }
            _ => {
                for c in e.children() {
                    self.expr(c, w, bound)?;
                }
                Ok(())
            }
        }
    }

    fn subplan(
        &mut self,
        id: SubPlanId,
        kind: SubLinkKind,
        w: usize,
        bound: &BTreeSet<ParamId>,
    ) -> Result<()> {
        let i = usize::from(id.0);
        let Some(def) = self.q.subplans.get(i) else {
            return Err(bad("P3", format!("SubPlanId {i} out of range")));
        };
        self.refs[i] += 1;
        if self.refs[i] > 1 {
            return Err(bad(
                "P3",
                format!("SubPlanId {i} is referenced more than once"),
            ));
        }
        if def.kind != kind {
            return Err(bad(
                "P3",
                format!(
                    "SubPlanDef {i} kind {:?} differs from SubLink {kind:?}",
                    def.kind
                ),
            ));
        }
        let needs_test = matches!(kind, SubLinkKind::Any | SubLinkKind::All);
        if needs_test != def.test.is_some() {
            return Err(bad(
                "P4",
                format!("SubPlanDef {i}: test presence does not match kind {kind:?}"),
            ));
        }
        for (_, e) in &def.params {
            self.expr(e, w, bound)?;
        }
        let mut inner = BTreeSet::new();
        let rescan = matches!(def.strategy, SubPlanStrategy::Rescan);
        if rescan {
            inner.clone_from(bound);
        }
        for (p, _) in &def.params {
            self.bind(*p)?;
            inner.insert(*p);
        }
        let sw = def.plan.width(&self.q.ctes);
        match &def.strategy {
            SubPlanStrategy::Rescan => {}
            SubPlanStrategy::InitOnce => {
                if !def.params.is_empty() {
                    return Err(bad("P4", format!("InitOnce subplan {i} has params")));
                }
            }
            SubPlanStrategy::Hashed {
                probe_keys,
                build_keys,
            } => {
                if kind != SubLinkKind::Any
                    || !def.params.is_empty()
                    || probe_keys.len() != build_keys.len()
                {
                    return Err(bad(
                        "P4",
                        format!("Hashed subplan {i} violates its conditions"),
                    ));
                }
                for k in probe_keys {
                    self.expr(k, w, bound)?;
                }
                for k in build_keys {
                    self.expr(k, sw, &BTreeSet::new())?;
                }
            }
        }
        if let Some(t) = &def.test {
            self.test_expr(t, w, sw, bound, i)?;
        }
        self.plan(&def.plan, &inner, false)
    }

    /// `SubPlanDef.test`: `Local` は文脈行 + 副問い合わせの行、`SubLinkOutput(i)` は `i` < 副問い合わせの幅。
    fn test_expr(
        &mut self,
        e: &PhysExpr,
        w: usize,
        sw: usize,
        bound: &BTreeSet<ParamId>,
        id: usize,
    ) -> Result<()> {
        match &e.kind {
            ExprKind::SubLinkOutput(i) => {
                if usize::from(*i) >= sw {
                    return Err(bad(
                        "P4",
                        format!("subplan {id}: SubLinkOutput({i}) >= width {sw}"),
                    ));
                }
                Ok(())
            }
            ExprKind::Column(_) | ExprKind::Aggregate(_) | ExprKind::SubLink { .. } => {
                self.expr(e, w, bound)
            }
            _ => {
                for c in e.children() {
                    self.test_expr(c, w, sw, bound, id)?;
                }
                Ok(())
            }
        }
    }

    fn plan(&mut self, p: &PhysicalPlan, bound: &BTreeSet<ParamId>, is_root: bool) -> Result<()> {
        use PhysicalPlan as P;
        if !is_root && matches!(p, P::Insert { .. } | P::Update { .. } | P::Delete { .. }) {
            return Err(bad("P10", "DML node below the root"));
        }
        let ctes = &self.q.ctes;
        for (e, w) in p.exprs_with_width(ctes) {
            self.expr(e, w, bound)?;
        }
        self.node_rules(p)?;
        if let P::NestedLoopParam {
            outer,
            inner,
            params,
            ..
        } = p
        {
            self.plan(outer, bound, false)?;
            let mut b = bound.clone();
            for (pid, _) in params {
                self.bind(*pid)?;
                b.insert(*pid);
            }
            return self.plan(inner, &b, false);
        }
        for c in p.children() {
            self.plan(c, bound, false)?;
        }
        Ok(())
    }

    /// P6・P7・P9 とノードごとの幅の整合。
    fn node_rules(&self, p: &PhysicalPlan) -> Result<()> {
        use PhysicalPlan as P;
        let ctes = &self.q.ctes;
        match p {
            P::NestedLoopJoin {
                kind,
                outer,
                inner,
                outer_width,
                inner_width,
                ..
            }
            | P::NestedLoopParam {
                kind,
                outer,
                inner,
                outer_width,
                inner_width,
                ..
            } => {
                if *outer_width != outer.width(ctes) || *inner_width != inner.width(ctes) {
                    return Err(bad("P6", "nested loop width differs from its inputs"));
                }
                if *kind == JoinKind::Full {
                    return Err(bad("P7", "nested loop join cannot be FULL"));
                }
            }
            P::HashJoin {
                left,
                right,
                left_keys,
                right_keys,
                key_types,
                left_width,
                right_width,
                ..
            } => {
                if *left_width != left.width(ctes) || *right_width != right.width(ctes) {
                    return Err(bad("P6", "hash join width differs from its inputs"));
                }
                if left_keys.len() != right_keys.len() || left_keys.len() != key_types.len() {
                    return Err(bad("P6", "hash join key counts differ"));
                }
            }
            P::Append { inputs } => {
                let w = inputs.first().map(|i| i.width(ctes));
                if inputs.iter().any(|i| Some(i.width(ctes)) != w) {
                    return Err(bad("P6", "Append inputs have different widths"));
                }
            }
            P::HashSetOp { left, right, .. } => {
                if left.width(ctes) != right.width(ctes) {
                    return Err(bad("P6", "HashSetOp inputs have different widths"));
                }
            }
            P::Unique { input, key_cols } => {
                let w = input.width(ctes);
                if key_cols.iter().any(|c| *c >= w) {
                    return Err(bad("P6", "Unique key column outside the input"));
                }
            }
            P::Insert {
                input, column_map, ..
            } => {
                let w = input.width(ctes);
                if column_map.iter().flatten().any(|c| *c >= w) {
                    return Err(bad("P6", "Insert column_map refers outside the input"));
                }
            }
            P::Update {
                input,
                n_user_cols,
                assigned,
                ..
            } => {
                if input.width(ctes) != n_user_cols + 1 + assigned.len() {
                    return Err(bad(
                        "P6",
                        "Update input width differs from n_user_cols + 1 + assigned",
                    ));
                }
                if assigned
                    .iter()
                    .enumerate()
                    .any(|(k, (_, pos))| *pos != n_user_cols + 1 + k)
                {
                    return Err(bad(
                        "P6",
                        "Update assigned positions are not n_user_cols + 1 + k",
                    ));
                }
            }
            P::Delete {
                input, n_user_cols, ..
            } => {
                if input.width(ctes) != n_user_cols + 1 {
                    return Err(bad("P6", "Delete input width differs from n_user_cols + 1"));
                }
            }
            P::SeqScan { rel, columns, .. } => {
                if columns.len() != rel.desc.attrs.len() {
                    return Err(bad("P6", "SeqScan columns differ from the table's columns"));
                }
            }
            P::IndexScan {
                rel,
                index,
                keys,
                columns,
                ..
            } => {
                if columns.len() != rel.desc.attrs.len() {
                    return Err(bad(
                        "P6",
                        "IndexScan columns differ from the table's columns",
                    ));
                }
                let n = index.columns.len();
                if keys.eq.len() > n
                    || ((keys.lower.is_some() || keys.upper.is_some()) && keys.eq.len() >= n)
                {
                    return Err(bad("P9", "IndexScan keys exceed the index columns"));
                }
            }
            P::Values { rows } => {
                if rows.is_empty() || rows.iter().any(|r| r.len() != rows[0].len()) {
                    return Err(bad("P7", "Values needs at least one row and equal widths"));
                }
            }
            P::CteScan { cte } if *cte >= ctes.len() => {
                return Err(bad("P8", format!("CteScan {cte} out of range")));
            }
            _ => {}
        }
        Ok(())
    }
}

impl PhysicalPlan {
    /// このプランを根として検査する（副問い合わせの参照は数えるが、全参照の完全性は
    /// [`PhysicalQuery::validate`] が見る）。
    pub fn validate(&self, cx: &PhysValidateCx<'_>) -> Result<()> {
        let mut st = Vstate {
            q: cx.query,
            refs: vec![0; cx.query.subplans.len()],
            binders: HashSet::new(),
        };
        st.plan(self, &BTreeSet::new(), true)
    }
}

impl PhysicalQuery {
    /// P1〜P11。違反は `Error::internal("invalid physical plan [P1] ...")`（XX000）。純粋関数。
    pub fn validate(&self) -> Result<()> {
        if self.root.width(&self.ctes) != self.output.len() {
            return Err(bad(
                "P11",
                format!(
                    "root width {} differs from output columns {}",
                    self.root.width(&self.ctes),
                    self.output.len()
                ),
            ));
        }
        let mut st = Vstate {
            q: self,
            refs: vec![0; self.subplans.len()],
            binders: HashSet::new(),
        };
        st.plan(&self.root, &BTreeSet::new(), true)?;
        for c in &self.ctes {
            // CTE は 1 回だけ実行して共有するので、外側で束縛された Param は使えない（P8）。
            st.plan(c, &BTreeSet::new(), false)?;
        }
        if let Some(i) = st.refs.iter().position(|n| *n != 1) {
            return Err(bad(
                "P3",
                format!(
                    "SubPlanId {i} is referenced {} times (expected 1)",
                    st.refs[i]
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::error::Span;
    use crate::expr::AggCall;
    use crate::types::Datum;
    use std::sync::Arc;

    fn table(n: i16) -> Arc<TableDef> {
        let cols = (1..=n)
            .map(|i| ColumnDef {
                name: format!("c{i}"),
                attnum: i,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            })
            .collect();
        Arc::new(table_def(16384, "t", cols, vec![]))
    }

    fn scan(n: usize) -> PhysicalPlan {
        let t = table(i16::try_from(n).unwrap());
        PhysicalPlan::SeqScan {
            rel: RelHandle::from_table(&t),
            columns: vec![SqlType::INT4; n],
            system_columns: vec![],
            filter: None,
        }
    }

    fn local(i: usize) -> PhysExpr {
        Expr::column(PhysCol::Local(i), SqlType::INT4)
    }

    fn param(p: u16) -> PhysExpr {
        Expr::column(PhysCol::Param(ParamId(p)), SqlType::INT4)
    }

    fn eq(a: PhysExpr, b: PhysExpr) -> PhysExpr {
        let op = crate::catalog::builtin::operators_named("=")[0];
        Expr::new(
            ExprKind::Operator {
                op,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn out(n: usize) -> Vec<OutputColumn> {
        (0..n)
            .map(|_| OutputColumn {
                name: "x".into(),
                ty: SqlType::INT4,
                table_oid: 0,
                attnum: 0,
            })
            .collect()
    }

    fn sublink(id: u16, kind: SubLinkKind) -> PhysExpr {
        Expr::new(
            ExprKind::SubLink {
                kind,
                test: None,
                query: SubPlanId(id),
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn msg(r: Result<()>) -> String {
        let e = r.expect_err("expected a validation failure");
        assert_eq!(e.sqlstate.code(), "XX000");
        e.message
    }

    fn nlj(kind: JoinKind, o: PhysicalPlan, i: PhysicalPlan) -> PhysicalPlan {
        let (ow, iw) = (o.width(&[]), i.width(&[]));
        PhysicalPlan::NestedLoopJoin {
            kind,
            outer: Box::new(o),
            inner: Box::new(i),
            join_filter: None,
            outer_width: ow,
            inner_width: iw,
        }
    }

    fn hash_join(kind: JoinKind, build_is_left: bool) -> PhysicalPlan {
        PhysicalPlan::HashJoin {
            kind,
            left: Box::new(scan(2)),
            right: Box::new(scan(3)),
            left_keys: vec![local(0)],
            right_keys: vec![local(1)],
            key_types: vec![SqlType::INT4],
            residual: None,
            build_is_left,
            left_width: 2,
            right_width: 3,
        }
    }

    #[test]
    fn width_per_node() {
        let b = Box::new;
        assert_eq!(scan(3).width(&[]), 3);
        let with_sys = PhysicalPlan::SeqScan {
            rel: RelHandle::from_table(&table(2)),
            columns: vec![SqlType::INT4; 2],
            system_columns: vec![crate::catalog::SystemColumn::Ctid],
            filter: None,
        };
        assert_eq!(with_sys.width(&[]), 3);
        assert_eq!(nlj(JoinKind::Inner, scan(2), scan(3)).width(&[]), 5);
        assert_eq!(nlj(JoinKind::Semi, scan(2), scan(3)).width(&[]), 2);
        assert_eq!(nlj(JoinKind::Anti, scan(2), scan(3)).width(&[]), 2);
        assert_eq!(hash_join(JoinKind::Left, false).width(&[]), 5);
        assert_eq!(hash_join(JoinKind::Semi, true).width(&[]), 2);
        let project = PhysicalPlan::Project {
            input: b(scan(3)),
            exprs: vec![local(0)],
        };
        assert_eq!(project.width(&[]), 1);
        let agg = PhysicalPlan::HashAggregate {
            input: b(scan(3)),
            keys: vec![local(0), local(1)],
            key_types: vec![SqlType::INT4; 2],
            aggs: vec![PhysAgg {
                kind: AggKind::CountStar,
                arg_types: vec![],
                args: vec![],
                distinct: false,
                filter: None,
                order_by: Vec::new(),
                result: SqlType::INT8,
            }],
        };
        assert_eq!(agg.width(&[]), 3);
        // CteScan は ctes の幅、DML は returning の数。
        let ctes = vec![scan(4)];
        assert_eq!(PhysicalPlan::CteScan { cte: 0 }.width(&ctes), 4);
        assert_eq!(
            PhysicalPlan::Append {
                inputs: vec![scan(2), scan(2)]
            }
            .width(&[]),
            2
        );
        assert_eq!(
            PhysicalPlan::Delete {
                rel: RelHandle::from_table(&table(1)),
                input: b(scan(2)),
                n_user_cols: 1,
                returning: None
            }
            .width(&[]),
            0
        );
        assert_eq!(
            PhysicalPlan::Delete {
                rel: RelHandle::from_table(&table(1)),
                input: b(scan(2)),
                n_user_cols: 1,
                returning: Some(vec![local(0), local(0)])
            }
            .width(&[]),
            2
        );
        assert_eq!(
            PhysicalPlan::FunctionScan {
                func: crate::catalog::builtin::functions_named("abs")[0],
                args: vec![]
            }
            .width(&[]),
            1
        );
    }

    #[test]
    fn children_order_matches_execution_and_display() {
        // HashJoin は build_is_left の両方で [probe, build]。
        for (build_is_left, expect) in [(false, [2, 3]), (true, [3, 2])] {
            let j = hash_join(JoinKind::Inner, build_is_left);
            let w: Vec<usize> = j.children().iter().map(|c| c.width(&[])).collect();
            assert_eq!(w, expect);
        }
        let n = nlj(JoinKind::Inner, scan(1), scan(4));
        let w: Vec<usize> = n.children().iter().map(|c| c.width(&[])).collect();
        assert_eq!(w, [1, 4]);
        assert_eq!(n.node_count(), 3);
        assert!(scan(1).children().is_empty());
        let app = PhysicalPlan::Append {
            inputs: vec![scan(1), scan(1), scan(1)],
        };
        assert_eq!(app.children().len(), 3);
    }

    #[test]
    fn exprs_with_width_per_node() {
        let b = Box::new;
        let upd = PhysicalPlan::Update {
            rel: RelHandle::from_table(&table(2)),
            input: b(scan(4)),
            n_user_cols: 2,
            assigned: vec![(0, 3)],
            checks: vec![PhysCheck {
                name: "ck".into(),
                expr: eq(local(1), local(0)),
            }],
            not_null: vec![false; 2],
            table_name: "t".into(),
            returning: Some(vec![local(0)]),
        };
        let widths: Vec<usize> = upd.exprs_with_width(&[]).iter().map(|(_, w)| *w).collect();
        assert_eq!(widths, [2, 2]);
        let mut hj = hash_join(JoinKind::Inner, false);
        if let PhysicalPlan::HashJoin { residual, .. } = &mut hj {
            *residual = Some(eq(local(0), local(4)));
        }
        let widths: Vec<usize> = hj.exprs_with_width(&[]).iter().map(|(_, w)| *w).collect();
        assert_eq!(widths, [2, 3, 5]);
        let res = PhysicalPlan::Result {
            exprs: vec![Expr::literal(Datum::Int4(1), SqlType::INT4)],
            one_time_filter: Some(Expr::bool_lit(true)),
        };
        assert!(res.exprs_with_width(&[]).iter().all(|(_, w)| *w == 0));
        let filter = PhysicalPlan::Filter {
            input: b(scan(3)),
            predicate: eq(local(2), local(0)),
        };
        assert_eq!(filter.exprs_with_width(&[])[0].1, 3);
        let nlp = PhysicalPlan::NestedLoopParam {
            kind: JoinKind::Inner,
            outer: b(scan(2)),
            inner: b(scan(1)),
            params: vec![(ParamId(0), local(1))],
            join_filter: Some(eq(local(0), local(2))),
            outer_width: 2,
            inner_width: 1,
        };
        let widths: Vec<usize> = nlp.exprs_with_width(&[]).iter().map(|(_, w)| *w).collect();
        assert_eq!(widths, [2, 3]);
    }

    #[test]
    fn uses_params_cases() {
        assert!(!scan(1).uses_params());
        let with_param = PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: eq(local(0), param(0)),
        };
        assert!(with_param.uses_params());
        // 子の中の Param も数える。
        let wrapped = PhysicalPlan::Limit {
            input: Box::new(with_param),
            limit: None,
            offset: None,
        };
        assert!(wrapped.uses_params());
        // NestedLoopParam.params の式の Param。
        let nlp = PhysicalPlan::NestedLoopParam {
            kind: JoinKind::Inner,
            outer: Box::new(scan(1)),
            inner: Box::new(scan(1)),
            params: vec![(ParamId(0), param(1))],
            join_filter: None,
            outer_width: 1,
            inner_width: 1,
        };
        assert!(nlp.uses_params());
        // SubLink を含めば true。
        let sub = PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: sublink(0, SubLinkKind::Exists),
        };
        assert!(sub.uses_params());
        assert!(sublink(0, SubLinkKind::Exists).uses_params());
        assert!(!eq(local(0), local(0)).uses_params());
    }

    fn query_with(root: PhysicalPlan) -> PhysicalQuery {
        let n = root.width(&[]);
        PhysicalQuery::single(root, out(n))
    }

    fn subdef(
        plan: PhysicalPlan,
        strategy: SubPlanStrategy,
        params: Vec<(ParamId, PhysExpr)>,
    ) -> SubPlanDef {
        SubPlanDef {
            plan,
            kind: SubLinkKind::Exists,
            test: None,
            params,
            strategy,
            explain: None,
        }
    }

    #[test]
    fn single_empty_and_exec_ids() {
        let e = PhysicalQuery::empty();
        e.validate().unwrap();
        assert_eq!(e.root.width(&[]), 0);
        let mut q = query_with(PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: sublink(0, SubLinkKind::Exists),
        });
        q.subplans.push(subdef(
            PhysicalPlan::Limit {
                input: Box::new(scan(1)),
                limit: None,
                offset: None,
            },
            SubPlanStrategy::InitOnce,
            vec![],
        ));
        q.ctes.push(scan(1));
        q.validate().unwrap();
        let ids = assign_exec_ids(&q);
        // 根 2 ノード（0, 1）、subplans[0] 2 ノード（2, 3）、ctes[0] 1 ノード（4）。
        assert_eq!(
            ids,
            ExecIds {
                root: 0,
                subplans: vec![2],
                ctes: vec![4],
                total: 5
            }
        );
    }

    #[test]
    fn physical_validate_rejects_each_rule() {
        // P1: Local が文脈行の幅以上。
        let q = query_with(PhysicalPlan::Filter {
            input: Box::new(scan(2)),
            predicate: eq(local(2), local(0)),
        });
        assert!(msg(q.validate()).contains("[P1]"));
        // 空の文脈行（Limit）に Local。
        let q = query_with(PhysicalPlan::Limit {
            input: Box::new(scan(1)),
            limit: Some(local(0)),
            offset: None,
        });
        assert!(msg(q.validate()).contains("[P1]"));
        // P2: 束縛されていない Param / n_params を超える Param / 同じ ParamId を 2 か所が束縛。
        let mut q = query_with(PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: eq(local(0), param(0)),
        });
        q.n_params = 1;
        assert!(msg(q.validate()).contains("[P2]"));
        q.n_params = 0;
        assert!(msg(q.validate()).contains("[P2]"));
        let nlp = |id: u16, inner: PhysicalPlan| PhysicalPlan::NestedLoopParam {
            kind: JoinKind::Inner,
            outer: Box::new(scan(1)),
            inner: Box::new(inner),
            params: vec![(ParamId(id), local(0))],
            join_filter: None,
            outer_width: 1,
            inner_width: 1,
        };
        let bound_use = PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: eq(local(0), param(0)),
        };
        let mut q = query_with(nlp(0, bound_use.clone()));
        q.n_params = 1;
        q.validate().unwrap();
        let twice = PhysicalPlan::NestedLoopJoin {
            kind: JoinKind::Inner,
            outer: Box::new(nlp(0, bound_use.clone())),
            inner: Box::new(nlp(0, bound_use)),
            join_filter: None,
            outer_width: 2,
            inner_width: 2,
        };
        let mut q = query_with(twice);
        q.n_params = 1;
        assert!(msg(q.validate()).contains("[P2]"));
        // P3: 同じ SubPlanId を 2 回参照 / kind の不一致 / 範囲外 / 参照されない。
        let two_refs = PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: Expr::and_all(vec![
                sublink(0, SubLinkKind::Exists),
                sublink(0, SubLinkKind::Exists),
            ]),
        };
        let mut q = query_with(two_refs);
        q.subplans
            .push(subdef(scan(1), SubPlanStrategy::InitOnce, vec![]));
        assert!(msg(q.validate()).contains("[P3]"));
        let one_ref = || {
            query_with(PhysicalPlan::Filter {
                input: Box::new(scan(1)),
                predicate: sublink(0, SubLinkKind::Exists),
            })
        };
        let mut q = one_ref();
        q.subplans.push(SubPlanDef {
            kind: SubLinkKind::Scalar,
            ..subdef(scan(1), SubPlanStrategy::InitOnce, vec![])
        });
        assert!(msg(q.validate()).contains("[P3]"));
        assert!(msg(one_ref().validate()).contains("[P3]"));
        let mut q = one_ref();
        q.subplans
            .push(subdef(scan(1), SubPlanStrategy::InitOnce, vec![]));
        q.subplans
            .push(subdef(scan(1), SubPlanStrategy::InitOnce, vec![]));
        assert!(msg(q.validate()).contains("[P3]"));
        // P4: 物理の SubLink.test が Some / InitOnce に params / InitOnce が外側の Param を使う。
        let mut with_test = one_ref();
        if let PhysicalPlan::Filter { predicate, .. } = &mut with_test.root
            && let ExprKind::SubLink { test, .. } = &mut predicate.kind
        {
            *test = Some(Box::new(Expr::bool_lit(true)));
        }
        with_test
            .subplans
            .push(subdef(scan(1), SubPlanStrategy::InitOnce, vec![]));
        assert!(msg(with_test.validate()).contains("[P4]"));
        let mut q = one_ref();
        q.n_params = 1;
        q.subplans.push(subdef(
            scan(1),
            SubPlanStrategy::InitOnce,
            vec![(ParamId(0), local(0))],
        ));
        assert!(msg(q.validate()).contains("[P4]"));
        let mut q = one_ref();
        q.n_params = 1;
        q.subplans.push(subdef(
            PhysicalPlan::Filter {
                input: Box::new(scan(1)),
                predicate: eq(local(0), param(0)),
            },
            SubPlanStrategy::InitOnce,
            vec![],
        ));
        assert!(msg(q.validate()).contains("[P2]"));
        // Rescan は params を束縛して内側で使える。
        let mut q = one_ref();
        q.n_params = 1;
        q.subplans.push(subdef(
            PhysicalPlan::Filter {
                input: Box::new(scan(1)),
                predicate: eq(local(0), param(0)),
            },
            SubPlanStrategy::Rescan,
            vec![(ParamId(0), local(0))],
        ));
        q.validate().unwrap();
        // P5: Aggregate 式。
        let agg_expr: PhysExpr = Expr::new(
            ExprKind::Aggregate(Box::new(AggCall {
                func: &crate::catalog::BuiltinAggregate {
                    oid: 1,
                    name: "count",
                    args: &[],
                    result: 20,
                    kind: AggKind::CountStar,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: Vec::new(),
            })),
            SqlType::INT8,
            Span::default(),
        );
        let q = query_with(PhysicalPlan::Filter {
            input: Box::new(scan(1)),
            predicate: agg_expr,
        });
        assert!(msg(q.validate()).contains("[P5]"));
        // P6: 幅の不一致。
        let bad_nlj = PhysicalPlan::NestedLoopJoin {
            kind: JoinKind::Inner,
            outer: Box::new(scan(1)),
            inner: Box::new(scan(1)),
            join_filter: None,
            outer_width: 2,
            inner_width: 1,
        };
        assert!(msg(query_with(bad_nlj).validate()).contains("[P6]"));
        let bad_append = PhysicalPlan::Append {
            inputs: vec![scan(1), scan(2)],
        };
        assert!(msg(query_with(bad_append).validate()).contains("[P6]"));
        let bad_unique = PhysicalPlan::Unique {
            input: Box::new(scan(1)),
            key_cols: vec![1],
        };
        assert!(msg(query_with(bad_unique).validate()).contains("[P6]"));
        let rel = RelHandle::from_table(&table(1));
        let bad_update = PhysicalPlan::Update {
            rel: rel.clone(),
            input: Box::new(scan(2)),
            n_user_cols: 1,
            assigned: vec![(0, 1)],
            checks: vec![],
            not_null: vec![false],
            table_name: "t".into(),
            returning: None,
        };
        assert!(msg(query_with(bad_update).validate()).contains("[P6]"));
        let good_update = PhysicalPlan::Update {
            rel: rel.clone(),
            input: Box::new(scan(3)),
            n_user_cols: 1,
            assigned: vec![(0, 2)],
            checks: vec![],
            not_null: vec![false],
            table_name: "t".into(),
            returning: None,
        };
        query_with(good_update).validate().unwrap();
        // P7: FULL の入れ子ループ / 空の Values。
        assert!(msg(query_with(nlj(JoinKind::Full, scan(1), scan(1))).validate()).contains("[P7]"));
        assert!(msg(query_with(PhysicalPlan::Values { rows: vec![] }).validate()).contains("[P7]"));
        // P8: CteScan の範囲外。
        assert!(msg(query_with(PhysicalPlan::CteScan { cte: 0 }).validate()).contains("[P8]"));
        // P10: DML が根以外。
        let del = PhysicalPlan::Delete {
            rel,
            input: Box::new(scan(2)),
            n_user_cols: 1,
            returning: Some(vec![local(0)]),
        };
        let q = query_with(PhysicalPlan::Limit {
            input: Box::new(del),
            limit: None,
            offset: None,
        });
        assert!(msg(q.validate()).contains("[P10]"));
        // P11: 根の幅と output の数。
        let mut q = query_with(scan(2));
        q.output = out(1);
        assert!(msg(q.validate()).contains("[P11]"));
        // PhysicalPlan::validate（単独）。
        let q = query_with(scan(1));
        q.root.validate(&PhysValidateCx { query: &q }).unwrap();
    }
}
