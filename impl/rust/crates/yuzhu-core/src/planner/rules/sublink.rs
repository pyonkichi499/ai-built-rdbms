//! R2 サブクエリの結合化（`m4/04` §6.2）。
//!
//! `Filter` の最上位 AND の項（と内部結合の ON の項）のうち、`EXISTS` / `NOT EXISTS` / `IN (subquery)` を
//! Semi / Anti 結合にする。PostgreSQL の `pull_up_sublinks` に相当する。

use std::collections::BTreeSet;

use super::{RuleCtx, on_roots};
use crate::error::Result;
use crate::expr::{ColId, ExprKind, SubLinkKind};
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{
    ColSet, Volatility, and_all, conjuncts, depth_error, expr_refs, expr_volatility,
    plan_free_cols, plan_output, take_plan,
};
use crate::types::Datum;

pub fn apply(q: &mut LogicalQuery, _cx: &RuleCtx<'_>) -> Result<()> {
    on_roots(q, &mut |p, _| rewrite(p, 0))
}

fn rewrite(p: LogicalPlan, depth: usize) -> Result<LogicalPlan> {
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    match p {
        LogicalPlan::Filter { input, predicate } => {
            let terms = conjuncts(predicate);
            filter_terms(*input, terms, depth)
        }
        LogicalPlan::Join {
            kind: JoinKind::Inner,
            left,
            right,
            on: Some(on),
        } if conjuncts(on.clone()).iter().any(is_pullable_shape) => {
            let (pull, keep): (Vec<LExpr>, Vec<LExpr>) =
                conjuncts(on).into_iter().partition(is_pullable_shape);
            let join = LogicalPlan::Join {
                kind: JoinKind::Inner,
                left,
                right,
                on: and_all(keep),
            };
            // 結合の ON の項は、結合の出力に対する `Filter` の項と同じ意味（内部結合）。
            filter_terms(join, pull, depth)
        }
        mut other => {
            for c in other.children_mut() {
                let child = take_plan(c);
                *c = rewrite(child, depth + 1)?;
            }
            Ok(other)
        }
    }
}

fn filter_terms(mut input: LogicalPlan, terms: Vec<LExpr>, depth: usize) -> Result<LogicalPlan> {
    let mut keep = Vec::new();
    for t in terms {
        match try_pull(t, &mut [&mut input]) {
            Ok(()) => {}
            Err(t) => keep.push(t),
        }
    }
    // 作った Semi / Anti の子と、元の入力の中を再帰する。
    let input = rewrite_children(input, depth)?;
    Ok(match and_all(keep) {
        Some(p) => LogicalPlan::Filter {
            input: Box::new(input),
            predicate: p,
        },
        None => input,
    })
}

/// 根の `input` 自体は（`Filter` でなければ）そのノードの規則に従って、子を再帰する。
fn rewrite_children(p: LogicalPlan, depth: usize) -> Result<LogicalPlan> {
    match p {
        LogicalPlan::Filter { .. } | LogicalPlan::Join { .. } => rewrite(p, depth + 1),
        mut other => {
            for c in other.children_mut() {
                let child = take_plan(c);
                *c = rewrite(child, depth + 1)?;
            }
            Ok(other)
        }
    }
}

fn is_pullable_shape(c: &LExpr) -> bool {
    match &c.kind {
        ExprKind::SubLink { kind, test, .. } => match kind {
            SubLinkKind::Exists => true,
            SubLinkKind::Any => test.is_some(),
            _ => false,
        },
        ExprKind::Not(x) => matches!(
            &x.kind,
            ExprKind::SubLink {
                kind: SubLinkKind::Exists,
                ..
            }
        ),
        _ => false,
    }
}

/// 解析の結果（`c` を消費するときに使う部品）。
struct Pulled {
    kind: JoinKind,
    side: usize,
    right: LogicalPlan,
    conds: Vec<LExpr>,
}

/// 変換できたら `Ok(())`（`sides` のどれかが Semi / Anti 結合に置き換わる）。できなければ `c` を返す。
fn try_pull(c: LExpr, sides: &mut [&mut LogicalPlan]) -> std::result::Result<(), LExpr> {
    let avails: Vec<ColSet> = sides
        .iter()
        .map(|s| plan_output(s).into_iter().collect())
        .collect();
    let Some(pulled) = analyze(&c, &avails) else {
        return Err(c);
    };
    let Pulled {
        kind,
        side,
        right,
        conds,
    } = pulled;
    let mut left = take_plan(sides[side]);
    let mut right = right;
    // 入れ子（§6.2.2）: ON の項にも同じ変換を適用する。Anti は右だけ。
    let mut keep = Vec::new();
    let left_cols: ColSet = plan_output(&left).into_iter().collect();
    for t in conds {
        let r = if kind == JoinKind::Semi {
            try_pull(t, &mut [&mut left, &mut right])
        } else if !inter(&expr_refs(&t), &left_cols).is_empty() {
            // 左の列を参照する項は右へ沈められない（右は左を参照できない）。
            Err(t)
        } else {
            try_pull(t, &mut [&mut right])
        };
        if let Err(t) = r {
            keep.push(t);
        }
    }
    *sides[side] = LogicalPlan::Join {
        kind,
        left: Box::new(left),
        right: Box::new(right),
        on: and_all(keep),
    };
    Ok(())
}

