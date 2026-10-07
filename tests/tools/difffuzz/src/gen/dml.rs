//! dml 領域: DDL（CREATE/DROP TABLE、制約）と INSERT / UPDATE / DELETE、エラー時の文の原子性、読み戻し。

use super::values::literal;
use super::{error_stmt, select_all, Cls, Col, Ctx, Table, Ty};
use crate::rng::Rng;

const TYS: [Ty; 16] = [
    Ty::Int4,
    Ty::Int4,
    Ty::Int8,
    Ty::Int2,
    Ty::Text,
    Ty::Bool,
    Ty::Varchar(8),
    Ty::Float8,
    Ty::Text,
    Ty::Int4,
    Ty::Numeric,
    Ty::NumU,
    Ty::Num51,
    Ty::Float4,
    Ty::Varchar(3),
    Ty::Numeric,
];

fn default_expr(rng: &mut Rng, ty: Ty) -> String {
    let k = rng.below(4);
    match (ty, k) {
        (Ty::Int2 | Ty::Int4 | Ty::Int8, 0) => "(1 + 2)".into(),
        (Ty::Int2 | Ty::Int4 | Ty::Int8, 1) => "-5".into(),
        (Ty::Text | Ty::Varchar(_), 0) => "('a' || 'b')".into(),
        (Ty::Text | Ty::Varchar(_), 1) => "upper('x')".into(),

        (Ty::Bool, 0) => "(1 = 1)".into(),
        (Ty::Numeric | Ty::NumU | Ty::Num51, 0) => "(1.5 + 2)".into(),
        (Ty::Numeric | Ty::NumU | Ty::Num51, 1) => "(10 / 4)".into(),
        (Ty::Float4 | Ty::Float8, 0) => "(1.0 / 3)".into(),
        (Ty::Varchar(_), 2) => "'toolongvalue'".into(),
        (_, 2) => "NULL".into(),
        _ => lit(rng, ty, false),
    }
}

/// dml 専用のテーブル作成（名前付き制約、テーブル制約、式の DEFAULT、IF NOT EXISTS など）。
fn make_table_dml(ctx: &mut Ctx, n: usize) -> usize {
    let name = ctx.fresh_table_name();
    let mut cols = Vec::new();
    let mut defs = Vec::new();
    for i in 0..n {
        let ty = *ctx.rng.pick(&TYS);
        let mut c = Col {
            name: format!("c{i}"),
            ty,
            not_null: false,
            default: None,
            check: None,
        };
        let mut d = format!("c{i} {}", ty.sql());
        if ctx.rng.chance(35) {
            let e = default_expr(&mut ctx.rng, ty);
            d.push_str(&format!(" DEFAULT {e}"));
            c.default = Some(e);
        }
        if ctx.rng.chance(25) {
            c.not_null = true;
            d.push_str(if ctx.rng.chance(20) {
                " CONSTRAINT nn_c NOT NULL"
            } else {
                " NOT NULL"
            });
        }
        if ctx.rng.chance(25) {
            let opts: Vec<String> = match ty {
                Ty::Int2 | Ty::Int4 | Ty::Int8 => {
                    vec![
                        format!("c{i} >= 0"),
                        format!("c{i} % 2 = 0"),
                        format!("c{i} BETWEEN 1 AND 50"),
                    ]
                }
                Ty::Text | Ty::Varchar(_) => {
                    vec![
                        format!("length(c{i}) < 6"),
                        format!("c{i} <> ''"),
                        format!("c{i} LIKE 'a%'"),
                    ]
                }
                Ty::Numeric | Ty::NumU | Ty::Num51 => vec![
                    format!("c{i} > 0"),
                    format!("c{i} <> 1.5"),
                    format!("c{i} < 100"),
                ],
                Ty::Float8 | Ty::Float4 => vec![format!("c{i} < 50")],
                Ty::Bool => vec![format!("c{i}")],
            };
            let k = ctx.rng.pick(&opts).clone();
            if ctx.rng.chance(30) {
                d.push_str(&format!(" CONSTRAINT chk_{i} CHECK ({k})"));
            } else {
                d.push_str(&format!(" CHECK ({k})"));
            }
            c.check = Some(k);
        }
        defs.push(d);
        cols.push(c);
    }
    if n >= 2 && ctx.rng.chance(20) {
        let k = ctx
            .rng
            .pick(&["c0 IS NOT NULL OR c1 IS NOT NULL", "c0 = c0 OR c1 IS NULL"])
            .to_string();
        defs.push(if ctx.rng.chance(50) {
            format!("CHECK ({k})")
        } else {
            format!("CONSTRAINT tbl_chk CHECK ({k})")
        });
    }
    let ine = if ctx.rng.chance(10) {
        "IF NOT EXISTS "
    } else {
        ""
    };
    ctx.push(format!("CREATE TABLE {ine}{name} ({});", defs.join(", ")));
    ctx.tables.push(Table { name, cols });
    ctx.tables.len() - 1
}

