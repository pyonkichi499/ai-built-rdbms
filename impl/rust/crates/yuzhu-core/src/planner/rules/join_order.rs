//! R7 結合順序（`m4/04` §6.7）。
//!
//! 内部結合の島（最大の内部結合の部分木）の葉を、直積を避けるように貪欲法で左深い木に並べ直す。
//! 統計もサイズも使わない（04-D7）。外部結合・Semi・Anti は葉の一部（不透明な単位）として動かさない。

use std::collections::{BTreeSet, HashMap};

use super::join_keys::normalize_on;
use super::{RuleCtx, on_roots};
use crate::catalog::{CatalogReader, builtin};
use crate::error::Result;
use crate::expr::{ColId, ExprKind};
use crate::planner::logical::{JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{
    Volatility, and_all, conjuncts, depth_error, expr_refs, expr_volatility, plan_output, take_plan,
};
use crate::planner::{MAX_JOIN_ISLAND, MAX_PLAN_DEPTH};

pub fn apply(q: &mut LogicalQuery, cx: &RuleCtx<'_>) -> Result<()> {
    let cat = cx.env.catalog;
    on_roots(q, &mut |p, _| walk(p, cat, 0))
}

fn walk(p: LogicalPlan, cat: &dyn CatalogReader, depth: usize) -> Result<LogicalPlan> {
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    if matches!(
        p,
        LogicalPlan::Join {
            kind: JoinKind::Inner,
            ..
        }
    ) {
        return island(p, cat, depth);
    }
    let mut p = p;
    for c in p.children_mut() {
        let child = take_plan(c);
        *c = walk(child, cat, depth + 1)?;
    }
    Ok(p)
}

fn flatten(p: LogicalPlan, leaves: &mut Vec<LogicalPlan>, conds: &mut Vec<LExpr>) {
    match p {
        LogicalPlan::Join {
            kind: JoinKind::Inner,
            left,
            right,
            on,
        } => {
            flatten(*left, leaves, conds);
            flatten(*right, leaves, conds);
            if let Some(on) = on {
                conds.extend(conjuncts(on));
            }
        }
        other => leaves.push(other),
    }
}

/// 等値で 2 つ以上の葉をつなぐ項か。
fn is_equi(c: &LExpr, owner: &HashMap<ColId, usize>) -> bool {
    let ExprKind::Operator { op, args } = &c.kind else {
        return false;
    };
    if op.name != "=" || args.len() != 2 || !builtin::operator_merge_hash(op.oid).1 {
        return false;
    }
    let rels = |e: &LExpr| -> BTreeSet<usize> {
        expr_refs(e)
            .iter()
            .filter_map(|c| owner.get(c).copied())
            .collect()
    };
    let (a, b) = (rels(&args[0]), rels(&args[1]));
    !a.is_empty()
        && !b.is_empty()
        && a.is_disjoint(&b)
        && args
            .iter()
            .all(|x| expr_volatility(x) != Volatility::Volatile && !x.contains_sublink())
}

fn island(p: LogicalPlan, cat: &dyn CatalogReader, depth: usize) -> Result<LogicalPlan> {
    let mut leaves = Vec::new();
    let mut conds = Vec::new();
    flatten(p, &mut leaves, &mut conds);
    // 葉の中の島は別に処理する。
    let mut ls = Vec::with_capacity(leaves.len());
    for l in leaves {
        ls.push(walk(l, cat, depth + 1)?);
    }
    let n = ls.len();
    let mut owner: HashMap<ColId, usize> = HashMap::new();
    for (i, l) in ls.iter().enumerate() {
        for c in plan_output(l) {
            owner.insert(c, i);
        }
    }
    let rels: Vec<BTreeSet<usize>> = conds
        .iter()
        .map(|c| {
            expr_refs(c)
                .iter()
                .filter_map(|x| owner.get(x).copied())
                .collect()
        })
        .collect();
    let equi: Vec<bool> = conds.iter().map(|c| is_equi(c, &owner)).collect();
    let reorder = n <= MAX_JOIN_ISLAND;

    let mut slots: Vec<Option<LogicalPlan>> = ls.into_iter().map(Some).collect();
    let mut cond_slots: Vec<Option<LExpr>> = conds.into_iter().map(Some).collect();
    let mut plan = slots[0]
        .take()
        .unwrap_or(LogicalPlan::Empty { cols: Vec::new() });
    let mut chosen: BTreeSet<usize> = BTreeSet::from([0]);
    let mut remaining: Vec<usize> = (1..n).collect();
    // 1 つの葉だけを参照する項・参照のない項は、最初に置けるものから置く。
    while !remaining.is_empty() {
        let rank = |j: usize| -> u8 {
            let mut best = 2u8;
            for (k, r) in rels.iter().enumerate() {
                if cond_slots[k].is_none() || !r.contains(&j) {
                    continue;
                }
                let mut rest = r.clone();
                rest.remove(&j);
                if rest.is_subset(&chosen) && !rest.is_empty() {
                    best = best.min(u8::from(!equi[k]));
                }
            }
            best
        };
        let pick = if reorder {
            *remaining
                .iter()
                .min_by_key(|j| (rank(**j), **j))
                .unwrap_or(&remaining[0])
        } else {
            remaining[0]
        };
        remaining.retain(|j| *j != pick);
        chosen.insert(pick);
        let mut placed = Vec::new();
        for (k, r) in rels.iter().enumerate() {
            if r.is_subset(&chosen)
                && let Some(c) = cond_slots[k].take()
            {
                placed.push(c);
            }
        }
        let leaf = slots[pick]
            .take()
            .unwrap_or(LogicalPlan::Empty { cols: Vec::new() });
        let on = normalize_on(and_all(placed).as_ref(), &plan, &leaf, cat);
        plan = LogicalPlan::Join {
            kind: JoinKind::Inner,
            left: Box::new(plan),
            right: Box::new(leaf),
            on,
        };
    }
    // 葉が 1 つの島は作らないが、念のため残りの項を最後の結合の ON に足す。
    let rest: Vec<LExpr> = cond_slots.into_iter().flatten().collect();
    if !rest.is_empty()
        && let LogicalPlan::Join { on, .. } = &mut plan
    {
        let mut all: Vec<LExpr> = on.take().map(conjuncts).unwrap_or_default();
        all.extend(rest);
        *on = and_all(all);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn run(sql: &str) -> String {
        let f = Fixture::new(
            "create table a(x int4, y int4); create table b(x int4, y int4); \
             create table c(x int4, y int4);",
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
        ];
        print_logical(&f.plan_with(sql, &names).unwrap())
    }

    fn order(p: &str) -> Vec<char> {
        p.lines()
            .filter_map(|l| l.trim().strip_prefix("Get "))
            .filter_map(|l| l.chars().next())
            .collect()
    }

    #[test]
    fn avoids_cross_products() {
        let p = run("select * from a, b, c where a.x = c.x and b.y = c.y");
        assert_eq!(order(&p), vec!['a', 'c', 'b'], "{p}");
        assert!(!p.contains("Join Inner\n"), "{p}");
    }

    #[test]
    fn keeps_syntactic_order_when_connected() {
        let p = run("select * from a join b on a.x = b.x join c on b.y = c.y");
        assert_eq!(order(&p), vec!['a', 'b', 'c'], "{p}");
    }

    #[test]
    fn outer_join_is_an_opaque_leaf() {
        let p = run("select * from a left join b on a.x = b.x, c where c.x = a.y");
        assert!(p.contains("Join Left"), "{p}");
        assert_eq!(order(&p).len(), 3, "{p}");
    }
}
