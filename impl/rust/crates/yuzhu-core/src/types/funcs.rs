//! Built-in string, math and bitwise functions added after the first
//! milestones (`substr`, `left`, `replace`, `btrim`, `lpad`, `md5`, `round`,
//! `sqrt`, `&`, `|`, `~`, the regex match operators, ...).
//! Arguments follow the conventions of `types::ops` (strict calls only).

use std::fmt::Write as _;

use super::Datum;
use super::regex::regex_match as regex_match_cached;
use crate::error::{Error, Result, SqlState, sqlstate};

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

fn text(args: &[Datum], i: usize) -> Result<&str> {
    args.get(i)
        .and_then(Datum::as_str)
        .ok_or_else(|| bad_arg("text function"))
}

fn int(args: &[Datum], i: usize) -> Result<i64> {
    args.get(i)
        .and_then(Datum::as_i64)
        .ok_or_else(|| bad_arg("integer function"))
}

#[allow(clippy::cast_precision_loss)]
fn float(args: &[Datum], i: usize) -> Result<f64> {
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

#[allow(clippy::unnecessary_wraps)]
fn out(s: String) -> Result<Datum> {
    Ok(Datum::Text(s))
}

fn len_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

fn char_slice(s: &str, from: usize, to: usize) -> String {
    s.chars().skip(from).take(to.saturating_sub(from)).collect()
}

/// `substr(text, start)`.
pub fn substr_no_len(args: &[Datum]) -> Result<Datum> {
    let s = text(args, 0)?;
    let start = int(args, 1)?.max(1);
    let skip = usize::try_from(start - 1).unwrap_or(usize::MAX);
    out(s.chars().skip(skip).collect())
}

/// `substr(text, start, len)`.
pub fn substr(args: &[Datum]) -> Result<Datum> {
    let s = text(args, 0)?;
    let (start, len) = (int(args, 1)?, int(args, 2)?);
    if len < 0 {
        return Err(Error::new(
            SqlState("22011"),
            "negative substring length not allowed",
        ));
    }
    let end = start.saturating_add(len);
    let from = start.max(1);
    if end <= from {
        return out(String::new());
    }
    let (from, end) = (
        usize::try_from(from - 1).unwrap_or(usize::MAX),
        usize::try_from(end - 1).unwrap_or(usize::MAX),
    );
    out(char_slice(s, from, end))
}

/// `left(text, n)`.
pub fn left(args: &[Datum]) -> Result<Datum> {
    let s = text(args, 0)?;
    let n = int(args, 1)?;
    let count = s.chars().count();
    let keep = if n >= 0 {
        usize::try_from(n).unwrap_or(usize::MAX).min(count)
    } else {
        count.saturating_sub(usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX))
    };
    out(s.chars().take(keep).collect())
}

/// `right(text, n)`.
pub fn right(args: &[Datum]) -> Result<Datum> {
    let s = text(args, 0)?;
    let n = int(args, 1)?;
    let count = s.chars().count();
    let skip = if n >= 0 {
        count.saturating_sub(usize::try_from(n).unwrap_or(usize::MAX))
    } else if n == i64::from(i32::MIN) {
        // PG は `-n` が INT_MIN のまま負になり、全体を返す。
        0
    } else {
        usize::try_from(n.unsigned_abs())
            .unwrap_or(usize::MAX)
            .min(count)
    };
    out(s.chars().skip(skip).collect())
}

/// `replace(text, from, to)`.
pub fn replace(args: &[Datum]) -> Result<Datum> {
    let (s, from, to) = (text(args, 0)?, text(args, 1)?, text(args, 2)?);
    if from.is_empty() {
        return out(s.to_owned());
    }
    out(s.replace(from, to))
}

fn trim_impl(args: &[Datum], leading: bool, trailing: bool) -> Result<Datum> {
    let s = text(args, 0)?;
    let set: Vec<char> = match args.get(1) {
        Some(_) => text(args, 1)?.chars().collect(),
        None => vec![' '],
    };
    let in_set = |c: char| set.contains(&c);
    let mut r = s;
    if leading {
        r = r.trim_start_matches(in_set);
    }
    if trailing {
        r = r.trim_end_matches(in_set);
    }
    out(r.to_owned())
}

