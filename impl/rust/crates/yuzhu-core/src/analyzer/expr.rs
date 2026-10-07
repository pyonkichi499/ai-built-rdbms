//! Expression analysis (PostgreSQL's `transformExpr`) and output column
//! names (`FigureColname`).

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind};
use super::coerce::{CoercionContext, resolve_unknown, tname};
use super::cte::CteScope;
use super::scope::{ParseExprKind, ScopeStack};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{BoolTestKind, Var};
use crate::sql::ast::{BoolTestValue, Expr, Literal, SessionValueKind, WhenClause};
use crate::types::{Datum, SqlType, oid};

/// Where an expression is analyzed.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExprCtx<'s> {
    pub(super) scopes: &'s ScopeStack,
    pub(super) kind: ParseExprKind,
    /// 副問い合わせ・CTE 参照の解析に使う CTE スコープ（N3）。持たない文脈（DEFAULT・CHECK）は `None`。
    #[allow(dead_code)]
    pub(super) ctes: Option<&'s CteScope<'s>>,
}

impl<'s> ExprCtx<'s> {
    pub(super) fn new(scopes: &'s ScopeStack, kind: ParseExprKind) -> Self {
        ExprCtx {
            scopes,
            kind,
            ctes: None,
        }
    }

    #[must_use]
    pub(super) fn with_ctes(self, ctes: &'s CteScope<'s>) -> Self {
        ExprCtx {
            ctes: Some(ctes),
            ..self
        }
    }
}

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

/// レベル 0 の `Var`（同じスコープの列）を含むか。LIMIT・OFFSET の検査と IN リストの分類に使う
/// （PostgreSQL の `contain_vars_of_level(.., 0)`）。
pub(super) fn contains_column_ref(e: &BoundExpr) -> bool {
    e.columns().iter().any(|v| v.levels_up == 0)
}