struct Peeled<'a> {
    from_tree: &'a LogicalPlan,
    where_: Vec<&'a LExpr>,
    proj: Option<&'a [(ColId, LExpr)]>,
}

fn has_scan(p: &LogicalPlan) -> bool {
    matches!(
        p,
        LogicalPlan::Get { .. }
            | LogicalPlan::Values { .. }
            | LogicalPlan::FunctionScan { .. }
            | LogicalPlan::CteScan { .. }
    ) || p.children().into_iter().any(has_scan)
}

fn peel(p: &LogicalPlan, allow_limit: bool) -> Option<Peeled<'_>> {
    let mut cur = p;
    if let LogicalPlan::Limit {
        input,
        limit,
        offset,
    } = cur
    {
        let ok = allow_limit
            && offset.is_none()
            && matches!(
                limit.as_ref().map(|l| &l.kind),
                Some(ExprKind::Literal(Datum::Int8(n))) if *n >= 1
            );
        if !ok {
            return None;
        }
        cur = input;
    }
    while let LogicalPlan::Sort { input, .. } | LogicalPlan::Distinct { input, on: None } = cur {
        cur = input;
    }
    let mut proj = None;
    if let LogicalPlan::Project { input, exprs } = cur {
        proj = Some(exprs.as_slice());
        cur = input;
    }
    let mut where_ = Vec::new();
    if let LogicalPlan::Filter { input, predicate } = cur {
        where_ = predicate.conjuncts();
        cur = input;
    }
    if matches!(
        cur,
        LogicalPlan::Aggregate { .. }
            | LogicalPlan::SetOp { .. }
            | LogicalPlan::Distinct { .. }
            | LogicalPlan::Limit { .. }
            | LogicalPlan::Sort { .. }
            | LogicalPlan::Result { .. }
            | LogicalPlan::Empty { .. }
    ) || !has_scan(cur)
    {
        return None;
    }
    Some(Peeled {
        from_tree: cur,
        where_,
        proj,
    })
}

fn no_volatile<'a>(it: impl IntoIterator<Item = &'a LExpr>) -> bool {
    it.into_iter()
        .all(|e| expr_volatility(e) != Volatility::Volatile)
}

fn has_sublink(e: &LExpr) -> bool {
    e.contains_sublink()
}

fn inter(a: &ColSet, b: &ColSet) -> ColSet {
    a.intersection(b).copied().collect()
}

#[allow(clippy::too_many_lines, clippy::single_match_else)]
fn analyze(c: &LExpr, avails: &[ColSet]) -> Option<Pulled> {
    // 1. 形
    let (kind, sub_kind, test, query) = match &c.kind {
        ExprKind::SubLink {
            kind: SubLinkKind::Exists,
            query,
            ..
        } => (JoinKind::Semi, SubLinkKind::Exists, None, query),
        ExprKind::SubLink {
            kind: SubLinkKind::Any,
            test: Some(t),
            query,
        } => (JoinKind::Semi, SubLinkKind::Any, Some(&**t), query),
        ExprKind::Not(x) => match &x.kind {
            ExprKind::SubLink {
                kind: SubLinkKind::Exists,
                query,
                ..
            } => (JoinKind::Anti, SubLinkKind::Exists, None, query),
            _ => return None,
        },
        _ => return None,
    };
    // 2. 側
    let union: ColSet = avails.iter().flatten().copied().collect();
    let all_refs = expr_refs(c);
    let r = inter(&all_refs, &union);
    if r.is_empty() {
        return None;
    }
    let side = avails.iter().position(|a| r.is_subset(a))?;
    let avail = &avails[side];
    // 3・4. 条件と変換
    let free = |p: &LogicalPlan| -> ColSet { inter(&plan_free_cols(p), avail) };
    match sub_kind {
        SubLinkKind::Exists => {
            let pe = peel(&query.plan, true)?;
            if !free(pe.from_tree).is_empty() || !no_volatile(pe.where_.iter().copied()) {
                return None;
            }
            let mut w_refs = BTreeSet::new();
            for w in &pe.where_ {
                w_refs.extend(expr_refs(w));
            }
            if inter(&w_refs, avail).is_empty() {
                return None;
            }
            Some(Pulled {
                kind,
                side,
                right: pe.from_tree.clone(),
                conds: pe.where_.into_iter().cloned().collect(),
            })
        }
        SubLinkKind::Any => {
            let test = test?;
            if free(&query.plan).is_empty() {
                // OpaqueAny
                if !no_volatile([test]) || inter(&expr_refs(test), avail).is_empty() {
                    return None;
                }
                let out = &query.output;
                let t = fix_output_types(test, out);
                Some(Pulled {
                    kind,
                    side,
                    right: query.plan.clone(),
                    conds: vec![t],
                })
            } else {
                let pe = peel(&query.plan, false)?;
                if !free(pe.from_tree).is_empty() || !no_volatile(pe.where_.iter().copied()) {
                    return None;
                }
                let out = &query.output;
                let mut ok = true;
                let t = test.try_rewrite(&mut |e| match &e.kind {
                    ExprKind::SubLinkOutput(i) => {
                        let Some(id) = out.get(usize::from(*i)) else {
                            ok = false;
                            return Ok(Some(e.clone()));
                        };
                        let rep = match pe.proj {
                            None => LExpr::new(ExprKind::Column(*id), e.ty, e.span),
                            Some(exprs) => match exprs.iter().find(|(c, _)| c == id) {
                                Some((_, x)) => {
                                    if has_sublink(x) || expr_volatility(x) == Volatility::Volatile
                                    {
                                        ok = false;
                                    }
                                    x.clone()
                                }
                                None => {
                                    ok = false;
                                    e.clone()
                                }
                            },
                        };
                        Ok(Some(rep))
                    }
                    _ => Ok(None),
                });
                let t = t.ok()?;
                if !ok || inter(&expr_refs(&t), avail).is_empty() {
                    return None;
                }
                let mut conds: Vec<LExpr> = pe.where_.into_iter().cloned().collect();
                conds.push(t);
                Some(Pulled {
                    kind,
                    side,
                    right: pe.from_tree.clone(),
                    conds,
                })
            }
        }
        _ => None,
    }
}

