//! 式生成。`expr(rng, scope, cls, depth)` は系統 `cls` の式を返す（PG で通ることが大半。桁あふれなどで落ちるものもある）。
//! query / dml からも列スコープ付きで使い回す。

use super::values::{int_lit, num_lit, text_lit};
use super::{error_stmt, Cls, Ctx};
use crate::rng::Rng;

pub type Scope = [(String, Cls)];

fn col_of(rng: &mut Rng, scope: &Scope, cls: Cls) -> Option<String> {
    let c: Vec<&String> = scope
        .iter()
        .filter(|(_, k)| *k == cls)
        .map(|(n, _)| n)
        .collect();
    if c.is_empty() {
        None
    } else {
        Some((*rng.pick(&c)).clone())
    }
}

fn paren(s: String) -> String {
    format!("({s})")
}

fn small_nonzero(rng: &mut Rng) -> String {
    let v = rng.range(1, 9);
    if rng.chance(20) {
        format!("({})", -v)
    } else {
        v.to_string()
    }
}

pub fn expr(rng: &mut Rng, scope: &Scope, cls: Cls, depth: u32) -> String {
    let leaf = depth == 0 || rng.chance(25);
    match cls {
        Cls::Int => int_expr(rng, scope, depth, leaf),
        Cls::Num => num_expr(rng, scope, depth, leaf),
        Cls::Text => text_expr(rng, scope, depth, leaf),
        Cls::Bool => bool_expr(rng, scope, depth, leaf),
    }
}

fn int_expr(rng: &mut Rng, scope: &Scope, d: u32, leaf: bool) -> String {
    if leaf {
        if rng.chance(45) {
            if let Some(c) = col_of(rng, scope, Cls::Int) {
                return c;
            }
        }
        let v = int_lit(rng);
        return if v < 0 {
            format!("({v})")
        } else if rng.chance(5) {
            "NULL".into()
        } else {
            v.to_string()
        };
    }
    let e = |rng: &mut Rng| expr(rng, scope, Cls::Int, d - 1);
    match rng.weighted(&[6, 6, 4, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 1]) {
        0 => paren(format!("{} + {}", e(rng), e(rng))),
        1 => paren(format!("{} - {}", e(rng), e(rng))),
        2 => paren(format!("{} * {}", e(rng), e(rng))),
        3 => paren(format!("{} / {}", e(rng), small_nonzero(rng))),
        4 => paren(format!("{} % {}", e(rng), small_nonzero(rng))),
        5 => format!("abs({})", e(rng)),
        6 => format!("length({})", expr(rng, scope, Cls::Text, d - 1)),
        7 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            expr(rng, scope, Cls::Bool, d - 1),
            e(rng),
            e(rng)
        ),
        8 => format!("COALESCE({}, {})", e(rng), e(rng)),
        9 => format!("NULLIF({}, {})", e(rng), e(rng)),
        10 => format!("GREATEST({}, {})", e(rng), e(rng)),
        11 => format!("LEAST({}, {})", e(rng), e(rng)),
        12 => format!(
            "position({} in {})",
            expr(rng, scope, Cls::Text, d - 1),
            expr(rng, scope, Cls::Text, d - 1)
        ),
        _ => format!(
            "({})::{}",
            e(rng),
            rng.pick(&["integer", "bigint", "smallint"])
        ),
    }
}

fn num_expr(rng: &mut Rng, scope: &Scope, d: u32, leaf: bool) -> String {
    if leaf {
        if rng.chance(45) {
            if let Some(c) = col_of(rng, scope, Cls::Num) {
                return c;
            }
        }
        let s = num_lit(rng);
        return if s.starts_with('-') {
            format!("({s})")
        } else {
            s
        };
    }
    let e = |rng: &mut Rng| expr(rng, scope, Cls::Num, d - 1);
    match rng.below(8) {
        0 => paren(format!("{} + {}", e(rng), e(rng))),
        1 => paren(format!("{} - {}", e(rng), e(rng))),
        2 => paren(format!("{} * {}", e(rng), e(rng))),
        3 => paren(format!("{} / {}.5", e(rng), rng.range(1, 9))),
        4 => format!("round({}, {})", e(rng), rng.range(0, 3)),
        5 => format!("abs({})", e(rng)),
        6 => format!("({})::numeric", expr(rng, scope, Cls::Int, d - 1)),
        _ => format!("COALESCE({}, {})", e(rng), e(rng)),
    }
}

