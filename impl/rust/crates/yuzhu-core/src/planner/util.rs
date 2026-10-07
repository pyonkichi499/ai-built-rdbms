//! 論理プランの共通部品（`m4/04` §3.2。持ち主は L1。L2 も使う）。

use std::collections::{BTreeSet, HashMap};

use super::logical::{LExpr, LogicalPlan, LogicalQuery, LogicalSubquery};
use crate::catalog::{BuiltinOperator, CastContext, CastMethod, CatalogReader, builtin};
use crate::error::Result;
use crate::expr::{ColId, Expr, ExprKind};
use crate::types::{Oid, SqlType};

/// 反復順が決まる列の集合（プランを決定的にする）。
pub type ColSet = BTreeSet<ColId>;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

// ---- 式 ----

/// 最上位の `And` を平坦化して conjunct の列にする。リテラル `true` は捨てる（空なら空）。
pub fn conjuncts(e: LExpr) -> Vec<LExpr> {
    let mut out = Vec::new();
    push_conjuncts(e, &mut out);
    out
}

fn push_conjuncts(e: LExpr, out: &mut Vec<LExpr>) {
    match e.kind {
        ExprKind::And(parts) => parts.into_iter().for_each(|p| push_conjuncts(p, out)),
        ExprKind::Literal(crate::types::Datum::Bool(true)) => {}
        kind => out.push(LExpr { kind, ..e }),
    }
}

/// 空 → `None`、1 個 → そのまま、2 個以上 → `And`。
pub fn and_all(parts: Vec<LExpr>) -> Option<LExpr> {
    if parts.is_empty() {
        None
    } else {
        Some(LExpr::and_all(parts))
    }
}

/// 参照する `ColId`（`SubLink` の中の自由な列を含む。`SubLinkOutput` は含まない）。
pub fn expr_refs(e: &LExpr) -> ColSet {
    let mut out = ColSet::new();
    e.free_cols(&mut out);
    out
}

fn proc_volatility(oid: Oid) -> Volatility {
    match builtin::proc_by_oid(oid).map(|p| p.volatility) {
        Some('i') => Volatility::Immutable,
        Some('s') => Volatility::Stable,
        _ => Volatility::Volatile,
    }
}

/// ノード 1 つ（子は見ない）の揮発性（`m4/04` §3.2.1）。`SubLink` の中のプランは見ない（Stable）。
pub fn node_volatility<C, Q>(n: &Expr<C, Q>) -> Volatility {
    match &n.kind {
        ExprKind::Function { func, .. } => proc_volatility(func.oid),
        ExprKind::Operator { op, .. } => builtin::operator_meta(op.oid)
            .map_or(Volatility::Volatile, |m| proc_volatility(m.proc_oid)),
        ExprKind::Cast { method, expr, .. } => match method {
            CastMethod::Function(_) => builtin::find_cast(expr.ty.oid, n.ty.oid)
                .map_or(Volatility::Volatile, |c| proc_volatility(c.func_oid)),
            CastMethod::Binary => Volatility::Immutable,
            CastMethod::InOut | CastMethod::Env(_) => Volatility::Stable,
        },
        ExprKind::SessionValue(_) | ExprKind::SubLink { .. } => Volatility::Stable,
        _ => Volatility::Immutable,
    }
}

/// 式の揮発性（`m4/04` §3.2.1。部分木の最大）。
pub fn expr_volatility(e: &LExpr) -> Volatility {
    let mut worst = Volatility::Immutable;
    e.walk(&mut |n| {
        let mut v = node_volatility(n);
        if let ExprKind::SubLink { query, .. } = &n.kind {
            visit_plan_exprs(&query.plan, &mut |x| v = v.max(expr_volatility(x)));
        }
        worst = worst.max(v);
        true
    });
    worst
}

fn visit_plan_exprs(p: &LogicalPlan, f: &mut dyn FnMut(&LExpr)) {
    p.exprs().into_iter().for_each(&mut *f);
    for c in p.children() {
        visit_plan_exprs(c, f);
    }
}

