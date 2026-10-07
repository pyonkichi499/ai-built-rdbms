//! 論理プランの書き換えルール（`m4/04` §3.3・§6）。
//!
//! P0-e では各ルールは恒等（`apply` は `Ok(())`）。L1 が 1 つずつ本実装にする。`run` は適用の
//! 順に呼び、`PlannerSettings::validate_plans` が真なら各ルールの後に `LogicalQuery::validate` を通す。

pub mod const_fold;
pub mod join_keys;
pub mod join_order;
pub mod outer_join;
pub mod prune;
pub mod pushdown;
pub mod sublink;
pub mod subquery_pullup;
#[doc(hidden)]
pub mod testutil;

use super::logical::{ColumnArena, LogicalPlan, LogicalQuery};
use super::util::{depth_error, for_each_subquery, take_plan};
use super::{PlanEnv, PlanTrace};
use crate::error::{Error, Result};

#[derive(Debug)]
pub struct RuleCtx<'a> {
    pub env: &'a PlanEnv<'a>,
}

pub type RuleFn = fn(&mut LogicalQuery, &RuleCtx<'_>) -> Result<()>;

/// 適用順。名前は `PlanTrace` と `plan_golden` の見出しに使う。
pub static RULES: &[(&str, RuleFn)] = &[
    ("const_fold", const_fold::apply),
    ("sublink", sublink::apply),
    ("subquery_pullup", subquery_pullup::apply),
    ("outer_join", outer_join::apply),
    ("pushdown", pushdown::apply),
    ("join_keys", join_keys::apply),
    ("join_order", join_order::apply),
    ("prune", prune::apply),
];

/// `RULES` を上から 1 回ずつ適用する。
pub fn run(q: &mut LogicalQuery, env: &PlanEnv<'_>, trace: &mut dyn PlanTrace) -> Result<()> {
    trace.logical("build", q);
    for (name, f) in RULES {
        f(q, &RuleCtx { env })?;
        if env.settings.validate_plans {
            super::validate::validate_logical(q, name)?;
        }
        trace.logical(name, q);
    }
    Ok(())
}

/// テスト用: 名前で指定したルールだけを適用する（差分テスト `m4/04` §11.4）。
pub fn run_only(q: &mut LogicalQuery, env: &PlanEnv<'_>, names: &[&str]) -> Result<()> {
    for name in names {
        let Some((_, f)) = RULES.iter().find(|(n, _)| n == name) else {
            return Err(Error::internal(format!("unknown rule {name}")));
        };
        f(q, &RuleCtx { env })?;
        if env.settings.validate_plans {
            super::validate::validate_logical(q, name)?;
        }
    }
    Ok(())
}

/// 根ごとに適用する関数（引数は根のプランと台帳）。
pub type RootFn<'f> = dyn FnMut(LogicalPlan, &ColumnArena) -> Result<LogicalPlan> + 'f;

/// `q.plan`、各 CTE の plan、式の中の入れ子の `LogicalSubquery.plan` のそれぞれを根として `f` を適用する
/// （`m4/04` §6 の冒頭）。`f` が根を書き換えたあとの木の中の副問い合わせに降りる（外側の根が先）。
pub fn on_roots(q: &mut LogicalQuery, f: &mut RootFn<'_>) -> Result<()> {
    let arena = &q.arena;
    root(&mut q.plan, arena, f, 0)?;
    for c in &mut q.ctes {
        root(&mut c.plan, arena, f, 0)?;
    }
    Ok(())
}

fn root(p: &mut LogicalPlan, arena: &ColumnArena, f: &mut RootFn<'_>, depth: usize) -> Result<()> {
    crate::sql::check_stack_depth()?;
    if depth > super::MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    let t = take_plan(p);
    *p = f(t, arena)?;
    nested(p, arena, f, depth)
}

fn nested(
    p: &mut LogicalPlan,
    arena: &ColumnArena,
    f: &mut RootFn<'_>,
    depth: usize,
) -> Result<()> {
    crate::sql::check_stack_depth()?;
    if depth > super::MAX_PLAN_DEPTH {
        return Err(depth_error());
    }
    for_each_subquery(p, &mut |sq| root(&mut sq.plan, arena, f, depth + 1))?;
    for c in p.children_mut() {
        nested(c, arena, f, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::testutil::Fixture;

    #[test]
    fn every_rule_keeps_plans_valid() {
        let f = Fixture::new(
            "create table t(a int4, b int4, c text); create table u(a int4, d int4, e text); \
             create table v(x int4, y int4); create table p(id int4 primary key, v int4);",
        )
        .unwrap();
        for sql in [
            "select t.c, count(*) from t join u on t.a = u.a where u.d > 1 group by t.c order by 2 desc",
            "select * from t left join u using (a) where u.d is not null",
            "select * from t full join u on t.a = u.a where u.d = 3",
            "select a from t where exists (select 1 from u where u.a = t.a)",
            "select a from t where not exists (select 1 from u where u.a = t.a and u.d > t.b)",
            "select a, (select max(d) from u where u.a = t.a) from t",
            "select a from t where a in (select a from u where d in (select x from v where v.y = t.b))",
            "with x as (select a from t) select * from x join x y on x.a = y.a",
            "with x as materialized (select a, b from t) select x.a from x, u where x.b = u.d",
            "select a from t union select a from u order by 1 limit 3",
            "select a from (select a from t union all select a from u) s where a = 1",
            "select distinct on (a, b) a, b, c from t order by a",
            "update t set b = u.d from u where t.a = u.a and u.d in (select x from v)",
            "delete from t where exists (select 1 from u where u.a = t.a)",
            "insert into t select a, d, e from u where d > 1",
            "select * from (select a, b + 1 as x from t) s join u on s.a = u.a where s.x = 3",
            "select a from t group by a having count(*) > 1 and a > 0",
            "select 1 where exists (select 1 from t)",
            "select t.a from t, u, v where t.a = v.x and u.d = v.y",
            "select t.a from t left join (select a, d + 1 as k from u) s on s.a = t.a where s.k > 2",
            "select a from t where a = any (select a from u) and b > all (select d from u)",
            "select a from t order by b limit 2 offset 1",
            "values (1, 2), (3, 4)",
            "select * from generate_series(1, 3) g where g > 1",
        ] {
            f.plan_all(sql)
                .unwrap_or_else(|e| panic!("{sql}: {} {}", e.sqlstate.code(), e.message));
        }
    }
}
