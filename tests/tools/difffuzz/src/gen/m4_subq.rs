//! subquery 領域: IN / NOT IN / EXISTS / スカラー / ANY / ALL、NULL を含む表、相関あり、UNION の腕からの相関、CTE、派生表。
//!
//! 生成しないもの: LATERAL（KD-3）、再帰・データ変更 CTE（KD-4）、`ANY` / `ALL` の配列形・行値（KD-9）、
//! 外側の集約（KD-8）、`GROUP BY (SELECT ...)`（KD-21）、相関 CTE の `MATERIALIZED`（KD-23）。
//! 複数行を返すスカラー副問い合わせ（21000）は評価順で出る出ないが変わりうるので、比較側が inconclusive として扱う。

use super::m4::{self, MTable};
use super::Ctx;
use crate::rng::Rng;

fn cmp_op(rng: &mut Rng) -> &'static str {
    rng.pick(&["=", "<>", "<", "<=", ">", ">="])
}

/// 内側の表 `y`（別名）の WHERE。`outer` があれば外側 `x` への相関を含めることがある。
fn inner_where(rng: &mut Rng, y: &MTable, xa: Option<&MTable>, corr_pct: u64) -> String {
    let mut conds = Vec::new();
    if xa.is_some() && rng.chance(corr_pct) {
        conds.push(match rng.below(4) {
            0 => "y.k = x.k".to_string(),
            1 => "y.a = x.a".to_string(),
            2 => format!("y.k {} x.a", cmp_op(rng)),
            _ => "y.a > x.a".to_string(),
        });
    }
    if rng.chance(40) {
        conds.push(m4::pred(rng, "y", y, 1));
    }
    if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    }
}

/// 外側 `x`（表 xt）の WHERE に置く述語。`ts` は使える表（y, z 用）。
fn sub_pred(rng: &mut Rng, xt: &MTable, yt: &MTable, zt: &MTable, depth: u32) -> String {
    if depth > 0 && rng.chance(25) {
        let a = sub_pred(rng, xt, yt, zt, depth - 1);
        let b = sub_pred(rng, xt, yt, zt, depth - 1);
        return match rng.below(3) {
            0 => format!("({a} AND {b})"),
            1 => format!("({a} OR {b})"),
            _ => format!("NOT ({a})"),
        };
    }
    let w = inner_where(rng, yt, Some(xt), 60);
    match rng.below(17) {
        0 => format!("x.k IN (SELECT y.k FROM {} y{w})", yt.name),
        1 => format!("x.k NOT IN (SELECT y.k FROM {} y{w})", yt.name),
        2 => format!("x.a IN (SELECT y.a FROM {} y{w})", yt.name),
        3 => format!("EXISTS (SELECT 1 FROM {} y{w})", yt.name),
        4 => format!("NOT EXISTS (SELECT 1 FROM {} y{w})", yt.name),
        5 => {
            let agg = *rng.pick(&["max(y.a)", "min(y.a)", "count(*)", "sum(y.a)", "max(y.k)"]);
            format!("x.a {} (SELECT {agg} FROM {} y{w})", cmp_op(rng), yt.name)
        }
        6 => format!("x.a {} ANY (SELECT y.a FROM {} y{w})", cmp_op(rng), yt.name),
        7 => format!("x.a {} ALL (SELECT y.a FROM {} y{w})", cmp_op(rng), yt.name),
        8 => format!("x.k = ANY (SELECT y.k FROM {} y{w})", yt.name),
        9 => format!("x.k <> ALL (SELECT y.k FROM {} y{w})", yt.name),
        // UNION の腕からの相関（外側の列を腕の WHERE から参照する）
        10 => format!(
            "EXISTS (SELECT 1 FROM {} y WHERE y.k = x.k UNION ALL SELECT 1 FROM {} z WHERE z.a = x.a)",
            yt.name, zt.name
        ),
        11 => format!(
            "x.k IN (SELECT y.k FROM {} y WHERE y.a <= x.a UNION SELECT z.k FROM {} z WHERE z.a > {})",
            yt.name,
            zt.name,
            rng.range(0, 5)
        ),
        12 => format!(
            "x.a NOT IN (SELECT y.a FROM {} y WHERE y.k = x.k INTERSECT SELECT z.a FROM {} z)",
            yt.name, zt.name
        ),
        13 => format!(
            "(SELECT count(*) FROM (SELECT y.k FROM {} y WHERE y.k = x.k UNION ALL SELECT z.k FROM {} z WHERE z.k = x.k) u) {} {}",
            yt.name,
            zt.name,
            cmp_op(rng),
            rng.range(0, 3)
        ),
        // 2 段の相関
        14 => format!(
            "x.k IN (SELECT y.k FROM {} y WHERE EXISTS (SELECT 1 FROM {} z WHERE z.k = y.k AND z.a = x.a))",
            yt.name, zt.name
        ),
        15 => format!(
            "EXISTS (SELECT 1 FROM {} y WHERE y.k = x.k AND y.a IN (SELECT z.a FROM {} z WHERE z.k <> x.k))",
            yt.name, zt.name
        ),
        // 複数行を返しうるスカラー（21000 の可能性。評価順で変わりうる）
        _ => format!("x.a = (SELECT y.a FROM {} y WHERE y.k = x.k)", yt.name),
    }
}

