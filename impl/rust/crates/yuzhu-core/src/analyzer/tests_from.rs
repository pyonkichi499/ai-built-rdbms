//! FROM 句・JOIN・名前解決・DML のアナライザのテスト（N1。`m4/03` §6.2）。
//!
//! SQL 文字列 → パーサ → アナライザ。ヘルパは `tests.rs` の `run` / `create` / `query`。

#![allow(clippy::many_single_char_names, clippy::items_after_statements)]

use super::bound::{
    BoundExpr, BoundExprKind, BoundQuery, BoundSelect, BoundSetExpr, BoundStatement, FromItem,
    JoinColSource, JoinType, RteKind, UpdateSource,
};
use super::tests::{catalog, create, query, run};
use crate::catalog::SystemColumn;
use crate::catalog::fake::FakeCatalog as MemoryCatalog;
use crate::error::{Error, sqlstate};
use crate::expr::{RteId, Var};
use crate::types::{SqlType, oid};

fn cat() -> MemoryCatalog {
    let mut c = catalog();
    for sql in [
        "CREATE TABLE u (a int, e text, c int)",
        "CREATE TABLE w (a int, b text, c int)",
        "CREATE TABLE p (id int, x int)",
        "CREATE TABLE m1 (k smallint)",
        "CREATE TABLE m2 (k bigint)",
        "CREATE TABLE mv (k varchar(3))",
        "CREATE TABLE mt (k text)",
        "CREATE TABLE r (id int, v int)",
    ] {
        create(&mut c, sql);
    }
    create(&mut c, "CREATE TABLE a2 (a int, b int)");
    c
}

#[track_caller]
fn err_full(c: &MemoryCatalog, sql: &str) -> Error {
    match run(c, sql) {
        Ok(s) => panic!("expected an error for {sql}, got {s:?}"),
        Err(e) => e,
    }
}

#[track_caller]
fn state(c: &MemoryCatalog, sql: &str) -> &'static str {
    err_full(c, sql).sqlstate.code()
}

#[track_caller]
fn select_body(q: &BoundQuery) -> &BoundSelect {
    match &q.body {
        BoundSetExpr::Select(s) => s,
        other => panic!("not a select body: {other:?}"),
    }
}

#[track_caller]
fn var_of(e: &BoundExpr) -> Var {
    match &e.kind {
        BoundExprKind::Column(v) => *v,
        other => panic!("not a column: {other:?}"),
    }
}

fn user(rte: u16, col: u16) -> Var {
    Var::user(RteId(rte), col)
}

/// 対象列（`targets[i]`）の `Var`。
#[track_caller]
fn target_var(q: &BoundQuery, i: usize) -> Var {
    var_of(&select_body(q).targets[i])
}

fn col_names(q: &BoundQuery) -> Vec<&str> {
    q.columns.iter().map(|c| c.name.as_str()).collect()
}

#[track_caller]
fn join_rte(s: &BoundSelect, i: usize) -> (JoinType, &[JoinColSource]) {
    match &s.rtable[i].kind {
        RteKind::Join { kind, sources, .. } => (*kind, sources),
        other => panic!("not a join: {other:?}"),
    }
}

// ---- RTE と RteId ------------------------------------------------------------------------

#[test]
fn join_rtes_are_post_order() {
    let c = cat();
    let q = query(&c, "SELECT * FROM t JOIN u ON t.a = u.a");
    let s = select_body(&q);
    assert_eq!(s.rtable.len(), 3);
    assert!(matches!(s.rtable[0].kind, RteKind::Table { .. }));
    assert!(matches!(s.rtable[1].kind, RteKind::Table { .. }));
    assert!(matches!(s.rtable[2].kind, RteKind::Join { .. }));
    assert_eq!(s.from.len(), 1);
    let FromItem::Join {
        rte,
        kind,
        left,
        right,
        on,
    } = &s.from[0]
    else {
        panic!("not a join item");
    };
    assert_eq!(*rte, RteId(2));
    assert_eq!(*kind, JoinType::Inner);
    assert!(matches!(**left, FromItem::Scan(RteId(0))));
    assert!(matches!(**right, FromItem::Scan(RteId(1))));
    assert!(on.is_some());
    // 3 つの結合: ((t join u) join p)。RteId は t=0, u=1, 結合=2, p=3, 結合=4
    let q = query(
        &c,
        "SELECT * FROM t JOIN u ON t.a = u.a JOIN p ON p.id = t.a",
    );
    let s = select_body(&q);
    assert_eq!(s.rtable.len(), 5);
    assert!(matches!(s.rtable[3].kind, RteKind::Table { .. }));
    assert!(matches!(s.rtable[4].kind, RteKind::Join { .. }));
    // 右側の結合: t JOIN (u JOIN p)
    let q = query(
        &c,
        "SELECT * FROM t JOIN (u JOIN p ON p.id = u.a) ON t.a = p.id",
    );
    let s = select_body(&q);
    assert_eq!(s.rtable.len(), 5);
    assert!(matches!(s.rtable[3].kind, RteKind::Join { .. }));
    assert!(matches!(s.rtable[4].kind, RteKind::Join { .. }));
    q_validate(&q);
}

fn q_validate(q: &BoundQuery) {
    q.validate().unwrap_or_else(|e| panic!("{e:?}"));
}

#[test]
fn comma_items_are_separate_from_items() {
    let c = cat();
    let q = query(&c, "SELECT * FROM t, u");
    let s = select_body(&q);
    assert_eq!(s.from.len(), 2);
    assert_eq!(s.rtable.len(), 2);
    let q = query(&c, "SELECT * FROM t, u JOIN p ON p.id = u.a");
    let s = select_body(&q);
    assert_eq!(s.from.len(), 2);
    assert!(matches!(s.from[1], FromItem::Join { .. }));
    assert_eq!(s.rtable.len(), 4);
    q_validate(&q);
}

#[test]
fn derived_table_and_values_rtes() {
    let c = cat();
    let q = query(&c, "SELECT * FROM (SELECT a, b FROM t) s");
    let s = select_body(&q);
    let RteKind::Subquery { query: inner } = &s.rtable[0].kind else {
        panic!("not a subquery rte");
    };
    assert_eq!(inner.columns.len(), 2);
    assert_eq!(s.rtable[0].refname.as_deref(), Some("s"));
    let names: Vec<&str> = s.rtable[0]
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["a", "b"]);
    // 別名なし（PG16 以降）
    let q = query(&c, "SELECT a FROM (SELECT a FROM t)");
    assert_eq!(select_body(&q).rtable[0].refname, None);
    // VALUES は RteKind::Values、ORDER BY つきは Subquery
    let q = query(&c, "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) v");
    let s = select_body(&q);
    let RteKind::Values { rows } = &s.rtable[0].kind else {
        panic!("not a values rte: {:?}", s.rtable[0].kind);
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(col_names(&q), ["column1", "column2"]);
    assert_eq!(s.rtable[0].columns[1].ty, SqlType::TEXT);
    let q = query(&c, "SELECT * FROM (VALUES (1), (2) ORDER BY 1) v");
    assert!(matches!(
        select_body(&q).rtable[0].kind,
        RteKind::Subquery { .. }
    ));
    q_validate(&q);
}

