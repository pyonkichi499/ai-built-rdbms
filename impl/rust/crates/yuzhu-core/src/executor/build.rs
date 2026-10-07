//! Builds an executor tree from a physical plan (`m4/02` §3.7、`m4/05` §3.2).
//!
//! `PhysicalPlan` の各ノードを `nodes/` のノードに 1 対 1 で対応させる。式は複製してノードに持たせる。
//! 溜める系ノード（`Sort` など）が `rewind` で結果を再利用してよいかは、作る時点で
//! `free_params(そのノードの部分木)` が空かで決める（11 §7.1 の C-6）。

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    clippy::only_used_in_recursion
)]

use std::collections::BTreeSet;

use super::BoxedExecutor;
use super::instrument::InstrBuild;
use super::nodes::collect_constants;
use super::nodes::{
    DeleteExec, DistinctExec, FilterExec, InsertExec, LimitExec, ProjectExec, ResultExec,
    SeqScanExec, SortExec, UpdateExec, ValuesExec, aggregate, append, cte_scan, function_scan,
    group_aggregate, hash_aggregate, hash_join, hash_setop, index_scan, materialize, nested_loop,
    unique,
};
use crate::expr::{ExprKind, ParamId, PhysCol};
use crate::planner::physical::{PhysExpr, PhysicalPlan, PhysicalQuery, SubPlanStrategy};

/// 構築の文脈。`query` が `None` のとき（`PhysicalQuery` を持たない単体テスト）、SubLink を含む木は
/// 「パラメータに依存する」とみなす（保守的）。
#[derive(Clone, Copy, Default)]
pub(crate) struct BuildEnv<'a> {
    pub query: Option<&'a PhysicalQuery>,
    /// EXPLAIN ANALYZE のとき、作った各ノードを `Instrumented` で包む（C-2）。
    pub instr: Option<&'a InstrBuild>,
}

/// `plan` の Executor 木を作る（`subplans` を持たない単体テスト用）。
pub fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    build_scoped(plan, &BuildEnv::default(), false)
}

/// 本番の入口。`query.root` を作る（`SubPlan` / CTE の Executor は `SubPlanStates` / `CteStates` が
/// 最初に使うときに作る）。
pub fn build_query(query: &PhysicalQuery) -> BoxedExecutor {
    build_scoped(
        &query.root,
        &BuildEnv {
            query: Some(query),
            instr: None,
        },
        false,
    )
}

/// `rewindable`: このノードが（親または祖先によって）`rewind` されうるか。root は false、
/// `NestedLoop*` の inner と、SubPlan / CTE の plan の root は true。P0-c のノードは使わない
/// （X1〜X3 のノードが使う）。
pub(crate) fn build_scoped(
    plan: &PhysicalPlan,
    env: &BuildEnv<'_>,
    rewindable: bool,
) -> BoxedExecutor {
    let exec = build_node(plan, env, rewindable);
    match env.instr {
        Some(ib) => ib.wrap(plan, exec),
        None => exec,
    }
}

