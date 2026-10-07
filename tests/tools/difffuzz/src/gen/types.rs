//! types 領域: 型付きの列への INSERT（正常・範囲外・書式不正）、キャスト、読み戻し。

use super::values::literal;
use super::{error_stmt, insert_row_sql, make_table, select_all, Ctx, Ty};

const ALL: [Ty; 9] = [
    Ty::Int2,
    Ty::Int4,
    Ty::Int8,
    Ty::Float8,
    Ty::Float8,
    Ty::Text,
    Ty::Varchar(5),
    Ty::Varchar(10),
    Ty::Bool,
];

const CAST_TARGETS: [&str; 9] = [
    "smallint",
    "integer",
    "bigint",
    "real",
    "float8",
    "double precision",
    "text",
    "varchar(3)",
    "boolean",
];

pub fn scenario(ctx: &mut Ctx) {
    let ncols = ctx.rng.range(1, 4) as usize;
    let ti = make_table(ctx, ncols, &ALL, false);
    let steps = ctx.rng.range(5, 12);
    for _ in 0..steps {
        match ctx.rng.weighted(&[4, 3, 3, 2]) {
            0 => {
                let n = ctx.rng.range(1, 3) as usize;
                let s = insert_row_sql(ctx, ti, n, 20);
                ctx.push(s);
            }
            1 => {
                let t = ctx.tables[ti].clone();
                ctx.push(select_all(&t));
            }
            2 => {
                // リテラルのキャスト（正常 / 不正 / 範囲外が混ざる）
                let src = *ctx.rng.pick(&ALL);
                let bad = ctx.rng.chance(25);
                let lit = literal(&mut ctx.rng, src, bad);
                let dst = *ctx.rng.pick(&CAST_TARGETS);
                let form = ctx.rng.below(3);
                let s = match form {
                    0 => format!("SELECT CAST({lit} AS {dst});"),
                    1 => format!("SELECT ({lit})::{dst};"),
                    _ => {
                        let v = ctx
                            .rng
                            .pick(&["1", "0", "t", "abc", "12.5", "-7", "99999999999"]);
                        format!("SELECT {dst} '{v}';")
                    }
                };
                ctx.push(s);
            }
            _ => {
                let s = error_stmt(ctx);
                ctx.push(s);
            }
        }
    }
    let t = ctx.tables[ti].clone();
    ctx.push(select_all(&t));
    extra(ctx);
}

const INT_STRS: &[&str] = &[
    "0",
    "1",
    "-1",
    "+5",
    "  7 ",
    "\t42\n",
    "-0",
    "007",
    "32767",
    "32768",
    "-32768",
    "-32769",
    "2147483647",
    "2147483648",
    "-2147483648",
    "-2147483649",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775808",
    "-9223372036854775809",
    "99999999999999999999",
    "1.5",
    "1e3",
    "0x10",
    "1_000",
    "",
    " ",
    "abc",
    "1 2",
    "--1",
    "+-1",
    "12abc",
    "٣",
    "1,000",
    "0b11",
    "0o7",
    "+",
    "-",
    "0 ",
    "00000000000000000000001",
    "-0000009223372036854775808",
    "- 1",
    "1 ",
    "\u{a0}1",
    "+0",
    "-32768 ",
    "٣٤",
];

const FLOAT_STRS: &[&str] = &[
    "0",
    "-0",
    "1",
    "1.5",
    "-2.25",
    "0.1",
    "0.3",
    "1e10",
    "1E-5",
    "1.5e300",
    "1e308",
    "1e309",
    "-1e309",
    "1e-320",
    "1e-400",
    "3.4028235e38",
    "3.4028236e38",
    "3.5e38",
    "1e-45",
    "1e-46",
    "1.17549435e-38",
    "NaN",
    "nan",
    "-NaN",
    "Infinity",
    "-Infinity",
    "inf",
    "-inf",
    "+inf",
    "infinit",
    "  2.5  ",
    ".5",
    "5.",
    "1e",
    "e5",
    "1.2.3",
    "123456789.123456789",
    "16777217",
    "0.30000000000000004",
    "1e+5",
    "1E+05",
    "+1.5",
    "- 1",
    "1e-",
    "0x10",
    "Inf",
    "+Infinity",
    "-nan",
    "nanx",
    "1_0",
    "100000000",
    "1e23",
    "4.9e-324",
    "2.2250738585072014e-308",
    "1.7976931348623157e308",
    "1.7976931348623159e308",
    "9007199254740993",
    "0.1e1",
    "1e0.5",
    "-Inf",
    "iNfInItY",
    "1e-39",
    "8.5e37",
    "1.4e-45",
    "3.4028234e38",
];

