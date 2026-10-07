//! `pg_get_expr` / `pg_get_constraintdef` / `pg_get_indexdef`（保存テキストを parse → analyze → deparse。
//! `m4/10-explain-copy-compat.md` §4.8）。
//!
//! 11 §7.1 の C-30: `deparse` の中でここだけが `analyzer` に依存する（00 §4.1 の例外 2）。
//! 保存形式は変えない（00 D-21）: `pg_attrdef.adbin` / `pg_constraint.conbin` には SQL のテキストが入っている。
//! 分析の段（[`analyze_stored`]）だけが analyzer の API に触れる（新しい `Var` 版の `*_bound` を使う）。

use std::sync::Arc;

use super::ident::{quote_identifier, quote_qualified};
use super::{ColText, ColumnNamer, DeparseCtx, DeparseOptions, deparse_expr};
use crate::analyzer::query::BoundQuery;
use crate::catalog::opclass::{default_opclass, opclass_by_oid};
use crate::catalog::{CatalogReader, CheckDef, ConstraintKind, IndexDef, SystemColumn, TableDef};
use crate::error::{Error, Result};
use crate::expr::{Expr, SYSTEM_COL_BASE, Var, system_col_from_index};
use crate::types::{Oid, TypeEnv};

/// 保存された式を analyzer に通した結果（`Var { rte: 0 }` で表の列を参照する）。
pub type StoredExpr = Expr<Var, Box<BoundQuery>>;

/// 保存された式の種類。解析の文脈（代入キャスト・`bool` への変換）を決める。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoredKind {
    /// 列の既定値（`attnum - 1` の位置の列の型への代入キャスト）。
    Default(usize),
    /// CHECK 制約（`bool`）。
    Check,
}

/// `Var { rte: 0, col, levels_up: 0 }` を列名にする（`quote_identifier` を通す。システム列も名前で）。
#[derive(Debug)]
pub struct TableNamer<'a> {
    pub table: Option<&'a TableDef>,
}

impl ColumnNamer<Var> for TableNamer<'_> {
    fn name(&self, v: &Var) -> Result<ColText> {
        let text = if v.col >= SYSTEM_COL_BASE {
            let sc = system_col_from_index(v.col - SYSTEM_COL_BASE)
                .ok_or_else(|| Error::internal(format!("bad system column {}", v.col)))?;
            system_column_name(sc).to_owned()
        } else {
            let col = self
                .table
                .and_then(|t| t.columns.get(usize::from(v.col)))
                .ok_or_else(|| Error::internal(format!("column {} is out of range", v.col)))?;
            quote_identifier(&col.name)
        };
        Ok(ColText { text, wrap: false })
    }
}

fn system_column_name(sc: SystemColumn) -> &'static str {
    match sc {
        SystemColumn::Ctid => "ctid",
        SystemColumn::Xmin => "xmin",
        SystemColumn::Cmin => "cmin",
        SystemColumn::Xmax => "xmax",
        SystemColumn::Cmax => "cmax",
        SystemColumn::TableOid => "tableoid",
    }
}

/// 解析済みの式を保存式の書式にする（`DeparseMode::Stored`）。
pub fn deparse_stored(
    e: &StoredExpr,
    table: Option<&TableDef>,
    pretty: bool,
    catalog: &dyn CatalogReader,
    type_env: &TypeEnv<'_>,
) -> Result<String> {
    let namer = TableNamer { table };
    let cx = DeparseCtx::<Var, Box<BoundQuery>> {
        opts: DeparseOptions::stored(pretty),
        namer: &namer,
        sublinks: None,
        type_env,
        catalog,
    };
    deparse_expr(e, &cx)
}

/// 保存テキスト → 解析 → 保存式の書式。`table` は列を持つ式（`None` は列なし）。
pub fn deparse_stored_text(
    src: &str,
    kind: StoredKind,
    table: Option<&Arc<TableDef>>,
    pretty: bool,
    catalog: &dyn CatalogReader,
    type_env: &TypeEnv<'_>,
) -> Result<String> {
    let e = analyze_stored(src, kind, table, catalog)?;
    deparse_stored(&e, table.map(|t| &**t), pretty, catalog, type_env)
}

