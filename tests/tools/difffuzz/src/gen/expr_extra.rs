//! expr 領域の追加生成器。型・演算子・関数を網羅的に振る単一文 SELECT。

use crate::rng::Rng;

const INTS: [&str; 16] = [
    "0",
    "1",
    "-1",
    "2",
    "7",
    "(-7)",
    "32767",
    "(-32768)",
    "2147483647",
    "(-2147483648)",
    "2147483648",
    "9223372036854775807",
    "(-9223372036854775808)",
    "4294967296",
    "100",
    "NULL",
];
const FLOATS: [&str; 20] = [
    "'0.0'::float8",
    "'1.5'::float8",
    "'-2.25'::float8",
    "'0.1'::float8",
    "'1e10'::float8",
    "'1e15'::float8",
    "'1e16'::float8",
    "'1e-5'::float8",
    "'1e-4'::float8",
    "'123456789.123456789'::float8",
    "'3.14159265358979'::float8",
    "'NaN'::float8",
    "'Infinity'::float8",
    "'-Infinity'::float8",
    "'-0.0'::float8",
    "'1e308'::float8",
    "'1e-320'::float8",
    "'4.5'::float8",
    "'5.5'::float8",
    "'0.5'::float8",
];
const NUMS: [&str; 14] = [
    "0::numeric",
    "1.5::numeric",
    "(-2.50)::numeric",
    "0.10::numeric",
    "123456789.123456789::numeric",
    "1e20::numeric",
    "0.000001::numeric",
    "2.5::numeric",
    "3.5::numeric",
    "(-0.5)::numeric",
    "100.00::numeric",
    "numeric 'NaN'",
    "99999999999999999999.99::numeric",
    "1e-10::numeric",
];
const TEXTS: [&str; 16] = [
    "''",
    "'a'",
    "'abc'",
    "'Hello'",
    "'  pad  '",
    "'foo bar baz'",
    "'日本語'",
    "'ABC'",
    "'100'",
    "'a%b'",
    "'a_b'",
    "'O''Brien'",
    "'x\\y'",
    "'Ünïcode'",
    "NULL",
    "'  42  '",
];
const TYPES: [&str; 12] = [
    "smallint",
    "integer",
    "bigint",
    "numeric",
    "numeric(5,2)",
    "real",
    "double precision",
    "text",
    "varchar(3)",
    "boolean",
    "char(4)",
    "numeric(3,0)",
];
const LIKES: [&str; 12] = [
    "'a%'", "'%b%'", "'_bc'", "'%'", "''", "'H%o'", "'a\\%b'", "'a%b'", "'%a_b%'", "'A%'",
    "'%日_'", "'100%'",
];

fn pi(r: &mut Rng) -> &'static str {
    r.pick(&INTS)
}
fn pf(r: &mut Rng) -> &'static str {
    r.pick(&FLOATS)
}
fn pn(r: &mut Rng) -> &'static str {
    r.pick(&NUMS)
}
fn pt(r: &mut Rng) -> &'static str {
    r.pick(&TEXTS)
}

fn pick_val(r: &mut Rng) -> &'static str {
    match r.below(5) {
        0 => pi(r),
        1 => pf(r),
        2 => pn(r),
        3 => pt(r),
        _ => r.pick(&[
            "true", "false", "'t'", "'yes'", "'maybe'", "'1.5'", "'  7 '", "'1e3'", "'.5'", "'5.'",
            "'inf'", "'-nan'", "'1e'", "' 1.5 '", "'1e400'", "'-'", "'+5'", "'1_000'",
        ]),
    }
}

fn pick_bool(r: &mut Rng) -> &'static str {
    r.pick(&[
        "TRUE",
        "FALSE",
        "NULL",
        "(1 < 2)",
        "(NULL = NULL)",
        "('a' LIKE 'a')",
    ])
}