/// SELECT 句の副問い合わせ。
fn sub_item(rng: &mut Rng, xt: &MTable, yt: &MTable) -> String {
    let w = inner_where(rng, yt, Some(xt), 80);
    match rng.below(6) {
        0 => format!("(SELECT count(*) FROM {} y{w})", yt.name),
        1 => format!("(SELECT max(y.a) FROM {} y{w})", yt.name),
        2 => format!("EXISTS (SELECT 1 FROM {} y{w})", yt.name),
        3 => format!("x.k IN (SELECT y.k FROM {} y{w})", yt.name),
        4 => format!("x.a NOT IN (SELECT y.a FROM {} y{w})", yt.name),
        _ => format!("(SELECT min(y.k) FROM {} y{w})", yt.name),
    }
}

fn outer_select(ctx: &mut Ctx, tabs: &[MTable]) -> (String, bool) {
    let xt = ctx.rng.pick(tabs).clone();
    let yt = ctx.rng.pick(tabs).clone();
    let zt = ctx.rng.pick(tabs).clone();
    let rng = &mut ctx.rng;
    let mut sel: Vec<String> = vec!["x.k".into(), "x.a".into()];
    if rng.chance(30) {
        let (c, _) = rng.pick(&xt.cols).clone();
        if c != "k" && c != "a" {
            sel.push(format!("x.{c}"));
        }
    }
    if rng.chance(30) {
        sel.push(sub_item(rng, &xt, &yt));
    }
    let mut sql = format!("SELECT {} FROM {} x", sel.join(", "), xt.name);
    if rng.chance(85) {
        let p = sub_pred(rng, &xt, &yt, &zt, 1);
        sql.push_str(&format!(" WHERE {p}"));
    }
    let ordered = rng.chance(85);
    if ordered {
        sql.push_str(&format!(
            " ORDER BY {}",
            m4::order_positions(rng, sel.len())
        ));
        sql.push_str(&m4::limit(rng));
    }
    sql.push(';');
    (sql, ordered)
}