pub fn contains_sublink(e: &LExpr) -> bool {
    e.contains_sublink()
}

/// 構造の等価（`span` は無視。`SubLink` を含むものは常に `false`）。
pub fn expr_eq(a: &LExpr, b: &LExpr) -> bool {
    !contains_sublink(a) && !contains_sublink(b) && a.same_as(b)
}

/// `Column(c)` を `map[c]` に置き換える（`SubLink` の中の自由な列も置き換える）。
#[allow(clippy::implicit_hasher)]
pub fn substitute(e: &LExpr, map: &HashMap<ColId, LExpr>) -> Result<LExpr> {
    e.substitute(map)
}

/// 関数・演算子が strict か（`PROCS` の `strict`）。表にないものは `false`（保守的）。
pub fn operator_is_strict(op: &BuiltinOperator) -> bool {
    builtin::operator_meta(op.oid)
        .and_then(|m| builtin::proc_by_oid(m.proc_oid))
        .is_some_and(|p| p.strict)
}

/// 演算子が Immutable か（実装関数の揮発性）。
pub fn operator_is_immutable(op: &BuiltinOperator) -> bool {
    builtin::operator_meta(op.oid)
        .is_some_and(|m| proc_volatility(m.proc_oid) == Volatility::Immutable)
}

/// 再帰が `MAX_PLAN_DEPTH` を超えたときのエラー（54001）。
pub fn depth_error() -> crate::error::Error {
    crate::error::Error::new(
        crate::error::sqlstate::STATEMENT_TOO_COMPLEX,
        "stack depth limit exceeded",
    )
}

/// `cols` がすべて NULL のとき `e` が NULL になる（NULL 伝播。`m4/04` §6.4.1）。
#[allow(clippy::only_used_in_recursion)]
pub fn nulls_out(e: &LExpr, cols: &ColSet, catalog: &dyn CatalogReader) -> bool {
    match &e.kind {
        ExprKind::Column(c) => cols.contains(c),
        ExprKind::Literal(d) => d.is_null(),
        ExprKind::Operator { op, args } => {
            operator_is_strict(op) && args.iter().any(|a| nulls_out(a, cols, catalog))
        }
        ExprKind::Function { func, args } => {
            func.strict && args.iter().any(|a| nulls_out(a, cols, catalog))
        }
        ExprKind::Cast { expr, .. } | ExprKind::CoerceTypmod { expr, .. } => {
            nulls_out(expr, cols, catalog)
        }
        ExprKind::Not(x) => nulls_out(x, cols, catalog),
        ExprKind::InList { expr, .. } => nulls_out(expr, cols, catalog),
        ExprKind::Like { expr, pattern, .. } => {
            nulls_out(expr, cols, catalog) || nulls_out(pattern, cols, catalog)
        }
        _ => false,
    }
}

/// `cols` がすべて NULL のとき `e` が確実に `true` になる（`NOT (x IS NULL)` の判定に使う）。
fn true_when_null(e: &LExpr, cols: &ColSet, catalog: &dyn CatalogReader) -> bool {
    match &e.kind {
        ExprKind::Literal(crate::types::Datum::Bool(b)) => *b,
        ExprKind::IsNull(x) => nulls_out(x, cols, catalog),
        ExprKind::Not(x) => match &x.kind {
            ExprKind::IsNotNull(y) => nulls_out(y, cols, catalog),
            _ => false,
        },
        ExprKind::And(args) => args.iter().all(|a| true_when_null(a, cols, catalog)),
        ExprKind::Or(args) => args.iter().any(|a| true_when_null(a, cols, catalog)),
        _ => false,
    }
}

