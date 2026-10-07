//! R3 派生表の展開（`m4/04` §6.3）。
//!
//! 派生表・インライン CTE の `Project` の層を取り除き、中の `Filter` / `Join` / 走査を親の木の一部にする。
//! `ColId` はパススルーで引き継がれるので、置換は「取り除いた `Project` より上の、再定義されるまでの式」
//! にだけ適用する（下から上へ置換表を返す）。

use std::collections::HashMap;

use super::{RuleCtx, on_roots};
use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::expr::{ColId, ExprKind};
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{
    ColSet, Volatility, depth_error, expr_volatility, nulls_out, plan_output, take_plan,
};

type Subst = HashMap<ColId, LExpr>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    None,
    Join,
    Filter,
    Aggregate,
    Project,
    Other,
}

pub fn apply(q: &mut LogicalQuery, cx: &RuleCtx<'_>) -> Result<()> {
    let catalog = cx.env.catalog;
    on_roots(q, &mut |p, _| {
        pull(p, Parent::None, false, catalog, 0).map(|(p, _)| p)
    })
}

fn removable(
    exprs: &[(ColId, LExpr)],
    input: &LogicalPlan,
    nullable: bool,
    cat: &dyn CatalogReader,
) -> bool {
    if exprs
        .iter()
        .any(|(_, e)| expr_volatility(e) == Volatility::Volatile || e.contains_sublink())
    {
        return false;
    }
    if !nullable {
        return true;
    }
    let cols: ColSet = plan_output(input).into_iter().collect();
    exprs
        .iter()
        .all(|(_, e)| matches!(e.kind, ExprKind::Column(_)) || nulls_out(e, &cols, cat))
}

fn pull(
    mut plan: LogicalPlan,
    parent: Parent,
    nullable: bool,
    cat: &dyn CatalogReader,
    depth: usize,
) -> Result<(LogicalPlan, Subst)> {
    use LogicalPlan as L;
    crate::sql::check_stack_depth()?;
    if depth > MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    let me = match &plan {
        L::Join { .. } => Parent::Join,
        L::Filter { .. } => Parent::Filter,
        L::Aggregate { .. } => Parent::Aggregate,
        L::Project { .. } => Parent::Project,
        _ => Parent::Other,
    };
    // 1. 子
    let mut merged = Subst::new();
    let mut left_subst = Subst::new();
    let kind = if let L::Join { kind, .. } = &plan {
        Some(*kind)
    } else {
        None
    };
    for (i, c) in plan.children_mut().into_iter().enumerate() {
        let child_nullable = match kind {
            Some(JoinKind::Left) => nullable || i == 1,
            Some(JoinKind::Full) => true,
            _ => nullable,
        };
        let child = take_plan(c);
        let (np, s) = pull(child, me, child_nullable, cat, depth + 1)?;
        *c = np;
        if i == 0 {
            s.clone_into(&mut left_subst);
        }
        merged.extend(s);
    }
    // 2. このノードの式
    if !merged.is_empty() {
        for e in plan.exprs_mut() {
            *e = e.substitute(&merged)?;
        }
        // 副問い合わせの自由な列（外側の参照）は `substitute` が `SubLink` の中まで置換する。
    }
    // 3. 取り除ける Project
    if let L::Project { input, exprs } = &plan
        && matches!(
            parent,
            Parent::Join | Parent::Filter | Parent::Aggregate | Parent::Project
        )
        && removable(exprs, input, nullable, cat)
    {
        let L::Project { input, exprs } = plan else {
            unreachable!("matched above")
        };
        for (id, e) in exprs {
            if !matches!(&e.kind, ExprKind::Column(c) if *c == id) {
                merged.insert(id, e);
            }
        }
        return Ok((*input, merged));
    }
    // 4. 戻りの置換表
    let ret = match &plan {
        L::Filter { .. } | L::Sort { .. } | L::Limit { .. } | L::Distinct { .. } => merged,
        L::Join { kind, .. } => match kind {
            JoinKind::Semi | JoinKind::Anti => left_subst,
            _ => merged,
        },
        _ => Subst::new(),
    };
    Ok((plan, ret))
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new("create table t(a int4, b int4, c text); create table u(a int4, d int4);")
            .unwrap()
    }

    fn run(sql: &str) -> String {
        print_logical(
            &fx()
                .plan_with(sql, &["const_fold", "subquery_pullup"])
                .unwrap(),
        )
    }

    #[test]
    fn derived_table_project_is_removed() {
        let p = run("select s.a from (select a, b + 1 as x from t where b > 1) s where s.x = 3");
        assert_eq!(p.matches("Project").count(), 1, "{p}");
        assert!(p.contains("((#1:t.b + 1) = 3)"), "{p}");
    }

    #[test]
    fn root_project_and_volatile_stay() {
        let p = run("select a from t");
        assert_eq!(p.matches("Project").count(), 1, "{p}");
        let p = run("select s.r is null from (select pg_sleep(0) as r from t) s");
        assert_eq!(p.matches("Project").count(), 2, "{p}");
    }

    #[test]
    fn null_side_needs_null_propagation() {
        // 外部結合の NULL 側の定数は NULL 拡張されないので展開しない。
        let p = run("select t.a, s.k from t left join (select a, 1 as k from u) s on s.a = t.a");
        assert_eq!(p.matches("Project").count(), 2, "{p}");
        // `d + 1` は入力が NULL なら NULL なので展開できる。
        let p =
            run("select t.a, s.k from t left join (select a, d + 1 as k from u) s on s.a = t.a");
        assert_eq!(p.matches("Project").count(), 1, "{p}");
    }
}