pub fn stmt(r: &mut Rng) -> String {
    let cmp = *r.pick(&["=", "<>", "<", "<=", ">", ">="]);
    let arith = *r.pick(&["+", "-", "*", "/", "%"]);
    let e = match r.below(66) {
        0 => format!("{} {} {}", pi(r), arith, pi(r)),
        1 => format!("{}::int4 {} {}::int4", pi(r), arith, pi(r)),
        2 => format!("{}::int2 {} {}::int2", pi(r), arith, pi(r)),
        3 => format!("{}::int8 {} {}::int8", pi(r), arith, pi(r)),
        4 => format!("{} {} {}", pf(r), *r.pick(&["+", "-", "*", "/"]), pf(r)),
        5 => format!("{}::float8 {} {}", pi(r), *r.pick(&["+", "*", "/"]), pf(r)),
        6 => format!(
            "{} {} {}",
            pn(r),
            *r.pick(&["+", "-", "*", "/", "%"]),
            pn(r)
        ),
        7 => format!(
            "{}::numeric {} {}",
            pi(r),
            *r.pick(&["+", "-", "*", "/"]),
            pn(r)
        ),
        8 => format!("({})::{}", pick_val(r), r.pick(&TYPES)),
        9 => format!("CAST({} AS {})", pick_val(r), r.pick(&TYPES)),
        10 => format!("{} {} {}", pf(r), cmp, pn(r)),
        11 => format!("{} {} {}", pi(r), cmp, pf(r)),
        12 => format!("{} {} {}", pt(r), cmp, pt(r)),
        13 => format!(
            "{}::real {} {}::real",
            pf(r),
            *r.pick(&["+", "*", "/"]),
            pf(r)
        ),
        14 => format!(
            "{}({})",
            r.pick(&[
                "abs", "sign", "ceil", "floor", "round", "trunc", "sqrt", "exp", "ln", "cbrt",
                "degrees", "radians"
            ]),
            pf(r)
        ),
        15 => format!(
            "{}({})",
            r.pick(&["abs", "sign", "ceil", "floor", "round", "trunc", "sqrt", "ln", "log"]),
            pn(r)
        ),
        16 => format!("{}({})", r.pick(&["abs", "sign"]), pi(r)),
        17 => format!("round({}, {})", pn(r), r.range(-3, 6)),
        18 => format!("trunc({}, {})", pn(r), r.range(-2, 4)),
        19 => format!(
            "power({}, {})",
            pf(r),
            r.pick(&["2", "0.5", "-1", "0", "10"])
        ),
        20 => format!("mod({}, {})", pi(r), r.pick(&["0", "3", "-3", "7"])),
        21 => format!("div({}, {})", pn(r), r.pick(&["0", "3", "-3", "0.7"])),
        22 => format!(
            "{}({})",
            r.pick(&[
                "length",
                "char_length",
                "octet_length",
                "upper",
                "lower",
                "initcap",
                "reverse",
                "ascii",
                "md5",
                "btrim",
                "ltrim",
                "rtrim",
                "bit_length"
            ]),
            pt(r)
        ),
        23 => format!(
            "{}({}, {})",
            r.pick(&["left", "right", "repeat"]),
            pt(r),
            r.range(-3, 5)
        ),
        24 => format!("substr({}, {}, {})", pt(r), r.range(-3, 6), r.range(-2, 6)),
        25 => format!(
            "substring({} from {} for {})",
            pt(r),
            r.range(-1, 5),
            r.range(0, 5)
        ),
        26 => format!(
            "{}({}, {}, {})",
            r.pick(&["lpad", "rpad"]),
            pt(r),
            r.range(-1, 8),
            pt(r)
        ),
        27 => format!(
            "{}({}, {}, {})",
            r.pick(&["replace", "translate"]),
            pt(r),
            pt(r),
            pt(r)
        ),
        28 => format!(
            "{} {}LIKE {}",
            pt(r),
            r.pick(&["", "NOT ", "I"]),
            r.pick(&LIKES)
        ),
        29 => format!(
            "{} || {}",
            pt(r),
            r.pick(&["1", "'1.5'::float8", "true", "'x'", "NULL", "2147483648"])
        ),
        30 => format!(
            "CASE {} WHEN {} THEN {} WHEN {} THEN {} ELSE {} END",
            pi(r),
            pi(r),
            pt(r),
            pi(r),
            pt(r),
            pt(r)
        ),
        31 => format!(
            "{}({}, {}, {})",
            r.pick(&["COALESCE", "GREATEST", "LEAST"]),
            pi(r),
            pf(r),
            pn(r)
        ),
        32 => format!("NULLIF({}, {})", pick_val(r), pick_val(r)),
        34 => format!(
            "{}::{} {} {}::{}",
            pi(r),
            r.pick(&["int2", "int4", "int8"]),
            r.pick(&["&", "|", "#", "<<", ">>"]),
            pi(r),
            r.pick(&["int2", "int4", "int8"])
        ),
        35 => format!(
            "{}({}::{})",
            r.pick(&["~", "-", "+", "@"]),
            pi(r),
            r.pick(&["int2", "int4", "int8"])
        ),
        36 => format!(
            "{}({})::{}",
            r.pick(&["round", "trunc", "ceil", "floor"]),
            pf(r),
            r.pick(&["int2", "int4", "int8", "numeric"])
        ),
        37 => format!(
            "{}::real {} {}::int4",
            pf(r),
            *r.pick(&["+", "-", "*", "/"]),
            pi(r)
        ),
        38 => format!(
            "{} {} {}",
            pt(r),
            r.pick(&["~", "~*", "!~", "!~*", "SIMILAR TO"]),
            r.pick(&[
                "'a.*'",
                "'^[A-Z]+$'",
                "'(ab|c)'",
                "'\\\\d+'",
                "'a|b'",
                "'%b%'",
                "'[a-c]{2}'",
                "''"
            ])
        ),
        39 => format!(
            "{}({}, {})",
            r.pick(&["strpos", "starts_with"]),
            pt(r),
            pt(r)
        ),
        40 => format!("split_part({}, {}, {})", pt(r), pt(r), r.range(-2, 3)),
        41 => format!(
            "{}({}, {}, {})",
            r.pick(&["concat", "concat_ws"]),
            pt(r),
            pi(r),
            pt(r)
        ),
        42 => format!(
            "{} {} {}",
            pi(r),
            r.pick(&["IS NOT DISTINCT FROM", "IS DISTINCT FROM"]),
            pi(r)
        ),
        43 => format!(
            "{} {}BETWEEN {}{} AND {}",
            pi(r),
            r.pick(&["", "NOT "]),
            r.pick(&["", "SYMMETRIC "]),
            pi(r),
            pi(r)
        ),
        44 => format!(
            "{} {}IN ({}, {}, {})",
            pi(r),
            r.pick(&["", "NOT "]),
            pi(r),
            pi(r),
            pi(r)
        ),
        45 => format!(
            "{}::text, {}::text",
            pf(r),
            r.pick(&[
                "1e100::float8",
                "123456789012345678::float8",
                "0.30000000000000004::float8",
                "1.0::real",
                "16777217::real",
                "1e-7::real",
                "3.4e38::real",
                "(-1e-300)::float8"
            ])
        ),
        46 => format!("chr({})", r.range(-1, 130)),
        47 => format!(
            "CASE WHEN {} THEN {} WHEN {} THEN {} END",
            pick_bool(r),
            pi(r),
            pick_bool(r),
            pi(r)
        ),
        48 => format!("{}({}, {})", r.pick(&["COALESCE", "NULLIF"]), pt(r), pt(r)),
        49 => format!("({})::{}::{}", pick_val(r), r.pick(&TYPES), r.pick(&TYPES)),
        50 => format!("{} {} {}", pf(r), cmp, pf(r)),
        51 => format!(
            "{}::int8 * {}::int8 {} {}::int8",
            pi(r),
            pi(r),
            arith,
            pi(r)
        ),
        52 => format!(
            "{}({}, {})",
            r.pick(&["gcd", "lcm", "mod", "power"]),
            pi(r),
            pi(r)
        ),
        53 => format!(
            "{}({})",
            r.pick(&[
                "lower",
                "upper",
                "length",
                "reverse",
                "initcap",
                "md5",
                "quote_literal",
                "quote_ident"
            ]),
            pt(r)
        ),
        57 => format!(
            "{}({}, {})",
            r.pick(&["round", "trunc", "power", "atan2", "log", "mod"]),
            pf(r),
            pf(r)
        ),
        58 => format!(
            "{}({})",
            r.pick(&[
                "sin",
                "cos",
                "tan",
                "asin",
                "acos",
                "atan",
                "sinh",
                "cosh",
                "tanh",
                "log10",
                "factorial",
                "pi"
            ]),
            pf(r)
        ),
        59 => format!(
            "{}::{} {} {}::{}",
            pn(r),
            r.pick(&["int2", "int4", "int8", "float4"]),
            arith,
            pi(r),
            r.pick(&["int2", "int4", "float4", "numeric"])
        ),
        60 => format!(
            "{}({}, {}, {})",
            r.pick(&["overlay", "regexp_replace", "substring"]),
            pt(r),
            pt(r),
            r.range(1, 3)
        ),
        61 => format!("{} {} {}", pick_val(r), cmp, pick_val(r)),
        62 => format!(
            "({})::{} {} ({})::{}",
            pick_val(r),
            r.pick(&["float4", "int8", "numeric(4,1)"]),
            arith,
            pick_val(r),
            r.pick(&["int4", "float8", "numeric"])
        ),
        63 => format!(
            "{}({}, {})",
            r.pick(&["left", "right", "btrim", "ltrim", "rtrim", "position", "strpos"]),
            pt(r),
            pt(r)
        ),
        64 => format!(
            "{}::bool {} {}::bool",
            pick_val(r),
            r.pick(&["AND", "OR", "=", "<>"]),
            pick_val(r)
        ),
        _ => format!(
            "{} {} {}",
            pick_bool(r),
            r.pick(&["AND", "OR", "IS DISTINCT FROM", "="]),
            pick_bool(r)
        ),
    };
    let out = if r.chance(20) {
        format!("{e}, pg_typeof({e})")
    } else {
        e
    };
    format!("SELECT {out};")
}

