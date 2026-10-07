//! setop 領域: UNION / INTERSECT / EXCEPT（ALL の有無）、括弧による結合順、腕の WHERE・集約、ORDER BY / LIMIT、副問い合わせ・CTE の中の集合演算。
//!
//! 腕の列は同じ型（整数だけ int4 / int8 の混在を許す）に揃える。

use super::m4::{self, MTable, MTy};
use super::Ctx;
use crate::rng::Rng;

/// 列の並び（名前, 型）。全部の表が k, a を持つので、それに追加する列は選んだ表が持っているものだけ。
fn pick_cols(rng: &mut Rng, tabs: &[MTable]) -> Vec<(String, MTy)> {
    let mut cols: Vec<(String, MTy)> = vec![("k".into(), MTy::Int4), ("a".into(), MTy::Int4)];
    for (n, t) in m4::COLS.iter().skip(2) {
        if tabs.iter().all(|x| x.has(n)) && rng.chance(40) {
            cols.push((n.to_string(), *t));
        }
    }
    let n = rng.range(1, cols.len().min(3) as i64) as usize;
    if rng.chance(50) {
        cols.truncate(n);
    } else {
        m4::shuffle(rng, &mut cols);
        cols.truncate(n);
    }
    cols
}

fn arm(rng: &mut Rng, tabs: &[MTable], cols: &[(String, MTy)]) -> String {
    match rng.below(10) {
        0 => {
            // 定数の腕
            let items: Vec<String> = cols.iter().map(|(_, t)| m4::lit(rng, *t, 20)).collect();
            format!("SELECT {}", items.join(", "))
        }
        1 => {
            // 集約の腕（先頭の列でグループ化）。
            let t = rng.pick(tabs).clone();
            let first = &cols[0].0;
            let mut items = vec![format!("y.{first}")];
            for (c, ty) in &cols[1..] {
                items.push(match ty {
                    t if t.is_int() => format!("max(y.{c})"),
                    MTy::Bool => format!("bool_and(y.{c})"),
                    _ => format!("min(y.{c})"),
                });
            }
            format!(
                "SELECT {} FROM {} y GROUP BY y.{first}",
                items.join(", "),
                t.name
            )
        }
        _ => {
            let t = rng.pick(tabs).clone();
            let items: Vec<String> = cols
                .iter()
                .map(|(c, ty)| {
                    if *ty == MTy::Int4 && rng.chance(12) {
                        format!("y.{c}::bigint")
                    } else {
                        format!("y.{c}")
                    }
                })
                .collect();
            let mut s = format!("SELECT {} FROM {} y", items.join(", "), t.name);
            if rng.chance(45) {
                s.push_str(&format!(" WHERE {}", m4::pred(rng, "y", &t, 1)));
            }
            s
        }
    }
}

fn op(rng: &mut Rng) -> &'static str {
    rng.pick(&[
        "UNION",
        "UNION ALL",
        "INTERSECT",
        "INTERSECT ALL",
        "EXCEPT",
        "EXCEPT ALL",
        "UNION",
        "UNION ALL",
    ])
}

/// 腕 2〜4 個の集合演算（括弧つき・なし）。
pub fn setop_expr(rng: &mut Rng, tabs: &[MTable], cols: &[(String, MTy)]) -> String {
    let n = rng.range(2, 4) as usize;
    let mut s = arm(rng, tabs, cols);
    let mut paren_left = false;
    for i in 1..n {
        let a = arm(rng, tabs, cols);
        if rng.chance(25) && i == 1 && n > 2 {
            // 先頭の 2 腕を括弧で包む
            s = format!("({s} {} {a})", op(rng));
            paren_left = true;
        } else if rng.chance(20) {
            s = format!("{s} {} ({a} {} {})", op(rng), op(rng), arm(rng, tabs, cols));
        } else {
            s = format!("{s} {} {a}", op(rng));
        }
    }
    let _ = paren_left;
    s
}

pub fn scenario(ctx: &mut Ctx) {
    let nt = ctx.rng.range(2, 3) as usize;
    let tabs = m4::setup(ctx, nt, 40);
    for _ in 0..ctx.rng.range(6, 10) {
        let r = ctx.rng.below(100);
        let cols = pick_cols(&mut ctx.rng, &tabs);
        let rng = &mut ctx.rng;
        let (sql, ordered) = if r < 5 {
            // 列数・型の食い違い
            let t = rng.pick(&tabs).clone();
            (
                match rng.below(3) {
                    0 => format!(
                        "SELECT k, a FROM {} UNION SELECT k FROM {};",
                        t.name, t.name
                    ),
                    1 => format!("SELECT k FROM {} UNION SELECT 'x'::text;", t.name),
                    _ => format!(
                        "SELECT k FROM {} INTERSECT SELECT s FROM {};",
                        t.name, t.name
                    ),
                },
                false,
            )
        } else if r < 70 {
            let e = setop_expr(rng, &tabs, &cols);
            let ordered = rng.chance(85);
            let mut sql = e;
            if ordered {
                sql.push_str(&format!(
                    " ORDER BY {}",
                    m4::order_positions(rng, cols.len())
                ));
                sql.push_str(&m4::limit(rng));
            }
            sql.push(';');
            (sql, ordered)
        } else if r < 85 {
            // 集合演算を派生表・IN の中に置く
            let e = setop_expr(rng, &tabs, &cols[..1]);
            let c0 = &cols[0].0;
            let t = rng.pick(&tabs).clone();
            if rng.chance(50) {
                (format!("SELECT count(*) FROM ({e}) u;"), true)
            } else {
                (
                    format!(
                        "SELECT x.k, x.a FROM {} x WHERE x.{c0} IN ({e}) ORDER BY 1, 2;",
                        t.name
                    ),
                    true,
                )
            }
        } else {
            // CTE
            let e = setop_expr(rng, &tabs, &cols);
            let mut sql = format!("WITH u AS ({e}) SELECT * FROM u");
            let ordered = rng.chance(85);
            if ordered {
                sql.push_str(&format!(
                    " ORDER BY {}",
                    m4::order_positions(rng, cols.len())
                ));
            }
            sql.push(';');
            (sql, ordered)
        };
        let i = ctx.push_q(sql, ordered);
        if ordered && ctx.rng.chance(20) {
            m4::plan_variant(ctx, i, false);
        }
    }
}
