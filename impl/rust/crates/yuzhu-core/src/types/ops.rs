//! Implementations of built-in functions, operators and casts.
//!
//! Every built-in has the `BuiltinFn` signature and is referenced from the
//! static tables in `catalog::builtin`. Strictness (NULL in, NULL out) is
//! handled by the executor, so implementations never see `Datum::Null` in
//! a strict call.
//!
//! Integer implementations accept any integer width (`Datum::as_i64`) and
//! produce the result type named in the function, so one body serves the
//! cross-width operators too (`int24pl` and `int4pl` both use [`int4pl`]).
//! Likewise float8 bodies accept float4 arguments (`float48pl` = [`float8pl`]).

use std::cmp::Ordering;

use super::{Datum, VARHDRSZ, cmp_datum, io};
use crate::error::{Error, Result, SqlState, sqlstate};

/// Signature of every built-in function / operator / cast implementation.
pub type BuiltinFn = fn(&[Datum]) -> Result<Datum>;

/// Applies a `varchar(n)` typmod to a string (PostgreSQL's `varchar()`
/// length-coercion function).
///
/// - `typmod < VARHDRSZ` (no limit) or short enough: returned unchanged.
/// - explicit cast (`'abcdef'::varchar(3)`): silently truncated.
/// - otherwise (assignment): truncated if all excess characters are spaces,
///   else `22001 value too long for type character varying(n)`.
///
/// Lengths are counted in characters.
pub fn varchar_coerce(s: String, typmod: i32, is_explicit: bool) -> Result<String> {
    if typmod < VARHDRSZ {
        return Ok(s);
    }
    let max = usize::try_from(typmod - VARHDRSZ).unwrap_or(0);
    let Some((cut, _)) = s.char_indices().nth(max) else {
        return Ok(s);
    };
    if !is_explicit && !s[cut..].chars().all(|c| c == ' ') {
        return Err(Error::new(
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            format!("value too long for type character varying({max})"),
        ));
    }
    let mut s = s;
    s.truncate(cut);
    Ok(s)
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

fn int_arg(args: &[Datum], i: usize) -> Result<i64> {
    args.get(i)
        .and_then(Datum::as_i64)
        .ok_or_else(|| bad_arg("integer function"))
}

#[allow(clippy::cast_precision_loss)]
fn float_arg(args: &[Datum], i: usize) -> Result<f64> {
    match args.get(i) {
        Some(Datum::Float4(v)) => Ok(f64::from(*v)),
        Some(Datum::Float8(v)) => Ok(*v),
        Some(d) => d
            .as_i64()
            .map(|v| v as f64)
            .ok_or_else(|| bad_arg("float function")),
        None => Err(bad_arg("float function")),
    }
}

fn f32_arg(args: &[Datum], i: usize) -> Result<f32> {
    match args.get(i) {
        Some(Datum::Float4(v)) => Ok(*v),
        _ => Err(bad_arg("float4 function")),
    }
}

fn text_arg(args: &[Datum], i: usize) -> Result<&str> {
    args.get(i)
        .and_then(Datum::as_str)
        .ok_or_else(|| bad_arg("text function"))
}

fn bool_arg(args: &[Datum], i: usize) -> Result<bool> {
    args.get(i)
        .and_then(Datum::as_bool)
        .ok_or_else(|| bad_arg("bool function"))
}

// ---------------------------------------------------------------------------
// Integers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IntTy {
    I2,
    I4,
    I8,
}

fn int_out_of_range(t: IntTy) -> Error {
    let name = match t {
        IntTy::I2 => "smallint",
        IntTy::I4 => "integer",
        IntTy::I8 => "bigint",
    };
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("{name} out of range"),
    )
}

fn division_by_zero() -> Error {
    Error::new(sqlstate::DIVISION_BY_ZERO, "division by zero")
}

fn make_int(v: Option<i64>, t: IntTy) -> Result<Datum> {
    let v = v.ok_or_else(|| int_out_of_range(t))?;
    match t {
        IntTy::I2 => i16::try_from(v)
            .map(Datum::Int2)
            .map_err(|_| int_out_of_range(t)),
        IntTy::I4 => i32::try_from(v)
            .map(Datum::Int4)
            .map_err(|_| int_out_of_range(t)),
        IntTy::I8 => Ok(Datum::Int8(v)),
    }
}

