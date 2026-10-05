//! 型ごとのリテラル生成。`bad = true` なら範囲外・不正な書式の値を混ぜる（PG でエラーになる想定）。

use super::Ty;
use crate::rng::Rng;

pub fn int_lit(rng: &mut Rng) -> i64 {
    match rng.weighted(&[70, 10, 10, 10]) {
        0 => rng.range(0, 100),
        1 => rng.range(-100, -1),
        2 => *rng.pick(&[2147483647, -2147483648, 32767, -32768, 65535, 2147483648]),
        _ => rng.range(-5_000_000_000, 5_000_000_000),
    }
}

pub fn text_lit(rng: &mut Rng) -> String {
    const WORDS: [&str; 12] =
        ["", "a", "abc", "Hello", "foo bar", "O''Brien", "  pad ", "xyz", "日本語", "ABC", "100", "a%b"];
    let mut s = rng.pick(&WORDS).to_string();
    if rng.chance(15) {
        s.push_str(rng.pick(&WORDS));
    }
    format!("'{s}'")
}

pub fn num_lit(rng: &mut Rng) -> String {
    match rng.below(4) {
        0 => format!("{}", rng.range(-100, 100)),
        1 => format!("{}.{:02}", rng.range(-50, 50), rng.below(100)),
        2 => format!("{}.{}", rng.range(0, 9), rng.below(10)),
        _ => "0.00".into(),
    }
}

pub fn literal(rng: &mut Rng, ty: Ty, bad: bool) -> String {
    if !bad && rng.chance(8) {
        return "NULL".into();
    }
    match ty {
        Ty::Int2 | Ty::Int4 | Ty::Int8 => {
            if bad {
                return rng.pick(&["'abc'", "'1.5'", "2147483648", "true", "''"]).to_string();
            }
            let v = int_lit(rng);
            // 範囲外は bad のときだけ（ただし int4 の境界値などは時々そのまま通す）
            let (lo, hi) = match ty {
                Ty::Int2 => (i16::MIN as i64, i16::MAX as i64),
                Ty::Int4 => (i32::MIN as i64, i32::MAX as i64),
                _ => (i64::MIN, i64::MAX),
            };
            if v < lo || v > hi { rng.range(0, 100).to_string() } else if v < 0 { format!("({v})") } else { v.to_string() }
        }
        Ty::Numeric => {
            if bad {
                return rng.pick(&["'abc'", "123456789.12", "1e10", "'NaN'x", "true", "99999999.995", "100000000", "'1,5'"]).to_string();
            }
            if rng.chance(25) {
                return rng
                    .pick(&["1.005", "99999999.99", "'-0.005'", "0.004", "'1e2'", "'NaN'", "2.675", "(-99999999.99)"])
                    .to_string();
            }
            let s = num_lit(rng);
            if s.starts_with('-') { format!("({s})") } else { s }
        }
        Ty::NumU | Ty::Num51 => {
            if bad {
                return if ty == Ty::Num51 {
                    rng.pick(&["'abc'", "10000", "99999.95", "1e5", "'NaN'x", "true", "'1.2.3'", "''"]).to_string()
                } else {
                    rng.pick(&["'abc'", "true", "'1.2.3'", "''", "'1e'", "'--1'"]).to_string()
                };
            }
            rng.pick(&[
                "0", "1.05", "'-3.14159'", "9999.94", "'  12.5  '", "0.05", "(-0.04)", "'NaN'", "1e2", "'1e-2'",
                "123456789012345678901234567890", "99.95", "0.0",
            ])
            .to_string()
        }
        Ty::Float4 => {
            if bad {
                return rng.pick(&["'abc'", "'1e39'", "'1.2.3'", "true", "'1e-50'"]).to_string();
            }
            rng.pick(&["0", "'1.5'", "'-2.25'", "100", "'0.1'", "3", "'1e10'", "'16777217'", "'NaN'", "'-inf'", "'3.4e38'"])
                .to_string()
        }
        Ty::Float8 => {
            if bad {
                return rng.pick(&["'abc'", "'1.2.3'", "true"]).to_string();
            }
            rng.pick(&["0", "'1.5'", "'-2.25'", "100", "'0.125'", "3"]).to_string()
        }
        Ty::Text => {
            if bad {
                return rng.pick(&["'1'", "true"]).to_string();
            }
            text_lit(rng)
        }
        Ty::Varchar(n) => {
            if bad {
                return format!("'{}'", "x".repeat(n as usize + 1 + rng.below(3) as usize));
            }
            let w = text_lit(rng);
            if w.chars().count() - 2 > n as usize { "'a'".into() } else { w }
        }
        Ty::Bool => {
            if bad {
                return rng.pick(&["'maybe'", "2", "'abc'", "'1.5'"]).to_string();
            }
            rng.pick(&["true", "false", "'t'", "'f'", "'yes'", "'0'", "'on'"]).to_string()
        }
    }
}