#[test]
fn derived_table_column_aliases() {
    let c = cat();
    let q = query(&c, "SELECT * FROM (SELECT a, b, c FROM t) s (x, y)");
    assert_eq!(col_names(&q), ["x", "y", "c"]);
    let q = query(&c, "SELECT x, y FROM (VALUES (1, 2)) v (x, y)");
    assert_eq!(col_names(&q), ["x", "y"]);
    let e = err_full(&c, "SELECT * FROM (SELECT 1) s (a, b)");
    assert_eq!(e.sqlstate, sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        e.message,
        "table \"s\" has 1 columns available but 2 columns specified"
    );
    let e = err_full(&c, "SELECT * FROM (VALUES (1, 2)) v (a, b, c)");
    assert_eq!(e.sqlstate, sqlstate::INVALID_COLUMN_REFERENCE);
    let q = query(&c, "SELECT * FROM t AS x (q)");
    assert_eq!(col_names(&q)[..2], ["q", "b"]);
    assert_eq!(
        state(&c, "SELECT * FROM t AS x (q1, q2, q3, q4, q5, q6, q7, q8)"),
        "42P10"
    );
}

#[test]
fn nested_derived_tables_and_joins() {
    let c = cat();
    let q = query(
        &c,
        "SELECT s.a FROM (SELECT * FROM (SELECT a FROM t) i) s JOIN u ON u.a = s.a",
    );
    q_validate(&q);
    assert_eq!(q.columns[0].name, "a");
}

#[test]
fn unnamed_derived_tables_do_not_conflict() {
    let c = cat();
    let q = query(&c, "SELECT * FROM (SELECT 1 a), (SELECT 2 b)");
    assert_eq!(select_body(&q).from.len(), 2);
    assert_eq!(col_names(&q), ["a", "b"]);
}

// ---- 列参照 ------------------------------------------------------------------------------

#[test]
fn column_references_over_several_items() {
    let c = cat();
    let q = query(&c, "SELECT t.a, u.e, p.x FROM t, u, p");
    assert_eq!(target_var(&q, 0), user(0, 0));
    assert_eq!(target_var(&q, 1), user(1, 1));
    assert_eq!(target_var(&q, 2), user(2, 1));
    // 修飾なしで 1 つの表にしかない列
    let q = query(&c, "SELECT e, x FROM t, u, p");
    assert_eq!(target_var(&q, 0), user(1, 1));
    assert_eq!(target_var(&q, 1), user(2, 1));
    // スキーマつき（別名なしの表）
    let q = query(&c, "SELECT public.t.a FROM t");
    assert_eq!(target_var(&q, 0), user(0, 0));
    // 4 部名: 現在のデータベースなら通り、違えば 0A000
    let q = query(&c, "SELECT postgres.public.t.a FROM t");
    assert_eq!(target_var(&q, 0), user(0, 0));
    let e = err_full(&c, "SELECT other.public.t.a FROM t");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "cross-database references are not implemented: other.public.t.a"
    );
    assert_eq!(state(&c, "SELECT a.b.c.d.e FROM t"), "42601");
}

#[test]
fn ambiguity_and_alias_errors() {
    let c = cat();
    let e = err_full(&c, "SELECT a FROM t, u");
    assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
    assert_eq!(e.message, "column reference \"a\" is ambiguous");
    assert_eq!(e.cursor_byte, Some(7));
    // 同じ RTE に同名の列が 2 つ
    let e = err_full(&c, "SELECT s.a FROM (SELECT 1 a, 2 a) s");
    assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
    // 別名で隠された名前
    let e = err_full(&c, "SELECT t.a FROM t AS t1");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        e.message,
        "invalid reference to FROM-clause entry for table \"t\""
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("Perhaps you meant to reference the table alias \"t1\".")
    );
    assert_eq!(e.cursor_byte, Some(7));
    let e = err_full(&c, "SELECT public.t.a FROM t AS t1");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert!(e.hint.is_some());
    // 存在しない修飾
    let e = err_full(&c, "SELECT x.a FROM t");
    assert_eq!(e.message, "missing FROM-clause entry for table \"x\"");
    assert_eq!(e.cursor_byte, Some(7));
    // 列がない
    let e = err_full(&c, "SELECT zz FROM t");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(e.message, "column \"zz\" does not exist");
    assert_eq!(e.cursor_byte, Some(7));
    let e = err_full(&c, "SELECT t.zz FROM t");
    assert_eq!(e.message, "column t.zz does not exist");
    assert_eq!(e.cursor_byte, Some(7));
    // 重複した別名
    for sql in [
        "SELECT * FROM t, t",
        "SELECT * FROM t x, u x",
        "SELECT * FROM t JOIN t USING (a)",
        "SELECT * FROM generate_series(1, 2), generate_series(1, 2)",
        "SELECT * FROM generate_series(1, 2) g, (SELECT 1) AS g",
    ] {
        let e = err_full(&c, sql);
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_ALIAS, "{sql}");
        assert!(e.cursor_byte.is_none(), "{sql}");
    }
    let e = err_full(&c, "SELECT * FROM t, t");
    assert_eq!(e.message, "table name \"t\" specified more than once");
    // 別名があれば同じ表を 2 回
    run(&c, "SELECT * FROM t x, t y").unwrap();
    // FROM がなければ 3 部名・表名は引けない
    assert_eq!(state(&c, "SELECT * FROM a.b.c.d"), "42601");
}

#[test]
fn whole_row_reference_is_not_supported() {
    let c = cat();
    let e = err_full(&c, "SELECT t FROM t");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "whole-row reference to \"t\" is not supported yet"
    );
}

#[test]
fn system_columns() {
    let c = cat();
    let q = query(&c, "SELECT ctid, xmin FROM t");
    assert_eq!(target_var(&q, 0), Var::system(RteId(0), SystemColumn::Ctid));
    assert_eq!(target_var(&q, 1), Var::system(RteId(0), SystemColumn::Xmin));
    assert_eq!(q.columns[0].attnum, -1);
    // 修飾すれば表ごとに選べる。修飾なしは曖昧
    let q = query(&c, "SELECT u.ctid FROM t, u");
    assert_eq!(target_var(&q, 0), Var::system(RteId(1), SystemColumn::Ctid));
    assert_eq!(state(&c, "SELECT ctid FROM t, u"), "42702");
    // ctid を持つ表が 1 つだけなら通る
    let q = query(&c, "SELECT ctid FROM t, (SELECT 1 a) s");
    assert_eq!(target_var(&q, 0), Var::system(RteId(0), SystemColumn::Ctid));
    // 派生表・VALUES・関数にはない
    for sql in [
        "SELECT ctid FROM (SELECT * FROM t) q",
        "SELECT ctid FROM (VALUES (1)) q",
        "SELECT ctid FROM generate_series(1, 2)",
    ] {
        assert_eq!(state(&c, sql), "42703", "{sql}");
    }
    // 結合の子の表: 修飾すれば使え、修飾なしは見えない
    let q = query(&c, "SELECT t.ctid FROM t JOIN p ON t.a = p.id");
    assert_eq!(target_var(&q, 0), Var::system(RteId(0), SystemColumn::Ctid));
    let e = err_full(&c, "SELECT ctid FROM t JOIN p ON t.a = p.id");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(e.message, "column \"ctid\" does not exist");
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "There are columns named \"ctid\", but they are in tables that cannot be referenced from this part of the query."
        )
    );
    assert_eq!(e.hint.as_deref(), Some("Try using a table-qualified name."));
}

