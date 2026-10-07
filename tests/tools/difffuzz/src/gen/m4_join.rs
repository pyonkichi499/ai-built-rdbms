//! join 領域: INNER / LEFT / RIGHT / FULL（等値）/ CROSS、USING / NATURAL、連鎖、派生表、generate_series。
//!
//! 結果は多重集合で比べる。ORDER BY が全出力列のときだけ順序も比べる。
//! 生成しないもの: `FULL JOIN ... ON true`（KD-2）、LATERAL（KD-3）、`USING (...) AS j`（KD-10）。

use super::m4::{self, MTable, MTy};
use super::Ctx;
use crate::rng::Rng;
use std::collections::BTreeMap;

/// FROM 句の 1 要素。
#[derive(Clone)]
pub struct Item {
    pub alias: String,
    pub cols: Vec<(String, MTy)>,
    /// 表名・派生表・関数呼び出し（`AS alias` の前まで）
    pub src: String,
    /// NATURAL / USING の対象にしてよい（通常の表と派生表）
    pub plain: bool,
}

pub struct Chain {
    pub from: String,
    pub items: Vec<Item>,
    /// `SELECT *` の列数
    pub star_n: usize,
    pub has_full: bool,
}

fn table_item(alias: &str, t: &MTable) -> Item {
    Item {
        alias: alias.into(),
        cols: t.cols.clone(),
        src: t.name.clone(),
        plain: true,
    }
}

/// 結合の右側に置く要素。たまに派生表・generate_series にする。
fn right_item(rng: &mut Rng, alias: &str, t: &MTable) -> Item {
    match rng.below(100) {
        0..=7 => Item {
            alias: alias.into(),
            cols: vec![("k".into(), MTy::Int4), ("cnt".into(), MTy::Int8)],
            src: format!("(SELECT k, count(*) AS cnt FROM {} GROUP BY k)", t.name),
            plain: true,
        },
        8..=12 => Item {
            alias: alias.into(),
            cols: vec![(alias.into(), MTy::Int4)],
            src: format!("generate_series({}, {})", rng.range(0, 2), rng.range(2, 5)),
            plain: false,
        },
        13..=17 => {
            let w = m4::pred(rng, "z", t, 0);
            Item {
                alias: alias.into(),
                cols: t.cols.clone(),
                src: format!("(SELECT * FROM {} z WHERE {w})", t.name),
                plain: true,
            }
        }
        _ => table_item(alias, t),
    }
}

fn eq_cands(l: &Item, r: &Item) -> Vec<(String, String)> {
    let mut same = Vec::new();
    let mut cross = Vec::new();
    for (ln, lt) in &l.cols {
        for (rn, rt) in &r.cols {
            if ln == rn && lt == rt {
                same.push((ln.clone(), rn.clone()));
            } else if lt.eq_compatible(*rt)
                && (ln == "k" || ln == "a" || ln == "b" || rn == "k" || rn == "a" || rn == "b")
            {
                cross.push((ln.clone(), rn.clone()));
            }
        }
    }
    if !same.is_empty() {
        same
    } else {
        cross
    }
}

pub fn item_table(it: &Item) -> MTable {
    MTable {
        name: it.alias.clone(),
        cols: it.cols.clone(),
        pk: Vec::new(),
        uniq: Vec::new(),
        serial: false,
    }
}

fn residual(rng: &mut Rng, items: &[Item]) -> String {
    let it = rng.pick(items);
    m4::pred(rng, &it.alias, &item_table(it), 0)
}

