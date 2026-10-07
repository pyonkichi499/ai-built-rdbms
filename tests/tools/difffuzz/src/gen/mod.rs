//! SQL 生成器。領域（expr / types / query / dml / txn）ごとにモジュールを持つ。
//!
//! 新しい領域を足すには、`fn scenario(&mut Ctx)` を持つモジュールを作り、`DOMAINS` と `generate` に 1 行足す。
//! 生成器はサーバの応答を見ない（同じ (seed, case) なら常に同じ SQL になる）。
//! 1 文は 1 行・末尾 `;`・引用符と括弧が釣り合っていること（psql が文の終わりを見失わないため）。

pub mod dml;
pub mod expr;
pub mod expr_extra;
pub mod m4;
pub mod m4_agg;
pub mod m4_ddl;
pub mod m4_index;
pub mod m4_join;
pub mod m4_setop;
pub mod m4_subq;
#[cfg(test)]
mod m4_tests;
pub mod query;
pub mod query_extra;
pub mod query_r3;
pub mod txn;
pub mod txn_r3;
pub mod types;
pub mod values;

use crate::rng::Rng;

/// M1〜M3 の領域（M5 の機能で `0A000` になる文を含む。`--skip-unsupported-legacy` の対象）。
pub const LEGACY_DOMAINS: [&str; 5] = ["expr", "types", "query", "dml", "txn"];

pub const DOMAINS: [&str; 11] = [
    "expr", "types", "query", "dml", "txn", "join", "agg", "subquery", "setop", "index", "ddl",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Int2,
    Int4,
    Int8,
    Numeric,
    /// 精度なし numeric
    NumU,
    /// numeric(5,1)
    Num51,
    Float4,
    Float8,
    Text,
    Varchar(u32),
    Bool,
}

/// 式生成で扱う型の系統。Float8 は式には使わない（出力の揺れを避ける）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cls {
    Int,
    Num,
    Text,
    Bool,
}

impl Ty {
    pub fn sql(self) -> String {
        match self {
            Ty::Int2 => "smallint".into(),
            Ty::Int4 => "integer".into(),
            Ty::Int8 => "bigint".into(),
            Ty::Numeric => "numeric(10,2)".into(),
            Ty::NumU => "numeric".into(),
            Ty::Num51 => "numeric(5,1)".into(),
            Ty::Float4 => "real".into(),
            Ty::Float8 => "double precision".into(),
            Ty::Text => "text".into(),
            Ty::Varchar(n) => format!("varchar({n})"),
            Ty::Bool => "boolean".into(),
        }
    }