/// `pg_get_expr(src, relid [, pretty])`。`relid` が 0 でなく表がなければ `None`（SQL の NULL）。
///
/// 種類は、表の既定値・CHECK のテキストと一致するもので決める（一致しなければ `bool` として解析する）。
pub fn pg_get_expr(
    src: &str,
    relid: Oid,
    pretty: bool,
    catalog: &dyn CatalogReader,
    type_env: &TypeEnv<'_>,
) -> Result<Option<String>> {
    let table = if relid == 0 {
        None
    } else {
        match catalog.table_by_oid(relid)? {
            Some(t) => Some(t),
            None => return Ok(None),
        }
    };
    let kind = table
        .as_ref()
        .and_then(|t| {
            t.columns
                .iter()
                .position(|c| c.default.as_ref().is_some_and(|d| d.expr_sql == src))
                .map(StoredKind::Default)
        })
        .unwrap_or(StoredKind::Check);
    deparse_stored_text(src, kind, table.as_ref(), pretty, catalog, type_env).map(Some)
}

/// `pg_get_constraintdef(oid [, pretty])`。制約がなければ `None`。
///
/// CHECK は `CHECK (` + 式 + `)`、PRIMARY KEY / UNIQUE は `PRIMARY KEY (a, b)` / `UNIQUE (a)`。
pub fn pg_get_constraintdef(
    oid: Oid,
    pretty: bool,
    catalog: &dyn CatalogReader,
    type_env: &TypeEnv<'_>,
) -> Result<Option<String>> {
    let Some(c) = catalog.constraint_by_oid(oid)? else {
        return Ok(None);
    };
    let Some(table) = catalog.table_by_oid(c.table_oid)? else {
        return Ok(None);
    };
    Ok(Some(match c.kind {
        ConstraintKind::Check => {
            let src = c
                .check_sql
                .as_deref()
                .ok_or_else(|| Error::internal("CHECK constraint without an expression"))?;
            let body = deparse_stored_text(
                src,
                StoredKind::Check,
                Some(&table),
                pretty,
                catalog,
                type_env,
            )?;
            let tail = if c.no_inherit { " NO INHERIT" } else { "" };
            format!("CHECK ({body}){tail}")
        }
        ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
            let cols = attnum_names(&table, c.columns.iter().copied())?;
            let head = if c.kind == ConstraintKind::PrimaryKey {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            format!("{head} ({})", cols.join(", "))
        }
    }))
}

fn attnum_names(table: &TableDef, attnums: impl Iterator<Item = i16>) -> Result<Vec<String>> {
    attnums
        .map(|n| {
            table
                .columns
                .iter()
                .find(|c| c.attnum == n)
                .map(|c| quote_identifier(&c.name))
                .ok_or_else(|| Error::internal(format!("attribute {n} not found")))
        })
        .collect()
}

/// `pg_get_indexdef(oid [, column [, pretty]])`。`column = 0` は全体、1 以上はその列の名前だけ。
/// 索引がなければ `None`。
pub fn pg_get_indexdef(
    oid: Oid,
    column: i32,
    pretty: bool,
    catalog: &dyn CatalogReader,
) -> Result<Option<String>> {
    let Some(index) = catalog.index_by_oid(oid)? else {
        return Ok(None);
    };
    let Some(table) = catalog.table_by_oid(index.table_oid)? else {
        return Ok(None);
    };
    if column > 0 {
        let Some(c) = usize::try_from(column - 1)
            .ok()
            .and_then(|i| index.columns.get(i))
        else {
            return Ok(None);
        };
        return Ok(Some(
            attnum_names(&table, std::iter::once(c.attnum))?.remove(0),
        ));
    }
    let rel = if pretty {
        match catalog.relation_name(table.oid)? {
            Some(n) => n,
            None => quote_qualified(&table.schema, &table.name),
        }
    } else {
        quote_qualified(&table.schema, &table.name)
    };
    let cols = index_column_defs(&index, &table)?;
    Ok(Some(format!(
        "CREATE {}INDEX {} ON {rel} USING btree ({})",
        if index.unique { "UNIQUE " } else { "" },
        quote_identifier(&index.name),
        cols.join(", ")
    )))
}

