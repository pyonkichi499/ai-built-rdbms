//! Expression evaluation. (Owned by the executor implementer.)
//!
//! Evaluation order is left to right. Strict operators/functions/casts
//! return NULL without being called when an argument is NULL; `AND`/`OR`
//! follow three-valued logic and stop at the first deciding operand;
//! `CASE` and `COALESCE` evaluate lazily (`COALESCE(1, 1/0)` is 1).
//!
//! This module also holds the output stage that turns result rows into
//! text ([`row_to_text`]), including the `regproc` display rule.

use super::{EvalCtx, ExecCtx, SessionInfo};
use crate::analyzer::{BoolTestKind, BoundExpr, BoundExprKind, SessionValueKind};
use crate::catalog::{BuiltinFunction, BuiltinOperator, CastMethod, FnKind, builtin};
use crate::error::{Error, Result, sqlstate};
use crate::types::io::OutputOpts;
use crate::types::{Datum, Oid, Row, SqlType, io, ops};

/// The evaluation context (session values and catalog) of a running
/// statement.
pub fn eval_ctx<'a>(ctx: &ExecCtx<'a>) -> EvalCtx<'a> {
    EvalCtx {
        session: ctx.session,
        catalog: ctx.catalog,
        runtime: ctx.runtime,
    }
}

/// Evaluates `expr` over `row`. AND/OR use three-valued logic; strict
/// functions and operators return NULL without being called when an
/// argument is NULL.
pub fn eval(expr: &BoundExpr, row: &Row, ctx: &ExecCtx<'_>) -> Result<Datum> {
    eval_expr(expr, row, &eval_ctx(ctx))
}

/// Like [`eval`], but takes only the session values and the catalog.
pub fn eval_expr(expr: &BoundExpr, row: &Row, ctx: &EvalCtx<'_>) -> Result<Datum> {
    Evaluator { row, ctx }.eval(expr)
}

/// Evaluates a predicate: `Some(b)` for a boolean, `None` for NULL.
pub fn eval_bool(expr: &BoundExpr, row: &Row, ctx: &EvalCtx<'_>) -> Result<Option<bool>> {
    let d = eval_expr(expr, row, ctx)?;
    to_bool(&d)
}

/// [`eval_bool`] for node code that holds an [`ExecCtx`].
pub fn eval_pred(expr: &BoundExpr, row: &Row, ctx: &ExecCtx<'_>) -> Result<Option<bool>> {
    eval_bool(expr, row, &eval_ctx(ctx))
}

/// The display name of a function OID for `regproc` output (PostgreSQL's
/// `regprocout`): the bare name if it is unique among the built-in
/// functions, `pg_catalog.name` if overloaded, `None` if unknown.
pub fn regproc_name(oid: Oid) -> Option<String> {
    builtin::regproc_name(oid)
}

/// The output stage: converts a result row to text values. `types` gives
/// the column types; `regproc` columns are shown by function name.
pub fn row_to_text(row: &Row, types: &[SqlType], opts: &OutputOpts) -> Vec<Option<String>> {
    row.iter()
        .enumerate()
        .map(|(i, d)| {
            let ty = types.get(i).copied().unwrap_or(SqlType::TEXT);
            io::output_text_regproc(d, ty, opts, &regproc_name)
        })
        .collect()
}

fn to_bool(d: &Datum) -> Result<Option<bool>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(*b)),
        other => Err(Error::internal(format!(
            "expected a boolean value, got {other:?}"
        ))),
    }
}

/// Calls a built-in function. `FnKind::Context` functions also get the
/// catalog and the session.
fn call_function(func: &BuiltinFunction, vals: &[Datum], ctx: &EvalCtx<'_>) -> Result<Datum> {
    match func.kind {
        FnKind::Pure(f) => f(vals),
        FnKind::Context(f) => f(vals, ctx.catalog, ctx.session),
        FnKind::Runtime(f) => f(vals, ctx.runtime),
    }
}

struct Evaluator<'a> {
    row: &'a Row,
    ctx: &'a EvalCtx<'a>,
}

