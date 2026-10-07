//! R5 述語の押し下げ（`m4/04` §6.5）。
//!
//! `Filter` の述語と結合の ON の述語を、列がそろう最も下のノードへ移す。Volatile な項は動かさない。
//! 任意拡張の定数の推移（§6.5.4）は実装していない。

use std::collections::HashMap;

use super::{RuleCtx, on_roots};
use crate::error::Result;
use crate::expr::ColId;
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{ColumnArena, JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{
    ColSet, Volatility, and_all, conjuncts, depth_error, expr_refs, expr_volatility, plan_output,
};

pub fn apply(q: &mut LogicalQuery, _cx: &RuleCtx<'_>) -> Result<()> {
    on_roots(q, &mut |p, arena| push(p, Vec::new(), arena, 0))
}

fn volatile(e: &LExpr) -> bool {
    expr_volatility(e) == Volatility::Volatile
}

fn wrap(plan: LogicalPlan, preds: Vec<LExpr>) -> LogicalPlan {
    match and_all(preds) {
        Some(p) => LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: p,
        },
        None => plan,
    }
}

fn colset(p: &LogicalPlan) -> ColSet {
    plan_output(p).into_iter().collect()
}

/// どちらの側に収まるか。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Both,
    /// 結合の列を 1 つも参照しない。
    None,
}

fn side_of(e: &LExpr, l: &ColSet, r: &ColSet) -> Side {
    let refs = expr_refs(e);
    let in_l = refs.iter().any(|c| l.contains(c));
    let in_r = refs.iter().any(|c| r.contains(c));
    match (in_l, in_r) {
        (false, false) => Side::None,
        (true, false) => Side::Left,
        (false, true) => Side::Right,
        (true, true) => Side::Both,
    }
}

