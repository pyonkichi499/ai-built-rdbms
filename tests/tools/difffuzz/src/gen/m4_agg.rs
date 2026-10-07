//! agg 領域: GROUP BY / HAVING、主キーへの関数従属、DISTINCT ON、集約関数（FILTER・DISTINCT 引数）。
//!
//! 浮動小数は使わない（sum / avg の加算順で結果が変わるため）。ウィンドウ関数・GROUPING SETS・集約内の ORDER BY・
//! string_agg などは生成しない（KD-7）。外側の集約（KD-8）と `GROUP BY (SELECT ...)`（KD-21）も避ける。

use super::m4::{self, MTable, MTy};
use super::m4_join::{self, Chain, Item};
use super::Ctx;
use crate::rng::Rng;

/// 型に合う集約式（引数は `col`）。
fn agg_expr(rng: &mut Rng, col: &str, ty: MTy) -> String {
    let distinct = if rng.chance(15) { "DISTINCT " } else { "" };
    match ty {
        MTy::Int4 | MTy::Int8 => match rng.below(7) {
            0 => format!("count({distinct}{col})"),
            1 => format!("sum({distinct}{col})"),
            2 => format!("min({col})"),
            3 => format!("max({col})"),
            4 => format!("avg({distinct}{col})"),
            5 => "count(*)".to_string(),
            _ => format!("sum({col} + 1)"),
        },
        MTy::Num => match rng.below(5) {
            0 => format!("sum({col})"),
            1 => format!("min({col})"),
            2 => format!("max({col})"),
            3 => format!("count({distinct}{col})"),
            _ => format!("avg({col})"),
        },
        MTy::Text | MTy::Chr | MTy::Date | MTy::Ts => match rng.below(4) {
            0 => format!("min({col})"),
            1 => format!("max({col})"),
            2 => format!("count({distinct}{col})"),
            _ => format!("count({col})"),
        },
        MTy::Bool => match rng.below(3) {
            0 => format!("bool_and({col})"),
            1 => format!("bool_or({col})"),
            _ => format!("count({col})"),
        },
    }
}

/// FILTER を付けることがある。
fn agg_item(rng: &mut Rng, it: &Item) -> String {
    let (c, ty) = rng.pick(&it.cols).clone();
    let col = format!("{}.{c}", it.alias);
    let mut e = agg_expr(rng, &col, ty);
    if rng.chance(22) {
        let t = m4_join::item_table(it);
        e.push_str(&format!(
            " FILTER (WHERE {})",
            m4::pred(rng, &it.alias, &t, 0)
        ));
    }
    e
}

fn having(rng: &mut Rng, items: &[Item], group_cols: &[String]) -> String {
    let it = rng.pick(items);
    let ints: Vec<&(String, MTy)> = it.cols.iter().filter(|(_, t)| t.is_int()).collect();
    match rng.below(5) {
        0 => format!("count(*) > {}", rng.range(0, 3)),
        1 if !ints.is_empty() => {
            let (c, _) = rng.pick(&ints);
            format!("max({}.{c}) IS NOT NULL", it.alias)
        }
        2 if !ints.is_empty() => {
            let (c, _) = rng.pick(&ints);
            format!("sum({}.{c}) >= {}", it.alias, rng.range(0, 8))
        }
        3 if !group_cols.is_empty() => {
            let g = rng.pick(group_cols);
            format!("{g} IS NOT NULL AND count(*) <= {}", rng.range(1, 5))
        }
        _ => format!("count(*) <> {}", rng.range(0, 3)),
    }
}

fn from_part(ctx: &mut Ctx, tabs: &[MTable], multi: bool) -> Chain {
    let n = if multi {
        ctx.rng.range(2, 3) as usize
    } else {
        1
    };
    // 集約の入力に USING / NATURAL は混ぜない（列名の曖昧さを避ける）
    m4_join::chain(&mut ctx.rng, tabs, n, true, false)
}

/// GROUP BY 付き / なしの集約問い合わせ。
fn grouped(ctx: &mut Ctx, tabs: &[MTable]) -> (String, bool, bool) {
    let multi = ctx.rng.chance(35);
    let ch = from_part(ctx, tabs, multi);
    let rng = &mut ctx.rng;
    // グループ列: 低基数の列を 0〜2 本
    let ng = rng.range(0, 2) as usize;
    let mut group: Vec<String> = Vec::new();
    for _ in 0..ng {
        let it = rng.pick(&ch.items);
        let cands: Vec<&(String, MTy)> = it
            .cols
            .iter()
            .filter(|(c, _)| c != "cnt" && c != "n" && c != "ts")
            .collect();
        if cands.is_empty() {
            continue;
        }
        let (c, _) = rng.pick(&cands);
        let r = format!("{}.{c}", it.alias);
        if !group.contains(&r) {
            group.push(r);
        }
    }
    let mut sel: Vec<String> = Vec::new();
    for g in &group {
        sel.push(if rng.chance(15) && g.ends_with(".a") {
            format!("{g} % 3")
        } else {
            g.clone()
        });
    }
    // 式でグループ化したときは SELECT の式と GROUP BY の式を揃える
    let group_exprs: Vec<String> = group
        .iter()
        .zip(&sel)
        .map(|(g, s)| if s != g { s.clone() } else { g.clone() })
        .collect();
    let nagg = rng.range(1, 3) as usize;
    for _ in 0..nagg {
        let it = rng.pick(&ch.items).clone();
        sel.push(agg_item(rng, &it));
    }
    let mut sql = format!("SELECT {} FROM {}", sel.join(", "), ch.from);
    if rng.chance(35) {
        let it = rng.pick(&ch.items).clone();
        let w = m4::pred(rng, &it.alias, &m4_join::item_table(&it), 1);
        sql.push_str(&format!(" WHERE {w}"));
    }
    if !group_exprs.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group_exprs.join(", ")));
    }
    if rng.chance(40) {
        let h = having(rng, &ch.items, &group_exprs);
        sql.push_str(&format!(" HAVING {h}"));
    }
    let ordered = rng.chance(80);
    if ordered {
        sql.push_str(&format!(
            " ORDER BY {}",
            m4::order_positions(rng, sel.len())
        ));
        sql.push_str(&m4::limit(rng));
    }
    sql.push(';');
    (sql, ordered, ch.has_full)
}