const BOOL_STRS: &[&str] = &[
    "t", "f", "true", "false", "TRUE", "False", "yes", "no", "y", "n", "on", "off", "of", "o", "1",
    "0", "tr", "tru", "fals", "  t  ", "ye", "ofx", "2", "-1", "", " ", "maybe", "truee", "yess",
    "T", "Yes", "NO", "On", "OFF", "TrUe", "\tt", "t\n", "f f", "00", "01", "+1", "oFf", "nO", "Y",
    "N",
];

const TEXT_STRS: &[&str] = &[
    "",
    "a",
    "abc",
    "abcde",
    "abcdef",
    "abc   ",
    "abcde   ",
    "abcdef  ",
    "  x",
    "日本語",
    "日本語テキスト",
    "O''Brien",
    "a\\b",
    "line1\nline2",
    "ＡＢＣ",
    "x y z w v u",
    "\t",
    "  ",
    "\\",
    "%",
    "_",
    "😀",
    "é",
    "e\u{301}",
    "12",
    "-3",
    "1.5",
    "true",
    "ß",
    "İ",
    "ǅ",
    "a  b ",
];

const INT_TYPES: [&str; 5] = ["smallint", "int2", "integer", "int", "bigint"];
const FLOAT_TYPES: [&str; 5] = ["real", "float4", "double precision", "float8", "float"];

fn q(s: &str) -> String {
    format!("'{s}'")
}