impl Evaluator<'_> {
    fn eval(&self, expr: &BoundExpr) -> Result<Datum> {
        match &expr.kind {
            BoundExprKind::Literal(d) => Ok(d.clone()),
            BoundExprKind::ColumnRef { index } => self
                .row
                .get(*index)
                .cloned()
                .ok_or_else(|| Error::internal(format!("column index {index} out of range"))),
            BoundExprKind::Operator { op, args } => {
                let Some(vals) = self.eval_strict_args(args)? else {
                    return Ok(Datum::Null);
                };
                (op.func)(&vals)
            }
            BoundExprKind::Function { func, args } => {
                if func.strict {
                    let Some(vals) = self.eval_strict_args(args)? else {
                        return Ok(Datum::Null);
                    };
                    call_function(func, &vals, self.ctx)
                } else {
                    let vals = args
                        .iter()
                        .map(|a| self.eval(a))
                        .collect::<Result<Vec<_>>>()?;
                    call_function(func, &vals, self.ctx)
                }
            }
            BoundExprKind::Cast {
                expr: inner,
                method,
            } => {
                let d = self.eval(inner)?;
                apply_cast(d, inner.ty, expr.ty, *method)
            }
            BoundExprKind::CoerceTypmod {
                expr: inner,
                explicit,
            } => {
                let d = self.eval(inner)?;
                coerce_typmod(d, expr.ty, *explicit)
            }
            BoundExprKind::And(args) => self.eval_and(args),
            BoundExprKind::Or(args) => self.eval_or(args),
            BoundExprKind::Not(inner) => Ok(match to_bool(&self.eval(inner)?)? {
                None => Datum::Null,
                Some(b) => Datum::Bool(!b),
            }),
            BoundExprKind::IsNull(inner) => Ok(Datum::Bool(self.eval(inner)?.is_null())),
            BoundExprKind::IsNotNull(inner) => Ok(Datum::Bool(!self.eval(inner)?.is_null())),
            BoundExprKind::BoolTest { expr: inner, test } => {
                let v = to_bool(&self.eval(inner)?)?;
                Ok(Datum::Bool(bool_test(v, *test)))
            }
            BoundExprKind::Case { arms, else_result } => {
                self.eval_case(arms, else_result.as_deref())
            }
            BoundExprKind::Coalesce(args) => self.eval_coalesce(args),
            BoundExprKind::NullIf { left, right, eq_op } => self.eval_nullif(left, right, eq_op),
            BoundExprKind::Like {
                expr: inner,
                pattern,
                escape,
                negated,
                case_insensitive,
            } => self.eval_like(
                inner,
                pattern,
                escape.as_deref(),
                *negated,
                *case_insensitive,
            ),
            BoundExprKind::InList {
                expr: inner,
                list,
                eq_op,
                negated,
            } => self.eval_in_list(inner, list, eq_op, *negated),
            BoundExprKind::SessionValue(kind) => Ok(session_value(self.ctx.session, *kind)),
        }
    }

    fn eval_case(
        &self,
        arms: &[(BoundExpr, BoundExpr)],
        else_result: Option<&BoundExpr>,
    ) -> Result<Datum> {
        for (cond, result) in arms {
            if to_bool(&self.eval(cond)?)? == Some(true) {
                return self.eval(result);
            }
        }
        match else_result {
            Some(e) => self.eval(e),
            None => Ok(Datum::Null),
        }
    }

    fn eval_coalesce(&self, args: &[BoundExpr]) -> Result<Datum> {
        for a in args {
            let d = self.eval(a)?;
            if !d.is_null() {
                return Ok(d);
            }
        }
        Ok(Datum::Null)
    }

    fn eval_nullif(
        &self,
        left: &BoundExpr,
        right: &BoundExpr,
        eq_op: &BuiltinOperator,
    ) -> Result<Datum> {
        let l = self.eval(left)?;
        let r = self.eval(right)?;
        if l.is_null() || r.is_null() {
            return Ok(l);
        }
        if call_eq(eq_op, &l, &r)? == Some(true) {
            Ok(Datum::Null)
        } else {
            Ok(l)
        }
    }

    fn eval_like(
        &self,
        inner: &BoundExpr,
        pattern: &BoundExpr,
        escape: Option<&BoundExpr>,
        negated: bool,
        case_insensitive: bool,
    ) -> Result<Datum> {
        let s = self.eval(inner)?;
        let p = self.eval(pattern)?;
        let e = match escape {
            Some(e) => Some(self.eval(e)?),
            None => None,
        };
        if s.is_null() || p.is_null() || e.as_ref().is_some_and(Datum::is_null) {
            return Ok(Datum::Null);
        }
        let esc = match &e {
            Some(e) => like_escape_char(text_of(e)?)?,
            None => Some('\\'),
        };
        let m = like_match(text_of(&s)?, text_of(&p)?, esc, case_insensitive)?;
        Ok(Datum::Bool(m != negated))
    }

    fn eval_in_list(
        &self,
        inner: &BoundExpr,
        list: &[BoundExpr],
        eq_op: &BuiltinOperator,
        negated: bool,
    ) -> Result<Datum> {
        // Like PostgreSQL's ScalarArrayOpExpr, every element is evaluated
        // before comparing.
        let x = self.eval(inner)?;
        let items = list
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<Vec<_>>>()?;
        if x.is_null() {
            return Ok(Datum::Null);
        }
        let mut saw_null = false;
        for item in &items {
            if item.is_null() {
                saw_null = true;
                continue;
            }
            match call_eq(eq_op, &x, item)? {
                Some(true) => return Ok(Datum::Bool(!negated)),
                Some(false) => {}
                None => saw_null = true,
            }
        }
        Ok(if saw_null {
            Datum::Null
        } else {
            Datum::Bool(negated)
        })
    }

    /// Evaluates all arguments; `None` if any is NULL (strict call).
    fn eval_strict_args(&self, args: &[BoundExpr]) -> Result<Option<Vec<Datum>>> {
        let mut vals = Vec::with_capacity(args.len());
        let mut any_null = false;
        for a in args {
            let d = self.eval(a)?;
            any_null |= d.is_null();
            vals.push(d);
        }
        Ok((!any_null).then_some(vals))
    }

    fn eval_and(&self, args: &[BoundExpr]) -> Result<Datum> {
        let mut saw_null = false;
        for a in args {
            match to_bool(&self.eval(a)?)? {
                Some(false) => return Ok(Datum::Bool(false)),
                Some(true) => {}
                None => saw_null = true,
            }
        }
        Ok(if saw_null {
            Datum::Null
        } else {
            Datum::Bool(true)
        })
    }

    fn eval_or(&self, args: &[BoundExpr]) -> Result<Datum> {
        let mut saw_null = false;
        for a in args {
            match to_bool(&self.eval(a)?)? {
                Some(true) => return Ok(Datum::Bool(true)),
                Some(false) => {}
                None => saw_null = true,
            }
        }
        Ok(if saw_null {
            Datum::Null
        } else {
            Datum::Bool(false)
        })
    }
}

