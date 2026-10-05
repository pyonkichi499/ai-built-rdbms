//! Text input/output in PostgreSQL's text representation.
//!
//! Behaviour verified against PostgreSQL 17 (defaults: `extra_float_digits = 1`).

use super::{Datum, Oid, SqlType, float_fmt, oid, type_display_name};
use crate::error::{Error, Result, sqlstate};

/// Session-dependent options of text output (the GUCs PostgreSQL's output
/// functions read). Every place that turns a value into text — row output,
/// `::text` casts, `||`, the "Failing row contains" detail — must use the
/// same options, so they are carried by the executor's session info.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputOpts {
    /// `extra_float_digits` (-15..=3). `> 0` = shortest round-trip digits;
    /// `<= 0` = `%.*g` with `DBL_DIG`/`FLT_DIG + extra_float_digits` digits.
    pub extra_float_digits: i32,
}

impl Default for OutputOpts {
    /// PostgreSQL's defaults (`extra_float_digits = 1`).
    fn default() -> Self {
        OutputOpts {
            extra_float_digits: 1,
        }
    }
}

/// Converts a value to its text representation with the default output
/// options (`extra_float_digits = 1`). NULL gives `None`.
///
/// Code that runs inside a session should use [`output_text_with`] with the
/// session's options instead.
pub fn output_text(d: &Datum, ty: SqlType) -> Option<String> {
    output_text_with(d, ty, &OutputOpts::default())
}

/// Converts a value to its text representation honouring `opts`. NULL gives
/// `None`.
///
/// The output depends only on the datum variant; `ty` is accepted so that
/// future types (numeric scale, timestamps, ...) can use it.
pub fn output_text_with(d: &Datum, ty: SqlType, opts: &OutputOpts) -> Option<String> {
    let _ = ty;
    let efd = opts.extra_float_digits;
    Some(match d {
        Datum::Null => return None,
        Datum::Bool(b) => if *b { "t" } else { "f" }.to_owned(),
        Datum::Int2(v) => v.to_string(),
        Datum::Int4(v) => v.to_string(),
        Datum::Int8(v) => v.to_string(),
        Datum::Float4(v) => float4_out_with(*v, efd),
        Datum::Float8(v) => float8_out_with(*v, efd),
        Datum::Numeric(n) => n.to_string(),
        Datum::Text(s) => s.clone(),
        Datum::Oid(_)
        | Datum::Char(_)
        | Datum::Xid(_)
        | Datum::Cid(_)
        | Datum::Tid(_)
        | Datum::OidVector(_)
        | Datum::Void
        | Datum::Int4Array(_) => {
            return super::sys::output_text(d);
        }
    })
}

/// Like [`output_text_with`], but a `regproc` column is shown by function
/// name. `regproc_name` maps a function OID to its display name (`None` =
/// unknown, shown as a number); 0 is shown as `-` (PostgreSQL's `regprocout`).
/// The executor's output stage passes a lookup into `catalog::builtin`.
pub fn output_text_regproc(
    d: &Datum,
    ty: SqlType,
    opts: &OutputOpts,
    regproc_name: &dyn Fn(Oid) -> Option<String>,
) -> Option<String> {
    if ty.oid == oid::REGPROC
        && let Datum::Oid(v) = d
    {
        if *v == 0 {
            return Some("-".into());
        }
        if let Some(name) = regproc_name(*v) {
            return Some(name);
        }
    }
    output_text_with(d, ty, opts)
}

/// `float8out` for any `extra_float_digits`.
pub fn float8_out_with(v: f64, extra_float_digits: i32) -> String {
    if extra_float_digits > 0 {
        float8_out(v)
    } else {
        // DBL_DIG = 15
        float_fmt::format_g(v, 15 + extra_float_digits)
    }
}

/// `float4out` for any `extra_float_digits`.
pub fn float4_out_with(v: f32, extra_float_digits: i32) -> String {
    if extra_float_digits > 0 {
        float4_out(v)
    } else {
        // FLT_DIG = 6
        float_fmt::format_g(f64::from(v), 6 + extra_float_digits)
    }
}

/// `float8out` with `extra_float_digits > 0`: shortest round-trip digits.
/// Fixed notation when the decimal exponent is in `-4..15`, otherwise
/// `d.ddde+XX` (as in PostgreSQL's `d2s.c`).
pub fn float8_out(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let (digits, exp) = float_fmt::f64_digits(v);
    format_shortest(v < 0.0, &digits, exp, 15)
}

