//! PostgreSQL-compatible arbitrary-precision `numeric` (type OID 1700).
//!
//! This is a hand-written port of the semantics of PostgreSQL 17's
//! `numeric.c`: base-10000 digits with a weight, sign (including NaN and
//! ±Infinity) and display scale (`dscale`); text I/O, typmod application,
//! arithmetic with PostgreSQL's result-scale rules, rounding (half away from
//! zero), comparison (NaN sorts above everything) and casts to and from the
//! integer and floating-point types. Errors carry the SQLSTATE and message
//! text PostgreSQL would report.
//!
//! Not yet implemented: `sqrt`, `power`, `exp`, `ln`, `log`.

#![forbid(unsafe_code)]

mod convert;
mod error;
mod io;
mod var;

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

pub use error::{NumericError, sqlstate};

use io::Parsed;
use var::{NUMERIC_DSCALE_MAX, NUMERIC_WEIGHT_MAX, NUMERIC_WEIGHT_MIN, Var};

/// Type OID of `numeric`.
pub const NUMERIC_OID: u32 = 1700;
/// Largest precision allowed in a typmod (`NUMERIC_MAX_PRECISION`).
pub const NUMERIC_MAX_PRECISION: i32 = 1000;
/// Smallest scale allowed in a typmod (`NUMERIC_MIN_SCALE`).
pub const NUMERIC_MIN_SCALE: i32 = -1000;
/// Largest scale allowed in a typmod (`NUMERIC_MAX_SCALE`).
pub const NUMERIC_MAX_SCALE: i32 = 1000;
/// Lower clamp of the scale argument of `trunc` (all digits that can exist
/// before the point); `round` allows one more for a carry.
const TRUNC_MIN_SCALE: i32 = -(NUMERIC_WEIGHT_MAX + 1) * var::DEC_DIGITS;
/// `VARHDRSZ`, the typmod offset.
const VARHDRSZ: i32 = 4;

/// A PostgreSQL `numeric` value.
///
/// Equality, ordering and hashing follow PostgreSQL: values compare by
/// magnitude ignoring display scale (`1.10 = 1.1`), and the total order is
/// `-Infinity < finite < Infinity < NaN` with `NaN = NaN`.
#[derive(Clone, Debug)]
pub enum Numeric {
    /// Not-a-number.
    NaN,
    /// `Infinity`.
    PosInf,
    /// `-Infinity`.
    NegInf,
    /// A finite value.
    Finite(Finite),
}

/// A finite numeric value, normalised (no leading or trailing zero digits;
/// zero has no digits and is non-negative) and within PostgreSQL's storage
/// limits (weight fits `i16`, `dscale <= 16383`).
#[derive(Clone, Debug)]
pub struct Finite(Var);

impl Finite {
    /// True if the value is negative.
    pub fn is_negative(&self) -> bool {
        self.0.neg
    }

    /// Weight of the first base-10000 digit.
    pub fn weight(&self) -> i16 {
        i16::try_from(self.0.weight).expect("weight within storage limits")
    }

    /// Display scale (decimal digits after the point).
    pub fn dscale(&self) -> u16 {
        u16::try_from(self.0.dscale).expect("dscale within storage limits")
    }

    /// Base-10000 digits, each in `0..10000`.
    pub fn digits(&self) -> &[i16] {
        &self.0.digits
    }
}

impl Numeric {
    /// Zero with display scale 0.
    pub fn zero() -> Self {
        Self::Finite(Finite(Var::zero(0)))
    }

    /// `make_result`: check storage limits and wrap.
    fn finish(v: Var) -> Result<Self, NumericError> {
        let mut v = v;
        v.strip();
        if v.dscale < 0 {
            v.dscale = 0;
        }
        if !v.digits.is_empty() && (v.weight > NUMERIC_WEIGHT_MAX || v.weight < NUMERIC_WEIGHT_MIN)
        {
            return Err(NumericError::overflow());
        }
        if v.dscale > NUMERIC_DSCALE_MAX {
            return Err(NumericError::overflow());
        }
        Ok(Self::Finite(Finite(v)))
    }

