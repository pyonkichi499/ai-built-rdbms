//! `sqrt`, `ln` and `log` for numeric (ports of `sqrt_var`, `ln_var`,
//! `log_var` and the result-scale rules of PostgreSQL's `numeric.c`).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::cmp::Ordering;

use crate::Numeric;
use crate::error::{NumericError, sqlstate};
use crate::var::{self, DEC_DIGITS, NBASE, Var};

const MIN_SIG_DIGITS: i32 = 16;
const MAX_DISPLAY_SCALE: i32 = 1000;
/// Extra decimal digits carried through intermediate steps.
const GUARD: i32 = 10;

fn small(v: u128) -> Var {
    Var::from_u128(v, false)
}

fn scaled(mut v: Var, scale: i32) -> Var {
    var::round_var(&mut v, scale);
    v
}

fn mul_round(a: &Var, b: &Var, scale: i32) -> Var {
    scaled(var::mul(a, b), scale)
}

/// `sqrt_var`: square root rounded to `rscale` decimal places.
#[allow(clippy::many_single_char_names)]
fn sqrt_var(a: &Var, rscale: i32) -> Result<Var, NumericError> {
    if a.is_zero() {
        return Ok(Var::zero(rscale));
    }
    let ws = (rscale + GUARD).max(0);
    // Initial guess from the leading digits.
    let d0 = f64::from(a.digits[0]);
    let d1 = a.digits.get(1).map_or(0.0, |&x| f64::from(x));
    let mut m = d0 + d1 / f64::from(NBASE);
    let mut w = a.weight;
    if w % 2 != 0 {
        m *= f64::from(NBASE);
        w -= 1;
    }
    let guess = Numeric::from_f64(m.sqrt());
    let mut x = match guess.var() {
        Some(v) if !v.is_zero() => v.clone(),
        _ => small(1),
    };
    x.weight += w / 2;
    x.neg = false;
    let two = small(2);
    for _ in 0..200 {
        let q = var::div(a, &x, ws, false)?;
        let s = var::add(&x, &q);
        let next = var::div(&s, &two, ws, false)?;
        let mut diff = var::sub(&next, &x);
        var::trunc_var(&mut diff, ws - 1);
        x = next;
        if diff.is_zero() {
            break;
        }
    }
    var::round_var(&mut x, rscale);
    Ok(x)
}

fn cmp_const(v: &Var, num: u128, den_pow10: i32) -> Ordering {
    // Compare with num / 10^den_pow10.
    let mut c = small(num);
    let p = var::div(&c, &small(10u128.pow(den_pow10 as u32)), den_pow10, false)
        .expect("nonzero divisor");
    c = p;
    var::cmp(v, &c)
}

/// `ln_var`: natural logarithm of a positive value, to `rscale` places.
fn ln_var(a: &Var, rscale: i32) -> Result<Var, NumericError> {
    // Reduce into 0.9 <= x <= 1.1 with square roots.
    let ws = rscale + 8;
    let mut x = a.clone();
    let mut fact = small(1);
    let two = small(2);
    while cmp_const(&x, 9, 1) == Ordering::Less || cmp_const(&x, 11, 1) == Ordering::Greater {
        // Each sqrt roughly halves the weight; adapt the working scale
        // (it may be negative, rounding left of the point).
        let local = rscale - x.weight * DEC_DIGITS / 2 + 8;
        x = sqrt_var(&x, local)?;
        fact = var::mul(&fact, &two);
    }
    let one = small(1);
    let num = var::sub(&x, &one);
    let den = var::add(&x, &one);
    let z = var::div(&num, &den, ws, true)?;
    let z2 = mul_round(&z, &z, ws);
    let mut term = z.clone();
    let mut result = z;
    let mut ni: u128 = 1;
    loop {
        ni += 2;
        term = mul_round(&term, &z2, ws);
        let elem = var::div(&term, &small(ni), ws, true)?;
        if elem.is_zero() {
            break;
        }
        result = var::add(&result, &elem);
    }
    let total = var::mul(&result, &var::mul(&fact, &two));
    Ok(scaled(total, rscale))
}

