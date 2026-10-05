//! Expression simplification that PostgreSQL's planner does
//! (`eval_const_expressions`) and that is observable through errors:
//!
//! - a strict operator / function with a NULL constant argument becomes NULL
//!   without evaluating its other arguments;
//! - `x OR true` becomes `true`, `x AND false` becomes `false`; the neutral
//!   constants (`false` in OR, `true` in AND) are dropped.

use crate::analyzer::{BoundExpr, BoundExprKind};
use crate::types::Datum;

fn is_null(e: &BoundExpr) -> bool {
    matches!(e.kind, BoundExprKind::Literal(Datum::Null))
}

fn bool_const(e: &BoundExpr) -> Option<bool> {
    match e.kind {
        BoundExprKind::Literal(Datum::Bool(b)) => Some(b),
        _ => None,
    }
}

pub(super) fn simplify(e: &mut BoundExpr) {
    let is_and = matches!(e.kind, BoundExprKind::And(_));
    match &mut e.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef { .. }
        | BoundExprKind::SessionValue(_) => {}
        BoundExprKind::Operator { args, .. } => {
            args.iter_mut().for_each(simplify);
            if args.iter().any(is_null) {
                e.kind = BoundExprKind::Literal(Datum::Null);
            }
        }
        BoundExprKind::Function { func, args } => {
            args.iter_mut().for_each(simplify);
            if func.strict && args.iter().any(is_null) {
                e.kind = BoundExprKind::Literal(Datum::Null);
            }
        }
        BoundExprKind::And(args) | BoundExprKind::Or(args) => {
            args.iter_mut().for_each(simplify);
            // The value that decides the whole expression (false for AND).
            let decisive = !is_and;
            if args.iter().any(|a| bool_const(a) == Some(decisive)) {
                e.kind = BoundExprKind::Literal(Datum::Bool(decisive));
                return;
            }
            args.retain(|a| bool_const(a) != Some(!decisive));
            match args.len() {
                0 => e.kind = BoundExprKind::Literal(Datum::Bool(!decisive)),
                1 => {
                    let only = args.pop().expect("one operand");
                    *e = only;
                }
                _ => {}
            }
        }
        BoundExprKind::Cast { expr, .. }
        | BoundExprKind::CoerceTypmod { expr, .. }
        | BoundExprKind::Not(expr)
        | BoundExprKind::IsNull(expr)
        | BoundExprKind::IsNotNull(expr)
        | BoundExprKind::BoolTest { expr, .. } => simplify(expr),
        BoundExprKind::Case { arms, else_result } => {
            for (c, r) in arms {
                simplify(c);
                simplify(r);
            }
            if let Some(r) = else_result {
                simplify(r);
            }
        }
        BoundExprKind::Coalesce(args) | BoundExprKind::MinMax { args, .. } => {
            args.iter_mut().for_each(simplify);
        }
        BoundExprKind::NullIf { left, right, .. }
        | BoundExprKind::DistinctFrom { left, right, .. } => {
            simplify(left);
            simplify(right);
        }
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            simplify(expr);
            simplify(pattern);
            if let Some(x) = escape {
                simplify(x);
            }
        }
        BoundExprKind::InList { expr, list, .. } => {
            simplify(expr);
            list.iter_mut().for_each(simplify);
        }
    }
}

/// Simplifies a WHERE filter and, like PostgreSQL's `order_qual_clauses`,
/// flattens its top-level AND and evaluates the cheaper conjuncts first
/// (stable), which decides whether an erroring operand is ever evaluated.
pub(super) fn simplify_opt(e: Option<&BoundExpr>) -> Option<BoundExpr> {
    e.map(|e| {
        let mut e = e.clone();
        simplify(&mut e);
        order_quals(&mut e);
        fold_dead_quals(&mut e);
        e
    })
}

/// `NOT e` is a constant NULL once PostgreSQL pushes the NOT down: through OR
/// (De Morgan) and into an IN list, which becomes `<> NULL` for a NULL element.
fn negation_is_null(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Or(args) => args.iter().any(negation_is_null),
        BoundExprKind::InList {
            list,
            negated: false,
            ..
        } => list.iter().any(is_null),
        _ => is_null(e),
    }
}

