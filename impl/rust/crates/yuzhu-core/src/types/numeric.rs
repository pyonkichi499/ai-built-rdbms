//! `numeric` と `yuzhu-numeric` の橋渡し（ディスク形式・typmod・リテラル・追加の関数）。担当 T1。
//!
//! 演算子・キャスト・基本の関数の本体は `types/ops.rs` の numeric 区画（`numeric_add`、
//! `int_to_numeric` など）にあり、ここにはそれ以外の橋渡しを置く（`m4/09-types-functions.md` §3）。
//! `NumericError` から `Error` への変換は `error.rs` の `From` 実装（SQLSTATE・文言・DETAIL を
//! そのまま写す）。

use yuzhu_numeric::Numeric;

use crate::error::{Error, Result};
use crate::types::Datum;

/// `numeric` の `sign` フィールド（ディスク形式。PostgreSQL の `NUMERIC_POS` などと同じ値）。
const SIGN_POS: u16 = 0x0000;
const SIGN_NEG: u16 = 0x4000;
const SIGN_NAN: u16 = 0xC000;
const SIGN_PINF: u16 = 0xD000;
const SIGN_NINF: u16 = 0xF000;

/// 固定ヘッダ（`ndigits` `weight` `sign` `dscale`、各 2 バイト）の大きさ。
const HEADER_LEN: usize = 8;

/// `numeric` のディスク上のペイロードを `out` に追記する（varlena ヘッダは呼び出し側）。
///
/// 形式（すべて LE）: `ndigits: u16`、`weight: i16`、`sign: u16`、`dscale: u16`、`digits: [u16; ndigits]`。
/// 0 と特殊値は `ndigits = 0`・`weight = 0`。
pub fn encode_numeric(n: &Numeric, out: &mut Vec<u8>) {
    let (weight, sign, dscale, digits): (i16, u16, u16, &[i16]) = match n {
        Numeric::NaN => (0, SIGN_NAN, 0, &[]),
        Numeric::PosInf => (0, SIGN_PINF, 0, &[]),
        Numeric::NegInf => (0, SIGN_NINF, 0, &[]),
        Numeric::Finite(f) => {
            let digits = f.digits();
            let weight = if digits.is_empty() { 0 } else { f.weight() };
            let sign = if f.is_negative() { SIGN_NEG } else { SIGN_POS };
            (weight, sign, f.dscale(), digits)
        }
    };
    out.reserve(HEADER_LEN + digits.len() * 2);
    // 桁数は weight の範囲（i16）で収まるので u16 に必ず入る。
    let ndigits = u16::try_from(digits.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&ndigits.to_le_bytes());
    out.extend_from_slice(&weight.to_le_bytes());
    out.extend_from_slice(&sign.to_le_bytes());
    out.extend_from_slice(&dscale.to_le_bytes());
    for d in digits {
        out.extend_from_slice(&d.to_le_bytes());
    }
}