/// `cols` がすべて NULL のとき `e` が NULL か `false` になる（その行が WHERE / ON で落ちる）。
pub fn rejects_null(e: &LExpr, cols: &ColSet, catalog: &dyn CatalogReader) -> bool {
    match &e.kind {
        ExprKind::Literal(crate::types::Datum::Bool(false)) => true,
        ExprKind::IsNotNull(x) => nulls_out(x, cols, catalog),
        ExprKind::Not(x) => nulls_out(x, cols, catalog) || true_when_null(x, cols, catalog),
        ExprKind::And(args) => args.iter().any(|a| rejects_null(a, cols, catalog)),
        ExprKind::Or(args) => args.iter().all(|a| rejects_null(a, cols, catalog)),
        _ => nulls_out(e, cols, catalog),
    }
}

/// `e` を型 `to` にそろえる。型 OID が同じならそのまま（typmod は見ない）、違えば暗黙キャスト
/// （`catalog.find_cast` の `Implicit`）を包む。暗黙キャストが無ければ `None`。
pub fn coerce_expr(e: LExpr, to: SqlType, catalog: &dyn CatalogReader) -> Option<LExpr> {
    if e.ty.oid == to.oid {
        return Some(e);
    }
    let cast = catalog.find_cast(e.ty.oid, to.oid)?;
    if cast.context != CastContext::Implicit {
        return None;
    }
    let span = e.span;
    Some(LExpr::new(
        ExprKind::Cast {
            expr: Box::new(e),
            method: cast.method,
            implicit: true,
        },
        to,
        span,
    ))
}

/// 式の木が `limit` 段より深いか（再帰は `limit + 1` 段で止まるのでスタックを溢れさせない）。
pub fn expr_depth_exceeds<C, Q>(e: &Expr<C, Q>, limit: usize) -> bool {
    fn go<C, Q>(e: &Expr<C, Q>, depth: usize, limit: usize) -> bool {
        depth > limit
            || crate::sql::check_stack_depth().is_err()
            || e.children().into_iter().any(|c| go(c, depth + 1, limit))
    }
    go(e, 1, limit)
}

// ---- プラン ----

/// ノードの出力列（順序つき）。
pub fn plan_output(p: &LogicalPlan) -> Vec<ColId> {
    p.output_cols()
}

/// `p` の中で参照され、`p` の中のどのノードにも定義されない `ColId`。
pub fn plan_free_cols(p: &LogicalPlan) -> ColSet {
    p.outer_refs()
}

pub fn children_mut(p: &mut LogicalPlan) -> Vec<&mut LogicalPlan> {
    p.children_mut()
}

pub fn exprs_mut(p: &mut LogicalPlan) -> Vec<&mut LExpr> {
    p.exprs_mut()
}

/// 式の中の `SubLink` の `LogicalSubquery`（直下だけ。入れ子は呼び出し側が再帰する）。
pub fn subqueries_mut(e: &mut LExpr) -> Vec<&mut LogicalSubquery> {
    fn go<'a>(e: &'a mut LExpr, out: &mut Vec<&'a mut LogicalSubquery>) {
        match &mut e.kind {
            ExprKind::SubLink { test, query, .. } => {
                if let Some(t) = test {
                    go(t, out);
                }
                out.push(&mut **query);
            }
            ExprKind::Operator { args, .. }
            | ExprKind::Function { args, .. }
            | ExprKind::And(args)
            | ExprKind::Or(args)
            | ExprKind::Coalesce(args)
            | ExprKind::MinMax { args, .. } => args.iter_mut().for_each(|a| go(a, out)),
            ExprKind::Cast { expr, .. }
            | ExprKind::CoerceTypmod { expr, .. }
            | ExprKind::Not(expr)
            | ExprKind::IsNull(expr)
            | ExprKind::IsNotNull(expr)
            | ExprKind::BoolTest { expr, .. } => go(expr, out),
            ExprKind::Case { arms, else_result } => {
                for (c, r) in arms {
                    go(c, out);
                    go(r, out);
                }
                if let Some(x) = else_result {
                    go(x, out);
                }
            }
            ExprKind::NullIf { left, right, .. } | ExprKind::DistinctFrom { left, right, .. } => {
                go(left, out);
                go(right, out);
            }
            ExprKind::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                go(expr, out);
                go(pattern, out);
                if let Some(x) = escape {
                    go(x, out);
                }
            }
            ExprKind::InList { expr, list, .. } => {
                go(expr, out);
                for a in &mut *list {
                    go(a, out);
                }
            }
            ExprKind::Aggregate(call) => {
                for a in &mut call.args {
                    go(a, out);
                }
                if let Some(f) = &mut call.filter {
                    go(f, out);
                }
                for k in &mut call.order_by {
                    go(&mut k.expr, out);
                }
            }
            ExprKind::Literal(_)
            | ExprKind::Column(_)
            | ExprKind::SessionValue(_)
            | ExprKind::SubLinkOutput(_) => {}
        }
    }
    let mut out = Vec::new();
    go(e, &mut out);
    out
}