/// CTE（参照 0・1・2 回、MATERIALIZED / NOT MATERIALIZED）。相関のない CTE だけ。
fn cte_select(ctx: &mut Ctx, tabs: &[MTable]) -> (String, bool) {
    let xt = ctx.rng.pick(tabs).clone();
    let yt = ctx.rng.pick(tabs).clone();
    let rng = &mut ctx.rng;
    let mat = *rng.pick(&["", "", "MATERIALIZED ", "NOT MATERIALIZED "]);
    let w = if rng.chance(50) {
        format!(" WHERE {}", m4::pred(rng, "y", &yt, 1))
    } else {
        String::new()
    };
    let mut sql = format!("WITH c0 AS {mat}(SELECT y.k, y.a FROM {} y{w})", yt.name);
    let second = rng.chance(40);
    if second {
        let m2 = *rng.pick(&["", "MATERIALIZED ", "NOT MATERIALIZED "]);
        sql.push_str(&format!(
            ", c1 AS {m2}(SELECT k, count(*) AS n FROM c0 GROUP BY k)"
        ));
    }
    let refs = rng.below(4); // 0: 参照なし、1: 1 回、2: 2 回（自己結合）、3: 副問い合わせの中
    let (body, n) = match refs {
        0 => (format!("SELECT x.k, x.a FROM {} x", xt.name), 2),
        1 => {
            if second {
                (format!("SELECT x.k, x.a, c1.n FROM {} x LEFT JOIN c1 ON c1.k = x.k", xt.name), 3)
            } else {
                (format!("SELECT x.k, x.a, c0.a FROM {} x JOIN c0 ON c0.k = x.k", xt.name), 3)
            }
        }
        2 => {
            if second {
                ("SELECT p.k, p.a, q.n FROM c0 p JOIN c1 q ON q.k = p.k".to_string(), 3)
            } else {
                ("SELECT p.k, p.a, q.a FROM c0 p JOIN c0 q ON q.k = p.k".to_string(), 3)
            }
        }
        _ => (
            format!(
                "SELECT x.k, x.a FROM {} x WHERE x.k IN (SELECT k FROM c0) AND x.a NOT IN (SELECT a FROM c0 WHERE a IS NOT NULL)",
                xt.name
            ),
            2,
        ),
    };
    sql.push(' ');
    sql.push_str(&body);
    let ordered = rng.chance(85);
    if ordered {
        sql.push_str(&format!(" ORDER BY {}", m4::order_positions(rng, n)));
        sql.push_str(&m4::limit(rng));
    }
    sql.push(';');
    (sql, ordered)
}

/// 派生表と generate_series を使う副問い合わせ。
fn derived(ctx: &mut Ctx, tabs: &[MTable]) -> (String, bool) {
    let xt = ctx.rng.pick(tabs).clone();
    let yt = ctx.rng.pick(tabs).clone();
    let rng = &mut ctx.rng;
    let sql = match rng.below(4) {
        0 => format!(
            "SELECT d.k, d.m FROM (SELECT y.k, max(y.a) AS m FROM {} y GROUP BY y.k) d WHERE d.m > {} ORDER BY 1, 2;",
            yt.name,
            rng.range(0, 4)
        ),
        1 => format!(
            "SELECT g, (SELECT count(*) FROM {} x WHERE x.k = g) FROM generate_series(0, {}) AS g ORDER BY 1, 2;",
            xt.name,
            rng.range(2, 6)
        ),
        2 => format!(
            "SELECT x.k, x.a FROM {} x WHERE x.k IN (SELECT g FROM generate_series(1, {}) AS g) ORDER BY 1, 2;",
            xt.name,
            rng.range(1, 5)
        ),
        _ => format!(
            "SELECT count(*), sum(d.c) FROM (SELECT y.k, count(*) AS c FROM {} y WHERE y.k IS NOT NULL GROUP BY y.k) d;",
            yt.name
        ),
    };
    (sql, true)
}

pub fn scenario(ctx: &mut Ctx) {
    // NULL の多い表（NOT IN / ALL の三値論理）
    let nt = ctx.rng.range(2, 3) as usize;
    let tabs = m4::setup(ctx, nt, 25);
    for _ in 0..ctx.rng.range(6, 10) {
        let r = ctx.rng.below(100);
        let (sql, ordered) = if r < 6 {
            (m4::error_query(ctx, &tabs), false)
        } else if r < 30 {
            cte_select(ctx, &tabs)
        } else if r < 40 {
            derived(ctx, &tabs)
        } else {
            outer_select(ctx, &tabs)
        };
        let i = ctx.push_q(sql, ordered);
        if ordered && ctx.rng.chance(20) {
            m4::plan_variant(ctx, i, false);
        }
    }
}