    /// Build from raw parts (used by storage layers decoding PostgreSQL's
    /// on-disk format). Digits beyond `dscale` are truncated, as
    /// `numeric_recv` does.
    pub fn from_parts(
        negative: bool,
        weight: i16,
        dscale: u16,
        digits: &[i16],
    ) -> Result<Self, NumericError> {
        if dscale > 0x3FFF {
            return Err(NumericError::new(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid scale in external \"numeric\" value",
            ));
        }
        if digits.iter().any(|&x| !(0..10000).contains(&x)) {
            return Err(NumericError::new(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid digit in external \"numeric\" value",
            ));
        }
        let mut v = Var {
            neg: negative,
            weight: i32::from(weight),
            dscale: i32::from(dscale),
            digits: digits.to_vec(),
        };
        var::trunc_var(&mut v, i32::from(dscale));
        Self::finish(v)
    }

    /// Parse text exactly like `numeric_in` with typmod -1.
    pub fn parse(s: &str) -> Result<Self, NumericError> {
        Self::parse_with_typmod(s, -1)
    }

    /// Parse text exactly like `numeric_in(s, typmod)`: syntax errors first,
    /// then typmod (rounding / `numeric field overflow`), then storage limits.
    pub fn parse_with_typmod(s: &str, typmod: i32) -> Result<Self, NumericError> {
        match io::parse(s)? {
            Parsed::NaN => Ok(Self::NaN),
            Parsed::PosInf => apply_typmod_special(Self::PosInf, typmod),
            Parsed::NegInf => apply_typmod_special(Self::NegInf, typmod),
            Parsed::Finite(mut v) => {
                apply_typmod(&mut v, typmod)?;
                Self::finish(v)
            }
        }
    }

    /// Sizing cast `numeric -> numeric(p, s)` (the `numeric(numeric, int4)`
    /// function): round to the scale and check the precision.
    pub fn apply_typmod(&self, typmod: i32) -> Result<Self, NumericError> {
        match self {
            Self::Finite(f) => {
                let mut v = f.0.clone();
                apply_typmod(&mut v, typmod)?;
                Self::finish(v)
            }
            other => apply_typmod_special(other.clone(), typmod),
        }
    }

    /// True for NaN.
    pub fn is_nan(&self) -> bool {
        matches!(self, Self::NaN)
    }

    /// True for `Infinity` or `-Infinity`.
    pub fn is_infinite(&self) -> bool {
        matches!(self, Self::PosInf | Self::NegInf)
    }

    /// True for a finite zero.
    pub fn is_zero(&self) -> bool {
        matches!(self, Self::Finite(f) if f.0.is_zero())
    }

    /// `scale(numeric)`: display scale, `None` for NaN/±Infinity.
    pub fn scale(&self) -> Option<i32> {
        match self {
            Self::Finite(f) => Some(f.0.dscale),
            _ => None,
        }
    }

    /// Sign as -1/0/1 for finite and infinite values, `None` for NaN.
    fn sign_internal(&self) -> Option<i32> {
        match self {
            Self::NaN => None,
            Self::PosInf => Some(1),
            Self::NegInf => Some(-1),
            Self::Finite(f) => Some(if f.0.is_zero() {
                0
            } else if f.0.neg {
                -1
            } else {
                1
            }),
        }
    }

    fn var(&self) -> Option<&Var> {
        match self {
            Self::Finite(f) => Some(&f.0),
            _ => None,
        }
    }

    fn inf(negative: bool) -> Self {
        if negative { Self::NegInf } else { Self::PosInf }
    }