fn int_binop(args: &[Datum], t: IntTy, f: fn(i64, i64) -> Option<i64>) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    make_int(f(a, b), t)
}

fn int_div(args: &[Datum], t: IntTy) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    if b == 0 {
        return Err(division_by_zero());
    }
    // i64::MIN / -1 overflows (None); narrower MIN / -1 fails the narrowing.
    make_int(a.checked_div(b), t)
}

fn int_mod(args: &[Datum], t: IntTy) -> Result<Datum> {
    let a = int_arg(args, 0)?;
    let b = int_arg(args, 1)?;
    if b == 0 {
        return Err(division_by_zero());
    }
    // INT_MIN % -1 is 0 in PostgreSQL (no error).
    make_int(Some(a.checked_rem(b).unwrap_or(0)), t)
}

macro_rules! int_ops {
    ($t:expr, $pl:ident, $mi:ident, $mul:ident, $div:ident, $md:ident, $um:ident, $abs:ident) => {
        pub fn $pl(args: &[Datum]) -> Result<Datum> {
            int_binop(args, $t, i64::checked_add)
        }
        pub fn $mi(args: &[Datum]) -> Result<Datum> {
            int_binop(args, $t, i64::checked_sub)
        }
        pub fn $mul(args: &[Datum]) -> Result<Datum> {
            int_binop(args, $t, i64::checked_mul)
        }
        pub fn $div(args: &[Datum]) -> Result<Datum> {
            int_div(args, $t)
        }
        pub fn $md(args: &[Datum]) -> Result<Datum> {
            int_mod(args, $t)
        }
        pub fn $um(args: &[Datum]) -> Result<Datum> {
            make_int(int_arg(args, 0)?.checked_neg(), $t)
        }
        pub fn $abs(args: &[Datum]) -> Result<Datum> {
            make_int(int_arg(args, 0)?.checked_abs(), $t)
        }
    };
}

int_ops!(
    IntTy::I2,
    int2pl,
    int2mi,
    int2mul,
    int2div,
    int2mod,
    int2um,
    int2abs
);
int_ops!(
    IntTy::I4,
    int4pl,
    int4mi,
    int4mul,
    int4div,
    int4mod,
    int4um,
    int4abs
);
int_ops!(
    IntTy::I8,
    int8pl,
    int8mi,
    int8mul,
    int8div,
    int8mod,
    int8um,
    int8abs
);

/// Prefix `+` (and any identity function).
pub fn identity(args: &[Datum]) -> Result<Datum> {
    args.first().cloned().ok_or_else(|| bad_arg("identity"))
}

// ---------------------------------------------------------------------------
// Comparison (any pair of same-category values; uses `cmp_datum`, which
// compares mixed integer widths and float widths numerically and gives
// PostgreSQL's NaN semantics)
// ---------------------------------------------------------------------------

fn compare(args: &[Datum], pred: fn(Ordering) -> bool) -> Result<Datum> {
    match args {
        [a, b] => Ok(Datum::Bool(pred(cmp_datum(a, b)))),
        _ => Err(bad_arg("comparison")),
    }
}

pub fn cmp_eq(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_eq)
}
pub fn cmp_ne(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_ne)
}
pub fn cmp_lt(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_lt)
}
pub fn cmp_le(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_le)
}
pub fn cmp_gt(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_gt)
}
pub fn cmp_ge(args: &[Datum]) -> Result<Datum> {
    compare(args, Ordering::is_ge)
}

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

fn float_overflow() -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}

fn float_underflow() -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: underflow",
    )
}

/// PostgreSQL's `check_float8_val`.
fn check_f64(v: f64, inf_is_valid: bool, zero_is_valid: bool) -> Result<f64> {
    if v.is_infinite() && !inf_is_valid {
        return Err(float_overflow());
    }
    if v == 0.0 && !zero_is_valid {
        return Err(float_underflow());
    }
    Ok(v)
}

fn check_f32(v: f32, inf_is_valid: bool, zero_is_valid: bool) -> Result<f32> {
    if v.is_infinite() && !inf_is_valid {
        return Err(float_overflow());
    }
    if v == 0.0 && !zero_is_valid {
        return Err(float_underflow());
    }
    Ok(v)
}

