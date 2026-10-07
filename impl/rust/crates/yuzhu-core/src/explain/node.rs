//! `ExplainNode` の補助（`m4/10-explain-copy-compat.md` §3.3、§3.5、§3.6、§3.8、§3.9）。
//!
//! `planner::explain_tree`（L2）が使う部品: ノードのタイトル、範囲表の数（`count_rtes`）、修飾の 3 つの規則、
//! 別名の重複（`_1`）、列の表示（`ColNames`）、コスト欄の width。木の型は `planner::physical::ExplainNode`。

use std::collections::{HashMap, HashSet};

use crate::analyzer::query::{
    BoundExpr, BoundQuery, BoundSelect, BoundStatement, FromItem, Rte, RteKind, SetOpKind,
    UpdateSource,
};
use crate::deparse::ident::{quote_identifier, quote_qualified};
use crate::deparse::{ColText, ColumnNamer};
use crate::error::{Error, Result};
use crate::expr::{ColId, ExprKind};
use crate::planner::logical::JoinKind;
use crate::types::{SqlType, VARHDRSZ, oid};

// ----- タイトル -------------------------------------------------------------------

/// リレーション名の表示。`verbose` は `schema.name`（§3.6）。
pub fn relation_text(schema: &str, name: &str, verbose: bool) -> String {
    if verbose {
        quote_qualified(schema, name)
    } else {
        quote_identifier(name)
    }
}

/// 別名があり、**名前と違えば** ` {alias}`（§3.6）。
fn alias_suffix(name: &str, alias: Option<&str>) -> String {
    match alias {
        Some(a) if a != name => format!(" {}", quote_identifier(a)),
        _ => String::new(),
    }
}

/// `Seq Scan on t` / `Seq Scan on public.t x`。
pub fn seq_scan_title(schema: &str, rel: &str, alias: Option<&str>, verbose: bool) -> String {
    format!(
        "Seq Scan on {}{}",
        relation_text(schema, rel, verbose),
        alias_suffix(rel, alias)
    )
}

/// `Index Scan using t_pkey on t`（`backward` なら `Index Scan Backward using ...`）。インデックス名は修飾しない。
pub fn index_scan_title(
    index: &str,
    schema: &str,
    rel: &str,
    alias: Option<&str>,
    verbose: bool,
    backward: bool,
) -> String {
    format!(
        "Index Scan{} using {} on {}{}",
        if backward { " Backward" } else { "" },
        quote_identifier(index),
        relation_text(schema, rel, verbose),
        alias_suffix(rel, alias)
    )
}

/// `Function Scan on generate_series g`。VERBOSE は `pg_catalog.generate_series`。別名が関数名と同じなら付けない。
pub fn function_scan_title(func: &str, alias: Option<&str>, verbose: bool) -> String {
    format!(
        "Function Scan on {}{}",
        relation_text("pg_catalog", func, verbose),
        alias_suffix(func, alias)
    )
}

/// `Values Scan on "*VALUES*"`（別名があればその名前）。
pub fn values_scan_title(alias: Option<&str>) -> String {
    match alias {
        Some(a) => format!("Values Scan on {}", quote_identifier(a)),
        None => "Values Scan on \"*VALUES*\"".to_owned(),
    }
}

/// `CTE Scan on c` / `CTE Scan on c c1`。
pub fn cte_scan_title(cte: &str, alias: Option<&str>) -> String {
    format!(
        "CTE Scan on {}{}",
        quote_identifier(cte),
        alias_suffix(cte, alias)
    )
}

/// 結合のアルゴリズム。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinAlgo {
    NestedLoop,
    Hash,
}

