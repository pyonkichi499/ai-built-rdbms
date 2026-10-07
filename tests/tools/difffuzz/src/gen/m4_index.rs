//! index 領域: PRIMARY KEY / UNIQUE / CREATE INDEX（単一・複合・DESC・NULLS FIRST）を持つ表に対する
//! 等値・範囲・IN・IS NULL・ORDER BY（LIMIT）・min/max と、一意制約の違反（23505）。
//!
//! 2 つの形がある。
//! - 差分: 表に DML（重複キーの INSERT・UPDATE・DELETE）を流し、各文の後で表の全内容を比べる。
//! - 索引の有無: 同じデータの「索引あり表」と「索引なし表」に同じ問い合わせを流し、同じサーバ上で結果が同じことを確かめる（検査 (e)）。

use super::m4::{self, Key, MTable, MTy};
use super::Ctx;
use crate::rng::Rng;

/// 索引の 1 列（列名, 型, 並び指定）。
pub type IdxCol = (String, MTy, String);

pub fn index_cols(rng: &mut Rng, t: &MTable, max: i64) -> Vec<IdxCol> {
    let n = rng.range(1, max.min(t.cols.len() as i64)) as usize;
    let mut cols = t.cols.clone();
    // 先頭は k か a が多い（結合・等値で当たりやすい）
    if rng.chance(60) {
        let p = cols
            .iter()
            .position(|(c, _)| c == if rng.chance(50) { "k" } else { "a" })
            .unwrap_or(0);
        cols.swap(0, p);
        let rest = &mut cols[1..];
        m4::shuffle(rng, rest);
    } else {
        m4::shuffle(rng, &mut cols);
    }
    cols.truncate(n);
    cols.into_iter()
        .map(|(c, ty)| {
            let o = match rng.below(8) {
                0 => " DESC",
                1 => " ASC",
                2 => " DESC NULLS LAST",
                3 => " NULLS FIRST",
                4 => " ASC NULLS LAST",
                _ => "",
            };
            (c, ty, o.to_string())
        })
        .collect()
}

