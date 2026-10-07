//! R4 外部結合の内部結合化（`m4/04` §6.4）。
//!
//! 上の述語が NULL 拡張された行を必ず落とすなら、外部結合を内部結合に（`Full` は `Left` か `Inner` に）する。
//! 上から下へ「上の述語」を持ち回る。

use super::{RuleCtx, on_roots};
use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::util::{ColSet, conjuncts, depth_error, plan_output, rejects_null, take_plan};

pub fn apply(q: &mut LogicalQuery, cx: &RuleCtx<'_>) -> Result<()> {
    let cat = cx.env.catalog;
    on_roots(q, &mut |p, _| reduce(p, &[], cat, 0))
}

fn rejects(quals: &[LExpr], cols: &ColSet, cat: &dyn CatalogReader) -> bool {
    quals.iter().any(|q| rejects_null(q, cols, cat))
}

fn cols_of(p: &LogicalPlan) -> ColSet {
    plan_output(p).into_iter().collect()
}

fn on_terms(on: Option<&LExpr>) -> Vec<LExpr> {
    on.cloned().map(conjuncts).unwrap_or_default()
}

fn reduce(
    plan: LogicalPlan,
    quals: &[LExpr],
    cat: &dyn CatalogReader,
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
            let mut q2 = quals.to_vec();
            q2.extend(conjuncts(predicate.clone()));
            L::Filter {
                input: Box::new(reduce(*input, &q2, cat, d)?),
                predicate,
            }
        }
        L::Join {
            mut kind,
            mut left,
            mut right,
            on,
        } => {
            // Left / Full の変換を先に決める。
            match kind {
                JoinKind::Left if rejects(quals, &cols_of(&right), cat) => kind = JoinKind::Inner,
                JoinKind::Full => {
                    let rl = rejects(quals, &cols_of(&left), cat);
                    let rr = rejects(quals, &cols_of(&right), cat);
                    match (rl, rr) {
                        (true, true) => kind = JoinKind::Inner,
                        (true, false) => kind = JoinKind::Left,
                        (false, true) => {
                            kind = JoinKind::Left;
                            std::mem::swap(&mut left, &mut right);
                        }
                        (false, false) => {}
                    }
                }
                _ => {}
            }
            let (lq, rq): (Vec<LExpr>, Vec<LExpr>) = match kind {
                JoinKind::Inner => {
                    let mut q2 = quals.to_vec();
                    q2.extend(on_terms(on.as_ref()));
                    (q2.clone(), q2)
                }
                JoinKind::Semi | JoinKind::Anti | JoinKind::Left => {
                    (quals.to_vec(), on_terms(on.as_ref()))
                }
                JoinKind::Full => (Vec::new(), Vec::new()),
            };
            L::Join {
                kind,
                left: Box::new(reduce(*left, &lq, cat, d)?),
                right: Box::new(reduce(*right, &rq, cat, d)?),
                on,
            }
        }
        mut other => {
            for c in other.children_mut() {
                let child = take_plan(c);
                *c = reduce(child, &[], cat, d)?;
            }
            other
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new("create table t(a int4, b int4); create table u(a int4, d int4);").unwrap()
    }

    fn run(sql: &str) -> String {
        print_logical(&fx().plan_with(sql, &["const_fold", "outer_join"]).unwrap())
    }

    #[test]
    fn left_join_with_null_rejecting_where_becomes_inner() {
        let p = run("select * from t left join u on t.a = u.a where u.d = 3");
        assert!(p.contains("Join Inner"), "{p}");
        let p = run("select * from t left join u on t.a = u.a where u.d is null");
        assert!(p.contains("Join Left"), "{p}");
        let p = run("select * from t left join u on t.a = u.a where u.d is not null");
        assert!(p.contains("Join Inner"), "{p}");
        let p = run("select * from t left join u on t.a = u.a where coalesce(u.d, 0) = 0");
        assert!(p.contains("Join Left"), "{p}");
    }

    #[test]
    fn full_join_directions() {
        // 右の列を拒否: 右が保存側の Left（左右を入れ替える）。
        let p = run("select * from t full join u on t.a = u.a where u.d = 3");
        assert!(p.contains("Join Left"), "{p}");
        let get_u = p.find("Get u").unwrap();
        let get_t = p.find("Get t").unwrap();
        assert!(get_u < get_t, "{p}");
        let p = run("select * from t full join u on t.a = u.a where t.b = 3");
        assert!(p.contains("Join Left"), "{p}");
        assert!(p.find("Get t").unwrap() < p.find("Get u").unwrap(), "{p}");
        let p = run("select * from t full join u on t.a = u.a where t.b = 3 and u.d = 1");
        assert!(p.contains("Join Inner"), "{p}");
        let p = run("select * from t full join u on t.a = u.a where t.b = 3 or u.d = 1");
        assert!(p.contains("Join Full"), "{p}");
    }
}