pub fn float8pl(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (float_arg(args, 0)?, float_arg(args, 1)?);
    check_f64(a + b, a.is_infinite() || b.is_infinite(), true).map(Datum::Float8)
}
pub fn float8mi(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (float_arg(args, 0)?, float_arg(args, 1)?);
    check_f64(a - b, a.is_infinite() || b.is_infinite(), true).map(Datum::Float8)
}
pub fn float8mul(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (float_arg(args, 0)?, float_arg(args, 1)?);
    check_f64(
        a * b,
        a.is_infinite() || b.is_infinite(),
        a == 0.0 || b == 0.0,
    )
    .map(Datum::Float8)
}
pub fn float8div(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (float_arg(args, 0)?, float_arg(args, 1)?);
    if b == 0.0 && !a.is_nan() {
        return Err(division_by_zero());
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(float_overflow());
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(float_underflow());
    }
    Ok(Datum::Float8(r))
}
pub fn float4pl(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (f32_arg(args, 0)?, f32_arg(args, 1)?);
    check_f32(a + b, a.is_infinite() || b.is_infinite(), true).map(Datum::Float4)
}
pub fn float4mi(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (f32_arg(args, 0)?, f32_arg(args, 1)?);
    check_f32(a - b, a.is_infinite() || b.is_infinite(), true).map(Datum::Float4)
}
pub fn float4mul(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (f32_arg(args, 0)?, f32_arg(args, 1)?);
    check_f32(
        a * b,
        a.is_infinite() || b.is_infinite(),
        a == 0.0 || b == 0.0,
    )
    .map(Datum::Float4)
}
pub fn float4div(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (f32_arg(args, 0)?, f32_arg(args, 1)?);
    if b == 0.0 && !a.is_nan() {
        return Err(division_by_zero());
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(float_overflow());
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(float_underflow());
    }
    Ok(Datum::Float4(r))
}
/// `dpow` (`float8 ^ float8`).
#[allow(clippy::float_cmp)]
pub fn float8pow(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (float_arg(args, 0)?, float_arg(args, 1)?);
    if a.is_nan() {
        return Ok(Datum::Float8(if b == 0.0 { 1.0 } else { f64::NAN }));
    }
    if b.is_nan() {
        return Ok(Datum::Float8(if a == 1.0 { 1.0 } else { f64::NAN }));
    }
    let invalid_power = |m: &str| Error::new(SqlState("2201F"), m.to_owned());
    if a == 0.0 && b < 0.0 {
        return Err(invalid_power(
            "zero raised to a negative power is undefined",
        ));
    }
    if a < 0.0 && b.is_finite() && b.fract() != 0.0 {
        return Err(invalid_power(
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    let r = a.powf(b);
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(float_overflow());
    }
    if r == 0.0 && a != 0.0 && a.is_finite() && b.is_finite() {
        return Err(float_underflow());
    }
    Ok(Datum::Float8(r))
}

pub fn float8um(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(-float_arg(args, 0)?))
}
pub fn float4um(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float4(-f32_arg(args, 0)?))
}
pub fn float8abs(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float_arg(args, 0)?.abs()))
}
pub fn float4abs(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float4(f32_arg(args, 0)?.abs()))
}

// ---------------------------------------------------------------------------
// Casts between numeric types
// ---------------------------------------------------------------------------

/// Integer or float (rounded half to even, like `rint()`) to an integer of
/// type `t`.
#[allow(clippy::cast_precision_loss)]
fn to_int(args: &[Datum], t: IntTy) -> Result<Datum> {
    match args.first() {
        Some(Datum::Float4(_) | Datum::Float8(_)) => {
            let f = float_arg(args, 0)?.round_ties_even();
            let (lo, hi) = match t {
                IntTy::I2 => (f64::from(i16::MIN), -f64::from(i16::MIN)),
                IntTy::I4 => (f64::from(i32::MIN), -f64::from(i32::MIN)),
                IntTy::I8 => (i64::MIN as f64, -(i64::MIN as f64)),
            };
            if f.is_nan() || f < lo || f >= hi {
                return Err(int_out_of_range(t));
            }
            #[allow(clippy::cast_possible_truncation)]
            make_int(Some(f as i64), t)
        }
        _ => make_int(Some(int_arg(args, 0)?), t),
    }
}