/// UPDATE の右辺: リテラル / DEFAULT / NULL / 列を使った式。
fn set_value(ctx: &mut Ctx, c: &Col, scope: &[(String, Cls)]) -> String {
    match ctx.rng.weighted(&[50, 15, 10, 25]) {
        0 => {
            let bad = ctx.rng.chance(8);
            lit(&mut ctx.rng, c.ty, bad)
        }
        1 => "DEFAULT".into(),
        2 => "NULL".into(),
        _ => match c.ty.cls() {
            Some(k) if !scope.is_empty() => format!("({})", dexpr(&mut ctx.rng, scope, k, 1)),
            _ => lit(&mut ctx.rng, c.ty, false),
        },
    }
}

/// INSERT の値: リテラル中心で DEFAULT / NULL / 簡単な式を混ぜる。
fn insert_value(ctx: &mut Ctx, c: &Col) -> String {
    match ctx.rng.weighted(&[70, 12, 10, 8]) {
        0 => {
            let bad = ctx.rng.chance(6);
            lit(&mut ctx.rng, c.ty, bad)
        }
        1 => "DEFAULT".into(),
        2 => "NULL".into(),
        _ => match c.ty {
            Ty::Int2 | Ty::Int4 | Ty::Int8 => {
                let (a, b) = (ctx.rng.range(0, 30), ctx.rng.range(0, 3000));
                format!("({a} * {b})")
            }
            Ty::Text | Ty::Varchar(_) => "('a' || 'bc')".into(),

            _ => lit(&mut ctx.rng, c.ty, false),
        },
    }
}

fn opt_where(ctx: &mut Ctx, scope: &[(String, Cls)], none_pct: u64) -> String {
    if ctx.rng.chance(none_pct) {
        String::new()
    } else {
        format!(" WHERE {}", dexpr(&mut ctx.rng, scope, Cls::Bool, 2))
    }
}

