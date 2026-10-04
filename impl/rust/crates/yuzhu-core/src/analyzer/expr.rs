//! Expression analysis (PostgreSQL's `transformExpr`) and output column
//! names (`FigureColname`).

use super::Analyzer;
use super::bound::{BoolTestKind, BoundExpr, BoundExprKind};
use super::coerce::{CoercionContext, resolve_unknown, tname};
use super::scope::{ExprKind, Scope};
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{BoolTestValue, Expr, Literal, SessionValueKind, WhenClause};
use crate::types::{Datum, SqlType, oid};

/// Where an expression is analyzed.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExprCtx<'s> {
    pub(super) scope: &'s Scope,
    pub(super) kind: ExprKind,
}

impl<'s> ExprCtx<'s> {
    pub(super) fn new(scope: &'s Scope, kind: ExprKind) -> Self {
        ExprCtx { scope, kind }
    }
}

/// Aggregate function names (M4). Calls to them are rejected with 0A000
/// rather than "function does not exist".
const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "bool_and",
    "bool_or",
    "every",
    "string_agg",
    "array_agg",
    "stddev",
    "variance",
];

/// Parses an integer literal (decimal, PG16 `0x`/`0o`/`0b` prefixes and
/// `_` separators, optional sign). `None` if it does not fit in int8.
pub(super) fn parse_int_literal(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let body: String = body.chars().filter(|c| *c != '_').collect();
    let lower = body.to_ascii_lowercase();
    let (radix, digits) = if let Some(d) = lower.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (2, d)
    } else {
        (10, lower.as_str())
    };
    if digits.is_empty() {
        return None;
    }
    let mag = u64::from_str_radix(digits, radix).ok()?;
    if neg {
        if mag == 1u64 << 63 {
            Some(i64::MIN)
        } else {
            i64::try_from(mag).ok().map(|v| -v)
        }
    } else {
        i64::try_from(mag).ok()
    }
}

fn bool_expr(kind: BoundExprKind, span: Span) -> BoundExpr {
    BoundExpr::new(kind, SqlType::BOOL, span)
}

/// Does the expression reference any input column?
pub(super) fn contains_column_ref(e: &BoundExpr) -> bool {
    let mut found = false;
    visit(e, &mut |x| {
        if matches!(x.kind, BoundExprKind::ColumnRef { .. }) {
            found = true;
        }
    });
    found
}

/// The column referenced when the expression references exactly one distinct
/// column (PostgreSQL names a CHECK `<table>_<col>_check` only then;
/// otherwise `<table>_check`).
pub(super) fn sole_column_ref(e: &BoundExpr) -> Option<usize> {
    let mut cols: Vec<usize> = Vec::new();
    visit(e, &mut |x| {
        if let BoundExprKind::ColumnRef { index } = x.kind
            && !cols.contains(&index)
        {
            cols.push(index);
        }
    });
    if let [only] = cols[..] {
        Some(only)
    } else {
        None
    }
}

/// Pre-order traversal.
pub(super) fn visit(e: &BoundExpr, f: &mut dyn FnMut(&BoundExpr)) {
    f(e);
    match &e.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::ColumnRef { .. }
        | BoundExprKind::SessionValue(_) => {}
        BoundExprKind::Operator { args, .. }
        | BoundExprKind::Function { args, .. }
        | BoundExprKind::And(args)
        | BoundExprKind::Or(args)
        | BoundExprKind::Coalesce(args) => {
            for a in args {
                visit(a, f);
            }
        }
        BoundExprKind::Cast { expr, .. }
        | BoundExprKind::CoerceTypmod { expr, .. }
        | BoundExprKind::Not(expr)
        | BoundExprKind::IsNull(expr)
        | BoundExprKind::IsNotNull(expr)
        | BoundExprKind::BoolTest { expr, .. } => visit(expr, f),
        BoundExprKind::Case { arms, else_result } => {
            for (c, r) in arms {
                visit(c, f);
                visit(r, f);
            }
            if let Some(e) = else_result {
                visit(e, f);
            }
        }
        BoundExprKind::NullIf { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        BoundExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            visit(expr, f);
            visit(pattern, f);
            if let Some(e) = escape {
                visit(e, f);
            }
        }
        BoundExprKind::InList { expr, list, .. } => {
            visit(expr, f);
            for a in list {
                visit(a, f);
            }
        }
    }
}