pub fn to_int2(args: &[Datum]) -> Result<Datum> {
    to_int(args, IntTy::I2)
}
pub fn to_int4(args: &[Datum]) -> Result<Datum> {
    to_int(args, IntTy::I4)
}
pub fn to_int8(args: &[Datum]) -> Result<Datum> {
    to_int(args, IntTy::I8)
}

pub fn to_float8(args: &[Datum]) -> Result<Datum> {
    float_arg(args, 0).map(Datum::Float8)
}

/// Integer or float8 to float4 (`dtof` checks overflow / underflow).
pub fn to_float4(args: &[Datum]) -> Result<Datum> {
    match args.first() {
        Some(Datum::Float8(v)) => {
            #[allow(clippy::cast_possible_truncation)]
            let r = *v as f32;
            if r.is_infinite() && !v.is_infinite() {
                return Err(float_overflow());
            }
            if r == 0.0 && *v != 0.0 {
                return Err(float_underflow());
            }
            Ok(Datum::Float4(r))
        }
        Some(Datum::Float4(v)) => Ok(Datum::Float4(*v)),
        _ => {
            #[allow(clippy::cast_precision_loss)]
            let r = int_arg(args, 0)? as f32;
            Ok(Datum::Float4(r))
        }
    }
}

/// `bool(int4)`.
pub fn int4_to_bool(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(int_arg(args, 0)? != 0))
}

/// `int4(bool)`.
pub fn bool_to_int4(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(i32::from(bool_arg(args, 0)?)))
}

/// `text(bool)`: `true` / `false` (unlike the output function's `t`/`f`).
pub fn bool_to_text(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(
        if bool_arg(args, 0)? { "true" } else { "false" }.to_owned(),
    ))
}

/// `name(text)`: truncates to 63 bytes.
pub fn text_to_name(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(
        io::truncate_identifier(text_arg(args, 0)?).to_owned(),
    ))
}

/// Text-to-text conversions that keep the value (`text(name)`, ...).
pub fn text_identity(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(text_arg(args, 0)?.to_owned()))
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// `textcat` (`text || text`). Also the body of `anytextcat` /
/// `textanycat`: the analyzer converts the non-text side to text first
/// (they are SQL functions `$1::text || $2` in PostgreSQL).
pub fn textcat(args: &[Datum]) -> Result<Datum> {
    let a = text_arg(args, 0)?;
    let b = text_arg(args, 1)?;
    let mut s = String::with_capacity(a.len() + b.len());
    s.push_str(a);
    s.push_str(b);
    Ok(Datum::Text(s))
}

/// `length(text)`: number of characters.
pub fn textlen(args: &[Datum]) -> Result<Datum> {
    let n = text_arg(args, 0)?.chars().count();
    Ok(Datum::Int4(i32::try_from(n).unwrap_or(i32::MAX)))
}

/// `lower(text)` in the C locale (ASCII only).
pub fn lower(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(text_arg(args, 0)?.to_ascii_lowercase()))
}

/// `upper(text)` in the C locale (ASCII only).
pub fn upper(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(text_arg(args, 0)?.to_ascii_uppercase()))
}

/// The string returned by `version()`.
pub fn version_string() -> String {
    format!(
        "PostgreSQL 16.0 (yuzhu {}) on {}-pc-linux-gnu, compiled by rustc, 64-bit",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    )
}

/// `version()`.
pub fn version(_args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Text(version_string()))
}

/// Matches `s` against a LIKE pattern (`%` any string, `_` one character)
/// with the default escape character (backslash). See
/// [`like_match_escape`] for other escapes. Case-insensitive matching
/// (ILIKE) folds ASCII only (C locale).
pub fn like_match(s: &str, pattern: &str, case_insensitive: bool) -> Result<bool> {
    like_match_escape(s, pattern, Some('\\'), case_insensitive)
}