fn bool_test(v: Option<bool>, test: BoolTestKind) -> bool {
    match test {
        BoolTestKind::IsTrue => v == Some(true),
        BoolTestKind::IsNotTrue => v != Some(true),
        BoolTestKind::IsFalse => v == Some(false),
        BoolTestKind::IsNotFalse => v != Some(false),
        BoolTestKind::IsUnknown => v.is_none(),
        BoolTestKind::IsNotUnknown => v.is_some(),
    }
}

fn session_value(s: &SessionInfo, kind: SessionValueKind) -> Datum {
    match kind {
        SessionValueKind::CurrentUser | SessionValueKind::User | SessionValueKind::CurrentRole => {
            Datum::Text(s.current_user.clone())
        }
        SessionValueKind::SessionUser => Datum::Text(s.session_user.clone()),
        SessionValueKind::CurrentCatalog => Datum::Text(s.database.clone()),
        SessionValueKind::CurrentSchema => {
            s.current_schema.clone().map_or(Datum::Null, Datum::Text)
        }
    }
}

/// Calls an equality operator on two non-NULL values.
fn call_eq(op: &BuiltinOperator, l: &Datum, r: &Datum) -> Result<Option<bool>> {
    to_bool(&(op.func)(&[l.clone(), r.clone()])?)
}

fn text_of(d: &Datum) -> Result<&str> {
    d.as_str()
        .ok_or_else(|| Error::internal(format!("expected a text value, got {d:?}")))
}

/// Converts `d` (of type `from`) to type `to` using `method`. NULL stays
/// NULL.
pub fn apply_cast(d: Datum, from: SqlType, to: SqlType, method: CastMethod) -> Result<Datum> {
    if d.is_null() {
        return Ok(Datum::Null);
    }
    match method {
        CastMethod::Binary => Ok(d),
        CastMethod::Function(f) => f(&[d]),
        CastMethod::InOut => {
            let s = io::output_text(&d, from).unwrap_or_default();
            io::input_text(&s, SqlType::of(to.oid))
        }
    }
}

/// Applies `ty.typmod` (the `varchar(n)` length) to a value.
pub fn coerce_typmod(d: Datum, ty: SqlType, explicit: bool) -> Result<Datum> {
    match d {
        Datum::Text(s) => Ok(Datum::Text(ops::varchar_coerce(s, ty.typmod, explicit)?)),
        other => Ok(other),
    }
}