/// 式が参照するレベル 0・`rte = 0` の列がちょうど 1 種類のとき、その列の位置
/// （PostgreSQL は CHECK を、列がちょうど 1 つのときだけ `<table>_<col>_check` と名づける。
/// それ以外は `<table>_check`）。
pub(super) fn sole_column_ref(e: &BoundExpr) -> Option<usize> {
    let mut cols: Vec<Var> = Vec::new();
    for v in e.columns() {
        if !cols.contains(&v) {
            cols.push(v);
        }
    }
    match cols[..] {
        [v] if v.levels_up == 0 && v.rte.0 == 0 && !v.is_system() => Some(usize::from(v.col)),
        _ => None,
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
        Expr::Case { else_result, .. } => {
            if let Some(d) = else_result {
                let (n, strength) = figure_colname_internal(d);
                if strength > 1 {
                    return (n, strength);
                }
            }
            (Some("case".to_owned()), 1)
        }
        Expr::Coalesce { .. } => (Some("coalesce".to_owned()), 2),
        Expr::MinMax { greatest, .. } => (
            Some(if *greatest { "greatest" } else { "least" }.to_owned()),
            2,
        ),
        Expr::NullIf { .. } => (Some("nullif".to_owned()), 2),
        Expr::Exists { .. } => (Some("exists".to_owned()), 2),
        Expr::Subquery { query, .. } => (Some(super::sublink::subquery_colname(query)), 2),
        Expr::Collate { expr, .. } => figure_colname_internal(expr),
        Expr::Row { .. } => (Some("row".to_owned()), 2),
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
    /// 列参照の名前の部品（`a`、`t.a`、`s.t.a`、`db.s.t.a`）。4 部は先頭がデータベース名で、現在の
    /// データベースなら 3 部として続ける。違えば `0A000`、5 部以上は `42601`。
    fn column_ref_parts<'p>(
        &self,
        parts: &'p [crate::sql::ast::Ident],
        span: Span,
    ) -> Result<&'p [crate::sql::ast::Ident]> {
        let shown = || {
            parts
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>()
                .join(".")
        };
        match parts.len() {
            0..=3 => Ok(parts),
            4 if parts[0].value == self.catalog.current_database() => Ok(&parts[1..]),
            4 => Err(Error::not_supported(format!(
                "cross-database references are not implemented: {}",
                shown()
            ))
            .with_span(span)),
            _ => Err(Error::syntax_at(
                span,
                format!(
                    "improper qualified name (too many dotted names): {}",
                    shown()
                ),
            )),
        }
    }

    /// Resolves a comparison operator for two operands and returns it with
    /// the operands coerced to its argument types.
    fn resolve_cmp_op(
        &self,
        name: &str,
        l: BoundExpr,
        r: BoundExpr,
        span: Span,
    ) -> Result<(
        &'static crate::catalog::BuiltinOperator,
        BoundExpr,
        BoundExpr,
    )> {
        let resolved = self.make_op(name, Some(l), r, span)?;
        let BoundExprKind::Operator { op, args } = resolved.kind else {
            return Err(Error::internal("comparison did not resolve to an operator"));
        };
        if op.result != oid::BOOL {
            return Err(Error::new(
                sqlstate::DATATYPE_MISMATCH,
                format!("operator {name} must return type boolean"),
            )
            .with_span(span));
        }
        let mut it = args.into_iter();
        let (Some(l), Some(r)) = (it.next(), it.next()) else {
            return Err(Error::internal("comparison operator without two arguments"));
        };
        Ok((op, l, r))
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn transform_expr(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr> {
        match e {
            Expr::Literal { value, span } => transform_literal(value, *span),
            Expr::Column { parts, span } => {
                if cx.kind == ParseExprKind::ColumnDefault {
                    return Err(Error::not_supported(
                        "cannot use column reference in DEFAULT expression",
                    )
                    .with_span(*span));
                }
                let parts = self.column_ref_parts(parts, *span)?;
                cx.scopes.resolve_column(parts, *span)
            }
            Expr::Parameter { index, span } => Err(Error::new(
                sqlstate::UNDEFINED_PARAMETER,
                format!("there is no parameter ${index}"),
            )
            .with_span(*span)),
            Expr::BinaryOp {
                op,
                op_schema,
                left,
                right,
                span,
            } => {
                let l = self.transform_expr(left, cx)?;
                let r = self.transform_expr(right, cx)?;
                self.qualified_operator(op_schema.as_ref(), op, Some(l), r, *span)
            }
            Expr::UnaryOp {
                op,
                op_schema,
                expr,
                span,
            } => {
                let a = self.transform_expr(expr, cx)?;
                self.qualified_operator(op_schema.as_ref(), op, None, a, *span)
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
            Expr::IsDistinctFrom {
                left,
                right,
                negated,
                span,
            } => {
                let l = self.transform_expr(left, cx)?;
                let r = self.transform_expr(right, cx)?;
                // PG の transformAExprDistinct: NULL 定数との比較は NullTest に畳む。
                let is_null_const =
                    |e: &BoundExpr| matches!(&e.kind, BoundExprKind::Literal(Datum::Null));
                let other = if is_null_const(&r) {
                    Some(l.clone())
                } else if is_null_const(&l) {
                    Some(r.clone())
                } else {
                    None
                };
                if let Some(other) = other {
                    let a = Box::new(resolve_unknown(other));
                    return Ok(bool_expr(
                        if *negated {
                            BoundExprKind::IsNull(a)
                        } else {
                            BoundExprKind::IsNotNull(a)
                        },
                        *span,
                    ));
                }
                let (eq_op, l, r) = self.resolve_cmp_op("=", l, r, *span)?;
                Ok(bool_expr(
                    BoundExprKind::DistinctFrom {
                        left: Box::new(l),
                        right: Box::new(r),
                        eq_op,
                        negated: *negated,
                    },
                    *span,
                ))
            }
            Expr::MinMax {
                greatest,
                args,
                span,
            } => {
                let name = if *greatest { "GREATEST" } else { "LEAST" };
                let args = args
                    .iter()
                    .map(|a| self.transform_expr(a, cx))
                    .collect::<Result<Vec<_>>>()?;
                let (args, ty) = self.coerce_all_to_common(args, name)?;
                let first = args
                    .first()
                    .cloned()
                    .ok_or_else(|| Error::internal("GREATEST/LEAST without arguments"))?;
                let (cmp, _, _) = self.resolve_cmp_op(
                    if *greatest { ">" } else { "<" },
                    first.clone(),
                    first,
                    *span,
                )?;
                Ok(BoundExpr::new(
                    BoundExprKind::MinMax {
                        greatest: *greatest,
                        args,
                        cmp,
                    },
                    ty,
                    *span,
                ))
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
                name, args, span, ..
            } => {
                let fname = &name.name().value;
                if let Some(agg) = self.analyze_agg_call(e, cx)? {
                    return Ok(agg);
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
            Expr::Collate {
                expr,
                collation,
                span,
            } => {
                let a = self.transform_expr(expr, cx)?;
                self.collate(a, collation, *span)
            }
            Expr::Row { span, .. } => {
                Err(Error::not_supported("row constructors is not supported yet").with_span(*span))
            }
            Expr::InSubquery { span, .. }
            | Expr::QuantifiedSubquery { span, .. }
            | Expr::Exists { span, .. }
            | Expr::Subquery { span, .. } => {
                if cx.kind == ParseExprKind::ColumnDefault {
                    return Err(
                        Error::not_supported("cannot use subquery in DEFAULT expression")
                            .with_span(*span),
                    );
                }
                self.analyze_sublink(e, cx)
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
                session_value_type(*kind),
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
                let eq = self.oper("=", Some(x.ty.oid), common, span)?;
                if eq.result != oid::BOOL {
                    return Err(Error::new(
                        sqlstate::DATATYPE_MISMATCH,
                        "operator for IN must yield boolean",
                    )
                    .with_span(span));
                }
                // PostgreSQL converts the list elements first, the left side last.
                let items = nonvars
                    .into_iter()
                    .map(|e| {
                        let e = self.coerce_to_common_type(e, common, "IN")?;
                        self.coerce_to_common_type(e, eq.right, "IN")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lhs = self.coerce_to_common_type(x.clone(), eq.left.unwrap_or(common), "IN")?;
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

/// Flattens a left-nested AND (or OR) chain into its operands. Like PostgreSQL's `makeAndExpr` /
/// `makeOrExpr`, only the left operand is flattened: `a AND (b AND c)` keeps its nested node
/// (visible in `pg_get_constraintdef`).
fn collect_bool_operands<'e>(e: &'e Expr, is_and: bool, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And { left, right, .. } if is_and => {
            collect_bool_operands(left, is_and, out);
            out.push(right);
        }
        Expr::Or { left, right, .. } if !is_and => {
            collect_bool_operands(left, is_and, out);
            out.push(right);
        }
        other => out.push(other),
    }
}

/// Type of a SQL value function (09 §6.4). A negative precision means no typmod; values above 6
/// are clamped to 6.
fn session_value_type(kind: SessionValueKind) -> SqlType {
    let typmod = |p: i32| if p < 0 { -1 } else { p.min(6) };
    match kind {
        SessionValueKind::CurrentDate => SqlType::DATE,
        SessionValueKind::CurrentTimestamp { precision } => {
            SqlType::new(oid::TIMESTAMPTZ, typmod(precision))
        }
        SessionValueKind::Now | SessionValueKind::TransactionTimestamp => SqlType::TIMESTAMPTZ,
        SessionValueKind::LocalTimestamp { precision } => {
            SqlType::new(oid::TIMESTAMP, typmod(precision))
        }
        _ => SqlType::NAME,
    }
}

fn numeric_literal(s: &str, span: Span) -> Result<(Datum, SqlType)> {
    let n = yuzhu_numeric::Numeric::parse(s).map_err(|e| Error::from(e).with_span(span))?;
    Ok((Datum::Numeric(n), SqlType::NUMERIC))
}

fn transform_literal(value: &Literal, span: Span) -> Result<BoundExpr> {
    let (datum, ty) = match value {
        Literal::Integer(s) => match parse_int_literal(s) {
            Some(v) => match i32::try_from(v) {
                Ok(v4) => (Datum::Int4(v4), SqlType::INT4),
                Err(_) => (Datum::Int8(v), SqlType::INT8),
            },
            None => numeric_literal(s, span)?,
        },
        Literal::Decimal(s) => numeric_literal(s, span)?,
        Literal::String(s) => (Datum::Text(s.clone()), SqlType::UNKNOWN),
        Literal::Bool(b) => (Datum::Bool(*b), SqlType::BOOL),
        Literal::Null => (Datum::Null, SqlType::UNKNOWN),
    };
    Ok(BoundExpr::new(BoundExprKind::Literal(datum), ty, span))
}
