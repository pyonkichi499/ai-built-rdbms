//! Type coercion (PostgreSQL's `parse_coerce.c`): coercion contexts, the
//! cast lookup (`find_coercion_pathway`), unknown literals, typmod
//! coercion and common-type resolution (`select_common_type`).
//!
//! Which conversion is used is decided by looking up `pg_cast`
//! (`CatalogReader::find_cast`) plus PostgreSQL's automatic I/O conversion
//! rule; nothing is hard-coded per type pair.

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind};
use super::params;
use crate::catalog::names::CatalogNames;
use crate::catalog::{CastContext, CastMethod, builtin};
use crate::error::{Error, Result, Span, sqlstate};
use crate::types::typeinfo::{self, ArrayKind};
use crate::types::{Datum, Oid, SqlType, io, oid, sys, type_display_name, typmod::takes_typmod};

/// PostgreSQL's `CoercionContext`. The order matters: a cast may be used
/// when `context >= cast.context`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum CoercionContext {
    Implicit,
    Assignment,
    Explicit,
}

impl CoercionContext {
    fn allows(self, c: CastContext) -> bool {
        let needed = match c {
            CastContext::Implicit => CoercionContext::Implicit,
            CastContext::Assignment => CoercionContext::Assignment,
            CastContext::Explicit => CoercionContext::Explicit,
        };
        self >= needed
    }
}

/// Result of `find_coercion_pathway`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Pathway {
    None,
    /// Binary-coercible (`RelabelType`).
    Relabel,
    /// A cast with a function or binary method from `pg_cast`.
    Cast(CastMethod),
    /// Output function of the source + input function of the target.
    CoerceViaIo,
    /// 配列から配列へ（要素ごとに変換。`m5/05` §5.4.4）。値は要素の変換方法。
    ArrayCoerce(CastMethod),
}

/// 通常の（`Datum::Array` の）配列型の要素型。`int2vector` / `oidvector` は `None`。
pub(super) fn regular_array_elem(t: Oid) -> Option<Oid> {
    (typeinfo::array_kind(t) == Some(ArrayKind::Regular))
        .then(|| typeinfo::element_type(t))
        .flatten()
}

/// 配列型を型として扱えるか（要素が配列の要素にできる型）。
fn is_supported_array(t: Oid) -> bool {
    regular_array_elem(t).is_some_and(typeinfo::is_array_element_type)
}

/// Format a type for messages (`format_type_be`).
pub(super) fn tname(oid: Oid) -> String {
    type_display_name(oid)
}

pub(super) fn type_not_supported(t: Oid) -> Error {
    Error::not_supported(format!("type {} is not supported yet", tname(t)))
}

