//! R1 定数畳み込み（`m4/04` §6.1）。
//!
//! 列・`SubLink`・`SessionValue` を含まず、Immutable な関数・演算子だけの部分木と、リテラルのキャストを
//! 計画時に評価する。`Cast(Env)` と `FnKind::Runtime` / `Context` は畳まない（11 §7.1 C-14）。
//! 評価は `executor::eval::eval_const`（C-30 の依存の例外）。

use super::RuleCtx;
use crate::catalog::{CastMethod, FnKind};
use crate::error::Result;
use crate::executor::eval::eval_const;
use crate::executor::{EvalCtx, NullRuntime, SessionInfo};
use crate::expr::{ExprKind, SubLinkKind};
use crate::planner::MAX_PLAN_DEPTH;
use crate::planner::logical::{JoinKind, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::physical::PhysExpr;
use crate::planner::util::{
    Volatility, depth_error, expr_eq, node_volatility, operator_is_immutable, operator_is_strict,
    recount_cte_refs, take_plan,
};
use crate::types::Datum;

/// `q.plan`、`q.ctes[*].plan`、式の中の入れ子の `LogicalSubquery` のすべてに適用する。
pub fn apply(q: &mut LogicalQuery, cx: &RuleCtx<'_>) -> Result<()> {
    let session = SessionInfo {
        current_user: String::new(),
        session_user: String::new(),
        database: String::new(),
        current_schema: None,
    };
    let runtime = NullRuntime;
    let f = Folder {
        ctx: EvalCtx {
            session: &session,
            catalog: cx.env.catalog,
            runtime: &runtime,
            type_env: cx.env.type_env,
        },
    };
    let p = take_plan(&mut q.plan);
    q.plan = f.plan(p, 0, false)?;
    for c in &mut q.ctes {
        let p = take_plan(&mut c.plan);
        c.plan = f.plan(p, 0, false)?;
    }
    recount_cte_refs(q);
    Ok(())
}

struct Folder<'a> {
    ctx: EvalCtx<'a>,
}

fn lit(e: &LExpr) -> Option<&Datum> {
    match &e.kind {
        ExprKind::Literal(d) => Some(d),
        _ => None,
    }
}

fn all_lit(xs: &[LExpr]) -> bool {
    xs.iter().all(|x| lit(x).is_some())
}

fn dummy() -> LExpr {
    LExpr::bool_lit(true)
}

