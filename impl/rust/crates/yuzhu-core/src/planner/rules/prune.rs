//! R8 列の刈り込み（`m4/04` §6.8）。
//!
//! 上で使わない列を落とす。`Project` の式と `Aggregate` の集約の呼び出しを捨て、結合・ソート・DISTINCT の
//! 直下の走査には必要な列だけを出す `Project` を足す。Volatile な式は捨てない。

use super::RuleCtx;
use crate::error::Result;
use crate::expr::ColId;
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{ColumnArena, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{
    ColSet, Volatility, depth_error, expr_refs, expr_volatility, for_each_subquery, take_plan,
};

pub fn apply(q: &mut LogicalQuery, _cx: &RuleCtx<'_>) -> Result<()> {
    let arena = &q.arena;
    let req: ColSet = q.output.iter().copied().collect();
    let p = take_plan(&mut q.plan);
    q.plan = prune(p, &req, Parent::Other, arena, 0)?;
    for c in &mut q.ctes {
        let req: ColSet = c.output.iter().copied().collect();
        let p = take_plan(&mut c.plan);
        c.plan = prune(p, &req, Parent::Other, arena, 0)?;
    }
    Ok(())
}

/// 走査に刈り込み用の `Project` を足してよい親か。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    Wide,
    Other,
}

fn refs_of<'a>(it: impl IntoIterator<Item = &'a LExpr>) -> ColSet {
    let mut s = ColSet::new();
    for e in it {
        s.extend(expr_refs(e));
    }
    s
}

fn scan_like(p: &LogicalPlan) -> bool {
    match p {
        LogicalPlan::Get { .. } => true,
        LogicalPlan::Filter { input, .. } => matches!(**input, LogicalPlan::Get { .. }),
        _ => false,
    }
}