/// `btrim(text [, chars])`.
pub fn btrim(args: &[Datum]) -> Result<Datum> {
    trim_impl(args, true, true)
}
/// `ltrim(text [, chars])`.
pub fn ltrim(args: &[Datum]) -> Result<Datum> {
    trim_impl(args, true, false)
}
/// `rtrim(text [, chars])`.
pub fn rtrim(args: &[Datum]) -> Result<Datum> {
    trim_impl(args, false, true)
}

/// `position(needle in haystack)` / `strpos(haystack, needle)`.
pub fn strpos(args: &[Datum]) -> Result<Datum> {
    let (hay, needle) = (text(args, 0)?, text(args, 1)?);
    let pos = hay
        .find(needle)
        .map_or(0, |byte| hay[..byte].chars().count() + 1);
    Ok(Datum::Int4(len_i32(pos)))
}

fn pad(args: &[Datum], left_pad: bool) -> Result<Datum> {
    // PG: MaxAllocSize / 最大エンコード長（UTF-8 は 4 バイト）。
    const MAX_LEN: i64 = 0x3fff_ffff / 4;
    let s = text(args, 0)?;
    let want = int(args, 1)?.max(0);
    if want > MAX_LEN {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "requested length too large",
        ));
    }
    let fill: Vec<char> = match args.get(2) {
        Some(_) => text(args, 2)?.chars().collect(),
        None => vec![' '],
    };
    let chars: Vec<char> = s.chars().collect();
    let want = usize::try_from(want).unwrap_or(0);
    if fill.is_empty() || chars.len() >= want {
        return out(chars.iter().take(want).collect());
    }
    let padding: String = fill.iter().cycle().take(want - chars.len()).collect();
    let body: String = chars.iter().collect();
    out(if left_pad {
        padding + &body
    } else {
        body + &padding
    })
}

/// `lpad(text, len [, fill])`.
pub fn lpad(args: &[Datum]) -> Result<Datum> {
    pad(args, true)
}
/// `rpad(text, len [, fill])`.
pub fn rpad(args: &[Datum]) -> Result<Datum> {
    pad(args, false)
}

/// `char_length(text)` / `character_length(text)`.
pub fn char_length(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(len_i32(text(args, 0)?.chars().count())))
}

/// `reverse(text)`.
pub fn reverse(args: &[Datum]) -> Result<Datum> {
    out(text(args, 0)?.chars().rev().collect())
}

/// `initcap(text)`: first letter of each word upper case, the rest lower
/// case (words are runs of ASCII alphanumerics, as in the C locale).
pub fn initcap(args: &[Datum]) -> Result<Datum> {
    let mut start = true;
    let r = text(args, 0)?
        .chars()
        .map(|c| {
            let word = c.is_ascii_alphanumeric();
            let r = if !word {
                c
            } else if start {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            start = !word;
            r
        })
        .collect();
    out(r)
}

/// `ascii(text)`: code point of the first character (0 for empty).
pub fn ascii(args: &[Datum]) -> Result<Datum> {
    let cp = text(args, 0)?.chars().next().map_or(0, u32::from);
    Ok(Datum::Int4(i32::try_from(cp).unwrap_or(0)))
}

/// `chr(int4)`.
pub fn chr(args: &[Datum]) -> Result<Datum> {
    let n = int(args, 0)?;
    if n == 0 {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "null character not permitted",
        ));
    }
    if n < 0 {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "character number must be positive",
        ));
    }
    match u32::try_from(n).ok().and_then(char::from_u32) {
        Some(c) => out(c.to_string()),
        None => Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {n}"),
        )),
    }
}

/// `md5(text)`: lower-case hex digest.
pub fn md5(args: &[Datum]) -> Result<Datum> {
    out(md5_hex(text(args, 0)?.as_bytes()))
}

#[allow(clippy::many_single_char_names)]
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let k: Vec<u32> = (0..64)
        .map(|i| ((f64::from(i + 1).sin().abs()) * 4_294_967_296.0) as u32)
        .collect();
    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

// ---------------------------------------------------------------------------
// Regular expression match operators
// ---------------------------------------------------------------------------

fn regex_match(args: &[Datum], icase: bool) -> Result<bool> {
    regex_match_cached(text(args, 0)?, text(args, 1)?, icase)
}

