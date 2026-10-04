//! Finite arbitrary-precision values in PostgreSQL's base-10000 layout and
//! the arithmetic kernels (a port of the `NumericVar` routines of
//! PostgreSQL's `numeric.c`).
//!
//! Value = (-1)^neg * Σ digits[i] * NBASE^(weight - i).
//! `dscale` is the number of decimal digits displayed after the point.

// Digit and weight values are always range-checked by construction
// (digits are 0..NBASE, indexes are non-negative), so the narrowing casts
// below cannot lose information.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::cmp::Ordering;

use crate::error::NumericError;

pub(crate) const NBASE: i32 = 10000;
pub(crate) const HALF_NBASE: i32 = 5000;
pub(crate) const DEC_DIGITS: i32 = 4;
/// `round_powers[di]`: `10^(DEC_DIGITS - di)`.
const ROUND_POWERS: [i32; 4] = [0, 1000, 100, 10];

/// Maximum weight storable (`NUMERIC_WEIGHT_MAX`).
pub(crate) const NUMERIC_WEIGHT_MAX: i32 = i16::MAX as i32;
/// Minimum weight storable (`NUMERIC_WEIGHT_MIN`).
pub(crate) const NUMERIC_WEIGHT_MIN: i32 = i16::MIN as i32;
/// Maximum display scale storable (`NUMERIC_DSCALE_MAX`).
pub(crate) const NUMERIC_DSCALE_MAX: i32 = 0x3FFF;

/// A finite numeric value (sign, weight, display scale and base-10000 digits).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Var {
    pub(crate) neg: bool,
    pub(crate) weight: i32,
    pub(crate) dscale: i32,
    pub(crate) digits: Vec<i16>,
}

#[inline]
pub(crate) fn d(x: i32) -> i16 {
    debug_assert!((0..NBASE).contains(&x));
    x as i16
}

impl Var {
    pub(crate) fn zero(dscale: i32) -> Self {
        Self {
            neg: false,
            weight: 0,
            dscale,
            digits: Vec::new(),
        }
    }

    /// True for a (stripped) zero.
    pub(crate) fn is_zero(&self) -> bool {
        self.digits.iter().all(|&x| x == 0)
    }

    pub(crate) fn ndigits(&self) -> i32 {
        self.digits.len() as i32
    }

    /// Weight of the last stored digit.
    fn low_weight(&self) -> i32 {
        self.weight - self.ndigits() + 1
    }

    /// The digit at weight `w` (0 outside the stored range).
    fn digit_at(&self, w: i32) -> i32 {
        let idx = self.weight - w;
        if idx >= 0 && idx < self.ndigits() {
            i32::from(self.digits[idx as usize])
        } else {
            0
        }
    }

    /// `strip_var`: drop leading/trailing zero digits, normalise zero.
    pub(crate) fn strip(&mut self) {
        let lead = self.digits.iter().take_while(|&&x| x == 0).count();
        if lead == self.digits.len() {
            self.digits.clear();
            self.weight = 0;
            self.neg = false;
            return;
        }
        if lead > 0 {
            self.digits.drain(..lead);
            self.weight -= lead as i32;
        }
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
    }

    pub(crate) fn stripped(mut self) -> Self {
        self.strip();
        self
    }

    pub(crate) fn from_u128(mut v: u128, neg: bool) -> Self {
        let mut digits = Vec::new();
        while v > 0 {
            digits.push(d((v % 10000) as i32));
            v /= 10000;
        }
        digits.reverse();
        let weight = digits.len() as i32 - 1;
        Self {
            neg,
            weight,
            dscale: 0,
            digits,
        }
        .stripped()
    }
}

/// `cmp_abs`: compare absolute values.
pub(crate) fn cmp_abs(a: &Var, b: &Var) -> Ordering {
    let hi = a.weight.max(b.weight);
    let lo = a.low_weight().min(b.low_weight());
    let mut w = hi;
    while w >= lo {
        match a.digit_at(w).cmp(&b.digit_at(w)) {
            Ordering::Equal => w -= 1,
            o => return o,
        }
    }
    Ordering::Equal
}