fn build_node(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let child = |p: &PhysicalPlan| build_scoped(p, env, rewindable);
    match plan {
        PhysicalPlan::Result {
            exprs,
            one_time_filter,
        } => Box::new(ResultExec::with_filter(
            exprs.clone(),
            one_time_filter.clone(),
        )),
        PhysicalPlan::Values { rows } => Box::new(ValuesExec::new(rows.clone())),
        PhysicalPlan::SeqScan {
            rel,
            system_columns,
            filter,
            ..
        } => Box::new(SeqScanExec::with_filter(
            rel.clone(),
            system_columns.clone(),
            filter.clone(),
        )),
        PhysicalPlan::IndexScan { .. } => index_scan::build(plan),
        PhysicalPlan::FunctionScan { .. } => function_scan::build_in(plan, env, rewindable),
        PhysicalPlan::Filter { input, predicate } => {
            Box::new(FilterExec::new(child(input), predicate.clone()))
        }
        PhysicalPlan::Project { input, exprs } => {
            Box::new(ProjectExec::new(child(input), exprs.clone()))
        }
        PhysicalPlan::Sort { input, keys } => Box::new(SortExec::with_reusable(
            child(input),
            keys.clone(),
            free_params(plan, env.query).is_empty(),
        )),
        PhysicalPlan::Unique { .. } => unique::build_in(plan, env, rewindable),
        PhysicalPlan::Distinct { input } => Box::new(DistinctExec::new(child(input))),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let mut folds = Vec::new();
            collect_folds(input, &mut folds);
            Box::new(LimitExec::new(child(input), limit.clone(), offset.clone()).with_folds(folds))
        }
        PhysicalPlan::Materialize { .. } => materialize::build(plan, env, rewindable),
        PhysicalPlan::NestedLoopJoin { .. } | PhysicalPlan::NestedLoopParam { .. } => {
            nested_loop::build(plan, env, rewindable)
        }
        PhysicalPlan::HashJoin { .. } => hash_join::build(plan, env, rewindable),
        PhysicalPlan::Aggregate { .. } => aggregate::build_in(plan, env, rewindable),
        PhysicalPlan::HashAggregate { .. } => hash_aggregate::build_in(plan, env, rewindable),
        PhysicalPlan::GroupAggregate { .. } => group_aggregate::build_in(plan, env, rewindable),
        PhysicalPlan::Append { .. } => append::build_in(plan, env, rewindable),
        PhysicalPlan::HashSetOp { .. } => hash_setop::build_in(plan, env, rewindable),
        PhysicalPlan::CteScan { .. } => cte_scan::build_in(plan, env, rewindable),
        PhysicalPlan::Insert {
            rel,
            input,
            column_map,
            defaults,
            checks,
            not_null,
            table_name,
            returning,
        } => Box::new(InsertExec::with_returning(
            rel.clone(),
            table_name.clone(),
            child(input),
            column_map.clone(),
            defaults.clone(),
            checks.clone(),
            not_null.clone(),
            returning.clone(),
        )),
        PhysicalPlan::Update {
            rel,
            input,
            n_user_cols,
            assigned,
            checks,
            not_null,
            table_name,
            returning,
        } => Box::new(UpdateExec::new(
            rel.clone(),
            table_name.clone(),
            child(input),
            *n_user_cols,
            assigned.clone(),
            checks.clone(),
            not_null.clone(),
            returning.clone(),
        )),
        PhysicalPlan::Delete {
            rel,
            input,
            n_user_cols,
            returning,
        } => Box::new(DeleteExec::with_returning(
            rel.clone(),
            child(input),
            *n_user_cols,
            returning.clone(),
        )),
    }
}

/// LIMIT の下にある Project / Filter / Result の定数部分式を集める。
fn collect_folds(plan: &PhysicalPlan, out: &mut Vec<PhysExpr>) {
    match plan {
        PhysicalPlan::Result { exprs, .. } => {
            for e in exprs {
                collect_constants(e, out);
            }
        }
        PhysicalPlan::Project { input, exprs } => {
            for e in exprs {
                collect_constants(e, out);
            }
            collect_folds(input, out);
        }
        PhysicalPlan::Filter { input, predicate } => {
            collect_constants(predicate, out);
            collect_folds(input, out);
        }
        PhysicalPlan::Sort { input, .. } | PhysicalPlan::Distinct { input } => {
            collect_folds(input, out);
        }
        _ => {}
    }
}

/// `query = None` で SubLink を見つけたときに入れる印（どの `ParamId` にも当たらない）。
const UNKNOWN_SUBPLAN: ParamId = ParamId(u16::MAX);

/// 木の外から値を受ける `ParamId` の集合（読む - 束縛する）。
///
/// - 読む: 木の式の `PhysCol::Param`。木の中の SubLink について、`SubPlanDef` の `params` / `test` /
///   `strategy` のキーの式の `Param`、と `free_params(def.plan)` から `def.params` の `ParamId` を除いたもの。
/// - 束縛する: `NestedLoopParam.params` の `ParamId`（inner と `join_filter` の読みから除く）。
///
/// `query = None` のとき、SubLink を含む木は空でないとみなす。
pub(crate) fn free_params(plan: &PhysicalPlan, query: Option<&PhysicalQuery>) -> BTreeSet<ParamId> {
    let ctes: &[PhysicalPlan] = query.map_or(&[], |q| q.ctes.as_slice());
    let mut out = BTreeSet::new();
    if let PhysicalPlan::NestedLoopParam {
        outer,
        inner,
        params,
        join_filter,
        ..
    } = plan
    {
        let bound: BTreeSet<ParamId> = params.iter().map(|(p, _)| *p).collect();
        for (_, e) in params {
            expr_params(e, query, &mut out);
        }
        if let Some(f) = join_filter {
            let mut reads = BTreeSet::new();
            expr_params(f, query, &mut reads);
            out.extend(reads.into_iter().filter(|p| !bound.contains(p)));
        }
        out.extend(free_params(outer, query));
        out.extend(
            free_params(inner, query)
                .into_iter()
                .filter(|p| !bound.contains(p)),
        );
        return out;
    }
    for (e, _) in plan.exprs_with_width(ctes) {
        expr_params(e, query, &mut out);
    }
    for c in plan.children() {
        out.extend(free_params(c, query));
    }
    out
}