/// Validates a LIKE `ESCAPE` string: empty = no escape character, one
/// character = that character.
pub fn like_escape_char(e: &str) -> Result<Option<char>> {
    let mut it = e.chars();
    match (it.next(), it.next()) {
        (None, _) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(
            Error::new(sqlstate::INVALID_ESCAPE_SEQUENCE, "invalid escape string")
                .with_hint("Escape string must be empty or one character."),
        ),
    }
}

/// `LIKE` matching (PostgreSQL's `MatchText`): `%` matches any sequence,
/// `_` any single character, `esc` quotes the next character. With
/// `case_insensitive`, ASCII letters are folded (C locale `ILIKE`).
pub fn like_match(s: &str, p: &str, esc: Option<char>, case_insensitive: bool) -> Result<bool> {
    let fold = |x: &str| -> Vec<char> {
        if case_insensitive {
            x.chars().map(|c| c.to_ascii_lowercase()).collect()
        } else {
            x.chars().collect()
        }
    };
    let t = fold(s);
    let pat = fold(p);
    let esc = if case_insensitive {
        esc.map(|c| c.to_ascii_lowercase())
    } else {
        esc
    };
    Ok(match_text(&t, &pat, esc)? == LikeResult::True)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LikeResult {
    True,
    False,
    /// No match possible for any suffix (stops the `%` search early).
    Abort,
}

fn escape_at_end() -> Error {
    Error::new(
        sqlstate::INVALID_ESCAPE_SEQUENCE,
        "LIKE pattern must not end with escape character",
    )
}

fn match_text(t: &[char], p: &[char], esc: Option<char>) -> Result<LikeResult> {
    let (mut ti, mut pi) = (0usize, 0usize);
    while ti < t.len() && pi < p.len() {
        let pc = p[pi];
        if Some(pc) == esc {
            pi += 1;
            if pi >= p.len() {
                return Err(escape_at_end());
            }
            if p[pi] != t[ti] {
                return Ok(LikeResult::False);
            }
        } else if pc == '%' {
            pi += 1;
            while pi < p.len() {
                if p[pi] == '%' {
                    pi += 1;
                } else if p[pi] == '_' && Some('_') != esc {
                    if ti >= t.len() {
                        return Ok(LikeResult::Abort);
                    }
                    ti += 1;
                    pi += 1;
                } else {
                    break;
                }
            }
            if pi >= p.len() {
                return Ok(LikeResult::True);
            }
            let first = if Some(p[pi]) == esc {
                if pi + 1 >= p.len() {
                    return Err(escape_at_end());
                }
                p[pi + 1]
            } else {
                p[pi]
            };
            let first_is_wild = Some(p[pi]) != esc && p[pi] == '_';
            while ti < t.len() {
                if first_is_wild || t[ti] == first {
                    let r = match_text(&t[ti..], &p[pi..], esc)?;
                    if r != LikeResult::False {
                        return Ok(r);
                    }
                }
                ti += 1;
            }
            return Ok(LikeResult::Abort);
        } else if pc == '_' {
            // matches any single character
        } else if pc != t[ti] {
            return Ok(LikeResult::False);
        }
        ti += 1;
        pi += 1;
    }
    if ti < t.len() {
        return Ok(LikeResult::False);
    }
    while pi < p.len() && p[pi] == '%' && Some('%') != esc {
        pi += 1;
    }
    Ok(if pi >= p.len() {
        LikeResult::True
    } else {
        LikeResult::Abort
    })
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::{BuiltinFunction, BuiltinOperator};
    use crate::error::Span;
    use crate::types::{cmp_datum, oid};

    pub(crate) fn session() -> SessionInfo {
        SessionInfo {
            current_user: "alice".into(),
            session_user: "bob".into(),
            database: "postgres".into(),
            current_schema: Some("public".into()),
        }
    }

    /// An `EvalCtx` over `s` and an empty catalog (leaked: test only).
    pub(crate) fn ectx(s: &SessionInfo) -> EvalCtx<'_> {
        let catalog: &'static crate::catalog::fake::FakeCatalog =
            Box::leak(Box::new(crate::catalog::fake::FakeCatalog::new("postgres")));
        EvalCtx {
            session: s,
            catalog,
            runtime: &crate::executor::NullRuntime,
        }
    }

    pub(crate) fn lit(d: Datum, ty: SqlType) -> BoundExpr {
        BoundExpr::new(BoundExprKind::Literal(d), ty, Span::default())
    }
    pub(crate) fn int(v: i32) -> BoundExpr {
        lit(Datum::Int4(v), SqlType::INT4)
    }
    pub(crate) fn text(s: &str) -> BoundExpr {
        lit(Datum::Text(s.into()), SqlType::TEXT)
    }
    pub(crate) fn null(ty: SqlType) -> BoundExpr {
        lit(Datum::Null, ty)
    }
    pub(crate) fn boolean(b: Option<bool>) -> BoundExpr {
        lit(b.map_or(Datum::Null, Datum::Bool), SqlType::BOOL)
    }
    pub(crate) fn col(index: usize, ty: SqlType) -> BoundExpr {
        BoundExpr::new(BoundExprKind::ColumnRef { index }, ty, Span::default())
    }

    fn int4eq(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(
            cmp_datum(&a[0], &a[1]) == std::cmp::Ordering::Equal,
        ))
    }
    fn int4gt(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(
            cmp_datum(&a[0], &a[1]) == std::cmp::Ordering::Greater,
        ))
    }
    fn int4div(a: &[Datum]) -> Result<Datum> {
        let (Datum::Int4(x), Datum::Int4(y)) = (&a[0], &a[1]) else {
            return Err(Error::internal("bad args"));
        };
        if *y == 0 {
            return Err(Error::new(sqlstate::DIVISION_BY_ZERO, "division by zero"));
        }
        Ok(Datum::Int4(x / y))
    }
    fn textlen(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Int4(
            i32::try_from(a[0].as_str().unwrap().chars().count()).unwrap(),
        ))
    }
    fn int4_to_int8(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Int8(a[0].as_i64().unwrap()))
    }

    pub(crate) static EQ: BuiltinOperator = BuiltinOperator {
        oid: 96,
        name: "=",
        left: Some(oid::INT4),
        right: oid::INT4,
        result: oid::BOOL,
        func: int4eq,
    };
    pub(crate) static GT: BuiltinOperator = BuiltinOperator {
        oid: 521,
        name: ">",
        left: Some(oid::INT4),
        right: oid::INT4,
        result: oid::BOOL,
        func: int4gt,
    };
    static DIV: BuiltinOperator = BuiltinOperator {
        oid: 528,
        name: "/",
        left: Some(oid::INT4),
        right: oid::INT4,
        result: oid::INT4,
        func: int4div,
    };
    static LENGTH: BuiltinFunction = BuiltinFunction {
        oid: 1257,
        name: "length",
        args: &[oid::TEXT],
        result: oid::INT4,
        strict: true,
        kind: FnKind::Pure(textlen),
    };

    pub(crate) fn op(o: &'static BuiltinOperator, l: BoundExpr, r: BoundExpr) -> BoundExpr {
        let ty = SqlType::of(o.result);
        BoundExpr::new(
            BoundExprKind::Operator {
                op: o,
                args: vec![l, r],
            },
            ty,
            Span::default(),
        )
    }
    fn div(l: BoundExpr, r: BoundExpr) -> BoundExpr {
        op(&DIV, l, r)
    }
    fn mk(kind: BoundExprKind, ty: SqlType) -> BoundExpr {
        BoundExpr::new(kind, ty, Span::default())
    }

    fn ev(e: &BoundExpr) -> Result<Datum> {
        eval_expr(e, &vec![], &ectx(&session()))
    }

    #[test]
    fn literals_columns_operators() {
        assert_eq!(ev(&int(3)).unwrap(), Datum::Int4(3));
        let row = vec![Datum::Int4(10), Datum::Text("x".into())];
        assert_eq!(
            eval_expr(&col(1, SqlType::TEXT), &row, &ectx(&session())).unwrap(),
            Datum::Text("x".into())
        );
        assert!(eval_expr(&col(5, SqlType::TEXT), &row, &ectx(&session())).is_err());
        assert_eq!(ev(&div(int(7), int(2))).unwrap(), Datum::Int4(3));
        // Strict: NULL argument -> NULL without calling (no division error).
        assert_eq!(ev(&div(null(SqlType::INT4), int(0))).unwrap(), Datum::Null);
        let e = ev(&div(int(1), int(0))).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DIVISION_BY_ZERO);
        let f = mk(
            BoundExprKind::Function {
                func: &LENGTH,
                args: vec![text("héllo")],
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&f).unwrap(), Datum::Int4(5));
        let f = mk(
            BoundExprKind::Function {
                func: &LENGTH,
                args: vec![null(SqlType::TEXT)],
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&f).unwrap(), Datum::Null);
    }

    #[test]
    fn three_valued_logic() {
        let t = || boolean(Some(true));
        let f = || boolean(Some(false));
        let n = || boolean(None);
        let and = |a, b| mk(BoundExprKind::And(vec![a, b]), SqlType::BOOL);
        let or = |a, b| mk(BoundExprKind::Or(vec![a, b]), SqlType::BOOL);
        assert_eq!(ev(&and(t(), n())).unwrap(), Datum::Null);
        assert_eq!(ev(&and(f(), n())).unwrap(), Datum::Bool(false));
        assert_eq!(ev(&and(n(), f())).unwrap(), Datum::Bool(false));
        assert_eq!(ev(&and(t(), t())).unwrap(), Datum::Bool(true));
        assert_eq!(ev(&or(f(), n())).unwrap(), Datum::Null);
        assert_eq!(ev(&or(n(), t())).unwrap(), Datum::Bool(true));
        assert_eq!(ev(&or(f(), f())).unwrap(), Datum::Bool(false));
        // Short circuit: false AND (1/0 = 1) is false.
        let boom = op(&EQ, div(int(1), int(0)), int(1));
        assert_eq!(ev(&and(f(), boom.clone())).unwrap(), Datum::Bool(false));
        assert_eq!(ev(&or(t(), boom)).unwrap(), Datum::Bool(true));
        let not = |a| mk(BoundExprKind::Not(Box::new(a)), SqlType::BOOL);
        assert_eq!(ev(&not(n())).unwrap(), Datum::Null);
        assert_eq!(ev(&not(t())).unwrap(), Datum::Bool(false));
    }

    #[test]
    fn null_tests_and_bool_tests() {
        let isnull = mk(
            BoundExprKind::IsNull(Box::new(null(SqlType::INT4))),
            SqlType::BOOL,
        );
        assert_eq!(ev(&isnull).unwrap(), Datum::Bool(true));
        let notnull = mk(BoundExprKind::IsNotNull(Box::new(int(1))), SqlType::BOOL);
        assert_eq!(ev(&notnull).unwrap(), Datum::Bool(true));
        let cases = [
            (None, BoolTestKind::IsTrue, false),
            (None, BoolTestKind::IsNotTrue, true),
            (None, BoolTestKind::IsFalse, false),
            (None, BoolTestKind::IsNotFalse, true),
            (None, BoolTestKind::IsUnknown, true),
            (Some(true), BoolTestKind::IsNotUnknown, true),
            (Some(false), BoolTestKind::IsFalse, true),
            (Some(true), BoolTestKind::IsNotTrue, false),
        ];
        for (v, test, want) in cases {
            let e = mk(
                BoundExprKind::BoolTest {
                    expr: Box::new(boolean(v)),
                    test,
                },
                SqlType::BOOL,
            );
            assert_eq!(ev(&e).unwrap(), Datum::Bool(want), "{v:?} {test:?}");
        }
    }

    #[test]
    fn case_and_coalesce_are_lazy() {
        let c = mk(
            BoundExprKind::Coalesce(vec![null(SqlType::INT4), int(1), div(int(1), int(0))]),
            SqlType::INT4,
        );
        assert_eq!(ev(&c).unwrap(), Datum::Int4(1));
        let c = mk(
            BoundExprKind::Coalesce(vec![null(SqlType::INT4), null(SqlType::INT4)]),
            SqlType::INT4,
        );
        assert_eq!(ev(&c).unwrap(), Datum::Null);
        let case = mk(
            BoundExprKind::Case {
                arms: vec![
                    (boolean(None), div(int(1), int(0))),
                    (boolean(Some(true)), int(2)),
                    (boolean(Some(true)), div(int(1), int(0))),
                ],
                else_result: None,
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&case).unwrap(), Datum::Int4(2));
        let case = mk(
            BoundExprKind::Case {
                arms: vec![(boolean(Some(false)), int(1))],
                else_result: None,
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&case).unwrap(), Datum::Null);
        let case = mk(
            BoundExprKind::Case {
                arms: vec![(boolean(Some(false)), int(1))],
                else_result: Some(Box::new(int(9))),
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&case).unwrap(), Datum::Int4(9));
    }

    #[test]
    fn nullif() {
        let n = |l, r| {
            mk(
                BoundExprKind::NullIf {
                    left: Box::new(l),
                    right: Box::new(r),
                    eq_op: &EQ,
                },
                SqlType::INT4,
            )
        };
        assert_eq!(ev(&n(int(1), int(1))).unwrap(), Datum::Null);
        assert_eq!(ev(&n(int(1), int(2))).unwrap(), Datum::Int4(1));
        assert_eq!(ev(&n(int(1), null(SqlType::INT4))).unwrap(), Datum::Int4(1));
        assert_eq!(ev(&n(null(SqlType::INT4), int(1))).unwrap(), Datum::Null);
    }

    #[test]
    fn in_list_null_semantics() {
        let inl = |x, list: Vec<BoundExpr>, negated| {
            mk(
                BoundExprKind::InList {
                    expr: Box::new(x),
                    list,
                    eq_op: &EQ,
                    negated,
                },
                SqlType::BOOL,
            )
        };
        let n = || null(SqlType::INT4);
        assert_eq!(
            ev(&inl(int(1), vec![int(2), int(1)], false)).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            ev(&inl(int(1), vec![int(2), int(3)], false)).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(
            ev(&inl(int(1), vec![int(2), n()], false)).unwrap(),
            Datum::Null
        );
        assert_eq!(
            ev(&inl(int(1), vec![n(), int(1)], false)).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(ev(&inl(n(), vec![int(1)], false)).unwrap(), Datum::Null);
        assert_eq!(
            ev(&inl(int(1), vec![int(2), int(3)], true)).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            ev(&inl(int(1), vec![int(1), n()], true)).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(
            ev(&inl(int(1), vec![int(2), n()], true)).unwrap(),
            Datum::Null
        );
    }

    #[test]
    fn like_matching() {
        let m = |s: &str, p: &str| like_match(s, p, Some('\\'), false).unwrap();
        assert!(m("abc", "abc"));
        assert!(m("abc", "a%"));
        assert!(m("abc", "%c"));
        assert!(m("abc", "%b%"));
        assert!(m("abc", "a_c"));
        assert!(!m("abc", "a_"));
        assert!(m("abc", "___"));
        assert!(!m("abc", "____"));
        assert!(m("", "%"));
        assert!(!m("", "_"));
        assert!(m("a%c", "a\\%c"));
        assert!(!m("abc", "a\\%c"));
        assert!(m("a_c", "a\\_c"));
        assert!(m("héllo", "h_llo"));
        assert!(m("abcabc", "%abc"));
        assert!(m("aaab", "%a%b"));
        assert!(!m("ABC", "abc"));
        assert!(m("x%_y", "%\\%\\_%"));
        assert!(m("ab", "a%%%b"));
        assert!(like_match("ABC", "a%c", Some('\\'), true).unwrap());
        // Custom escape and no escape.
        assert!(like_match("a%", "a#%", Some('#'), false).unwrap());
        assert!(like_match("a\\b", "a\\b", None, false).unwrap());
        // Escape at end of pattern.
        // Not reached when the text runs out first (PostgreSQL: false).
        assert!(!like_match("a", "a\\", Some('\\'), false).unwrap());
        let e = like_match("ab", "a\\", Some('\\'), false).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_ESCAPE_SEQUENCE);
        assert_eq!(e.message, "LIKE pattern must not end with escape character");
        assert!(like_match("ab", "%\\", Some('\\'), false).is_err());
        assert_eq!(like_escape_char("").unwrap(), None);
        assert_eq!(like_escape_char("#").unwrap(), Some('#'));
        assert_eq!(
            like_escape_char("ab").unwrap_err().sqlstate,
            sqlstate::INVALID_ESCAPE_SEQUENCE
        );
    }

    #[test]
    fn like_node() {
        let like = |s, p, negated| {
            mk(
                BoundExprKind::Like {
                    expr: Box::new(s),
                    pattern: Box::new(p),
                    escape: None,
                    negated,
                    case_insensitive: false,
                },
                SqlType::BOOL,
            )
        };
        assert_eq!(
            ev(&like(text("abc"), text("a%"), false)).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            ev(&like(text("abc"), text("a%"), true)).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(
            ev(&like(null(SqlType::TEXT), text("a%"), false)).unwrap(),
            Datum::Null
        );
    }

    fn ctx_fn(
        args: &[Datum],
        cat: &dyn crate::catalog::CatalogReader,
        s: &SessionInfo,
    ) -> Result<Datum> {
        Ok(Datum::Text(format!(
            "{}:{}:{}",
            cat.current_database(),
            s.current_user,
            args[0].as_i64().unwrap()
        )))
    }
    static CTX_FN: BuiltinFunction = BuiltinFunction {
        oid: 1,
        name: "ctx_fn",
        args: &[oid::INT4],
        result: oid::TEXT,
        strict: true,
        kind: FnKind::Context(ctx_fn),
    };

    #[test]
    fn context_functions_see_catalog_and_session() {
        let f = |arg| {
            mk(
                BoundExprKind::Function {
                    func: &CTX_FN,
                    args: vec![arg],
                },
                SqlType::TEXT,
            )
        };
        assert_eq!(
            ev(&f(int(7))).unwrap(),
            Datum::Text("postgres:alice:7".into())
        );
        // Strict: NULL in, NULL out without calling.
        assert_eq!(ev(&f(null(SqlType::INT4))).unwrap(), Datum::Null);
    }

    #[test]
    fn regproc_output_uses_function_names() {
        // 0 is "-", an unknown OID stays numeric, an overloaded name is
        // schema-qualified, a unique name is bare.
        assert_eq!(regproc_name(1397).as_deref(), Some("pg_catalog.abs"));
        assert_eq!(regproc_name(89).as_deref(), Some("version"));
        assert_eq!(regproc_name(4_000_000_000), None);
        let types = [
            SqlType::of(oid::REGPROC),
            SqlType::of(oid::REGPROC),
            SqlType::of(oid::REGPROC),
            SqlType::of(oid::OID),
            SqlType::INT4,
        ];
        let row = vec![
            Datum::Oid(0),
            Datum::Oid(89),
            Datum::Oid(4_000_000_000),
            Datum::Oid(89),
            Datum::Null,
        ];
        assert_eq!(
            row_to_text(&row, &types, &OutputOpts::default()),
            vec![
                Some("-".into()),
                Some("version".into()),
                Some("4000000000".into()),
                Some("89".into()),
                None
            ]
        );
    }
    #[test]
    fn casts_typmod_and_session_values() {
        let c = mk(
            BoundExprKind::Cast {
                expr: Box::new(int(5)),
                method: CastMethod::Function(int4_to_int8),
            },
            SqlType::INT8,
        );
        assert_eq!(ev(&c).unwrap(), Datum::Int8(5));
        let c = mk(
            BoundExprKind::Cast {
                expr: Box::new(boolean(Some(true))),
                method: CastMethod::InOut,
            },
            SqlType::TEXT,
        );
        assert_eq!(ev(&c).unwrap(), Datum::Text("t".into()));
        let c = mk(
            BoundExprKind::Cast {
                expr: Box::new(text("12x")),
                method: CastMethod::InOut,
            },
            SqlType::INT4,
        );
        assert_eq!(
            ev(&c).unwrap_err().sqlstate,
            sqlstate::INVALID_TEXT_REPRESENTATION
        );
        let c = mk(
            BoundExprKind::Cast {
                expr: Box::new(null(SqlType::TEXT)),
                method: CastMethod::InOut,
            },
            SqlType::INT4,
        );
        assert_eq!(ev(&c).unwrap(), Datum::Null);
        let t = |explicit| {
            mk(
                BoundExprKind::CoerceTypmod {
                    expr: Box::new(text("abcd")),
                    explicit,
                },
                SqlType::varchar(3),
            )
        };
        assert_eq!(ev(&t(true)).unwrap(), Datum::Text("abc".into()));
        assert_eq!(
            ev(&t(false)).unwrap_err().sqlstate,
            sqlstate::STRING_DATA_RIGHT_TRUNCATION
        );
        let sv = |k| mk(BoundExprKind::SessionValue(k), SqlType::NAME);
        assert_eq!(
            ev(&sv(SessionValueKind::CurrentUser)).unwrap(),
            Datum::Text("alice".into())
        );
        assert_eq!(
            ev(&sv(SessionValueKind::SessionUser)).unwrap(),
            Datum::Text("bob".into())
        );
        assert_eq!(
            ev(&sv(SessionValueKind::CurrentCatalog)).unwrap(),
            Datum::Text("postgres".into())
        );
        let mut s = session();
        s.current_schema = None;
        assert_eq!(
            eval_expr(&sv(SessionValueKind::CurrentSchema), &vec![], &ectx(&s)).unwrap(),
            Datum::Null
        );
        assert!(eval_bool(&int(1), &vec![], &ectx(&session())).is_err());
        assert_eq!(
            eval_bool(&op(&GT, int(2), int(1)), &vec![], &ectx(&session())).unwrap(),
            Some(true)
        );
    }
}
