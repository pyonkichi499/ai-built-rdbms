//! query 領域: 単一テーブルの SELECT（式・WHERE・ORDER BY・LIMIT/OFFSET・DISTINCT・VALUES・別名・式の型推論）。

use super::expr::{expr, random_cls};
use super::{error_stmt, insert_row_sql, make_table, Cls, Ctx, Table, Ty};

const TYS: [Ty; 9] =
    [Ty::Int4, Ty::Int4, Ty::Int8, Ty::Int2, Ty::Text, Ty::Varchar(5), Ty::Bool, Ty::Numeric, Ty::Float8];

fn num_expr(ctx: &mut Ctx, scope: &[(String, Cls)]) -> String {
    let c = random_cls(&mut ctx.rng);
    let d = ctx.rng.range(1, 3) as u32;
    expr(&mut ctx.rng, scope, c, d)
}

pub fn dir(ctx: &mut Ctx) -> &'static str {
    match ctx.rng.below(7) {
        0 => " DESC",
        1 => " ASC",
        2 => " NULLS FIRST",
        3 => " NULLS LAST",
        4 => " DESC NULLS LAST",
        5 => " ASC NULLS FIRST",
        _ => "",
    }
}

pub fn limit_offset(ctx: &mut Ctx, sql: &mut String) {
    if ctx.rng.chance(45) {
        match ctx.rng.below(9) {
            0 => sql.push_str(" LIMIT ALL"),
            1 => sql.push_str(" LIMIT NULL"),
            2 => sql.push_str(&format!(" FETCH FIRST {} ROWS ONLY", ctx.rng.range(0, 4))),
            3 => sql.push_str(" FETCH NEXT ROW ONLY"),
            4 => sql.push_str(&format!(" LIMIT {} + 1", ctx.rng.range(0, 3))),
            5 => sql.push_str(&format!(" LIMIT {}::bigint", ctx.rng.range(0, 3))),
            6 if ctx.rng.chance(30) => sql.push_str(&format!(" LIMIT {}", -ctx.rng.range(1, 3))),
            _ => sql.push_str(&format!(" LIMIT {}", ctx.rng.range(0, 6))),
        }
    }
    if ctx.rng.chance(25) {
        match ctx.rng.below(5) {
            0 => sql.push_str(&format!(" OFFSET {} ROWS", ctx.rng.range(0, 4))),
            1 => sql.push_str(" OFFSET NULL"),
            2 if ctx.rng.chance(30) => sql.push_str(&format!(" OFFSET {}", -ctx.rng.range(1, 3))),
            _ => sql.push_str(&format!(" OFFSET {}", ctx.rng.range(0, 4))),
        }
    }
}

fn where_clause(ctx: &mut Ctx, t: &Table, scope: &[(String, Cls)]) -> String {
    let d = ctx.rng.range(1, 3) as u32;
    let col = ctx.rng.pick(&t.cols).clone();
    let cname = col.name.clone();
    let (l1, l2, l3) = match col.ty {
        Ty::Text | Ty::Varchar(_) => ("'a'", "'abc'", "'zz'"),
        Ty::Bool => ("true", "false", "true"),
        _ => ("1", "2", "50"),
    };
    let _ = ctx.rng.below(1);
    match ctx.rng.below(14) {
        0 => format!("{cname} IS NULL"),
        1 => format!("{cname} IS NOT NULL"),
        2 => format!("{cname} IN ({l1}, {l2}, NULL)"),
        3 => format!("{cname} NOT IN ({l1}, {l2})"),
        4 => format!("{cname} BETWEEN {l1} AND {l3}"),
        5 => format!("{cname} IS DISTINCT FROM {cname}"),
        6 => format!("({}) IS TRUE", expr(&mut ctx.rng, scope, Cls::Bool, d)),
        7 => format!("({}) IS NOT FALSE", expr(&mut ctx.rng, scope, Cls::Bool, d)),
        8 => format!("({}) IS UNKNOWN", expr(&mut ctx.rng, scope, Cls::Bool, d)),
        9 => "NULL".into(),
        10 => (if ctx.rng.chance(50) { "true" } else { "false" }).into(),
        11 => format!("{} OR {}", expr(&mut ctx.rng, scope, Cls::Bool, d), expr(&mut ctx.rng, scope, Cls::Bool, d)),
        12 => format!("NOT ({})", expr(&mut ctx.rng, scope, Cls::Bool, d)),
        _ => expr(&mut ctx.rng, scope, Cls::Bool, d),
    }
}