impl Analyzer<'_> {
    /// `pg_type.typcategory` (`U` for unknown OIDs).
    pub(super) fn category(&self, t: Oid) -> char {
        self.catalog.type_by_oid(t).map_or('U', |t| t.category)
    }

    /// `pg_type.typispreferred`.
    pub(super) fn is_preferred(&self, t: Oid) -> bool {
        self.catalog.type_by_oid(t).is_some_and(|t| t.preferred)
    }

    /// PostgreSQL's `find_coercion_pathway(target, source, ccontext)`.
    pub(super) fn find_coercion_pathway(
        &self,
        target: Oid,
        source: Oid,
        ctx: CoercionContext,
    ) -> Pathway {
        if source == target {
            return Pathway::Relabel;
        }
        // 配列 → 配列: 要素の経路があれば ArrayCoerce（PostgreSQL の COERCION_PATH_ARRAYCOERCE）。
        if let (Some(te), Some(se)) = (regular_array_elem(target), regular_array_elem(source)) {
            return match self.find_coercion_pathway(te, se, ctx) {
                Pathway::None => Pathway::None,
                Pathway::Relabel => Pathway::ArrayCoerce(CastMethod::Binary),
                Pathway::CoerceViaIo => Pathway::ArrayCoerce(CastMethod::InOut),
                Pathway::Cast(m) | Pathway::ArrayCoerce(m) => Pathway::ArrayCoerce(m),
            };
        }
        if let Some(c) = self.catalog.find_cast(source, target) {
            if !ctx.allows(c.context) {
                return Pathway::None;
            }
            return match c.method {
                CastMethod::Binary => Pathway::Relabel,
                CastMethod::InOut => Pathway::CoerceViaIo,
                m @ (CastMethod::Function(_) | CastMethod::Env(_)) => Pathway::Cast(m),
            };
        }
        // No pg_cast entry: automatic I/O conversion to string types in
        // assignment context, from string types in explicit context.
        if ctx >= CoercionContext::Assignment && self.category(target) == 'S' {
            return Pathway::CoerceViaIo;
        }
        if ctx >= CoercionContext::Explicit && self.category(source) == 'S' {
            return Pathway::CoerceViaIo;
        }
        Pathway::None
    }

    /// PostgreSQL's `can_coerce_type` for one argument.
    pub(super) fn can_coerce(&self, input: Oid, target: Oid, ctx: CoercionContext) -> bool {
        if input == target {
            return true;
        }
        // `anynonarray` does not accept arrays.
        if builtin::is_polymorphic(target) {
            return self.category(input) != 'A';
        }
        if target == oid::ANYARRAY && self.category(input) == 'A' {
            return true;
        }
        if input == oid::UNKNOWN {
            return true;
        }
        !matches!(
            self.find_coercion_pathway(target, input, ctx),
            Pathway::None
        )
    }

    /// PostgreSQL's `coerce_to_target_type`: type conversion, then typmod
    /// coercion (`isExplicit` = explicit context). `Ok(None)` when there is
    /// no conversion path (the caller reports the error).
    pub(super) fn coerce_to_target_type(
        &self,
        expr: BoundExpr,
        target: SqlType,
        ctx: CoercionContext,
    ) -> Result<Option<BoundExpr>> {
        let Some(e) = self.coerce_type(expr, target.oid, ctx)? else {
            return Ok(None);
        };
        if let Some(te) = regular_array_elem(target.oid)
            && target.typmod >= 0
            && takes_typmod(te)
        {
            return Ok(Some(array_typmod(e, te, target, ctx)));
        }
        Ok(Some(coerce_typmod(
            e,
            target,
            ctx == CoercionContext::Explicit,
        )))
    }

    /// Input function for an unknown literal. regclass / regtype resolve names through the catalog.
    fn input_literal(&self, s: &str, ty: SqlType) -> Result<Datum> {
        let names = CatalogNames(self.catalog);
        match ty.oid {
            oid::REGCLASS => sys::regclass_in(s, Some(&names)).map(Datum::Oid),
            oid::REGTYPE => sys::regtype_in(s, Some(&names)).map(Datum::Oid),
            oid::REGNAMESPACE => sys::regnamespace_in(s, Some(&names)).map(Datum::Oid),
            _ => io::input_text(s, ty),
        }
    }

    /// PostgreSQL's `coerce_type` (no typmod). Unknown literals are
    /// converted right away with the target's input function, so invalid
    /// input is reported at analysis time (22P02), as in PostgreSQL.
    pub(super) fn coerce_type(
        &self,
        expr: BoundExpr,
        target: Oid,
        ctx: CoercionContext,
    ) -> Result<Option<BoundExpr>> {
        let src = expr.ty.oid;
        if src == target {
            return Ok(Some(expr));
        }
        if !builtin::is_supported_type(target) && !is_supported_array(target) {
            return Err(type_not_supported(target).with_span(expr.span));
        }
        if src == oid::UNKNOWN {
            if params::extern_param_index(&expr).is_some() {
                // H2: unknown の `$n` は型を確定するだけ（キャストを挟まない。typmod は適用しない）。
                return self.coerce_extern_param(expr, target).map(Some);
            }
            if let BoundExprKind::Literal(d) = &expr.kind {
                if matches!(d, Datum::Text(_)) && !io::input_is_eager(target) {
                    // date / timestamp / timestamptz depend on DateStyle, TimeZone and `now`:
                    // the planner folds `Cast(InOut)` of a literal (09 §4.1).
                    return Ok(Some(make_cast(
                        expr,
                        CastMethod::InOut,
                        target,
                        ctx != CoercionContext::Explicit,
                    )));
                }
                let datum = match d {
                    Datum::Text(s) => self
                        .input_literal(s, SqlType::of(target))
                        .map_err(|e| e.with_span(expr.span))?,
                    other => other.clone(),
                };
                return Ok(Some(BoundExpr::new(
                    BoundExprKind::Literal(datum),
                    SqlType::of(target),
                    expr.span,
                )));
            }
            if self.category(target) == 'S' {
                return Ok(Some(make_cast(
                    expr,
                    CastMethod::InOut,
                    target,
                    ctx != CoercionContext::Explicit,
                )));
            }
            return Ok(None);
        }
        let implicit = ctx != CoercionContext::Explicit;
        Ok(match self.find_coercion_pathway(target, src, ctx) {
            Pathway::None => None,
            Pathway::Relabel => Some(make_cast(expr, CastMethod::Binary, target, implicit)),
            Pathway::Cast(m) => Some(make_cast(expr, m, target, implicit)),
            Pathway::CoerceViaIo => Some(make_cast(expr, CastMethod::InOut, target, implicit)),
            Pathway::ArrayCoerce(m) => regular_array_elem(target)
                .map(|te| make_array_coerce(expr, m, SqlType::of(te), target, implicit)),
        })
    }

    /// `coerce_to_boolean`: WHERE, CHECK, AND/OR/NOT, CASE WHEN, IS TRUE.
    pub(super) fn coerce_to_boolean(&self, expr: BoundExpr, construct: &str) -> Result<BoundExpr> {
        self.coerce_to_specific_type(expr, SqlType::BOOL, construct)
    }

    /// `coerce_to_specific_type` (assignment context), e.g. LIMIT → int8.
    pub(super) fn coerce_to_specific_type(
        &self,
        expr: BoundExpr,
        target: SqlType,
        construct: &str,
    ) -> Result<BoundExpr> {
        if expr.ty.oid == target.oid {
            return Ok(expr);
        }
        let (src, span) = (expr.ty.oid, expr.span);
        self.coerce_to_target_type(expr, target, CoercionContext::Assignment)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "argument of {construct} must be type {}, not type {}",
                        tname(target.oid),
                        tname(src)
                    ),
                )
                .with_span(span)
            })
    }

    /// Explicit cast (`CAST(x AS t)`, `x::t`): 42846 if impossible.
    pub(super) fn coerce_explicit(
        &self,
        expr: BoundExpr,
        target: SqlType,
        span: Span,
    ) -> Result<BoundExpr> {
        let src = expr.ty.oid;
        self.coerce_to_target_type(expr, target, CoercionContext::Explicit)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::CANNOT_COERCE,
                    format!("cannot cast type {} to {}", tname(src), tname(target.oid)),
                )
                .with_span(span)
            })
    }

    /// Assignment to a column (`transformAssignedExpr`): assignment cast,
    /// then typmod with `isExplicit = false`. `what` is `expression` or
    /// `default expression`.
    pub(super) fn coerce_assignment(
        &self,
        expr: BoundExpr,
        col_name: &str,
        col_ty: SqlType,
        what: &str,
    ) -> Result<BoundExpr> {
        let (src, span) = (expr.ty.oid, expr.span);
        self.coerce_to_target_type(expr, col_ty, CoercionContext::Assignment)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "column \"{col_name}\" is of type {} but {what} is of type {}",
                        tname(col_ty.oid),
                        tname(src)
                    ),
                )
                .with_hint("You will need to rewrite or cast the expression.")
                .with_span(span)
            })
    }

    /// PostgreSQL's `select_common_type`. `context` (`CASE`, `VALUES`, ...)
    /// names the construct in the 42804 error; with `None` a mismatch gives
    /// `Ok(None)` instead of an error.
    pub(super) fn select_common_type(
        &self,
        exprs: &[&BoundExpr],
        context: Option<&str>,
    ) -> Result<Option<Oid>> {
        let Some(first) = exprs.first() else {
            return Ok(Some(oid::TEXT));
        };
        let mut ptype = first.ty.oid;
        if ptype != oid::UNKNOWN && exprs.iter().all(|e| e.ty.oid == ptype) {
            return Ok(Some(ptype));
        }
        let mut pcat = self.category(ptype);
        let mut ppref = self.is_preferred(ptype);
        for e in &exprs[1..] {
            let ntype = e.ty.oid;
            if ntype == oid::UNKNOWN || ntype == ptype {
                continue;
            }
            let ncat = self.category(ntype);
            let npref = self.is_preferred(ntype);
            if ptype == oid::UNKNOWN {
                (ptype, pcat, ppref) = (ntype, ncat, npref);
            } else if ncat != pcat {
                let Some(ctx) = context else {
                    return Ok(None);
                };
                return Err(Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "{ctx} types {} and {} cannot be matched",
                        tname(ptype),
                        tname(ntype)
                    ),
                )
                .with_span(e.span));
            } else if !ppref
                && self.can_coerce(ptype, ntype, CoercionContext::Implicit)
                && !self.can_coerce(ntype, ptype, CoercionContext::Implicit)
            {
                (ptype, pcat, ppref) = (ntype, ncat, npref);
            }
        }
        if ptype == oid::UNKNOWN {
            ptype = oid::TEXT;
        }
        Ok(Some(ptype))
    }

    /// PostgreSQL's `verify_common_type`.
    pub(super) fn verify_common_type(&self, common: Oid, exprs: &[&BoundExpr]) -> bool {
        exprs
            .iter()
            .all(|e| self.can_coerce(e.ty.oid, common, CoercionContext::Implicit))
    }

    /// PostgreSQL's `coerce_to_common_type` (implicit, no typmod).
    pub(super) fn coerce_to_common_type(
        &self,
        expr: BoundExpr,
        target: Oid,
        context: &str,
    ) -> Result<BoundExpr> {
        let src = expr.ty.oid;
        if src == target {
            return Ok(expr);
        }
        let span = expr.span;
        if self.can_coerce(src, target, CoercionContext::Implicit)
            && let Some(e) = self.coerce_type(expr, target, CoercionContext::Implicit)?
        {
            return Ok(e);
        }
        Err(Error::new(
            sqlstate::CANNOT_COERCE,
            format!(
                "{context} could not convert type {} to {}",
                tname(src),
                tname(target)
            ),
        )
        .with_span(span))
    }

    /// Resolves a list of expressions to their common type and coerces
    /// them (CASE results, COALESCE, VALUES columns). Returns the common
    /// type with `select_common_typmod`.
    pub(super) fn coerce_all_to_common(
        &self,
        exprs: Vec<BoundExpr>,
        context: &str,
    ) -> Result<(Vec<BoundExpr>, SqlType)> {
        let refs: Vec<&BoundExpr> = exprs.iter().collect();
        let common = self
            .select_common_type(&refs, Some(context))?
            .unwrap_or(oid::TEXT);
        let typmod = select_common_typmod(&refs, common);
        let out = exprs
            .into_iter()
            .map(|e| self.coerce_to_common_type(e, common, context))
            .collect::<Result<Vec<_>>>()?;
        Ok((out, SqlType::new(common, typmod)))
    }
}