// ---- 参照できない位置の表（LATERAL） ---------------------------------------------------------

#[test]
fn derived_table_cannot_see_left_items() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM t, (SELECT t.a) s");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        e.message,
        "invalid reference to FROM-clause entry for table \"t\""
    );
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "There is an entry for table \"t\", but it cannot be referenced from this part of the query."
        )
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("To reference that table, you must mark this subquery with LATERAL.")
    );
    let e = err_full(&c, "SELECT * FROM t, (SELECT b) s");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(e.message, "column \"b\" does not exist");
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "There is a column named \"b\" in table \"t\", but it cannot be referenced from this part of the query."
        )
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("To reference that column, you must mark this subquery with LATERAL.")
    );
    // JOIN の右辺の派生表から左: INNER / LEFT は HINT つき、RIGHT / FULL は DETAIL だけ
    let e = err_full(&c, "SELECT * FROM t JOIN (SELECT t.a) s ON true");
    assert!(e.hint.is_some());
    let e = err_full(&c, "SELECT * FROM t RIGHT JOIN (SELECT t.a) s ON true");
    assert!(e.detail.is_some());
    assert!(e.hint.is_none());
}

#[test]
fn join_on_sees_only_its_own_items() {
    let c = cat();
    // `u` と `p` だけが見える
    run(&c, "SELECT * FROM t, u JOIN p ON p.id = u.a").unwrap();
    let e = err_full(&c, "SELECT * FROM t, u JOIN p ON p.id = t.a");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        e.message,
        "invalid reference to FROM-clause entry for table \"t\""
    );
    assert!(e.detail.is_some());
    assert!(e.hint.is_none());
    // ON から 3 つ目の表は見えない
    let e = err_full(&c, "SELECT * FROM t JOIN u ON t.a = p.id JOIN p ON true");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
}

#[test]
fn function_arguments_cannot_reference_left_items() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM t, generate_series(1, t.a)");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "implicit LATERAL reference in a function in FROM is not supported yet"
    );
    let e = err_full(&c, "SELECT * FROM t, generate_series(1, a)");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    // 外側のスコープでも左の項目でもない列は通常の 42703
    assert_eq!(state(&c, "SELECT * FROM generate_series(1, zz)"), "42703");
}

// ---- `*` -----------------------------------------------------------------------------------

#[test]
fn star_expansion() {
    let c = cat();
    let q = query(&c, "SELECT * FROM u");
    assert_eq!(col_names(&q), ["a", "e", "c"]);
    let q = query(&c, "SELECT u.*, t.a FROM t, u");
    assert_eq!(col_names(&q)[..4], ["a", "e", "c", "a"]);
    // FROM なし
    let e = err_full(&c, "SELECT *");
    assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
    assert_eq!(e.message, "SELECT * with no tables specified is not valid");
    let e = err_full(&c, "SELECT x.* FROM t");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(e.message, "missing FROM-clause entry for table \"x\"");
    // 別名つきの列リスト
    let q = query(&c, "SELECT * FROM u AS x (q, r)");
    assert_eq!(col_names(&q), ["q", "r", "c"]);
    // 同名の出力列が 2 つあっても * は両方出す
    let q = query(&c, "SELECT * FROM (SELECT 1 a, 2 a) s");
    assert_eq!(col_names(&q), ["a", "a"]);
}

#[test]
fn star_over_joins_puts_merged_columns_first() {
    let c = cat();
    // t(a, b, c, ...) join u(a, e, c) using (a): a, b, c, ..., e, c
    let q = query(&c, "SELECT * FROM t JOIN u USING (a)");
    let names = col_names(&q);
    assert_eq!(names[0], "a");
    assert_eq!(names.iter().filter(|n| **n == "a").count(), 1);
    assert_eq!(names[1], "b");
    assert!(names.contains(&"e"));
    // t.* は USING の列も含む全列、u.* も
    let q = query(&c, "SELECT t.*, u.* FROM t JOIN u USING (a)");
    assert_eq!(q.columns.len(), 7 + 3);
    q_validate(&q);
}

// ---- JOIN ---------------------------------------------------------------------------------

#[test]
fn join_kinds_and_on() {
    let c = cat();
    for (sql, kind) in [
        ("SELECT * FROM t JOIN u ON t.a = u.a", JoinType::Inner),
        ("SELECT * FROM t INNER JOIN u ON t.a = u.a", JoinType::Inner),
        ("SELECT * FROM t LEFT JOIN u ON t.a = u.a", JoinType::Left),
        ("SELECT * FROM t RIGHT JOIN u ON t.a = u.a", JoinType::Right),
        ("SELECT * FROM t FULL JOIN u ON t.a = u.a", JoinType::Full),
        ("SELECT * FROM t CROSS JOIN u", JoinType::Cross),
    ] {
        let q = query(&c, sql);
        let s = select_body(&q);
        let FromItem::Join { kind: k, on, .. } = &s.from[0] else {
            panic!("{sql}");
        };
        assert_eq!(*k, kind, "{sql}");
        assert_eq!(on.is_some(), kind != JoinType::Cross, "{sql}");
        assert_eq!(join_rte(s, 2).0, kind, "{sql}");
        // ON 結合と CROSS は左の全列 ++ 右の全列
        assert_eq!(s.rtable[2].columns.len(), 7 + 3, "{sql}");
        q_validate(&q);
    }
}

#[test]
fn join_on_must_be_boolean_and_cannot_contain_aggregates() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM t JOIN u ON t.a");
    assert_eq!(e.sqlstate, sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        e.message,
        "argument of JOIN/ON must be type boolean, not type integer"
    );
    assert_eq!(e.cursor_byte, Some(26));
    // 集約は N2 の担当だが、少なくとも通らない
    assert!(run(&c, "SELECT * FROM t JOIN u ON count(*) > 1").is_err());
}

#[test]
fn using_merges_columns_and_expands_them() {
    let c = cat();
    // INNER: 併合列は左の列（t.a）と同じ式
    let q = query(&c, "SELECT a, t.a, u.a FROM t JOIN u USING (a)");
    assert_eq!(target_var(&q, 0), user(0, 0));
    assert_eq!(target_var(&q, 1), user(0, 0));
    assert_eq!(target_var(&q, 2), user(1, 0));
    let s = select_body(&q);
    let (kind, sources) = join_rte(s, 2);
    assert_eq!(kind, JoinType::Inner);
    assert_eq!(sources[0], JoinColSource::Left(0));
    // on は等号（元の左右の型で解決）
    let FromItem::Join { on: Some(on), .. } = &s.from[0] else {
        panic!("no on");
    };
    let BoundExprKind::Operator { op, args } = &on.kind else {
        panic!("on is not an operator: {on:?}");
    };
    assert_eq!(op.name, "=");
    assert_eq!(var_of(&args[0]), user(0, 0));
    assert_eq!(var_of(&args[1]), user(1, 0));
    // 併合列は先頭、左の残り、右の残り
    let names: Vec<&str> = s.rtable[2]
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names[0], "a");
    assert_eq!(&names[names.len() - 2..], ["e", "c"]);
    assert_eq!(sources[1], JoinColSource::Left(1));
    q_validate(&q);
}