fn values_stmt(ctx: &mut Ctx) -> String {
    let ncol = ctx.rng.range(1, 3) as usize;
    let nrow = ctx.rng.range(1, 4) as usize;
    let pool = [
        "1", "2", "NULL", "2.5", "'a'", "'b'", "true", "-3", "10000000000", "0.10", "'1'", "1.0e0", "NULL::text", "2147483647",
    ];
    // 列ごとに型の系統を揃えることが多い（混ぜるとエラー・型昇格の確認になる）
    let fam: Vec<&[&str]> = (0..ncol)
        .map(|_| match ctx.rng.below(5) {
            0 => &["1", "2", "NULL", "-3", "2147483647"][..],
            1 => &["1", "2.5", "NULL", "0.10", "10000000000"][..],
            2 => &["'a'", "'b'", "NULL", "'1'", "''"][..],
            3 => &["true", "NULL", "false"][..],
            _ => &pool[..],
        })
        .collect();
    let rows: Vec<String> = (0..nrow)
        .map(|_| {
            let v: Vec<&str> = fam.iter().map(|f| *ctx.rng.pick(f)).collect();
            format!("({})", v.join(", "))
        })
        .collect();
    let order: Vec<String> = (1..=ncol).map(|i| format!("{i}{}", dir(ctx))).collect();
    let mut sql = match ctx.rng.below(3) {
        0 => format!("VALUES {} ORDER BY {}", rows.join(", "), order.join(", ")),
        1 => {
            let cols: Vec<String> = (0..ncol).map(|i| format!("v{i}")).collect();
            let _ = cols; format!("VALUES {} ORDER BY {} OFFSET 0", rows.join(", "), order.join(", "))
        }
        _ => format!("VALUES {} ORDER BY 1{} LIMIT 3", rows.join(", "), dir(ctx)),
    };
    limit_offset(ctx, &mut sql);
    sql.push(';');
    sql
}

fn typeof_stmt(ctx: &mut Ctx, scope: &[(String, Cls)], t: &Table) -> String {
    // 式の型推論: pg_typeof と、演算の結果型（int2+int8, numeric/float 混在など）
    let a = ctx.rng.pick(&t.cols).name.clone();
    let b = ctx.rng.pick(&t.cols).name.clone();
    let lits = ["1", "1.5", "1::smallint", "1::bigint", "'x'", "NULL", "true", "1e0", "'1'", "2147483648"];
    let l = *ctx.rng.pick(&lits);
    let e = match ctx.rng.below(8) {
        0 => format!("{a} + {l}"),
        1 => format!("{a} * {b}"),
        2 => format!("COALESCE({a}, {l})"),
        3 => format!("CASE WHEN {a} IS NULL THEN {l} ELSE {b} END"),
        4 => format!("GREATEST({a}, {l})"),
        5 => format!("NULLIF({a}, {l})"),
        6 => format!("{a} = {l}"),
        _ => num_expr(ctx, scope),
    };
    format!("SELECT pg_typeof({e})::text, {e} FROM {} ORDER BY 2, 1 LIMIT {};", t.name, ctx.rng.range(1, 3))
}