/// Structural equality ignoring source spans (PostgreSQL's `equal()` on
/// analyzed expressions), used to match ORDER BY expressions to targets.
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
pub(super) fn same_expr(a: &BoundExpr, b: &BoundExpr) -> bool {
    use BoundExprKind as K;
    if a.ty != b.ty {
        return false;
    }
    let all = |x: &[BoundExpr], y: &[BoundExpr]| {
        x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_expr(p, q))
    };
    let opt = |x: &Option<Box<BoundExpr>>, y: &Option<Box<BoundExpr>>| match (x, y) {
        (None, None) => true,
        (Some(p), Some(q)) => same_expr(p, q),
        _ => false,
    };
    match (&a.kind, &b.kind) {
        (K::Literal(x), K::Literal(y)) => match (x, y) {
            (Datum::Float4(p), Datum::Float4(q)) => p.to_bits() == q.to_bits(),
            (Datum::Float8(p), Datum::Float8(q)) => p.to_bits() == q.to_bits(),
            _ => x == y,
        },
        (K::ColumnRef { index: x }, K::ColumnRef { index: y }) => x == y,
        (K::Operator { op: o1, args: a1 }, K::Operator { op: o2, args: a2 }) => {
            o1.oid == o2.oid && all(a1, a2)
        }
        (K::Function { func: f1, args: a1 }, K::Function { func: f2, args: a2 }) => {
            f1.oid == f2.oid && all(a1, a2)
        }
        (
            K::Cast {
                expr: e1,
                method: m1,
            },
            K::Cast {
                expr: e2,
                method: m2,
            },
        ) => std::mem::discriminant(m1) == std::mem::discriminant(m2) && same_expr(e1, e2),
        (
            K::CoerceTypmod {
                expr: e1,
                explicit: x1,
            },
            K::CoerceTypmod {
                expr: e2,
                explicit: x2,
            },
        ) => x1 == x2 && same_expr(e1, e2),
        (K::And(x), K::And(y)) | (K::Or(x), K::Or(y)) | (K::Coalesce(x), K::Coalesce(y)) => {
            all(x, y)
        }
        (K::Not(x), K::Not(y))
        | (K::IsNull(x), K::IsNull(y))
        | (K::IsNotNull(x), K::IsNotNull(y)) => same_expr(x, y),
        (K::BoolTest { expr: e1, test: t1 }, K::BoolTest { expr: e2, test: t2 }) => {
            t1 == t2 && same_expr(e1, e2)
        }
        (
            K::Case {
                arms: r1,
                else_result: e1,
            },
            K::Case {
                arms: r2,
                else_result: e2,
            },
        ) => {
            r1.len() == r2.len()
                && r1
                    .iter()
                    .zip(r2)
                    .all(|((c1, v1), (c2, v2))| same_expr(c1, c2) && same_expr(v1, v2))
                && opt(e1, e2)
        }
        (
            K::NullIf {
                left: l1,
                right: r1,
                ..
            },
            K::NullIf {
                left: l2,
                right: r2,
                ..
            },
        ) => same_expr(l1, l2) && same_expr(r1, r2),
        (
            K::Like {
                expr: e1,
                pattern: p1,
                escape: s1,
                negated: n1,
                case_insensitive: c1,
            },
            K::Like {
                expr: e2,
                pattern: p2,
                escape: s2,
                negated: n2,
                case_insensitive: c2,
            },
        ) => n1 == n2 && c1 == c2 && same_expr(e1, e2) && same_expr(p1, p2) && opt(s1, s2),
        (
            K::InList {
                expr: e1,
                list: l1,
                negated: n1,
                ..
            },
            K::InList {
                expr: e2,
                list: l2,
                negated: n2,
                ..
            },
        ) => n1 == n2 && same_expr(e1, e2) && all(l1, l2),
        (K::SessionValue(x), K::SessionValue(y)) => x == y,
        _ => false,
    }
}