/// 連鎖を作る。`tabs` の先頭から `n` 個を順に結合する（表は繰り返してよい）。
pub fn chain(rng: &mut Rng, tabs: &[MTable], n: usize, allow_full: bool, allow_nat: bool) -> Chain {
    let first = rng.pick(tabs).clone();
    let mut items = vec![table_item("x0", &first)];
    let mut from = format!("{} AS x0", first.name);
    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    for (c, _) in &first.cols {
        *names.entry(c.clone()).or_default() += 1;
    }
    let mut star_n = first.cols.len();
    let mut has_full = false;
    for i in 1..n {
        let alias = format!("x{i}");
        let t = rng.pick(tabs).clone();
        let right = right_item(rng, &alias, &t);
        let src = format!("{} AS {alias}", right.src);
        let kind = rng.weighted(&[
            26,
            24,
            16,
            if allow_full { 14 } else { 0 },
            6,
            if allow_nat { 6 } else { 0 },
            if allow_nat { 14 } else { 0 },
        ]);
        let word = |rng: &mut Rng| match rng.below(10) {
            0..=3 => "JOIN",
            4 => "INNER JOIN",
            5..=6 => "LEFT JOIN",
            7 => "LEFT OUTER JOIN",
            8 => "RIGHT JOIN",
            _ => "FULL JOIN",
        };
        // USING / NATURAL は左側に同名の列が 1 つだけで、右にも同じ名前がある場合に限る
        let common: Vec<String> = right
            .cols
            .iter()
            .filter(|(c, _)| names.get(c).copied().unwrap_or(0) >= 1 && right.plain)
            .map(|(c, _)| c.clone())
            .collect();
        let unique_common: Vec<String> =
            common.iter().filter(|c| names[*c] == 1).cloned().collect();
        let mut done = false;
        if kind == 4 {
            from.push_str(&format!(" CROSS JOIN {src}"));
            star_n += right.cols.len();
            for (c, _) in &right.cols {
                *names.entry(c.clone()).or_default() += 1;
            }
            done = true;
        } else if kind == 5 && !unique_common.is_empty() && unique_common.len() == common.len() {
            // NATURAL（共通列がすべて左に 1 つずつ）
            let w = match rng.below(5) {
                0 => "NATURAL LEFT JOIN",
                1 => "NATURAL RIGHT JOIN",
                2 if allow_full => "NATURAL FULL JOIN",
                _ => "NATURAL JOIN",
            };
            if w.contains("FULL") {
                has_full = true;
            }
            from.push_str(&format!(" {w} {src}"));
            star_n += right.cols.len() - common.len();
            for (c, _) in &right.cols {
                if !common.contains(c) {
                    *names.entry(c.clone()).or_default() += 1;
                }
            }
            done = true;
        } else if kind == 6 && !unique_common.is_empty() {
            let mut cols = unique_common.clone();
            m4::shuffle(rng, &mut cols);
            cols.truncate(rng.range(1, 2) as usize);
            let mut w = word(rng);
            if w == "FULL JOIN" && !allow_full {
                w = "LEFT JOIN";
            }
            if w == "FULL JOIN" {
                has_full = true;
            }
            from.push_str(&format!(" {w} {src} USING ({})", cols.join(", ")));
            star_n += right.cols.len() - cols.len();
            for (c, _) in &right.cols {
                if !cols.contains(c) {
                    *names.entry(c.clone()).or_default() += 1;
                }
            }
            done = true;
        }
        if !done {
            let mut w = match kind {
                0 => "JOIN",
                1 => "LEFT JOIN",
                2 => "RIGHT JOIN",
                3 => "FULL JOIN",
                _ => word(rng),
            };
            if w == "FULL JOIN" && !allow_full {
                w = "LEFT JOIN";
            }
            if w == "FULL JOIN" {
                has_full = true;
            }
            // ON: 左の既存要素のどれかとの等値（1〜2 本）
            let mut conds = Vec::new();
            let neq = if rng.chance(20) { 2 } else { 1 };
            for _ in 0..neq {
                let l = rng.pick(&items).clone();
                let cands = eq_cands(&l, &right);
                if let Some((lc, rc)) = (!cands.is_empty()).then(|| rng.pick(&cands).clone()) {
                    conds.push(format!("{}.{lc} = {alias}.{rc}", l.alias));
                }
            }
            if conds.is_empty() {
                // 比べられる列がなければ k 同士（派生表・関数には必ず整数の列がある）
                let l = items[0].clone();
                let rc = right
                    .cols
                    .iter()
                    .find(|(_, t)| t.is_int())
                    .map(|(c, _)| c.clone())
                    .unwrap_or_else(|| "k".into());
                conds.push(format!("{}.k = {alias}.{rc}", l.alias));
            }
            // 残りの条件（外部結合の ON の残り）
            if rng.chance(if w == "FULL JOIN" { 12 } else { 28 }) {
                let mut all = items.clone();
                all.push(right.clone());
                conds.push(residual(rng, &all));
            }
            from.push_str(&format!(" {w} {src} ON {}", conds.join(" AND ")));
            star_n += right.cols.len();
            for (c, _) in &right.cols {
                *names.entry(c.clone()).or_default() += 1;
            }
        }
        items.push(right);
    }
    // 派生表・関数は residual の対象外（by_alias は表のものだけ。派生表 SELECT * は表と同じ列なので参照できる）
    Chain {
        from,
        items,
        star_n,
        has_full,
    }
}

/// 連鎖の中の列参照 `alias.col`。
pub fn pick_ref(rng: &mut Rng, items: &[Item]) -> String {
    let it = rng.pick(items);
    let (c, _) = rng.pick(&it.cols);
    format!("{}.{c}", it.alias)
}

/// FROM 句の連鎖から SELECT 文を作る。
pub fn select_stmt(ctx: &mut Ctx, ch: &Chain) -> (String, bool) {
    let rng = &mut ctx.rng;
    let (list, n) = if rng.chance(25) {
        ("*".to_string(), ch.star_n)
    } else {
        let k = rng.range(1, 5) as usize;
        let refs: Vec<String> = (0..k).map(|_| pick_ref(rng, &ch.items)).collect();
        (refs.join(", "), k)
    };
    let distinct = if rng.chance(12) { "DISTINCT " } else { "" };
    let mut sql = format!("SELECT {distinct}{list} FROM {}", ch.from);
    if rng.chance(45) {
        let it = rng.pick(&ch.items).clone();
        let w = m4::pred(rng, &it.alias, &item_table(&it), 1);
        sql.push_str(&format!(" WHERE {w}"));
    }
    let ordered = rng.chance(75);
    if ordered {
        sql.push_str(&format!(" ORDER BY {}", m4::order_positions(rng, n)));
        sql.push_str(&m4::limit(rng));
    }
    sql.push(';');
    (sql, ordered)
}

pub fn scenario(ctx: &mut Ctx) {
    let nt = ctx.rng.range(2, 3) as usize;
    let tabs = m4::setup(ctx, nt, 12);
    for _ in 0..ctx.rng.range(5, 9) {
        let r = ctx.rng.below(100);
        if r < 8 {
            let s = m4::error_query(ctx, &tabs);
            ctx.push_q(s, false);
            continue;
        }
        let n = ctx.rng.range(2, 4) as usize;
        let ch = chain(&mut ctx.rng, &tabs, n, true, true);
        let (sql, ordered) = select_stmt(ctx, &ch);
        let i = ctx.push_q(sql, ordered);
        if ctx.rng.chance(25) {
            m4::plan_variant(ctx, i, ch.has_full);
        }
    }
}