pub fn scenario(ctx: &mut Ctx) {
    if ctx.rng.chance(35) {
        super::query_extra::xtable_scenario(ctx);
        if ctx.rng.chance(50) {
            let s = super::query_extra::extra_values(ctx);
            ctx.push(s);
        }
        return;
    }
    let ncols = ctx.rng.range(2, 5) as usize;
    let ti = make_table(ctx, ncols, &TYS, false);
    for _ in 0..ctx.rng.range(1, 3) {
        let n = ctx.rng.range(2, 8) as usize;
        let s = insert_row_sql(ctx, ti, n, 0);
        ctx.push(s);
    }
    let t = ctx.tables[ti].clone();
    let scope = t.scope();
    for _ in 0..ctx.rng.range(5, 10) {
        let r = ctx.rng.below(100);
        if r < 20 {
            let s = error_stmt(ctx);
            ctx.push(s);
            continue;
        }
        if r < 34 {
            super::query_r3::r3_stmt(ctx, &t);
            continue;
        }
        if r < 42 {
            super::query_extra::structure_extra(ctx, &t);
            continue;
        }
        if r < 46 {
            let s = values_stmt(ctx);
            ctx.push(s);
            continue;
        }
        if r < 54 {
            let s = typeof_stmt(ctx, &scope, &t);
            ctx.push(s);
            continue;
        }
        if r < 60 {
            // FROM なしの SELECT（別名・型・LIMIT）
            let e = num_expr(ctx, &[]);
            let mut sql = format!("SELECT {e} AS a, pg_typeof({e})::text AS \"T Y\"");
            if ctx.rng.chance(30) {
                sql.push_str(" WHERE ");
                sql.push_str(&expr(&mut ctx.rng, &[], Cls::Bool, 2));
            }
            limit_offset(ctx, &mut sql);
            sql.push(';');
            ctx.push(sql);
            continue;
        }
        // 別名・テーブル別名・列の並び
        let use_alias = ctx.rng.chance(25);
        let tref = if use_alias { "x".to_string() } else { t.name.clone() };
        let from = if use_alias { format!("{} AS x", t.name) } else { t.name.clone() };
        let mut items: Vec<String> = Vec::new();
        let mut aliases: Vec<Option<String>> = Vec::new();
        match ctx.rng.below(6) {
            0 | 1 => {
                items.push("*".into());
            }
            2 => {
                items.push(format!("{tref}.*"));
            }
            _ => {}
        }
        let star = !items.is_empty();
        let k = ctx.rng.range(if star { 0 } else { 1 }, 3) as usize;
        for i in 0..k {
            let e = if ctx.rng.chance(25) {
                let c = ctx.rng.pick(&t.cols).name.clone();
                if use_alias && ctx.rng.chance(50) { format!("{tref}.{c}") } else { c }
            } else {
                num_expr(ctx, &scope)
            };
            match ctx.rng.below(6) {
                0 => {
                    items.push(format!("{e} AS r{i}"));
                    aliases.push(Some(format!("r{i}")));
                }
                1 => {
                    items.push(format!("{e} r{i}"));
                    aliases.push(Some(format!("r{i}")));
                }
                2 => {
                    items.push(format!("{e} AS \"Mixed Case\""));
                    aliases.push(None);
                }
                _ => {
                    items.push(e);
                    aliases.push(None);
                }
            }
        }
        let width = if star {
            (if items[0] == "*" || items[0].ends_with(".*") { t.cols.len() } else { 0 }) + k
        } else {
            items.len()
        };
        // DISTINCT / DISTINCT ON
        let distinct = match ctx.rng.below(100) {
            0..=11 => "DISTINCT ",
            12..=15 => "ALL ",
            _ => "",
        };
        let mut sql = format!("SELECT {distinct}{} FROM {from}", items.join(", "));
        if ctx.rng.chance(60) {
            let w = where_clause(ctx, &t, &scope);
            sql.push_str(&format!(" WHERE {w}"));
        }
        // ORDER BY: 先頭に別の指定（別名・列名・式）を置き、最後に全位置番号で順序を確定させる
        let mut order: Vec<String> = Vec::new();
        match ctx.rng.below(10) {
            0 => {
                if let Some(Some(a)) = aliases.iter().find(|a| a.is_some()) {
                    order.push(format!("{a}{}", dir(ctx)));
                }
            }
            1 if !distinct.starts_with("DISTINCT") => {
                let c = ctx.rng.pick(&t.cols).name.clone();
                order.push(format!("{c}{}", dir(ctx)));
            }
            2 if !distinct.starts_with("DISTINCT") => {
                let c = ctx.rng.pick(&t.cols).name.clone();
                order.push(format!("({c}) IS NULL{}", dir(ctx)));
            }
            _ => {}
        }
        for i in 1..=width.max(1) {
            order.push(format!("{i}{}", dir(ctx)));
        }
        sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
        limit_offset(ctx, &mut sql);
        sql.push(';');
        ctx.push(sql);
    }
}

/// DIFFFUZZ_SKIP_FUNCS=1 のとき、yuzhu 未実装が分かっている関数を含む文を捨てる（他の差分が隠れないように）。
pub fn drop_known_missing(ctx: &mut Ctx) {
    if std::env::var("DIFFFUZZ_SKIP_FUNCS").is_err() {
        return;
    }
    const NEEDLES: [&str; 6] = ["position(", "round(", "substr(", "trim(", "replace(", "left("];
    ctx.stmts.retain(|s| !(s.starts_with("SELECT") && NEEDLES.iter().any(|n| s.contains(n))));
}