    /// `numeric_add`.
    pub fn checked_add(&self, other: &Self) -> Result<Self, NumericError> {
        match (self, other) {
            (Self::Finite(a), Self::Finite(b)) => Self::finish(var::add(&a.0, &b.0)),
            (Self::NaN, _)
            | (_, Self::NaN)
            | (Self::PosInf, Self::NegInf)
            | (Self::NegInf, Self::PosInf) => Ok(Self::NaN),
            (Self::PosInf, _) | (_, Self::PosInf) => Ok(Self::PosInf),
            (Self::NegInf, _) | (_, Self::NegInf) => Ok(Self::NegInf),
        }
    }

    /// `numeric_sub`.
    pub fn checked_sub(&self, other: &Self) -> Result<Self, NumericError> {
        match (self, other) {
            (Self::Finite(a), Self::Finite(b)) => Self::finish(var::sub(&a.0, &b.0)),
            (Self::NaN, _)
            | (_, Self::NaN)
            | (Self::PosInf, Self::PosInf)
            | (Self::NegInf, Self::NegInf) => Ok(Self::NaN),
            (Self::PosInf, _) | (_, Self::NegInf) => Ok(Self::PosInf),
            (Self::NegInf, _) | (_, Self::PosInf) => Ok(Self::NegInf),
        }
    }

    /// `numeric_mul`: result scale is the sum of the input scales.
    pub fn checked_mul(&self, other: &Self) -> Result<Self, NumericError> {
        if let (Self::Finite(a), Self::Finite(b)) = (self, other) {
            return Self::finish(var::mul(&a.0, &b.0));
        }
        match (self.sign_internal(), other.sign_internal()) {
            (Some(s1), Some(s2)) => {
                // At least one side is infinite.
                if s1 == 0 || s2 == 0 {
                    Ok(Self::NaN)
                } else {
                    Ok(Self::inf(s1 * s2 < 0))
                }
            }
            _ => Ok(Self::NaN),
        }
    }

    /// Shared special-value handling of `numeric_div` and `numeric_div_trunc`.
    fn div_special(&self, other: &Self) -> Result<Option<Self>, NumericError> {
        if self.is_nan() || other.is_nan() {
            return Ok(Some(Self::NaN));
        }
        if self.is_infinite() {
            if other.is_infinite() {
                return Ok(Some(Self::NaN));
            }
            let s1 = self.sign_internal().unwrap_or(0);
            return match other.sign_internal() {
                Some(0) => Err(NumericError::division_by_zero()),
                Some(s2) => Ok(Some(Self::inf(s1 * s2 < 0))),
                None => Ok(Some(Self::NaN)),
            };
        }
        if other.is_infinite() {
            return Ok(Some(Self::zero()));
        }
        Ok(None)
    }

    /// `numeric_div`: result scale from `select_div_scale`, rounded half
    /// away from zero.
    pub fn checked_div(&self, other: &Self) -> Result<Self, NumericError> {
        if let Some(r) = self.div_special(other)? {
            return Ok(r);
        }
        let (a, b) = (self.var().expect("finite"), other.var().expect("finite"));
        let rscale = var::select_div_scale(a, b);
        Self::finish(var::div(a, b, rscale, true)?)
    }

    /// `div(numeric, numeric)`: quotient truncated toward zero, scale 0.
    pub fn div_trunc(&self, other: &Self) -> Result<Self, NumericError> {
        if let Some(r) = self.div_special(other)? {
            return Ok(r);
        }
        let (a, b) = (self.var().expect("finite"), other.var().expect("finite"));
        Self::finish(var::div(a, b, 0, false)?)
    }

    /// `numeric_mod` (`%`, `mod`): result has the sign of the dividend and
    /// scale `max(s1, s2)`.
    pub fn checked_rem(&self, other: &Self) -> Result<Self, NumericError> {
        match (self, other) {
            (Self::Finite(a), Self::Finite(b)) => Self::finish(var::modulo(&a.0, &b.0)?),
            (Self::NaN, _) | (_, Self::NaN) => Ok(Self::NaN),
            (Self::PosInf | Self::NegInf, _) => {
                if other.sign_internal() == Some(0) {
                    Err(NumericError::division_by_zero())
                } else {
                    Ok(Self::NaN)
                }
            }
            (Self::Finite(_), _) => Ok(self.clone()),
        }
    }