fn fix_output_types(test: &LExpr, out: &[ColId]) -> LExpr {
    test.try_rewrite(&mut |e| match &e.kind {
        ExprKind::SubLinkOutput(i) => Ok(out
            .get(usize::from(*i))
            .map(|c| LExpr::new(ExprKind::Column(*c), e.ty, e.span))),
        _ => Ok(None),
    })
    .unwrap_or_else(|_| test.clone())
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new(
            "create table t(a int4, b int4, c text); create table u(a int4, d int4); \
             create table v(x int4, y int4);",
        )
        .unwrap()
    }

    fn run(sql: &str) -> String {
        print_logical(&fx().plan_with(sql, &["const_fold", "sublink"]).unwrap())
    }

    #[test]
    fn exists_becomes_semi_and_not_exists_anti() {
        let p = run("select a from t where exists (select 1 from u where u.a = t.a)");
        assert!(p.contains("Join Semi on (#3:u.a = #0:t.a)"), "{p}");
        assert!(!p.contains("SubLink"), "{p}");
        let p = run("select a from t where not exists (select 1 from u where u.a = t.a)");
        assert!(p.contains("Join Anti on (#3:u.a = #0:t.a)"), "{p}");
    }

    #[test]
    fn nested_sublink_referencing_left_stays_in_anti_on() {
        let p = run(
            "select a from t where not exists (select 1 from u where u.a = t.a \
             and u.a in (select x from v where v.y <> t.a))",
        );
        assert!(p.contains("Join Anti"), "{p}");
    }

    #[test]
    fn in_subquery_correlated_and_opaque() {
        let p = run("select a from t where a in (select a from u where u.d = t.b)");
        assert!(
            p.contains("Join Semi on ((#4:u.d = #1:t.b) AND (#0:t.a = #3:u.a))"),
            "{p}"
        );
        let p = run("select a from t where a in (select a from u limit 5)");
        assert!(
            p.contains("Join Semi on (#0:t.a = #3:u.a)") && p.contains("Limit"),
            "{p}"
        );
    }

    #[test]
    fn uncorrelated_exists_or_not_in_and_or_stay() {
        for sql in [
            "select a from t where exists (select 1 from u where u.d = 3)",
            "select a from t where exists (select 1 from u where u.a = t.a) or a = 1",
            "select a from t where a not in (select a from u)",
        ] {
            let p = run(sql);
            assert!(p.contains("SubLink") && !p.contains("Join"), "{sql}\n{p}");
        }
    }

    #[test]
    fn nested_exists_and_join_on() {
        let p = run(
            "select a from t where exists (select 1 from u where u.a = t.a and \
             exists (select 1 from v where v.x = u.d))",
        );
        assert!(p.contains("Join Semi on (#5:v.x = #4:u.d)"), "{p}");
        assert!(!p.contains("SubLink"), "{p}");
        let p = run(
            "select t.a from t join u on u.a = t.a and exists (select 1 from v where v.x = t.b)",
        );
        assert!(p.contains("Join Semi on (#5:v.x = #1:t.b)"), "{p}");
    }
}
