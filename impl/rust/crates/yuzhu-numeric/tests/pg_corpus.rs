//! Differential test against results produced by a real PostgreSQL 17
//! (`tests/data/pg17_corpus.tsv`, regenerate with `tests/data/gen_corpus.py`).

use std::fmt::Write as _;

use yuzhu_numeric::{Numeric, NumericError, make_typmod};

const CORPUS: &str = include_str!("data/pg17_corpus.tsv");
/// Edge cases found by adversarial review (`tests/data/gen_regress.py`).
const REGRESS: &str = include_str!("data/pg17_regress.tsv");

/// Undo PostgreSQL COPY text-format escaping.
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

fn num(s: &str) -> Numeric {
    Numeric::parse(s).unwrap_or_else(|e| panic!("corpus operand {s:?} must parse: {e}"))
}

fn err_text(e: &NumericError) -> String {
    let mut s = format!("ERR:{}:{}", e.sqlstate(), e.message());
    if let Some(d) = e.detail() {
        let _ = write!(s, ":{d}");
    }
    s
}

fn parse_pg_float(s: &str) -> f64 {
    match s {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => s.parse().unwrap(),
    }
}

/// `numeric -> float8/float4`: compared by value (bit pattern), so the
/// result is echoed as `expected` when equal.
fn eval_to_float(op: &str, a: &str, expected: &str) -> String {
    let got = if op == "tof8" {
        num(a).to_f64()
    } else {
        num(a).to_f32().map(f64::from)
    };
    match got {
        Err(e) => err_text(&e),
        Ok(v) => match expected.strip_prefix("OK:") {
            Some(exp) => {
                let e = match exp {
                    "NaN" | "Infinity" | "-Infinity" => parse_pg_float(exp),
                    _ if op == "tof4" => f64::from(exp.parse::<f32>().unwrap()),
                    _ => parse_pg_float(exp),
                };
                let same = (v.is_nan() && e.is_nan()) || v.to_bits() == e.to_bits();
                if same {
                    expected.to_owned()
                } else {
                    format!("OK:{v:e}")
                }
            }
            None => format!("OK:{v:e}"),
        },
    }
}

