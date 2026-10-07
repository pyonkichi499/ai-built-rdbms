//! M4 領域（join / agg / subquery / setop / index / ddl）の共通部品: 表のモデル、リテラル、述語、INSERT、プラン変種。
//!
//! 列名は型と 1 対 1（k, a: integer、b: bigint、s: text、n: numeric(6,2)、c: char(3)、d: date、ts: timestamp、f: boolean）。
//! 表は k と a を必ず持ち、ほかの列は乱数で選ぶ。NATURAL / USING が型違いの同名列にならず、結合キーが当たりやすい。
//! 浮動小数・interval・タイムゾーンつき時刻・誤差の出る演算は使わない。

use super::Ctx;
use crate::rng::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MTy {
    Int4,
    Int8,
    Text,
    Num,
    Chr,
    Date,
    Ts,
    Bool,
}

impl MTy {
    pub fn sql(self) -> &'static str {
        match self {
            MTy::Int4 => "integer",
            MTy::Int8 => "bigint",
            MTy::Text => "text",
            MTy::Num => "numeric(6,2)",
            MTy::Chr => "char(3)",
            MTy::Date => "date",
            MTy::Ts => "timestamp",
            MTy::Bool => "boolean",
        }
    }

    pub fn is_int(self) -> bool {
        matches!(self, MTy::Int4 | MTy::Int8)
    }

    /// `=` で比べられる組み合わせ（整数どうしは型違いでも可）。
    pub fn eq_compatible(self, o: MTy) -> bool {
        self == o || (self.is_int() && o.is_int())
    }
}

pub const COLS: [(&str, MTy); 9] = [
    ("k", MTy::Int4),
    ("a", MTy::Int4),
    ("b", MTy::Int8),
    ("s", MTy::Text),
    ("n", MTy::Num),
    ("c", MTy::Chr),
    ("d", MTy::Date),
    ("ts", MTy::Ts),
    ("f", MTy::Bool),
];

#[derive(Clone, Debug)]
pub struct MTable {
    pub name: String,
    pub cols: Vec<(String, MTy)>,
    /// PRIMARY KEY の列（空なら無し）
    pub pk: Vec<String>,
    /// 単一列の UNIQUE
    pub uniq: Vec<String>,
    /// k が serial
    pub serial: bool,
}

impl MTable {
    pub fn has(&self, col: &str) -> bool {
        self.cols.iter().any(|(n, _)| n == col)
    }