#[test]
fn using_with_left_right_and_full() {
    let c = cat();
    let q = query(&c, "SELECT a FROM t LEFT JOIN u USING (a)");
    assert_eq!(target_var(&q, 0), user(0, 0));
    assert_eq!(join_rte(select_body(&q), 2).1[0], JoinColSource::Left(0));
    let q = query(&c, "SELECT a FROM t RIGHT JOIN u USING (a)");
    assert_eq!(target_var(&q, 0), user(1, 0));
    assert_eq!(join_rte(select_body(&q), 2).1[0], JoinColSource::Right(0));
    // FULL は COALESCE(t.a, u.a)。t.a と u.a は元の列
    let q = query(&c, "SELECT a, t.a, u.a FROM t FULL JOIN u USING (a)");
    let s = select_body(&q);
    let BoundExprKind::Coalesce(parts) = &s.targets[0].kind else {
        panic!("not coalesce: {:?}", s.targets[0]);
    };
    assert_eq!(var_of(&parts[0]), user(0, 0));
    assert_eq!(var_of(&parts[1]), user(1, 0));
    assert_eq!(s.targets[0].ty, SqlType::INT4);
    assert_eq!(var_of(&s.targets[1]), user(0, 0));
    assert_eq!(join_rte(s, 2).1[0], JoinColSource::Coalesce(0, 0));
    // 由来: 併合列が COALESCE なら由来なし、INNER なら元の表の列
    assert_eq!(q.columns[0].table_oid, 0);
    assert_ne!(q.columns[1].table_oid, 0);
    let q = query(&c, "SELECT a FROM t JOIN u USING (a)");
    assert_ne!(q.columns[0].table_oid, 0);
    assert_eq!(q.columns[0].attnum, 1);
    q_validate(&q);
}

#[test]
fn using_merged_column_types() {
    let c = cat();
    // smallint と bigint: 併合列は bigint。INNER は型が合う右の列を選ぶ（C-25）
    let q = query(&c, "SELECT k, m1.k, m2.k FROM m1 JOIN m2 USING (k)");
    let s = select_body(&q);
    assert_eq!(s.targets[0].ty, SqlType::INT8);
    assert_eq!(var_of(&s.targets[0]), user(1, 0));
    assert_eq!(s.targets[1].ty, SqlType::INT2);
    assert_eq!(s.targets[2].ty, SqlType::INT8);
    assert_eq!(join_rte(s, 2).1[0], JoinColSource::Right(0));
    assert_eq!(s.rtable[2].columns[0].ty, SqlType::INT8);
    // 逆順: bigint が左なら Left
    let q = query(&c, "SELECT k FROM m2 JOIN m1 USING (k)");
    assert_eq!(join_rte(select_body(&q), 2).1[0], JoinColSource::Left(0));
    // LEFT でも併合列は共通型（左の列にキャストを挟む）
    let q = query(&c, "SELECT k FROM m1 LEFT JOIN m2 USING (k)");
    let s = select_body(&q);
    assert_eq!(s.targets[0].ty, SqlType::INT8);
    assert!(matches!(s.targets[0].kind, BoundExprKind::Cast { .. }));
    assert_eq!(join_rte(s, 2).1[0], JoinColSource::Left(0));
    // FULL: COALESCE(CAST(m1.k), m2.k)
    let q = query(&c, "SELECT k FROM m1 FULL JOIN m2 USING (k)");
    assert_eq!(select_body(&q).targets[0].ty, SqlType::INT8);
    // varchar と text: 左の型が残る（PG 17 の実機。両方向に暗黙キャストがあるので最初の型のまま）。
    // varchar(3) と varchar(3) は typmod も保つ
    let q = query(&c, "SELECT k FROM mv JOIN mt USING (k)");
    assert_eq!(select_body(&q).targets[0].ty.oid, oid::VARCHAR);
    let q = query(&c, "SELECT k FROM mt JOIN mv USING (k)");
    assert_eq!(select_body(&q).targets[0].ty.oid, oid::TEXT);
    let q = query(&c, "SELECT k FROM mv a JOIN mv b USING (k)");
    assert_eq!(
        select_body(&q).targets[0].ty,
        select_body(&q).rtable[0].columns[0].ty
    );
    q_validate(&q);
}

#[test]
fn natural_join() {
    let c = cat();
    // 共通名（a と c）がすべて USING になる。左の列の順
    let q = query(&c, "SELECT * FROM t NATURAL JOIN u");
    let s = select_body(&q);
    let names: Vec<&str> = s.rtable[2]
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names[..2], ["a", "c"]);
    let FromItem::Join { on: Some(on), .. } = &s.from[0] else {
        panic!("no on");
    };
    assert!(matches!(on.kind, BoundExprKind::And(ref v) if v.len() == 2));
    // 共通列がなければ直積（エラーにならない）。on は None
    let q = query(&c, "SELECT * FROM p NATURAL JOIN u");
    let s = select_body(&q);
    let FromItem::Join { on, .. } = &s.from[0] else {
        panic!("not a join");
    };
    assert!(on.is_none());
    assert_eq!(s.rtable[2].columns.len(), 5);
    // 連鎖: 直前の結合の列に対して共通列を見る
    let q = query(&c, "SELECT * FROM t NATURAL JOIN u NATURAL JOIN w");
    q_validate(&q);
    // NATURAL LEFT
    let q = query(&c, "SELECT * FROM t NATURAL LEFT JOIN u");
    assert_eq!(join_rte(select_body(&q), 2).0, JoinType::Left);
}

#[test]
fn using_chain_and_multiple_columns() {
    let c = cat();
    // 複数の列: 指定順に先頭へ
    let q = query(&c, "SELECT * FROM t JOIN u USING (c, a)");
    let names = col_names(&q);
    assert_eq!(names[..2], ["c", "a"]);
    // 連鎖: 2 つ目の USING の左は結合。a は 1 つに見える
    let q = query(&c, "SELECT a FROM t JOIN u USING (a) JOIN w USING (a)");
    q_validate(&q);
    assert_eq!(q.columns[0].name, "a");
    // 結合の左に同名の列が 2 つ残っている名前（c）
    let e = err_full(&c, "SELECT * FROM t JOIN u USING (a) JOIN w USING (c)");
    assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
    assert_eq!(
        e.message,
        "common column name \"c\" appears more than once in left table"
    );
}