/// 1 つ DML 文を積む（txn 領域からも使う）。
pub fn dml_stmt(ctx: &mut Ctx, ti: usize) {
    let t = ctx.tables[ti].clone();
    let scope = t.scope();
    match ctx
        .rng
        .weighted(&[4, 2, 3, 2, 2, 2, 3, 3, 2, 2, 2, 1, 4, 3, 3, 1, 1])
    {
        0 => {
            let n = ctx.rng.range(1, 3) as usize;
            let s = insert_rows(ctx, ti, n, 10);
            ctx.push(s);
        }
        1 => {
            // 列リスト指定 + DEFAULT
            let c = ctx.rng.pick(&t.cols).clone();
            let bad = ctx.rng.chance(10);
            let v = if ctx.rng.chance(30) {
                "DEFAULT".to_string()
            } else {
                lit(&mut ctx.rng, c.ty, bad)
            };
            ctx.push(format!("INSERT INTO {} ({}) VALUES ({v});", t.name, c.name));
        }
        2 => {
            let c = ctx.rng.pick(&t.cols).clone();
            let bad = ctx.rng.chance(10);
            let v = lit(&mut ctx.rng, c.ty, bad);
            let w = dexpr(&mut ctx.rng, &scope, Cls::Bool, 2);
            ctx.push(format!("UPDATE {} SET {} = {v} WHERE {w};", t.name, c.name));
        }
        3 => {
            let w = dexpr(&mut ctx.rng, &scope, Cls::Bool, 2);
            ctx.push(format!("DELETE FROM {} WHERE {w};", t.name));
        }
        4 => {
            let w = dexpr(&mut ctx.rng, &scope, Cls::Bool, 1);
            let _c = ctx.rng.pick(&t.cols).clone();
            ctx.push(format!("DELETE FROM {} WHERE {w};", t.name));
        }
        5 => {
            let n = ctx.rng.range(1, 2) as usize;
            let ins = insert_rows(ctx, ti, n, 5);
            ctx.push(ins.trim_end_matches(';').to_string() + ";");
        }
        6 => {
            // 複数列 UPDATE（式つき）。途中で失敗したら文全体が巻き戻る
            let k = ctx.rng.range(1, 2) as usize;
            let mut sets: Vec<(String, String)> = Vec::new();
            for _ in 0..k {
                let c = ctx.rng.pick(&t.cols).clone();
                if sets.iter().any(|(n, _)| *n == c.name) {
                    continue;
                }
                let v = set_value(ctx, &c, &scope);
                sets.push((c.name.clone(), v));
            }
            let sets: Vec<String> = sets.iter().map(|(n, v)| format!("{n} = {v}")).collect();
            let w = opt_where(ctx, &scope, 20);
            let _ = ctx.rng.chance(25);
            let ret = "";
            ctx.push(format!(
                "UPDATE {} SET {}{w}{ret};",
                t.name,
                sets.join(", ")
            ));
        }
        7 => {
            // 複数行 INSERT（式・DEFAULT・NULL 混在）。一部の行だけ不正で文全体が失敗する場合もある
            let n = ctx.rng.range(2, 5) as usize;
            let rows: Vec<String> = (0..n)
                .map(|_| {
                    let vs: Vec<String> = t.cols.iter().map(|c| insert_value(ctx, c)).collect();
                    format!("({})", vs.join(", "))
                })
                .collect();
            let _ = ctx.rng.chance(20);
            let ret = "";
            ctx.push(format!(
                "INSERT INTO {} VALUES {}{ret};",
                t.name,
                rows.join(", ")
            ));
        }
        8 => {
            // INSERT ... SELECT（自テーブル・別テーブル）
            let other = ctx.rng.below(ctx.tables.len() as u64) as usize;
            let o = ctx.tables[other].clone();
            let n = t.cols.len().min(o.cols.len()).min(2);
            let cols: Vec<String> = t.cols.iter().take(n).map(|c| c.name.clone()).collect();
            let srcs: Vec<String> = o.cols.iter().take(n).map(|c| c.name.clone()).collect();
            let w = opt_where(ctx, &o.scope(), 50);
            ctx.push(format!(
                "INSERT INTO {} ({}) SELECT {} FROM {}{w} ORDER BY {};",
                t.name,
                cols.join(", "),
                srcs.join(", "),
                o.name,
                (1..=n)
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        9 => {
            // 列リスト付き INSERT（順序入替・一部列のみ → DEFAULT/NULL 補完）
            let mut cs = t.cols.clone();
            if ctx.rng.chance(50) {
                cs.reverse();
            }
            let k = ctx.rng.range(1, cs.len() as i64) as usize;
            cs.truncate(k);
            let vs: Vec<String> = cs.iter().map(|c| insert_value(ctx, c)).collect();
            let names: Vec<String> = cs.iter().map(|c| c.name.clone()).collect();
            ctx.push(format!(
                "INSERT INTO {} ({}) VALUES ({});",
                t.name,
                names.join(", "),
                vs.join(", ")
            ));
        }
        10 => {
            let w = opt_where(ctx, &scope, 30);
            ctx.push(format!("DELETE FROM {}{w};", t.name));
        }
        11 => {
            // 特殊形: DEFAULT VALUES / 列数不一致 / 重複列 / 存在しない列など
            let c = ctx.rng.pick(&t.cols).clone();
            let s = match ctx.rng.below(7) {
                0 => format!("INSERT INTO {} DEFAULT VALUES;", t.name),
                1 => format!(
                    "INSERT INTO {} ({}, {}) VALUES (1, 2);",
                    t.name, c.name, c.name
                ),
                2 => format!("INSERT INTO {} VALUES (1);", t.name),
                3 => format!("UPDATE {} SET {} = 1, {} = 2;", t.name, c.name, c.name),
                4 => format!("INSERT INTO {} ({}) VALUES (1, 2);", t.name, c.name),
                5 => format!("INSERT INTO {} DEFAULT VALUES;", t.name),
                _ => format!("INSERT INTO {} ({}) SELECT 1, 2;", t.name, c.name),
            };
            ctx.push(s);
        }
        12 => {
            // RETURNING 付き INSERT / UPDATE / DELETE
            let c = ctx.rng.pick(&t.cols).clone();
            let ret = if ctx.rng.chance(50) {
                "*".to_string()
            } else {
                c.name.clone()
            };
            let ob = format!(" ORDER BY {}", 1);
            let _ = ob;
            match ctx.rng.below(3) {
                0 => {
                    let vs: Vec<String> = t.cols.iter().map(|c| insert_value(ctx, c)).collect();
                    ctx.push(format!(
                        "INSERT INTO {} VALUES ({}) RETURNING {ret};",
                        t.name,
                        vs.join(", ")
                    ));
                }
                1 => {
                    let v = set_value(ctx, &c, &scope);
                    let w = opt_where(ctx, &scope, 30);
                    ctx.push(format!("WITH u AS (UPDATE {} SET {} = {v}{w} RETURNING {ret}) SELECT * FROM u ORDER BY 1;", t.name, c.name));
                }
                _ => {
                    let w = opt_where(ctx, &scope, 30);
                    ctx.push(format!(
                        "WITH d AS (DELETE FROM {}{w} RETURNING {ret}) SELECT * FROM d ORDER BY 1;",
                        t.name
                    ));
                }
            }
        }
        13 => {
            // UPDATE ... FROM / DELETE ... USING（別テーブルとの結合）
            let other = ctx.rng.below(ctx.tables.len() as u64) as usize;
            let o = ctx.tables[other].clone();
            let c = ctx.rng.pick(&t.cols).clone();
            let oc = ctx.rng.pick(&o.cols).clone();
            if ctx.rng.chance(50) {
                ctx.push(format!(
                    "UPDATE {} AS a SET {} = {} FROM {} AS b WHERE a.{} IS NOT DISTINCT FROM b.{};",
                    t.name,
                    c.name,
                    if oc.ty == c.ty {
                        format!("b.{}", oc.name)
                    } else {
                        "DEFAULT".into()
                    },
                    o.name,
                    t.cols[0].name,
                    o.cols[0].name
                ));
            } else {
                ctx.push(format!(
                    "DELETE FROM {} AS a USING {} AS b WHERE a.{} IS NOT DISTINCT FROM b.{};",
                    t.name, o.name, t.cols[0].name, o.cols[0].name
                ));
            }
        }
        14 => {
            // 型変換つき INSERT（式 → 列型へ代入キャスト）
            let c = ctx.rng.pick(&t.cols).clone();
            let e = ctx
                .rng
                .pick(&[
                    "1.5",
                    "2.5",
                    "(-0.5)",
                    "(10 / 3)",
                    "1e3",
                    "2147483648",
                    "32768",
                    "'12'",
                    "'x'",
                    "(1 = 1)",
                    "('ab' || 'cd')",
                    "1.0::float8",
                    "0.5::numeric",
                    "70000",
                ])
                .to_string();
            ctx.push(format!("INSERT INTO {} ({}) VALUES ({e});", t.name, c.name));
        }
        15 => {
            // TRUNCATE / 自己参照 INSERT SELECT
            if ctx.rng.chance(40) {
                ctx.push(format!("TRUNCATE {};", t.name));
            } else {
                ctx.push(format!("INSERT INTO {} SELECT * FROM {};", t.name, t.name));
            }
        }
        16 => {
            // 複数行 UPDATE で一意でない順序に依存しない自己更新（全行 SET 列 = 列）
            let c = ctx.rng.pick(&t.cols).clone();
            ctx.push(format!("UPDATE {} SET {} = {};", t.name, c.name, c.name));
        }
        _ => {}
    }
}

/// DDL 文（DROP / 再作成 / 存在しない対象 / 不正な定義）。
fn ddl_stmt(ctx: &mut Ctx, ti: usize) {
    let t = ctx.tables[ti].clone();
    match ctx.rng.below(40) {
        24 => {
            ctx.push(format!("CREATE TABLE {}_g (a integer, b text);", t.name));
            ctx.push(format!("INSERT INTO {}_g VALUES (1, 'x'), (2, NULL);", t.name));
        }
        25 => ctx.push(format!("CREATE TABLE {}_h (a integer, b text DEFAULT 'q');", t.name)),
        26 => ctx.push(format!("CREATE TABLE {}_i (\"Mixed Case\" integer, \"select\" text);", t.name)),
        27 => ctx.push(format!("CREATE TABLE {}_j (a integer, b integer, CHECK (a < b), CHECK (a > 0));", t.name)),
        28 => ctx.push(format!("CREATE TABLE {}_k (a smallint DEFAULT 40000);", t.name)),
        29 => ctx.push(format!("CREATE TABLE {}_l (a text DEFAULT 1);", t.name)),
        30 => ctx.push(format!("CREATE TABLE {}_m (a bool DEFAULT 'yes', b float4 DEFAULT 1e50);", t.name)),
        31 => ctx.push(format!("CREATE TABLE {}_n (a integer CHECK (a > 0) CHECK (a < 10), b integer NOT NULL DEFAULT NULL);", t.name)),
        32 => ctx.push(format!("CREATE TABLE {}_o AS SELECT 1 AS x, 'a'::text AS y;", t.name)),
        33 => ctx.push(format!("DROP TABLE {}_none, {}_none2;", t.name, t.name)),
        34 => ctx.push(format!("CREATE TABLE {}_p (a integer, b numeric(4,2) DEFAULT 99.999);", t.name)),
        35 => ctx.push(format!("CREATE TABLE {}_q (a varchar(3) CHECK (length(a) > 5));", t.name)),
        36 => ctx.push(format!("CREATE TABLE {}_r (a int4, b int8, c int2, d character varying(4), e character(3));", t.name)),
        37 => ctx.push(format!("INSERT INTO {}_none VALUES (1);", t.name)),
        38 => ctx.push(format!("UPDATE {}_none SET a = 1;", t.name)),
        39 => ctx.push(format!("DELETE FROM {}_none;", t.name)),
        0 => ctx.push(format!("DROP TABLE IF EXISTS {}_none;", t.name)),
        1 => ctx.push(format!("DROP TABLE {}_none;", t.name)),
        2 => ctx.push(format!("CREATE TABLE {} (x integer);", t.name)),
        3 => ctx.push(format!("CREATE TABLE IF NOT EXISTS {} (x integer);", t.name)),
        4 => ctx.push(format!("CREATE TABLE {}_b (a integer, a text);", t.name)),
        5 => ctx.push(format!("CREATE TABLE {}_b (a nosuchtype);", t.name)),
        6 => ctx.push(format!("CREATE TABLE {}_d (a integer DEFAULT 'x');", t.name)),
        7 => ctx.push(format!("CREATE TABLE {}_e (a integer CHECK (a + 'x' > 0));", t.name)),
        8 => ctx.push(format!("CREATE TABLE {}_f (a integer, CHECK (nosuch > 0));", t.name)),
        10 => ctx.push(format!("CREATE TABLE {}_g AS SELECT * FROM {};", t.name, t.name)),
        11 => ctx.push(format!("CREATE TABLE {}_h (LIKE {});", t.name, t.name)),
        12 => ctx.push(format!("DROP TABLE IF EXISTS {}_none, {}_none2;", t.name, t.name)),
        13 => ctx.push(format!("CREATE TABLE {}_i (a numeric(3,5));", t.name)),
        14 => ctx.push(format!("CREATE TABLE {}_j (a varchar(0));", t.name)),
        15 => ctx.push(format!("CREATE TABLE {}_k (a integer NOT NULL NULL);", t.name)),
        16 => ctx.push(format!("CREATE TABLE {}_l (a integer DEFAULT 1 DEFAULT 2);", t.name)),
        17 => ctx.push(format!("CREATE TABLE {}_m (a integer CHECK (a > 0), b integer, CONSTRAINT x CHECK (b > 0), CONSTRAINT x CHECK (b < 9));", t.name)),
        18 => ctx.push(format!("CREATE TABLE {}_n (a integer, CHECK (a IS NOT NULL AND a > 0));", t.name)),
        19 => ctx.push(format!("CREATE TABLE {}_o (a integer DEFAULT (SELECT 1));", t.name)),
        20 => ctx.push(format!("CREATE TABLE {}_p (a real, b numeric DEFAULT 1.234567, c varchar(2) DEFAULT 'ab', d \"char\", e name);", t.name)),
        21 => ctx.push(format!("CREATE TABLE {}_q (a integer, a2 integer GENERATED ALWAYS AS (a + 1) STORED);", t.name)),
        22 => ctx.push(format!("CREATE TEMP TABLE {}_r (a integer);", t.name)),
        23 => ctx.push(format!("CREATE TABLE {}_s ();", t.name)),
        _ => {
            let n = format!("{}_tmp", t.name);
            ctx.push(format!("CREATE TABLE {n} (a integer NOT NULL, b text DEFAULT 'z');"));
            ctx.push(format!("INSERT INTO {n} (a) VALUES (1), (2);"));
            ctx.push(format!("SELECT * FROM {n} ORDER BY 1;"));
            ctx.push(format!("DROP TABLE {n};"));
            ctx.push(format!("SELECT * FROM {n};"));
        }
    }
}

pub fn scenario(ctx: &mut Ctx) {
    let ncols = ctx.rng.range(2, 5) as usize;
    make_table_dml(ctx, ncols);
    if ctx.rng.chance(30) {
        let n2 = ctx.rng.range(2, 3) as usize;
        make_table_dml(ctx, n2);
    }
    for _ in 0..ctx.rng.range(6, 16) {
        let ti = ctx.rng.below(ctx.tables.len() as u64) as usize;
        match ctx.rng.weighted(&[12, 3, 5, 2]) {
            0 => dml_stmt(ctx, ti),
            1 => {
                let t = ctx.tables[ti].clone();
                ctx.push(select_all(&t));
            }
            2 => {
                let s = error_stmt(ctx);
                ctx.push(s);
            }
            _ => ddl_stmt(ctx, ti),
        }
    }
    for i in 0..ctx.tables.len() {
        let t = ctx.tables[i].clone();
        ctx.push(select_all(&t));
    }
    for i in 0..ctx.tables.len() {
        let n = ctx.tables[i].name.clone();
        let all: Vec<String> = "ghijklmnopqrs"
            .replace(' ', "")
            .chars()
            .map(|c| format!("{n}_{c}"))
            .collect();
        ctx.push(format!("DROP TABLE IF EXISTS {};", all.join(", ")));
    }
}

/// 未対応（numeric・ARRAY）を避けたリテラル。
fn lit(rng: &mut Rng, ty: Ty, bad: bool) -> String {
    if bad && ty == Ty::Text {
        return literal(rng, ty, false);
    }
    if bad && (ty != Ty::Varchar(8) && ty != Ty::Text) {
        return bad_lit(rng, ty);
    }
    literal(rng, ty, bad)
}

fn insert_rows(ctx: &mut Ctx, ti: usize, nrows: usize, invalid_pct: u64) -> String {
    let t = ctx.tables[ti].clone();
    let rows: Vec<String> = (0..nrows)
        .map(|_| {
            let vals: Vec<String> = t
                .cols
                .iter()
                .map(|c| {
                    let bad = ctx.rng.chance(invalid_pct);
                    lit(&mut ctx.rng, c.ty, bad)
                })
                .collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    format!("INSERT INTO {} VALUES {};", t.name, rows.join(", "))
}

fn col_of(rng: &mut Rng, scope: &[(String, Cls)], cls: Cls) -> Option<String> {
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

/// M1〜M3 の範囲で対応している関数・演算子だけを使う式（numeric / GREATEST / IS DISTINCT FROM などは使わない）。
fn dexpr(rng: &mut Rng, scope: &[(String, Cls)], cls: Cls, d: u32) -> String {
    let leaf = d == 0 || rng.chance(25);
    let cls = if cls == Cls::Num { Cls::Int } else { cls };
    match cls {
        Cls::Int => {
            if leaf {
                if rng.chance(50) {
                    if let Some(c) = col_of(rng, scope, Cls::Int) {
                        return c;
                    }
                }
                let v = super::values::int_lit(rng);
                return if v < 0 {
                    format!("({v})")
                } else if rng.chance(5) {
                    "NULL".into()
                } else {
                    v.to_string()
                };
            }
            let e = |rng: &mut Rng| dexpr(rng, scope, Cls::Int, d - 1);
            let nz = rng.range(1, 9);
            match rng.below(11) {
                0 => format!("({} + {})", e(rng), e(rng)),
                1 => format!("({} - {})", e(rng), e(rng)),
                2 => format!("({} * {})", e(rng), e(rng)),
                3 => format!("({} / {nz})", e(rng)),
                4 => format!("({} % {nz})", e(rng)),
                5 => format!("abs({})", e(rng)),
                6 => format!("length({})", dexpr(rng, scope, Cls::Text, d - 1)),
                7 => format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    dexpr(rng, scope, Cls::Bool, d - 1),
                    e(rng),
                    e(rng)
                ),
                8 => format!("COALESCE({}, {})", e(rng), e(rng)),
                9 => format!("NULLIF({}, {})", e(rng), e(rng)),
                _ => format!(
                    "({})::{}",
                    e(rng),
                    rng.pick(&["integer", "bigint", "smallint"])
                ),
            }
        }
        Cls::Text => {
            if leaf {
                if rng.chance(50) {
                    if let Some(c) = col_of(rng, scope, Cls::Text) {
                        return c;
                    }
                }
                return if rng.chance(5) {
                    "NULL".into()
                } else {
                    super::values::text_lit(rng)
                };
            }
            let e = |rng: &mut Rng| dexpr(rng, scope, Cls::Text, d - 1);
            match rng.below(7) {
                0 | 1 => format!("({} || {})", e(rng), e(rng)),
                2 => format!("upper({})", e(rng)),
                3 => format!("lower({})", e(rng)),
                4 => format!("({})::text", dexpr(rng, scope, Cls::Int, d - 1)),
                5 => format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    dexpr(rng, scope, Cls::Bool, d - 1),
                    e(rng),
                    e(rng)
                ),
                _ => format!("COALESCE({}, {})", e(rng), e(rng)),
            }
        }
        _ => {
            if leaf {
                if rng.chance(30) {
                    if let Some(c) = col_of(rng, scope, Cls::Bool) {
                        return c;
                    }
                }
                return rng.pick(&["TRUE", "FALSE", "NULL"]).to_string();
            }
            let b = |rng: &mut Rng| dexpr(rng, scope, Cls::Bool, d - 1);
            let i = |rng: &mut Rng| dexpr(rng, scope, Cls::Int, d - 1);
            let t = |rng: &mut Rng| dexpr(rng, scope, Cls::Text, d - 1);
            let op = |rng: &mut Rng| *rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
            match rng.weighted(&[8, 4, 3, 2, 2, 2, 2, 2, 2, 2, 2]) {
                0 => format!("({} {} {})", i(rng), op(rng), i(rng)),
                1 => format!("({} {} {})", t(rng), op(rng), t(rng)),
                2 => format!("({} AND {})", b(rng), b(rng)),
                3 => format!("({} OR {})", b(rng), b(rng)),
                4 => format!("(NOT {})", b(rng)),
                5 => format!("({} IS NULL)", i(rng)),
                6 => format!("({} IS NOT NULL)", t(rng)),
                7 => format!("({} IN ({}, {}, {}))", i(rng), i(rng), i(rng), i(rng)),
                8 => format!("({} BETWEEN {} AND {})", i(rng), i(rng), i(rng)),
                9 => format!(
                    "({} LIKE '{}')",
                    t(rng),
                    rng.pick(&["a%", "%b%", "_bc", "%", "H%o", "100"])
                ),
                _ => format!("({} IS TRUE)", b(rng)),
            }
        }
    }
}

/// 不正値（bad）のうち、未対応の numeric に落ちるものを除いたリテラル。
fn bad_lit(rng: &mut Rng, ty: Ty) -> String {
    match ty {
        Ty::Int2 | Ty::Int4 | Ty::Int8 => rng
            .pick(&[
                "'abc'",
                "'1.5'",
                "2147483648",
                "true",
                "''",
                "'99999999999999999999'",
                "' 7 '",
                "'0x10'",
            ])
            .to_string(),
        Ty::Bool => rng.pick(&["'maybe'", "2", "'abc'", "'tru'"]).to_string(),
        Ty::Float8 | Ty::Float4 => rng
            .pick(&["'abc'", "'1.2.3'", "true", "'1e39'", "'1e400'", "''"])
            .to_string(),
        _ => literal(rng, ty, true),
    }
}