/// 結合ノードのタイトル（§3.3 の表。PostgreSQL 17 [実機] と一致させる）。
/// `build_is_left` は `Hash` のときだけ使う（左がビルド側なら Right 系の名前）。
pub fn join_title(algo: JoinAlgo, kind: JoinKind, build_is_left: bool) -> String {
    match algo {
        JoinAlgo::NestedLoop => match kind {
            JoinKind::Inner => "Nested Loop".to_owned(),
            JoinKind::Left => "Nested Loop Left Join".to_owned(),
            JoinKind::Full => "Nested Loop Full Join".to_owned(),
            JoinKind::Semi => "Nested Loop Semi Join".to_owned(),
            JoinKind::Anti => "Nested Loop Anti Join".to_owned(),
        },
        JoinAlgo::Hash => match (kind, build_is_left) {
            (JoinKind::Inner, _) => "Hash Join",
            (JoinKind::Left, false) => "Hash Left Join",
            (JoinKind::Left, true) => "Hash Right Join",
            (JoinKind::Full, _) => "Hash Full Join",
            (JoinKind::Semi, false) => "Hash Semi Join",
            (JoinKind::Semi, true) => "Hash Right Semi Join",
            (JoinKind::Anti, false) => "Hash Anti Join",
            (JoinKind::Anti, true) => "Hash Right Anti Join",
        }
        .to_owned(),
    }
}

/// `HashSetOp Intersect` / `HashSetOp Except All`。`Union` は `Append` / `HashAggregate` で表すのでここには来ない。
pub fn hash_setop_title(op: SetOpKind, all: bool) -> Result<String> {
    let name = match op {
        SetOpKind::Intersect => "Intersect",
        SetOpKind::Except => "Except",
        SetOpKind::Union => return Err(Error::internal("HashSetOp for UNION")),
    };
    Ok(format!("HashSetOp {name}{}", if all { " All" } else { "" }))
}

/// DML ノードの種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DmlKind {
    Insert,
    Update,
    Delete,
}

/// `Insert on public.t` / `Update on t x`。
pub fn dml_title(
    kind: DmlKind,
    schema: &str,
    rel: &str,
    alias: Option<&str>,
    verbose: bool,
) -> String {
    let word = match kind {
        DmlKind::Insert => "Insert",
        DmlKind::Update => "Update",
        DmlKind::Delete => "Delete",
    };
    format!(
        "{word} on {}{}",
        relation_text(schema, rel, verbose),
        alias_suffix(rel, alias)
    )
}

// ----- 別名の重複 -----------------------------------------------------------------

/// 1 つの計画の中で別名を一意にする（§3.6。PostgreSQL の `set_rtable_names`）。
/// 最初の出現は元の名前、2 回目以降は `{名前}_{n}`（`n` は 1 から。使用済みなら次の番号）。
#[derive(Debug, Default)]
pub struct AliasAllocator {
    used: HashSet<String>,
}

impl AliasAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allocate(&mut self, base: &str) -> String {
        if self.used.insert(base.to_owned()) {
            return base.to_owned();
        }
        let mut n = 1u32;
        loop {
            let cand = format!("{base}_{n}");
            if self.used.insert(cand.clone()) {
                return cand;
            }
            n += 1;
        }
    }
}

// ----- 修飾の規則 -----------------------------------------------------------------

/// §3.5 の 3 つの規則。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QualifyRule {
    /// `Output:` の各要素。
    Output,
    /// スキャンノードの `Filter:` `Index Cond:` `Function Call:`。
    Scan,
    /// `Sort Key` `Group Key` `Hash Cond` `Join Filter`、結合と集約の `Filter:`、`One-Time Filter`。
    Upper,
}

/// 列名に表名を付けるか。`n_rtable` は文全体の範囲表の数（[`count_rtes`]）。
/// 外側の列を参照するパラメータは、この規則に関係なく常に修飾する（呼び出し側が判断する）。
pub fn use_prefix(rule: QualifyRule, verbose: bool, n_rtable: usize) -> bool {
    match rule {
        QualifyRule::Output => n_rtable > 1,
        QualifyRule::Scan => verbose,
        QualifyRule::Upper => verbose || n_rtable > 1,
    }
}

// ----- 列の表示 -------------------------------------------------------------------

impl ColText {
    /// ベーステーブルの列（修飾の判断を済ませた名前。参照するとき包まない）。
    pub fn plain(text: impl Into<String>) -> Self {
        ColText {
            text: text.into(),
            wrap: false,
        }
    }

    /// 計算列（式の deparse。上のノードが参照するとき `(` `)` で包む）。
    pub fn computed(text: impl Into<String>) -> Self {
        ColText {
            text: text.into(),
            wrap: true,
        }
    }
}