    pub fn cls(self) -> Option<Cls> {
        match self {
            Ty::Int2 | Ty::Int4 | Ty::Int8 => Some(Cls::Int),
            Ty::Numeric | Ty::NumU | Ty::Num51 => Some(Cls::Num),
            Ty::Text | Ty::Varchar(_) => Some(Cls::Text),
            Ty::Bool => Some(Cls::Bool),
            Ty::Float8 | Ty::Float4 => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Col {
    pub name: String,
    pub ty: Ty,
    pub not_null: bool,
    pub default: Option<String>,
    pub check: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub cols: Vec<Col>,
}

impl Table {
    /// 式で使える列（名前, 系統）。
    pub fn scope(&self) -> Vec<(String, Cls)> {
        self.cols
            .iter()
            .filter_map(|c| c.ty.cls().map(|k| (c.name.clone(), k)))
            .collect()
    }

    pub fn positions(&self) -> String {
        (1..=self.cols.len())
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub struct Ctx {
    pub rng: Rng,
    /// テーブル名の接頭辞 `fz_<seed>_<case>_`
    pub prefix: String,
    pub tables: Vec<Table>,
    pub stmts: Vec<String>,
    /// M4 領域の読み取り文（添字 -> 全出力列を ORDER BY で並べているか）。多重集合比較と評価順エラーの別扱いの対象。
    pub q: std::collections::BTreeMap<usize, bool>,
    /// (基準の文の添字, 変種の文の添字)。同じサーバ上で結果が同じであることを確かめる（プラン変種・索引の有無）。
    pub pairs: Vec<(usize, usize)>,
    next_table: usize,
}

impl Ctx {
    pub fn new(seed: u64, case: u64) -> Self {
        Ctx {
            rng: Rng::for_case(seed, case),
            prefix: format!("fz_{seed}_{case}_"),
            tables: Vec::new(),
            stmts: Vec::new(),
            q: Default::default(),
            pairs: Vec::new(),
            next_table: 0,
        }
    }

    pub fn push(&mut self, s: impl Into<String>) {
        self.stmts.push(s.into());
    }

    /// 読み取り文を積んで添字を返す。`ordered` は全出力列を ORDER BY で並べているとき true。
    pub fn push_q(&mut self, s: impl Into<String>, ordered: bool) -> usize {
        self.stmts.push(s.into());
        let i = self.stmts.len() - 1;
        self.q.insert(i, ordered);
        i
    }

    pub fn push_pick(&mut self, opts: &[&str]) {
        let s = (*self.rng.pick(opts)).to_string();
        self.stmts.push(s);
    }

    pub fn fresh_table_name(&mut self) -> String {
        let n = format!("{}t{}", self.prefix, self.next_table);
        self.next_table += 1;
        n
    }

    /// 後始末用。PG 側にもテーブルを残さない。
    pub fn cleanup_stmts(&self) -> Vec<String> {
        let mut v = vec!["ROLLBACK;".to_string()];
        v.extend(
            (0..self.next_table).map(|i| format!("DROP TABLE IF EXISTS {}t{i};", self.prefix)),
        );
        v
    }
}

pub fn generate(domain: &str, ctx: &mut Ctx) -> Result<(), String> {
    match domain {
        "expr" => expr::scenario(ctx),
        "types" => types::scenario(ctx),
        "query" => { query::scenario(ctx); query::drop_known_missing(ctx) }
        "dml" => dml::scenario(ctx),
        "txn" => txn::scenario(ctx),
        "join" => m4_join::scenario(ctx),
        "agg" => m4_agg::scenario(ctx),
        "subquery" => m4_subq::scenario(ctx),
        "setop" => m4_setop::scenario(ctx),
        "index" => m4_index::scenario(ctx),
        "ddl" => m4_ddl::scenario(ctx),
        d => {
            return Err(format!(
                "unknown domain: {d} (expr, types, query, dml, txn, join, agg, subquery, setop, index, ddl, all)"
            ))
        }
    }
    Ok(())
}

/// 乱数で列定義を作る。`n` 列、型は `tys` から。
pub fn random_columns(ctx: &mut Ctx, n: usize, tys: &[Ty], constraints: bool) -> Vec<Col> {
    let mut cols = Vec::new();
    for i in 0..n {
        let ty = *ctx.rng.pick(tys);
        let mut c = Col {
            name: format!("c{i}"),
            ty,
            not_null: false,
            default: None,
            check: None,
        };
        if constraints {
            c.not_null = ctx.rng.chance(25);
            if ctx.rng.chance(30) {
                c.default = Some(values::literal(&mut ctx.rng, ty, false));
            }
            if ctx.rng.chance(20) {
                c.check = match ty {
                    Ty::Int2 | Ty::Int4 | Ty::Int8 => Some(format!("c{i} >= 0")),
                    Ty::Text | Ty::Varchar(_) => Some(format!("length(c{i}) < 6")),
                    Ty::Numeric => Some(format!("c{i} <> 0")),
                    _ => None,
                };
            }
        }
        cols.push(c);
    }
    cols
}

pub fn create_table_sql(t: &Table) -> String {
    let defs: Vec<String> = t
        .cols
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", c.name, c.ty.sql());
            if let Some(d) = &c.default {
                s.push_str(&format!(" DEFAULT {d}"));
            }
            if c.not_null {
                s.push_str(" NOT NULL");
            }
            if let Some(k) = &c.check {
                s.push_str(&format!(" CHECK ({k})"));
            }
            s
        })
        .collect();
    format!("CREATE TABLE {} ({});", t.name, defs.join(", "))
}

/// 作ったテーブルを ctx に登録して CREATE 文を積む。
pub fn make_table(ctx: &mut Ctx, n: usize, tys: &[Ty], constraints: bool) -> usize {
    let name = ctx.fresh_table_name();
    let cols = random_columns(ctx, n, tys, constraints);
    let t = Table { name, cols };
    let sql = create_table_sql(&t);
    ctx.push(sql);
    ctx.tables.push(t);
    ctx.tables.len() - 1
}

pub fn insert_row_sql(ctx: &mut Ctx, ti: usize, nrows: usize, invalid_pct: u64) -> String {
    let t = ctx.tables[ti].clone();
    let rows: Vec<String> = (0..nrows)
        .map(|_| {
            let vals: Vec<String> = t
                .cols
                .iter()
                .map(|c| {
                    let bad = ctx.rng.chance(invalid_pct);
                    values::literal(&mut ctx.rng, c.ty, bad)
                })
                .collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    format!("INSERT INTO {} VALUES {};", t.name, rows.join(", "))
}

pub fn select_all(t: &Table) -> String {
    format!("SELECT * FROM {} ORDER BY {};", t.name, t.positions())
}

/// PG でエラーになる文。テーブルがあればテーブルに絡むものも混ぜる。
pub fn error_stmt(ctx: &mut Ctx) -> String {
    let generic: [&str; 14] = [
        "SELECT 1 / 0;",
        "SELECT 'abc'::integer;",
        "SELECT 1 + 'a';",
        "SELECT nosuchfn(1);",
        "SELECT nocol;",
        "SELECT 1 +;",
        "SELECT 2147483647 + 1;",
        "SELECT NOT 1;",
        "SELECT CASE WHEN 1 THEN 2 END;",
        "SELECT * FROM fz_no_such_table;",
        "SELEC 1;",
        "SELECT 'x' LIKE 1;",
        "SELECT 9223372036854775807::bigint + 1;",
        "SELECT 1::boolean AND 2;",
    ];
    if !ctx.tables.is_empty() && ctx.rng.chance(60) {
        let t = ctx.rng.pick(&ctx.tables.clone()).clone();
        let c = ctx.rng.pick(&t.cols).clone();
        return match ctx.rng.below(7) {
            0 => format!("SELECT nosuchcol FROM {};", t.name),
            1 => format!(
                "INSERT INTO {} VALUES (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);",
                t.name
            ),
            2 => format!(
                "INSERT INTO {} ({}) VALUES (DEFAULT, DEFAULT);",
                t.name, c.name
            ),
            3 => format!("UPDATE {} SET nosuchcol = 1;", t.name),
            4 => format!(
                "SELECT {} + {} FROM {};",
                c.name,
                c.name,
                t.name.replace("_t", "_x")
            ),
            5 => format!(
                "DELETE FROM {} WHERE {} = 'zz_{}';",
                t.name,
                c.name,
                ctx.rng.below(9)
            ),
            _ => format!("CREATE TABLE {} (a integer);", t.name),
        };
    }
    ctx.rng.pick(&generic).to_string()
}