fn index_column_defs(index: &IndexDef, table: &TableDef) -> Result<Vec<String>> {
    index
        .columns
        .iter()
        .map(|ic| {
            let col = table
                .columns
                .iter()
                .find(|c| c.attnum == ic.attnum)
                .ok_or_else(|| Error::internal(format!("attribute {} not found", ic.attnum)))?;
            let mut s = quote_identifier(&col.name);
            let default_class = default_opclass(col.ty.oid).map(|c| c.oid);
            if Some(ic.opclass) != default_class
                && let Some(oc) = opclass_by_oid(ic.opclass)
            {
                s.push(' ');
                s.push_str(&quote_identifier(oc.name));
            }
            if ic.descending {
                s.push_str(" DESC");
            }
            // 既定: ASC は NULLS LAST、DESC は NULLS FIRST。
            if ic.nulls_first != ic.descending {
                s.push_str(if ic.nulls_first {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
            Ok(s)
        })
        .collect()
}

/// 保存テキストを解析する（analyzer への唯一の入口）。
fn analyze_stored(
    src: &str,
    kind: StoredKind,
    table: Option<&Arc<TableDef>>,
    catalog: &dyn CatalogReader,
) -> Result<StoredExpr> {
    if let (StoredKind::Default(i), Some(t)) = (kind, table) {
        let col = t
            .columns
            .get(i)
            .ok_or_else(|| Error::internal("default column out of range"))?;
        return crate::analyzer::analyze_column_default_bound(catalog, col)?
            .ok_or_else(|| Error::internal("column without a default"));
    }
    let mut def = table.map_or_else(empty_table, |t| (**t).clone());
    def.checks = vec![CheckDef {
        name: "pg_get_expr".into(),
        expr_sql: src.to_owned(),
        no_inherit: false,
    }];
    crate::analyzer::analyze_table_checks_bound(catalog, &def)?
        .into_iter()
        .next()
        .map(|c| c.expr)
        .ok_or_else(|| Error::internal("no check expression"))
}

fn empty_table() -> TableDef {
    TableDef {
        oid: 0,
        namespace: 0,
        schema: String::new(),
        name: String::new(),
        kind: crate::catalog::RelKind::Table,
        locator: crate::storage::smgr::RelFileLocator {
            spc_oid: 0,
            db_oid: 0,
            rel_number: crate::storage::smgr::RelFileNumber(0),
        },
        columns: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        sequence: None,
        identity_seqs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::types::{SqlType, oid};
    use yuzhu_datetime::{DateTimeEnv, TimeZone, ZoneDb};

    fn with_env<R>(f: impl FnOnce(&TypeEnv<'_>) -> R) -> R {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        let env = TypeEnv {
            datetime: Some(DateTimeEnv::new(&tz, &zones)),
            ..TypeEnv::default()
        };
        f(&env)
    }

    fn numeric_10_2() -> SqlType {
        SqlType::new(oid::NUMERIC, ((10 << 16) | 2) + 4)
    }

    /// §4.9 の表の `chk` 表。
    fn chk_table(cat: &mut FakeCatalog) -> Arc<TableDef> {
        cat.add(
            &TableBuilder::new("chk")
                .column("i", SqlType::INT4)
                .column("bi", SqlType::INT8)
                .column("t", SqlType::TEXT)
                .column("v", SqlType::varchar(10))
                .column("c", SqlType::new(oid::BPCHAR, 7))
                .column("b", SqlType::BOOL)
                .column("n", numeric_10_2())
                .column("f", SqlType::FLOAT8)
                .column("d", SqlType::DATE)
                .column("ts", SqlType::TIMESTAMP)
                .column("tz", SqlType::TIMESTAMPTZ),
        )
    }

    fn check(cat: &FakeCatalog, t: &Arc<TableDef>, src: &str, pretty: bool) -> Result<String> {
        with_env(|env| deparse_stored_text(src, StoredKind::Check, Some(t), pretty, cat, env))
    }

    /// §4.9 の CHECK の表（非 pretty、pretty）を、解析器を通して確かめる。`datetime` は日時の行だけ／それ以外だけ。
    fn check_rows_through_the_analyzer(datetime: bool) {
        let mut cat = FakeCatalog::new("db");
        let t = chk_table(&mut cat);
        #[rustfmt::skip]
        let rows: &[(&str, &str, &str)] = &[
            ("i > 0", "(i > 0)", "i > 0"),
            ("i + 1 + 2 > 0", "(((i + 1) + 2) > 0)", "(i + 1 + 2) > 0"),
            ("i + 1 * 2 > 3", "((i + (1 * 2)) > 3)", "(i + 1 * 2) > 3"),
            ("(i + 1) * 2 > 3", "(((i + 1) * 2) > 3)", "((i + 1) * 2) > 3"),
            ("i - (1 - 2) > 0", "((i - (1 - 2)) > 0)", "(i - (1 - 2)) > 0"),
            ("i - 1 - 2 > 0", "(((i - 1) - 2) > 0)", "(i - 1 - 2) > 0"),
            ("-i < 0", "((- i) < 0)", "(- i) < 0"),
            ("t = 'abc'", "(t = 'abc'::text)", "t = 'abc'::text"),
            ("v = 'x'", "((v)::text = 'x'::text)", "v::text = 'x'::text"),
            ("t || 'x' = 'yx'", "((t || 'x'::text) = 'yx'::text)", "(t || 'x'::text) = 'yx'::text"),
            ("lower(t) = 'abc'", "(lower(t) = 'abc'::text)", "lower(t) = 'abc'::text"),
            ("lower(t || 'a') = 'x'", "(lower((t || 'a'::text)) = 'x'::text)", "lower(t || 'a'::text) = 'x'::text"),
            ("t LIKE 'a%'", "(t ~~ 'a%'::text)", "t ~~ 'a%'::text"),
            ("t NOT ILIKE 'a%'", "(t !~~* 'a%'::text)", "t !~~* 'a%'::text"),
            ("t LIKE 'a!%' ESCAPE '!'", "(t ~~ like_escape('a!%'::text, '!'::text))", "t ~~ like_escape('a!%'::text, '!'::text)"),
            ("i IN (1,2,3)", "(i = ANY (ARRAY[1, 2, 3]))", "i = ANY (ARRAY[1, 2, 3])"),
            ("i NOT IN (1,2,3)", "(i <> ALL (ARRAY[1, 2, 3]))", "i <> ALL (ARRAY[1, 2, 3])"),
            ("i IN (1)", "(i = 1)", "i = 1"),
            ("i IN (1, NULL)", "(i = ANY (ARRAY[1, NULL::integer]))", "i = ANY (ARRAY[1, NULL::integer])"),
            ("i BETWEEN 1 AND 5", "((i >= 1) AND (i <= 5))", "i >= 1 AND i <= 5"),
            ("i NOT BETWEEN 1 AND 5", "((i < 1) OR (i > 5))", "i < 1 OR i > 5"),
            ("t IS NULL", "(t IS NULL)", "t IS NULL"),
            ("t IS NOT NULL", "(t IS NOT NULL)", "t IS NOT NULL"),
            ("b IS NOT TRUE", "(b IS NOT TRUE)", "b IS NOT TRUE"),
            ("b", "b", "b"),
            ("NOT b", "(NOT b)", "NOT b"),
            ("b AND i > 1", "(b AND (i > 1))", "b AND i > 1"),
            ("(b AND i > 1) OR i < 0", "((b AND (i > 1)) OR (i < 0))", "b AND i > 1 OR i < 0"),
            ("b AND (i > 1 OR i < 0)", "(b AND ((i > 1) OR (i < 0)))", "b AND (i > 1 OR i < 0)"),
            ("NOT (b AND i > 1)", "(NOT (b AND (i > 1)))", "NOT (b AND i > 1)"),
            ("NOT (i > 1)", "(NOT (i > 1))", "NOT i > 1"),
            ("NOT b AND b", "((NOT b) AND b)", "NOT b AND b"),
            ("NOT (NOT b)", "(NOT (NOT b))", "NOT (NOT b)"),
            ("(b OR b) AND (b OR b)", "((b OR b) AND (b OR b))", "(b OR b) AND (b OR b)"),
            ("(i + 1) IS NULL", "((i + 1) IS NULL)", "(i + 1) IS NULL"),
            ("(i + 1)::text IS NULL", "(((i + 1))::text IS NULL)", "((i + 1)::text) IS NULL"),
            ("COALESCE(b AND b, true) = b", "(COALESCE((b AND b), true) = b)", "COALESCE(b AND b, true) = b"),
            ("NULLIF(i, 0) = 1", "(NULLIF(i, 0) = 1)", "NULLIF(i, 0) = 1"),
            ("i::bigint > 1", "((i)::bigint > 1)", "i::bigint > 1"),
            ("(i + 1)::bigint > 0", "(((i + 1))::bigint > 0)", "(i + 1)::bigint > 0"),
            ("i::bigint::text = t", "(((i)::bigint)::text = t)", "i::bigint::text = t"),
            ("i::numeric > 1.5", "((i)::numeric > 1.5)", "i::numeric > 1.5"),
            ("n = 1.50", "(n = 1.50)", "n = 1.50"),
            ("f > 'Infinity'", "(f > 'Infinity'::double precision)", "f > 'Infinity'::double precision"),
            ("i > -1", "(i > '-1'::integer)", "i > '-1'::integer"),
            ("d > '2020-01-01'", "(d > '2020-01-01'::date)", "d > '2020-01-01'::date"),
            ("ts > '2020-01-01 10:00'", "(ts > '2020-01-01 10:00:00'::timestamp without time zone)", "ts > '2020-01-01 10:00:00'::timestamp without time zone"),
            ("tz > current_timestamp", "(tz > CURRENT_TIMESTAMP)", "tz > CURRENT_TIMESTAMP"),
            ("d > current_date", "(d > CURRENT_DATE)", "d > CURRENT_DATE"),
            ("ts::date = d", "((ts)::date = d)", "ts::date = d"),
            ("current_user = t", "(CURRENT_USER = t)", "CURRENT_USER = t"),
            ("current_schema() = t", "(\"current_schema\"() = t)", "\"current_schema\"() = t"),
            ("t = ''", "(t = ''::text)", "t = ''::text"),
            ("t = 'it''s'", "(t = 'it''s'::text)", "t = 'it''s'::text"),
            ("t = 'a\\b'", "(t = 'a\\b'::text)", "t = 'a\\b'::text"),
            ("case when i > 1 then 'a' else 'b' end = t", "(\nCASE\n    WHEN (i > 1) THEN 'a'::text\n    ELSE 'b'::text\nEND = t)", "\nCASE\n    WHEN i > 1 THEN 'a'::text\n    ELSE 'b'::text\nEND = t"),
            ("case when i > 1 then 1 end = 1", "(\nCASE\n    WHEN (i > 1) THEN 1\n    ELSE NULL::integer\nEND = 1)", "\nCASE\n    WHEN i > 1 THEN 1\n    ELSE NULL::integer\nEND = 1"),
        ];
        let mut failures = Vec::new();
        for (sql, plain, pretty) in rows {
            let is_datetime =
                sql.starts_with("d ") || sql.starts_with("ts") || sql.starts_with("tz");
            if is_datetime != datetime {
                continue;
            }
            for (want, p) in [(plain, false), (pretty, true)] {
                match check(&cat, &t, sql, p) {
                    Ok(got) if got == *want => {}
                    Ok(got) => failures.push(format!(
                        "[{}] {sql}\n  want {want:?}\n  got  {got:?}",
                        if p { "pretty" } else { "plain" }
                    )),
                    Err(e) => failures.push(format!(
                        "[{}] {sql}: error {}",
                        if p { "pretty" } else { "plain" },
                        e.message
                    )),
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn check_table_through_the_analyzer() {
        check_rows_through_the_analyzer(false);
    }

    #[test]
    #[ignore = "date / timestamp / CURRENT_* are not analyzed yet (T2)"]
    fn check_table_datetime_rows_through_the_analyzer() {
        check_rows_through_the_analyzer(true);
    }

    #[test]
    fn defaults_hide_the_root_implicit_cast() {
        let mut cat = FakeCatalog::new("db");
        let t = cat.add(
            &TableBuilder::new("dd")
                .column("x", SqlType::INT8)
                .default_sql("x", "1")
                .column("w", SqlType::varchar(5))
                .default_sql("w", "'ab'")
                .column("z", SqlType::TEXT)
                .default_sql("z", "'a'")
                .column("h", SqlType::INT4)
                .default_sql("h", "-1")
                .column("i", SqlType::INT4)
                .default_sql("i", "1+2")
                .column("n", SqlType::TEXT)
                .default_sql("n", "'a'||'b'")
                .column("p", SqlType::INT8)
                .default_sql("p", "(1)::bigint"),
        );
        let get = |name: &str, pretty: bool| {
            let src = t
                .column(name)
                .unwrap()
                .default
                .as_ref()
                .unwrap()
                .expr_sql
                .clone();
            with_env(|env| pg_get_expr(&src, t.oid, pretty, &cat, env))
                .unwrap()
                .unwrap()
        };
        assert_eq!(get("x", false), "1");
        assert_eq!(get("w", false), "'ab'::character varying");
        assert_eq!(get("z", false), "'a'::text");
        assert_eq!(get("h", false), "'-1'::integer");
        assert_eq!(get("i", false), "(1 + 2)");
        assert_eq!(get("i", true), "1 + 2");
        assert_eq!(get("n", false), "('a'::text || 'b'::text)");
        assert_eq!(get("n", true), "'a'::text || 'b'::text");
        assert_eq!(get("p", false), "(1)::bigint");
        assert_eq!(get("p", true), "1::bigint");
    }

    #[test]
    fn pg_get_expr_returns_null_for_a_missing_table() {
        let cat = FakeCatalog::new("db");
        let r = with_env(|env| pg_get_expr("1", 99_999, false, &cat, env)).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn pg_get_expr_without_a_relation_has_no_columns() {
        let cat = FakeCatalog::new("db");
        let r = with_env(|env| pg_get_expr("true", 0, false, &cat, env)).unwrap();
        assert_eq!(r.as_deref(), Some("true"));
        // 列を参照すると解析のエラー。
        assert!(with_env(|env| pg_get_expr("a > 0", 0, false, &cat, env)).is_err());
    }

    #[test]
    fn column_names_are_quoted_and_system_columns_use_their_names() {
        let t = TableDef {
            columns: vec![crate::catalog::ColumnDef {
                name: "Mixed Case".into(),
                attnum: 1,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            }],
            ..(*{
                let mut cat = FakeCatalog::new("db");
                cat.add(&TableBuilder::new("q").column("a", SqlType::INT4))
            })
            .clone()
        };
        let namer = TableNamer { table: Some(&t) };
        let v = Var::user(crate::expr::RteId(0), 0);
        assert_eq!(namer.name(&v).unwrap().text, "\"Mixed Case\"");
        let ctid = Var::system(crate::expr::RteId(0), SystemColumn::Ctid);
        assert_eq!(namer.name(&ctid).unwrap().text, "ctid");
        assert!(namer.name(&Var::user(crate::expr::RteId(0), 5)).is_err());
    }

    #[test]
    fn constraint_and_index_definitions() {
        let mut cat = FakeCatalog::new("db");
        let t = cat.add(
            &TableBuilder::new("tt")
                .column("id", SqlType::INT4)
                .column("name", SqlType::TEXT)
                .column("n", SqlType::NUMERIC)
                .primary_key(&["id"])
                .unique(&["name"])
                .index_ordered(
                    Some("tt_multi"),
                    &[("n", true, true), ("name", false, false)],
                    false,
                )
                .check("tt_n_check", "n > 0"),
        );
        let find_index = |name: &str| t.indexes.iter().find(|i| i.name == name).unwrap().clone();
        let pk = find_index("tt_pkey");
        let get = |oid: Oid, col: i32, pretty: bool| {
            pg_get_indexdef(oid, col, pretty, &cat).unwrap().unwrap()
        };
        assert_eq!(
            get(pk.oid, 0, false),
            "CREATE UNIQUE INDEX tt_pkey ON public.tt USING btree (id)"
        );
        assert_eq!(get(pk.oid, 1, false), "id");
        assert_eq!(pg_get_indexdef(pk.oid, 2, false, &cat).unwrap(), None);
        let multi = find_index("tt_multi");
        // DESC は NULLS FIRST が既定、ASC は NULLS LAST が既定。
        assert_eq!(
            get(multi.oid, 0, false),
            "CREATE INDEX tt_multi ON public.tt USING btree (n DESC, name)"
        );
        assert_eq!(pg_get_indexdef(99_999, 0, false, &cat).unwrap(), None);
        // 制約。
        with_env(|env| {
            let pk_con = pk.constraint.clone().unwrap();
            assert_eq!(
                pg_get_constraintdef(pk_con.oid, false, &cat, env)
                    .unwrap()
                    .as_deref(),
                Some("PRIMARY KEY (id)")
            );
            let uq = find_index("tt_name_key").constraint.clone().unwrap();
            assert_eq!(
                pg_get_constraintdef(uq.oid, true, &cat, env)
                    .unwrap()
                    .as_deref(),
                Some("UNIQUE (name)")
            );
            assert_eq!(
                pg_get_constraintdef(99_999, false, &cat, env).unwrap(),
                None
            );
        });
    }
}