/// PostgreSQL's `select_common_typmod`: the typmod if every input has the
/// common type with the same typmod, else -1.
pub(super) fn select_common_typmod(exprs: &[&BoundExpr], common: Oid) -> i32 {
    let mut result: Option<i32> = None;
    for e in exprs {
        if e.ty.oid != common {
            return -1;
        }
        match result {
            None => result = Some(e.ty.typmod),
            Some(t) if t != e.ty.typmod => return -1,
            Some(_) => {}
        }
    }
    result.unwrap_or(-1)
}

fn make_cast(expr: BoundExpr, method: CastMethod, target: Oid, implicit: bool) -> BoundExpr {
    let span = expr.span;
    BoundExpr::new(
        BoundExprKind::Cast {
            expr: Box::new(expr),
            method,
            implicit,
        },
        SqlType::of(target),
        span,
    )
}

/// PostgreSQL's `coerce_type_typmod`: applies the length coercion when the
/// target has a typmod different from the expression's (varchar, bpchar, numeric,
/// timestamp, timestamptz).
pub(super) fn coerce_typmod(expr: BoundExpr, target: SqlType, explicit: bool) -> BoundExpr {
    if target.typmod < 0 || target.typmod == expr.ty.typmod || !takes_typmod(target.oid) {
        return expr;
    }
    let span = expr.span;
    BoundExpr::new(
        BoundExprKind::CoerceTypmod {
            expr: Box::new(expr),
            explicit,
        },
        target,
        span,
    )
}