impl Folder<'_> {
    fn eval(&self, e: &LExpr) -> Result<LExpr> {
        let span = e.span;
        let ty = e.ty;
        let phys: PhysExpr = e.try_map(&mut |_n: &LExpr| Ok(None))?;
        let d = eval_const(&phys, &Vec::new(), &self.ctx).map_err(|err| {
            if span == crate::error::Span::default() {
                err
            } else {
                err.with_span(span)
            }
        })?;
        Ok(LExpr::new(ExprKind::Literal(d), ty, span))
    }

    fn null_of(e: &LExpr) -> LExpr {
        LExpr::new(ExprKind::Literal(Datum::Null), e.ty, e.span)
    }

    fn many(&self, xs: Vec<LExpr>) -> Result<Vec<LExpr>> {
        xs.into_iter().map(|x| self.expr(x)).collect()
    }

    #[allow(clippy::boxed_local)]
    fn boxed(&self, x: Box<LExpr>) -> Result<Box<LExpr>> {
        Ok(Box::new(self.expr(*x)?))
    }

    #[allow(clippy::too_many_lines)]
    fn expr(&self, e: LExpr) -> Result<LExpr> {
        use ExprKind as K;
        let LExpr { kind, ty, span } = e;
        let mk = |kind| LExpr { kind, ty, span };
        match kind {
            K::Literal(_) | K::Column(_) | K::SessionValue(_) | K::SubLinkOutput(_) => Ok(mk(kind)),
            K::Operator { op, args } => {
                let args = self.many(args)?;
                let all = all_lit(&args);
                let e = mk(K::Operator { op, args });
                if operator_is_strict(op) && Self::any_null(&e) {
                    return Ok(Self::null_of(&e));
                }
                if all && operator_is_immutable(op) {
                    return self.eval(&e);
                }
                Ok(e)
            }
            K::Function { func, args } => {
                let args = self.many(args)?;
                let all = all_lit(&args);
                let e = mk(K::Function { func, args });
                if func.strict && Self::any_null(&e) {
                    return Ok(Self::null_of(&e));
                }
                if all
                    && matches!(func.kind, FnKind::Pure(_))
                    && node_volatility(&e) == Volatility::Immutable
                {
                    return self.eval(&e);
                }
                Ok(e)
            }
            K::Cast {
                expr,
                method,
                implicit,
            } => {
                let expr = self.boxed(expr)?;
                let foldable = lit(&expr).is_some() && !matches!(method, CastMethod::Env(_));
                let e = mk(K::Cast {
                    expr,
                    method,
                    implicit,
                });
                if foldable { self.eval(&e) } else { Ok(e) }
            }
            K::CoerceTypmod { expr, explicit } => {
                let expr = self.boxed(expr)?;
                let foldable = lit(&expr).is_some();
                let e = mk(K::CoerceTypmod { expr, explicit });
                if foldable { self.eval(&e) } else { Ok(e) }
            }
            K::And(args) => self.and_or(args, true, mk),
            K::Or(args) => self.and_or(args, false, mk),
            K::Not(x) => {
                let x = self.expr(*x)?;
                match x.kind {
                    K::Literal(Datum::Null) => Ok(mk(K::Literal(Datum::Null))),
                    K::Literal(Datum::Bool(b)) => Ok(mk(K::Literal(Datum::Bool(!b)))),
                    K::Not(y) => Ok(*y),
                    K::Operator { op, args }
                        if args.len() == 2
                            && operator_is_strict(op)
                            && crate::catalog::builtin::operator_meta(op.oid)
                                .is_some_and(|m| m.negate != 0)
                            && crate::catalog::builtin::operator_meta(op.oid)
                                .and_then(|m| crate::catalog::builtin::operator_by_oid(m.negate))
                                .is_some() =>
                    {
                        let neg = crate::catalog::builtin::operator_meta(op.oid)
                            .and_then(|m| crate::catalog::builtin::operator_by_oid(m.negate));
                        match neg {
                            Some(nop) => Ok(mk(K::Operator { op: nop, args })),
                            None => Ok(mk(K::Not(Box::new(LExpr {
                                kind: K::Operator { op, args },
                                ..x
                            })))),
                        }
                    }
                    other => Ok(mk(K::Not(Box::new(LExpr { kind: other, ..x })))),
                }
            }
            K::IsNull(x) => {
                let x = self.boxed(x)?;
                let e = mk(K::IsNull(x));
                self.eval_if_children_lit(e)
            }
            K::IsNotNull(x) => {
                let x = self.boxed(x)?;
                let e = mk(K::IsNotNull(x));
                self.eval_if_children_lit(e)
            }
            K::BoolTest { expr, test } => {
                let expr = self.boxed(expr)?;
                let e = mk(K::BoolTest { expr, test });
                self.eval_if_children_lit(e)
            }
            K::Case { arms, else_result } => {
                let mut kept = Vec::new();
                let mut else_r = else_result;
                let mut decided = false;
                for (c, r) in arms {
                    let c = self.expr(c)?;
                    match lit(&c) {
                        Some(Datum::Bool(true)) => {
                            else_r = Some(self.boxed(Box::new(r))?);
                            decided = true;
                            break;
                        }
                        Some(Datum::Bool(false) | Datum::Null) => {}
                        _ => kept.push((c, self.expr(r)?)),
                    }
                }
                if !decided {
                    else_r = else_r.map(|x| self.boxed(x)).transpose()?;
                }
                if kept.is_empty() {
                    return Ok(match else_r {
                        Some(x) => *x,
                        None => mk(K::Literal(Datum::Null)),
                    });
                }
                Ok(mk(K::Case {
                    arms: kept,
                    else_result: else_r,
                }))
            }
            K::Coalesce(args) => {
                let mut kept: Vec<LExpr> = Vec::new();
                for a in args {
                    let a = self.expr(a)?;
                    match lit(&a) {
                        Some(Datum::Null) => {}
                        Some(_) => {
                            kept.push(a);
                            break;
                        }
                        None => kept.push(a),
                    }
                }
                Ok(match kept.len() {
                    0 => mk(K::Literal(Datum::Null)),
                    1 => kept.remove(0),
                    _ => mk(K::Coalesce(kept)),
                })
            }
            K::NullIf { left, right, eq_op } => {
                let (left, right) = (self.boxed(left)?, self.boxed(right)?);
                // PG は厳格な演算子に NULL 定数が渡ると、他の引数を評価せずに NULL へ畳む（右辺の 22003 などを出さない）。
                if matches!(lit(&left), Some(Datum::Null)) {
                    return Ok(mk(K::Literal(Datum::Null)));
                }
                let e = mk(K::NullIf { left, right, eq_op });
                if operator_is_immutable(eq_op) {
                    self.eval_if_children_lit(e)
                } else {
                    Ok(e)
                }
            }
            K::DistinctFrom {
                left,
                right,
                eq_op,
                negated,
            } => {
                let (left, right) = (self.boxed(left)?, self.boxed(right)?);
                let e = mk(K::DistinctFrom {
                    left,
                    right,
                    eq_op,
                    negated,
                });
                if operator_is_immutable(eq_op) {
                    self.eval_if_children_lit(e)
                } else {
                    Ok(e)
                }
            }
            K::MinMax {
                greatest,
                args,
                cmp,
            } => {
                let args = self.many(args)?;
                let e = mk(K::MinMax {
                    greatest,
                    args,
                    cmp,
                });
                if operator_is_immutable(cmp) {
                    self.eval_if_children_lit(e)
                } else {
                    Ok(e)
                }
            }
            K::Like {
                expr,
                pattern,
                escape,
                negated,
                case_insensitive,
            } => {
                let expr = self.boxed(expr)?;
                let pattern = self.boxed(pattern)?;
                let escape = escape.map(|x| self.boxed(x)).transpose()?;
                let e = mk(K::Like {
                    expr,
                    pattern,
                    escape,
                    negated,
                    case_insensitive,
                });
                self.eval_if_children_lit(e)
            }
            K::InList {
                expr,
                list,
                eq_op,
                negated,
            } => {
                let expr = self.boxed(expr)?;
                let list = self.many(list)?;
                let e = mk(K::InList {
                    expr,
                    list,
                    eq_op,
                    negated,
                });
                if operator_is_immutable(eq_op) {
                    self.eval_if_children_lit(e)
                } else {
                    Ok(e)
                }
            }
            K::Aggregate(mut call) => {
                call.args = self.many(std::mem::take(&mut call.args))?;
                call.filter = call.filter.take().map(|f| self.expr(f)).transpose()?;
                for k in &mut call.order_by {
                    let e =
                        std::mem::replace(&mut k.expr, mk(K::Literal(crate::types::Datum::Null)));
                    k.expr = self.expr(e)?;
                }
                Ok(mk(K::Aggregate(call)))
            }
            K::SubLink {
                kind,
                test,
                mut query,
            } => {
                let test = test.map(|t| self.boxed(t)).transpose()?;
                let p = take_plan(&mut query.plan);
                query.plan = self.plan(p, 0, false)?;
                if matches!(query.plan, LogicalPlan::Empty { .. }) {
                    return Ok(match kind {
                        SubLinkKind::Exists | SubLinkKind::Any => {
                            mk(K::Literal(Datum::Bool(false)))
                        }
                        SubLinkKind::All => mk(K::Literal(Datum::Bool(true))),
                        SubLinkKind::Scalar => mk(K::Literal(Datum::Null)),
                    });
                }
                Ok(mk(K::SubLink { kind, test, query }))
            }
        }
    }

    fn any_null(e: &LExpr) -> bool {
        e.children()
            .into_iter()
            .any(|c| lit(c).is_some_and(Datum::is_null))
    }

    /// 子がすべてリテラルなら評価する（子のない式は対象外）。
    fn eval_if_children_lit(&self, e: LExpr) -> Result<LExpr> {
        let kids = e.children();
        if !kids.is_empty() && kids.iter().all(|c| lit(c).is_some()) {
            self.eval(&e)
        } else {
            Ok(e)
        }
    }

    /// `is_and`: And（false で打ち切り、true を捨てる）/ Or（true で打ち切り、false を捨てる）。
    fn and_or(
        &self,
        args: Vec<LExpr>,
        is_and: bool,
        mk: impl Fn(
            ExprKind<crate::expr::ColId, Box<crate::planner::logical::LogicalSubquery>>,
        ) -> LExpr,
    ) -> Result<LExpr> {
        let mut out: Vec<LExpr> = Vec::new();
        let mut stack: Vec<LExpr> = args.into_iter().rev().collect();
        while let Some(a) = stack.pop() {
            let a = self.expr(a)?;
            match &a.kind {
                ExprKind::Literal(Datum::Bool(b)) => {
                    if *b != is_and {
                        return Ok(mk(ExprKind::Literal(Datum::Bool(*b))));
                    }
                }
                ExprKind::And(_) if is_and => {
                    if let ExprKind::And(inner) = a.kind {
                        stack.extend(inner.into_iter().rev().map(Self::already));
                    }
                }
                ExprKind::Or(_) if !is_and => {
                    if let ExprKind::Or(inner) = a.kind {
                        stack.extend(inner.into_iter().rev().map(Self::already));
                    }
                }
                _ => {
                    let dup = node_volatility_max(&a) != Volatility::Volatile
                        && out.iter().any(|o| expr_eq(o, &a));
                    if !dup {
                        out.push(a);
                    }
                }
            }
        }
        Ok(match out.len() {
            0 => mk(ExprKind::Literal(Datum::Bool(is_and))),
            1 => out.remove(0),
            _ => mk(if is_and {
                ExprKind::And(out)
            } else {
                ExprKind::Or(out)
            }),
        })
    }

    /// 平坦化で取り出した子（すでに畳み済み）。もう一度畳んでも結果は同じなのでそのまま返す。
    fn already(e: LExpr) -> LExpr {
        e
    }

    // ---- プラン ----

    fn exprs_of(&self, p: &mut LogicalPlan) -> Result<()> {
        for e in p.exprs_mut() {
            let x = std::mem::replace(e, dummy());
            *e = self.expr(x)?;
        }
        Ok(())
    }

    /// `keep_sort`: この `Sort` の定数キーを落とさない（DISTINCT ON の入力）。
    #[allow(clippy::too_many_lines)]
    fn plan(&self, mut p: LogicalPlan, depth: usize, keep_sort: bool) -> Result<LogicalPlan> {
        use LogicalPlan as L;
        crate::sql::check_stack_depth()?;
        if depth > MAX_PLAN_DEPTH {
            return Err(depth_error());
        }
        let under_distinct_on = matches!(&p, L::Distinct { on: Some(_), .. });
        for c in p.children_mut() {
            let child = take_plan(c);
            *c = self.plan(child, depth + 1, under_distinct_on)?;
        }
        self.exprs_of(&mut p)?;
        Ok(match p {
            L::Filter { input, predicate } => {
                let predicate = order_filter(predicate);
                match &predicate.kind {
                    ExprKind::Literal(Datum::Bool(true)) => *input,
                    ExprKind::Literal(Datum::Bool(false) | Datum::Null) => L::Empty {
                        cols: input.output_cols(),
                    },
                    _ if matches!(*input, L::Empty { .. }) => *input,
                    _ => match *input {
                        L::Result {
                            one_time_filter: None,
                            cols,
                        } => L::Result {
                            one_time_filter: Some(predicate),
                            cols,
                        },
                        other => L::Filter {
                            input: Box::new(other),
                            predicate,
                        },
                    },
                }
            }
            L::Join {
                kind,
                left,
                right,
                on,
            } => {
                let on_lit = on.as_ref().and_then(|o| lit(o).cloned());
                let l_empty = matches!(*left, L::Empty { .. });
                let r_empty = matches!(*right, L::Empty { .. });
                let on_false = matches!(on_lit, Some(Datum::Bool(false) | Datum::Null));
                let on_true = matches!(on_lit, Some(Datum::Bool(true)));
                let node = |kind, left, right, on| L::Join {
                    kind,
                    left,
                    right,
                    on,
                };
                match kind {
                    JoinKind::Inner | JoinKind::Semi if on_false || l_empty || r_empty => {
                        let j = node(kind, left, right, on);
                        L::Empty {
                            cols: j.output_cols(),
                        }
                    }
                    JoinKind::Left | JoinKind::Anti if l_empty => {
                        let j = node(kind, left, right, on);
                        L::Empty {
                            cols: j.output_cols(),
                        }
                    }
                    JoinKind::Anti if r_empty => *left,
                    JoinKind::Inner if is_unit(&right) => *left,
                    JoinKind::Inner if is_unit(&left) => *right,
                    JoinKind::Inner if on_true => node(kind, left, right, None),
                    _ => node(kind, left, right, on),
                }
            }
            L::Project { input, exprs } => {
                if matches!(*input, L::Empty { .. }) {
                    L::Empty {
                        cols: exprs.iter().map(|(c, _)| *c).collect(),
                    }
                } else {
                    L::Project { input, exprs }
                }
            }
            L::Sort { input, mut keys } => {
                if !keep_sort {
                    let consts: Vec<crate::expr::ColId> = match &*input {
                        L::Project { exprs, .. } => exprs
                            .iter()
                            .filter(|(_, e)| lit(e).is_some())
                            .map(|(c, _)| *c)
                            .collect(),
                        _ => Vec::new(),
                    };
                    keys.retain(|k| match &k.expr.kind {
                        ExprKind::Literal(_) => false,
                        ExprKind::Column(c) => !consts.contains(c),
                        _ => true,
                    });
                }
                if keys.is_empty() || matches!(*input, L::Empty { .. }) {
                    *input
                } else {
                    L::Sort { input, keys }
                }
            }
            L::Distinct { input, on } => {
                if matches!(*input, L::Empty { .. }) {
                    *input
                } else {
                    L::Distinct { input, on }
                }
            }
            L::Limit {
                input,
                limit,
                offset,
            } => {
                // 負の LIMIT / OFFSET は空の入力でもエラーにする（PG と同じ）ので、
                // 非負の定数でない場合は Limit を残す。
                let harmless = |e: &Option<LExpr>| match e {
                    None => true,
                    Some(e) => match lit(e) {
                        Some(Datum::Null) => true,
                        Some(d) => d.as_i64().is_some_and(|v| v >= 0),
                        None => false,
                    },
                };
                if matches!(*input, L::Empty { .. }) && harmless(&limit) && harmless(&offset) {
                    *input
                } else {
                    L::Limit {
                        input,
                        limit,
                        offset,
                    }
                }
            }
            L::Aggregate {
                input,
                group_by,
                aggs,
            } => {
                if !group_by.is_empty() && matches!(*input, L::Empty { .. }) {
                    L::Empty {
                        cols: group_by
                            .iter()
                            .map(|(c, _)| *c)
                            .chain(aggs.iter().map(|(c, _)| *c))
                            .collect(),
                    }
                } else {
                    L::Aggregate {
                        input,
                        group_by,
                        aggs,
                    }
                }
            }
            L::Result {
                one_time_filter,
                cols,
            } => match one_time_filter.as_ref().and_then(lit) {
                Some(Datum::Bool(false) | Datum::Null) => L::Empty { cols },
                Some(Datum::Bool(true)) => L::Result {
                    one_time_filter: None,
                    cols,
                },
                _ => L::Result {
                    one_time_filter,
                    cols,
                },
            },
            other => other,
        })
    }
}

