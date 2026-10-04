//! Shortest decimal digits of a binary float, matching PostgreSQL's Ryu
//! port (`src/common/d2s.c`, `f2s.c`) exactly:
//!
//! - Integers in `[1, 2^mantissa_bits+1)` with no fractional bits take the
//!   "small int" fast path and print their exact value (`d2d_small_int`).
//! - Otherwise the shortest digit string strictly inside the rounding
//!   interval is chosen (PostgreSQL builds Ryu with `STRICTLY_SHORTEST = 0`,
//!   i.e. `acceptBounds = false`: the interval bounds are excluded even for
//!   even mantissas). Among candidates of that length the one closest to
//!   the exact value wins; exact ties round to even.
//!
//! Implemented with the Steele-White / Burger-Dybvig free-format algorithm
//! on a tiny bignum instead of Ryu's lookup tables.

// Bit-level float decoding and limb arithmetic: the casts are intentional.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_lines
)]

use std::cmp::Ordering;

/// Arbitrary-precision unsigned integer (little-endian 32-bit limbs).
#[derive(Clone, Debug)]
struct Big(Vec<u32>);

impl Big {
    fn from_u64(v: u64) -> Big {
        let mut b = Big(vec![(v & 0xffff_ffff) as u32, (v >> 32) as u32]);
        b.trim();
        b
    }

    fn trim(&mut self) {
        while self.0.len() > 1 && *self.0.last().unwrap_or(&1) == 0 {
            self.0.pop();
        }
    }

    fn mul_small(&mut self, m: u32) {
        let mut carry = 0u64;
        for limb in &mut self.0 {
            let v = u64::from(*limb) * u64::from(m) + carry;
            *limb = (v & 0xffff_ffff) as u32;
            carry = v >> 32;
        }
        if carry > 0 {
            self.0.push(carry as u32);
        }
    }

    fn mul_pow10(&mut self, n: u32) {
        for _ in 0..n {
            self.mul_small(10);
        }
    }

    fn shl(&mut self, bits: u32) {
        let limbs = (bits / 32) as usize;
        let rem = bits % 32;
        if rem > 0 {
            let mut carry = 0u32;
            for limb in &mut self.0 {
                let v = (*limb << rem) | carry;
                carry = *limb >> (32 - rem);
                *limb = v;
            }
            if carry > 0 {
                self.0.push(carry);
            }
        }
        if limbs > 0 {
            let mut v = vec![0u32; limbs];
            v.extend_from_slice(&self.0);
            self.0 = v;
        }
        self.trim();
    }

    fn add(&self, other: &Big) -> Big {
        let n = self.0.len().max(other.0.len());
        let mut out = Vec::with_capacity(n + 1);
        let mut carry = 0u64;
        for i in 0..n {
            let v = u64::from(*self.0.get(i).unwrap_or(&0))
                + u64::from(*other.0.get(i).unwrap_or(&0))
                + carry;
            out.push((v & 0xffff_ffff) as u32);
            carry = v >> 32;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        let mut b = Big(out);
        b.trim();
        b
    }

    /// `self -= other`; requires `self >= other`.
    fn sub_assign(&mut self, other: &Big) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let mut v = i64::from(self.0[i]) - i64::from(*other.0.get(i).unwrap_or(&0)) - borrow;
            if v < 0 {
                v += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            self.0[i] = v as u32;
        }
        debug_assert_eq!(borrow, 0);
        self.trim();
    }