/// `float4out` with `extra_float_digits > 0`. Fixed notation when the
/// decimal exponent is in `-4..6` (PostgreSQL's `f2s.c`).
pub fn float4_out(v: f32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let (digits, exp) = float_fmt::f32_digits(v);
    format_shortest(v < 0.0, &digits, exp, 6)
}

/// Lays out shortest digits PostgreSQL-style (`to_chars` in d2s.c/f2s.c):
/// fixed notation when `-4 <= exp < fixed_upper`, else `d.ddde[+-]XX`.
fn format_shortest(neg: bool, digit_values: &[u8], exp: i32, fixed_upper: i32) -> String {
    let digits: String = digit_values.iter().map(|d| char::from(b'0' + d)).collect();
    let ndigits = i32::try_from(digits.len()).unwrap_or(i32::MAX);

    let mut out = String::with_capacity(digits.len() + 8);
    if neg {
        out.push('-');
    }
    if (-4..fixed_upper).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let int_len = exp + 1;
            if ndigits <= int_len {
                out.push_str(&digits);
                for _ in 0..(int_len - ndigits) {
                    out.push('0');
                }
            } else {
                let split = usize::try_from(int_len).unwrap_or(0);
                out.push_str(&digits[..split]);
                out.push('.');
                out.push_str(&digits[split..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let abs = exp.unsigned_abs();
        if abs < 10 {
            out.push('0');
        }
        out.push_str(&abs.to_string());
    }
    out
}

/// Parses the text representation of a value of type `ty`
/// (the type's input function, e.g. `'123'::int4`).
///
/// The typmod is ignored: like PostgreSQL's coercion of unknown literals,
/// length limits (`varchar(n)`) are applied afterwards by a separate typmod
/// coercion step (see `types::ops::varchar_coerce`). `name` input is
/// truncated to 63 bytes as PostgreSQL does.
pub fn input_text(s: &str, ty: SqlType) -> Result<Datum> {
    match ty.oid {
        oid::BOOL => bool_in(s).map(Datum::Bool),
        oid::INT2 => int_in(s, oid::INT2)
            .map(|v| Datum::Int2(i16::try_from(v).expect("range checked by int_in"))),
        oid::INT4 => int_in(s, oid::INT4)
            .map(|v| Datum::Int4(i32::try_from(v).expect("range checked by int_in"))),
        oid::INT8 => int_in(s, oid::INT8).map(Datum::Int8),
        oid::FLOAT4 => float4_in(s).map(Datum::Float4),
        oid::FLOAT8 => float8_in(s).map(Datum::Float8),
        oid::NUMERIC => Ok(Datum::Numeric(yuzhu_numeric::Numeric::parse(s)?)),
        oid::TEXT | oid::VARCHAR | oid::UNKNOWN => Ok(Datum::Text(s.to_owned())),
        oid::NAME => Ok(Datum::Text(truncate_identifier(s).to_owned())),
        t if super::sys::handles(t) => super::sys::input_text(s, ty),
        other => Err(Error::internal(format!(
            "no input function for type with OID {other}"
        ))),
    }
}

/// Truncates to `MAX_IDENTIFIER_LENGTH` bytes on a character boundary.
pub fn truncate_identifier(s: &str) -> &str {
    let max = super::MAX_IDENTIFIER_LENGTH;
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// C `isspace` in the "C" locale.
fn is_pg_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn invalid_syntax(type_oid: Oid, s: &str) -> Error {
    Error::new(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        format!(
            "invalid input syntax for type {}: \"{s}\"",
            type_display_name(type_oid)
        ),
    )
}

/// `boolin`: case-insensitive prefixes of true/false/yes/no, `on`/`off`
/// (at least two characters), `1`/`0`; surrounding whitespace is ignored.
pub fn bool_in(s: &str) -> Result<bool> {
    let v = s.trim_matches(is_pg_space).to_ascii_lowercase();
    let len = v.len();
    let prefix_of = |word: &str, min: usize| len >= min && word.starts_with(v.as_str());
    let r = match v.as_bytes().first() {
        Some(b't') if prefix_of("true", 1) => Some(true),
        Some(b'f') if prefix_of("false", 1) => Some(false),
        Some(b'y') if prefix_of("yes", 1) => Some(true),
        Some(b'n') if prefix_of("no", 1) => Some(false),
        Some(b'o') if prefix_of("on", 2) => Some(true),
        Some(b'o') if prefix_of("off", 2) => Some(false),
        Some(b'1') if len == 1 => Some(true),
        Some(b'0') if len == 1 => Some(false),
        _ => None,
    };
    r.ok_or_else(|| invalid_syntax(oid::BOOL, s))
}

/// `int2in` / `int4in` / `int8in` (PostgreSQL 16+ `pg_strtoint*_safe`):
/// surrounding whitespace, optional sign, `0x`/`0o`/`0b` prefixes and
/// single underscores between digits are accepted. Returns the value as
/// i64, already range-checked for `type_oid`.
pub fn int_in(s: &str, type_oid: Oid) -> Result<i64> {
    let (min, max, acc_limit): (i64, i64, u64) = match type_oid {
        oid::INT2 => (
            i64::from(i16::MIN),
            i64::from(i16::MAX),
            u64::from(u16::MAX),
        ),
        oid::INT4 => (
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            u64::from(u32::MAX),
        ),
        _ => (i64::MIN, i64::MAX, u64::MAX),
    };
    let out_of_range = || {
        Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            format!(
                "value \"{s}\" is out of range for type {}",
                type_display_name(type_oid)
            ),
        )
    };
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_pg_space(char::from(b[i])) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut radix = 10u32;
    if i + 1 < b.len() && b[i] == b'0' {
        match b[i + 1] {
            b'x' | b'X' => radix = 16,
            b'o' | b'O' => radix = 8,
            b'b' | b'B' => radix = 2,
            _ => {}
        }
        if radix != 10 {
            i += 2;
        }
    }
    let first_digit = i;
    let mut acc: u64 = 0;
    while i < b.len() {
        let c = char::from(b[i]);
        if let Some(d) = c.to_digit(radix) {
            acc = acc
                .checked_mul(u64::from(radix))
                .and_then(|a| a.checked_add(u64::from(d)))
                .filter(|a| *a <= acc_limit)
                .ok_or_else(out_of_range)?;
            i += 1;
        } else if c == '_' {
            // An underscore must be followed by a digit; in decimal it must
            // also follow one (a prefix like `0x_1` is fine).
            let next_ok = b.get(i + 1).is_some_and(|n| char::from(*n).is_digit(radix));
            if !next_ok || (radix == 10 && i == first_digit) {
                return Err(invalid_syntax(type_oid, s));
            }
            i += 1;
        } else {
            break;
        }
    }
    if i == first_digit {
        return Err(invalid_syntax(type_oid, s));
    }
    while i < b.len() && is_pg_space(char::from(b[i])) {
        i += 1;
    }
    if i != b.len() {
        return Err(invalid_syntax(type_oid, s));
    }
    let value = if neg {
        if acc > min.unsigned_abs() {
            return Err(out_of_range());
        }
        // acc <= |min| so the negation fits.
        0i64.checked_sub_unsigned(acc).ok_or_else(out_of_range)?
    } else {
        i64::try_from(acc).map_err(|_| out_of_range())?
    };
    if value < min || value > max {
        return Err(out_of_range());
    }
    Ok(value)
}

/// Splits off surrounding whitespace and checks the number contains a
/// non-zero mantissa digit (used to detect underflow to zero).
fn has_nonzero_mantissa(num: &str) -> bool {
    num.chars()
        .take_while(|c| *c != 'e' && *c != 'E')
        .any(|c| c.is_ascii_digit() && c != '0')
}

/// `float8in`. Accepts NaN / Infinity / inf (any case, optional sign) and
/// surrounding whitespace. Overflow and underflow to zero raise 22003.
/// Hexadecimal floats accepted by C `strtod` (`0x1.8p3`) are supported.
pub fn float8_in(s: &str) -> Result<f64> {
    let num = s.trim_matches(is_pg_space);
    if let Some(r) = parse_hex_float(num) {
        let (v, nonzero) = r.ok_or_else(|| invalid_syntax(oid::FLOAT8, s))?;
        check_float_range(v.is_infinite(), v == 0.0 && nonzero, num, oid::FLOAT8)?;
        return Ok(v);
    }
    let v: f64 = num.parse().map_err(|_| invalid_syntax(oid::FLOAT8, s))?;
    check_float_range(v.is_infinite(), v == 0.0, num, oid::FLOAT8)?;
    Ok(v)
}

/// `float4in`; see `float8_in`.
pub fn float4_in(s: &str) -> Result<f32> {
    let num = s.trim_matches(is_pg_space);
    if let Some(r) = parse_hex_float(num) {
        let (v, nonzero) = r.ok_or_else(|| invalid_syntax(oid::FLOAT4, s))?;
        #[allow(clippy::cast_possible_truncation)]
        let v = v as f32;
        check_float_range(v.is_infinite(), v == 0.0 && nonzero, num, oid::FLOAT4)?;
        return Ok(v);
    }
    let v: f32 = num.parse().map_err(|_| invalid_syntax(oid::FLOAT4, s))?;
    check_float_range(v.is_infinite(), v == 0.0, num, oid::FLOAT4)?;
    Ok(v)
}

/// Parses a C99 hexadecimal float (`[+-]0x<hex>[.<hex>][p[+-]<dec>]`).
/// `None` = no `0x` prefix (not a hex literal); `Some(None)` = malformed;
/// otherwise `(value, mantissa_is_nonzero)`.
#[allow(clippy::option_option)]
fn parse_hex_float(num: &str) -> Option<Option<(f64, bool)>> {
    let (neg, rest) = match num.as_bytes().first() {
        Some(b'-') => (true, &num[1..]),
        Some(b'+') => (false, &num[1..]),
        _ => (false, num),
    };
    let body = rest
        .strip_prefix("0x")
        .or_else(|| rest.strip_prefix("0X"))?;
    Some(parse_hex_body(body).map(|(v, nz)| (if neg { -v } else { v }, nz)))
}

fn parse_hex_body(body: &str) -> Option<(f64, bool)> {
    let b = body.as_bytes();
    let mut i = 0;
    let mut mant: u64 = 0;
    let mut exp: i64 = 0;
    let mut digits = 0usize;
    let mut nonzero = false;
    let mut seen_dot = false;
    while i < b.len() {
        let c = b[i];
        if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if let Some(d) = char::from(c).to_digit(16) {
            digits += 1;
            nonzero |= d != 0;
            if mant >> 60 == 0 {
                mant = (mant << 4) | u64::from(d);
                if seen_dot {
                    exp -= 4;
                }
            } else if !seen_dot {
                // Beyond u64 precision: drop the digit, scale up instead.
                exp += 4;
            }
        } else {
            break;
        }
        i += 1;
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'p' || b[i] == b'P') {
        i += 1;
        let eneg = match b.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let start = i;
        let mut e: i64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            e = (e * 10 + i64::from(b[i] - b'0')).min(100_000);
            i += 1;
        }
        if i == start {
            return None;
        }
        exp += if eneg { -e } else { e };
    }
    if i != b.len() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let mut v = mant as f64;
    let mut e = exp.clamp(-5000, 5000);
    while e != 0 && v != 0.0 && v.is_finite() {
        let step = e.clamp(-1000, 1000);
        v *= 2f64.powi(i32::try_from(step).ok()?);
        e -= step;
    }
    Some((v, nonzero))
}

fn check_float_range(is_inf: bool, is_zero: bool, num: &str, type_oid: Oid) -> Result<()> {
    let lower = num.to_ascii_lowercase();
    let explicit_inf = lower.contains("inf");
    if (is_inf && !explicit_inf) || (is_zero && has_nonzero_mantissa(num)) {
        return Err(Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            format!(
                "\"{num}\" is out of range for type {}",
                type_display_name(type_oid)
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::approx_constant)]
mod tests {
    use super::*;

    fn f8(v: f64) -> String {
        output_text(&Datum::Float8(v), SqlType::FLOAT8).unwrap()
    }
    fn f4(v: f32) -> String {
        output_text(&Datum::Float4(v), SqlType::FLOAT4).unwrap()
    }

    #[test]
    fn output_text_with_extra_float_digits() {
        let o = |efd| OutputOpts {
            extra_float_digits: efd,
        };
        let t =
            |v: f64, efd| output_text_with(&Datum::Float8(v), SqlType::FLOAT8, &o(efd)).unwrap();
        assert_eq!(t(0.1 + 0.2, 0), "0.3");
        assert_eq!(t(1.0 / 3.0, 0), "0.333333333333333");
        assert_eq!(t(0.1 + 0.2, 1), "0.30000000000000004");
        assert_eq!(t(0.1 + 0.2, 3), "0.30000000000000004");
        assert_eq!(t(3.25, -15), "3");
        assert_eq!(t(f64::from(0.1f32), 0), "0.100000001490116");
        assert_eq!(t(1e20, 0), "1e+20");
        assert_eq!(t(1e-5, 0), "1e-05");
        assert_eq!(t(123_456.0, 0), "123456");
        assert_eq!(t(-0.0, 0), "-0");
        assert_eq!(t(f64::INFINITY, 0), "Infinity");
        assert_eq!(t(99999.95, -10), "1e+05");
        let f4 =
            |v: f32, efd| output_text_with(&Datum::Float4(v), SqlType::FLOAT4, &o(efd)).unwrap();
        assert_eq!(f4(1.0 / 3.0, 0), "0.333333");
        assert_eq!(f4(1.0 / 3.0, 1), "0.33333334");
        assert_eq!(f4(1.0 / 3.0, -2), "0.3333");
        // Non-float values ignore the options; default = extra_float_digits 1.
        assert_eq!(
            output_text_with(&Datum::Int4(7), SqlType::INT4, &o(0)).unwrap(),
            "7"
        );
        assert_eq!(OutputOpts::default().extra_float_digits, 1);
        assert_eq!(
            output_text(&Datum::Float8(1.0 / 3.0), SqlType::FLOAT8).unwrap(),
            "0.3333333333333333"
        );
    }

    #[test]
    fn float8_output_matches_pg17() {
        // Expected values taken from PostgreSQL 17.
        let cases: &[(f64, &str)] = &[
            (1e20, "1e+20"),
            (1e15, "1e+15"),
            (1e14, "100000000000000"),
            (123_456_789_012_345.6, "123456789012345.6"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123_456_789_012_345_680.0, "1.2345678901234568e+17"),
            (1.5e300, "1.5e+300"),
            (0.1, "0.1"),
            (-0.0, "-0"),
            (0.0, "0"),
            (2.5e-310, "2.5e-310"),
            (1e16, "1e+16"),
            (0.000_123_456, "0.000123456"),
            (1.5e-7, "1.5e-07"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            // Exclusive rounding-interval bounds (Ryu with acceptBounds = false).
            (-68_659_681_752_636_816.0, "-6.8659681752636816e+16"),
            // Exact ties round to even.
            (181_705_388_398_854.62, "181705388398854.62"),
            (2_277_415_065_412.531_2, "2277415065412.5312"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (v, want) in cases {
            assert_eq!(f8(*v), *want, "float8 {v:e}");
        }
    }

    #[test]
    fn float4_output_matches_pg17() {
        let cases: &[(f32, &str)] = &[
            (1e6, "1e+06"),
            (1e7, "1e+07"),
            (123_456.0, "123456"),
            (1_234_567.0, "1.234567e+06"),
            (12_345_678.0, "1.2345678e+07"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (3.14159, "3.14159"),
            (1e20, "1e+20"),
            (1.1, "1.1"),
            (1e-45, "1e-45"),
            (-0.0, "-0"),
            (999_999.0, "999999"),
            (9_999_999.0, "9.999999e+06"),
            (100_000.0, "100000"),
            (-3_827_620.2, "-3.8276202e+06"),
            (121_636_864.0, "1.21636864e+08"),
            (-435_429.62, "-435429.62"),
            (f32::MAX, "3.4028235e+38"),
            (1e-45, "1e-45"),
        ];
        for (v, want) in cases {
            assert_eq!(f4(*v), *want, "float4 {v:e}");
        }
    }

    #[test]
    fn other_output() {
        assert_eq!(output_text(&Datum::Null, SqlType::INT4), None);
        assert_eq!(output_text(&Datum::Bool(true), SqlType::BOOL).unwrap(), "t");
        assert_eq!(
            output_text(&Datum::Bool(false), SqlType::BOOL).unwrap(),
            "f"
        );
        assert_eq!(output_text(&Datum::Int2(-3), SqlType::INT2).unwrap(), "-3");
        assert_eq!(
            output_text(&Datum::Int8(i64::MIN), SqlType::INT8).unwrap(),
            "-9223372036854775808"
        );
        assert_eq!(
            output_text(&Datum::Text(String::new()), SqlType::TEXT).unwrap(),
            ""
        );
    }

    fn int4(s: &str) -> Result<Datum> {
        input_text(s, SqlType::INT4)
    }

    #[test]
    fn int_input() {
        assert_eq!(int4(" 12 ").unwrap(), Datum::Int4(12));
        assert_eq!(int4("0x1F").unwrap(), Datum::Int4(31));
        assert_eq!(int4("1_000").unwrap(), Datum::Int4(1000));
        assert_eq!(int4("0o17").unwrap(), Datum::Int4(15));
        assert_eq!(int4("0x_1").unwrap(), Datum::Int4(1));
        assert_eq!(
            input_text("-0b101", SqlType::INT2).unwrap(),
            Datum::Int2(-5)
        );
        assert_eq!(input_text("+7", SqlType::INT8).unwrap(), Datum::Int8(7));
        assert_eq!(int4("-2147483648").unwrap(), Datum::Int4(i32::MIN));
        assert_eq!(int4("-0x80000000").unwrap(), Datum::Int4(i32::MIN));
        assert_eq!(
            input_text("-9223372036854775808", SqlType::INT8).unwrap(),
            Datum::Int8(i64::MIN)
        );
        for bad in [
            "abc", "1.5", "", "  ", "+", "1__0", "_1", "1_", "0x", "inf", "1 2",
        ] {
            let e = int4(bad).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION, "{bad:?}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type integer: \"{bad}\"")
            );
        }
        let e = int4("3000000000").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(
            e.message,
            "value \"3000000000\" is out of range for type integer"
        );
        assert_eq!(
            int4("2147483648").unwrap_err().sqlstate,
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE
        );
        let e = input_text("40000", SqlType::INT2).unwrap_err();
        assert_eq!(
            e.message,
            "value \"40000\" is out of range for type smallint"
        );
        let e = input_text("99999999999999999999", SqlType::INT8).unwrap_err();
        assert_eq!(
            e.message,
            "value \"99999999999999999999\" is out of range for type bigint"
        );
        assert_eq!(
            input_text("9223372036854775808", SqlType::INT8)
                .unwrap_err()
                .sqlstate,
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE
        );
    }

    #[test]
    fn float_input() {
        assert_eq!(float8_in(" 1.5 ").unwrap(), 1.5);
        assert_eq!(float8_in("infinity").unwrap(), f64::INFINITY);
        assert_eq!(float8_in("+inf").unwrap(), f64::INFINITY);
        assert_eq!(float4_in("-Infinity").unwrap(), f32::NEG_INFINITY);
        assert_eq!(float8_in(".5").unwrap(), 0.5);
        assert_eq!(float8_in("5.").unwrap(), 5.0);
        assert!(float8_in(" NaN ").unwrap().is_nan());
        assert_eq!(float8_in("1e-310").unwrap(), 1e-310);
        assert_eq!(float8_in("0").unwrap(), 0.0);
        assert_eq!(float8_in("-0.000").unwrap(), 0.0);
        assert_eq!(float4_in("1e-40").unwrap(), 1e-40);

        let e = float8_in("1e400").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(
            e.message,
            "\"1e400\" is out of range for type double precision"
        );
        let e = float8_in(" 1e-400").unwrap_err();
        assert_eq!(
            e.message,
            "\"1e-400\" is out of range for type double precision"
        );
        let e = float4_in("1e40").unwrap_err();
        assert_eq!(e.message, "\"1e40\" is out of range for type real");
        let e = float8_in("abc").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION);
        assert_eq!(
            e.message,
            "invalid input syntax for type double precision: \"abc\""
        );
        let e = float4_in("abc").unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type real: \"abc\"");
        assert!(float8_in("1_000.5").is_err());
        assert!(float8_in("").is_err());
    }

    #[test]
    fn bool_input() {
        for t in [" yes ", "Tr", "1", "true", "T", "on", "ON", "y"] {
            assert_eq!(
                input_text(t, SqlType::BOOL).unwrap(),
                Datum::Bool(true),
                "{t:?}"
            );
        }
        for f in ["of", "off", "n", "no", "0", "F", "false"] {
            assert_eq!(
                input_text(f, SqlType::BOOL).unwrap(),
                Datum::Bool(false),
                "{f:?}"
            );
        }
        for bad in ["o", "", "abc", "truex", "2", "yess"] {
            let e = input_text(bad, SqlType::BOOL).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION);
            assert_eq!(
                e.message,
                format!("invalid input syntax for type boolean: \"{bad}\"")
            );
        }
    }

    #[test]
    fn text_input() {
        assert_eq!(
            input_text(" a ", SqlType::varchar(1)).unwrap(),
            Datum::Text(" a ".into())
        );
        let long = "あ".repeat(30); // 90 bytes
        let Datum::Text(n) = input_text(&long, SqlType::NAME).unwrap() else {
            panic!()
        };
        assert_eq!(n.len(), 63);
    }
}