/// `FigureColname`: the output column name of a target expression without
/// an alias.
pub(super) fn figure_colname(e: &Expr) -> String {
    figure_colname_internal(e)
        .0
        .unwrap_or_else(|| "?column?".to_owned())
}

fn figure_colname_internal(e: &Expr) -> (Option<String>, u8) {
    match e {
        Expr::Column { parts, .. } => (parts.last().map(|p| p.value.clone()), 2),
        Expr::Function { name, .. } => (Some(name.name().value.clone()), 2),
        Expr::Cast {
            expr, type_name, ..
        } => {
            let (n, strength) = figure_colname_internal(expr);
            if strength <= 1
                && let Some(t) = type_name.names.last()
            {
                return (Some(t.value.clone()), 1);
            }
            (n, strength)
        }
        // TRUE / FALSE are `'t'::bool` casts in PostgreSQL's grammar.
        Expr::Literal {
            value: Literal::Bool(_),
            ..
        } => (Some("bool".to_owned()), 1),
        Expr::Case { .. } => (Some("case".to_owned()), 1),
        Expr::Coalesce { .. } => (Some("coalesce".to_owned()), 2),
        Expr::NullIf { .. } => (Some("nullif".to_owned()), 2),
        Expr::Exists { .. } => (Some("exists".to_owned()), 2),
        Expr::SessionValue { kind, .. } => {
            let n = match kind {
                SessionValueKind::User => "user",
                k => k.column_name(),
            };
            (Some(n.to_owned()), 2)
        }
        _ => (None, 0),
    }
}