#[allow(clippy::too_many_lines)]
fn push(
    plan: LogicalPlan,
    preds: Vec<LExpr>,
    arena: &ColumnArena,
    depth: usize,
) -> Result<LogicalPlan> {
    use LogicalPlan as L;
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    let d = depth + 1;
    Ok(match plan {
        L::Filter { input, predicate } => {
            let (vol, nonvol): (Vec<LExpr>, Vec<LExpr>) =
                conjuncts(predicate).into_iter().partition(volatile);
            // PostgreSQL は上位の述語を先に並べる（引き上げた副問い合わせの述語は後ろ）。
            let mut all = preds;
            all.extend(nonvol);
            let inner = push(*input, all, arena, d)?;
            wrap(inner, vol)
        }
        p @ (L::Get { .. } | L::Values { .. } | L::FunctionScan { .. } | L::CteScan { .. }) => {
            wrap(p, preds)
        }
        L::Result {
            one_time_filter,
            cols,
        } => {
            let mut all: Vec<LExpr> = one_time_filter.into_iter().flat_map(conjuncts).collect();
            all.extend(preds);
            L::Result {
                one_time_filter: and_all(all),
                cols,
            }
        }
        e @ L::Empty { .. } => e,
        L::Project { input, exprs } => {
            let ids: HashMap<ColId, &LExpr> = exprs.iter().map(|(c, e)| (*c, e)).collect();
            let mut down = Vec::new();
            let mut stay = Vec::new();
            for p in preds {
                let refs = expr_refs(&p);
                let mut map = HashMap::new();
                let mut ok = true;
                for c in refs {
                    if let Some(e) = ids.get(&c) {
                        if volatile(e) || e.contains_sublink() {
                            ok = false;
                            break;
                        }
                        map.insert(c, (*e).clone());
                    }
                }
                if ok {
                    down.push(p.substitute(&map)?);
                } else {
                    stay.push(p);
                }
            }
            let input = push(*input, down, arena, d)?;
            wrap(
                L::Project {
                    input: Box::new(input),
                    exprs,
                },
                stay,
            )
        }
        L::Aggregate {
            input,
            group_by,
            aggs,
        } => {
            let mut down = Vec::new();
            let mut stay = Vec::new();
            if group_by.is_empty() {
                stay = preds;
            } else {
                let gmap: HashMap<ColId, &LExpr> = group_by.iter().map(|(c, e)| (*c, e)).collect();
                for p in preds {
                    let refs = expr_refs(&p);
                    let agg_ids: ColSet = aggs.iter().map(|(c, _)| *c).collect();
                    let mut map = HashMap::new();
                    let mut ok = !refs.iter().any(|c| agg_ids.contains(c));
                    if ok {
                        for c in refs {
                            if let Some(e) = gmap.get(&c) {
                                if volatile(e) || e.contains_sublink() {
                                    ok = false;
                                    break;
                                }
                                map.insert(c, (*e).clone());
                            }
                        }
                    }
                    if ok {
                        down.push(p.substitute(&map)?);
                    } else {
                        stay.push(p);
                    }
                }
            }
            let input = push(*input, down, arena, d)?;
            wrap(
                L::Aggregate {
                    input: Box::new(input),
                    group_by,
                    aggs,
                },
                stay,
            )
        }
        L::Distinct { input, on: None } => L::Distinct {
            input: Box::new(push(*input, preds, arena, d)?),
            on: None,
        },
        L::Distinct {
            input,
            on: Some(on),
        } => wrap(
            L::Distinct {
                input: Box::new(push(*input, Vec::new(), arena, d)?),
                on: Some(on),
            },
            preds,
        ),
        L::Sort { input, keys } => L::Sort {
            input: Box::new(push(*input, preds, arena, d)?),
            keys,
        },
        L::Limit {
            input,
            limit,
            offset,
        } => wrap(
            L::Limit {
                input: Box::new(push(*input, Vec::new(), arena, d)?),
                limit,
                offset,
            },
            preds,
        ),
        L::SetOp {
            op,
            all,
            left,
            right,
            cols,
            left_cols,
            right_cols,
        } => {
            let colset: ColSet = cols.iter().copied().collect();
            let mut lp = Vec::new();
            let mut rp = Vec::new();
            let mut stay = Vec::new();
            for p in preds {
                let refs = expr_refs(&p);
                // SubLink を複製すると ColId が二重に定義される。
                if p.contains_sublink() || refs.iter().any(|c| !colset.contains(c)) {
                    stay.push(p);
                    continue;
                }
                let mk = |to: &[ColId]| -> HashMap<ColId, LExpr> {
                    cols.iter()
                        .zip(to)
                        .map(|(c, t)| (*c, LExpr::column(*t, arena.get(*t).ty)))
                        .collect()
                };
                lp.push(p.substitute(&mk(&left_cols))?);
                rp.push(p.substitute(&mk(&right_cols))?);
            }
            let node = L::SetOp {
                op,
                all,
                left: Box::new(push(*left, lp, arena, d)?),
                right: Box::new(push(*right, rp, arena, d)?),
                cols,
                left_cols,
                right_cols,
            };
            wrap(node, stay)
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
        } => L::Insert {
            table,
            rel,
            input: Box::new(push(*input, Vec::new(), arena, d)?),
            input_cols,
            column_map,
            defaults,
            checks,
            not_null,
            returning,
        },
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
        } => L::Update {
            table,
            rel,
            input: Box::new(push(*input, Vec::new(), arena, d)?),
            old_cols,
            ctid,
            new_values,
            checks,
            not_null,
            returning,
        },
        L::Delete {
            table,
            rel,
            input,
            old_cols,
            ctid,
            returning,
        } => L::Delete {
            table,
            rel,
            input: Box::new(push(*input, Vec::new(), arena, d)?),
            old_cols,
            ctid,
            returning,
        },
        L::Join {
            kind,
            left,
            right,
            on,
        } => push_join(kind, *left, *right, on, preds, arena, d)?,
    })
}

fn push_join(
    kind: JoinKind,
    left: LogicalPlan,
    right: LogicalPlan,
    on: Option<LExpr>,
    preds: Vec<LExpr>,
    arena: &ColumnArena,
    d: usize,
) -> Result<LogicalPlan> {
    let (l, r) = (colset(&left), colset(&right));
    let on_terms: Vec<LExpr> = on.map(conjuncts).unwrap_or_default();
    let mut lp = Vec::new();
    let mut rp = Vec::new();
    let mut keep_on = Vec::new();
    let mut above = Vec::new();
    match kind {
        JoinKind::Inner => {
            for p in preds.into_iter().chain(on_terms) {
                if volatile(&p) {
                    keep_on.push(p);
                    continue;
                }
                match side_of(&p, &l, &r) {
                    Side::Left | Side::None => lp.push(p),
                    Side::Right => rp.push(p),
                    Side::Both => keep_on.push(p),
                }
            }
        }
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti => {
            for p in preds {
                match side_of(&p, &l, &r) {
                    Side::Left | Side::None => lp.push(p),
                    _ => above.push(p),
                }
            }
            for p in on_terms {
                if volatile(&p) {
                    keep_on.push(p);
                    continue;
                }
                match (kind, side_of(&p, &l, &r)) {
                    (_, Side::Right | Side::None) => rp.push(p),
                    (JoinKind::Semi, Side::Left) => lp.push(p),
                    _ => keep_on.push(p),
                }
            }
        }
        JoinKind::Full => {
            above = preds;
            keep_on = on_terms;
        }
    }
    let left = push(left, lp, arena, d)?;
    let right = push(right, rp, arena, d)?;
    let node = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
        sink(kind, left, right, and_all(keep_on))
    } else {
        LogicalPlan::Join {
            kind,
            left: Box::new(left),
            right: Box::new(right),
            on: and_all(keep_on),
        }
    };
    Ok(wrap(node, above))
}