/// `ColId` → 表示（§3.8 の `names`）。ノードを下から作るたびに足す。
#[derive(Debug, Default, Clone)]
pub struct ColNames {
    map: HashMap<ColId, ColText>,
}

impl ColNames {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: ColId, text: ColText) {
        self.map.insert(id, text);
    }

    pub fn get(&self, id: ColId) -> Option<&ColText> {
        self.map.get(&id)
    }
}

impl ColumnNamer<ColId> for ColNames {
    fn name(&self, col: &ColId) -> Result<ColText> {
        self.map
            .get(col)
            .cloned()
            .ok_or_else(|| Error::internal(format!("no display name for column {}", col.0)))
    }
}

// ----- コスト欄の width -------------------------------------------------------------

/// 出力列 1 つの平均幅（§3.9。値に意味はなく、書式を壊さないためのもの）。
pub fn type_width(ty: SqlType) -> u32 {
    match ty.oid {
        oid::BOOL | oid::CHAR => 1,
        oid::INT2 => 2,
        oid::INT4 | oid::FLOAT4 | oid::DATE | oid::OID | oid::REGCLASS => 4,
        oid::INT8 | oid::FLOAT8 | oid::TIMESTAMP | oid::TIMESTAMPTZ => 8,
        oid::VARCHAR | oid::BPCHAR if ty.typmod > VARHDRSZ => {
            let n = u32::try_from(ty.typmod - VARHDRSZ).unwrap_or(0);
            if n <= 32 {
                n
            } else {
                32 + (n.min(1000) - 32) / 2
            }
        }
        // numeric・text・typmod なしの可変長型はどれも 32。
        _ => 32,
    }
}

/// ノードの出力列の幅の和。
pub fn row_width(types: impl IntoIterator<Item = SqlType>) -> u32 {
    types.into_iter().map(type_width).sum()
}

// ----- count_rtes ---------------------------------------------------------------

/// 文全体の範囲表の数（`n_rtable`。§3.5）。JOIN の `Rte`・FROM 句の副問い合わせ・VALUES・CTE 参照・
/// 式の中の副問い合わせ・CTE の本体のものを含む（インライン展開で消えたものも数える）。
///
/// PostgreSQL の最終範囲表に合わせる: `INSERT` は対象表 1 つに加え、1 行の `VALUES` は 0、複数行の `VALUES` は
/// `*VALUES*` の 1、`SELECT` は `*SELECT*` の 1 + 本体の数。集合演算は腕（集合演算でないもの）ごとに 1 + 腕の数。
pub fn count_rtes(stmt: &BoundStatement) -> usize {
    match stmt {
        BoundStatement::Select(q) => count_query(q),
        BoundStatement::Insert(ins) => {
            let src = &ins.source;
            let source = match &src.body {
                crate::analyzer::query::BoundSetExpr::Values { rows, .. }
                    if rows.len() == 1 && src.ctes.is_empty() =>
                {
                    rows.iter().flatten().map(count_expr).sum::<usize>()
                }
                crate::analyzer::query::BoundSetExpr::Values { .. } => count_query(src),
                _ => 1 + count_query(src),
            };
            let returning = ins
                .returning
                .iter()
                .flat_map(|r| &r.targets)
                .map(count_expr)
                .sum::<usize>();
            1 + source + returning
        }
        BoundStatement::Update(u) => {
            let assigns: usize = u
                .assignments
                .iter()
                .map(|(_, s)| match s {
                    UpdateSource::Expr(e) => count_expr(e),
                    UpdateSource::Default(e) => e.as_ref().map_or(0, count_expr),
                })
                .sum();
            count_rtable(&u.rtable)
                + count_from(&u.from)
                + u.filter.as_ref().map_or(0, count_expr)
                + assigns
                + u.returning
                    .iter()
                    .flat_map(|r| &r.targets)
                    .map(count_expr)
                    .sum::<usize>()
        }
        BoundStatement::Delete(d) => {
            count_rtable(&d.rtable)
                + count_from(&d.from)
                + d.filter.as_ref().map_or(0, count_expr)
                + d.returning
                    .iter()
                    .flat_map(|r| &r.targets)
                    .map(count_expr)
                    .sum::<usize>()
        }
        BoundStatement::Explain(e) => count_rtes(&e.inner),
        BoundStatement::Copy(_) | BoundStatement::Ddl(_) | BoundStatement::Checkpoint => 0,
    }
}