#[test]
fn using_errors() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM t JOIN u USING (a, a)");
    assert_eq!(e.sqlstate, sqlstate::DUPLICATE_COLUMN);
    assert_eq!(
        e.message,
        "column name \"a\" appears more than once in USING clause"
    );
    let e = err_full(&c, "SELECT * FROM t JOIN u USING (e)");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(
        e.message,
        "column \"e\" specified in USING clause does not exist in left table"
    );
    let e = err_full(&c, "SELECT * FROM t JOIN u USING (b)");
    assert_eq!(
        e.message,
        "column \"b\" specified in USING clause does not exist in right table"
    );
    // 一方に同名が 2 つ（派生表の中）
    let e = err_full(&c, "SELECT * FROM (SELECT 1 a, 2 a) s JOIN u USING (a)");
    assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
    assert_eq!(
        e.message,
        "common column name \"a\" appears more than once in left table"
    );
    let e = err_full(&c, "SELECT * FROM u JOIN (SELECT 1 a, 2 a) s USING (a)");
    assert!(e.message.ends_with("in right table"));
    // = が解決できない型の組
    let e = err_full(&c, "SELECT * FROM m1 JOIN mt USING (k)");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(e.message, "operator does not exist: smallint = text");
    assert!(e.cursor_byte.is_none());
}

#[test]
fn qualified_names_through_joins() {
    let c = cat();
    // WHERE / ORDER BY / GROUP BY で併合列を修飾なしで使える
    run(
        &c,
        "SELECT a FROM t JOIN u USING (a) WHERE a > 1 ORDER BY a",
    )
    .unwrap();
    // 結合の両側にある列（c）は修飾なしだと曖昧
    assert_eq!(state(&c, "SELECT c FROM t JOIN u ON t.a = u.a"), "42702");
    // 修飾すれば区別できる
    let q = query(&c, "SELECT t.c, u.c FROM t JOIN u ON t.a = u.a");
    assert_eq!(target_var(&q, 0), user(0, 2));
    assert_eq!(target_var(&q, 1), user(1, 2));
    // 1 つにしかない列は修飾なしで使える
    let q = query(&c, "SELECT e FROM t JOIN u ON t.a = u.a");
    assert_eq!(target_var(&q, 0), user(1, 1));
}

#[test]
fn join_vars_are_never_left_in_bound_output() {
    let c = cat();
    for sql in [
        "SELECT a, b FROM t JOIN u USING (a)",
        "SELECT * FROM t FULL JOIN u USING (a, c)",
        "SELECT * FROM (t JOIN u USING (a)) JOIN w USING (a)",
        "SELECT a FROM t JOIN u USING (a) WHERE a = 1",
    ] {
        let q = query(&c, sql);
        q_validate(&q);
        let s = select_body(&q);
        let mut check = |e: &BoundExpr| {
            for v in e.columns() {
                assert!(
                    !matches!(s.rtable[usize::from(v.rte.0)].kind, RteKind::Join { .. }),
                    "{sql}: {v:?}"
                );
            }
        };
        s.targets.iter().for_each(&mut check);
        s.filter.iter().for_each(&mut check);
    }
}

// ---- 派生表・関数・VALUES ------------------------------------------------------------------

#[test]
fn generate_series_rte_and_names() {
    let c = cat();
    let q = query(&c, "SELECT * FROM generate_series(1, 3)");
    let s = select_body(&q);
    let RteKind::Function { call } = &s.rtable[0].kind else {
        panic!("not a function rte");
    };
    assert!(matches!(call.kind, BoundExprKind::Function { .. }));
    assert_eq!(s.rtable[0].columns[0].ty, SqlType::INT4);
    assert_eq!(col_names(&q), ["generate_series"]);
    // 別名が列名にもなる（pgbench -i の `from generate_series(1, N) as aid`）
    let q = query(&c, "SELECT aid FROM generate_series(1, 3) AS aid");
    assert_eq!(col_names(&q), ["aid"]);
    let q = query(&c, "SELECT g FROM generate_series(1, 3) g");
    assert_eq!(col_names(&q), ["g"]);
    // 列別名
    let q = query(&c, "SELECT x FROM generate_series(1, 3) AS g(x)");
    assert_eq!(col_names(&q), ["x"]);
    let e = err_full(&c, "SELECT * FROM generate_series(1, 3) AS g(x, y)");
    assert_eq!(e.sqlstate, sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        e.message,
        "table \"g\" has 1 columns available but 2 columns specified"
    );
    // 関数名の列が使える
    run(&c, "SELECT generate_series FROM generate_series(1, 3)").unwrap();
    // 引数の暗黙のキャスト: bigint 版
    let q = query(&c, "SELECT * FROM generate_series(1, 3::bigint)");
    assert_eq!(select_body(&q).rtable[0].columns[0].ty, SqlType::INT8);
    // step 付き、副問い合わせの引数は通る
    run(&c, "SELECT * FROM generate_series(1, 10, 2)").unwrap();
    run(&c, "SELECT * FROM generate_series(1, (SELECT 2))").unwrap();
    q_validate(&q);
}

#[test]
fn generate_series_errors() {
    let c = cat();
    // 引数の個数・関数名・スキーマ
    for sql in [
        "SELECT * FROM generate_series(1)",
        "SELECT * FROM generate_series()",
        "SELECT * FROM generate_series(1, 2, 3, 4)",
        "SELECT * FROM nosuchfunc(1)",
    ] {
        let e = err_full(&c, sql);
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FUNCTION, "{sql}");
        assert_eq!(e.cursor_byte, Some(14), "{sql}");
    }
    let e = err_full(&c, "SELECT * FROM public.generate_series(1, 2)");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        e.message,
        "function public.generate_series(integer, integer) does not exist"
    );
    assert_eq!(
        state(&c, "SELECT * FROM nosuch.generate_series(1, 2)"),
        "3F000"
    );
    run(&c, "SELECT * FROM pg_catalog.generate_series(1, 2)").unwrap();
    // 候補が曖昧
    assert_eq!(
        state(
            &c,
            "SELECT * FROM generate_series(1::smallint, 3::smallint)"
        ),
        "42725"
    );
    // 集合返却でない関数
    let e = err_full(&c, "SELECT * FROM lower('A')");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(e.message, "function \"lower\" in FROM is not supported yet");
    // FROM 句以外では呼べない
    let e = err_full(&c, "SELECT generate_series(1, 3)");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "set-returning functions are only supported in the FROM clause"
    );
    assert_eq!(
        state(&c, "SELECT * FROM t WHERE generate_series(1, 3) = 1"),
        "0A000"
    );
}

#[test]
fn values_in_from_errors() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM (VALUES (1), (2, 3)) v");
    assert_eq!(e.message, "VALUES lists must all be the same length");
    assert_eq!(state(&c, "SELECT * FROM (VALUES (1), ('a')) v"), "22P02");
    // 型は列ごとの共通型
    let q = query(&c, "SELECT * FROM (VALUES (1, 'x'), (2.5, 'y')) v");
    assert_eq!(select_body(&q).rtable[0].columns[0].ty, SqlType::NUMERIC);
}

#[test]
fn table_relation_kinds() {
    let c = cat();
    let e = err_full(&c, "SELECT * FROM nosuch");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(e.message, "relation \"nosuch\" does not exist");
    assert_eq!(e.cursor_byte, Some(14));
    let e = err_full(&c, "SELECT * FROM s.nosuch");
    assert_eq!(e.message, "relation \"s.nosuch\" does not exist");
    let e = err_full(&c, "SELECT * FROM other.public.t");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(state(&c, "SELECT * FROM a.b.c.d"), "42601");
}