/// `encode_numeric` の逆。壊れていたら `XX001`（`Numeric::from_parts` の `22P03` は写し替える）。
pub fn decode_numeric(b: &[u8]) -> Result<Numeric> {
    let bad = |what: &str| Error::corrupted(format!("invalid stored numeric value: {what}"));
    if b.len() < HEADER_LEN {
        return Err(bad("too short"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let ndigits = usize::from(u16_at(0));
    let weight = i16::from_le_bytes([b[2], b[3]]);
    let sign = u16_at(4);
    let dscale = u16_at(6);
    if b.len() != HEADER_LEN + ndigits * 2 {
        return Err(bad("length does not match ndigits"));
    }
    match sign {
        SIGN_NAN | SIGN_PINF | SIGN_NINF => {
            if ndigits != 0 || weight != 0 || dscale != 0 {
                return Err(bad("special value with digits"));
            }
            return Ok(match sign {
                SIGN_NAN => Numeric::NaN,
                SIGN_PINF => Numeric::PosInf,
                _ => Numeric::NegInf,
            });
        }
        SIGN_POS | SIGN_NEG => {}
        _ => return Err(bad("invalid sign")),
    }
    let digits: Vec<i16> = (0..ndigits)
        .map(|i| i16::from_le_bytes([b[HEADER_LEN + 2 * i], b[HEADER_LEN + 2 * i + 1]]))
        .collect();
    Numeric::from_parts(sign == SIGN_NEG, weight, dscale, &digits).map_err(|e| bad(e.message()))
}

/// `numeric(p,s)` 列への長さ強制（`numeric_support` / `numeric()` の `apply_typmod`）。
/// `typmod = -1` なら何もしない。範囲外は `22003 numeric field overflow`（DETAIL 付き）。
pub fn apply_numeric_typmod(n: &Numeric, typmod: i32) -> Result<Numeric> {
    if typmod < 0 {
        return Ok(n.clone());
    }
    Ok(n.apply_typmod(typmod)?)
}

/// 文字列を `numeric_in` と同じ規則で読む（`typmod` は `-1` で無指定）。
pub fn parse_numeric(s: &str, typmod: i32) -> Result<Numeric> {
    Ok(Numeric::parse_with_typmod(s, typmod)?)
}

/// 数値リテラル（`1.5`、`.5`、`1.`、`1e3`、`1.0e-3`、`i64` に収まらない整数）を numeric にする
/// （アナライザの `transform_literal`。`m4/09-types-functions.md` §6.1）。
/// `1e3` は `1000`、`1.0e-3` は `0.0010`。範囲外は `22003 value overflows numeric format`。
pub fn numeric_literal(text: &str) -> Result<Datum> {
    Ok(Datum::Numeric(Numeric::parse(text)?))
}

fn numeric_arg(args: &[Datum], i: usize) -> Result<&Numeric> {
    match args.get(i) {
        Some(Datum::Numeric(n)) => Ok(n),
        _ => Err(Error::internal("numeric function: bad argument")),
    }
}

/// `scale(numeric)`（OID 3281）。NaN と Infinity は NULL。
pub fn numeric_scale(args: &[Datum]) -> Result<Datum> {
    Ok(numeric_arg(args, 0)?
        .scale()
        .map_or(Datum::Null, Datum::Int4))
}

/// `div(numeric, numeric)`（OID 1973）: 0 に向かって丸めた整数商。
pub fn numeric_div_trunc(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Numeric(
        numeric_arg(args, 0)?.div_trunc(numeric_arg(args, 1)?)?,
    ))
}

// ---------------------------------------------------------------------------
// M5 TY-2: 穴埋めの関数（gcd lcm min_scale trim_scale width_bucket factorial pow）とバイナリの橋渡し
// ---------------------------------------------------------------------------

fn num_at(args: &[Datum], i: usize) -> Result<&Numeric> {
    numeric_arg(args, i)
}

/// ユークリッドの互除法（引数は有限）。
fn gcd_finite(a: &Numeric, b: &Numeric) -> Result<Numeric> {
    let (mut a, mut b) = (a.abs(), b.abs());
    while !b.is_zero() {
        let r = a.checked_rem(&b)?;
        a = b;
        b = r;
    }
    Ok(a)
}

/// 二つの引数の scale の大きい方（有限のときだけ呼ぶ）。
fn max_scale(a: &Numeric, b: &Numeric) -> i32 {
    a.scale().unwrap_or(0).max(b.scale().unwrap_or(0))
}

/// `gcd(numeric, numeric)`（OID 5048）。NaN と ±Infinity は NaN。結果の scale は二つの大きい方。
pub fn numeric_gcd(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (num_at(args, 0)?, num_at(args, 1)?);
    if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
        return Ok(Datum::Numeric(Numeric::NaN));
    }
    let g = gcd_finite(a, b)?;
    Ok(Datum::Numeric(g.trunc(max_scale(a, b))?))
}

/// `lcm(numeric, numeric)`（OID 5049）。どちらかが 0 なら 0。
pub fn numeric_lcm(args: &[Datum]) -> Result<Datum> {
    let (a, b) = (num_at(args, 0)?, num_at(args, 1)?);
    if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
        return Ok(Datum::Numeric(Numeric::NaN));
    }
    let scale = max_scale(a, b);
    if a.is_zero() || b.is_zero() {
        return Ok(Datum::Numeric(Numeric::zero().trunc(scale)?));
    }
    let g = gcd_finite(a, b)?;
    let l = a.div_trunc(&g)?.checked_mul(b)?.abs();
    Ok(Datum::Numeric(l.trunc(scale)?))
}

/// 末尾のゼロを除いた scale（有限値）。
fn min_scale_of(n: &Numeric) -> i32 {
    let s = n.to_string();
    s.split_once('.').map_or(0, |(_, frac)| {
        i32::try_from(frac.trim_end_matches('0').len()).unwrap_or(i32::MAX)
    })
}