/// Resolves a still-unknown expression (a string literal or NULL) to text,
/// as PostgreSQL does for output columns (`resolveTargetListUnknowns`).
pub(super) fn resolve_unknown(mut expr: BoundExpr) -> BoundExpr {
    if expr.ty.oid != oid::UNKNOWN || matches!(expr.kind, BoundExprKind::ExternParam(_)) {
        return expr;
    }
    if matches!(expr.kind, BoundExprKind::Literal(_)) {
        expr.ty = SqlType::TEXT;
        expr
    } else {
        make_cast(expr, CastMethod::InOut, oid::TEXT, true)
    }
}

fn make_array_coerce(
    expr: BoundExpr,
    method: CastMethod,
    elem_ty: SqlType,
    target: Oid,
    implicit: bool,
) -> BoundExpr {
    let span = expr.span;
    BoundExpr::new(
        BoundExprKind::ArrayCoerce {
            expr: Box::new(expr),
            elem_method: method,
            elem_ty,
            explicit: !implicit,
        },
        SqlType::of(target),
        span,
    )
}

/// 配列の typmod（`varchar(3)[]` など）を要素ごとに適用する。すでに `ArrayCoerce` なら要素の typmod を
/// 差し替え、そうでなければ要素の変換なし（`Binary`）の `ArrayCoerce` で包む。
fn array_typmod(expr: BoundExpr, elem: Oid, target: SqlType, ctx: CoercionContext) -> BoundExpr {
    if expr.ty.typmod == target.typmod {
        return expr;
    }
    let elem_ty = SqlType::new(elem, target.typmod);
    let (span, ty) = (expr.span, expr.ty);
    let kind = match expr.kind {
        BoundExprKind::ArrayCoerce {
            expr: inner,
            elem_method,
            explicit,
            ..
        } => BoundExprKind::ArrayCoerce {
            expr: inner,
            elem_method,
            elem_ty,
            explicit,
        },
        kind => BoundExprKind::ArrayCoerce {
            expr: Box::new(BoundExpr::new(kind, ty, span)),
            elem_method: CastMethod::Binary,
            elem_ty,
            explicit: ctx == CoercionContext::Explicit,
        },
    };
    BoundExpr::new(kind, target, span)
}