impl Analyzer<'_> {
    #[allow(clippy::too_many_lines)]
    pub(super) fn transform_expr(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr> {
        match e {
            Expr::Literal { value, span } => transform_literal(value, *span),
            Expr::Column { parts, span } => {
                if cx.kind == ExprKind::ColumnDefault {
                    return Err(Error::not_supported(
                        "cannot use column reference in DEFAULT expression",
                    )
                    .with_span(*span));
                }
                let (index, col) = cx.scope.resolve_column(parts, *span)?;
                Ok(BoundExpr::new(
                    BoundExprKind::ColumnRef { index },
                    col.ty,
                    *span,
                ))
            }
            Expr::Parameter { index, span } => Err(Error::new(
                sqlstate::UNDEFINED_PARAMETER,
                format!("there is no parameter ${index}"),
            )
            .with_span(*span)),
            Expr::BinaryOp {
                op,
                left,
                right,
                span,
            } => {
                let l = self.transform_expr(left, cx)?;
                let r = self.transform_expr(right, cx)?;
                self.make_op(op, Some(l), r, *span)
            }
            Expr::UnaryOp { op, expr, span } => {
                let a = self.transform_expr(expr, cx)?;
                self.make_op(op, None, a, *span)
            }
            Expr::And { span, .. } | Expr::Or { span, .. } => {
                let is_and = matches!(e, Expr::And { .. });
                let mut operands = Vec::new();
                collect_bool_operands(e, is_and, &mut operands);
                let construct = if is_and { "AND" } else { "OR" };
                let args = operands
                    .into_iter()
                    .map(|o| {
                        let b = self.transform_expr(o, cx)?;
                        self.coerce_to_boolean(b, construct)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(bool_expr(
                    if is_and {
                        BoundExprKind::And(args)
                    } else {
                        BoundExprKind::Or(args)
                    },
                    *span,
                ))
            }
            Expr::Not { expr, span } => {
                let a = self.transform_expr(expr, cx)?;
                let a = self.coerce_to_boolean(a, "NOT")?;
                Ok(bool_expr(BoundExprKind::Not(Box::new(a)), *span))
            }
            Expr::IsNull {
                expr,
                negated,
                span,
            } => {
                let a = Box::new(resolve_unknown(self.transform_expr(expr, cx)?));
                Ok(bool_expr(
                    if *negated {
                        BoundExprKind::IsNotNull(a)
                    } else {
                        BoundExprKind::IsNull(a)
                    },
                    *span,
                ))
            }
            Expr::IsBool {
                expr,
                value,
                negated,
                span,
            } => {
                let (test, construct) = match (value, negated) {
                    (BoolTestValue::True, false) => (BoolTestKind::IsTrue, "IS TRUE"),
                    (BoolTestValue::True, true) => (BoolTestKind::IsNotTrue, "IS NOT TRUE"),
                    (BoolTestValue::False, false) => (BoolTestKind::IsFalse, "IS FALSE"),
                    (BoolTestValue::False, true) => (BoolTestKind::IsNotFalse, "IS NOT FALSE"),
                    (BoolTestValue::Unknown, false) => (BoolTestKind::IsUnknown, "IS UNKNOWN"),
                    (BoolTestValue::Unknown, true) => {
                        (BoolTestKind::IsNotUnknown, "IS NOT UNKNOWN")
                    }
                };
                let a = self.transform_expr(expr, cx)?;
                let a = self.coerce_to_boolean(a, construct)?;
                Ok(bool_expr(
                    BoundExprKind::BoolTest {
                        expr: Box::new(a),
                        test,
                    },
                    *span,
                ))
            }
            Expr::IsDistinctFrom { span, .. } => {
                Err(Error::not_supported("IS DISTINCT FROM is not supported yet").with_span(*span))
            }
            Expr::Cast {
                expr,
                type_name,
                span,
                ..
            } => {
                let target = self.resolve_type_name(type_name)?;
                let a = self.transform_expr(expr, cx)?;
                self.coerce_explicit(a, target, *span)
            }
            Expr::Function {
                name,
                args,
                distinct,
                star,
                span,
            } => {
                let fname = &name.name().value;
                if *star || *distinct || AGGREGATES.contains(&fname.as_str()) {
                    return Err(
                        Error::not_supported("aggregate functions are not supported yet")
                            .with_span(*span),
                    );
                }
                if let Some(schema) = name.schema()
                    && schema.value != "pg_catalog"
                {
                    if schema.value != "public" {
                        return Err(Error::new(
                            sqlstate::INVALID_SCHEMA_NAME,
                            format!("schema \"{}\" does not exist", schema.value),
                        )
                        .with_span(schema.span));
                    }
                    let shown: Vec<&str> = name.parts.iter().map(|p| p.value.as_str()).collect();
                    let args = args
                        .iter()
                        .map(|a| self.transform_expr(a, cx).map(|b| tname(b.ty.oid)))
                        .collect::<Result<Vec<_>>>()?;
                    return Err(Error::new(
                        sqlstate::UNDEFINED_FUNCTION,
                        format!(
                            "function {}({}) does not exist",
                            shown.join("."),
                            args.join(", ")
                        ),
                    )
                    .with_span(*span));
                }
                let args = args
                    .iter()
                    .map(|a| self.transform_expr(a, cx))
                    .collect::<Result<Vec<_>>>()?;
                self.make_func_call(fname, args, *span)
            }
            Expr::Case {
                operand,
                whens,
                else_result,
                span,
            } => self.transform_case(operand.as_deref(), whens, else_result.as_deref(), *span, cx),
            Expr::Between {
                expr,
                low,
                high,
                negated,
                symmetric,
                span,
            } => {
                let x = self.transform_expr(expr, cx)?;
                let lo = self.transform_expr(low, cx)?;
                let hi = self.transform_expr(high, cx)?;
                let range = |lo: BoundExpr, hi: BoundExpr| -> Result<BoundExpr> {
                    if *negated {
                        let a = self.make_op("<", Some(x.clone()), lo, *span)?;
                        let b = self.make_op(">", Some(x.clone()), hi, *span)?;
                        self.bool_combine(false, vec![a, b], *span)
                    } else {
                        let a = self.make_op(">=", Some(x.clone()), lo, *span)?;
                        let b = self.make_op("<=", Some(x.clone()), hi, *span)?;
                        self.bool_combine(true, vec![a, b], *span)
                    }
                };
                if *symmetric {
                    let first = range(lo.clone(), hi.clone())?;
                    let second = range(hi, lo)?;
                    self.bool_combine(*negated, vec![first, second], *span)
                } else {
                    range(lo, hi)
                }
            }
            Expr::InList {
                expr,
                list,
                negated,
                span,
            } => self.transform_in(expr, list, *negated, *span, cx),
            Expr::InSubquery { span, .. }
            | Expr::Exists { span, .. }
            | Expr::Subquery { span, .. } => {
                Err(Error::not_supported("subqueries are not supported yet").with_span(*span))
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                negated,
                case_insensitive,
                span,
            } => {
                let opname = match (*negated, *case_insensitive) {
                    (false, false) => "~~",
                    (true, false) => "!~~",
                    (false, true) => "~~*",
                    (true, true) => "!~~*",
                };
                let l = self.transform_expr(expr, cx)?;
                let p = self.transform_expr(pattern, cx)?;
                let escape = match escape {
                    Some(esc) => {
                        let b = self.transform_expr(esc, cx)?;
                        let (src, sp) = (b.ty.oid, b.span);
                        if src != oid::UNKNOWN && self.category(src) != 'S' {
                            return Err(Error::new(
                                sqlstate::UNDEFINED_FUNCTION,
                                format!(
                                    "function like_escape(text, {}) does not exist",
                                    tname(src)
                                ),
                            )
                            .with_span(sp));
                        }
                        let b = self
                            .coerce_type(b, oid::TEXT, CoercionContext::Implicit)?
                            .ok_or_else(|| Error::internal("ESCAPE coercion failed"))?;
                        Some(Box::new(b))
                    }
                    None => None,
                };
                let resolved = self.make_op(opname, Some(l), p, *span)?;
                let ty = resolved.ty;
                let BoundExprKind::Operator { args, .. } = resolved.kind else {
                    return Err(Error::internal("LIKE did not resolve to an operator"));
                };
                let mut it = args.into_iter();
                let (Some(l), Some(p)) = (it.next(), it.next()) else {
                    return Err(Error::internal("LIKE operator without two arguments"));
                };
                Ok(BoundExpr::new(
                    BoundExprKind::Like {
                        expr: Box::new(l),
                        pattern: Box::new(p),
                        escape,
                        negated: *negated,
                        case_insensitive: *case_insensitive,
                    },
                    ty,
                    *span,
                ))
            }
            Expr::Coalesce { args, span } => {
                let args = args
                    .iter()
                    .map(|a| self.transform_expr(a, cx))
                    .collect::<Result<Vec<_>>>()?;
                let (args, ty) = self.coerce_all_to_common(args, "COALESCE")?;
                Ok(BoundExpr::new(BoundExprKind::Coalesce(args), ty, *span))
            }
            Expr::NullIf { left, right, span } => {
                let l = self.transform_expr(left, cx)?;
                let r = self.transform_expr(right, cx)?;
                let resolved = self.make_op("=", Some(l), r, *span)?;
                let BoundExprKind::Operator { op, args } = resolved.kind else {
                    return Err(Error::internal("NULLIF did not resolve to an operator"));
                };
                if op.result != oid::BOOL {
                    return Err(Error::new(
                        sqlstate::DATATYPE_MISMATCH,
                        "NULLIF requires = operator to yield boolean",
                    )
                    .with_span(*span));
                }
                let mut it = args.into_iter();
                let (Some(l), Some(r)) = (it.next(), it.next()) else {
                    return Err(Error::internal("NULLIF operator without two arguments"));
                };
                let ty = l.ty;
                Ok(BoundExpr::new(
                    BoundExprKind::NullIf {
                        left: Box::new(l),
                        right: Box::new(r),
                        eq_op: op,
                    },
                    ty,
                    *span,
                ))
            }
            Expr::SessionValue { kind, span } => Ok(BoundExpr::new(
                BoundExprKind::SessionValue(*kind),
                SqlType::NAME,
                *span,
            )),
            Expr::Default { span } => Err(Error::syntax_at(
                *span,
                "DEFAULT is not allowed in this context",
            )),
        }
    }

    /// AND / OR of already-typed operands (BETWEEN lowering).
    fn bool_combine(&self, is_and: bool, args: Vec<BoundExpr>, span: Span) -> Result<BoundExpr> {
        let construct = if is_and { "AND" } else { "OR" };
        let args = args
            .into_iter()
            .map(|a| self.coerce_to_boolean(a, construct))
            .collect::<Result<Vec<_>>>()?;
        Ok(bool_expr(
            if is_and {
                BoundExprKind::And(args)
            } else {
                BoundExprKind::Or(args)
            },
            span,
        ))
    }

    fn transform_case(
        &self,
        operand: Option<&Expr>,
        whens: &[WhenClause],
        else_result: Option<&Expr>,
        span: Span,
        cx: &ExprCtx<'_>,
    ) -> Result<BoundExpr> {
        let operand = match operand {
            Some(o) => {
                let b = self.transform_expr(o, cx)?;
                Some(if b.ty.oid == oid::UNKNOWN {
                    self.coerce_to_common_type(b, oid::TEXT, "CASE")?
                } else {
                    b
                })
            }
            None => None,
        };
        let mut conds = Vec::with_capacity(whens.len());
        let mut results = Vec::with_capacity(whens.len() + 1);
        for w in whens {
            let c = match &operand {
                Some(op) => {
                    let v = self.transform_expr(&w.condition, cx)?;
                    self.make_op("=", Some(op.clone()), v, w.condition.span())?
                }
                None => self.transform_expr(&w.condition, cx)?,
            };
            conds.push(self.coerce_to_boolean(c, "CASE/WHEN")?);
            results.push(self.transform_expr(&w.result, cx)?);
        }
        // PostgreSQL puts the ELSE result first when choosing the type.
        let else_expr = match else_result {
            Some(e) => self.transform_expr(e, cx)?,
            None => BoundExpr::new(BoundExprKind::Literal(Datum::Null), SqlType::UNKNOWN, span),
        };
        let mut all = Vec::with_capacity(results.len() + 1);
        all.push(else_expr);
        all.extend(results);
        let (mut coerced, ty) = self.coerce_all_to_common(all, "CASE")?;
        let else_c = coerced.remove(0);
        let arms = conds.into_iter().zip(coerced).collect();
        Ok(BoundExpr::new(
            BoundExprKind::Case {
                arms,
                else_result: else_result.map(|_| Box::new(else_c)),
            },
            ty,
            span,
        ))
    }

    /// `x [NOT] IN (list)` (PostgreSQL's `transformAExprIn`): constant-ish
    /// elements with a common type become one `InList`; the rest (or all,
    /// if there is no common type) become `x = e` comparisons joined by
    /// OR (`x <> e` joined by AND for NOT IN).
    fn transform_in(
        &self,
        expr: &Expr,
        list: &[Expr],
        negated: bool,
        span: Span,
        cx: &ExprCtx<'_>,
    ) -> Result<BoundExpr> {
        let x = self.transform_expr(expr, cx)?;
        let elems = list
            .iter()
            .map(|e| self.transform_expr(e, cx))
            .collect::<Result<Vec<_>>>()?;
        let (nonvars, vars): (Vec<BoundExpr>, Vec<BoundExpr>) =
            elems.into_iter().partition(|e| !contains_column_ref(e));
        let mut parts = Vec::new();
        let mut rest = vars;
        if nonvars.len() > 1 {
            let mut all: Vec<&BoundExpr> = vec![&x];
            all.extend(nonvars.iter());
            let common = self
                .select_common_type(&all, None)?
                .filter(|c| self.verify_common_type(*c, &all));
            if let Some(common) = common {
                let eq = self.oper("=", Some(common), common, span)?;
                if eq.result != oid::BOOL {
                    return Err(Error::new(
                        sqlstate::DATATYPE_MISMATCH,
                        "operator for IN must yield boolean",
                    )
                    .with_span(span));
                }
                let lhs = self.coerce_to_common_type(x.clone(), common, "IN")?;
                let lhs = self.coerce_to_common_type(lhs, eq.left.unwrap_or(common), "IN")?;
                let items = nonvars
                    .into_iter()
                    .map(|e| {
                        let e = self.coerce_to_common_type(e, common, "IN")?;
                        self.coerce_to_common_type(e, eq.right, "IN")
                    })
                    .collect::<Result<Vec<_>>>()?;
                parts.push(bool_expr(
                    BoundExprKind::InList {
                        expr: Box::new(lhs),
                        list: items,
                        eq_op: eq,
                        negated,
                    },
                    span,
                ));
            } else {
                rest.splice(0..0, nonvars);
            }
        } else {
            rest.splice(0..0, nonvars);
        }
        let opname = if negated { "<>" } else { "=" };
        for e in rest {
            let espan = e.span;
            let c = self.make_op(opname, Some(x.clone()), e, espan)?;
            parts.push(c);
        }
        if parts.len() == 1 {
            let p = parts.pop().expect("one part");
            return if matches!(p.kind, BoundExprKind::InList { .. }) {
                Ok(p)
            } else {
                self.coerce_to_boolean(p, if negated { "AND" } else { "OR" })
            };
        }
        self.bool_combine(negated, parts, span)
    }
}

/// Flattens nested AND (or OR) nodes into their operands.
fn collect_bool_operands<'e>(e: &'e Expr, is_and: bool, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And { left, right, .. } if is_and => {
            collect_bool_operands(left, is_and, out);
            collect_bool_operands(right, is_and, out);
        }
        Expr::Or { left, right, .. } if !is_and => {
            collect_bool_operands(left, is_and, out);
            collect_bool_operands(right, is_and, out);
        }
        other => out.push(other),
    }
}

fn transform_literal(value: &Literal, span: Span) -> Result<BoundExpr> {
    let (datum, ty) = match value {
        Literal::Integer(s) => match parse_int_literal(s) {
            Some(v) => match i32::try_from(v) {
                Ok(v4) => (Datum::Int4(v4), SqlType::INT4),
                Err(_) => (Datum::Int8(v), SqlType::INT8),
            },
            None => {
                return Err(Error::not_supported(
                    "type numeric is not supported yet (integer literal out of bigint range)",
                )
                .with_span(span));
            }
        },
        Literal::Decimal(_) => {
            return Err(Error::not_supported(
                "type numeric is not supported yet (write the value as '1.5'::float8)",
            )
            .with_span(span));
        }
        Literal::String(s) => (Datum::Text(s.clone()), SqlType::UNKNOWN),
        Literal::Bool(b) => (Datum::Bool(*b), SqlType::BOOL),
        Literal::Null => (Datum::Null, SqlType::UNKNOWN),
    };
    Ok(BoundExpr::new(BoundExprKind::Literal(datum), ty, span))
}