/// `text ~ text`.
pub fn textregexeq(args: &[Datum]) -> Result<Datum> {
    regex_match(args, false).map(Datum::Bool)
}
/// `text !~ text`.
pub fn textregexne(args: &[Datum]) -> Result<Datum> {
    regex_match(args, false).map(|b| Datum::Bool(!b))
}
/// `text ~* text`.
pub fn texticregexeq(args: &[Datum]) -> Result<Datum> {
    regex_match(args, true).map(Datum::Bool)
}
/// `text !~* text`.
pub fn texticregexne(args: &[Datum]) -> Result<Datum> {
    regex_match(args, true).map(|b| Datum::Bool(!b))
}
/// `name ~ text`.
pub fn nameregexeq(args: &[Datum]) -> Result<Datum> {
    regex_match(args, false).map(Datum::Bool)
}
/// `name !~ text`.
pub fn nameregexne(args: &[Datum]) -> Result<Datum> {
    regex_match(args, false).map(|b| Datum::Bool(!b))
}
/// `name ~* text`.
pub fn nameicregexeq(args: &[Datum]) -> Result<Datum> {
    regex_match(args, true).map(Datum::Bool)
}
/// `name !~* text`.
pub fn nameicregexne(args: &[Datum]) -> Result<Datum> {
    regex_match(args, true).map(|b| Datum::Bool(!b))
}

// ---------------------------------------------------------------------------
// Math
// ---------------------------------------------------------------------------

fn float_out_of_range() -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}

fn checked(arg: f64, r: f64) -> Result<Datum> {
    if r.is_infinite() && !arg.is_infinite() {
        return Err(float_out_of_range());
    }
    Ok(Datum::Float8(r))
}

/// `round(float8)`: ties to even (`rint`).
pub fn dround(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.round_ties_even()))
}
/// `trunc(float8)`.
pub fn dtrunc(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.trunc()))
}
/// `ceil(float8)`.
pub fn dceil(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.ceil()))
}
/// `floor(float8)`.
pub fn dfloor(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.floor()))
}
/// `sign(float8)`.
pub fn dsign(args: &[Datum]) -> Result<Datum> {
    let v = float(args, 0)?;
    // PG は v > 0 → 1、v < 0 → -1、それ以外（±0・NaN）は 0。
    Ok(Datum::Float8(if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        0.0
    }))
}
/// `sqrt(float8)`.
pub fn dsqrt(args: &[Datum]) -> Result<Datum> {
    let v = float(args, 0)?;
    if v < 0.0 {
        return Err(Error::new(
            SqlState("2201F"),
            "cannot take square root of a negative number",
        ));
    }
    Ok(Datum::Float8(v.sqrt()))
}
/// `f64` を仮数部 `[0.5, 1)` と指数部に分ける（C の `frexp`。有限の非ゼロ値のみ）。
fn frexp(x: f64) -> (f64, i32) {
    let (x, bias) = if x.abs() < f64::MIN_POSITIVE {
        (x * 2f64.powi(54), -54)
    } else {
        (x, 0)
    };
    let raw = x.to_bits();
    let exp = ((raw >> 52) & 0x7ff) as i32 - 1022;
    let m = f64::from_bits((raw & !(0x7ffu64 << 52)) | (1022u64 << 52));
    (m, exp + bias)
}

/// glibc の `cbrt`（sysdeps/ieee754/dbl-64/s_cbrt.c）と同じ計算。
/// PostgreSQL は libm の `cbrt` を使うため、最終桁まで一致させる（`f64::cbrt` は 1ulp 異なることがある）。
#[allow(clippy::excessive_precision, clippy::cast_sign_loss)]
fn glibc_cbrt(x: f64) -> f64 {
    const CBRT2: f64 = 1.259_921_049_894_873_2;
    const SQR_CBRT2: f64 = 1.587_401_051_968_199_5;
    const FACTOR: [f64; 5] = [1.0 / SQR_CBRT2, 1.0 / CBRT2, 1.0, CBRT2, SQR_CBRT2];
    if x == 0.0 || !x.is_finite() {
        return x + x;
    }
    let (xm, xe) = frexp(x.abs());
    let u = 0.354_895_765_043_919_86
        + (1.508_191_937_815_848_96
            + (-2.114_994_941_673_712_87
                + (2.446_931_225_635_344_3
                    + (-1.834_692_774_836_130_86
                        + (0.784_932_344_976_639_262 - 0.145_263_899_385_486_377 * xm) * xm)
                        * xm)
                    * xm)
                * xm)
            * xm;
    let t2 = u * u * u;
    let ym = u * (t2 + 2.0 * xm) / (2.0 * t2 + xm) * FACTOR[(2 + xe % 3) as usize];
    let k = xe / 3;
    let scaled = ym * 2f64.powi(k / 2) * 2f64.powi(k - k / 2);
    if x > 0.0 { scaled } else { -scaled }
}