    /// Unary minus (`numeric_uminus`).
    #[must_use]
    pub fn negate(&self) -> Self {
        match self {
            Self::NaN => Self::NaN,
            Self::PosInf => Self::NegInf,
            Self::NegInf => Self::PosInf,
            Self::Finite(f) => {
                let mut v = f.0.clone();
                v.neg = !v.neg && !v.is_zero();
                Self::Finite(Finite(v))
            }
        }
    }

    /// `abs(numeric)`.
    #[must_use]
    pub fn abs(&self) -> Self {
        match self {
            Self::NaN => Self::NaN,
            Self::PosInf | Self::NegInf => Self::PosInf,
            Self::Finite(f) => {
                let mut v = f.0.clone();
                v.neg = false;
                Self::Finite(Finite(v))
            }
        }
    }

    /// `sign(numeric)`: -1, 0 or 1 (scale 0); NaN for NaN.
    #[must_use]
    pub fn sign(&self) -> Self {
        match self.sign_internal() {
            None => Self::NaN,
            Some(s) => Self::from(i64::from(s)),
        }
    }

    /// `round(numeric, int)`: half away from zero; negative scales round
    /// to the left of the point.
    pub fn round(&self, scale: i32) -> Result<Self, NumericError> {
        let Some(v) = self.var() else {
            return Ok(self.clone());
        };
        let scale = scale.clamp(TRUNC_MIN_SCALE - 1, NUMERIC_DSCALE_MAX);
        let mut v = v.clone();
        var::round_var(&mut v, scale);
        Self::finish(v)
    }

    /// `trunc(numeric, int)`.
    pub fn trunc(&self, scale: i32) -> Result<Self, NumericError> {
        let Some(v) = self.var() else {
            return Ok(self.clone());
        };
        let scale = scale.clamp(TRUNC_MIN_SCALE, NUMERIC_DSCALE_MAX);
        let mut v = v.clone();
        var::trunc_var(&mut v, scale);
        Self::finish(v)
    }

    /// `ceil(numeric)` / `ceiling(numeric)`.
    pub fn ceil(&self) -> Result<Self, NumericError> {
        match self.var() {
            Some(v) => Self::finish(var::ceil_floor(v, true)),
            None => Ok(self.clone()),
        }
    }

    /// `floor(numeric)`.
    pub fn floor(&self) -> Result<Self, NumericError> {
        match self.var() {
            Some(v) => Self::finish(var::ceil_floor(v, false)),
            None => Ok(self.clone()),
        }
    }