/// 型の入出力・範囲・typmod を直接突く文を積む。
fn extra(ctx: &mut Ctx) {
    let n = ctx.rng.range(4, 10);
    for _ in 0..n {
        let s = match ctx
            .rng
            .weighted(&[5, 5, 4, 3, 3, 3, 3, 2, 2, 2, 3, 3, 2, 4, 3, 3, 3, 4, 4])
        {
            0 => format!(
                "SELECT {}::{};",
                q(ctx.rng.pick(INT_STRS)),
                ctx.rng.pick(&INT_TYPES)
            ),
            1 => {
                let t = *ctx.rng.pick(&FLOAT_TYPES);
                let t = if t == "float" && ctx.rng.chance(50) {
                    *ctx.rng
                        .pick(&["float(10)", "float(24)", "float(25)", "float(53)"])
                } else {
                    t
                };
                format!("SELECT {}::{};", q(ctx.rng.pick(FLOAT_STRS)), t)
            }
            2 => format!("SELECT {}::boolean;", q(ctx.rng.pick(BOOL_STRS))),
            3 => {
                let a = *ctx.rng.pick(&[
                    "32767::int2",
                    "(-32768)::int2",
                    "2147483647",
                    "(-2147483648)",
                    "9223372036854775807::bigint",
                    "(-9223372036854775807-1)",
                    "1",
                    "0",
                    "-1",
                    "100000",
                ]);
                let b = *ctx.rng.pick(&[
                    "1::int2",
                    "2::int2",
                    "0",
                    "1",
                    "-1",
                    "2",
                    "(-1)::bigint",
                    "0::bigint",
                    "2147483647",
                    "32767::int2",
                ]);
                let op = *ctx.rng.pick(&["+", "-", "*", "/", "%"]);
                format!("SELECT ({a}) {op} ({b});")
            }
            4 => {
                let v = *ctx.rng.pick(&[
                    "0.5",
                    "1.5",
                    "2.5",
                    "-0.5",
                    "-1.5",
                    "-2.5",
                    "32767.5",
                    "32768.4",
                    "2147483647.5",
                    "2147483648",
                    "9.2233720368547758e18",
                    "-9.2233720368547758e18",
                    "1e19",
                    "'NaN'",
                    "'Infinity'",
                    "'-Infinity'",
                ]);
                let ft = *ctx.rng.pick(&["float4", "float8"]);
                let it = *ctx.rng.pick(&INT_TYPES);
                let v = if v.starts_with('\'') {
                    v.to_string()
                } else {
                    q(v)
                };
                format!("SELECT ({v}::{ft})::{it};")
            }
            5 => {
                let a = *ctx.rng.pick(&[
                    "0.1",
                    "0.2",
                    "1.0",
                    "3",
                    "1e20",
                    "1e-20",
                    "123456.789",
                    "16777216",
                    "'NaN'",
                    "'Infinity'",
                    "'-Infinity'",
                    "0",
                    "-0.0",
                    "1e308",
                    "3.4e38",
                ]);
                let b = *ctx.rng.pick(&[
                    "0.1",
                    "0.2",
                    "3",
                    "0",
                    "1e308",
                    "3.4e38",
                    "'NaN'",
                    "'Infinity'",
                    "2",
                    "-1",
                ]);
                let ft = *ctx.rng.pick(&["float4", "float8"]);
                let op = *ctx.rng.pick(&["+", "-", "*", "/", "<", "=", ">="]);
                let a = if a.starts_with('\'') {
                    a.to_string()
                } else {
                    q(a)
                };
                let b = if b.starts_with('\'') {
                    b.to_string()
                } else {
                    q(b)
                };
                format!("SELECT {a}::{ft} {op} {b}::{ft};")
            }
            6 => {
                let v = q(ctx.rng.pick(TEXT_STRS));
                let n = ctx.rng.range(1, 8);
                match ctx.rng.below(4) {
                    0 => format!("SELECT {v}::varchar({n});"),
                    1 => format!("SELECT CAST({v} AS varchar({n}));"),
                    2 => format!("SELECT {v}::text::varchar({n})::text;"),
                    _ => format!("SELECT length({v}::varchar({n}));"),
                }
            }
            7 => {
                let ty = *ctx.rng.pick(&[
                    "varchar(0)",
                    "varchar(-1)",
                    "varchar(10485760)",
                    "varchar(10485761)",
                    "varchar(1.5)",
                    "varchar(a)",
                    "int4(3)",
                    "boolean(1)",
                    "text(5)",
                    "float(0)",
                    "float(54)",
                    "float4(3)",
                    "smallint(2)",
                    "varchar(1,2)",
                ]);
                format!("SELECT 'a'::{ty};")
            }
            8 => {
                let e = *ctx.rng.pick(&[
                    "1",
                    "2147483648",
                    "'1.5'::float8",
                    "'1e3'::float4",
                    "'a'",
                    "true",
                    "1::int2",
                    "1::int2 + 1",
                    "1::int2 + 1::bigint",
                    "'1.5'::float4 + 1",
                    "'1.5'::float4 + '1.5'::float8",
                    "(1 = 1)",
                    "NULL",
                    "'a'::varchar(3)",
                    "'a'::varchar(3) || 'b'",
                    "-32768",
                    "-9223372036854775808",
                    "+1",
                ]);
                format!("SELECT pg_typeof({e});")
            }
            9 => {
                let name = ctx.fresh_table_name();
                let vn = ctx.rng.range(1, 6);
                ctx.push(format!(
                    "CREATE TABLE {name} (a float4, b float8, c varchar({vn}), d boolean, e smallint, f bigint);"
                ));
                let k = ctx.rng.range(1, 4);
                for _ in 0..k {
                    let a = q(ctx.rng.pick(FLOAT_STRS));
                    let b = q(ctx.rng.pick(FLOAT_STRS));
                    let c = q(ctx.rng.pick(TEXT_STRS));
                    let d = q(ctx.rng.pick(BOOL_STRS));
                    let e = q(ctx.rng.pick(INT_STRS));
                    let f = q(ctx.rng.pick(INT_STRS));
                    ctx.push(format!(
                        "INSERT INTO {name} VALUES ({a}, {b}, {c}, {d}, {e}, {f});"
                    ));
                }
                format!("SELECT * FROM {name} ORDER BY 1, 2, 3, 4, 5, 6;")
            }
            10 => {
                let a = *ctx.rng.pick(&[
                    "32767::int2",
                    "(-32768)::int2",
                    "2147483647",
                    "(-2147483648)",
                    "9223372036854775807::bigint",
                    "(-9223372036854775807-1)",
                    "0",
                    "1",
                    "-5",
                ]);
                match ctx.rng.below(6) {
                    0 => format!("SELECT -({a});"),
                    1 => format!("SELECT abs({a});"),
                    2 => format!("SELECT +({a});"),
                    3 => {
                        let b = *ctx.rng.pick(&[
                            "1::int2",
                            "2147483648",
                            "0",
                            "(-1)",
                            "32767::int2",
                            "2147483647",
                        ]);
                        let op = *ctx.rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
                        format!("SELECT ({a}) {op} ({b});")
                    }
                    4 => {
                        format!("SELECT ({a})::text, ({a})::float4, ({a})::float8, ({a})::boolean;")
                    }
                    _ => format!("SELECT ({a})::varchar({});", ctx.rng.range(1, 12)),
                }
            }
            11 => {
                let f = q(ctx.rng.pick(FLOAT_STRS));
                match ctx.rng.below(6) {
                    0 => format!("SELECT {f}::float4::text, {f}::float8::text;"),
                    1 => format!("SELECT {f}::float8::float4;"),
                    2 => format!("SELECT {f}::float4::float8;"),
                    3 => format!("SELECT -({f}::float8), abs({f}::float4);"),
                    4 => {
                        let b = q(ctx.rng.pick(BOOL_STRS));
                        format!("SELECT {b}::boolean::text, {b}::boolean::int, NOT {b}::boolean;")
                    }
                    _ => {
                        let i = q(ctx.rng.pick(INT_STRS));
                        format!("SELECT {i}::bigint::text, {i}::int::bool, {i}::int2::float8;")
                    }
                }
            }
            13 => {
                // 整数リテラルの書式（16 進・8 進・2 進・アンダースコア・指数）と型の決まり方
                let l = *ctx.rng.pick(&[
                    "0x10",
                    "0xFF",
                    "0X1f",
                    "0o17",
                    "0b101",
                    "1_000",
                    "1_0_0",
                    "0x_1",
                    "0x",
                    "0b2",
                    "0o8",
                    "1__0",
                    "1_",
                    "1.",
                    ".5",
                    "1.e2",
                    "1e2",
                    "1e+2",
                    "1e",
                    "00012",
                    "0x7FFFFFFF",
                    "0x80000000",
                    "0xFFFFFFFFFFFFFFFF",
                    "0x7FFFFFFFFFFFFFFF",
                    "2147483648",
                    "9223372036854775808",
                    "1.5e0",
                ]);
                match ctx.rng.below(3) {
                    0 => format!("SELECT {l};"),
                    1 => format!("SELECT pg_typeof({l});"),
                    _ => format!("SELECT {l}::text;"),
                }
            }
            14 => {
                // 演算子: ビット演算・シフト・累乗・除算の端
                let a = *ctx.rng.pick(&[
                    "1::int2",
                    "(-1)::int2",
                    "32767::int2",
                    "5",
                    "(-5)",
                    "2147483647",
                    "9223372036854775807::bigint",
                    "(-9223372036854775807-1)",
                    "0",
                    "255",
                ]);
                let b = *ctx.rng.pick(&[
                    "0", "1", "2", "7", "15", "16", "31", "32", "63", "64", "(-1)", "3::int2",
                ]);
                let op = *ctx
                    .rng
                    .pick(&["&", "|", "#", "<<", ">>", "/", "%", "^", "div"]);
                match op {
                    "div" => format!("SELECT div(({a})::float8, ({b})::float8), mod(({a})::bigint, ({b})::bigint);"),
                    "^" => format!("SELECT ({a})::float8 ^ ({b})::float8;"),
                    _ => format!("SELECT ({a}) {op} ({b});"),
                }
            }
            15 => {
                // 浮動小数の出力・関数・特殊値
                let f = q(ctx.rng.pick(FLOAT_STRS));
                let t = *ctx.rng.pick(&["float4", "float8"]);
                match ctx.rng.below(7) {
                    0 => format!("SELECT {f}::{t} = {f}::{t}, {f}::{t} < 'NaN'::{t}, {f}::{t} > 'Infinity'::{t};"),
                    1 => format!("SELECT sqrt(abs({f}::{t})), ceil({f}::{t}), floor({f}::{t}), round({f}::{t}), trunc({f}::{t});"),
                    2 => format!("SELECT sign({f}::{t}), abs({f}::{t}), ({f}::{t})::int8;"),
                    3 => format!("SELECT {f}::{t} / 0, {f}::{t} * 0, {f}::{t} - {f}::{t};"),
                    4 => format!("SELECT {f}::{t}::numeric;"),
                    5 => format!("SELECT round({f}::float8, 2), ln(abs({f}::float8)), exp({f}::float8);"),
                    _ => format!("SELECT {f}::{t} % 2, mod({f}::float8::int8, 3);"),
                }
            }
            16 => {
                // text / varchar の関数と比較・連結・NULL・暗黙キャスト
                let a = q(ctx.rng.pick(TEXT_STRS));
                let b = q(ctx.rng.pick(TEXT_STRS));
                let n = ctx.rng.range(1, 6);
                match ctx.rng.below(8) {
                    0 => format!("SELECT {a} || {b}, {a}::varchar({n}) || {b}::varchar({n}), {a} || 1, {a} || true;"),
                    1 => format!("SELECT length({a}), char_length({a}), octet_length({a}), upper({a}), lower({a});"),
                    2 => format!("SELECT {a} < {b}, {a} = {b}, {a}::varchar({n}) = {b}::varchar({n});"),
                    3 => format!("SELECT substr({a}, {n}), substr({a}, 2, {n}), left({a}, {n}), right({a}, -{n});"),
                    4 => format!("SELECT trim({a}), ltrim({a}), rtrim({a}), position({b} in {a});"),
                    5 => format!("SELECT {a}::varchar({n})::bigint;"),
                    6 => format!("SELECT {a}::varchar({n}) IS NULL, coalesce(NULL::varchar({n}), {a});"),
                    _ => format!("SELECT replace({a}, {b}, 'x'), repeat({a}, {n}), reverse({a});"),
                }
            }
            17 => {
                // 表の列への UPDATE / DEFAULT / 範囲外 / typmod 超過
                let name = ctx.fresh_table_name();
                let vn = ctx.rng.range(1, 5);
                let it = *ctx.rng.pick(&["smallint", "integer", "bigint"]);
                let ft = *ctx.rng.pick(&["real", "double precision"]);
                let def = *ctx.rng.pick(&["1", "32768", "'x'", "0.5", "'abc'"]);
                ctx.push(format!("CREATE TABLE {name} (i {it} DEFAULT {def}, f {ft}, c varchar({vn}) DEFAULT 'ab', b boolean DEFAULT 'y');"));
                let k = ctx.rng.range(1, 3);
                for _ in 0..k {
                    let i = q(ctx.rng.pick(INT_STRS));
                    let f = q(ctx.rng.pick(FLOAT_STRS));
                    let c = q(ctx.rng.pick(TEXT_STRS));
                    let b = q(ctx.rng.pick(BOOL_STRS));
                    match ctx.rng.below(3) {
                        0 => ctx.push(format!("INSERT INTO {name} (f) VALUES ({f});")),
                        1 => ctx.push(format!("INSERT INTO {name} VALUES ({i}, {f}, {c}, {b});")),
                        _ => ctx.push(format!("INSERT INTO {name} (i, c) VALUES ({i}, {c});")),
                    }
                }
                let i = q(ctx.rng.pick(INT_STRS));
                let c = q(ctx.rng.pick(TEXT_STRS));
                match ctx.rng.below(4) {
                    0 => ctx.push(format!("UPDATE {name} SET i = {i};")),
                    1 => ctx.push(format!("UPDATE {name} SET c = {c};")),
                    2 => ctx.push(format!("UPDATE {name} SET i = i * 2, f = f / 0;")),
                    _ => ctx.push(format!("UPDATE {name} SET b = {c};")),
                }
                format!("SELECT * FROM {name} ORDER BY 1, 2, 3, 4;")
            }
            18 => {
                // 文字列リテラルの書式（E 文字列・Unicode エスケープ・ドル引用）と型変換
                let l = *ctx.rng.pick(&[
                    "E'a\\nb'",
                    "E'\\x41'",
                    "E'\\101'",
                    "E'\\u00e9'",
                    "E'\\U0001F600'",
                    "E'\\q'",
                    "$$a b$$",
                    "$t$x'y$t$",
                    "E'\\0'",
                    "E'\\x'",
                    "'é'",
                    "''''",
                    "E'\\b\\f\\r\\t'",
                ]);
                match ctx.rng.below(5) {
                    0 => format!("SELECT {l};"),
                    1 => format!("SELECT length({l}), {l}::varchar(2);"),
                    2 => format!("SELECT {l}::int;"),
                    3 => format!("SELECT pg_typeof({l}), {l} = {l};"),
                    _ => format!("SELECT ({l})::boolean;"),
                }
            }
            _ => {
                let ty = *ctx.rng.pick(&[
                    "int2",
                    "int4",
                    "int8",
                    "float4",
                    "float8",
                    "bool",
                    "varchar",
                    "character varying(4)",
                    "char varying(4)",
                    "character varying",
                    "double precision",
                    "real",
                    "smallint",
                    "bigint",
                    "integer",
                    "int",
                    "text",
                    "boolean",
                    "oid",
                    "name",
                    "unknowntype",
                    "char",
                    "character(3)",
                ]);
                let v = *ctx
                    .rng
                    .pick(&["1", "0", "'1'", "'a'", "true", "'1.5'", "NULL"]);
                format!("SELECT {v}::{ty};")
            }
        };
        ctx.push(s);
    }
}