/// `cbrt(float8)`.
pub fn dcbrt(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(glibc_cbrt(float(args, 0)?)))
}
/// `exp(float8)`.
pub fn dexp(args: &[Datum]) -> Result<Datum> {
    let v = float(args, 0)?;
    let r = v.exp();
    if r == 0.0 && v.is_finite() {
        return Err(Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow",
        ));
    }
    checked(v, r)
}

fn log_arg(args: &[Datum]) -> Result<f64> {
    let v = float(args, 0)?;
    if v == 0.0 {
        return Err(Error::new(
            SqlState("2201E"),
            "cannot take logarithm of zero",
        ));
    }
    if v < 0.0 {
        return Err(Error::new(
            SqlState("2201E"),
            "cannot take logarithm of a negative number",
        ));
    }
    Ok(v)
}

/// `ln(float8)`.
pub fn dlog1(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(log_arg(args)?.ln()))
}
/// `log10(float8)` / `log(float8)`.
pub fn dlog10(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(log_arg(args)?.log10()))
}

// ---------------------------------------------------------------------------
// Bitwise operators on integers
// ---------------------------------------------------------------------------

macro_rules! bit_ops {
    ($ty:ty, $variant:ident, $and:ident, $or:ident, $xor:ident, $not:ident) => {
        #[allow(clippy::cast_possible_truncation)]
        pub fn $and(args: &[Datum]) -> Result<Datum> {
            Ok(Datum::$variant((int(args, 0)? & int(args, 1)?) as $ty))
        }
        #[allow(clippy::cast_possible_truncation)]
        pub fn $or(args: &[Datum]) -> Result<Datum> {
            Ok(Datum::$variant((int(args, 0)? | int(args, 1)?) as $ty))
        }
        #[allow(clippy::cast_possible_truncation)]
        pub fn $xor(args: &[Datum]) -> Result<Datum> {
            Ok(Datum::$variant((int(args, 0)? ^ int(args, 1)?) as $ty))
        }
        #[allow(clippy::cast_possible_truncation)]
        pub fn $not(args: &[Datum]) -> Result<Datum> {
            Ok(Datum::$variant(!(int(args, 0)? as $ty)))
        }
    };
}

bit_ops!(i16, Int2, int2and, int2or, int2xor, int2not);
bit_ops!(i32, Int4, int4and, int4or, int4xor, int4not);
bit_ops!(i64, Int8, int8and, int8or, int8xor, int8not);

// Shift counts wrap like the C `<<` / `>>` on x86 (the count is masked to the
// width of the promoted operand), which is what PostgreSQL shows.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int2shl(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int2(
        (int(args, 0)? as i32).wrapping_shl(int(args, 1)? as u32) as i16,
    ))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int2shr(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int2(
        (int(args, 0)? as i32).wrapping_shr(int(args, 1)? as u32) as i16,
    ))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int4shl(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(
        (int(args, 0)? as i32).wrapping_shl(int(args, 1)? as u32),
    ))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int4shr(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(
        (int(args, 0)? as i32).wrapping_shr(int(args, 1)? as u32),
    ))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int8shl(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int8(
        int(args, 0)?.wrapping_shl(int(args, 1)? as u32),
    ))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn int8shr(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int8(
        int(args, 0)?.wrapping_shr(int(args, 1)? as u32),
    ))
}

// ---------------------------------------------------------------------------
// split_part, translate, quote_*, concat, gcd/lcm, degrees/radians, ...
// ---------------------------------------------------------------------------

/// `split_part(text, delimiter, n)`; a negative `n` counts from the end.
pub fn split_part(args: &[Datum]) -> Result<Datum> {
    let (s, delim, n) = (text(args, 0)?, text(args, 1)?, int(args, 2)?);
    if n == 0 {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "field position must not be zero",
        ));
    }
    if s.is_empty() {
        return out(String::new());
    }
    if delim.is_empty() {
        return out(if n == 1 || n == -1 {
            s.to_owned()
        } else {
            String::new()
        });
    }
    let fields: Vec<&str> = s.split(delim).collect();
    let idx = if n > 0 {
        usize::try_from(n - 1).ok()
    } else {
        fields
            .len()
            .checked_sub(usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX))
    };
    out(idx
        .and_then(|i| fields.get(i))
        .map_or_else(String::new, |f| (*f).to_owned()))
}