#[allow(clippy::too_many_lines)]
fn prune(
    plan: LogicalPlan,
    required: &ColSet,
    parent: Parent,
    arena: &ColumnArena,
    depth: usize,
) -> Result<LogicalPlan> {
    use LogicalPlan as L;
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    let d = depth + 1;
    if parent == Parent::Wide && scan_like(&plan) {
        let out = plan.output_cols();
        let needed: Vec<ColId> = out
            .iter()
            .copied()
            .filter(|c| required.contains(c))
            .collect();
        if !needed.is_empty() && needed.len() < out.len() {
            let inner = prune(plan, required, Parent::Other, arena, d)?;
            return Ok(L::Project {
                input: Box::new(inner),
                exprs: needed
                    .into_iter()
                    .map(|c| (c, LExpr::column(c, arena.get(c).ty)))
                    .collect(),
            });
        }
    }
    let mut node = match plan {
        L::Project { input, exprs } => {
            let mut kept: Vec<(ColId, LExpr)> = exprs
                .iter()
                .filter(|(c, e)| required.contains(c) || expr_volatility(e) == Volatility::Volatile)
                .cloned()
                .collect();
            if kept.is_empty() {
                kept.extend(exprs.into_iter().take(1));
            }
            let child_req = refs_of(kept.iter().map(|(_, e)| e));
            L::Project {
                input: Box::new(prune(*input, &child_req, Parent::Other, arena, d)?),
                exprs: kept,
            }
        }
        L::Filter { input, predicate } => {
            let mut r = required.clone();
            r.extend(expr_refs(&predicate));
            L::Filter {
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                predicate,
            }
        }
        L::Join {
            kind,
            left,
            right,
            on,
        } => {
            let mut need = required.clone();
            if let Some(on) = &on {
                need.extend(expr_refs(on));
            }
            let lr: ColSet = left
                .output_cols()
                .into_iter()
                .filter(|c| need.contains(c))
                .collect();
            let rr: ColSet = right
                .output_cols()
                .into_iter()
                .filter(|c| need.contains(c))
                .collect();
            L::Join {
                kind,
                left: Box::new(prune(*left, &lr, Parent::Wide, arena, d)?),
                right: Box::new(prune(*right, &rr, Parent::Wide, arena, d)?),
                on,
            }
        }
        L::Aggregate {
            input,
            group_by,
            aggs,
        } => {
            let aggs: Vec<_> = aggs
                .into_iter()
                .filter(|(c, _)| required.contains(c))
                .collect();
            let mut r = refs_of(group_by.iter().map(|(_, e)| e));
            for (_, a) in &aggs {
                r.extend(refs_of(
                    a.args
                        .iter()
                        .chain(a.filter.iter())
                        .chain(a.order_by.iter().map(|k| &k.expr)),
                ));
            }
            L::Aggregate {
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                group_by,
                aggs,
            }
        }
        L::Distinct { input, on: None } => {
            let all: ColSet = input.output_cols().into_iter().collect();
            L::Distinct {
                input: Box::new(prune(*input, &all, Parent::Wide, arena, d)?),
                on: None,
            }
        }
        L::Distinct {
            input,
            on: Some(on),
        } => {
            let mut r = required.clone();
            r.extend(refs_of(on.iter()));
            L::Distinct {
                input: Box::new(prune(*input, &r, Parent::Wide, arena, d)?),
                on: Some(on),
            }
        }
        L::Sort { input, keys } => {
            let mut r = required.clone();
            r.extend(refs_of(keys.iter().map(|k| &k.expr)));
            L::Sort {
                input: Box::new(prune(*input, &r, Parent::Wide, arena, d)?),
                keys,
            }
        }
        L::Limit {
            input,
            limit,
            offset,
        } => {
            let mut r = required.clone();
            r.extend(refs_of(limit.iter().chain(offset.iter())));
            L::Limit {
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                limit,
                offset,
            }
        }
        L::SetOp {
            op,
            all,
            left,
            right,
            cols,
            left_cols,
            right_cols,
        } => {
            let lr: ColSet = left_cols.iter().copied().collect();
            let rr: ColSet = right_cols.iter().copied().collect();
            L::SetOp {
                op,
                all,
                left: Box::new(prune(*left, &lr, Parent::Other, arena, d)?),
                right: Box::new(prune(*right, &rr, Parent::Other, arena, d)?),
                cols,
                left_cols,
                right_cols,
            }
        }
        L::Insert {
            table,
            rel,
            input,
            input_cols,
            column_map,
            defaults,
            checks,
            not_null,
            returning,
        } => {
            let r: ColSet = input_cols.iter().copied().collect();
            L::Insert {
                table,
                rel,
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                input_cols,
                column_map,
                defaults,
                checks,
                not_null,
                returning,
            }
        }
        L::Update {
            table,
            rel,
            input,
            old_cols,
            ctid,
            new_values,
            checks,
            not_null,
            returning,
        } => {
            let mut r: ColSet = old_cols.iter().copied().collect();
            r.insert(ctid);
            r.extend(new_values.iter().map(|(_, c)| *c));
            L::Update {
                table,
                rel,
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                old_cols,
                ctid,
                new_values,
                checks,
                not_null,
                returning,
            }
        }
        L::Delete {
            table,
            rel,
            input,
            old_cols,
            ctid,
            returning,
        } => {
            let mut r: ColSet = old_cols.iter().copied().collect();
            r.insert(ctid);
            L::Delete {
                table,
                rel,
                input: Box::new(prune(*input, &r, Parent::Other, arena, d)?),
                old_cols,
                ctid,
                returning,
            }
        }
        leaf @ (L::Get { .. }
        | L::Values { .. }
        | L::FunctionScan { .. }
        | L::CteScan { .. }
        | L::Result { .. }
        | L::Empty { .. }) => leaf,
    };
    // 式の中の副問い合わせは、自分の出力列だけが必要。
    for_each_subquery(&mut node, &mut |sq| {
        let r: ColSet = sq.output.iter().copied().collect();
        let p = take_plan(&mut sq.plan);
        sq.plan = prune(p, &r, Parent::Other, arena, d)?;
        Ok(())
    })?;
    Ok(node)
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn run(sql: &str) -> String {
        let f = Fixture::new(
            "create table t(a int4, b int4, c int4); create table u(a int4, d int4, e int4);",
        )
        .unwrap();
        let names = [
            "const_fold",
            "sublink",
            "subquery_pullup",
            "outer_join",
            "pushdown",
            "join_keys",
            "join_order",
            "prune",
        ];
        print_logical(&f.plan_with(sql, &names).unwrap())
    }

    #[test]
    fn join_inputs_are_narrowed() {
        let p = run("select t.c from t join u on t.a = u.a");
        assert!(p.contains("Project [#0:t.a #2:t.c]"), "{p}");
        assert!(p.contains("Project [#3:u.a]"), "{p}");
    }

    #[test]
    fn unused_aggregates_and_projections_are_dropped() {
        let p = run("select s.a from (select a, count(*) as n from t group by a) s");
        assert!(!p.contains("count"), "{p}");
    }

    #[test]
    fn full_width_use_keeps_the_scan_bare() {
        let p = run("select * from t join u on t.a = u.a");
        assert_eq!(p.matches("Project").count(), 1, "{p}");
    }

    #[test]
    fn dml_inputs_keep_their_shape() {
        let p = run("update t set b = u.d from u where t.a = u.a");
        assert!(p.starts_with("Update"), "{p}");
        let p = run("delete from t where a = 1");
        assert!(p.starts_with("Delete"), "{p}");
    }
}