/// LIKE with an explicit escape character (`None` = no escape character,
/// i.e. `ESCAPE ''`).
pub fn like_match_escape(
    s: &str,
    pattern: &str,
    escape: Option<char>,
    case_insensitive: bool,
) -> Result<bool> {
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum P {
        Lit(char),
        One,
        Any,
    }
    let fold = |c: char| {
        if case_insensitive {
            c.to_ascii_lowercase()
        } else {
            c
        }
    };
    let mut pat = Vec::new();
    let mut it = pattern.chars();
    while let Some(c) = it.next() {
        if Some(c) == escape {
            match it.next() {
                Some(n) => pat.push(P::Lit(fold(n))),
                None => {
                    return Err(Error::new(
                        sqlstate::INVALID_ESCAPE_SEQUENCE,
                        "LIKE pattern must not end with escape character",
                    ));
                }
            }
        } else if c == '%' {
            if pat.last() != Some(&P::Any) {
                pat.push(P::Any);
            }
        } else if c == '_' {
            pat.push(P::One);
        } else {
            pat.push(P::Lit(fold(c)));
        }
    }
    let text: Vec<char> = s.chars().map(fold).collect();
    // Iterative wildcard matching with backtracking to the last `%`.
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        match pat.get(pi) {
            Some(P::Lit(c)) if *c == text[ti] => {
                ti += 1;
                pi += 1;
            }
            Some(P::One) => {
                ti += 1;
                pi += 1;
            }
            Some(P::Any) => {
                star = Some((pi, ti));
                pi += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return Ok(false),
            },
        }
    }
    while pat.get(pi) == Some(&P::Any) {
        pi += 1;
    }
    Ok(pi == pat.len())
}

/// Validates a LIKE `ESCAPE` string: empty (no escape) or one character.
pub fn like_escape_char(esc: &str) -> Result<Option<char>> {
    let mut it = esc.chars();
    match (it.next(), it.next()) {
        (None, _) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(Error::new(SqlState("22019"), "invalid escape string")
            .with_hint("Escape string must be empty or one character.")),
    }
}

/// `textlike` (`~~`). The optional third argument is the escape string.
pub fn textlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, false).map(Datum::Bool)
}
/// `textnlike` (`!~~`).
pub fn textnlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, false).map(|b| Datum::Bool(!b))
}
/// `texticlike` (`~~*`).
pub fn texticlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, true).map(Datum::Bool)
}
/// `texticnlike` (`!~~*`).
pub fn texticnlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, true).map(|b| Datum::Bool(!b))
}

fn like_impl(args: &[Datum], ci: bool) -> Result<bool> {
    let s = text_arg(args, 0)?;
    let p = text_arg(args, 1)?;
    let esc = match args.get(2) {
        Some(Datum::Text(e)) => like_escape_char(e)?,
        _ => Some('\\'),
    };
    like_match_escape(s, p, esc, ci)
}