/// `starts_with(text, prefix)`.
pub fn starts_with(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(text(args, 0)?.starts_with(text(args, 1)?)))
}

/// `translate(text, from, to)`: characters of `from` without a counterpart in
/// `to` are deleted; the first occurrence in `from` wins.
pub fn translate(args: &[Datum]) -> Result<Datum> {
    let from: Vec<char> = text(args, 1)?.chars().collect();
    let to: Vec<char> = text(args, 2)?.chars().collect();
    out(text(args, 0)?
        .chars()
        .filter_map(|c| match from.iter().position(|&f| f == c) {
            None => Some(c),
            Some(i) => to.get(i).copied(),
        })
        .collect())
}

/// `quote_ident(text)`: quoted unless it is a plain lower-case identifier
/// that is not a (non-unreserved) keyword.
pub fn quote_ident(args: &[Datum]) -> Result<Datum> {
    use crate::sql::token::{KeywordCategory, keyword_category};
    let s = text(args, 0)?;
    let mut chars = s.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
        && keyword_category(s) == KeywordCategory::Unreserved;
    if plain {
        out(s.to_owned())
    } else {
        out(format!("\"{}\"", s.replace('"', "\"\"")))
    }
}

/// `quote_literal(text)`: single quotes doubled; `E'...'` when it has a backslash.
pub fn quote_literal(args: &[Datum]) -> Result<Datum> {
    let s = text(args, 0)?;
    let body = s.replace('\'', "''");
    if s.contains('\\') {
        out(format!("E'{}'", body.replace('\\', "\\\\")))
    } else {
        out(format!("'{body}'"))
    }
}

/// `octet_length(text)`.
pub fn octet_length(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(len_i32(text(args, 0)?.len())))
}

/// `bit_length(text)`.
pub fn bit_length(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(
        i32::try_from(text(args, 0)?.len().saturating_mul(8)).map_err(|_| int4_range())?,
    ))
}

/// Text of one `concat` argument; `None` for NULL. The analyzer already cast
/// everything but `bool` to text; `bool` prints as `t` / `f` like its output function.
fn concat_piece(d: &Datum) -> Result<Option<&str>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(if *b { "t" } else { "f" })),
        Datum::Text(s) => Ok(Some(s)),
        _ => Err(bad_arg("concat")),
    }
}

/// `concat(VARIADIC "any")`: NULL arguments are ignored.
pub fn concat(args: &[Datum]) -> Result<Datum> {
    let mut r = String::new();
    for a in args {
        if let Some(s) = concat_piece(a)? {
            r.push_str(s);
        }
    }
    out(r)
}

/// `concat_ws(sep, VARIADIC "any")`: NULL separator gives NULL; NULL arguments are skipped.
pub fn concat_ws(args: &[Datum]) -> Result<Datum> {
    let Some(sep) = args.first().map(concat_piece).transpose()?.flatten() else {
        return Ok(Datum::Null);
    };
    let mut parts = Vec::new();
    for a in args.iter().skip(1) {
        if let Some(s) = concat_piece(a)? {
            parts.push(s);
        }
    }
    out(parts.join(sep))
}