#[test]
fn derived_table_inside_derived_table_sees_only_outer_scopes() {
    let c = cat();
    // 外側の問い合わせの列は、派生表の中の副問い合わせから levels_up で参照できる
    let q = query(&c, "SELECT (SELECT s.a) FROM (SELECT a FROM t) s");
    q_validate(&q);
}

// ---- 出力列の由来 -----------------------------------------------------------------------------

#[test]
fn output_column_origins() {
    let c = cat();
    let q = query(&c, "SELECT a, a + 1, 1 FROM t");
    assert_ne!(q.columns[0].table_oid, 0);
    assert_eq!(q.columns[0].attnum, 1);
    assert_eq!(q.columns[1].table_oid, 0);
    assert_eq!(q.columns[2].table_oid, 0);
    // 派生表を通した列は元の表の列
    let q = query(&c, "SELECT x, y FROM (SELECT b AS x, a + 1 AS y FROM t) s");
    assert_ne!(q.columns[0].table_oid, 0);
    assert_eq!(q.columns[0].attnum, 2);
    assert_eq!(q.columns[1].table_oid, 0);
    // 結合の列
    let q = query(&c, "SELECT e FROM t JOIN u ON t.a = u.a");
    assert_eq!(q.columns[0].attnum, 2);
    // VALUES・関数にはない
    let q = query(&c, "SELECT column1 FROM (VALUES (1)) v");
    assert_eq!(q.columns[0].table_oid, 0);
    let q = query(&c, "SELECT * FROM generate_series(1, 2)");
    assert_eq!(q.columns[0].table_oid, 0);
    // システム列は負の attnum
    let q = query(&c, "SELECT t.ctid FROM t JOIN u ON true");
    assert_eq!(q.columns[0].attnum, -1);
}

// ---- COLLATE / OPERATOR ----------------------------------------------------------------------

#[test]
fn collate_expressions() {
    let c = cat();
    for ok in [
        "SELECT 'a' COLLATE \"C\"",
        "SELECT 'a' COLLATE \"POSIX\"",
        "SELECT 'a' COLLATE \"default\"",
        "SELECT 'a' COLLATE pg_catalog.\"default\"",
        "SELECT b COLLATE \"C\" FROM t",
        "SELECT v COLLATE \"C\" FROM t",
        "SELECT b FROM t ORDER BY b COLLATE \"C\"",
        "SELECT b COLLATE \"C\" = 'x' FROM t",
    ] {
        run(&c, ok).unwrap_or_else(|e| panic!("{ok}: {e:?}"));
    }
    // 式をそのまま返す（型も変えない）
    let q = query(&c, "SELECT 'a' COLLATE \"C\"");
    assert_eq!(select_body(&q).targets[0].ty.oid, oid::TEXT);
    let e = err_full(&c, "SELECT 'a' COLLATE nosuch");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
    assert_eq!(
        e.message,
        "collation \"nosuch\" for encoding \"UTF8\" does not exist"
    );
    assert_eq!(e.cursor_byte, Some(11));
    let e = err_full(&c, "SELECT 'a' COLLATE pg_catalog.nosuch");
    assert_eq!(
        e.message,
        "collation \"pg_catalog.nosuch\" for encoding \"UTF8\" does not exist"
    );
    let e = err_full(&c, "SELECT 'a' COLLATE public.\"C\"");
    assert_eq!(
        e.message,
        "collation \"public.C\" for encoding \"UTF8\" does not exist"
    );
    assert_eq!(state(&c, "SELECT 'a' COLLATE nosuch.\"C\""), "3F000");
    let e = err_full(&c, "SELECT 1 COLLATE \"C\"");
    assert_eq!(e.sqlstate, sqlstate::DATATYPE_MISMATCH);
    assert_eq!(e.message, "collations are not supported by type integer");
    assert_eq!(state(&c, "SELECT a COLLATE \"C\" FROM t"), "42804");
    assert_eq!(state(&c, "SELECT 'a' COLLATE \"en_US\""), "42704");
    // 出力列名は内側の式の名前
    let q = query(&c, "SELECT b COLLATE \"C\" FROM t");
    assert_eq!(col_names(&q), ["b"]);
}

#[test]
fn qualified_operators() {
    let c = cat();
    let q = query(&c, "SELECT 1 OPERATOR(pg_catalog.+) 2");
    assert!(matches!(
        select_body(&q).targets[0].kind,
        BoundExprKind::Operator { .. }
    ));
    run(&c, "SELECT a FROM t WHERE a OPERATOR(pg_catalog.=) 1").unwrap();
    run(&c, "SELECT OPERATOR(pg_catalog.-) 1").unwrap();
    let e = err_full(&c, "SELECT 1 OPERATOR(nosuch.+) 2");
    assert_eq!(e.sqlstate, sqlstate::INVALID_SCHEMA_NAME);
    assert_eq!(e.message, "schema \"nosuch\" does not exist");
    let e = err_full(&c, "SELECT 1 OPERATOR(public.+) 2");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        e.message,
        "operator does not exist: integer public.+ integer"
    );
}

// ---- DML -------------------------------------------------------------------------------------

fn update(c: &MemoryCatalog, sql: &str) -> super::bound::BoundUpdate {
    match run(c, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        BoundStatement::Update(u) => u,
        other => panic!("not an update: {other:?}"),
    }
}

fn delete(c: &MemoryCatalog, sql: &str) -> super::bound::BoundDelete {
    match run(c, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        BoundStatement::Delete(d) => d,
        other => panic!("not a delete: {other:?}"),
    }
}

#[test]
fn update_from_rtable_layout() {
    let c = cat();
    let u = update(&c, "UPDATE r SET v = t.a * 100 FROM t WHERE t.a = r.id");
    assert_eq!(u.rtable.len(), 2);
    assert!(matches!(u.rtable[0].kind, RteKind::Table { .. }));
    assert_eq!(u.rtable[0].refname.as_deref(), Some("r"));
    assert_eq!(u.rtable[1].refname.as_deref(), Some("t"));
    assert_eq!(u.from.len(), 1);
    assert!(matches!(u.from[0], FromItem::Scan(RteId(1))));
    // SET の右辺は FROM の列を参照できる
    let (idx, UpdateSource::Expr(e)) = &u.assignments[0] else {
        panic!("not an expression assignment");
    };
    assert_eq!(*idx, 1);
    assert!(e.columns().iter().any(|v| v.rte == RteId(1)));
    let st = run(&c, "UPDATE r SET v = t.a FROM t WHERE t.a = r.id").unwrap();
    st.validate().unwrap();
    // JOIN も使える
    let u = update(
        &c,
        "UPDATE r SET v = 1 FROM t JOIN u ON u.a = t.a WHERE t.a = r.id",
    );
    assert_eq!(u.rtable.len(), 4);
    assert_eq!(u.from.len(), 1);
    run(
        &c,
        "UPDATE r SET v = 1 FROM t JOIN u ON u.a = t.a WHERE t.a = r.id",
    )
    .unwrap()
    .validate()
    .unwrap();
}