/// ノードの式の中の `SubLink` の `LogicalSubquery`（直下だけ）に `f` を適用する。
pub fn for_each_subquery(
    p: &mut LogicalPlan,
    f: &mut dyn FnMut(&mut LogicalSubquery) -> Result<()>,
) -> Result<()> {
    for e in p.exprs_mut() {
        for q in subqueries_mut(e) {
            f(q)?;
        }
    }
    Ok(())
}

/// `p` を空のプランに置き換えて、元のプランを返す（`&mut` から所有権を取り出す）。
pub fn take_plan(p: &mut LogicalPlan) -> LogicalPlan {
    std::mem::replace(p, LogicalPlan::Empty { cols: Vec::new() })
}

/// 子から先（post-order）に変換する。ノードの式の中の `SubLink` の plan にも降りる。
pub fn transform_up(
    mut p: LogicalPlan,
    f: &mut dyn FnMut(LogicalPlan) -> Result<LogicalPlan>,
) -> Result<LogicalPlan> {
    for c in p.children_mut() {
        let child = take_plan(c);
        *c = transform_up(child, f)?;
    }
    for_each_subquery(&mut p, &mut |q| {
        let sub = take_plan(&mut q.plan);
        q.plan = transform_up(sub, f)?;
        Ok(())
    })?;
    f(p)
}

/// 木の中の `CteScan` を数える（式の中の副問い合わせを含む）。
pub fn count_cte_scans(p: &LogicalPlan, counts: &mut [u32]) {
    if let LogicalPlan::CteScan { cte, .. } = p
        && let Some(c) = counts.get_mut(usize::from(cte.0))
    {
        *c += 1;
    }
    for e in p.exprs() {
        e.walk(&mut |n| {
            if let ExprKind::SubLink { query, .. } = &n.kind {
                count_cte_scans(&query.plan, counts);
            }
            true
        });
    }
    for c in p.children() {
        count_cte_scans(c, counts);
    }
}

/// 共有 CTE の `refs` を、木に残っている `CteScan` の数に合わせる（`Empty` への置き換えなどのあと）。
pub fn recount_cte_refs(q: &mut LogicalQuery) {
    let mut counts = vec![0u32; q.ctes.len()];
    count_cte_scans(&q.plan, &mut counts);
    for c in &q.ctes {
        count_cte_scans(&c.plan, &mut counts);
    }
    for (c, n) in q.ctes.iter_mut().zip(counts) {
        if !c.inline {
            c.refs = n;
        }
    }
}

// ---- 結合条件 ----

#[derive(Debug)]
pub struct EquiKey {
    pub left: LExpr,
    pub right: LExpr,
    pub op: &'static BuiltinOperator,
    pub key_type: SqlType,
}

#[derive(Debug)]
pub struct SplitOn {
    pub keys: Vec<EquiKey>,
    pub residual: Vec<LExpr>,
}