fn text_expr(rng: &mut Rng, scope: &Scope, d: u32, leaf: bool) -> String {
    if leaf {
        if rng.chance(45) {
            if let Some(c) = col_of(rng, scope, Cls::Text) {
                return c;
            }
        }
        return if rng.chance(5) {
            "NULL".into()
        } else {
            text_lit(rng)
        };
    }
    let e = |rng: &mut Rng| expr(rng, scope, Cls::Text, d - 1);
    match rng.below(12) {
        0 | 1 => paren(format!("{} || {}", e(rng), e(rng))),
        2 => format!("upper({})", e(rng)),
        3 => format!("lower({})", e(rng)),
        4 => format!(
            "substr({}, {}, {})",
            e(rng),
            rng.range(-1, 5),
            rng.range(0, 5)
        ),
        5 => format!("trim({})", e(rng)),
        6 => format!("repeat({}, {})", e(rng), rng.range(0, 3)),
        7 => format!("replace({}, {}, {})", e(rng), text_lit(rng), text_lit(rng)),
        8 => format!("({})::text", expr(rng, scope, Cls::Int, d - 1)),
        9 => format!("left({}, {})", e(rng), rng.range(-2, 4)),
        10 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            expr(rng, scope, Cls::Bool, d - 1),
            e(rng),
            e(rng)
        ),
        _ => format!("COALESCE({}, {})", e(rng), e(rng)),
    }
}

fn bool_expr(rng: &mut Rng, scope: &Scope, d: u32, leaf: bool) -> String {
    if leaf {
        if rng.chance(30) {
            if let Some(c) = col_of(rng, scope, Cls::Bool) {
                return c;
            }
        }
        return rng.pick(&["TRUE", "FALSE", "NULL"]).to_string();
    }
    let b = |rng: &mut Rng| expr(rng, scope, Cls::Bool, d - 1);
    let i = |rng: &mut Rng| expr(rng, scope, Cls::Int, d - 1);
    let t = |rng: &mut Rng| expr(rng, scope, Cls::Text, d - 1);
    let op = |rng: &mut Rng| *rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
    match rng.weighted(&[8, 4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1]) {
        0 => paren(format!("{} {} {}", i(rng), op(rng), i(rng))),
        1 => paren(format!("{} {} {}", t(rng), op(rng), t(rng))),
        2 => paren(format!("{} AND {}", b(rng), b(rng))),
        3 => paren(format!("{} OR {}", b(rng), b(rng))),
        4 => format!("(NOT {})", b(rng)),
        5 => paren(format!("{} IS NULL", i(rng))),
        6 => paren(format!("{} IS NOT NULL", t(rng))),
        7 => paren(format!(
            "{} IN ({}, {}, {})",
            i(rng),
            i(rng),
            i(rng),
            i(rng)
        )),
        8 => paren(format!("{} BETWEEN {} AND {}", i(rng), i(rng), i(rng))),
        9 => paren(format!(
            "{} LIKE '{}'",
            t(rng),
            rng.pick(&["a%", "%b%", "_bc", "%", "H%o", "100"])
        )),
        10 => paren(format!("{} IS DISTINCT FROM {}", i(rng), i(rng))),
        11 => paren(format!(
            "{} {} {}",
            i(rng),
            op(rng),
            rng.pick(&["'1.5'::float8", "'-0.5'::float8", "'1e3'::float8"])
        )),
        _ => paren(format!("{} IS TRUE", b(rng))),
    }
}

pub fn random_cls(rng: &mut Rng) -> Cls {
    match rng.weighted(&[5, 4, 3, 2]) {
        0 => Cls::Int,
        1 => Cls::Text,
        2 => Cls::Bool,
        _ => Cls::Num,
    }
}

/// expr 領域: 定数式の SELECT を並べる。約 3 割はエラー文。
/// 既定では yuzhu が未対応の機能（numeric、GREATEST/LEAST、IS DISTINCT FROM、M4 以降の関数）を含む文を作り直して避ける。
pub fn scenario(ctx: &mut Ctx) {
    let allow = std::env::var("DIFFFUZZ_ALLOW_UNSUPPORTED").is_ok_and(|v| v == "1");
    let n = ctx.rng.range(6, 16);
    for _ in 0..n {
        if ctx.rng.chance(30) {
            let s = error_stmt(ctx);
            ctx.push(s);
            continue;
        }
        let mut s = String::new();
        for _ in 0..40 {
            s = gen_stmt(ctx, allow);
            let skip_missing = std::env::var("DIFFFUZZ_SKIP_MISSING").is_ok_and(|v| v == "1")
                && super::expr_extra::uses_missing(&s);
            if !skip_missing && (allow || !super::expr_extra::uses_unsupported(&s)) {
                break;
            }
        }
        ctx.push(s);
    }
}

fn gen_stmt(ctx: &mut Ctx, allow: bool) -> String {
    if ctx.rng.chance(60) {
        return super::expr_extra::stmt(&mut ctx.rng);
    }
    let k = ctx.rng.range(1, 3);
    let items: Vec<String> = (0..k)
        .map(|_| {
            let mut c = random_cls(&mut ctx.rng);
            if c == Cls::Num && !allow {
                c = Cls::Int;
            }
            let depth = ctx.rng.range(1, 4) as u32;
            expr(&mut ctx.rng, &[], c, depth)
        })
        .collect();
    format!("SELECT {};", items.join(", "))
}