fn node_volatility_max(e: &LExpr) -> Volatility {
    crate::planner::util::expr_volatility(e)
}

/// FROM なしの派生表（1 行・列なし・条件なし）。
fn is_unit(p: &LogicalPlan) -> bool {
    matches!(
        p,
        LogicalPlan::Result {
            one_time_filter: None,
            cols
        } if cols.is_empty()
    )
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use crate::planner::print::print_logical;
    use crate::planner::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new("create table t(a int4, b int4, c text); create table u(a int4, d int4);")
            .unwrap()
    }

    fn fold(sql: &str) -> String {
        print_logical(&fx().plan_with(sql, &["const_fold"]).unwrap())
    }

    fn err(sql: &str) -> String {
        let e = fx().plan_with(sql, &["const_fold"]).unwrap_err();
        e.sqlstate.code().to_owned()
    }

    #[test]
    fn folds_constant_subtrees() {
        let p = fold("select a from t where a = 1 + 2");
        assert!(p.contains("Filter (#0:t.a = 3)"), "{p}");
    }

    #[test]
    fn and_or_short_circuit_and_errors() {
        assert_eq!(fold("select 1 where false and 1/0 = 1"), "Empty\n");
        assert_eq!(err("select 1/0 where false"), "22012");
        assert_eq!(err("select a from t where a = 1 or 1/0 = 1"), "22012");
        let p = fold("select a from t where a > 0 and true");
        assert!(p.contains("Filter (#0:t.a > 0)"), "{p}");
    }

    #[test]
    fn where_and_const_null_is_empty() {
        assert_eq!(
            fold("select a from t where a * -54 = 2 and null"),
            "Empty\n"
        );
        assert_eq!(
            fold("select a from t where a > null and a * -54 = 2"),
            "Empty\n"
        );
    }

    #[test]
    fn negative_limit_over_empty_is_kept() {
        assert_eq!(fold("select a from t where false limit 1"), "Empty\n");
        assert!(fold("select a from t where false limit -1").contains("Limit"));
        assert!(fold("select a from t where false offset -1").contains("Limit"));
    }

    #[test]
    fn filter_false_becomes_empty() {
        assert_eq!(fold("select a from t where false"), "Empty\n");
        assert_eq!(fold("select count(*) from t having false"), "Empty\n");
        assert_eq!(
            fold("select a from t where exists (select 1 from u where false)"),
            "Empty\n"
        );
    }

    #[test]
    fn not_uses_the_negator() {
        let p = fold("select a from t where not (a > 1)");
        assert!(p.contains("(#0:t.a <= 1)"), "{p}");
    }

    #[test]
    fn case_and_coalesce_are_lazy() {
        assert!(fold("select case when true then 1 else 1/0 end").contains(":= 1]"));
        assert!(fold("select coalesce(1, 1/0)").contains(":= 1]"));
    }

    #[test]
    fn constant_sort_keys_are_dropped_and_dup_conjuncts_merged() {
        let p = fold("select a from t order by 1+1 limit 3");
        assert!(!p.contains("Sort"), "{p}");
        let p = fold("select a from t where a is not null and a is not null");
        assert_eq!(p.matches("IS NOT NULL").count(), 1, "{p}");
    }
}

