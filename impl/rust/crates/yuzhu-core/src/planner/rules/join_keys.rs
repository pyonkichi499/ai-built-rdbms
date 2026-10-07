//! R6 結合キーの正規化（`m4/04` §6.6）。
//!
//! `Join.on` を「等値キー… ++ 残り…」の順に並べ、キーを `左の式 = 右の式` の向きにそろえる。

use super::{RuleCtx, on_roots};
use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::expr::ExprKind;
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{ColSet, and_all, depth_error, plan_output, split_on};
use crate::types::SqlType;

pub fn apply(q: &mut LogicalQuery, cx: &RuleCtx<'_>) -> Result<()> {
    let cat = cx.env.catalog;
    on_roots(q, &mut |mut p, _| {
        walk(&mut p, cat, 0)?;
        Ok(p)
    })
}

fn walk(p: &mut LogicalPlan, cat: &dyn CatalogReader, depth: usize) -> Result<()> {
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    if let LogicalPlan::Join {
        left, right, on, ..
    } = p
        && on.is_some()
    {
        *on = normalize_on(on.as_ref(), left, right, cat);
    }
    for c in p.children_mut() {
        walk(c, cat, depth + 1)?;
    }
    Ok(())
}

/// `on` を「キー ++ 残り」に並べ直す（R7 も使う）。
pub fn normalize_on(
    on: Option<&LExpr>,
    left: &LogicalPlan,
    right: &LogicalPlan,
    cat: &dyn CatalogReader,
) -> Option<LExpr> {
    let l: ColSet = plan_output(left).into_iter().collect();
    let r: ColSet = plan_output(right).into_iter().collect();
    let s = split_on(on, &l, &r, cat);
    let mut terms: Vec<LExpr> = s
        .keys
        .into_iter()
        .map(|k| {
            LExpr::new(
                ExprKind::Operator {
                    op: k.op,
                    args: vec![k.left, k.right],
                },
                SqlType::BOOL,
                crate::error::Span::default(),
            )
        })
        .collect();
    terms.extend(s.residual);
    and_all(terms)
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn run(sql: &str) -> String {
        let f = Fixture::new("create table t(a int4, b int4); create table u(a int4, d int4);")
            .unwrap();
        print_logical(&f.plan_with(sql, &["join_keys"]).unwrap())
    }

    #[test]
    fn keys_first_and_oriented_left_to_right() {
        let p = run("select * from t join u on t.b < u.d and u.a = t.a");
        assert!(
            p.contains("Join Inner on ((#0:t.a = #2:u.a) AND (#1:t.b < #3:u.d))"),
            "{p}"
        );
    }

    #[test]
    fn non_equi_and_same_side_terms_stay_residual() {
        let p = run("select * from t join u on t.a = t.b and t.a > u.a");
        assert!(
            p.contains("on ((#0:t.a = #1:t.b) AND (#0:t.a > #2:u.a))"),
            "{p}"
        );
    }
}
