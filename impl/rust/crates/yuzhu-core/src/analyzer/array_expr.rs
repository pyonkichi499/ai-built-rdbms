//! 配列の式の解析（`m5/05-types-core.md` §5.4.2〜§5.4.5）: `ARRAY[...]`、`ARRAY(SELECT ...)`、`a[i]`、
//! `x op ANY|ALL (array)`、`T[]` への型名の解決。
//!
//! M5 は 1 次元だけ。多次元・スライス・添字代入は `0A000`。

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind};
use super::coerce::{CoercionContext, regular_array_elem, tname};
use super::expr::ExprCtx;
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::SubLinkKind;
use crate::sql::ast::{Expr, Quantifier, TypeName};
use crate::types::typeinfo::{self, ArrayKind};
use crate::types::{Oid, SqlType, oid};

fn no_array_type(elem: Oid, span: Span) -> Error {
    Error::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("could not find array type for data type {}", tname(elem)),
    )
    .with_span(span)
}

fn multidim(span: Span) -> Error {
    Error::not_supported("multidimensional arrays are not supported yet").with_span(span)
}

impl Analyzer<'_> {
    /// 式の中の型名（`::T`、`CAST(.. AS T)`）。`T[]`（1 次元）は要素型の配列型。
    /// DDL の列の型は `resolve_type_name` のまま（ユーザー表の配列列は M5 では作らない。TY-D4）。
    pub(super) fn resolve_cast_type(&self, tn: &TypeName) -> Result<SqlType> {
        if tn.array_bounds.is_empty() {
            return self.resolve_type_name(tn);
        }
        if tn.array_bounds.len() > 1 {
            return Err(multidim(tn.span));
        }
        let mut base = tn.clone();
        base.array_bounds.clear();
        let elem = self.resolve_type_name(&base)?;
        let Some(arr) =
            typeinfo::array_type_of(elem.oid).filter(|_| typeinfo::is_array_element_type(elem.oid))
        else {
            return Err(no_array_type(elem.oid, tn.span));
        };
        Ok(SqlType::new(arr, elem.typmod))
    }

    /// `ARRAY[e1, ...]`。`target` は `ARRAY[...]::T[]` の要素型（各要素を明示キャストする）。
    pub(super) fn transform_array_ctor(
        &self,
        elems: &[Expr],
        target: Option<SqlType>,
        span: Span,
        cx: &ExprCtx<'_>,
    ) -> Result<BoundExpr> {
        // 入れ子の `ARRAY[[1,2],[3,4]]` は内側の `[..]` も ArrayConstructor。
        if elems
            .iter()
            .any(|e| matches!(e, Expr::ArrayConstructor { .. }))
        {
            return Err(multidim(span));
        }
        let bound = elems
            .iter()
            .map(|e| self.transform_expr(e, cx))
            .collect::<Result<Vec<_>>>()?;
        if let Some(b) = bound
            .iter()
            .find(|b| typeinfo::array_kind(b.ty.oid).is_some())
        {
            return Err(multidim(b.span));
        }
        #[allow(clippy::single_match_else)]
        let (elems, elem_ty) = match target {
            Some(t) => {
                let elems = bound
                    .into_iter()
                    .map(|b| {
                        let s = b.span;
                        self.coerce_explicit(b, t, s)
                    })
                    .collect::<Result<Vec<_>>>()?;
                (elems, t)
            }
            None => {
                if bound.is_empty() {
                    return Err(Error::new(
                        sqlstate::INDETERMINATE_DATATYPE,
                        "cannot determine type of empty array",
                    )
                    .with_hint(
                        "Explicitly cast to the desired type, for example ARRAY[]::integer[].",
                    )
                    .with_span(span));
                }
                let (elems, common) = self.coerce_all_to_common(bound, "ARRAY")?;
                (elems, SqlType::of(common.oid))
            }
        };
        let arr = typeinfo::array_type_of(elem_ty.oid)
            .filter(|_| typeinfo::is_array_element_type(elem_ty.oid))
            .ok_or_else(|| no_array_type(elem_ty.oid, span))?;
        Ok(BoundExpr::new(
            BoundExprKind::ArrayCtor {
                elem_type: elem_ty.oid,
                elems,
            },
            SqlType::new(arr, if target.is_some() { elem_ty.typmod } else { -1 }),
            span,
        ))
    }

    /// `ARRAY(SELECT ...)` の SubLink（本体は `analyze_sublink` が解析済み）。
    #[allow(clippy::unused_self)]
    pub(super) fn array_sublink(
        &self,
        sub: super::bound::BoundQuery,
        span: Span,
    ) -> Result<BoundExpr> {
        if sub.columns.len() != 1 {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                "subquery must return only one column",
            )
            .with_span(span));
        }
        let elem = sub.columns[0].ty;
        if typeinfo::array_kind(elem.oid).is_some() {
            return Err(multidim(span));
        }
        let arr = typeinfo::array_type_of(elem.oid).ok_or_else(|| no_array_type(elem.oid, span))?;
        Ok(BoundExpr::new(
            BoundExprKind::SubLink {
                kind: SubLinkKind::Array,
                test: None,
                query: Box::new(sub),
            },
            SqlType::of(arr),
            span,
        ))
    }

    /// `a[i]`。
    pub(super) fn transform_subscript(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr> {
        let Expr::Subscript {
            expr,
            lower,
            upper,
            slice,
            span,
        } = e
        else {
            return Err(Error::internal("transform_subscript without a subscript"));
        };
        if *slice || upper.is_some() {
            return Err(Error::not_supported("array slices are not supported yet").with_span(*span));
        }
        let base = self.transform_expr(expr, cx)?;
        let Some(elem) = typeinfo::element_type(base.ty.oid) else {
            return Err(Error::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "cannot subscript type {} because it does not support subscripting",
                    tname(base.ty.oid)
                ),
            )
            .with_span(*span));
        };
        let Some(index) = lower else {
            return Err(Error::internal("subscript without an index"));
        };
        let idx = self.transform_expr(index, cx)?;
        let idx_span = idx.span;
        let idx = self
            .coerce_to_target_type(idx, SqlType::INT4, CoercionContext::Assignment)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    "array subscript must have type integer",
                )
                .with_span(idx_span)
            })?;
        let typmod = if typeinfo::array_kind(base.ty.oid) == Some(ArrayKind::Regular) {
            base.ty.typmod
        } else {
            -1
        };
        Ok(BoundExpr::new(
            BoundExprKind::ArraySubscript {
                array: Box::new(base),
                indexes: vec![idx],
            },
            SqlType::new(elem, typmod),
            *span,
        ))
    }

    /// `x op ANY|SOME|ALL (array)`（`make_scalar_array_op`）。
    pub(super) fn transform_quantified_array(
        &self,
        e: &Expr,
        cx: &ExprCtx<'_>,
    ) -> Result<BoundExpr> {
        let Expr::QuantifiedArray {
            expr,
            op,
            op_schema,
            quantifier,
            array,
            span,
        } = e
        else {
            return Err(Error::internal(
                "transform_quantified_array without a quantified array",
            ));
        };
        if let Some(s) = op_schema.as_ref().filter(|s| s.value != "pg_catalog") {
            return Err(super::not_supported(
                "OPERATOR() in an array comparison",
                s.span,
            ));
        }
        let left = self.transform_expr(expr, cx)?;
        let right = self.transform_expr(array, cx)?;
        let quant = match quantifier {
            Quantifier::Any => "ANY",
            Quantifier::All => "ALL",
        };
        let right_elem = if right.ty.oid == oid::UNKNOWN {
            oid::UNKNOWN
        } else {
            typeinfo::element_type(right.ty.oid).ok_or_else(|| {
                Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("op {quant} (array) requires array on right side"),
                )
                .with_span(*span)
            })?
        };
        let operator = self.oper(op, Some(left.ty.oid), right_elem, *span)?;
        if operator.result != oid::BOOL {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("op {quant} (array) requires operator to yield boolean"),
            )
            .with_span(*span));
        }
        let left = match operator.left {
            Some(lt) => self.coerce_operand(left, lt)?,
            None => left,
        };
        // 右辺は演算子の右の型の配列にそろえる。`int2vector` / `oidvector` は評価側が配列として読む。
        let right_is_vector = typeinfo::array_kind(right.ty.oid) == Some(ArrayKind::Vector);
        let right = if right_is_vector && right_elem == operator.right {
            right
        } else if let Some(arr) = typeinfo::array_type_of(operator.right)
            && regular_array_elem(arr).is_some()
        {
            self.coerce_operand(right, arr)?
        } else {
            right
        };
        Ok(BoundExpr::new(
            BoundExprKind::ScalarArrayOp {
                op: operator,
                use_or: matches!(quantifier, Quantifier::Any),
                left: Box::new(left),
                array: Box::new(right),
            },
            SqlType::BOOL,
            *span,
        ))
    }

    /// 解決済みの演算子の引数の型へ暗黙変換する。多相の宣言（`anynonarray` など）はそのまま。
    fn coerce_operand(&self, e: BoundExpr, declared: Oid) -> Result<BoundExpr> {
        if declared == e.ty.oid
            || declared == oid::ANYARRAY
            || crate::catalog::builtin::is_polymorphic(declared)
        {
            return Ok(e);
        }
        let (src, span) = (e.ty.oid, e.span);
        self.coerce_type(e, declared, CoercionContext::Implicit)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "cannot convert type {} to {} for the array comparison",
                        tname(src),
                        tname(declared)
                    ),
                )
                .with_span(span)
            })
    }
}
