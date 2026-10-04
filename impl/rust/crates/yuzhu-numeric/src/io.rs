//! Text input (`numeric_in` / `set_var_from_str`) and output
//! (`get_str_from_var`).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::fmt::Write as _;

use crate::error::NumericError;
use crate::var::{self, DEC_DIGITS, NUMERIC_WEIGHT_MAX, Var, d};

/// Result of parsing, before typmod application and range checks.
#[derive(Debug)]
pub(crate) enum Parsed {
    NaN,
    PosInf,
    NegInf,
    Finite(Var),
}

/// C `isspace` in the C locale.
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn starts_with_ci(s: &[u8], pat: &[u8]) -> bool {
    s.len() >= pat.len() && s[..pat.len()].eq_ignore_ascii_case(pat)
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cursor<'_> {
    fn at(&self, off: usize) -> u8 {
        self.b.get(self.p + off).copied().unwrap_or(0)
    }
}

/// `numeric_in` without typmod application.
pub(crate) fn parse(s: &str) -> Result<Parsed, NumericError> {
    let invalid = || NumericError::invalid_syntax(s);
    let mut c = Cursor {
        b: s.as_bytes(),
        p: 0,
    };
    while c.p < c.b.len() && is_space(c.b[c.p]) {
        c.p += 1;
    }
    let numstart = c.p;
    let mut neg = false;
    if c.at(0) == b'+' {
        c.p += 1;
    } else if c.at(0) == b'-' {
        neg = true;
        c.p += 1;
    }

    let res = if !c.at(0).is_ascii_digit() && c.at(0) != b'.' {
        // Must be NaN or infinity (NaN must not have a sign).
        if starts_with_ci(&c.b[numstart..], b"nan") {
            c.p = numstart + 3;
            Parsed::NaN
        } else if starts_with_ci(&c.b[c.p..], b"infinity") {
            c.p += 8;
            if neg { Parsed::NegInf } else { Parsed::PosInf }
        } else if starts_with_ci(&c.b[c.p..], b"inf") {
            c.p += 3;
            if neg { Parsed::NegInf } else { Parsed::PosInf }
        } else {
            return Err(invalid());
        }
    } else {
        let base = if c.at(0) == b'0' {
            match c.at(1) {
                b'x' | b'X' => 16,
                b'o' | b'O' => 8,
                b'b' | b'B' => 2,
                _ => 10,
            }
        } else {
            10
        };
        let mut v = if base == 10 {
            parse_decimal(&mut c, s)?
        } else {
            c.p += 2;
            parse_non_decimal(&mut c, base, s)?
        };
        v.neg = neg && !v.digits.is_empty();
        Parsed::Finite(v)
    };
    // Only trailing spaces may remain.
    while c.p < c.b.len() {
        if !is_space(c.b[c.p]) {
            return Err(invalid());
        }
        c.p += 1;
    }
    Ok(res)
}

/// `set_var_from_str` (the sign has already been consumed).
fn parse_decimal(c: &mut Cursor<'_>, s: &str) -> Result<Var, NumericError> {
    let invalid = || NumericError::invalid_syntax(s);
    let mut have_dp = false;
    if c.at(0) == b'.' {
        have_dp = true;
        c.p += 1;
    }
    if !c.at(0).is_ascii_digit() {
        return Err(invalid());
    }
    let mut dec: Vec<u8> = Vec::new();
    let mut dweight: i64 = -1;
    let mut dscale: i64 = 0;
    loop {
        let ch = c.at(0);
        if ch.is_ascii_digit() {
            dec.push(ch - b'0');
            c.p += 1;
            if have_dp {
                dscale += 1;
            } else {
                dweight += 1;
            }
        } else if ch == b'.' {
            if have_dp {
                return Err(invalid());
            }
            have_dp = true;
            c.p += 1;
            if c.at(0) == b'_' {
                return Err(invalid());
            }
        } else if ch == b'_' {
            c.p += 1;
            if !c.at(0).is_ascii_digit() {
                return Err(invalid());
            }
        } else {
            break;
        }
    }
    if matches!(c.at(0), b'e' | b'E') {
        c.p += 1;
        let exponent = parse_exponent(c, s)?;
        dweight += exponent;
        dscale = (dscale - exponent).max(0);
    }
    let dd = i64::from(DEC_DIGITS);
    let weight = if dweight >= 0 {
        (dweight + 1 + dd - 1) / dd - 1
    } else {
        -((-dweight - 1) / dd + 1)
    };
    let offset = (weight + 1) * dd - (dweight + 1);
    let ndec = dec.len() as i64;
    let ndigits = (ndec + offset + dd - 1) / dd;
    let mut digits = Vec::with_capacity(ndigits as usize);
    for k in 0..ndigits {
        let mut acc = 0i32;
        for j in 0..dd {
            let idx = k * dd + j - offset;
            let x = if idx >= 0 && idx < ndec {
                i32::from(dec[idx as usize])
            } else {
                0
            };
            acc = acc * 10 + x;
        }
        digits.push(d(acc));
    }
    let weight = i32::try_from(weight).map_err(|_| NumericError::overflow())?;
    let dscale = i32::try_from(dscale).map_err(|_| NumericError::overflow())?;
    Ok(Var {
        neg: false,
        weight,
        dscale,
        digits,
    }
    .stripped())
}