pub fn create_index_sql(
    rng: &mut Rng,
    t: &MTable,
    name: Option<&str>,
    unique: bool,
    cols: &[IdxCol],
) -> String {
    let u = if unique { "UNIQUE " } else { "" };
    let ine = if name.is_some() && rng.chance(10) {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let nm = name.map(|n| format!("{n} ")).unwrap_or_default();
    let using = if rng.chance(15) { "USING btree " } else { "" };
    let list: Vec<String> = cols.iter().map(|(c, _, o)| format!("{c}{o}")).collect();
    format!(
        "CREATE {u}INDEX {ine}{nm}ON {} {using}({});",
        t.name,
        list.join(", ")
    )
}

/// 索引の先頭列を使う問い合わせ。`t` の名前は呼び出し側が置き換えられるよう、そのまま入れる。
pub fn index_query(ctx: &mut Ctx, t: &MTable, key: &[IdxCol]) -> (String, bool) {
    let rng = &mut ctx.rng;
    let (c, ty, _) = key[0].clone();
    let l1 = m4::lit(rng, ty, 0);
    let l2 = m4::lit(rng, ty, 0);
    let (lo, hi) = (l1.clone(), l2.clone());
    let cond = if ty == MTy::Bool {
        match rng.below(3) {
            0 => format!("{c} = {l1}"),
            1 => format!("{c} IS NULL"),
            _ => format!("{c} IS NOT NULL"),
        }
    } else {
        match rng.below(10) {
            0 | 1 => format!("{c} = {l1}"),
            2 => format!("{c} > {l1}"),
            3 => format!("{c} <= {l1}"),
            4 => format!("{c} >= {lo} AND {c} < {hi}"),
            5 => format!("{c} IN ({l1}, {l2}, {})", m4::lit(rng, ty, 0)),
            6 => format!("{c} IS NULL"),
            7 => format!("{c} BETWEEN {lo} AND {hi}"),
            8 if key.len() > 1 => {
                let (c2, ty2, _) = key[1].clone();
                format!(
                    "{c} = {l1} AND {c2} {} {}",
                    rng.pick(&["=", ">", "<=", "<>"]),
                    m4::lit(rng, ty2, 0)
                )
            }
            _ => format!("{c} <> {l1}"),
        }
    };
    let cond = if rng.chance(20) {
        format!("{cond} AND {}", m4::pred(rng, &t.name, t, 0))
    } else {
        cond
    };
    let all = t.order_all();
    match rng.below(10) {
        0..=2 => (
            format!("SELECT * FROM {} WHERE {cond} ORDER BY {all};", t.name),
            true,
        ),
        3 => (
            format!(
                "SELECT {c} FROM {} WHERE {cond} ORDER BY 1{};",
                t.name,
                m4::dir(rng)
            ),
            true,
        ),
        4 | 5 => {
            // 索引の並びで ORDER BY（先頭に索引列、続けて全列で順序を決める）
            let d = m4::dir(rng);
            (
                format!(
                    "SELECT * FROM {} WHERE {cond} ORDER BY {c}{d}, {all}{};",
                    t.name,
                    m4::limit(rng)
                ),
                true,
            )
        }
        6 => {
            let d = m4::dir(rng);
            (
                format!(
                    "SELECT * FROM {} ORDER BY {c}{d}, {all} LIMIT {};",
                    t.name,
                    rng.range(1, 5)
                ),
                true,
            )
        }
        7 if ty != MTy::Bool => (
            format!("SELECT min({c}), max({c}), count(*) FROM {};", t.name),
            true,
        ),
        8 => (
            format!("SELECT count(*) FROM {} WHERE {cond};", t.name),
            true,
        ),
        _ => (
            format!(
                "SELECT {c}, count(*) FROM {} WHERE {cond} GROUP BY {c} ORDER BY 1, 2;",
                t.name
            ),
            true,
        ),
    }
}

fn key_for(rng: &mut Rng) -> Key {
    match rng.weighted(&[25, 30, 10, 15, 20]) {
        0 => Key::Plain,
        1 => Key::Pk,
        2 => Key::PkKA,
        3 => Key::UniqA,
        _ => Key::PkUniq,
    }
}

/// 索引つきの表と索引なしの表（同じデータ）に同じ問い合わせを流し、結果が同じことを確かめる。
fn pair_scenario(ctx: &mut Ctx) {
    let key = key_for(&mut ctx.rng);
    let t = m4::make_table(ctx, key, 45);
    // 索引なしの表（同じ列、制約なし）
    let pname = ctx.fresh_table_name();
    let p = MTable {
        name: pname,
        cols: t.cols.clone(),
        pk: Vec::new(),
        uniq: Vec::new(),
        serial: false,
    };
    let psql = m4::create_sql(ctx, &p);
    ctx.push(psql);
    let mut idx: Vec<Vec<IdxCol>> = Vec::new();
    for _ in 0..ctx.rng.range(1, 3) {
        let cols = index_cols(&mut ctx.rng, &t, 3);
        idx.push(cols);
    }
    let before = ctx.rng.chance(50);
    let mk = |ctx: &mut Ctx, idx: &[Vec<IdxCol>]| {
        for (n, cols) in idx.iter().enumerate() {
            let name = format!("{}ix{n}", ctx.prefix);
            let s = create_index_sql(&mut ctx.rng, &t, Some(&name), false, cols);
            ctx.push(s);
        }
    };
    if before {
        mk(ctx, &idx);
    }
    let rounds = if key == Key::Plain {
        ctx.rng.range(1, 2)
    } else {
        1
    };
    for _ in 0..rounds {
        let n = ctx.rng.range(3, 8) as usize;
        let s = m4::insert_sql(ctx, &t, n, true, 15);
        ctx.push(s.replace(&t.name, &p.name));
        ctx.push(s);
    }
    if !before {
        mk(ctx, &idx);
    }
    for _ in 0..ctx.rng.range(5, 9) {
        let cols = ctx.rng.pick(&idx).clone();
        let (sql, ordered) = index_query(ctx, &t, &cols);
        let i = ctx.push_q(sql.clone(), ordered);
        let j = ctx.push_q(sql.replace(&t.name, &p.name), ordered);
        ctx.pairs.push((i, j));
        if ctx.rng.chance(30) {
            // 索引を使わない / 全走査を避ける設定でも同じ結果
            m4::plan_variant(ctx, i, false);
        }
    }
}

fn diff_scenario(ctx: &mut Ctx) {
    let key = key_for(&mut ctx.rng);
    let t = m4::make_table(ctx, key, 50);
    let mut unique_cols: Vec<String> = t.pk.iter().chain(t.uniq.iter()).cloned().collect();
    let mut idx: Vec<Vec<IdxCol>> = Vec::new();
    let nidx = ctx.rng.range(0, 3);
    let before = ctx.rng.chance(50);
    let mut specs: Vec<(Vec<IdxCol>, bool)> = Vec::new();
    for _ in 0..nidx {
        let cols = index_cols(&mut ctx.rng, &t, 3);
        let unique = ctx.rng.chance(20);
        specs.push((cols, unique));
    }
    let emit = |ctx: &mut Ctx,
                specs: &[(Vec<IdxCol>, bool)],
                unique_cols: &mut Vec<String>,
                idx: &mut Vec<Vec<IdxCol>>| {
        for (n, (cols, unique)) in specs.iter().enumerate() {
            let named = ctx.rng.chance(70);
            let name = format!("{}ix{n}", ctx.prefix);
            let s = create_index_sql(
                &mut ctx.rng,
                &t,
                if named { Some(&name) } else { None },
                *unique,
                cols,
            );
            ctx.push(s);
            if *unique {
                unique_cols.extend(cols.iter().map(|(c, _, _)| c.clone()));
            }
            idx.push(cols.clone());
        }
    };
    if before {
        emit(ctx, &specs, &mut unique_cols, &mut idx);
    }
    let n = ctx.rng.range(3, 8) as usize;
    let clean = ctx.rng.chance(60);
    let s = m4::insert_sql(ctx, &t, n, clean, 15);
    ctx.push(s);
    if !before {
        emit(ctx, &specs, &mut unique_cols, &mut idx);
    }
    let allsel = format!("SELECT * FROM {} ORDER BY {};", t.name, t.order_all());
    ctx.push_q(allsel.clone(), true);
    let keycols: Vec<IdxCol> = if idx.is_empty() {
        vec![("k".into(), MTy::Int4, String::new())]
    } else {
        idx[0].clone()
    };
    for _ in 0..ctx.rng.range(6, 11) {
        let r = ctx.rng.below(100);
        if r < 40 {
            let (sql, ordered) = index_query(ctx, &t, &keycols);
            let i = ctx.push_q(sql, ordered);
            if ctx.rng.chance(20) {
                m4::plan_variant(ctx, i, false);
            }
        } else if r < 58 {
            // 重複を含みうる INSERT（23505 になりうる）
            let n = ctx.rng.range(1, 3) as usize;
            let s = m4::insert_sql(ctx, &t, n, false, 15);
            ctx.push(s);
            ctx.push_q(allsel.clone(), true);
        } else if r < 72 {
            // 索引のない列だけを動かす UPDATE（複数行）か、キー列を 1 行ずつ動かす UPDATE
            let free: Vec<&(String, MTy)> = t
                .cols
                .iter()
                .filter(|(c, _)| !unique_cols.contains(c))
                .collect();
            let s = if !free.is_empty() && ctx.rng.chance(60) {
                let (c, ty) = (*ctx.rng.pick(&free)).clone();
                let v = m4::lit(&mut ctx.rng, ty, 10);
                format!(
                    "UPDATE {} AS x SET {c} = {v} WHERE {};",
                    t.name,
                    m4::pred(&mut ctx.rng, "x", &t, 1)
                )
            } else {
                let (c, ty) = ctx.rng.pick(&t.cols).clone();
                let v = m4::lit(&mut ctx.rng, ty, 0);
                let w = m4::lit(&mut ctx.rng, ty, 0);
                format!("UPDATE {} SET {c} = {v} WHERE {c} = {w};", t.name)
            };
            ctx.push(s);
            ctx.push_q(allsel.clone(), true);
        } else if r < 82 {
            let s = format!(
                "DELETE FROM {} AS x WHERE {};",
                t.name,
                m4::pred(&mut ctx.rng, "x", &t, 1)
            );
            ctx.push(s);
            ctx.push_q(allsel.clone(), true);
        } else if r < 90 {
            // 削除したキーを入れ直す
            let n = ctx.rng.range(1, 3) as usize;
            let s = m4::insert_sql(ctx, &t, n, false, 15);
            ctx.push(s);
            let (sql, ordered) = index_query(ctx, &t, &keycols);
            ctx.push_q(sql, ordered);
        } else {
            let s = m4::error_query(ctx, std::slice::from_ref(&t));
            ctx.push_q(s, false);
        }
    }
}

pub fn scenario(ctx: &mut Ctx) {
    if ctx.rng.chance(40) {
        pair_scenario(ctx);
    } else {
        diff_scenario(ctx);
    }
}