fn expr_params(e: &PhysExpr, query: Option<&PhysicalQuery>, out: &mut BTreeSet<ParamId>) {
    match &e.kind {
        ExprKind::Column(PhysCol::Param(p)) => {
            out.insert(*p);
        }
        ExprKind::SubLink { query: id, .. } => {
            let Some(def) = query.and_then(|q| q.subplans.get(usize::from(id.0))) else {
                out.insert(UNKNOWN_SUBPLAN);
                return;
            };
            for (_, pe) in &def.params {
                expr_params(pe, query, out);
            }
            if let Some(t) = &def.test {
                expr_params(t, query, out);
            }
            if let SubPlanStrategy::Hashed {
                probe_keys,
                build_keys,
            } = &def.strategy
            {
                for k in probe_keys.iter().chain(build_keys) {
                    expr_params(k, query, out);
                }
            }
            let bound: BTreeSet<ParamId> = def.params.iter().map(|(p, _)| *p).collect();
            out.extend(
                free_params(&def.plan, query)
                    .into_iter()
                    .filter(|p| !bound.contains(p)),
            );
        }
        _ => {
            for c in e.children() {
                expr_params(c, query, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, op, param};
    use crate::expr::SubLinkKind;
    use crate::planner::logical::JoinKind;
    use crate::planner::physical::{SortKey, SubPlanDef};
    use crate::types::SqlType;

    fn values() -> PhysicalPlan {
        PhysicalPlan::Values {
            rows: vec![vec![int(1)]],
        }
    }

    fn filter_on_param(p: u16) -> PhysicalPlan {
        PhysicalPlan::Filter {
            input: Box::new(values()),
            predicate: op(&GT, col(0, SqlType::INT4), param(p, SqlType::INT4)),
        }
    }

    #[test]
    fn free_params_reads_minus_bound() {
        assert!(free_params(&values(), None).is_empty());
        assert_eq!(
            free_params(&filter_on_param(3), None),
            BTreeSet::from([ParamId(3)])
        );
        // NestedLoopParam が束縛した Param は inner と join_filter では自由でない。
        let nlp = PhysicalPlan::NestedLoopParam {
            kind: JoinKind::Inner,
            outer: Box::new(values()),
            inner: Box::new(filter_on_param(0)),
            params: vec![(ParamId(0), col(0, SqlType::INT4))],
            join_filter: Some(op(&GT, col(0, SqlType::INT4), param(0, SqlType::INT4))),
            outer_width: 1,
            inner_width: 1,
        };
        assert!(free_params(&nlp, None).is_empty());
        // 束縛されていない Param は自由なまま。
        let nlp = PhysicalPlan::NestedLoopParam {
            kind: JoinKind::Inner,
            outer: Box::new(values()),
            inner: Box::new(filter_on_param(1)),
            params: vec![(ParamId(0), col(0, SqlType::INT4))],
            join_filter: None,
            outer_width: 1,
            inner_width: 1,
        };
        assert_eq!(free_params(&nlp, None), BTreeSet::from([ParamId(1)]));
    }

    fn sublink_plan() -> (PhysicalPlan, PhysicalQuery) {
        let sub = PhysExpr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Scalar,
                test: None,
                query: crate::expr::SubPlanId(0),
            },
            SqlType::INT4,
            crate::error::Span::default(),
        );
        let root = PhysicalPlan::Project {
            input: Box::new(values()),
            exprs: vec![sub],
        };
        let mut q = PhysicalQuery::single(root.clone(), vec![]);
        q.n_params = 2;
        q.subplans.push(SubPlanDef {
            plan: filter_on_param(1),
            kind: SubLinkKind::Scalar,
            test: None,
            // Param(1) は SubLink が束縛する。
            params: vec![(ParamId(1), col(0, SqlType::INT4))],
            strategy: SubPlanStrategy::Rescan,
            explain: None,
        });
        (root, q)
    }

    #[test]
    fn free_params_through_sublinks() {
        let (root, q) = sublink_plan();
        // query なし: SubLink は保守的に「依存する」。
        assert!(!free_params(&root, None).is_empty());
        // query あり: SubLink が束縛した Param は外から見て自由でない。
        assert!(free_params(&root, Some(&q)).is_empty());
    }

    #[test]
    fn sort_reusable_follows_free_params() {
        // 構築が通ることだけ確かめる（再利用の振る舞いは SortExec のテスト）。
        let sort = PhysicalPlan::Sort {
            input: Box::new(filter_on_param(0)),
            keys: vec![SortKey {
                expr: col(0, SqlType::INT4),
                descending: false,
                nulls_first: false,
            }],
        };
        let _ = build(&sort);
        assert!(!free_params(&sort, None).is_empty());
    }
}