/// Exponent digits after `e`/`E` (sign included); bounded like PostgreSQL.
fn parse_exponent(c: &mut Cursor<'_>, s: &str) -> Result<i64, NumericError> {
    let invalid = || NumericError::invalid_syntax(s);
    let mut eneg = false;
    if c.at(0) == b'+' {
        c.p += 1;
    } else if c.at(0) == b'-' {
        eneg = true;
        c.p += 1;
    }
    if !c.at(0).is_ascii_digit() {
        return Err(invalid());
    }
    let mut exponent: i64 = 0;
    loop {
        let ch = c.at(0);
        if ch.is_ascii_digit() {
            exponent = exponent * 10 + i64::from(ch - b'0');
            c.p += 1;
            if exponent > i64::from(i32::MAX / 2) {
                return Err(NumericError::overflow());
            }
        } else if ch == b'_' {
            c.p += 1;
            if !c.at(0).is_ascii_digit() {
                return Err(invalid());
            }
        } else {
            break;
        }
    }
    Ok(if eneg { -exponent } else { exponent })
}

/// `set_var_from_non_decimal_integer_str` (prefix already consumed).
fn parse_non_decimal(c: &mut Cursor<'_>, base: u32, s: &str) -> Result<Var, NumericError> {
    let invalid = || NumericError::invalid_syntax(s);
    let firstdigit = c.p;
    let mut dest = Var::zero(0);
    let mut tmp: u128 = 0;
    let mut mul: u128 = 1;
    let limit = (i64::MAX as u128) / u128::from(base);
    let flush = |dest: &mut Var, tmp: u128, mul: u128| -> Result<(), NumericError> {
        let m = Var::from_u128(mul, false);
        let t = Var::from_u128(tmp, false);
        *dest = var::add(&var::mul(dest, &m), &t);
        if dest.weight > NUMERIC_WEIGHT_MAX {
            return Err(NumericError::overflow());
        }
        Ok(())
    };
    loop {
        let ch = c.at(0);
        if let Some(dv) = char::from(ch).to_digit(base) {
            if mul > limit {
                flush(&mut dest, tmp, mul)?;
                tmp = 0;
                mul = 1;
            }
            tmp = tmp * u128::from(base) + u128::from(dv);
            mul *= u128::from(base);
            c.p += 1;
        } else if ch == b'_' {
            c.p += 1;
            if char::from(c.at(0)).to_digit(base).is_none() {
                return Err(invalid());
            }
        } else {
            break;
        }
    }
    if c.p == firstdigit {
        return Err(invalid());
    }
    flush(&mut dest, tmp, mul)?;
    dest.dscale = 0;
    Ok(dest)
}

/// `get_str_from_var`: fixed-point text with exactly `dscale` fraction digits.
pub(crate) fn format_var(v: &Var, out: &mut String) {
    if v.neg && !v.is_zero() {
        out.push('-');
    }
    let n = v.ndigits();
    let digit = |i: i32| -> i16 {
        if i >= 0 && i < n {
            v.digits[i as usize]
        } else {
            0
        }
    };
    let mut di: i32;
    if v.weight < 0 {
        di = v.weight + 1;
        out.push('0');
    } else {
        for i in 0..=v.weight {
            if i == 0 {
                let _ = write!(out, "{}", digit(i));
            } else {
                let _ = write!(out, "{:04}", digit(i));
            }
        }
        di = v.weight + 1;
    }
    if v.dscale > 0 {
        out.push('.');
        let end = out.len() + v.dscale as usize;
        let mut i = 0;
        while i < v.dscale {
            let _ = write!(out, "{:04}", digit(di));
            di += 1;
            i += DEC_DIGITS;
        }
        out.truncate(end);
    }
}