/// 式の中の副問い合わせが持つ範囲表の数。
fn count_expr(e: &BoundExpr) -> usize {
    let mut total = 0;
    e.walk(&mut |n| {
        if let ExprKind::SubLink { query, .. } = &n.kind {
            total += count_query(query);
        }
        true
    });
    total
}

fn count_rtable(rtable: &[Rte]) -> usize {
    rtable.len()
        + rtable
            .iter()
            .map(|r| match &r.kind {
                RteKind::Subquery { query } => count_query(query),
                RteKind::Values { rows } => rows.iter().flatten().map(count_expr).sum(),
                RteKind::Function { call } => count_expr(call),
                RteKind::Table { .. } | RteKind::Join { .. } | RteKind::CteRef { .. } => 0,
            })
            .sum::<usize>()
}

fn count_from(items: &[FromItem]) -> usize {
    items
        .iter()
        .map(|i| match i {
            FromItem::Scan(_) => 0,
            FromItem::Join {
                left, right, on, ..
            } => {
                count_from(std::slice::from_ref(left))
                    + count_from(std::slice::from_ref(right))
                    + on.as_ref().map_or(0, count_expr)
            }
        })
        .sum()
}

fn count_select(s: &BoundSelect) -> usize {
    count_rtable(&s.rtable)
        + count_from(&s.from)
        + s.filter
            .iter()
            .chain(&s.group_by)
            .chain(s.having.iter())
            .chain(&s.targets)
            .map(count_expr)
            .sum::<usize>()
}

fn count_query(q: &BoundQuery) -> usize {
    use crate::analyzer::query::BoundSetExpr as B;
    let ctes: usize = q.ctes.iter().map(|c| count_query(&c.query)).sum();
    let body = match &q.body {
        B::Select(s) => count_select(s),
        B::Values { rows, .. } => 1 + rows.iter().flatten().map(count_expr).sum::<usize>(),
        B::SetOp { left, right, .. } => count_arm(left) + count_arm(right),
    };
    let tail: usize = q.limit.iter().chain(&q.offset).map(count_expr).sum();
    ctes + body + tail
}