#[test]
fn update_from_name_rules() {
    let c = cat();
    // 同じ表を 2 回
    let e = err_full(&c, "UPDATE r SET v = 1 FROM r");
    assert_eq!(e.sqlstate, sqlstate::DUPLICATE_ALIAS);
    assert_eq!(e.message, "table name \"r\" specified more than once");
    run(&c, "UPDATE r SET v = 1 FROM r AS r2 WHERE r2.id = r.id").unwrap();
    // 対象表に別名があれば元の名前では参照できない
    let e = err_full(&c, "UPDATE r AS x SET v = 1 WHERE r.id = 1");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        e.hint.as_deref(),
        Some("Perhaps you meant to reference the table alias \"x\".")
    );
    // FROM 句の中から対象表は見えない
    let e = err_full(&c, "UPDATE r SET v = 1 FROM (SELECT r.id) s");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        e.message,
        "invalid reference to FROM-clause entry for table \"r\""
    );
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "There is an entry for table \"r\", but it cannot be referenced from this part of the query."
        )
    );
    assert!(e.hint.is_none());
    let e = err_full(&c, "UPDATE r SET v = 1 FROM (SELECT id) s");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "There is a column named \"id\" in table \"r\", but it cannot be referenced from this part of the query."
        )
    );
    // 修飾なしの列が曖昧
    assert_eq!(state(&c, "UPDATE t SET a = 1 FROM u WHERE c = 1"), "42702");
    // システム列: FROM の表にもあるので修飾なしは曖昧、FROM がなければ通る
    assert_eq!(
        state(&c, "UPDATE r SET v = 1 FROM t WHERE ctid = '(0,1)'"),
        "42702"
    );
    run(&c, "UPDATE r SET v = 1 WHERE ctid = '(0,1)'").unwrap();
    let u = update(&c, "UPDATE r SET v = 1 WHERE ctid = '(0,1)'");
    assert!(
        u.filter
            .as_ref()
            .unwrap()
            .columns()
            .contains(&Var::system(RteId(0), SystemColumn::Ctid))
    );
}

#[test]
fn update_analysis_order_follows_postgres() {
    let c = cat();
    // WHERE が SET より先
    let e = err_full(&c, "UPDATE r SET v = zz WHERE yy = 1");
    assert_eq!(e.message, "column \"yy\" does not exist");
    // FROM が WHERE より先
    let e = err_full(&c, "UPDATE r SET v = 1 FROM nosuch WHERE yy = 1");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    // SET の列がない
    let e = err_full(&c, "UPDATE r SET nosuch = 1");
    assert_eq!(
        e.message,
        "column \"nosuch\" of relation \"r\" does not exist"
    );
    let e = err_full(&c, "UPDATE r SET r.v = 1");
    assert_eq!(e.message, "column \"r\" of relation \"r\" does not exist");
    assert_eq!(
        e.hint.as_deref(),
        Some("SET target columns cannot be qualified with the relation name.")
    );
    assert_eq!(state(&c, "UPDATE r SET v = 1, v = 2"), "42601");
    assert_eq!(state(&c, "UPDATE r SET ctid = '(0,1)'"), "0A000");
    assert_eq!(state(&c, "UPDATE r SET v = 'x'::text"), "42804");
}

#[test]
fn delete_using() {
    let c = cat();
    let d = delete(&c, "DELETE FROM r USING t WHERE t.a = r.id");
    assert_eq!(d.rtable.len(), 2);
    assert_eq!(d.from.len(), 1);
    assert!(d.filter.is_some());
    let st = run(
        &c,
        "DELETE FROM r USING t, u WHERE t.a = r.id AND u.a = t.a",
    )
    .unwrap();
    st.validate().unwrap();
    assert_eq!(state(&c, "DELETE FROM r USING r"), "42712");
    run(&c, "DELETE FROM r USING r AS r2 WHERE r2.id = r.id").unwrap();
    assert_eq!(state(&c, "DELETE FROM r USING nosuch"), "42P01");
    let e = err_full(&c, "DELETE FROM r USING (SELECT r.id) s");
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
    // USING が WHERE より先に解析される
    assert_eq!(
        state(&c, "DELETE FROM r USING nosuch WHERE zz = 1"),
        "42P01"
    );
}

#[test]
fn returning_is_disabled_until_enabled() {
    let c = cat();
    for sql in [
        "INSERT INTO r VALUES (1, 2) RETURNING id",
        "UPDATE r SET v = 1 RETURNING v",
        "DELETE FROM r RETURNING *",
    ] {
        let e = err_full(&c, sql);
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED, "{sql}");
        assert_eq!(e.message, "RETURNING is not supported yet", "{sql}");
        assert!(e.cursor_byte.is_some(), "{sql}");
    }
    // 無効の間は、対象表のエラーより先に出る
    let e = err_full(&c, "UPDATE nosuch SET v = 1 RETURNING v");
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
}

fn parse_stmt(sql: &str) -> crate::sql::ast::Statement {
    let mut v = crate::sql::parse(sql).unwrap();
    assert_eq!(v.len(), 1);
    v.remove(0)
}