/// `min_scale(numeric)`（OID 5042）。NaN と ±Infinity は NULL。
pub fn numeric_min_scale(args: &[Datum]) -> Result<Datum> {
    let n = num_at(args, 0)?;
    if n.is_nan() || n.is_infinite() {
        return Ok(Datum::Null);
    }
    Ok(Datum::Int4(min_scale_of(n)))
}

/// `trim_scale(numeric)`（OID 5043）。NaN と ±Infinity はそのまま。
pub fn numeric_trim_scale(args: &[Datum]) -> Result<Datum> {
    let n = num_at(args, 0)?;
    if n.is_nan() || n.is_infinite() {
        return Ok(Datum::Numeric(n.clone()));
    }
    Ok(Datum::Numeric(n.trunc(min_scale_of(n))?))
}

fn width_bucket_error(msg: &str) -> Error {
    Error::new(
        crate::error::sqlstate::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION,
        msg,
    )
}

fn int4_out_of_range() -> Error {
    Error::new(
        crate::error::sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "integer out of range",
    )
}

/// `width_bucket(numeric, numeric, numeric, int4)`（OID 2170）。
pub fn numeric_width_bucket(args: &[Datum]) -> Result<Datum> {
    let (op, b1, b2) = (num_at(args, 0)?, num_at(args, 1)?, num_at(args, 2)?);
    let Some(Datum::Int4(count)) = args.get(3) else {
        return Err(Error::internal("width_bucket: bad argument"));
    };
    let count = *count;
    if count <= 0 {
        return Err(width_bucket_error("count must be greater than zero"));
    }
    if op.is_nan() || b1.is_nan() || b2.is_nan() {
        return Err(width_bucket_error(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    if b1.is_infinite() || b2.is_infinite() {
        return Err(width_bucket_error("lower and upper bounds must be finite"));
    }
    let ascending = match b1.cmp(b2) {
        std::cmp::Ordering::Equal => {
            return Err(width_bucket_error("lower bound cannot equal upper bound"));
        }
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
    };
    let (lo, hi) = if ascending { (b1, b2) } else { (b2, b1) };
    let result = if ascending {
        if op < b1 {
            0
        } else if op >= b2 {
            count.checked_add(1).ok_or_else(int4_out_of_range)?
        } else {
            bucket_inner(&op.checked_sub(lo)?, &hi.checked_sub(lo)?, count)?
        }
    } else if op > b1 {
        0
    } else if op <= b2 {
        count.checked_add(1).ok_or_else(int4_out_of_range)?
    } else {
        bucket_inner(&b1.checked_sub(op)?, &b1.checked_sub(b2)?, count)?
    };
    Ok(Datum::Int4(result))
}

/// `width_bucket(float8, float8, float8, int4)`（OID 320）。numeric 版の引数が整数・float8 のときの解決先。
pub fn float8_width_bucket(args: &[Datum]) -> Result<Datum> {
    let f = |i: usize| match args.get(i) {
        Some(Datum::Float8(v)) => Ok(*v),
        _ => Err(Error::internal("width_bucket: bad argument")),
    };
    let (op, b1, b2) = (f(0)?, f(1)?, f(2)?);
    let Some(Datum::Int4(count)) = args.get(3) else {
        return Err(Error::internal("width_bucket: bad argument"));
    };
    let count = *count;
    if count <= 0 {
        return Err(width_bucket_error("count must be greater than zero"));
    }
    if op.is_nan() || b1.is_nan() || b2.is_nan() {
        return Err(width_bucket_error(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    if b1.is_infinite() || b2.is_infinite() {
        return Err(width_bucket_error("lower and upper bounds must be finite"));
    }
    #[allow(clippy::float_cmp)]
    if b1 == b2 {
        return Err(width_bucket_error("lower bound cannot equal upper bound"));
    }
    let (below, above, num, den) = if b1 < b2 {
        (op < b1, op >= b2, op - b1, b2 - b1)
    } else {
        (op > b1, op <= b2, b1 - op, b1 - b2)
    };
    let r = if below {
        0
    } else if above {
        count.checked_add(1).ok_or_else(int4_out_of_range)?
    } else {
        let x = if den.is_infinite() {
            let (a, b) = if b1 < b2 { (op, b1) } else { (b1, op) };
            let (hi, lo) = if b1 < b2 { (b2, b1) } else { (b1, b2) };
            f64::from(count) * (a / 2.0 - b / 2.0) / (hi / 2.0 - lo / 2.0)
        } else {
            f64::from(count) * (num / den)
        };
        // 丸め誤差で count に届いたら count - 1 に戻す（PG と同じ）。
        #[allow(clippy::cast_possible_truncation)]
        let i = x as i32;
        i.min(count - 1) + 1
    };
    Ok(Datum::Int4(r))
}

/// `floor(count * num / den) + 1`。
fn bucket_inner(num: &Numeric, den: &Numeric, count: i32) -> Result<i32> {
    let q = num
        .checked_mul(&Numeric::from(count))?
        .checked_div(den)?
        .floor()?;
    q.to_i32()?.checked_add(1).ok_or_else(int4_out_of_range)
}

/// `factorial(int8)`（OID 1376）。
pub fn numeric_factorial(args: &[Datum]) -> Result<Datum> {
    let Some(n) = args.first().and_then(Datum::as_i64) else {
        return Err(Error::internal("factorial: bad argument"));
    };
    if n < 0 {
        return Err(Error::new(
            crate::error::sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "factorial of a negative number is undefined",
        ));
    }
    if n > 32177 {
        return Err(Error::new(
            crate::error::sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value overflows numeric format",
        ));
    }
    let mut acc = Numeric::from(1_i64);
    for i in 2..=n {
        acc = acc.checked_mul(&Numeric::from(i))?;
    }
    Ok(Datum::Numeric(acc))
}

/// `pow(numeric, numeric)`（OID 1738）: M6（TY-D15）。
pub fn numeric_pow(_args: &[Datum]) -> Result<Datum> {
    Err(Error::not_supported("numeric pow is not supported yet"))
}

/// `numeric_send`: ネットワークバイト順のバイナリ形式。
pub fn numeric_send(n: &Numeric) -> Vec<u8> {
    n.to_binary()
}

/// `numeric_recv`: バイナリ形式を読み、`typmod` を適用する（入力は 1 個の値ちょうど）。
pub fn numeric_recv(buf: &[u8], typmod: i32) -> Result<Datum> {
    Ok(Datum::Numeric(Numeric::from_binary(buf, typmod)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::cmp_datum;
    use crate::types::ops;
    use std::cmp::Ordering;
    use std::fmt::Write as _;

    fn n(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    fn enc(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        encode_numeric(&n(s), &mut v);
        v
    }

    fn hex(b: &[u8]) -> String {
        b.iter()
            .fold(String::new(), |mut s, x| {
                let _ = write!(s, "{x:02X} ");
                s
            })
            .trim_end()
            .to_owned()
    }

    #[test]
    fn encode_matches_design_examples() {
        // 09 §3.5 のバイト例（varlena ヘッダを除いたペイロード）。
        assert_eq!(hex(&enc("1.5")), "02 00 00 00 00 00 01 00 01 00 88 13");
        assert_eq!(hex(&enc("-123.456")), "02 00 00 00 00 40 03 00 7B 00 D0 11");
        assert_eq!(hex(&enc("0")), "00 00 00 00 00 00 00 00");
        assert_eq!(hex(&enc("NaN")), "00 00 00 00 00 C0 00 00");
        assert_eq!(hex(&enc("Infinity")), "00 00 00 00 00 D0 00 00");
        assert_eq!(hex(&enc("-Infinity")), "00 00 00 00 00 F0 00 00");
        // 0 の dscale は保たれる。
        assert_eq!(hex(&enc("0.00")), "00 00 00 00 00 00 02 00");
    }

    #[test]
    fn roundtrip_preserves_value_and_scale() {
        for s in [
            "0",
            "0.000",
            "1",
            "-1",
            "1.10",
            "123456789.123456789",
            "-0.0001",
            "1e100",
            "1.5e-100",
            "NaN",
            "Infinity",
            "-Infinity",
            "99999999999999999999.99",
        ] {
            let orig = n(s);
            let mut b = Vec::new();
            encode_numeric(&orig, &mut b);
            let back = decode_numeric(&b).unwrap();
            assert_eq!(back.to_string(), orig.to_string(), "{s}");
        }
    }

    #[test]
    fn decode_rejects_corrupt_payloads() {
        let code = |b: &[u8]| decode_numeric(b).unwrap_err().sqlstate.code();
        assert_eq!(code(&[]), "XX001");
        assert_eq!(code(&[0, 0, 0, 0, 0, 0, 0]), "XX001");
        // ndigits = 1 だが桁がない。
        assert_eq!(code(&[1, 0, 0, 0, 0, 0, 0, 0]), "XX001");
        // 桁が 10000 以上（from_parts の 22P03 を XX001 に写す）。
        assert_eq!(code(&[1, 0, 0, 0, 0, 0, 0, 0, 0x10, 0x27]), "XX001");
        // 負の桁。
        assert_eq!(code(&[1, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF]), "XX001");
        // 不正な sign。
        assert_eq!(code(&[0, 0, 0, 0, 1, 0, 0, 0]), "XX001");
        // NaN に dscale がある。
        assert_eq!(code(&[0, 0, 0, 0, 0, 0xC0, 1, 0]), "XX001");
        // dscale が範囲外。
        assert_eq!(code(&[0, 0, 0, 0, 0, 0, 0xFF, 0xFF]), "XX001");
    }

    #[test]
    fn typmod_and_literals() {
        let t = yuzhu_numeric::make_typmod(&[5, 2]).unwrap();
        assert_eq!(
            apply_numeric_typmod(&n("1.005"), t).unwrap().to_string(),
            "1.01"
        );
        let e = apply_numeric_typmod(&n("1000"), t).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22003");
        assert_eq!(e.message, "numeric field overflow");
        assert!(e.detail.is_some());
        assert_eq!(
            apply_numeric_typmod(&n("1.10"), -1).unwrap().to_string(),
            "1.10"
        );
        assert_eq!(parse_numeric("1.255", t).unwrap().to_string(), "1.26");
        let lit = |s: &str| numeric_literal(s).unwrap();
        assert_eq!(lit("1e3"), Datum::Numeric(n("1000")));
        assert_eq!(lit("1.0e-3").as_numeric_string(), "0.0010");
        assert_eq!(lit(".5").as_numeric_string(), "0.5");
        assert_eq!(lit("1.").as_numeric_string(), "1");
        assert_eq!(
            lit("123456789012345678901234567890").as_numeric_string(),
            "123456789012345678901234567890"
        );
        let e = numeric_literal("1e1000000").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22003");
        assert_eq!(e.message, "value overflows numeric format");
    }

    trait AsNumericString {
        fn as_numeric_string(&self) -> String;
    }
    impl AsNumericString for Datum {
        fn as_numeric_string(&self) -> String {
            match self {
                Datum::Numeric(n) => n.to_string(),
                other => panic!("not numeric: {other:?}"),
            }
        }
    }

    #[test]
    fn scale_and_div() {
        let d = |s: &str| Datum::Numeric(n(s));
        assert_eq!(numeric_scale(&[d("1.50")]).unwrap(), Datum::Int4(2));
        assert_eq!(numeric_scale(&[d("-3")]).unwrap(), Datum::Int4(0));
        assert_eq!(numeric_scale(&[d("NaN")]).unwrap(), Datum::Null);
        assert_eq!(numeric_scale(&[d("Infinity")]).unwrap(), Datum::Null);
        assert_eq!(
            numeric_div_trunc(&[d("-7.5"), d("2")])
                .unwrap()
                .as_numeric_string(),
            "-3"
        );
        let e = numeric_div_trunc(&[d("1"), d("0")]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22012");
        assert_eq!(e.message, "division by zero");
        assert!(numeric_scale(&[Datum::Int4(1)]).is_err());
    }

    #[test]
    fn operators_through_datum() {
        let d = |s: &str| Datum::Numeric(n(s));
        assert_eq!(
            ops::numeric_add(&[d("1.5"), d("2.25")])
                .unwrap()
                .as_numeric_string(),
            "3.75"
        );
        assert_eq!(
            ops::numeric_div(&[d("10"), d("4.0")])
                .unwrap()
                .as_numeric_string(),
            "2.5000000000000000"
        );
        assert_eq!(
            ops::numeric_mod(&[d("-5"), d("2.0")])
                .unwrap()
                .as_numeric_string(),
            "-1.0"
        );
        assert_eq!(
            ops::numeric_round(&[d("1234.5678"), Datum::Int4(-2)])
                .unwrap()
                .as_numeric_string(),
            "1200"
        );
        assert_eq!(cmp_datum(&d("1.10"), &d("1.1")), Ordering::Equal);
        assert_eq!(cmp_datum(&d("NaN"), &d("Infinity")), Ordering::Greater);
        assert_eq!(
            ops::numeric_power(&[d("2"), d("3")]).unwrap_err().message,
            "numeric power is not supported yet"
        );
    }
}

/// `yuzhu-numeric` の差分コーパス（PostgreSQL 17 で採取）を、`Datum` 経由の橋渡し（演算子・キャスト・
/// 関数・typmod・ディスク形式）に流す。結果・SQLSTATE・文言が `Numeric` の直接呼び出しと同じ
/// （= PostgreSQL と同じ）であることを確かめる（`m4/09-types-functions.md` §12.1）。
#[cfg(test)]
mod bridge_corpus {
    use super::*;
    use crate::types::{cmp_datum, ops};
    use std::fmt::Write as _;

    const CORPUS: &str = include_str!("../../../yuzhu-numeric/tests/data/pg17_corpus.tsv");
    const REGRESS: &str = include_str!("../../../yuzhu-numeric/tests/data/pg17_regress.tsv");

    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('v') => out.push('\x0b'),
                Some('f') => out.push('\x0c'),
                Some('b') => out.push('\x08'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            }
        }
        out
    }

    fn fnv64(s: &str) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        for b in s.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01B3);
        }
        h
    }

    fn err_text(e: &Error) -> String {
        let mut s = format!("ERR:{}:{}", e.sqlstate.code(), e.message);
        if let Some(d) = &e.detail {
            let _ = write!(s, ":{d}");
        }
        s
    }

    fn pg_float(s: &str) -> f64 {
        match s {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => s.parse().unwrap(),
        }
    }

    /// `Datum::Numeric` の結果を `OK:<text>` にし、ディスク形式の往復も確かめる。
    fn ok(r: Result<Datum>) -> String {
        match r {
            Ok(Datum::Numeric(n)) => {
                let mut buf = Vec::new();
                encode_numeric(&n, &mut buf);
                let back = decode_numeric(&buf).expect("decode of encode output");
                assert_eq!(back.to_string(), n.to_string(), "disk round trip");
                format!("OK:{n}")
            }
            Ok(Datum::Int2(v)) => format!("OK:{v}"),
            Ok(Datum::Int4(v)) => format!("OK:{v}"),
            Ok(Datum::Int8(v)) => format!("OK:{v}"),
            Ok(other) => panic!("unexpected result {other:?}"),
            Err(e) => err_text(&e),
        }
    }

    fn eval_to_float(op: &str, a: &Datum, expected: &str) -> String {
        let got = if op == "tof8" {
            ops::numeric_to_float8(std::slice::from_ref(a))
        } else {
            ops::numeric_to_float4(std::slice::from_ref(a))
        };
        match got {
            Err(e) => err_text(&e),
            Ok(d) => {
                let v = d.as_f64().expect("float result");
                let exp = expected.strip_prefix("OK:").map(|e| match e {
                    "NaN" | "Infinity" | "-Infinity" => pg_float(e),
                    _ if op == "tof4" => f64::from(e.parse::<f32>().unwrap()),
                    _ => pg_float(e),
                });
                match exp {
                    Some(e) if (v.is_nan() && e.is_nan()) || v.to_bits() == e.to_bits() => {
                        expected.to_owned()
                    }
                    _ => format!("OK:{v:e}"),
                }
            }
        }
    }

    fn eval(op: &str, a: &str, b: &str, expected: &str) -> Option<String> {
        let operand_count = match op {
            "add" | "sub" | "mul" | "div" | "mod" | "divtrunc" | "cmp" => 2,
            "in" | "typmod" | "fromf8" | "fromf4" | "fromi8" | "recv" | "send" => 0,
            _ => 1,
        };
        let mut ds = Vec::new();
        for s in [a, b].iter().take(operand_count) {
            match parse_numeric(s, -1) {
                Ok(n) => ds.push(Datum::Numeric(n)),
                Err(e) => return Some(err_text(&e)),
            }
        }
        let typmod = || b.parse::<i32>().unwrap();
        Some(match op {
            "in" => ok(parse_numeric(a, typmod()).map(Datum::Numeric)),
            "cast" => ok(parse_numeric(a, -1)
                .and_then(|n| apply_numeric_typmod(&n, typmod()))
                .map(Datum::Numeric)),
            "add" => ok(ops::numeric_add(&ds)),
            "sub" => ok(ops::numeric_sub(&ds)),
            "mul" => ok(ops::numeric_mul(&ds)),
            "div" => ok(ops::numeric_div(&ds)),
            "mod" => ok(ops::numeric_mod(&ds)),
            "divtrunc" => ok(numeric_div_trunc(&ds)),
            "cmp" => format!("OK:{}", cmp_datum(&ds[0], &ds[1]) as i8),
            "round" => {
                ds.push(Datum::Int4(typmod()));
                ok(ops::numeric_round(&ds))
            }
            "trunc" => {
                ds.push(Datum::Int4(typmod()));
                ok(ops::numeric_trunc(&ds))
            }
            "ceil" => ok(ops::numeric_ceil(&ds)),
            "floor" => ok(ops::numeric_floor(&ds)),
            "abs" => ok(ops::numeric_abs(&ds)),
            "neg" => ok(ops::numeric_uminus(&ds)),
            "sign" => ok(ops::numeric_sign(&ds)),
            "toi2" => ok(ops::numeric_to_int2(&ds)),
            "toi4" => ok(ops::numeric_to_int4(&ds)),
            "toi8" => ok(ops::numeric_to_int8(&ds)),
            "tof8" | "tof4" => eval_to_float(op, &ds[0], expected),
            "fromf8" => ok(ops::float_to_numeric(&[Datum::Float8(pg_float(a))])),
            "fromf4" => {
                let f: f32 = match a {
                    "NaN" => f32::NAN,
                    "Infinity" => f32::INFINITY,
                    "-Infinity" => f32::NEG_INFINITY,
                    _ => a.parse().unwrap(),
                };
                ok(ops::float_to_numeric(&[Datum::Float4(f)]))
            }
            "fromi8" => ok(ops::int_to_numeric(&[Datum::Int8(a.parse().unwrap())])),
            // 橋渡しの対象外（Extended Query 用の BE 形式と typmod の符号化は yuzhu-numeric の直接テスト）。
            _ => return None,
        })
    }

    fn check(name: &str, corpus: &str, min_cases: usize) {
        let (mut total, mut checked) = (0, 0);
        let mut failures = Vec::new();
        for line in corpus.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 4, "malformed {name} line: {line}");
            let (op, a, b, expected) = (f[0], unescape(f[1]), unescape(f[2]), unescape(f[3]));
            total += 1;
            let Some(got) = eval(op, &a, &b, &expected) else {
                continue;
            };
            checked += 1;
            let got = match got.strip_prefix("OK:") {
                Some(body) if got.len() > 100 => {
                    format!("OKH:{}:{:016x}", body.len(), fnv64(body))
                }
                _ => got,
            };
            if got != expected {
                let cut = |s: &str| s.chars().take(80).collect::<String>();
                failures.push(format!(
                    "{op}({}, {}): expected {:?}, got {:?}",
                    cut(&a),
                    cut(&b),
                    cut(&expected),
                    cut(&got)
                ));
            }
        }
        assert!(total >= min_cases, "{name} too small: {total}");
        assert!(checked * 10 >= total * 8, "{name}: too few bridge cases");
        assert!(
            failures.is_empty(),
            "{} of {checked} {name} cases differ from PostgreSQL 17:\n{}",
            failures.len(),
            failures
                .iter()
                .take(40)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn numeric_bridge_corpus() {
        check("corpus", CORPUS, 30_000);
    }

    #[test]
    fn numeric_bridge_regress() {
        check("regress", REGRESS, 4_000);
    }
}

/// TY-2 の関数の単体テスト（期待値は PostgreSQL 17 の実機）。
#[cfg(test)]
mod gap_tests {
    use super::*;

    fn d(s: &str) -> Datum {
        Datum::Numeric(s.parse().unwrap())
    }

    fn s(r: Result<Datum>) -> String {
        match r.unwrap() {
            Datum::Numeric(n) => n.to_string(),
            Datum::Int4(i) => i.to_string(),
            Datum::Null => "NULL".into(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gcd_lcm() {
        for (a, b, g, l) in [
            ("12", "18", "6", "36"),
            ("-12", "18", "6", "36"),
            ("0", "0", "0", "0"),
            ("0", "5", "5", "0"),
            ("1.5", "3", "1.5", "3.0"),
            ("12.00", "18", "6.00", "36.00"),
            ("NaN", "1", "NaN", "NaN"),
            ("Infinity", "1", "NaN", "NaN"),
            ("1", "-Infinity", "NaN", "NaN"),
        ] {
            assert_eq!(s(numeric_gcd(&[d(a), d(b)])), g, "gcd({a},{b})");
            assert_eq!(s(numeric_lcm(&[d(a), d(b)])), l, "lcm({a},{b})");
        }
        assert_eq!(s(numeric_gcd(&[d("0.5"), d("0.3")])), "0.1");
        assert_eq!(
            s(numeric_gcd(&[d("12345678901234567890"), d("9876543210")])),
            "90"
        );
        assert_eq!(
            s(numeric_lcm(&[d("1e100"), d("3")])),
            format!("3{}", "0".repeat(100))
        );
    }

    #[test]
    fn scales() {
        assert_eq!(s(numeric_min_scale(&[d("1.500")])), "1");
        assert_eq!(s(numeric_min_scale(&[d("0.00")])), "0");
        assert_eq!(s(numeric_min_scale(&[d("100")])), "0");
        assert_eq!(s(numeric_min_scale(&[d("NaN")])), "NULL");
        assert_eq!(s(numeric_min_scale(&[d("-Infinity")])), "NULL");
        assert_eq!(s(numeric_trim_scale(&[d("0.0010")])), "0.001");
        assert_eq!(s(numeric_trim_scale(&[d("100.00")])), "100");
        assert_eq!(s(numeric_trim_scale(&[d("NaN")])), "NaN");
        assert_eq!(s(numeric_trim_scale(&[d("Infinity")])), "Infinity");
    }

    #[test]
    fn width_bucket() {
        let wb = |op: &str, b1: &str, b2: &str, c: i32| {
            numeric_width_bucket(&[d(op), d(b1), d(b2), Datum::Int4(c)])
        };
        assert_eq!(s(wb("5.35", "0.024", "10.06", 5)), "3");
        assert_eq!(s(wb("-1", "0", "10", 5)), "0");
        assert_eq!(s(wb("10", "0", "10", 5)), "6");
        assert_eq!(s(wb("5", "10", "0", 5)), "3");
        assert_eq!(s(wb("11", "10", "0", 5)), "0");
        assert_eq!(s(wb("0", "10", "0", 5)), "6");
        assert_eq!(s(wb("Infinity", "0", "10", 5)), "6");
        for (r, st) in [
            (wb("1", "0", "10", 0), "2201G"),
            (wb("NaN", "0", "10", 3), "2201G"),
            (wb("1", "5", "5", 3), "2201G"),
            (wb("1", "0", "Infinity", 3), "2201G"),
            (wb("10", "0", "10", i32::MAX), "22003"),
        ] {
            assert_eq!(r.unwrap_err().sqlstate.code(), st);
        }
    }

    #[test]
    fn factorial() {
        let f = |n: i64| numeric_factorial(&[Datum::Int8(n)]);
        assert_eq!(s(f(0)), "1");
        assert_eq!(s(f(5)), "120");
        assert_eq!(s(f(20)), "2432902008176640000");
        assert_eq!(s(f(25)), "15511210043330985984000000");
        assert_eq!(f(-1).unwrap_err().sqlstate.code(), "22003");
        assert_eq!(f(32178).unwrap_err().sqlstate.code(), "22003");
    }

    #[test]
    fn pow_is_not_supported() {
        let e = numeric_pow(&[d("2"), d("3")]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "0A000");
    }

    #[test]
    fn binary_roundtrip() {
        for v in [
            "0",
            "1.50",
            "-12345.6789",
            "NaN",
            "Infinity",
            "-Infinity",
            "1e100",
        ] {
            let n: Numeric = v.parse().unwrap();
            let b = numeric_send(&n);
            assert_eq!(
                numeric_recv(&b, -1).unwrap(),
                Datum::Numeric(n.clone()),
                "{v}"
            );
            assert_eq!(
                numeric_send(&match numeric_recv(&b, -1).unwrap() {
                    Datum::Numeric(x) => x,
                    _ => unreachable!(),
                }),
                b
            );
        }
        // typmod は recv で適用される
        let b = numeric_send(&"1.2345".parse().unwrap());
        assert_eq!(s(numeric_recv(&b, (6 << 16) + 2 + 4)), "1.23");
        assert_eq!(
            numeric_recv(&b[..3], -1).unwrap_err().sqlstate.code(),
            "08P01"
        );
        assert_eq!(
            numeric_recv(&[0, 0, 0, 0, 0x12, 0x34, 0, 0], -1)
                .unwrap_err()
                .sqlstate
                .code(),
            "22P03"
        );
    }
}