/// Evaluate one case; returns the result in the corpus format. Floating
/// results are compared by value, so they are returned as `F:<bits>`.
fn eval(op: &str, a: &str, b: &str, expected: &str) -> String {
    let ok = |r: Result<Numeric, NumericError>| match r {
        Ok(n) => format!("OK:{n}"),
        Err(e) => err_text(&e),
    };
    // Operands that PostgreSQL itself rejects (`a::numeric` fails) yield
    // that error, which must match too.
    let operands: &[&str] = match op {
        "in" | "typmod" | "fromf8" | "fromf4" | "fromi8" | "recv" => &[],
        "add" | "sub" | "mul" | "div" | "mod" | "divtrunc" | "cmp" => &[a, b],
        _ => &[a],
    };
    for s in operands {
        if let Err(e) = Numeric::parse(s) {
            return err_text(&e);
        }
    }
    match op {
        "in" => ok(Numeric::parse_with_typmod(a, b.parse().unwrap())),
        "cast" => ok(num(a).apply_typmod(b.parse().unwrap())),
        "typmod" => {
            let mods: Vec<i32> = a.split(',').map(|x| x.parse().unwrap()).collect();
            match make_typmod(&mods) {
                Ok(t) => format!("OK:{t}"),
                Err(e) => err_text(&e),
            }
        }
        "add" => ok(num(a).checked_add(&num(b))),
        "sub" => ok(num(a).checked_sub(&num(b))),
        "mul" => ok(num(a).checked_mul(&num(b))),
        "div" => ok(num(a).checked_div(&num(b))),
        "mod" => ok(num(a).checked_rem(&num(b))),
        "divtrunc" => ok(num(a).div_trunc(&num(b))),
        "cmp" => format!("OK:{}", num(a).cmp(&num(b)) as i8),
        "round" => ok(num(a).round(b.parse().unwrap())),
        "trunc" => ok(num(a).trunc(b.parse().unwrap())),
        "ceil" => ok(num(a).ceil()),
        "floor" => ok(num(a).floor()),
        "abs" => ok(Ok(num(a).abs())),
        "neg" => ok(Ok(num(a).negate())),
        "sign" => ok(Ok(num(a).sign())),
        "toi2" => num(a)
            .to_i16()
            .map_or_else(|e| err_text(&e), |v| format!("OK:{v}")),
        "toi4" => num(a)
            .to_i32()
            .map_or_else(|e| err_text(&e), |v| format!("OK:{v}")),
        "toi8" => num(a)
            .to_i64()
            .map_or_else(|e| err_text(&e), |v| format!("OK:{v}")),
        "tof8" | "tof4" => eval_to_float(op, a, expected),
        "fromf8" => ok(Ok(Numeric::from_f64(parse_pg_float(a)))),
        "fromf4" => {
            let f: f32 = match a {
                "NaN" => f32::NAN,
                "Infinity" => f32::INFINITY,
                "-Infinity" => f32::NEG_INFINITY,
                _ => a.parse().unwrap(),
            };
            ok(Ok(Numeric::from_f32(f)))
        }
        "fromi8" => ok(Ok(Numeric::from(a.parse::<i64>().unwrap()))),
        "send" => {
            let n = num(a);
            let bin = n.to_binary();
            // The binary form must also round-trip through recv.
            let back = Numeric::from_binary(&bin, -1).expect("recv of send output");
            assert_eq!(
                back.to_string(),
                n.to_string(),
                "send/recv round trip of {a}"
            );
            let mut s = String::from("OK:");
            for byte in bin {
                let _ = write!(s, "{byte:02x}");
            }
            s
        }
        "recv" => {
            let bytes: Vec<u8> = (0..a.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&a[i..i + 2], 16).unwrap())
                .collect();
            ok(Numeric::from_binary(&bytes, b.parse().unwrap()))
        }
        _ => panic!("unknown op {op}"),
    }
}

/// Normalise a result to the stored form (long texts become length+hash).
fn stored_form(res: String) -> String {
    match res.strip_prefix("OK:") {
        Some(body) if res.len() > 100 => format!("OKH:{}:{:016x}", body.len(), fnv64(body)),
        _ => res,
    }
}

/// Run every case of a fixture and panic listing the mismatches.
fn check_fixture(name: &str, corpus: &str, min_cases: usize) {
    let mut total = 0;
    let mut failures = Vec::new();
    for line in corpus.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 4, "malformed {name} line: {line}");
        let (op, a, b, expected) = (f[0], unescape(f[1]), unescape(f[2]), unescape(f[3]));
        total += 1;
        let got = stored_form(eval(op, &a, &b, &expected));
        if got != expected {
            let show = |s: &str| -> String {
                if s.len() > 120 {
                    format!("<{} chars>", s.len())
                } else {
                    format!("{s:?}")
                }
            };
            failures.push(format!(
                "{op}({}, {}): expected {}, got {}",
                show(&a),
                show(&b),
                show(&expected),
                show(&got)
            ));
        }
    }
    assert!(total >= min_cases, "{name} too small: {total}");
    if !failures.is_empty() {
        let shown: Vec<&String> = failures.iter().take(60).collect();
        panic!(
            "{} of {total} {name} cases differ from PostgreSQL 17:\n{}",
            failures.len(),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

#[test]
fn pg17_corpus_matches() {
    check_fixture("corpus", CORPUS, 30_000);
}

#[test]
fn pg17_regress_matches() {
    check_fixture("regress", REGRESS, 4_000);
}

/// Ad-hoc differential runs: point `YUZHU_NUMERIC_EXTRA_FIXTURE` at a file
/// in the corpus format (e.g. a fuzz batch generated against PostgreSQL).
#[test]
fn extra_fixture_from_env() {
    if let Ok(path) = std::env::var("YUZHU_NUMERIC_EXTRA_FIXTURE") {
        let text = std::fs::read_to_string(&path).expect("read extra fixture");
        check_fixture(&path, &text, 0);
    }
}