/// Semi / Anti を、左の内部結合・左結合の片側へ沈める（§6.5.2）。
fn sink(kind: JoinKind, left: LogicalPlan, right: LogicalPlan, on: Option<LExpr>) -> LogicalPlan {
    let refs: ColSet = on.as_ref().map(expr_refs).unwrap_or_default();
    let mk = |left, right, on| LogicalPlan::Join {
        kind,
        left: Box::new(left),
        right: Box::new(right),
        on,
    };
    let LogicalPlan::Join {
        kind: lk @ (JoinKind::Inner | JoinKind::Left),
        left: ll,
        right: lr,
        on: lon,
    } = left
    else {
        return mk(left, right, on);
    };
    let (lcols, rcols) = (colset(&ll), colset(&lr));
    let uses_l = refs.iter().any(|c| lcols.contains(c));
    let uses_r = refs.iter().any(|c| rcols.contains(c));
    let rebuild = |a, b| LogicalPlan::Join {
        kind: lk,
        left: Box::new(a),
        right: Box::new(b),
        on: lon.clone(),
    };
    if uses_l && !uses_r {
        let sunk = sink(kind, *ll, right, on);
        rebuild(sunk, *lr)
    } else if uses_r && !uses_l && lk == JoinKind::Inner {
        let sunk = sink(kind, *lr, right, on);
        rebuild(*ll, sunk)
    } else {
        mk(
            LogicalPlan::Join {
                kind: lk,
                left: ll,
                right: lr,
                on: lon,
            },
            right,
            on,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new(
            "create table t(a int4, b int4, c int4); create table u(a int4, d int4, e int4); \
             create table v(x int4, y int4);",
        )
        .unwrap()
    }

    fn run(sql: &str) -> String {
        let all = [
            "const_fold",
            "sublink",
            "subquery_pullup",
            "outer_join",
            "pushdown",
        ];
        print_logical(&fx().plan_with(sql, &all).unwrap())
    }

    #[test]
    fn inner_join_predicates_go_to_each_side() {
        let p = run("select * from t join u on t.a = u.a where t.b = 1 and u.d = 2 and t.c = u.e");
        assert!(
            p.contains("Filter (#1:t.b = 1)") && p.contains("Filter (#4:u.d = 2)"),
            "{p}"
        );
        assert!(
            p.contains("Join Inner on ((#0:t.a = #3:u.a) AND (#2:t.c = #5:u.e))")
                || p.contains("Join Inner on ((#2:t.c = #5:u.e) AND (#0:t.a = #3:u.a))"),
            "{p}"
        );
    }

    #[test]
    fn left_join_keeps_left_on_terms_and_pushes_right_ones() {
        let p = run("select * from t left join u on t.a = u.a and u.d = 2 and t.b = 3");
        assert!(p.contains("Filter (#4:u.d = 2)"), "{p}");
        assert!(
            p.contains("t.b = 3") && !p.contains("Filter (#1:t.b = 3)"),
            "{p}"
        );
        let p = run("select * from t left join u on t.a = u.a where t.b is null");
        assert!(p.contains("Filter (#1:t.b IS NULL)"), "{p}");
    }

    #[test]
    fn derived_table_and_having_push_down() {
        let p = run("select s.a from (select a, b + 1 as x from t where b > 1) s where s.x = 3");
        assert_eq!(p.matches("Filter").count(), 1, "{p}");
        assert!(p.contains("(((#1:t.b + 1) = 3) AND (#1:t.b > 1))"), "{p}");
        let p = run("select a, count(*) from t group by a having a > 1 and count(*) > 2");
        assert!(p.contains("Filter (#0:t.a > 1)"), "{p}");
        assert!(p.contains(":count) > 2") || p.contains("count > 2"), "{p}");
        // group by なしの HAVING は押し下げない。
        let p = run("select count(*) from t having count(*) > 1 and 1 = 1");
        assert!(
            p.find("Filter").unwrap() < p.find("Aggregate").unwrap(),
            "{p}"
        );
    }

    #[test]
    fn set_op_and_semi_sink() {
        let p = run("select a from (select a from t union all select a from u) s where a = 1");
        assert_eq!(p.matches("= 1)").count(), 2, "{p}");
        let p = run(
            "select t.a from t join u on t.a = u.a where exists (select 1 from v where v.x = t.b)",
        );
        // Semi は t 側へ沈む。
        let semi = p.find("Join Semi").unwrap();
        let inner = p.find("Join Inner").unwrap();
        assert!(inner < semi, "{p}");
    }
}