/// `cmp_var`: signed comparison of finite values.
pub(crate) fn cmp(a: &Var, b: &Var) -> Ordering {
    let az = a.is_zero();
    let bz = b.is_zero();
    if az {
        if bz {
            return Ordering::Equal;
        }
        return if b.neg {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if bz {
        return if a.neg {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    match (a.neg, b.neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => cmp_abs(a, b),
        (true, true) => cmp_abs(b, a),
    }
}

/// |a| + |b|, result is non-negative.
fn add_abs(a: &Var, b: &Var) -> Var {
    let hi = a.weight.max(b.weight) + 1;
    let lo = a.low_weight().min(b.low_weight());
    let n = (hi - lo + 1).max(0) as usize;
    let mut digits = vec![0i16; n];
    let mut carry = 0;
    for (i, w) in (lo..=hi).enumerate() {
        let mut s = a.digit_at(w) + b.digit_at(w) + carry;
        if s >= NBASE {
            s -= NBASE;
            carry = 1;
        } else {
            carry = 0;
        }
        digits[n - 1 - i] = d(s);
    }
    Var {
        neg: false,
        weight: hi,
        dscale: a.dscale.max(b.dscale),
        digits,
    }
    .stripped()
}

/// |a| - |b|, requires |a| >= |b|; result is non-negative.
fn sub_abs(a: &Var, b: &Var) -> Var {
    let hi = a.weight.max(b.weight);
    let lo = a.low_weight().min(b.low_weight());
    let n = (hi - lo + 1).max(0) as usize;
    let mut digits = vec![0i16; n];
    let mut borrow = 0;
    for (i, w) in (lo..=hi).enumerate() {
        let mut s = a.digit_at(w) - b.digit_at(w) - borrow;
        if s < 0 {
            s += NBASE;
            borrow = 1;
        } else {
            borrow = 0;
        }
        digits[n - 1 - i] = d(s);
    }
    debug_assert_eq!(borrow, 0);
    Var {
        neg: false,
        weight: hi,
        dscale: a.dscale.max(b.dscale),
        digits,
    }
    .stripped()
}

/// `a + (-1)^b_neg * |b|`.
fn add_signed(a: &Var, b: &Var, b_neg: bool) -> Var {
    let dscale = a.dscale.max(b.dscale);
    let mut r = if a.neg == b_neg {
        let mut r = add_abs(a, b);
        r.neg = a.neg;
        r
    } else {
        match cmp_abs(a, b) {
            Ordering::Equal => Var::zero(dscale),
            Ordering::Greater => {
                let mut r = sub_abs(a, b);
                r.neg = a.neg;
                r
            }
            Ordering::Less => {
                let mut r = sub_abs(b, a);
                r.neg = b_neg;
                r
            }
        }
    };
    r.dscale = dscale;
    if r.digits.is_empty() {
        r.neg = false;
    }
    r
}

pub(crate) fn add(a: &Var, b: &Var) -> Var {
    add_signed(a, b, b.neg)
}

pub(crate) fn sub(a: &Var, b: &Var) -> Var {
    add_signed(a, b, !b.neg)
}

/// Exact product with `dscale = a.dscale + b.dscale` (capped by rounding at
/// `NUMERIC_DSCALE_MAX`, as `numeric_mul` does).
pub(crate) fn mul(a: &Var, b: &Var) -> Var {
    let dscale = a.dscale + b.dscale;
    let mut r = if a.is_zero() || b.is_zero() {
        Var::zero(dscale)
    } else {
        let n1 = a.digits.len();
        let n2 = b.digits.len();
        let mut acc = vec![0u64; n1 + n2];
        for (i, &x) in a.digits.iter().enumerate() {
            if x == 0 {
                continue;
            }
            let x = x as u64;
            for (j, &y) in b.digits.iter().enumerate() {
                acc[i + j + 1] += x * y as u64;
            }
        }
        let mut digits = vec![0i16; n1 + n2];
        let mut carry = 0u64;
        for k in (0..n1 + n2).rev() {
            let v = acc[k] + carry;
            digits[k] = d((v % 10000) as i32);
            carry = v / 10000;
        }
        debug_assert_eq!(carry, 0);
        Var {
            neg: a.neg != b.neg,
            weight: a.weight + b.weight + 1,
            dscale,
            digits,
        }
        .stripped()
    };
    r.dscale = dscale;
    if r.dscale > NUMERIC_DSCALE_MAX {
        round_var(&mut r, NUMERIC_DSCALE_MAX);
    }
    r
}

/// Long division of big-endian base-NBASE magnitudes (Knuth algorithm D).
/// Returns floor(u / v). `v` must be non-zero.
#[allow(clippy::many_single_char_names)] // Knuth's notation
fn div_limbs(u: &[i64], v: &[i64]) -> Vec<i64> {
    let b = i64::from(NBASE);
    let u = &u[u.iter().take_while(|&&x| x == 0).count()..];
    let v = &v[v.iter().take_while(|&&x| x == 0).count()..];
    assert!(!v.is_empty(), "division by zero magnitude");
    if u.len() < v.len() {
        return Vec::new();
    }
    let n = v.len();
    if n == 1 {
        let dv = v[0];
        let mut q = Vec::with_capacity(u.len());
        let mut rem = 0i64;
        for &x in u {
            let cur = rem * b + x;
            q.push(cur / dv);
            rem = cur % dv;
        }
        return q;
    }
    let m = u.len() - n;
    let norm = b / (v[0] + 1);
    // Normalised dividend with one extra leading limb.
    let mut un = vec![0i64; u.len() + 1];
    let mut carry = 0i64;
    for i in (0..u.len()).rev() {
        let t = u[i] * norm + carry;
        un[i + 1] = t % b;
        carry = t / b;
    }
    un[0] = carry;
    let mut vn = vec![0i64; n];
    carry = 0;
    for i in (0..n).rev() {
        let t = v[i] * norm + carry;
        vn[i] = t % b;
        carry = t / b;
    }
    debug_assert_eq!(carry, 0);
    let mut q = vec![0i64; m + 1];
    for j in 0..=m {
        let num = un[j] * b + un[j + 1];
        let mut qhat = num / vn[0];
        let mut rhat = num % vn[0];
        while qhat >= b || qhat * vn[1] > rhat * b + un[j + 2] {
            qhat -= 1;
            rhat += vn[0];
            if rhat >= b {
                break;
            }
        }
        // Multiply and subtract.
        let mut borrow = 0i64;
        let mut mcarry = 0i64;
        for i in (0..n).rev() {
            let p = qhat * vn[i] + mcarry;
            mcarry = p / b;
            let mut s = un[j + 1 + i] - p % b - borrow;
            if s < 0 {
                s += b;
                borrow = 1;
            } else {
                borrow = 0;
            }
            un[j + 1 + i] = s;
        }
        let s = un[j] - mcarry - borrow;
        if s < 0 {
            // Add back.
            qhat -= 1;
            let mut c = 0i64;
            for i in (0..n).rev() {
                let mut t = un[j + 1 + i] + vn[i] + c;
                if t >= b {
                    t -= b;
                    c = 1;
                } else {
                    c = 0;
                }
                un[j + 1 + i] = t;
            }
            un[j] = s + c;
            debug_assert_eq!(un[j], 0);
        } else {
            un[j] = s;
        }
        q[j] = qhat;
    }
    q
}

/// |a| / |b| truncated to `frac` base-NBASE fractional digits.
#[allow(clippy::many_single_char_names)]
fn div_abs_trunc(a: &Var, b: &Var, frac: i32) -> Var {
    if a.is_zero() {
        return Var::zero(0);
    }
    let k = a.low_weight() - b.low_weight() + frac;
    let mut u: Vec<i64> = a.digits.iter().map(|&x| i64::from(x)).collect();
    let mut v: Vec<i64> = b.digits.iter().map(|&x| i64::from(x)).collect();
    if k >= 0 {
        u.resize(u.len() + k as usize, 0);
    } else {
        v.resize(v.len() + (-k) as usize, 0);
    }
    let q = div_limbs(&u, &v);
    let weight = q.len() as i32 - 1 - frac;
    Var {
        neg: false,
        weight,
        dscale: 0,
        digits: q.into_iter().map(|x| d(x as i32)).collect(),
    }
    .stripped()
}

/// `div_var`: a / b computed exactly to `rscale` decimal places, then
/// rounded half away from zero (`round`) or truncated.
pub(crate) fn div(a: &Var, b: &Var, rscale: i32, round: bool) -> Result<Var, NumericError> {
    if b.is_zero() {
        return Err(NumericError::division_by_zero());
    }
    let frac = rscale.div_euclid(DEC_DIGITS) + 1;
    let mut q = div_abs_trunc(a, b, frac);
    q.neg = a.neg != b.neg;
    if round {
        round_var(&mut q, rscale);
    } else {
        trunc_var(&mut q, rscale);
    }
    Ok(q)
}

/// `mod_var`: a - trunc(a / b) * b.
pub(crate) fn modulo(a: &Var, b: &Var) -> Result<Var, NumericError> {
    let q = div(a, b, 0, false)?;
    let t = mul(b, &q);
    Ok(sub(a, &t))
}

/// `select_div_scale`: result scale for `/`.
pub(crate) fn select_div_scale(a: &Var, b: &Var) -> i32 {
    const NUMERIC_MIN_SIG_DIGITS: i32 = 16;
    const NUMERIC_MAX_DISPLAY_SCALE: i32 = 1000;
    let first = |v: &Var| -> (i32, i32) {
        v.digits
            .iter()
            .enumerate()
            .find(|&(_, &x)| x != 0)
            .map_or((0, 0), |(i, &x)| (v.weight - i as i32, i32::from(x)))
    };
    let (w1, f1) = first(a);
    let (w2, f2) = first(b);
    let mut qweight = w1 - w2;
    if f1 <= f2 {
        qweight -= 1;
    }
    let rscale = NUMERIC_MIN_SIG_DIGITS - qweight * DEC_DIGITS;
    rscale
        .max(a.dscale)
        .max(b.dscale)
        .clamp(0, NUMERIC_MAX_DISPLAY_SCALE)
}

/// `round_var`: round half away from zero to `rscale` decimal places
/// (`rscale` may be negative) and set `dscale = rscale`.
pub(crate) fn round_var(v: &mut Var, rscale: i32) {
    v.dscale = rscale;
    let di = (v.weight + 1) * DEC_DIGITS + rscale;
    if di < 0 {
        v.digits.clear();
        v.weight = 0;
        v.neg = false;
        return;
    }
    let mut ndigits = ((di + DEC_DIGITS - 1) / DEC_DIGITS) as usize;
    let di = di % DEC_DIGITS;
    if ndigits < v.digits.len() || (ndigits == v.digits.len() && di > 0) {
        let mut carry;
        if di == 0 {
            carry = i32::from(i32::from(v.digits[ndigits]) >= HALF_NBASE);
            v.digits.truncate(ndigits);
        } else {
            v.digits.truncate(ndigits);
            ndigits -= 1;
            let pow10 = ROUND_POWERS[di as usize];
            let cur = i32::from(v.digits[ndigits]);
            let extra = cur % pow10;
            let mut nd = cur - extra;
            carry = 0;
            if extra >= pow10 / 2 {
                nd += pow10;
                if nd >= NBASE {
                    nd -= NBASE;
                    carry = 1;
                }
            }
            v.digits[ndigits] = d(nd);
        }
        let mut idx = ndigits;
        while carry != 0 {
            if idx == 0 {
                v.digits.insert(0, 1);
                v.weight += 1;
                break;
            }
            idx -= 1;
            let s = i32::from(v.digits[idx]) + carry;
            if s >= NBASE {
                v.digits[idx] = d(s - NBASE);
                carry = 1;
            } else {
                v.digits[idx] = d(s);
                carry = 0;
            }
        }
    }
    v.strip();
}

/// `trunc_var`: truncate toward zero to `rscale` decimal places.
pub(crate) fn trunc_var(v: &mut Var, rscale: i32) {
    v.dscale = rscale;
    let di = (v.weight + 1) * DEC_DIGITS + rscale;
    if di <= 0 {
        v.digits.clear();
        v.weight = 0;
        v.neg = false;
        return;
    }
    let ndigits = ((di + DEC_DIGITS - 1) / DEC_DIGITS) as usize;
    if ndigits <= v.digits.len() {
        v.digits.truncate(ndigits);
        let di = di % DEC_DIGITS;
        if di > 0 {
            let pow10 = ROUND_POWERS[di as usize];
            let cur = i32::from(v.digits[ndigits - 1]);
            v.digits[ndigits - 1] = d(cur - cur % pow10);
        }
    }
    v.strip();
}

/// `ceil_var` (`ceil = true`) / `floor_var`.
pub(crate) fn ceil_floor(v: &Var, ceil: bool) -> Var {
    let mut t = v.clone();
    trunc_var(&mut t, 0);
    let one = Var::from_u128(1, false);
    if cmp(v, &t) != Ordering::Equal {
        if ceil && !v.neg {
            t = add(&t, &one);
        } else if !ceil && v.neg {
            t = sub(&t, &one);
        }
    }
    t
}

/// Decimal digits before the point of a normalised var, used by typmod
/// checks (`apply_typmod`): returns the number of decimal digits up to and
/// including the most significant non-zero one, counted from the point
/// (negative for values below 1), or `None` for zero.
pub(crate) fn int_decimal_digits(v: &Var) -> Option<i32> {
    let mut ddigits = (v.weight + 1) * DEC_DIGITS;
    for &dig in &v.digits {
        if dig != 0 {
            if dig < 10 {
                ddigits -= 3;
            } else if dig < 100 {
                ddigits -= 2;
            } else if dig < 1000 {
                ddigits -= 1;
            }
            return Some(ddigits);
        }
        ddigits -= DEC_DIGITS;
    }
    None
}