/// Placeholder body for catalog entries of types yuzhu does not implement
/// yet (numeric, date, interval). They exist so that operator resolution
/// sees the same candidates as PostgreSQL; the analyzer rejects values of
/// those types before any of these could run.
pub fn unsupported(_args: &[Datum]) -> Result<Datum> {
    Err(Error::not_supported(
        "this function is not supported yet".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Datum::*;

    #[test]
    fn varchar_typmod() {
        let tm = 3 + VARHDRSZ;
        assert_eq!(varchar_coerce("ab".into(), tm, false).unwrap(), "ab");
        assert_eq!(varchar_coerce("abc".into(), tm, false).unwrap(), "abc");
        assert_eq!(varchar_coerce("abc  ".into(), tm, false).unwrap(), "abc");
        assert_eq!(varchar_coerce("abcdef".into(), tm, true).unwrap(), "abc");
        assert_eq!(
            varchar_coerce("あいうえ".into(), tm, true).unwrap(),
            "あいう"
        );
        let e = varchar_coerce("abcd".into(), tm, false).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::STRING_DATA_RIGHT_TRUNCATION);
        assert_eq!(e.message, "value too long for type character varying(3)");
        assert_eq!(
            varchar_coerce("abcdef".into(), -1, false).unwrap(),
            "abcdef"
        );
    }

    fn code(r: Result<Datum>) -> &'static str {
        r.unwrap_err().sqlstate.code()
    }

    #[test]
    fn integer_arithmetic() {
        assert_eq!(int4pl(&[Int4(2), Int4(3)]).unwrap(), Int4(5));
        assert_eq!(code(int4pl(&[Int4(i32::MAX), Int4(1)])), "22003");
        assert_eq!(
            int4pl(&[Int4(i32::MAX), Int4(1)]).unwrap_err().message,
            "integer out of range"
        );
        assert_eq!(code(int2mul(&[Int2(200), Int2(200)])), "22003");
        assert_eq!(int4pl(&[Int2(32767), Int4(1)]).unwrap(), Int4(32768));
        assert_eq!(code(int8pl(&[Int8(i64::MAX), Int4(1)])), "22003");
        assert_eq!(int8mi(&[Int8(-i64::MAX), Int4(1)]).unwrap(), Int8(i64::MIN));
        assert_eq!(int4div(&[Int4(-7), Int4(2)]).unwrap(), Int4(-3));
        assert_eq!(int4mod(&[Int4(-7), Int4(2)]).unwrap(), Int4(-1));
        assert_eq!(code(int4div(&[Int4(1), Int4(0)])), "22012");
        assert_eq!(code(int4mod(&[Int4(1), Int4(0)])), "22012");
        assert_eq!(code(int4div(&[Int4(i32::MIN), Int4(-1)])), "22003");
        assert_eq!(code(int8div(&[Int8(i64::MIN), Int4(-1)])), "22003");
        assert_eq!(int4mod(&[Int4(i32::MIN), Int4(-1)]).unwrap(), Int4(0));
        assert_eq!(int8mod(&[Int8(i64::MIN), Int8(-1)]).unwrap(), Int8(0));
        assert_eq!(code(int4um(&[Int4(i32::MIN)])), "22003");
        assert_eq!(code(int4abs(&[Int4(i32::MIN)])), "22003");
        assert_eq!(code(int2abs(&[Int2(i16::MIN)])), "22003");
        assert_eq!(int8abs(&[Int8(-5)]).unwrap(), Int8(5));
        assert_eq!(identity(&[Int4(5)]).unwrap(), Int4(5));
    }

    #[test]
    fn float_arithmetic() {
        assert_eq!(
            float8pl(&[Float8(1.5), Float8(2.25)]).unwrap(),
            Float8(3.75)
        );
        assert_eq!(
            float8div(&[Float8(1.0), Float8(4.0)]).unwrap(),
            Float8(0.25)
        );
        assert_eq!(code(float8div(&[Float8(1.0), Float8(0.0)])), "22012");
        assert_eq!(code(float8mul(&[Float8(1e300), Float8(1e300)])), "22003");
        assert_eq!(code(float8mul(&[Float8(1e-300), Float8(1e-300)])), "22003");
        assert_eq!(
            float8pl(&[Float8(f64::INFINITY), Float8(1.0)]).unwrap(),
            Float8(f64::INFINITY)
        );
        assert!(matches!(
            float8div(&[Float8(f64::NAN), Float8(0.0)]).unwrap(),
            Float8(v) if v.is_nan()
        ));
        // float48: computed in float8
        assert_eq!(float8pl(&[Float4(0.5), Float8(1.0)]).unwrap(), Float8(1.5));
        assert_eq!(code(float4mul(&[Float4(1e30), Float4(1e30)])), "22003");
        assert_eq!(float4abs(&[Float4(-0.5)]).unwrap(), Float4(0.5));
        assert_eq!(float8um(&[Float8(2.5)]).unwrap(), Float8(-2.5));
        assert_eq!(
            float8pow(&[Float8(2.0), Float8(10.0)]).unwrap(),
            Float8(1024.0)
        );
        assert_eq!(code(float8pow(&[Float8(0.0), Float8(-1.0)])), "2201F");
        assert_eq!(code(float8pow(&[Float8(-8.0), Float8(0.5)])), "2201F");
        assert_eq!(code(float8pow(&[Float8(10.0), Float8(400.0)])), "22003");
    }

    #[test]
    fn comparisons() {
        assert_eq!(cmp_eq(&[Int2(1), Int8(1)]).unwrap(), Bool(true));
        assert_eq!(
            cmp_eq(&[Float8(f64::NAN), Float8(f64::NAN)]).unwrap(),
            Bool(true)
        );
        assert_eq!(
            cmp_gt(&[Float8(f64::NAN), Float8(f64::INFINITY)]).unwrap(),
            Bool(true)
        );
        assert_eq!(
            cmp_lt(&[Text("B".into()), Text("a".into())]).unwrap(),
            Bool(true)
        );
        assert_eq!(cmp_lt(&[Bool(false), Bool(true)]).unwrap(), Bool(true));
        assert_eq!(cmp_eq(&[Float4(0.1), Float8(0.1)]).unwrap(), Bool(false));
    }

    #[test]
    fn numeric_casts() {
        assert_eq!(to_int4(&[Float8(2.5)]).unwrap(), Int4(2));
        assert_eq!(to_int4(&[Float8(3.5)]).unwrap(), Int4(4));
        assert_eq!(to_int4(&[Float8(-2.5)]).unwrap(), Int4(-2));
        assert_eq!(to_int8(&[Float8(-3.5)]).unwrap(), Int8(-4));
        assert_eq!(to_int2(&[Float4(1.5)]).unwrap(), Int2(2));
        assert_eq!(code(to_int4(&[Float8(1e10)])), "22003");
        assert_eq!(code(to_int4(&[Float8(f64::NAN)])), "22003");
        assert_eq!(code(to_int8(&[Float8(f64::INFINITY)])), "22003");
        assert_eq!(code(to_int2(&[Float8(40000.0)])), "22003");
        assert_eq!(code(to_int4(&[Int8(3_000_000_000)])), "22003");
        assert_eq!(code(to_int2(&[Int4(40000)])), "22003");
        assert_eq!(to_int8(&[Int2(100)]).unwrap(), Int8(100));
        assert_eq!(
            to_int4(&[Float8(-2_147_483_648.4)]).unwrap(),
            Int4(i32::MIN)
        );
        assert_eq!(code(to_float4(&[Float8(1e300)])), "22003");
        assert_eq!(code(to_float4(&[Float8(1e-300)])), "22003");
        assert_eq!(
            to_float4(&[Int4(16_777_217)]).unwrap(),
            Float4(16_777_216.0)
        );
        assert_eq!(to_float8(&[Float4(1.5)]).unwrap(), Float8(1.5));
        assert_eq!(int4_to_bool(&[Int4(-1)]).unwrap(), Bool(true));
        assert_eq!(bool_to_int4(&[Bool(true)]).unwrap(), Int4(1));
        assert_eq!(bool_to_text(&[Bool(false)]).unwrap(), Text("false".into()));
    }

    #[test]
    fn strings() {
        assert_eq!(
            textcat(&[Text("a".into()), Text("b".into())]).unwrap(),
            Text("ab".into())
        );
        assert_eq!(textlen(&[Text("あいう".into())]).unwrap(), Int4(3));
        assert_eq!(upper(&[Text("aéb".into())]).unwrap(), Text("AéB".into()));
        assert_eq!(lower(&[Text("Ä".into())]).unwrap(), Text("Ä".into()));
        assert!(version_string().starts_with("PostgreSQL "));
    }

    #[test]
    fn like() {
        let m = |s: &str, p: &str| like_match(s, p, false).unwrap();
        assert!(m("abc", "abc"));
        assert!(m("abc", "a%"));
        assert!(m("abc", "%c"));
        assert!(m("abc", "%b%"));
        assert!(m("abc", "a_c"));
        assert!(!m("abc", "a_"));
        assert!(!m("abc", "____"));
        assert!(!m("abc", "ABC"));
        assert!(!m("abc", ""));
        assert!(m("", ""));
        assert!(m("", "%"));
        assert!(!m("", "_"));
        assert!(m("abc", "%%"));
        assert!(m("abc", "a%b%c"));
        assert!(m("abc", "%a%%c%"));
        assert!(m("a%c", "a\\%c"));
        assert!(!m("abc", "a\\%c"));
        assert!(m("a\\c", "a\\\\c"));
        assert!(m("あいう", "_い_"));
        assert!(m("aXbXc", "%X%c"));
        assert!(!m("a.c", "abc"));
        assert!(like_match("ABC", "a%", true).unwrap());
        assert_eq!(
            like_match("a", "a\\", false).unwrap_err().sqlstate.code(),
            "22025"
        );
        assert!(like_match_escape("a%", "a%", None, false).unwrap());
        assert!(like_match_escape("a%", "a#%", Some('#'), false).unwrap());
        assert_eq!(like_escape_char("ab").unwrap_err().sqlstate.code(), "22019");
        assert_eq!(
            textnlike(&[Text("abc".into()), Text("x%".into())]).unwrap(),
            Bool(true)
        );
    }
}