fn int4_range() -> Error {
    Error::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

fn int8_range() -> Error {
    Error::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
}

fn gcd_u64(a: i64, b: i64) -> u64 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn lcm_u128(a: i64, b: i64) -> Option<u64> {
    if a == 0 || b == 0 {
        return Some(0);
    }
    let g = gcd_u64(a, b);
    u64::try_from(u128::from(a.unsigned_abs() / g) * u128::from(b.unsigned_abs())).ok()
}

/// `gcd(int4, int4)`.
pub fn int4gcd(args: &[Datum]) -> Result<Datum> {
    let g = gcd_u64(int(args, 0)?, int(args, 1)?);
    i32::try_from(g).map(Datum::Int4).map_err(|_| int4_range())
}
/// `gcd(int8, int8)`.
pub fn int8gcd(args: &[Datum]) -> Result<Datum> {
    let g = gcd_u64(int(args, 0)?, int(args, 1)?);
    i64::try_from(g).map(Datum::Int8).map_err(|_| int8_range())
}
/// `lcm(int4, int4)`.
pub fn int4lcm(args: &[Datum]) -> Result<Datum> {
    lcm_u128(int(args, 0)?, int(args, 1)?)
        .and_then(|l| i32::try_from(l).ok())
        .map(Datum::Int4)
        .ok_or_else(int4_range)
}
/// `lcm(int8, int8)`.
pub fn int8lcm(args: &[Datum]) -> Result<Datum> {
    lcm_u128(int(args, 0)?, int(args, 1)?)
        .and_then(|l| i64::try_from(l).ok())
        .map(Datum::Int8)
        .ok_or_else(int8_range)
}

fn trig_range_err() -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "input is out of range",
    )
}
/// 三角関数の共通部: NaN は NaN、±Infinity は範囲外エラー（PostgreSQL と同じ）。
fn trig(args: &[Datum], f: fn(f64) -> f64) -> Result<Datum> {
    let v = float(args, 0)?;
    if v.is_nan() {
        return Ok(Datum::Float8(f64::NAN));
    }
    if v.is_infinite() {
        return Err(trig_range_err());
    }
    Ok(Datum::Float8(f(v)))
}
/// `asin` / `acos` の共通部: 定義域 `[-1, 1]` の外はエラー。
fn inverse_trig(args: &[Datum], f: fn(f64) -> f64) -> Result<Datum> {
    let v = float(args, 0)?;
    if v.is_nan() {
        return Ok(Datum::Float8(f64::NAN));
    }
    if !(-1.0..=1.0).contains(&v) {
        return Err(trig_range_err());
    }
    Ok(Datum::Float8(f(v)))
}
/// `sin(float8)`.
pub fn dsin(args: &[Datum]) -> Result<Datum> {
    trig(args, f64::sin)
}
/// `cos(float8)`.
pub fn dcos(args: &[Datum]) -> Result<Datum> {
    trig(args, f64::cos)
}
/// `tan(float8)`.
pub fn dtan(args: &[Datum]) -> Result<Datum> {
    trig(args, f64::tan)
}
/// `asin(float8)`.
pub fn dasin(args: &[Datum]) -> Result<Datum> {
    inverse_trig(args, f64::asin)
}
/// `acos(float8)`.
pub fn dacos(args: &[Datum]) -> Result<Datum> {
    inverse_trig(args, f64::acos)
}
/// `atan(float8)`.
pub fn datan(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.atan()))
}
/// `atan2(float8, float8)`.
pub fn datan2(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.atan2(float(args, 1)?)))
}
/// `sinh(float8)`.
pub fn dsinh(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.sinh()))
}
/// `cosh(float8)`.
pub fn dcosh(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.cosh()))
}
/// `tanh(float8)`.
pub fn dtanh(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Float8(float(args, 0)?.tanh()))
}

/// `degrees(float8)`.
pub fn degrees(args: &[Datum]) -> Result<Datum> {
    // PG は RADIANS_PER_DEGREE での除算（乗算だと最終桁がずれる）。
    let a = float(args, 0)?;
    checked(a, a / 0.017_453_292_519_943_295)
}
/// `radians(float8)`.
pub fn radians(args: &[Datum]) -> Result<Datum> {
    let a = float(args, 0)?;
    let r = a * 0.017_453_292_519_943_295;
    if r == 0.0 && a != 0.0 {
        return Err(Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow",
        ));
    }
    Ok(Datum::Float8(r))
}