/// In a WHERE clause NULL is as good as false, so PostgreSQL folds the whole
/// filter to false when a top-level conjunct is a constant NULL / false (a
/// negated IN list with a NULL element expands to `<> NULL` conjuncts), and
/// never evaluates the other conjuncts.
fn fold_dead_quals(e: &mut BoundExpr) {
    fn has_null(list: &[BoundExpr]) -> bool {
        list.iter().any(is_null)
    }
    fn dead(q: &BoundExpr) -> bool {
        is_null(q)
            || bool_const(q) == Some(false)
            || match &q.kind {
                BoundExprKind::InList {
                    list,
                    negated: true,
                    ..
                } => has_null(list),
                BoundExprKind::Not(inner) => negation_is_null(inner),
                _ => false,
            }
    }
    let is_dead = match &e.kind {
        BoundExprKind::And(args) => args.iter().any(dead),
        _ => dead(e),
    };
    if is_dead {
        e.kind = BoundExprKind::Literal(Datum::Bool(false));
    }
}

fn flatten_and(e: BoundExpr, out: &mut Vec<BoundExpr>) {
    match e.kind {
        BoundExprKind::And(args) => args.into_iter().for_each(|a| flatten_and(a, out)),
        _ => out.push(e),
    }
}

fn order_quals(e: &mut BoundExpr) {
    if !matches!(e.kind, BoundExprKind::And(_)) {
        return;
    }
    let (ty, span) = (e.ty, e.span);
    let taken = std::mem::replace(
        e,
        BoundExpr::new(BoundExprKind::Literal(Datum::Null), ty, span),
    );
    let mut quals = Vec::new();
    flatten_and(taken, &mut quals);
    quals.sort_by_cached_key(cost);
    *e = BoundExpr::new(BoundExprKind::And(quals), ty, span);
}

/// Approximate planner cost: the number of operator / function calls.
fn cost(e: &BoundExpr) -> usize {
    if foldable(e) {
        return 0;
    }
    match &e.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef { .. }
        | BoundExprKind::SessionValue(_) => 0,
        BoundExprKind::Operator { args, .. } | BoundExprKind::Function { args, .. } => {
            1 + args.iter().map(cost).sum::<usize>()
        }
        BoundExprKind::And(args)
        | BoundExprKind::Or(args)
        | BoundExprKind::Coalesce(args)
        | BoundExprKind::MinMax { args, .. } => args.iter().map(cost).sum(),
        BoundExprKind::Cast { expr, .. }
        | BoundExprKind::CoerceTypmod { expr, .. }
        | BoundExprKind::Not(expr)
        | BoundExprKind::IsNull(expr)
        | BoundExprKind::IsNotNull(expr)
        | BoundExprKind::BoolTest { expr, .. } => cost(expr),
        BoundExprKind::Case { arms, else_result } => {
            arms.iter().map(|(c, r)| cost(c) + cost(r)).sum::<usize>()
                + else_result.as_deref().map_or(0, cost)
        }
        BoundExprKind::NullIf { left, right, .. }
        | BoundExprKind::DistinctFrom { left, right, .. } => 1 + cost(left) + cost(right),
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => 1 + cost(expr) + cost(pattern) + escape.as_deref().map_or(0, cost),
        BoundExprKind::InList { expr, list, .. } => {
            cost(expr) + list.iter().map(|x| 1 + cost(x)).sum::<usize>()
        }
    }
}

/// Constant subtree PostgreSQL would fold at plan time (so it costs nothing
/// at run time). Operators are assumed immutable; functions must be.
fn foldable(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Literal(_) => true,
        BoundExprKind::Operator { args, .. } => args.iter().all(foldable),
        BoundExprKind::Function { func, args } => {
            !args.is_empty()
                && !func.name.starts_with("random")
                && !matches!(
                    func.name,
                    "nextval"
                        | "setval"
                        | "pg_sleep"
                        | "setseed"
                        | "set_config"
                        | "current_setting"
                )
                && args.iter().all(foldable)
        }
        BoundExprKind::Cast { expr, .. } | BoundExprKind::CoerceTypmod { expr, .. } => {
            foldable(expr)
        }
        _ => false,
    }
}
