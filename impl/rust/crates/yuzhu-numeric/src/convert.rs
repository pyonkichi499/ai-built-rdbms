//! Casts between numeric and the integer / floating-point types.

use crate::error::{NumericError, sqlstate};
use crate::io::{self, Parsed};
use crate::var::{self, Var};
use crate::{Finite, Numeric};

impl From<i64> for Numeric {
    /// `int8_numeric` (exact, scale 0).
    fn from(v: i64) -> Self {
        Numeric::Finite(Finite(Var::from_u128(u128::from(v.unsigned_abs()), v < 0)))
    }
}

impl From<i32> for Numeric {
    fn from(v: i32) -> Self {
        Self::from(i64::from(v))
    }
}

impl From<i16> for Numeric {
    fn from(v: i16) -> Self {
        Self::from(i64::from(v))
    }
}

/// Parse the output of C's `%.{N}g` (here produced in Rust's `{:.Ne}`
/// form) the way `set_var_from_str` would: trailing fraction zeros are
/// dropped first, as `%g` does, which determines the display scale.
fn from_sci_text(s: &str) -> Numeric {
    let (mant, exp) = s.split_once('e').expect("exponent form");
    let mant = if mant.contains('.') {
        mant.trim_end_matches('0').trim_end_matches('.')
    } else {
        mant
    };
    let text = format!("{mant}e{exp}");
    match io::parse(&text) {
        Ok(Parsed::Finite(v)) => Numeric::Finite(Finite(v)),
        _ => unreachable!("formatted float is a valid numeric"),
    }
}

impl Numeric {
    /// `float8_numeric`: NaN/±Infinity map to the special values; finite
    /// values go through 15 significant digits (`%.15g`, `DBL_DIG`).
    pub fn from_f64(v: f64) -> Self {
        if v.is_nan() {
            Self::NaN
        } else if v.is_infinite() {
            Self::inf(v < 0.0)
        } else {
            from_sci_text(&format!("{v:.14e}"))
        }
    }

    /// `float4_numeric`: like [`Numeric::from_f64`] with 6 significant
    /// digits (`FLT_DIG`).
    pub fn from_f32(v: f32) -> Self {
        if v.is_nan() {
            Self::NaN
        } else if v.is_infinite() {
            Self::inf(v < 0.0)
        } else {
            from_sci_text(&format!("{:.5e}", f64::from(v)))
        }
    }

    /// `numeric_float8`: via the text form and a correctly rounded parse.
    /// Values that overflow to infinity or underflow to zero raise 22003
    /// like `float8in`.
    pub fn to_f64(&self) -> Result<f64, NumericError> {
        match self {
            Self::NaN => Ok(f64::NAN),
            Self::PosInf => Ok(f64::INFINITY),
            Self::NegInf => Ok(f64::NEG_INFINITY),
            Self::Finite(f) => {
                let s = self.to_string();
                let v: f64 = s.parse().expect("numeric text parses as f64");
                if v.is_infinite() || (v == 0.0 && !f.0.is_zero()) {
                    return Err(NumericError::new(
                        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                        format!("\"{s}\" is out of range for type double precision"),
                    ));
                }
                Ok(v)
            }
        }
    }

    /// `numeric_float4`: like [`Numeric::to_f64`] for `real`.
    pub fn to_f32(&self) -> Result<f32, NumericError> {
        match self {
            Self::NaN => Ok(f32::NAN),
            Self::PosInf => Ok(f32::INFINITY),
            Self::NegInf => Ok(f32::NEG_INFINITY),
            Self::Finite(f) => {
                let s = self.to_string();
                let v: f32 = s.parse().expect("numeric text parses as f32");
                if v.is_infinite() || (v == 0.0 && !f.0.is_zero()) {
                    return Err(NumericError::new(
                        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                        format!("\"{s}\" is out of range for type real"),
                    ));
                }
                Ok(v)
            }
        }
    }

    /// Round half away from zero to an integer and range-check it.
    fn to_int(
        &self,
        type_name: &str,
        range_msg: &str,
        min: i128,
        max: i128,
    ) -> Result<i128, NumericError> {
        let f = match self {
            Self::NaN => {
                return Err(NumericError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format!("cannot convert NaN to {type_name}"),
                ));
            }
            Self::PosInf | Self::NegInf => {
                return Err(NumericError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format!("cannot convert infinity to {type_name}"),
                ));
            }
            Self::Finite(f) => f,
        };
        let out_of_range = || NumericError::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, range_msg);
        let mut v = f.0.clone();
        var::round_var(&mut v, 0);
        if v.digits.is_empty() {
            return Ok(0);
        }
        // i64 needs at most 5 base-10000 digits before the point.
        if v.weight > 4 {
            return Err(out_of_range());
        }
        let mut acc: i128 = 0;
        for i in 0..=v.weight {
            let idx = usize::try_from(i).expect("non-negative");
            acc = acc * 10000 + v.digits.get(idx).map_or(0, |&x| i128::from(x));
        }
        if v.neg {
            acc = -acc;
        }
        if acc < min || acc > max {
            return Err(out_of_range());
        }
        Ok(acc)
    }

    /// `numeric_int8`: rounds half away from zero; 22003 `bigint out of range`.
    pub fn to_i64(&self) -> Result<i64, NumericError> {
        self.to_int(
            "bigint",
            "bigint out of range",
            i64::MIN.into(),
            i64::MAX.into(),
        )
        .map(|v| i64::try_from(v).expect("range checked"))
    }

    /// `numeric_int4`: rounds half away from zero; 22003 `integer out of range`.
    pub fn to_i32(&self) -> Result<i32, NumericError> {
        self.to_int(
            "integer",
            "integer out of range",
            i32::MIN.into(),
            i32::MAX.into(),
        )
        .map(|v| i32::try_from(v).expect("range checked"))
    }

    /// `numeric_int2`: rounds half away from zero; 22003 `smallint out of range`.
    pub fn to_i16(&self) -> Result<i16, NumericError> {
        self.to_int(
            "smallint",
            "smallint out of range",
            i16::MIN.into(),
            i16::MAX.into(),
        )
        .map(|v| i16::try_from(v).expect("range checked"))
    }
}