    /// Binary wire format (`numeric_send`).
    pub fn to_binary(&self) -> Vec<u8> {
        let (ndigits, weight, sign, dscale, digits): (u16, i16, u16, u16, &[i16]) = match self {
            Self::NaN => (0, 0, 0xC000, 0, &[]),
            // PostgreSQL reads the dscale of ±Infinity from the short-header
            // bits of the special-value header (0xD000 & 0x1F80) >> 7 = 32,
            // so that is what goes on the wire.
            Self::PosInf => (0, 0, 0xD000, 32, &[]),
            Self::NegInf => (0, 0, 0xF000, 32, &[]),
            Self::Finite(f) => (
                u16::try_from(f.0.digits.len()).expect("digit count fits u16"),
                f.weight(),
                if f.0.neg { 0x4000 } else { 0 },
                f.dscale(),
                &f.0.digits,
            ),
        };
        let mut out = Vec::with_capacity(8 + 2 * digits.len());
        out.extend_from_slice(&ndigits.to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
        for &x in digits {
            out.extend_from_slice(&x.to_be_bytes());
        }
        out
    }

    /// Decode the binary wire format (`numeric_recv`) and apply `typmod`.
    /// The input must be exactly one value.
    pub fn from_binary(buf: &[u8], typmod: i32) -> Result<Self, NumericError> {
        let short = || {
            NumericError::new(
                sqlstate::PROTOCOL_VIOLATION,
                "insufficient data left in message",
            )
        };
        let get = |i: usize| -> Result<[u8; 2], NumericError> {
            buf.get(i..i + 2).map(|s| [s[0], s[1]]).ok_or_else(short)
        };
        let ndigits = usize::from(u16::from_be_bytes(get(0)?));
        let weight = i16::from_be_bytes(get(2)?);
        let sign = u16::from_be_bytes(get(4)?);
        if !matches!(sign, 0 | 0x4000 | 0xC000 | 0xD000 | 0xF000) {
            return Err(NumericError::new(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid sign in external \"numeric\" value",
            ));
        }
        let dscale = u16::from_be_bytes(get(6)?);
        if dscale & 0x3FFF != dscale {
            return Err(NumericError::new(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "invalid scale in external \"numeric\" value",
            ));
        }
        let mut digits = Vec::with_capacity(ndigits);
        for i in 0..ndigits {
            let x = i16::from_be_bytes(get(8 + 2 * i)?);
            if !(0..10000).contains(&x) {
                return Err(NumericError::new(
                    sqlstate::INVALID_BINARY_REPRESENTATION,
                    "invalid digit in external \"numeric\" value",
                ));
            }
            digits.push(x);
        }
        if buf.len() != 8 + 2 * ndigits {
            return Err(NumericError::new(
                sqlstate::INVALID_BINARY_REPRESENTATION,
                "incorrect binary data format",
            ));
        }
        match sign {
            0xC000 => Ok(Self::NaN),
            0xD000 => apply_typmod_special(Self::PosInf, typmod),
            0xF000 => apply_typmod_special(Self::NegInf, typmod),
            _ => {
                let mut v = Var {
                    neg: sign == 0x4000,
                    weight: i32::from(weight),
                    dscale: i32::from(dscale),
                    digits,
                };
                var::trunc_var(&mut v, i32::from(dscale));
                apply_typmod(&mut v, typmod)?;
                Self::finish(v)
            }
        }
    }
}

/// Precision and scale encoded in a numeric typmod, or `None` for -1 /
/// invalid typmods (`is_valid_numeric_typmod`).
pub fn typmod_precision_scale(typmod: i32) -> Option<(i32, i32)> {
    if typmod < VARHDRSZ {
        return None;
    }
    let t = typmod - VARHDRSZ;
    let precision = (t >> 16) & 0xffff;
    let scale = ((t & 0x7ff) ^ 1024) - 1024;
    Some((precision, scale))
}

/// `numerictypmodin`: build a typmod from the modifiers of `numeric(p)` or
/// `numeric(p, s)`.
pub fn make_typmod(mods: &[i32]) -> Result<i32, NumericError> {
    let check_p = |p: i32| {
        if (1..=NUMERIC_MAX_PRECISION).contains(&p) {
            Ok(())
        } else {
            Err(NumericError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("NUMERIC precision {p} must be between 1 and {NUMERIC_MAX_PRECISION}"),
            ))
        }
    };
    let (p, s) = match *mods {
        [p, s] => {
            check_p(p)?;
            if !(NUMERIC_MIN_SCALE..=NUMERIC_MAX_SCALE).contains(&s) {
                return Err(NumericError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!(
                        "NUMERIC scale {s} must be between {NUMERIC_MIN_SCALE} and {NUMERIC_MAX_SCALE}"
                    ),
                ));
            }
            (p, s)
        }
        [p] => {
            check_p(p)?;
            (p, 0)
        }
        _ => {
            return Err(NumericError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid NUMERIC type modifier",
            ));
        }
    };
    Ok(((p << 16) | (s & 0x7ff)) + VARHDRSZ)
}

/// `numerictypmodout`: `"(p,s)"`, or an empty string for no typmod.
pub fn format_typmod(typmod: i32) -> String {
    match typmod_precision_scale(typmod) {
        Some((p, s)) => format!("({p},{s})"),
        None => String::new(),
    }
}