    fn cmp(&self, other: &Big) -> Ordering {
        let a = self.0.len();
        let b = other.0.len();
        if a != b {
            return a.cmp(&b);
        }
        for i in (0..a).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

/// Shortest digits for `m2 * 2^e2` (`m2 > 0`). Returns the decimal digits
/// (no leading/trailing zeros) and the scientific exponent of the first
/// digit. `unequal_gaps` is true when the value is a power of two with a
/// normal exponent above the minimum (the gap below is half the gap above).
pub(super) fn shortest_digits(
    m2: u64,
    e2: i32,
    mantissa_bits: u32,
    unequal_gaps: bool,
) -> (Vec<u8>, i32) {
    // Small-integer fast path (exact value, trailing zeros stripped).
    if e2 <= 0 && e2 >= -(mantissa_bits as i32) && m2 >> mantissa_bits == 1 {
        let shift = (-e2) as u32;
        if m2 & ((1u64 << shift) - 1) == 0 {
            let int = m2 >> shift;
            let mut digits: Vec<u8> = int.to_string().bytes().map(|b| b - b'0').collect();
            let exp = digits.len() as i32 - 1;
            while digits.len() > 1 && digits.last() == Some(&0) {
                digits.pop();
            }
            return (digits, exp);
        }
    }

    // Burger & Dybvig setup: v = r/s, upper half-gap = mp/s, lower = mm/s.
    let (mut r, mut s, mut mp, mut mm);
    if e2 >= 0 {
        let e = e2 as u32;
        let mut be = Big::from_u64(1);
        be.shl(e);
        if unequal_gaps {
            r = Big::from_u64(m2);
            r.shl(e + 2);
            s = Big::from_u64(4);
            mp = be.clone();
            mp.shl(1);
            mm = be;
        } else {
            r = Big::from_u64(m2);
            r.shl(e + 1);
            s = Big::from_u64(2);
            mp = be.clone();
            mm = be;
        }
    } else {
        let ne = (-e2) as u32;
        if unequal_gaps {
            r = Big::from_u64(m2);
            r.shl(2);
            s = Big::from_u64(1);
            s.shl(ne + 2);
            mp = Big::from_u64(2);
            mm = Big::from_u64(1);
        } else {
            r = Big::from_u64(m2);
            r.shl(1);
            s = Big::from_u64(1);
            s.shl(ne + 1);
            mp = Big::from_u64(1);
            mm = Big::from_u64(1);
        }
    }

    // Estimate k = ceil(log10(v)), then fix up so that (r + mp) / s <= 1
    // (the exclusive upper bound may equal 10^k) and 10 * (r + mp) / s > 1.
    #[allow(clippy::cast_precision_loss)]
    let approx = (m2 as f64).log10() + f64::from(e2) * std::f64::consts::LOG10_2;
    let mut k = approx.ceil() as i32;
    if k >= 0 {
        s.mul_pow10(k as u32);
    } else {
        let n = (-k) as u32;
        r.mul_pow10(n);
        mp.mul_pow10(n);
        mm.mul_pow10(n);
    }
    loop {
        if r.add(&mp).cmp(&s) == Ordering::Greater {
            s.mul_small(10);
            k += 1;
            continue;
        }
        let mut high10 = r.add(&mp);
        high10.mul_small(10);
        if high10.cmp(&s) != Ordering::Greater {
            r.mul_small(10);
            mp.mul_small(10);
            mm.mul_small(10);
            k -= 1;
            continue;
        }
        break;
    }

    // Digit generation with exclusive bounds.
    let mut digits: Vec<u8> = Vec::with_capacity(20);
    loop {
        r.mul_small(10);
        mp.mul_small(10);
        mm.mul_small(10);
        let mut d = 0u8;
        while r.cmp(&s) != Ordering::Less {
            r.sub_assign(&s);
            d += 1;
        }
        let low_ok = r.cmp(&mm) == Ordering::Less;
        let high_ok = r.add(&mp).cmp(&s) == Ordering::Greater;
        match (low_ok, high_ok) {
            (false, false) => digits.push(d),
            (true, false) => {
                digits.push(d);
                break;
            }
            (false, true) => {
                digits.push(d + 1);
                break;
            }
            (true, true) => {
                let mut twice = r.clone();
                twice.shl(1);
                let up = match twice.cmp(&s) {
                    Ordering::Less => false,
                    Ordering::Greater => true,
                    Ordering::Equal => d % 2 == 1,
                };
                digits.push(if up { d + 1 } else { d });
                break;
            }
        }
    }

    // Propagate a carry out of a rounded-up 9 (defensive; should not occur).
    let mut i = digits.len();
    while i > 0 && digits[i - 1] >= 10 {
        digits[i - 1] -= 10;
        if i == 1 {
            digits.insert(0, 1);
            k += 1;
        } else {
            digits[i - 2] += 1;
        }
        i -= 1;
    }
    while digits.len() > 1 && digits.last() == Some(&0) {
        digits.pop();
    }
    (digits, k - 1)
}

/// Decodes a finite, non-zero f64 and returns its shortest digits.
pub(super) fn f64_digits(v: f64) -> (Vec<u8>, i32) {
    let bits = v.to_bits();
    let mant = bits & ((1u64 << 52) - 1);
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let (m2, e2) = if exp == 0 {
        (mant, 1 - 1023 - 52)
    } else {
        (mant | (1u64 << 52), exp - 1023 - 52)
    };
    shortest_digits(m2, e2, 52, mant == 0 && exp > 1)
}

/// Decodes a finite, non-zero f32 and returns its shortest digits.
pub(super) fn f32_digits(v: f32) -> (Vec<u8>, i32) {
    let bits = v.to_bits();
    let mant = u64::from(bits & ((1u32 << 23) - 1));
    let exp = ((bits >> 23) & 0xff) as i32;
    let (m2, e2) = if exp == 0 {
        (mant, 1 - 127 - 23)
    } else {
        (mant | (1u64 << 23), exp - 127 - 23)
    };
    shortest_digits(m2, e2, 23, mant == 0 && exp > 1)
}

/// C's `%.*g` (what `float8out` / `float4out` use when
/// `extra_float_digits <= 0`), with PostgreSQL's spellings of the special
/// values. `precision` below 1 is treated as 1.
pub(super) fn format_g(v: f64, precision: i32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let precision = precision.max(1);
    let p = usize::try_from(precision).unwrap_or(1);
    let e_form = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = e_form.split_once('e').unwrap_or((&e_form, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            s.to_owned()
        }
    };
    if exp < -4 || exp >= precision {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mantissa), exp.abs())
    } else {
        let decimals = usize::try_from(precision - 1 - exp).unwrap_or(0);
        strip(&format!("{v:.decimals$}"))
    }
}

#[cfg(test)]
mod format_g_tests {
    use super::format_g;

    #[test]
    fn matches_c_printf_g() {
        assert_eq!(format_g(1e15, 15), "1e+15");
        assert_eq!(format_g(1e14, 15), "100000000000000");
        assert_eq!(format_g(1.0 / 3.0, 15), "0.333333333333333");
        assert_eq!(format_g(1e-5, 15), "1e-05");
        assert_eq!(format_g(1e-4, 15), "0.0001");
        assert_eq!(format_g(99999.95, 5), "1e+05");
        assert_eq!(format_g(3.25, 0), "3");
        assert_eq!(format_g(-0.0, 15), "-0");
        assert_eq!(format_g(f64::NAN, 15), "NaN");
        assert_eq!(format_g(f64::NEG_INFINITY, 15), "-Infinity");
    }
}