/// 集合演算の腕。さらに集合演算で、WITH・ORDER BY・LIMIT がなければ腕には `Rte` を足さない。
fn count_arm(q: &BoundQuery) -> usize {
    use crate::analyzer::query::BoundSetExpr as B;
    match &q.body {
        B::SetOp { left, right, .. }
            if q.ctes.is_empty()
                && q.order_by.is_empty()
                && q.limit.is_none()
                && q.offset.is_none() =>
        {
            count_arm(left) + count_arm(right)
        }
        _ => 1 + count_query(q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_titles() {
        assert_eq!(seq_scan_title("public", "t", None, false), "Seq Scan on t");
        assert_eq!(
            seq_scan_title("public", "t", None, true),
            "Seq Scan on public.t"
        );
        assert_eq!(
            seq_scan_title("public", "t", Some("x"), true),
            "Seq Scan on public.t x"
        );
        // 別名が名前と同じなら付けない。
        assert_eq!(
            seq_scan_title("public", "t", Some("t"), false),
            "Seq Scan on t"
        );
        assert_eq!(
            seq_scan_title("public", "MyTable", Some("T 1"), false),
            "Seq Scan on \"MyTable\" \"T 1\""
        );
        assert_eq!(
            seq_scan_title("public", "t", Some("t_1"), false),
            "Seq Scan on t t_1"
        );
    }

    #[test]
    fn index_scan_titles() {
        assert_eq!(
            index_scan_title("t_pkey", "public", "t", None, false, false),
            "Index Scan using t_pkey on t"
        );
        assert_eq!(
            index_scan_title("t_pkey", "public", "t", Some("x"), true, true),
            "Index Scan Backward using t_pkey on public.t x"
        );
    }

    #[test]
    fn function_values_cte_titles() {
        assert_eq!(
            function_scan_title("generate_series", None, false),
            "Function Scan on generate_series"
        );
        assert_eq!(
            function_scan_title("generate_series", Some("generate_series"), true),
            "Function Scan on pg_catalog.generate_series"
        );
        assert_eq!(
            function_scan_title("generate_series", Some("g"), true),
            "Function Scan on pg_catalog.generate_series g"
        );
        assert_eq!(values_scan_title(None), "Values Scan on \"*VALUES*\"");
        assert_eq!(values_scan_title(Some("v")), "Values Scan on v");
        assert_eq!(cte_scan_title("c", None), "CTE Scan on c");
        assert_eq!(cte_scan_title("c", Some("c1")), "CTE Scan on c c1");
    }

    #[test]
    fn join_titles_follow_the_table() {
        use JoinAlgo::{Hash, NestedLoop};
        let rows = [
            (JoinKind::Inner, "Hash Join", "Hash Join"),
            (JoinKind::Left, "Hash Left Join", "Hash Right Join"),
            (JoinKind::Full, "Hash Full Join", "Hash Full Join"),
            (JoinKind::Semi, "Hash Semi Join", "Hash Right Semi Join"),
            (JoinKind::Anti, "Hash Anti Join", "Hash Right Anti Join"),
        ];
        for (k, probe_left, build_left) in rows {
            assert_eq!(join_title(Hash, k, false), probe_left);
            assert_eq!(join_title(Hash, k, true), build_left);
        }
        assert_eq!(
            join_title(NestedLoop, JoinKind::Inner, false),
            "Nested Loop"
        );
        assert_eq!(
            join_title(NestedLoop, JoinKind::Left, true),
            "Nested Loop Left Join"
        );
        assert_eq!(
            join_title(NestedLoop, JoinKind::Full, false),
            "Nested Loop Full Join"
        );
        assert_eq!(
            join_title(NestedLoop, JoinKind::Semi, false),
            "Nested Loop Semi Join"
        );
        assert_eq!(
            join_title(NestedLoop, JoinKind::Anti, false),
            "Nested Loop Anti Join"
        );
    }

    #[test]
    fn setop_and_dml_titles() {
        assert_eq!(
            hash_setop_title(SetOpKind::Intersect, false).unwrap(),
            "HashSetOp Intersect"
        );
        assert_eq!(
            hash_setop_title(SetOpKind::Except, true).unwrap(),
            "HashSetOp Except All"
        );
        assert!(hash_setop_title(SetOpKind::Union, false).is_err());
        assert_eq!(
            dml_title(DmlKind::Insert, "public", "t", None, true),
            "Insert on public.t"
        );
        assert_eq!(
            dml_title(DmlKind::Update, "public", "t", Some("x"), false),
            "Update on t x"
        );
        assert_eq!(
            dml_title(DmlKind::Delete, "public", "t", None, false),
            "Delete on t"
        );
    }

    #[test]
    fn alias_allocator_numbers_repeats() {
        let mut a = AliasAllocator::new();
        assert_eq!(a.allocate("t"), "t");
        assert_eq!(a.allocate("t"), "t_1");
        assert_eq!(a.allocate("t"), "t_2");
        assert_eq!(a.allocate("u"), "u");
        // `t_1` が先に使われていれば飛ばす。
        let mut b = AliasAllocator::new();
        assert_eq!(b.allocate("t_1"), "t_1");
        assert_eq!(b.allocate("t"), "t");
        assert_eq!(b.allocate("t"), "t_2");
    }

    #[test]
    fn qualification_rules() {
        use QualifyRule::{Output, Scan, Upper};
        // 単一表: Output は修飾しない。Scan と Upper は verbose のときだけ。
        assert!(!use_prefix(Output, true, 1));
        assert!(!use_prefix(Scan, false, 1));
        assert!(use_prefix(Scan, true, 1));
        assert!(!use_prefix(Upper, false, 1));
        assert!(use_prefix(Upper, true, 1));
        // 複数: Output と Upper は常に、Scan は verbose のときだけ。
        assert!(use_prefix(Output, false, 2));
        assert!(use_prefix(Upper, false, 2));
        assert!(!use_prefix(Scan, false, 2));
    }

    #[test]
    fn column_names() {
        let mut n = ColNames::new();
        n.insert(ColId(1), ColText::plain("t.a"));
        n.insert(ColId(2), ColText::computed("(b + 1)"));
        assert_eq!(
            n.name(&ColId(1)).unwrap(),
            ColText {
                text: "t.a".into(),
                wrap: false
            }
        );
        assert!(n.name(&ColId(2)).unwrap().wrap);
        assert_eq!(n.get(ColId(2)).unwrap().text, "(b + 1)");
        assert!(n.name(&ColId(9)).is_err());
    }

    // ----- count_rtes ----------------------------------------------------------

    mod rtes {
        use super::super::*;
        use crate::analyzer::query::{
            BoundDistinct, BoundInsert, BoundSetExpr, JoinType, RteColumn,
        };
        use crate::catalog::TableDef;
        use crate::catalog::fake::table_def;
        use crate::error::Span;
        use crate::expr::{Expr, RteId, SubLinkKind};
        use std::sync::Arc;

        fn table() -> Arc<TableDef> {
            Arc::new(table_def(16384, "t", vec![], vec![]))
        }

        fn rte(kind: RteKind) -> Rte {
            Rte {
                kind,
                refname: None,
                columns: vec![RteColumn {
                    name: "a".into(),
                    ty: SqlType::INT4,
                }],
                span: Span::default(),
            }
        }

        fn base() -> Rte {
            rte(RteKind::Table { table: table() })
        }

        fn int(n: i32) -> BoundExpr {
            Expr::literal(crate::types::Datum::Int4(n), SqlType::INT4)
        }

        fn select(rtable: Vec<Rte>, filter: Option<BoundExpr>) -> BoundSelect {
            BoundSelect {
                rtable,
                from: vec![],
                filter,
                group_by: vec![],
                having: None,
                has_agg: false,
                targets: vec![int(1)],
                n_visible: 1,
                distinct: BoundDistinct::None,
            }
        }

        fn query(body: BoundSetExpr) -> BoundQuery {
            BoundQuery {
                ctes: vec![],
                body,
                order_by: vec![],
                limit: None,
                offset: None,
                columns: vec![],
            }
        }

        fn q_select(rtable: Vec<Rte>, filter: Option<BoundExpr>) -> BoundQuery {
            query(BoundSetExpr::Select(Box::new(select(rtable, filter))))
        }

        fn exists(q: BoundQuery) -> BoundExpr {
            Expr::new(
                ExprKind::SubLink {
                    kind: SubLinkKind::Exists,
                    test: None,
                    query: Box::new(q),
                },
                SqlType::BOOL,
                Span::default(),
            )
        }

        fn stmt(q: BoundQuery) -> BoundStatement {
            BoundStatement::Select(Box::new(q))
        }

        #[test]
        fn single_table_and_no_from() {
            assert_eq!(count_rtes(&stmt(q_select(vec![base()], None))), 1);
            assert_eq!(count_rtes(&stmt(q_select(vec![], None))), 0);
        }

        #[test]
        fn join_counts_the_join_rte() {
            let join = rte(RteKind::Join {
                kind: JoinType::Inner,
                left: RteId(0),
                right: RteId(1),
                sources: vec![],
            });
            assert_eq!(
                count_rtes(&stmt(q_select(vec![base(), base(), join], None))),
                3
            );
        }

        #[test]
        fn pulled_up_subquery_counts_both() {
            // SELECT * FROM (SELECT a, b FROM t WHERE b > 5) s  → s と t で 2。
            let sub = rte(RteKind::Subquery {
                query: Box::new(q_select(vec![base()], None)),
            });
            assert_eq!(count_rtes(&stmt(q_select(vec![sub], None))), 2);
        }

        #[test]
        fn sublinks_in_expressions_are_counted() {
            let f = exists(q_select(vec![base()], None));
            assert_eq!(count_rtes(&stmt(q_select(vec![base()], Some(f)))), 2);
            // 入れ子の副問い合わせ。
            let inner = exists(q_select(vec![base()], None));
            let outer = exists(q_select(vec![base()], Some(inner)));
            assert_eq!(count_rtes(&stmt(q_select(vec![base()], Some(outer)))), 3);
        }

        #[test]
        fn set_operations_count_each_arm() {
            let arm = || Box::new(q_select(vec![base()], None));
            let union = query(BoundSetExpr::SetOp {
                op: SetOpKind::Union,
                all: false,
                left: arm(),
                right: arm(),
                left_coerce: None,
                right_coerce: None,
                types: vec![],
            });
            // 各腕: `*SELECT* n` の 1 + 表の 1。
            assert_eq!(count_rtes(&stmt(union.clone())), 4);
            // 3 つ並んだ UNION（入れ子の集合演算の腕には足さない）。
            let three = query(BoundSetExpr::SetOp {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(union),
                right: arm(),
                left_coerce: None,
                right_coerce: None,
                types: vec![],
            });
            assert_eq!(count_rtes(&stmt(three)), 6);
        }

        #[test]
        fn values_and_ctes() {
            let values = |rows: usize| {
                query(BoundSetExpr::Values {
                    rows: (0..rows).map(|_| vec![int(1)]).collect(),
                    types: vec![SqlType::INT4],
                })
            };
            assert_eq!(count_rtes(&stmt(values(2))), 1);
            let mut q = q_select(vec![base()], None);
            q.ctes.push(crate::analyzer::query::BoundCte {
                name: "c".into(),
                query: q_select(vec![base()], None),
                materialize: crate::analyzer::query::CteMaterialize::Default,
                col_aliases: vec![],
            });
            assert_eq!(count_rtes(&stmt(q)), 2);
        }

        #[test]
        fn insert_counts_the_target_and_the_source() {
            let insert = |source: BoundQuery| {
                BoundStatement::Insert(BoundInsert {
                    table: table(),
                    source: Box::new(source),
                    coercions: None,
                    column_map: vec![],
                    defaults: vec![],
                    checks: vec![],
                    overriding: None,
                    returning: None,
                })
            };
            let values = |rows: usize| {
                query(BoundSetExpr::Values {
                    rows: (0..rows).map(|_| vec![int(1)]).collect(),
                    types: vec![SqlType::INT4],
                })
            };
            // 1 行の VALUES は Rte を足さない。複数行は `*VALUES*`。SELECT は `*SELECT*` + 本体。
            assert_eq!(count_rtes(&insert(values(1))), 1);
            assert_eq!(count_rtes(&insert(values(3))), 2);
            assert_eq!(count_rtes(&insert(q_select(vec![base()], None))), 3);
        }

        #[test]
        fn explain_counts_its_statement_and_utilities_have_none() {
            use crate::analyzer::query::{BoundExplain, ExplainOptions};
            let e = BoundStatement::Explain(Box::new(BoundExplain {
                options: ExplainOptions::default(),
                inner: stmt(q_select(vec![base(), base()], None)),
            }));
            assert_eq!(count_rtes(&e), 2);
            assert_eq!(count_rtes(&BoundStatement::Checkpoint), 0);
        }
    }
    #[test]
    fn widths() {
        assert_eq!(type_width(SqlType::BOOL), 1);
        assert_eq!(type_width(SqlType::INT2), 2);
        assert_eq!(type_width(SqlType::INT4), 4);
        assert_eq!(type_width(SqlType::INT8), 8);
        assert_eq!(type_width(SqlType::NUMERIC), 32);
        assert_eq!(type_width(SqlType::TEXT), 32);
        assert_eq!(type_width(SqlType::varchar(10)), 10);
        assert_eq!(type_width(SqlType::varchar(32)), 32);
        assert_eq!(type_width(SqlType::varchar(100)), 32 + (100 - 32) / 2);
        assert_eq!(type_width(SqlType::varchar(5000)), 32 + (1000 - 32) / 2);
        assert_eq!(row_width([SqlType::INT4, SqlType::TEXT]), 36);
    }
}