fn with_returning<T>(
    c: &MemoryCatalog,
    sql: &str,
    f: impl FnOnce(&super::Analyzer<'_>, crate::sql::ast::Statement) -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let a = super::Analyzer { catalog: c };
    f(&a, parse_stmt(sql))
}

#[test]
fn returning_when_enabled() {
    use crate::sql::ast::Statement as S;
    let c = cat();
    let upd = |sql: &str| {
        with_returning(&c, sql, |a, st| {
            let S::Update(u) = st else { panic!() };
            a.analyze_update_with(&u, true)
        })
    };
    let del = |sql: &str| {
        with_returning(&c, sql, |a, st| {
            let S::Delete(d) = st else { panic!() };
            a.analyze_delete_with(&d, true)
        })
    };
    let ins = |sql: &str| {
        with_returning(&c, sql, |a, st| {
            let S::Insert(i) = st else { panic!() };
            a.analyze_insert_with(&i, true)
        })
    };
    // 対象表の列・式・別名・`*`
    let u = upd("UPDATE r SET v = 1 RETURNING v, id + 1 AS n, *").unwrap();
    let ret = u.returning.as_ref().unwrap();
    assert_eq!(ret.targets.len(), 4);
    let names: Vec<&str> = ret.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["v", "n", "id", "v"]);
    assert_eq!(ret.columns[0].attnum, 2);
    assert_eq!(ret.columns[1].table_oid, 0);
    assert!(
        ret.targets
            .iter()
            .all(|t| t.columns().iter().all(|v| v.rte == RteId(0)))
    );
    // システム列
    let d = del("DELETE FROM r RETURNING ctid, r.*").unwrap();
    let ret = d.returning.as_ref().unwrap();
    assert_eq!(ret.columns.len(), 3);
    assert_eq!(ret.columns[0].attnum, -1);
    // unknown は text
    let d = del("DELETE FROM r RETURNING 'x'").unwrap();
    assert_eq!(d.returning.unwrap().columns[0].ty, SqlType::TEXT);
    // INSERT: VALUES でも INSERT ... SELECT でも、SELECT の FROM は見えない
    let i = ins("INSERT INTO r VALUES (1, 2) RETURNING id, v").unwrap();
    assert_eq!(i.returning.as_ref().unwrap().targets.len(), 2);
    let i = ins("INSERT INTO r SELECT a, c FROM t RETURNING *").unwrap();
    assert_eq!(i.returning.as_ref().unwrap().targets.len(), 2);
    assert_eq!(
        ins("INSERT INTO r VALUES (1, 2) RETURNING t.a")
            .unwrap_err()
            .sqlstate,
        sqlstate::UNDEFINED_TABLE
    );
    assert_eq!(
        ins("INSERT INTO r VALUES (1, 2) RETURNING zz")
            .unwrap_err()
            .sqlstate,
        sqlstate::UNDEFINED_COLUMN
    );
    // 別名で参照
    let u = upd("UPDATE r AS x SET v = 1 RETURNING x.v").unwrap();
    assert_eq!(u.returning.unwrap().columns[0].name, "v");
    // 他の表を参照すると 0A000
    for sql in [
        "UPDATE r SET v = 1 FROM t WHERE t.a = r.id RETURNING t.a",
        "UPDATE r SET v = 1 FROM t WHERE t.a = r.id RETURNING *",
        "DELETE FROM r USING t WHERE t.a = r.id RETURNING t.*",
    ] {
        let e = match sql.split_whitespace().next() {
            Some("UPDATE") => upd(sql).unwrap_err(),
            _ => del(sql).unwrap_err(),
        };
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED, "{sql}");
        assert_eq!(
            e.message, "RETURNING referencing other tables is not supported yet",
            "{sql}"
        );
    }
    // 対象表の列だけなら FROM があっても通る
    upd("UPDATE r SET v = 1 FROM t WHERE t.a = r.id RETURNING r.id, r.*").unwrap();
    // 副問い合わせは 0A000
    let e = upd("UPDATE r SET v = 1 RETURNING (SELECT 1)").unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    // 解析の順序: RETURNING の列のエラーは SET の列のエラーより先
    let e = upd("UPDATE r SET nosuch = 1 RETURNING qq").unwrap_err();
    assert_eq!(e.message, "column \"qq\" does not exist");
    // 検証（B9）
    let u = upd("UPDATE r SET v = 1 RETURNING id").unwrap();
    super::bound::BoundStatement::Update(u).validate().unwrap();
}

#[test]
fn dml_relation_kinds() {
    use crate::catalog::fake::{TableBuilder, default_sequence_params};
    let mut c = cat();
    c.add(&TableBuilder::new("ix_t").column("a", SqlType::INT4).index(
        Some("ix_t_idx"),
        &["a"],
        false,
    ));
    c.add_sequence("sq1", default_sequence_params(SqlType::INT4, None));
    assert_eq!(state(&c, "UPDATE nosuch SET v = 1"), "42P01");
    // 索引は表として開けない（FROM・UPDATE・DELETE・INSERT）
    for sql in [
        "SELECT * FROM ix_t_idx",
        "UPDATE ix_t_idx SET a = 1",
        "DELETE FROM ix_t_idx",
        "INSERT INTO ix_t_idx VALUES (1)",
        "UPDATE r SET v = 1 FROM ix_t_idx",
        "DELETE FROM r USING ix_t_idx",
    ] {
        let e = err_full(&c, sql);
        assert_eq!(e.sqlstate, sqlstate::WRONG_OBJECT_TYPE, "{sql}");
        assert_eq!(e.message, "cannot open relation \"ix_t_idx\"", "{sql}");
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for indexes."),
            "{sql}"
        );
        assert!(e.cursor_byte.is_some(), "{sql}");
    }
    // シーケンスは SELECT できるが、書き換えられない
    run(&c, "SELECT * FROM sq1").unwrap();
    for sql in [
        "UPDATE sq1 SET last_value = 1",
        "DELETE FROM sq1",
        "INSERT INTO sq1 VALUES (1, 0, true)",
    ] {
        let e = err_full(&c, sql);
        assert_eq!(e.sqlstate, sqlstate::WRONG_OBJECT_TYPE, "{sql}");
        assert_eq!(e.message, "cannot change sequence \"sq1\"", "{sql}");
    }
}

#[test]
fn cte_references_in_from() {
    let c = cat();
    let q = query(&c, "WITH x AS (SELECT a, b FROM t) SELECT * FROM x");
    let s = select_body(&q);
    assert!(matches!(
        s.rtable[0].kind,
        RteKind::CteRef { levels_up: 0, .. }
    ));
    assert_eq!(s.rtable[0].refname.as_deref(), Some("x"));
    assert_eq!(col_names(&q), ["a", "b"]);
    // 列別名で先頭から改名。CTE の列の由来が出力列に残る
    let q = query(
        &c,
        "WITH x AS (SELECT a, b FROM t) SELECT q, b FROM x AS y (q)",
    );
    assert_eq!(col_names(&q), ["q", "b"]);
    assert_eq!(q.columns[0].attnum, 1);
    let e = err_full(&c, "WITH x AS (SELECT 1) SELECT * FROM x AS y (a, b)");
    assert_eq!(e.sqlstate, sqlstate::INVALID_COLUMN_REFERENCE);
    // 同じ CTE を別名なしで 2 回は 42712、別名があれば通る
    let e = err_full(&c, "WITH x AS (SELECT 1) SELECT * FROM x, x");
    assert_eq!(e.sqlstate, sqlstate::DUPLICATE_ALIAS);
    run(
        &c,
        "WITH x AS (SELECT 1 a) SELECT * FROM x p, x q WHERE p.a = q.a",
    )
    .unwrap();
    // スキーマつきの名前は CTE を引かない
    let e = err_full(&c, "WITH x AS (SELECT 1) SELECT * FROM public.x");
    assert_eq!(e.message, "relation \"public.x\" does not exist");
    // 実表と同名なら CTE が勝つ
    let q = query(&c, "WITH t AS (SELECT 1 AS only_col) SELECT * FROM t");
    assert_eq!(col_names(&q), ["only_col"]);
    // 派生表の中から: levels_up は BoundQuery の入れ子を数える
    let q = query(
        &c,
        "WITH x AS (SELECT 1 a) SELECT * FROM (SELECT * FROM x) d",
    );
    let s = select_body(&q);
    let RteKind::Subquery { query: inner } = &s.rtable[0].kind else {
        panic!("not a subquery");
    };
    assert!(matches!(
        select_body(inner).rtable[0].kind,
        RteKind::CteRef { levels_up: 1, .. }
    ));
    q_validate(&q);
    // 結合
    run(
        &c,
        "WITH x AS (SELECT a FROM u) SELECT * FROM t JOIN x USING (a)",
    )
    .unwrap()
    .validate()
    .unwrap();
}

#[test]
fn dml_target_with_alias_scope() {
    let c = cat();
    let u = update(&c, "UPDATE r AS x SET v = x.id + 1 WHERE x.id > 0");
    assert_eq!(u.rtable[0].refname.as_deref(), Some("x"));
    assert!(u.from.is_empty());
}
