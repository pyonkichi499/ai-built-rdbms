//! C library number parsing (`strtol`, `strtod`, `atoi`) with the exact
//! acceptance rules PostgreSQL's date/time parser relies on.

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strtol`/`strtoll` in base 10. Returns `(value, consumed, overflow)`.
/// When no digits are found, `consumed` is 0 (the C `endptr == str` case).
pub(crate) fn strtol(s: &[u8]) -> (i64, usize, bool) {
    let mut p = 0;
    while p < s.len() && is_c_space(s[p]) {
        p += 1;
    }
    let mut neg = false;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        neg = s[p] == b'-';
        p += 1;
    }
    let digits_start = p;
    let mut acc: i128 = 0;
    let mut overflow = false;
    while p < s.len() && s[p].is_ascii_digit() {
        if !overflow {
            acc = acc * 10 + i128::from(s[p] - b'0');
            if acc > i128::from(i64::MAX) + 1 {
                overflow = true;
            }
        }
        p += 1;
    }
    if p == digits_start {
        return (0, 0, false);
    }
    let v = if neg { -acc } else { acc };
    if overflow || v > i128::from(i64::MAX) || v < i128::from(i64::MIN) {
        return (if neg { i64::MIN } else { i64::MAX }, p, true);
    }
    #[allow(clippy::cast_possible_truncation)]
    (v as i64, p, false)
}

/// PostgreSQL's `strtoint`: `strtol` plus an `int` range check.
pub(crate) fn strtoint(s: &[u8]) -> (i32, usize, bool) {
    let (v, n, of) = strtol(s);
    #[allow(clippy::cast_possible_truncation)]
    let iv = v as i32;
    (iv, n, of || i64::from(iv) != v)
}

/// `atoi`: `(int) strtol(s, NULL, 10)`.
pub(crate) fn atoi(s: &[u8]) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let v = strtol(s).0 as i32;
    v
}

/// `strtod`. Returns `(value, consumed, erange)`; `consumed == 0` when no
/// conversion could be performed.
pub(crate) fn strtod(s: &[u8]) -> (f64, usize, bool) {
    let mut p = 0;
    while p < s.len() && is_c_space(s[p]) {
        p += 1;
    }
    let start = p;
    let mut neg = false;
    if p < s.len() && (s[p] == b'+' || s[p] == b'-') {
        neg = s[p] == b'-';
        p += 1;
    }
    let rest = &s[p..];
    let lower: Vec<u8> = rest.iter().take(8).map(u8::to_ascii_lowercase).collect();
    if lower.starts_with(b"inf") {
        let n = if lower.starts_with(b"infinity") { 8 } else { 3 };
        let v = if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return (v, p + n, false);
    }
    if lower.starts_with(b"nan") {
        let mut n = 3;
        if rest.get(3) == Some(&b'(') {
            let mut q = 4;
            while q < rest.len() && (rest[q].is_ascii_alphanumeric() || rest[q] == b'_') {
                q += 1;
            }
            if rest.get(q) == Some(&b')') {
                n = q + 1;
            }
        }
        return (f64::NAN, p + n, false);
    }
    if rest.len() >= 2
        && rest[0] == b'0'
        && (rest[1] == b'x' || rest[1] == b'X')
        && let Some((v, n)) = parse_hex_float(&rest[2..])
    {
        let v = if neg { -v } else { v };
        return (v, p + 2 + n, v.is_infinite());
    }
    // Decimal: digits [. digits] [e [sign] digits]
    let mut q = p;
    let mut ndigits = 0;
    while q < s.len() && s[q].is_ascii_digit() {
        q += 1;
        ndigits += 1;
    }
    if q < s.len() && s[q] == b'.' {
        q += 1;
        while q < s.len() && s[q].is_ascii_digit() {
            q += 1;
            ndigits += 1;
        }
    }
    if ndigits == 0 {
        return (0.0, 0, false);
    }
    if q < s.len() && (s[q] == b'e' || s[q] == b'E') {
        let mut r = q + 1;
        if r < s.len() && (s[r] == b'+' || s[r] == b'-') {
            r += 1;
        }
        let exp_start = r;
        while r < s.len() && s[r].is_ascii_digit() {
            r += 1;
        }
        if r > exp_start {
            q = r;
        }
    }
    let text = std::str::from_utf8(&s[start..q]).unwrap_or("0");
    let text = text.strip_prefix('+').unwrap_or(text);
    let mut norm = text.to_owned();
    if norm.ends_with('.') {
        norm.push('0');
    }
    if norm.starts_with('.') {
        norm.insert(0, '0');
    } else if norm.starts_with("-.") {
        norm.insert(1, '0');
    }
    let v: f64 = norm.parse().unwrap_or(0.0);
    let has_nonzero_digit = s[p..q]
        .iter()
        .take_while(|c| **c != b'e' && **c != b'E')
        .any(|c| (b'1'..=b'9').contains(c));
    let erange = v.is_infinite() || (v == 0.0 && has_nonzero_digit) || is_subnormal(v);
    (v, q, erange)
}

fn is_subnormal(v: f64) -> bool {
    v != 0.0 && v.is_finite() && v.abs() < f64::MIN_POSITIVE
}

fn parse_hex_float(s: &[u8]) -> Option<(f64, usize)> {
    let mut q = 0;
    let mut mant: f64 = 0.0;
    let mut ndigits = 0;
    let mut exp: i32 = 0;
    while q < s.len() && s[q].is_ascii_hexdigit() {
        mant = mant * 16.0 + f64::from(hexval(s[q]));
        q += 1;
        ndigits += 1;
    }
    if q < s.len() && s[q] == b'.' {
        q += 1;
        while q < s.len() && s[q].is_ascii_hexdigit() {
            mant = mant * 16.0 + f64::from(hexval(s[q]));
            exp -= 4;
            q += 1;
            ndigits += 1;
        }
    }
    if ndigits == 0 {
        return None;
    }
    if q < s.len() && (s[q] == b'p' || s[q] == b'P') {
        let (e, n, _) = strtol(&s[q + 1..]);
        let first = s.get(q + 1).copied().unwrap_or(0);
        if n > 0 && (first.is_ascii_digit() || first == b'+' || first == b'-') {
            #[allow(clippy::cast_possible_truncation)]
            {
                exp = exp.saturating_add(e.clamp(-100_000, 100_000) as i32);
            }
            q += 1 + n;
        }
    }
    Some((mant * 2f64.powi(exp), q))
}

fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// C `rint` (round half to even) followed by a saturating cast.
pub(crate) fn rint(v: f64) -> f64 {
    v.round_ties_even()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtol_basics() {
        assert_eq!(strtol(b"123x"), (123, 3, false));
        assert_eq!(strtol(b"-5"), (-5, 2, false));
        assert_eq!(strtol(b"x"), (0, 0, false));
        assert!(strtol(b"99999999999999999999").2);
        assert!(strtoint(b"3000000000").2);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn strtod_basics() {
        assert_eq!(strtod(b".5"), (0.5, 2, false));
        assert_eq!(strtod(b"1e2D").0, 100.0);
        assert_eq!(strtod(b"1e2D").1, 3);
        assert_eq!(strtod(b"5.").1, 2);
        assert_eq!(strtod(b".").1, 0);
        assert_eq!(strtod(b"0x10").0, 16.0);
        assert!(strtod(b"-inf").0.is_infinite());
    }
}