/// `estimate_ln_dweight`: decimal weight of ln(v), approximately.
fn estimate_ln_dweight(v: &Var) -> i32 {
    if v.neg {
        return 0;
    }
    if cmp_const(v, 9, 1) != Ordering::Less && cmp_const(v, 11, 1) != Ordering::Greater {
        let x = var::sub(v, &small(1));
        return match x.clone().stripped().digits.first() {
            Some(&d0) => {
                let x = x.stripped();
                x.weight * DEC_DIGITS + f64::from(d0).log10() as i32
            }
            None => 0,
        };
    }
    if v.digits.is_empty() {
        return 0;
    }
    let mut digits = i64::from(v.digits[0]);
    let mut dweight = v.weight * DEC_DIGITS;
    if v.digits.len() > 1 {
        digits = digits * i64::from(NBASE) + i64::from(v.digits[1]);
        dweight -= DEC_DIGITS;
    }
    let ln_var = (digits as f64).ln() + f64::from(dweight) * std::f64::consts::LN_10;
    ln_var.abs().log10() as i32
}

fn clamp_rscale(r: i32, dscales: &[i32]) -> i32 {
    dscales
        .iter()
        .fold(r, |acc, &d| acc.max(d))
        .clamp(0, MAX_DISPLAY_SCALE)
}

fn err_log(zero: bool) -> NumericError {
    NumericError::new(
        sqlstate::INVALID_ARGUMENT_FOR_LOG,
        if zero {
            "cannot take logarithm of zero"
        } else {
            "cannot take logarithm of a negative number"
        },
    )
}

/// `log_var`: logarithm of `num` in `base`.
fn log_var(base: &Var, num: &Var) -> Result<Var, NumericError> {
    let base_dw = estimate_ln_dweight(base);
    let num_dw = estimate_ln_dweight(num);
    let result_dw = num_dw - base_dw;
    let rscale = clamp_rscale(MIN_SIG_DIGITS - result_dw, &[base.dscale, num.dscale]);
    let base_rscale = (rscale + result_dw - base_dw + 8).max(0);
    let num_rscale = (rscale + result_dw - num_dw + 8).max(0);
    let ln_base = ln_var(base, base_rscale)?;
    let ln_num = ln_var(num, num_rscale)?;
    if ln_base.is_zero() {
        return Err(NumericError::division_by_zero());
    }
    var::div(&ln_num, &ln_base, rscale, true)
}

impl Numeric {
    /// `numeric_sqrt`.
    pub fn sqrt(&self) -> Result<Self, NumericError> {
        match self {
            Self::NaN => Ok(Self::NaN),
            Self::PosInf => Ok(Self::PosInf),
            Self::NegInf => Err(sqrt_negative()),
            Self::Finite(f) => {
                let a = &f.0;
                if a.neg && !a.is_zero() {
                    return Err(sqrt_negative());
                }
                let sweight = (a.weight + 1) * DEC_DIGITS / 2 - 1;
                let rscale = clamp_rscale(MIN_SIG_DIGITS - sweight, &[a.dscale]);
                Self::finish(sqrt_var(a, rscale)?)
            }
        }
    }

    /// Check the argument of a logarithm; `Ok(Some(v))` is a special result.
    fn log_arg(&self) -> Result<Option<Self>, NumericError> {
        match self {
            Self::NaN => Ok(Some(Self::NaN)),
            Self::PosInf => Ok(Some(Self::PosInf)),
            Self::NegInf => Err(err_log(false)),
            Self::Finite(f) => {
                if f.0.is_zero() {
                    Err(err_log(true))
                } else if f.0.neg {
                    Err(err_log(false))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// `numeric_ln`.
    pub fn ln(&self) -> Result<Self, NumericError> {
        if let Some(r) = self.log_arg()? {
            return Ok(r);
        }
        let a = self.var().expect("finite");
        let rscale = clamp_rscale(MIN_SIG_DIGITS - estimate_ln_dweight(a), &[a.dscale]);
        Self::finish(ln_var(a, rscale)?)
    }

    /// `numeric_log(base, x)`; `log10(x)` is `log(10, x)`.
    pub fn log(&self, x: &Self) -> Result<Self, NumericError> {
        if self.is_nan() || x.is_nan() {
            return Ok(Self::NaN);
        }
        // Validate both arguments (zero / negative errors first, as PG does).
        let b_special = self.log_arg()?;
        let x_special = x.log_arg()?;
        if b_special.is_some() || x_special.is_some() {
            // Infinite arguments: PG reports NaN-like results via float math;
            // keep the simple cases.
            return match (b_special, x_special) {
                (Some(_), Some(_)) => Ok(Self::NaN),
                (Some(_), None) => Ok(Self::zero()),
                (None, _) => Ok(Self::PosInf),
            };
        }
        let (b, v) = (self.var().expect("finite"), x.var().expect("finite"));
        Self::finish(log_var(b, v)?)
    }
}

fn sqrt_negative() -> NumericError {
    NumericError::new(
        sqlstate::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
        "cannot take square root of a negative number",
    )
}