/// 主キーへの関数従属: 主キーだけでグループ化して、同じ表のほかの列を SELECT / HAVING に書く。
fn pk_dependent(ctx: &mut Ctx, tabs: &[MTable]) -> Option<(String, bool, bool)> {
    let pks: Vec<&MTable> = tabs.iter().filter(|t| !t.pk.is_empty()).collect();
    if pks.is_empty() {
        return None;
    }
    let x = (*ctx.rng.pick(&pks)).clone();
    let y = ctx.rng.pick(tabs).clone();
    let rng = &mut ctx.rng;
    let mut sel: Vec<String> = x.pk.iter().map(|c| format!("x.{c}")).collect();
    for (c, _) in &x.cols {
        if !x.pk.contains(c) && rng.chance(45) {
            sel.push(format!("x.{c}"));
        }
    }
    let yitem = Item {
        alias: "y".into(),
        cols: y.cols.clone(),
        src: y.name.clone(),
        plain: true,
    };
    for _ in 0..rng.range(1, 2) {
        sel.push(agg_item(rng, &yitem));
    }
    let jt = match rng.below(3) {
        0 => "JOIN",
        1 => "LEFT JOIN",
        _ => "RIGHT JOIN",
    };
    let mut sql = format!(
        "SELECT {} FROM {} x {jt} {} y ON y.k = x.k",
        sel.join(", "),
        x.name,
        y.name
    );
    if rng.chance(30) {
        sql.push_str(&format!(" WHERE {}", m4::pred(rng, "y", &y, 0)));
    }
    sql.push_str(&format!(
        " GROUP BY {}",
        x.pk.iter()
            .map(|c| format!("x.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if rng.chance(35) {
        sql.push_str(&format!(" HAVING count(y.k) > {}", rng.range(0, 2)));
    }
    sql.push_str(&format!(
        " ORDER BY {}",
        m4::order_positions(rng, sel.len())
    ));
    sql.push(';');
    // RIGHT JOIN のとき x.k は NULL になりうるが、PK でグループ化した x の列は同じ NULL 群に入る（PG と同じ結果になるはず）
    Some((sql, true, false))
}

/// DISTINCT ON（ON の式を先頭に並べ、残りの出力列もすべて ORDER BY に入れて結果を決める）。
fn distinct_on(ctx: &mut Ctx, tabs: &[MTable]) -> (String, bool, bool) {
    let multi = ctx.rng.chance(30);
    let ch = from_part(ctx, tabs, multi);
    let rng = &mut ctx.rng;
    let mut cols: Vec<String> = Vec::new();
    for _ in 0..rng.range(2, 4) {
        let r = m4_join::pick_ref(rng, &ch.items);
        if !cols.contains(&r) {
            cols.push(r);
        }
    }
    let non = rng.range(1, cols.len().max(1) as i64) as usize;
    let on = cols[..non.min(cols.len())].to_vec();
    let mut sql = format!(
        "SELECT DISTINCT ON ({}) {} FROM {}",
        on.join(", "),
        cols.join(", "),
        ch.from
    );
    if rng.chance(30) {
        let it = rng.pick(&ch.items).clone();
        sql.push_str(&format!(
            " WHERE {}",
            m4::pred(rng, &it.alias, &m4_join::item_table(&it), 0)
        ));
    }
    sql.push_str(&format!(
        " ORDER BY {}",
        m4::order_positions(rng, cols.len())
    ));
    sql.push_str(&m4::limit(rng));
    sql.push(';');
    (sql, true, ch.has_full)
}

fn agg_errors(ctx: &mut Ctx, tabs: &[MTable]) -> String {
    let t = ctx.rng.pick(tabs).clone();
    match ctx.rng.below(6) {
        0 => format!("SELECT x.a, count(*) FROM {} x GROUP BY x.k;", t.name),
        1 => format!("SELECT max(x.k), x.k FROM {} x;", t.name),
        2 => format!("SELECT count(*) FROM {} x HAVING x.k > 1;", t.name),
        3 => format!("SELECT sum(x.s) FROM {} x;", t.name),
        4 => format!(
            "SELECT x.k FROM {} x GROUP BY x.k HAVING sum(count(*)) > 1;",
            t.name
        ),
        _ => format!(
            "SELECT x.k FROM {} x WHERE sum(x.a) > 1 GROUP BY x.k;",
            t.name
        ),
    }
}

pub fn scenario(ctx: &mut Ctx) {
    let nt = ctx.rng.range(2, 3) as usize;
    let tabs = m4::setup(ctx, nt, 15);
    for _ in 0..ctx.rng.range(5, 9) {
        let r = ctx.rng.below(100);
        let (sql, ordered, has_full) = if r < 6 {
            (agg_errors(ctx, &tabs), false, true)
        } else if r < 30 {
            match pk_dependent(ctx, &tabs) {
                Some(x) => x,
                None => grouped(ctx, &tabs),
            }
        } else if r < 45 {
            distinct_on(ctx, &tabs)
        } else {
            grouped(ctx, &tabs)
        };
        let i = ctx.push_q(sql, ordered);
        if ordered && ctx.rng.chance(25) {
            m4::plan_variant(ctx, i, has_full);
        }
    }
}