/// `apply_typmod`.
fn apply_typmod(v: &mut Var, typmod: i32) -> Result<(), NumericError> {
    let Some((precision, scale)) = typmod_precision_scale(typmod) else {
        return Ok(());
    };
    let maxdigits = precision - scale;
    var::round_var(v, scale);
    if v.dscale < 0 {
        v.dscale = 0;
    }
    if let Some(ddigits) = var::int_decimal_digits(v)
        && ddigits > maxdigits
    {
        let bound = if maxdigits == 0 {
            "1".to_owned()
        } else {
            format!("10^{maxdigits}")
        };
        return Err(NumericError::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "numeric field overflow",
        )
        .with_detail(format!(
            "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
        )));
    }
    Ok(())
}

/// `apply_typmod_special`: NaN passes, infinities fail under any typmod.
fn apply_typmod_special(n: Numeric, typmod: i32) -> Result<Numeric, NumericError> {
    if n.is_nan() {
        return Ok(n);
    }
    match typmod_precision_scale(typmod) {
        None => Ok(n),
        Some((precision, scale)) => Err(NumericError::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "numeric field overflow",
        )
        .with_detail(format!(
            "A field with precision {precision}, scale {scale} cannot hold an infinite value."
        ))),
    }
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NaN => f.write_str("NaN"),
            Self::PosInf => f.write_str("Infinity"),
            Self::NegInf => f.write_str("-Infinity"),
            Self::Finite(v) => {
                let mut s = String::new();
                io::format_var(&v.0, &mut s);
                f.write_str(&s)
            }
        }
    }
}

impl FromStr for Numeric {
    type Err = NumericError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Ord for Numeric {
    /// `cmp_numerics`.
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(n: &Numeric) -> u8 {
            match n {
                Numeric::NegInf => 0,
                Numeric::Finite(_) => 1,
                Numeric::PosInf => 2,
                Numeric::NaN => 3,
            }
        }
        match (self, other) {
            (Self::Finite(a), Self::Finite(b)) => var::cmp(&a.0, &b.0),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl PartialOrd for Numeric {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Numeric {}

impl Hash for Numeric {
    /// Consistent with `Eq`: display scale is ignored (digits are stored
    /// without trailing zeros).
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::NaN => state.write_u8(0),
            Self::PosInf => state.write_u8(1),
            Self::NegInf => state.write_u8(2),
            Self::Finite(f) => {
                state.write_u8(3);
                f.0.neg.hash(state);
                f.0.weight.hash(state);
                f.0.digits.hash(state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    #[test]
    fn basics() {
        assert_eq!(n("  1.230 ").to_string(), "1.230");
        assert_eq!(n("1e3").to_string(), "1000");
        assert_eq!(n("1.0e-20").to_string(), "0.000000000000000000010");
        assert_eq!(n("-0.0").to_string(), "0.0");
        assert_eq!(
            n("1.50").checked_mul(&n("2.25")).unwrap().to_string(),
            "3.3750"
        );
        assert_eq!(
            n("1.0").checked_div(&n("3")).unwrap().to_string(),
            "0.33333333333333333333"
        );
        assert_eq!(
            n("100").checked_div(&n("3")).unwrap().to_string(),
            "33.3333333333333333"
        );
        assert_eq!(n("-7.5").checked_rem(&n("2")).unwrap().to_string(), "-1.5");
        assert_eq!(n("2.5").round(0).unwrap().to_string(), "3");
        assert_eq!(n("-2.5").round(0).unwrap().to_string(), "-3");
        assert_eq!(n("1.10"), n("1.1"));
        assert!(Numeric::NaN > Numeric::PosInf);
        let e = Numeric::parse_with_typmod("123.456", make_typmod(&[4, 2]).unwrap()).unwrap_err();
        assert_eq!(e.sqlstate(), "22003");
        assert_eq!(
            e.detail(),
            Some(
                "A field with precision 4, scale 2 must round to an absolute value less than 10^2."
            )
        );
    }
}