/// M1〜M3 の yuzhu が未対応（0A000 または 42883）の機能を含む文か。
/// 環境変数 `DIFFFUZZ_ALLOW_UNSUPPORTED=1` で、この判定を無効にする（未対応機能も生成する）。
pub fn uses_unsupported(s: &str) -> bool {
    const DENY: [&str; 38] = [
        "numeric",
        "GREATEST",
        "LEAST",
        "DISTINCT",
        "round(",
        "mod(",
        "sign(",
        "div(",
        "trunc(",
        "ceil(",
        "floor(",
        "sqrt(",
        "exp(",
        "ln(",
        "log(",
        "cbrt(",
        "degrees(",
        "radians(",
        "power(",
        "char_length",
        "octet_length",
        "initcap",
        "reverse",
        "ascii",
        "md5",
        "btrim",
        "ltrim",
        "rtrim",
        "bit_length",
        "left(",
        "right(",
        "substr",
        "lpad",
        "rpad",
        "replace",
        "translate",
        "position",
        "char(",
    ];
    DENY.iter().any(|d| s.contains(d)) || s.contains("trim(")
}

/// yuzhu に未実装と分かっている関数・演算子（ラウンド 2 の差分ファジングで判明。42883）を含む文か。
/// 環境変数 `DIFFFUZZ_SKIP_MISSING=1` のとき、expr 領域の生成で作り直して避ける（隠れた差分を見るため）。
pub fn uses_missing(s: &str) -> bool {
    const M: [&str; 28] = [
        "sin(",
        "cos(",
        "tan(",
        "asin(",
        "acos(",
        "atan(",
        "sinh(",
        "cosh(",
        "tanh(",
        "atan2(",
        "overlay",
        "char(",
        "split_part",
        "starts_with",
        "translate",
        "concat",
        "quote_ident",
        "quote_literal",
        "div(",
        "gcd",
        "lcm",
        "degrees",
        "radians",
        "octet_length",
        "bit_length",
        "(@",
        "char(",
        "SIMILAR",
    ];
    M.iter().any(|d| s.contains(d))
}