/// `div(numeric, numeric)`: truncated integer division.
pub fn numeric_div_trunc(args: &[Datum]) -> Result<Datum> {
    let (Some(Datum::Numeric(a)), Some(Datum::Numeric(b))) = (args.first(), args.get(1)) else {
        return Err(bad_arg("div"));
    };
    Ok(Datum::Numeric(a.checked_div(b)?.trunc(0)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Datum::*;

    fn t(s: &str) -> Datum {
        Text(s.to_owned())
    }

    #[test]
    fn string_functions() {
        assert_eq!(substr(&[t("abc"), Int4(1), Int4(2)]).unwrap(), t("ab"));
        assert_eq!(substr(&[t("abc"), Int4(-1), Int4(3)]).unwrap(), t("a"));
        assert_eq!(left(&[t("abc"), Int4(-1)]).unwrap(), t("ab"));
        assert_eq!(right(&[t("abc"), Int4(-1)]).unwrap(), t("bc"));
        assert_eq!(lpad(&[t("hi"), Int4(5), t("xy")]).unwrap(), t("xyxhi"));
        assert_eq!(btrim(&[t("  a ")]).unwrap(), t("a"));
        assert_eq!(strpos(&[t("abc"), t("c")]).unwrap(), Int4(3));
        assert_eq!(initcap(&[t("hELLO wORLD")]).unwrap(), t("Hello World"));
    }

    #[test]
    fn pad_rejects_oversized_length_like_postgres() {
        let e = lpad(&[t("abc"), Int4(i32::MAX)]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(e.message, "requested length too large");
        assert!(rpad(&[t("abc"), Int4(i32::MAX)]).is_err());
        assert_eq!(lpad(&[t("abc"), Int4(-5)]).unwrap(), t(""));
    }

    #[test]
    fn right_with_int_min_returns_whole_string() {
        assert_eq!(right(&[t("abc"), Int4(i32::MIN)]).unwrap(), t("abc"));
        assert_eq!(left(&[t("abc"), Int4(i32::MIN)]).unwrap(), t(""));
    }

    #[test]
    fn radians_underflow_is_an_error() {
        let e = radians(&[Float8(5e-324)]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "value out of range: underflow");
        assert_eq!(radians(&[Float8(0.0)]).unwrap(), Float8(0.0));
    }

    #[test]
    fn md5_digest() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }
}

// ---------------------------------------------------------------------------
// generate_series（`m4/09-types-functions.md` §10.1。FROM 句でだけ呼べる集合返却関数）
// ---------------------------------------------------------------------------

/// `generate_series(start, stop[, step])` の行の生成器。加算が桁あふれしたら終わる。
struct Series {
    cur: i64,
    stop: i64,
    step: i64,
    /// 結果が int4 なら true（桁あふれの境界が `i32` になる）。
    int4: bool,
    done: bool,
}

impl crate::catalog::SetIter for Series {
    fn next(&mut self) -> Result<Option<Datum>> {
        if self.done {
            return Ok(None);
        }
        let in_range = if self.step > 0 {
            self.cur <= self.stop
        } else {
            self.cur >= self.stop
        };
        if !in_range {
            self.done = true;
            return Ok(None);
        }
        let v = self.cur;
        match self.cur.checked_add(self.step) {
            Some(n) if !self.int4 || i32::try_from(n).is_ok() => self.cur = n,
            _ => self.done = true,
        }
        Ok(Some(if self.int4 {
            Datum::Int4(i32::try_from(v).map_err(|_| bad_arg("generate_series"))?)
        } else {
            Datum::Int8(v)
        }))
    }
}

/// 引数に NULL があれば 0 行（strict）。`step = 0` は `22023`。
fn series_begin(args: &[Datum], int4: bool) -> Result<Box<dyn crate::catalog::SetIter>> {
    if args.iter().any(Datum::is_null) {
        return Ok(Box::new(Series {
            cur: 0,
            stop: 0,
            step: 1,
            int4,
            done: true,
        }));
    }
    let first = int(args, 0)?;
    let last = int(args, 1)?;
    let by = if args.len() > 2 { int(args, 2)? } else { 1 };
    if by == 0 {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot equal zero",
        ));
    }
    Ok(Box::new(Series {
        cur: first,
        stop: last,
        step: by,
        int4,
        done: false,
    }))
}

/// `generate_series(int4, int4[, int4])`。
pub fn generate_series_i4(args: &[Datum]) -> Result<Box<dyn crate::catalog::SetIter>> {
    series_begin(args, true)
}

/// `generate_series(int8, int8[, int8])`。
pub fn generate_series_i8(args: &[Datum]) -> Result<Box<dyn crate::catalog::SetIter>> {
    series_begin(args, false)
}

#[cfg(test)]
mod series_tests {
    use super::*;

    fn collect(
        f: fn(&[Datum]) -> Result<Box<dyn crate::catalog::SetIter>>,
        a: &[Datum],
    ) -> Vec<Datum> {
        let mut it = f(a).unwrap();
        let mut out = Vec::new();
        while let Some(d) = it.next().unwrap() {
            out.push(d);
            assert!(out.len() < 100, "runaway series");
        }
        out
    }