/// `on` の conjunct を等値キーと残りに分ける（`m4/04` §6.6）。`left` / `right` はそれぞれの子の出力列。
pub fn split_on(
    on: Option<&LExpr>,
    left: &ColSet,
    right: &ColSet,
    catalog: &dyn CatalogReader,
) -> SplitOn {
    let mut out = SplitOn {
        keys: Vec::new(),
        residual: Vec::new(),
    };
    let Some(on) = on else { return out };
    for c in conjuncts(on.clone()) {
        match equi_key(&c, left, right, catalog) {
            Some(k) => out.keys.push(k),
            None => out.residual.push(c),
        }
    }
    out
}

fn implicit_cast_exists(catalog: &dyn CatalogReader, from: Oid, to: Oid) -> bool {
    catalog
        .find_cast(from, to)
        .is_some_and(|c| c.context == CastContext::Implicit)
}

fn equi_key(
    c: &LExpr,
    left: &ColSet,
    right: &ColSet,
    catalog: &dyn CatalogReader,
) -> Option<EquiKey> {
    let ExprKind::Operator { op, args } = &c.kind else {
        return None;
    };
    if op.name != "=" || args.len() != 2 || !builtin::operator_merge_hash(op.oid).1 {
        return None;
    }
    let (a, b) = (&args[0], &args[1]);
    if expr_volatility(a) == Volatility::Volatile
        || expr_volatility(b) == Volatility::Volatile
        || contains_sublink(a)
        || contains_sublink(b)
    {
        return None;
    }
    // 式の参照列（結合の両側の出力に限る）が空でなく、`set` に収まるか。
    let within = |e: &LExpr, set: &ColSet| -> Option<bool> {
        let refs: ColSet = expr_refs(e)
            .into_iter()
            .filter(|c| left.contains(c) || right.contains(c))
            .collect();
        (!refs.is_empty()).then(|| refs.is_subset(set))
    };
    let (a_left, b_right) = (within(a, left)?, within(b, right)?);
    let (la, lb, key_op) = if a_left && b_right {
        (a, b, *op)
    } else if within(a, right)? && within(b, left)? {
        let com = builtin::operator_meta(op.oid)?.com;
        if com == 0 {
            return None;
        }
        (b, a, builtin::operator_by_oid(com)?)
    } else {
        return None;
    };
    let (ta, tb) = (la.ty.oid, lb.ty.oid);
    let key_type = if ta == tb {
        SqlType::of(ta)
    } else if implicit_cast_exists(catalog, ta, tb) {
        SqlType::of(tb)
    } else if implicit_cast_exists(catalog, tb, ta) {
        SqlType::of(ta)
    } else {
        return None;
    };
    Some(EquiKey {
        left: la.clone(),
        right: lb.clone(),
        op: key_op,
        key_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::ColId;

    fn col(i: u32) -> LExpr {
        LExpr::column(ColId(i), SqlType::BOOL)
    }

    #[test]
    fn conjuncts_flatten_and_drop_true() {
        let e = LExpr::and_all(vec![col(0), LExpr::bool_lit(true), col(1)]);
        let parts = conjuncts(e);
        assert_eq!(parts.len(), 2);
        assert!(and_all(Vec::new()).is_none());
        assert!(and_all(parts).is_some());
    }

    #[test]
    fn refs_and_equality() {
        let e = LExpr::and_all(vec![col(3), col(1)]);
        assert_eq!(
            expr_refs(&e).into_iter().map(|c| c.0).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(expr_eq(&col(1), &col(1)));
        assert!(!expr_eq(&col(1), &col(2)));
        assert_eq!(expr_volatility(&e), Volatility::Immutable);
    }

    #[test]
    fn transform_up_visits_children_first() {
        let leaf = LogicalPlan::Result {
            one_time_filter: None,
            cols: Vec::new(),
        };
        let p = LogicalPlan::Filter {
            input: Box::new(leaf),
            predicate: LExpr::bool_lit(true),
        };
        let mut seen = Vec::new();
        transform_up(p, &mut |n| {
            seen.push(matches!(n, LogicalPlan::Result { .. }));
            Ok(n)
        })
        .unwrap();
        assert_eq!(seen, vec![true, false]);
    }
}