    pub fn names(&self) -> String {
        self.cols
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 結果の全列を ORDER BY で並べる句（列名）。
    pub fn order_all(&self) -> String {
        self.names()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// 制約なし
    Plain,
    /// PRIMARY KEY (k)
    Pk,
    /// PRIMARY KEY (k, a)
    PkKA,
    /// PRIMARY KEY (k) で k は serial
    PkSerial,
    /// UNIQUE (a)（k は制約なし）
    UniqA,
    /// PK(k) と UNIQUE(a)
    PkUniq,
}

pub fn pick_key(rng: &mut Rng) -> Key {
    match rng.weighted(&[40, 25, 8, 6, 8, 13]) {
        0 => Key::Plain,
        1 => Key::Pk,
        2 => Key::PkKA,
        3 => Key::PkSerial,
        4 => Key::UniqA,
        _ => Key::PkUniq,
    }
}

/// 表を作って CREATE 文を積む。`extra_pct` はほかの列を選ぶ確率。
pub fn make_table(ctx: &mut Ctx, key: Key, extra_pct: u64) -> MTable {
    let name = ctx.fresh_table_name();
    let mut cols = vec![("k".to_string(), MTy::Int4), ("a".to_string(), MTy::Int4)];
    for (n, t) in COLS.iter().skip(2) {
        if ctx.rng.chance(extra_pct) {
            cols.push((n.to_string(), *t));
        }
    }
    let mut t = MTable {
        name,
        cols,
        pk: Vec::new(),
        uniq: Vec::new(),
        serial: false,
    };
    match key {
        Key::Plain => {}
        Key::Pk => t.pk = vec!["k".into()],
        Key::PkKA => t.pk = vec!["k".into(), "a".into()],
        Key::PkSerial => {
            t.pk = vec!["k".into()];
            t.serial = true;
        }
        Key::UniqA => t.uniq = vec!["a".into()],
        Key::PkUniq => {
            t.pk = vec!["k".into()];
            t.uniq = vec!["a".into()];
        }
    }
    let sql = create_sql(ctx, &t);
    ctx.push(sql);
    t
}

pub fn create_sql(ctx: &mut Ctx, t: &MTable) -> String {
    let inline_pk = t.pk.len() == 1 && ctx.rng.chance(50);
    let mut defs = Vec::new();
    for (n, ty) in &t.cols {
        let mut d = if n == "k" && t.serial {
            "k serial".to_string()
        } else {
            format!("{n} {}", ty.sql())
        };
        if inline_pk && t.pk[0] == *n {
            d.push_str(" PRIMARY KEY");
        }
        if t.uniq.contains(n) && ctx.rng.chance(50) {
            d.push_str(" UNIQUE");
        }
        defs.push(d);
    }
    // 列定義の中で UNIQUE を付けなかったものはテーブル制約で付ける
    let inlined: Vec<bool> = defs.iter().map(|d| d.ends_with(" UNIQUE")).collect();
    for (i, u) in t.uniq.iter().enumerate() {
        let done = t
            .cols
            .iter()
            .position(|(n, _)| n == u)
            .map(|p| inlined[p])
            .unwrap_or(false);
        if !done {
            defs.push(format!("UNIQUE ({u})"));
        }
        let _ = i;
    }
    if !t.pk.is_empty() && !inline_pk {
        defs.push(format!("PRIMARY KEY ({})", t.pk.join(", ")));
    }
    format!("CREATE TABLE {} ({});", t.name, defs.join(", "))
}

pub fn shuffle<T>(rng: &mut Rng, v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        v.swap(i, j);
    }
}

/// lo..=hi から重複なしで n 個。
pub fn distinct_ints(rng: &mut Rng, n: usize, lo: i64, hi: i64) -> Vec<i64> {
    let mut all: Vec<i64> = (lo..=hi).collect();
    shuffle(rng, &mut all);
    all.truncate(n);
    all
}

/// 列の型ごとのリテラル。NULL は `null_pct` パーセント。
pub fn lit(rng: &mut Rng, ty: MTy, null_pct: u64) -> String {
    if rng.chance(null_pct) {
        return "NULL".into();
    }
    match ty {
        MTy::Int4 => rng.range(0, 6).to_string(),
        MTy::Int8 => {
            if rng.chance(10) {
                "3000000000".into()
            } else {
                rng.range(0, 6).to_string()
            }
        }
        MTy::Text => format!("'{}'", rng.pick(&["a", "ab", "abc", "b", "", "ba", "zz"])),
        MTy::Num => rng
            .pick(&[
                "0", "1.5", "2.25", "(-3.10)", "10", "99.99", "100.5", "1.50",
            ])
            .to_string(),
        MTy::Chr => format!("'{}'", rng.pick(&["a", "ab", "abc", "b", "x", "ba"])),
        MTy::Date => format!(
            "'{}'",
            rng.pick(&[
                "2024-01-01",
                "2024-01-15",
                "2024-02-29",
                "2023-12-31",
                "1999-12-31",
                "2024-01-15"
            ])
        ),
        MTy::Ts => format!(
            "'{}'",
            rng.pick(&[
                "2024-01-15 10:30:00",
                "2024-01-15 00:00:00",
                "2024-02-01 23:59:59",
                "2024-01-15 10:30:00.5",
                "2000-01-01 00:00:00",
                "2024-01-15 10:30:00"
            ])
        ),
        MTy::Bool => rng.pick(&["true", "false"]).to_string(),
    }
}

/// INSERT 文。`clean` なら主キー・UNIQUE を破らない（serial の k は省略する）。
pub fn insert_sql(ctx: &mut Ctx, t: &MTable, nrows: usize, clean: bool, null_pct: u64) -> String {
    let nrows = nrows.max(1);
    let pk_distinct = !t.pk.is_empty() && clean;
    let ks = distinct_ints(&mut ctx.rng, nrows, 0, 9);
    let mut pairs: Vec<(i64, i64)> = Vec::new();
    if pk_distinct && t.pk.len() == 2 {
        let mut seen = std::collections::BTreeSet::new();
        let mut guard = 0;
        while pairs.len() < nrows && guard < 100 {
            guard += 1;
            let p = (ctx.rng.range(0, 3), ctx.rng.range(0, 3));
            if seen.insert(p) {
                pairs.push(p);
            }
        }
    }
    let uniq_vals = distinct_ints(&mut ctx.rng, nrows, 0, 9);
    let omit_k = t.serial && (clean || ctx.rng.chance(50));
    let col_names: Vec<&String> = t
        .cols
        .iter()
        .map(|(n, _)| n)
        .filter(|n| !(omit_k && *n == "k"))
        .collect();
    let mut rows = Vec::new();
    for r in 0..nrows.min(if pk_distinct && t.pk.len() == 2 {
        pairs.len()
    } else {
        nrows
    }) {
        let mut vals = Vec::new();
        for (n, ty) in &t.cols {
            if omit_k && n == "k" {
                continue;
            }
            let v = if pk_distinct && t.pk.len() == 2 && (n == "k" || n == "a") {
                (if n == "k" { pairs[r].0 } else { pairs[r].1 }).to_string()
            } else if n == "k" && pk_distinct {
                ks[r].to_string()
            } else if t.uniq.contains(n) && clean {
                if ctx.rng.chance(10) {
                    "NULL".into()
                } else {
                    uniq_vals[r].to_string()
                }
            } else if n == "k" && !t.pk.is_empty() {
                // 重複を許す: 小さな範囲から
                ctx.rng.range(0, 4).to_string()
            } else if n == "k" {
                lit(&mut ctx.rng, *ty, 15)
            } else if t.pk.contains(n) {
                ctx.rng.range(0, 4).to_string()
            } else {
                lit(&mut ctx.rng, *ty, null_pct)
            };
            vals.push(v);
        }
        rows.push(format!("({})", vals.join(", ")));
    }
    if rows.is_empty() {
        rows.push(format!(
            "({})",
            col_names
                .iter()
                .map(|_| "NULL")
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let cols = if omit_k {
        format!(
            " ({})",
            col_names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    format!("INSERT INTO {}{cols} VALUES {};", t.name, rows.join(", "))
}

/// 表を 1〜2 回の INSERT で埋める。
pub fn fill(ctx: &mut Ctx, t: &MTable, null_pct: u64) {
    let n = ctx.rng.range(3, 8) as usize;
    let s = insert_sql(ctx, t, n, true, null_pct);
    ctx.push(s);
}

/// 結合などで使う表の組。`n` 表。k に重なりが出る。
pub fn setup(ctx: &mut Ctx, n: usize, null_pct: u64) -> Vec<MTable> {
    let mut v = Vec::new();
    for _ in 0..n {
        let key = pick_key(&mut ctx.rng);
        let t = make_table(ctx, key, 45);
        fill(ctx, &t, null_pct);
        if t.pk.is_empty() && ctx.rng.chance(40) {
            // 重複のある表は 2 回目の INSERT で重複キーを足す
            let s = insert_sql(ctx, &t, 3, false, null_pct);
            ctx.push(s);
        }
        v.push(t);
    }
    v
}

/// 1 列の原子的な述語。
pub fn atom(rng: &mut Rng, al: &str, t: &MTable) -> String {
    let (name, ty) = rng.pick(&t.cols).clone();
    let c = format!("{al}.{name}");
    match ty {
        MTy::Int4 | MTy::Int8 => match rng.below(9) {
            0 => format!("{c} = {}", rng.range(0, 5)),
            1 => format!("{c} > {}", rng.range(0, 5)),
            2 => format!("{c} <= {}", rng.range(0, 5)),
            3 => format!("{c} <> {}", rng.range(0, 5)),
            4 => format!(
                "{c} IN ({}, {}, {})",
                rng.range(0, 6),
                rng.range(0, 6),
                rng.range(0, 6)
            ),
            5 => format!("{c} BETWEEN {} AND {}", rng.range(0, 3), rng.range(3, 6)),
            6 => format!("{c} IS NULL"),
            7 => format!("{c} IS NOT NULL"),
            _ => format!("{c} + 1 >= {}", rng.range(1, 6)),
        },
        MTy::Text => match rng.below(6) {
            0 => format!("{c} = {}", lit(rng, ty, 0)),
            1 => format!("{c} > {}", lit(rng, ty, 0)),
            2 => format!("{c} LIKE '{}%'", rng.pick(&["a", "b", "ab"])),
            3 => format!("{c} IS NOT NULL"),
            4 => format!("{c} <= {}", lit(rng, ty, 0)),
            _ => format!("length({c}) > {}", rng.range(0, 2)),
        },
        MTy::Num => match rng.below(5) {
            0 => format!("{c} > {}", lit(rng, ty, 0)),
            1 => format!("{c} <= {}", lit(rng, ty, 0)),
            2 => format!("{c} = {}", lit(rng, ty, 0)),
            3 => format!("{c} IS NULL"),
            _ => format!("{c} BETWEEN 0 AND 50"),
        },
        MTy::Chr => match rng.below(4) {
            0 => format!("{c} = {}", lit(rng, ty, 0)),
            1 => format!("{c} < {}", lit(rng, ty, 0)),
            2 => format!("{c} IS NOT NULL"),
            _ => format!("{c} >= {}", lit(rng, ty, 0)),
        },
        MTy::Date => match rng.below(5) {
            0 => format!("{c} >= {}", lit(rng, ty, 0)),
            1 => format!("{c} = date {}", lit(rng, ty, 0)),
            2 => format!("{c} < {}", lit(rng, ty, 0)),
            3 => format!("{c} IS NULL"),
            _ => format!("{c} BETWEEN '2024-01-01' AND '2024-12-31'"),
        },
        MTy::Ts => match rng.below(4) {
            0 => format!("{c} > {}", lit(rng, ty, 0)),
            1 => format!("{c} <= {}", lit(rng, ty, 0)),
            2 => format!("{c}::date = '2024-01-15'"),
            _ => format!("{c} IS NOT NULL"),
        },
        MTy::Bool => match rng.below(4) {
            0 => c,
            1 => format!("NOT {c}"),
            2 => format!("{c} IS TRUE"),
            _ => format!("{c} IS NULL"),
        },
    }
}

pub fn pred(rng: &mut Rng, al: &str, t: &MTable, depth: u32) -> String {
    if depth > 0 && rng.chance(35) {
        let a = pred(rng, al, t, depth - 1);
        return match rng.below(3) {
            0 => format!("({a} AND {})", pred(rng, al, t, depth - 1)),
            1 => format!("({a} OR {})", pred(rng, al, t, depth - 1)),
            _ => format!("NOT ({a})"),
        };
    }
    atom(rng, al, t)
}

/// ORDER BY の方向。
pub fn dir(rng: &mut Rng) -> &'static str {
    match rng.below(8) {
        0 => " DESC",
        1 => " ASC",
        2 => " NULLS FIRST",
        3 => " DESC NULLS LAST",
        _ => "",
    }
}

/// `ORDER BY 1 [dir], 2 [dir], ...`（全出力列）。
pub fn order_positions(rng: &mut Rng, n: usize) -> String {
    (1..=n)
        .map(|i| format!("{i}{}", dir(rng)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 全出力列を並べた ORDER BY が付くときの LIMIT / OFFSET。
pub fn limit(rng: &mut Rng) -> String {
    if rng.chance(30) {
        let mut s = format!(" LIMIT {}", rng.range(0, 6));
        if rng.chance(30) {
            s.push_str(&format!(" OFFSET {}", rng.range(0, 3)));
        }
        s
    } else {
        String::new()
    }
}

/// 構文・名前のエラー（SQLSTATE が決まっているもの）。
pub fn error_query(ctx: &mut Ctx, tabs: &[MTable]) -> String {
    let t = ctx.rng.pick(tabs).clone();
    let u = ctx.rng.pick(tabs).clone();
    match ctx.rng.below(8) {
        0 => format!("SELECT x.nope FROM {} x;", t.name),
        1 => format!("SELECT k FROM {} x JOIN {} y ON x.k = y.k;", t.name, u.name),
        2 => format!("SELECT * FROM {} x JOIN {} y USING (nope);", t.name, u.name),
        3 => format!("SELECT x.k FROM {} x, {} x;", t.name, u.name),
        4 => format!(
            "SELECT x.k FROM {} x WHERE x.k IN (SELECT y.k, y.a FROM {} y);",
            t.name, u.name
        ),
        5 => format!("SELECT x.k FROM {} x WHERE count(*) > 1;", t.name),
        6 => format!(
            "SELECT x.k FROM {} x UNION SELECT y.k, y.a FROM {} y;",
            t.name, u.name
        ),
        _ => format!("SELECT sum(x.k), x.a FROM {} x;", t.name),
    }
}

/// 同じ問い合わせを `enable_*` を変えて流し、基準の結果と同じことを確かめる（検査 (d)）。
/// `no_full` が true のとき hashjoin を切らない（FULL はハッシュだけ。KD-2）。
pub fn plan_variant(ctx: &mut Ctx, base: usize, no_full: bool) {
    let sql = ctx.stmts[base].clone();
    let ordered = ctx.q.get(&base).copied().unwrap_or(false);
    let profiles: [(&str, &[&str]); 8] = [
        ("hashjoin", &["enable_hashjoin"]),
        ("nestloop", &["enable_nestloop"]),
        ("both", &["enable_hashjoin", "enable_nestloop"]),
        ("indexscan", &["enable_indexscan"]),
        ("seqscan", &["enable_seqscan"]),
        ("hashagg", &["enable_hashagg"]),
        ("material", &["enable_material"]),
        ("sort", &["enable_sort"]),
    ];
    let mut idx = ctx.rng.below(profiles.len() as u64) as usize;
    if no_full && profiles[idx].1.contains(&"enable_hashjoin") {
        idx = 3;
    }
    let names = profiles[idx].1;
    for n in names {
        ctx.push(format!("SET {n} = off;"));
    }
    let j = ctx.push_q(sql, ordered);
    ctx.pairs.push((base, j));
    for n in names {
        ctx.push(format!("RESET {n};"));
    }
}