    #[test]
    fn series_rules() {
        use Datum::{Int4, Int8, Null};
        assert_eq!(
            collect(generate_series_i4, &[Int4(1), Int4(3)]),
            vec![Int4(1), Int4(2), Int4(3)]
        );
        assert_eq!(
            collect(generate_series_i4, &[Int4(3), Int4(1), Int4(-1)]),
            vec![Int4(3), Int4(2), Int4(1)]
        );
        assert!(collect(generate_series_i4, &[Int4(3), Int4(1)]).is_empty());
        assert_eq!(
            collect(generate_series_i4, &[Int4(1), Int4(10), Int4(4)]),
            vec![Int4(1), Int4(5), Int4(9)]
        );
        // 桁あふれで止まる。
        assert_eq!(
            collect(generate_series_i4, &[Int4(i32::MAX - 1), Int4(i32::MAX)]),
            vec![Int4(i32::MAX - 1), Int4(i32::MAX)]
        );
        assert_eq!(
            collect(generate_series_i8, &[Int8(i64::MAX - 1), Int8(i64::MAX)]),
            vec![Int8(i64::MAX - 1), Int8(i64::MAX)]
        );
        assert_eq!(
            collect(
                generate_series_i4,
                &[Int4(i32::MIN + 1), Int4(i32::MIN), Int4(-1)]
            ),
            vec![Int4(i32::MIN + 1), Int4(i32::MIN)]
        );
        // NULL は 0 行。
        assert!(collect(generate_series_i4, &[Null, Int4(3)]).is_empty());
        assert!(collect(generate_series_i8, &[Int8(1), Null, Int8(1)]).is_empty());
        let Err(e) = generate_series_i4(&[Int4(1), Int4(3), Int4(0)]) else {
            panic!("step 0 must fail")
        };
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "step size cannot equal zero");
    }
}

// ---------------------------------------------------------------------------
// pg_size_pretty(bigint)（`\dt+`。`m4/11-tests-plan.md` G-3）
// ---------------------------------------------------------------------------

/// `pg_size_pretty(bigint)`: `10 kB` のように、整数の単位（半分の端数は絶対値が大きい側に丸める）。
#[allow(clippy::manual_midpoint)] // 絶対値が大きい側へ丸める（midpoint は 0 方向）
pub fn pg_size_pretty(args: &[Datum]) -> Result<Datum> {
    // (単位, 打ち切りの上限, 丸めるか, 次の単位へ進むときのシフト)。PostgreSQL の `size_pretty_units`。
    const UNITS: [(&str, i64, bool, u32); 6] = [
        ("bytes", 10 * 1024, false, 9),
        ("kB", 20 * 1024 - 1, true, 10),
        ("MB", 20 * 1024 - 1, true, 10),
        ("GB", 20 * 1024 - 1, true, 10),
        ("TB", 20 * 1024 - 1, true, 10),
        ("PB", i64::MAX, true, 0),
    ];
    let mut size = int(args, 0)?;
    for (name, limit, round, shift) in UNITS {
        if size.unsigned_abs() < limit.unsigned_abs() || name == "PB" {
            if round {
                size = (size + if size < 0 { -1 } else { 1 }) / 2;
            }
            return Ok(Datum::Text(format!("{size} {name}")));
        }
        size >>= shift;
    }
    Err(bad_arg("pg_size_pretty"))
}

/// `obj_description(oid[, name])`: コメントは持たないので常に NULL。
pub fn obj_description(_args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Null)
}

#[cfg(test)]
mod size_pretty_tests {
    use super::*;

    #[test]
    fn matches_postgresql() {
        let f = |n: i64| match pg_size_pretty(&[Datum::Int8(n)]).unwrap() {
            Datum::Text(s) => s,
            other => panic!("{other:?}"),
        };
        for (n, want) in [
            (0, "0 bytes"),
            (1023, "1023 bytes"),
            (10239, "10239 bytes"),
            (10240, "10 kB"),
            (10752, "11 kB"),
            (10753, "11 kB"),
            (11264, "11 kB"),
            (20479, "20 kB"),
            (20991, "20 kB"),
            (20992, "21 kB"),
            (1_048_576, "1024 kB"),
            (10_485_247, "10239 kB"),
            (10_485_248, "10 MB"),
            (20_971_520, "20 MB"),
            (21_474_836_480, "20 GB"),
            (-10240, "-10 kB"),
            (-10752, "-11 kB"),
            (-10753, "-11 kB"),
            (-20992, "-21 kB"),
            (i64::MAX, "8192 PB"),
        ] {
            assert_eq!(f(n), want, "{n}");
        }
    }
}