fn is_null_lit(e: &LExpr) -> bool {
    matches!(e.kind, ExprKind::Literal(Datum::Null))
}

/// `NOT e` が定数 NULL になるか（PG は NOT を OR（ド・モルガン）と IN 一覧へ押し込む。
/// NULL を含む IN 一覧の否定は `<> NULL` になる）。
fn negation_is_null(e: &LExpr) -> bool {
    match &e.kind {
        ExprKind::Or(args) => args.iter().any(negation_is_null),
        ExprKind::InList {
            list,
            negated: false,
            ..
        } => list.iter().any(is_null_lit),
        _ => is_null_lit(e),
    }
}

/// WHERE では NULL は false と同じなので、最上位の AND の項が定数 NULL / false なら
/// PG は全体を false に畳み、他の項を評価しない。
fn dead_qual(q: &LExpr) -> bool {
    is_null_lit(q)
        || matches!(q.kind, ExprKind::Literal(Datum::Bool(false)))
        || match &q.kind {
            ExprKind::InList {
                list,
                negated: true,
                ..
            } => list.iter().any(is_null_lit),
            ExprKind::Not(inner) => negation_is_null(inner),
            _ => false,
        }
}

/// 近似のプランナコスト（演算子・関数の呼び出し数。サブクエリは最後に回す）。
fn qual_cost(e: &LExpr) -> usize {
    let kids: usize = e.children().into_iter().map(qual_cost).sum();
    match &e.kind {
        ExprKind::Operator { .. }
        | ExprKind::Function { .. }
        | ExprKind::NullIf { .. }
        | ExprKind::DistinctFrom { .. }
        | ExprKind::Like { .. } => 1 + kids,
        ExprKind::InList { list, .. } => kids + list.len(),
        ExprKind::SubLink { .. } => 1000 + kids,
        _ => kids,
    }
}

/// PG の `order_qual_clauses` と同じく、WHERE の最上位 AND を平坦化して安い項から評価する（安定ソート）。
/// 定数 NULL / false の項があれば全体を false にする。
fn order_filter(p: LExpr) -> LExpr {
    if !matches!(p.kind, ExprKind::And(_)) {
        return if dead_qual(&p) {
            LExpr::new(ExprKind::Literal(Datum::Bool(false)), p.ty, p.span)
        } else {
            p
        };
    }
    let (ty, span) = (p.ty, p.span);
    let mut quals = crate::planner::util::conjuncts(p);
    if quals.iter().any(dead_qual) {
        return LExpr::new(ExprKind::Literal(Datum::Bool(false)), ty, span);
    }
    quals.sort_by_cached_key(qual_cost);
    match quals.len() {
        0 => LExpr::new(ExprKind::Literal(Datum::Bool(true)), ty, span),
        1 => quals.remove(0),
        _ => LExpr::new(ExprKind::And(quals), ty, span),
    }
}
