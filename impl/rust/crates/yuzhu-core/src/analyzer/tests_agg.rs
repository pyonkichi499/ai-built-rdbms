//! 集約・GROUP BY・HAVING・関数従属・DISTINCT ON・ORDER BY のアナライザのテスト（N2。`m4/03` §6.2）。

use super::bound::{
    BoundAggCall, BoundDistinct, BoundExpr, BoundExprKind, BoundQuery, BoundSelect, BoundSetExpr,
};
use super::tests::{err, names, query, run, types};
use crate::catalog::fake::{FakeCatalog as MemoryCatalog, TableBuilder};
use crate::expr::{RteId, Var};
use crate::types::{SqlType, oid};

fn cat() -> MemoryCatalog {
    let mut c = MemoryCatalog::new("postgres");
    c.add(
        &TableBuilder::new("t")
            .column("a", SqlType::INT4)
            .column("b", SqlType::INT4)
            .column("c", SqlType::TEXT)
            .column("f", SqlType::BOOL)
            .column("v", SqlType::varchar(5)),
    );
    c.add(
        &TableBuilder::new("u")
            .column("a", SqlType::INT4)
            .column("e", SqlType::INT4),
    );
    c.add(
        &TableBuilder::new("p")
            .column("id", SqlType::INT4)
            .column("v", SqlType::INT4)
            .primary_key(&["id"]),
    );
    c.add(
        &TableBuilder::new("pk2")
            .column("a", SqlType::INT4)
            .column("b", SqlType::INT4)
            .column("c", SqlType::INT4)
            .primary_key(&["a", "b"]),
    );
    c.add(
        &TableBuilder::new("uq")
            .column_nn("id", SqlType::INT4)
            .column("v", SqlType::INT4)
            .unique(&["id"]),
    );
    c
}

/// `(SQLSTATE, メッセージ, 位置（0 始まりのバイト）)`。
#[track_caller]
fn err_full(c: &MemoryCatalog, sql: &str) -> (&'static str, String, Option<u32>) {
    match run(c, sql) {
        Ok(s) => panic!("expected error for {sql}, got {s:?}"),
        Err(e) => (e.sqlstate.code(), e.message, e.cursor_byte),
    }
}

#[track_caller]
fn select_body(q: &BoundQuery) -> &BoundSelect {
    match &q.body {
        BoundSetExpr::Select(s) => s,
        other => panic!("not a select body: {other:?}"),
    }
}

#[track_caller]
fn sel(c: &MemoryCatalog, sql: &str) -> (BoundQuery, BoundSelect) {
    let q = query(c, sql);
    let s = select_body(&q).clone();
    (q, s)
}

#[track_caller]
fn agg(e: &BoundExpr) -> &BoundAggCall {
    match &e.kind {
        BoundExprKind::Aggregate(a) => a,
        other => panic!("not an aggregate: {other:?}"),
    }
}

/// `SELECT <expr> FROM t` の最初の出力。
#[track_caller]
fn first(c: &MemoryCatalog, sql: &str) -> BoundExpr {
    sel(c, sql).1.targets.remove(0)
}

#[track_caller]
fn var(e: &BoundExpr) -> Var {
    match &e.kind {
        BoundExprKind::Column(v) => *v,
        other => panic!("not a column: {other:?}"),
    }
}

// ----- 集約の解決（5.5.1）--------------------------------------------------------

#[test]
fn aggregate_result_types() {
    let c = cat();
    assert_eq!(
        types(
            &c,
            "SELECT count(*), count(a), sum(a), sum(a::bigint), avg(a), avg(a::float8), max(c), min(a) FROM t"
        ),
        [
            oid::INT8,
            oid::INT8,
            oid::INT8,
            oid::NUMERIC,
            oid::NUMERIC,
            oid::FLOAT8,
            oid::TEXT,
            oid::INT4
        ]
    );
    assert_eq!(
        types(
            &c,
            "SELECT sum(a::smallint), sum(a::float4), sum(a::float8), sum(a::numeric), avg(a::smallint) FROM t"
        ),
        [
            oid::INT8,
            oid::FLOAT4,
            oid::FLOAT8,
            oid::NUMERIC,
            oid::NUMERIC
        ]
    );
    assert_eq!(
        types(&c, "SELECT bool_and(f), bool_or(f), every(f) FROM t"),
        [oid::BOOL, oid::BOOL, oid::BOOL]
    );
    // 集約の出力名は関数名（FigureColname）。
    assert_eq!(
        names(
            &c,
            "SELECT count(*), sum(a), max(c) FILTER (WHERE a > 1) FROM t"
        ),
        ["count", "sum", "max"]
    );
}

#[test]
fn aggregate_call_shapes() {
    let c = cat();
    let e = first(&c, "SELECT count(*) FROM t");
    let a = agg(&e);
    assert!(a.args.is_empty() && !a.distinct && a.filter.is_none());
    assert_eq!(a.func.args, &[] as &[u32]);

    let e = first(&c, "SELECT count(DISTINCT a) FROM t");
    assert!(agg(&e).distinct);
    assert_eq!(agg(&e).args.len(), 1);

    let e = first(&c, "SELECT count(a) FILTER (WHERE b > 1) FROM t");
    let f = agg(&e).filter.as_ref().expect("filter");
    assert_eq!(f.ty, SqlType::BOOL);

    // `count("any")` の引数は変換しない。unknown の定数は text。
    let e = first(&c, "SELECT count('x') FROM t");
    assert_eq!(agg(&e).args[0].ty, SqlType::TEXT);
    let e = first(&c, "SELECT count(a) FROM t");
    assert!(matches!(agg(&e).args[0].kind, BoundExprKind::Column(_)));
    first(&c, "SELECT count(NULL) FROM t");
    first(&c, "SELECT pg_catalog.count(*) FROM t");
}

#[test]
fn aggregate_argument_coercion() {
    let c = cat();
    // 引数の型の行が選ばれる
    let e = first(&c, "SELECT sum(1::smallint) FROM t");
    assert_eq!(agg(&e).func.args, &[oid::INT2]);
    assert_eq!(agg(&e).func.result, oid::INT8);
    // varchar は text の行（キャストを挟む）
    let e = first(&c, "SELECT max(v) FROM t");
    assert_eq!(agg(&e).func.args, &[oid::TEXT]);
    assert!(matches!(agg(&e).args[0].kind, BoundExprKind::Cast { .. }));
    assert_eq!(e.ty, SqlType::TEXT);
    // unknown の定数: min / max は text、ほかは曖昧
    assert_eq!(
        types(&c, "SELECT max('a'), min(NULL) FROM t"),
        [oid::TEXT, oid::TEXT]
    );
    // int2 → int4 などの暗黙キャストは完全一致の行がなければ使う
    let e = first(&c, "SELECT max(1::smallint) FROM t");
    assert_eq!(e.ty.oid, oid::INT2);
}

#[test]
fn aggregate_resolution_errors() {
    let c = cat();
    assert_eq!(err_full(&c, "SELECT abs(DISTINCT a) FROM t").0, "42809");
    assert_eq!(
        err_full(&c, "SELECT abs(DISTINCT a) FROM t").1,
        "DISTINCT specified, but abs is not an aggregate function"
    );
    assert_eq!(
        err_full(&c, "SELECT abs(a) FILTER (WHERE true) FROM t"),
        (
            "42809",
            "FILTER specified, but abs is not an aggregate function".to_owned(),
            Some(7)
        )
    );
    assert_eq!(
        err_full(&c, "SELECT abs(*) FROM t"),
        ("42883", "function abs() does not exist".to_owned(), Some(7))
    );
    assert_eq!(
        err_full(&c, "SELECT count() FROM t").1,
        "count(*) must be used to call a parameterless aggregate function"
    );
    assert_eq!(err(&c, "SELECT count() FROM t"), "42809");
    assert_eq!(
        err_full(&c, "SELECT sum() FROM t").1,
        "function sum() does not exist"
    );
    assert_eq!(err(&c, "SELECT sum(*) FROM t"), "42883");
    assert_eq!(
        err_full(&c, "SELECT sum(c) FROM t").1,
        "function sum(text) does not exist"
    );
    assert_eq!(err(&c, "SELECT avg('1'::text) FROM t"), "42883");
    assert_eq!(
        err_full(&c, "SELECT sum('1') FROM t"),
        (
            "42725",
            "function sum(unknown) is not unique".to_owned(),
            Some(7)
        )
    );
    assert_eq!(err(&c, "SELECT sum(NULL) FROM t"), "42725");
    assert_eq!(
        err_full(&c, "SELECT min(f) FROM t").1,
        "function min(boolean) does not exist"
    );
    assert_eq!(
        err_full(&c, "SELECT count(a, b) FROM t").1,
        "function count(integer, integer) does not exist"
    );
    assert_eq!(
        err_full(&c, "SELECT count(DISTINCT a, b) FROM t").0,
        "42883"
    );
    assert_eq!(
        err_full(&c, "SELECT count(*) FILTER (WHERE a) FROM t").1,
        "argument of FILTER must be type boolean, not type integer"
    );
    // スキーマ付き
    assert_eq!(
        err_full(&c, "SELECT public.count(*) FROM t").1,
        "function public.count() does not exist"
    );
    assert_eq!(err(&c, "SELECT nosuch.count(*) FROM t"), "3F000");
    // 存在しない関数に DISTINCT
    assert_eq!(err(&c, "SELECT nosuch(DISTINCT a) FROM t"), "42883");
    // 未対応の集約名
    assert_eq!(
        err_full(&c, "SELECT array_agg(c) FROM t"),
        (
            "0A000",
            "aggregate function array_agg is not supported yet".to_owned(),
            Some(7)
        )
    );
    assert_eq!(err(&c, "SELECT array_agg(DISTINCT a) FROM t"), "0A000");
    assert_eq!(err(&c, "SELECT stddev(a) FROM t"), "0A000");
    // 引数のエラーは解決より先
    assert_eq!(err(&c, "SELECT sum(zz) FROM t"), "42703");
}

// ----- 禁止位置・入れ子（5.5.1、5.12.1）------------------------------------------

#[test]
fn aggregates_forbidden_positions() {
    let c = cat();
    let forbidden = |sql: &str, clause: &str| {
        let (code, msg, _) = err_full(&c, sql);
        assert_eq!(
            (code, msg.as_str()),
            (
                "42803",
                format!("aggregate functions are not allowed in {clause}").as_str()
            ),
            "{sql}"
        );
    };
    forbidden("SELECT 1 FROM t WHERE count(*) > 1", "WHERE");
    forbidden("SELECT 1 FROM t GROUP BY count(*)", "GROUP BY");
    forbidden("SELECT a FROM t LIMIT count(*)", "LIMIT");
    forbidden("SELECT a FROM t OFFSET count(*)", "OFFSET");
    forbidden("VALUES (count(*))", "VALUES");
    forbidden(
        "SELECT count(*) FILTER (WHERE count(*) > 1) FROM t",
        "FILTER",
    );
    forbidden("UPDATE t SET a = count(*)", "UPDATE");
    forbidden("SELECT 1 FROM t JOIN u ON count(*) > 1", "JOIN conditions");
    forbidden(
        "SELECT 1 FROM generate_series(1, count(*)) AS g",
        "functions in FROM",
    );
    forbidden(
        "CREATE TABLE x (a int CHECK (count(*) > 1))",
        "check constraints",
    );
    forbidden(
        "CREATE TABLE x (a int DEFAULT count(*))",
        "DEFAULT expressions",
    );
    // 位置は集約の呼び出し
    assert_eq!(
        err_full(&c, "SELECT 1 FROM t WHERE a > count(*)").2,
        Some(26)
    );
    // 引数の中の集約は、内側が先に文脈の検査を受ける
    forbidden("SELECT 1 FROM t WHERE sum(count(*)) > 1", "WHERE");
}

#[test]
fn nested_aggregates() {
    let c = cat();
    assert_eq!(
        err_full(&c, "SELECT sum(count(*)) FROM t"),
        (
            "42803",
            "aggregate function calls cannot be nested".to_owned(),
            Some(11)
        )
    );
    assert_eq!(
        err_full(&c, "SELECT sum(count(*) FILTER (WHERE a > 0)) FROM t").1,
        "aggregate function calls cannot be nested"
    );
    assert_eq!(
        err_full(&c, "SELECT max(a) FILTER (WHERE a > sum(b)) FROM t").1,
        "aggregate functions are not allowed in FILTER"
    );
    assert_eq!(err(&c, "SELECT abs(sum(count(a))) FROM t"), "42803");
}

// ----- グループ化の検査（5.5.3）---------------------------------------------------

#[test]
fn ungrouped_columns() {
    let c = cat();
    assert_eq!(
        err_full(&c, "SELECT a, count(*) FROM t"),
        (
            "42803",
            "column \"t.a\" must appear in the GROUP BY clause or be used in an aggregate function"
                .to_owned(),
            Some(7)
        )
    );
    assert_eq!(err(&c, "SELECT count(*) FROM t HAVING a > 1"), "42803");
    assert_eq!(
        err_full(&c, "SELECT a FROM t ORDER BY count(*)"),
        (
            "42803",
            "column \"t.a\" must appear in the GROUP BY clause or be used in an aggregate function"
                .to_owned(),
            Some(7)
        )
    );
    assert_eq!(err(&c, "SELECT a, b FROM t GROUP BY a"), "42803");
    assert_eq!(err(&c, "SELECT a + b FROM t GROUP BY a"), "42803");
    assert_eq!(
        err(&c, "SELECT sum(b) FROM t GROUP BY a HAVING b > 1"),
        "42803"
    );
    // システム列
    assert_eq!(
        err_full(&c, "SELECT ctid, count(*) FROM t").1,
        "column \"t.ctid\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    // GROUP BY の式の一致
    let (_, s) = sel(&c, "SELECT a + 1 FROM t GROUP BY a + 1");
    assert!(s.has_agg && s.group_by.len() == 1);
    assert_eq!(err(&c, "SELECT a + 2 FROM t GROUP BY a + 1"), "42803");
    assert_eq!(err(&c, "SELECT a + 1, b FROM t GROUP BY a + 1"), "42803");
    sel(&c, "SELECT (a + 1) * 2 FROM t GROUP BY a + 1");
    // 集約の中は何でもよい
    sel(&c, "SELECT a, sum(b * 2 + a) FROM t GROUP BY a");
    sel(
        &c,
        "SELECT abs(sum(b)), CASE WHEN count(*) > 3 THEN 'many' ELSE 'few' END FROM t",
    );
    // ORDER BY の式
    sel(&c, "SELECT a FROM t GROUP BY a ORDER BY count(*)");
    sel(&c, "SELECT a FROM t GROUP BY a ORDER BY a + 1");
    assert_eq!(err(&c, "SELECT a FROM t GROUP BY a ORDER BY b"), "42803");
}

#[test]
fn has_agg_flag() {
    let c = cat();
    let has = |sql: &str| sel(&c, sql).1.has_agg;
    assert!(!has("SELECT a FROM t"));
    assert!(!has("SELECT 1"));
    assert!(has("SELECT count(*) FROM t"));
    assert!(has("SELECT a FROM t GROUP BY a"));
    // 集約がなくても HAVING があれば集約の問い合わせ
    assert!(has("SELECT 1 HAVING false"));
    assert!(has("SELECT a FROM t GROUP BY a HAVING a > 1"));
    // ORDER BY の集約だけ
    assert!(has("SELECT 1 FROM t ORDER BY count(*)"));
    // DISTINCT ON の式の集約
    assert!(has("SELECT DISTINCT ON (count(*)) 1 FROM t"));
    // filter は集約を持たない。having は bool
    let (_, s) = sel(
        &c,
        "SELECT a FROM t WHERE b > 1 GROUP BY a HAVING count(*) > 1",
    );
    assert!(s.filter.is_some());
    assert_eq!(s.having.as_ref().map(|h| h.ty), Some(SqlType::BOOL));
    assert_eq!(
        err_full(&c, "SELECT a FROM t GROUP BY a HAVING a").1,
        "argument of HAVING must be type boolean, not type integer"
    );
}

#[test]
fn having_ignores_output_aliases() {
    // HAVING は出力列の別名・位置番号を見ない（`c` は t.c）。
    let c = cat();
    assert_eq!(err(&c, "SELECT count(*) AS c FROM t HAVING c > 1"), "42883");
}

// ----- 関数従属（5.5.4）-----------------------------------------------------------

#[test]
fn primary_key_dependency() {
    let c = cat();
    let (_, s) = sel(&c, "SELECT id, v FROM p GROUP BY id");
    assert_eq!(s.group_by.len(), 2);
    assert_eq!(var(&s.group_by[0]), Var::user(RteId(0), 0));
    assert_eq!(var(&s.group_by[1]), Var::user(RteId(0), 1));
    // 重複なし
    let (_, s) = sel(&c, "SELECT v, v + 1, id FROM p GROUP BY id HAVING v > 0");
    assert_eq!(s.group_by.len(), 2);
    // 従属を使わなければ足されない
    let (_, s) = sel(&c, "SELECT id FROM p GROUP BY id");
    assert_eq!(s.group_by.len(), 1);
    // 別名つきでも通る。集約の中の列は従属の対象外
    sel(&c, "SELECT x.id, x.v FROM p x GROUP BY x.id");
    let (_, s) = sel(&c, "SELECT id, sum(v) FROM p GROUP BY id");
    assert_eq!(s.group_by.len(), 1);
    // 複合主キーは全列が要る
    assert_eq!(
        err_full(&c, "SELECT a, c FROM pk2 GROUP BY a").1,
        "column \"pk2.c\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    let (_, s) = sel(&c, "SELECT a, c FROM pk2 GROUP BY a, b");
    assert_eq!(s.group_by.len(), 3);
    // UNIQUE NOT NULL は対象外
    assert_eq!(
        err_full(&c, "SELECT id, v FROM uq GROUP BY id").1,
        "column \"uq.v\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    // 主キーのない表
    assert_eq!(err(&c, "SELECT a, b FROM t GROUP BY a"), "42803");
    // システム列も同じ表の従属
    sel(&c, "SELECT id, ctid FROM p GROUP BY id");
}

// ----- GROUP BY / ORDER BY の項目の解決（5.6.1〜5.6.3）------------------------------

#[test]
fn group_by_resolution() {
    let c = cat();
    // 入力列が先: `b` は t.b
    let (_, s) = sel(&c, "SELECT b, count(*) FROM t GROUP BY b");
    assert_eq!(var(&s.group_by[0]), Var::user(RteId(0), 1));
    assert_eq!(
        err(&c, "SELECT a AS b, count(*) FROM t GROUP BY b"),
        "42803"
    );
    // 入力列にない名前は出力列の別名
    let (_, s) = sel(&c, "SELECT a + 0 AS zz, count(*) FROM t GROUP BY zz");
    assert_eq!(s.group_by.len(), 1);
    assert!(s.group_by[0].same_as(&s.targets[0]));
    // 式の中の別名は見ない
    assert_eq!(
        err(&c, "SELECT (a + 1) AS k, count(*) FROM t GROUP BY k + 1"),
        "42703"
    );
    // 位置番号
    let (_, s) = sel(&c, "SELECT b, count(*) FROM t GROUP BY 1");
    assert!(s.group_by[0].same_as(&s.targets[0]));
    assert_eq!(
        err_full(&c, "SELECT a FROM t GROUP BY 0"),
        (
            "42P10",
            "GROUP BY position 0 is not in select list".to_owned(),
            Some(25)
        )
    );
    assert_eq!(err(&c, "SELECT a FROM t GROUP BY -1"), "42P10");
    assert_eq!(err(&c, "SELECT a FROM t GROUP BY 2"), "42P10");
    // 整数以外のリテラル
    for lit in ["TRUE", "NULL", "1.5", "'a'"] {
        let (code, msg, _) = err_full(&c, &format!("SELECT a FROM t GROUP BY {lit}"));
        assert_eq!(
            (code, msg.as_str()),
            ("42601", "non-integer constant in GROUP BY")
        );
    }
    // 集約を含む出力列を指す
    assert_eq!(
        err_full(&c, "SELECT count(*) FROM t GROUP BY 1"),
        (
            "42803",
            "aggregate functions are not allowed in GROUP BY".to_owned(),
            Some(7)
        )
    );
    assert_eq!(err(&c, "SELECT count(*) AS n FROM t GROUP BY n"), "42803");
    // 重複は残す
    let (_, s) = sel(&c, "SELECT a FROM t GROUP BY a, a");
    assert_eq!(s.group_by.len(), 2);
    // ORDER BY で足された resjunk と同じ式
    let (_, s) = sel(&c, "SELECT a FROM t GROUP BY a, a + 1 ORDER BY a + 1");
    assert_eq!(s.group_by.len(), 2);
}

#[test]
fn order_by_resolution() {
    let c = cat();
    // 出力列の名前が先: `a` は 2 番目の出力列（t.b）
    let q = query(&c, "SELECT a AS b, b AS a FROM t ORDER BY a");
    assert_eq!(q.order_by[0].target, 1);
    // 式の中の別名は見ない
    assert_eq!(err(&c, "SELECT a + b AS s FROM t ORDER BY s + 1"), "42703");
    assert_eq!(
        err_full(&c, "SELECT a AS x, b AS x FROM t ORDER BY x").1,
        "ORDER BY \"x\" is ambiguous"
    );
    // 同じ式の同名は曖昧でない
    query(&c, "SELECT a AS x, a AS x FROM t ORDER BY x");
    assert_eq!(
        err_full(&c, "SELECT a FROM t ORDER BY 4"),
        (
            "42P10",
            "ORDER BY position 4 is not in select list".to_owned(),
            Some(25)
        )
    );
    // 整数以外のリテラル（R7）
    for lit in ["TRUE", "FALSE", "NULL", "1.5", "'a'"] {
        let (code, msg, _) = err_full(&c, &format!("SELECT a FROM t ORDER BY {lit}"));
        assert_eq!(
            (code, msg.as_str()),
            ("42601", "non-integer constant in ORDER BY"),
            "{lit}"
        );
    }
    // 式は resjunk（同じ式が出力にあればそれ）
    let q = query(&c, "SELECT a FROM t ORDER BY b");
    assert_eq!(q.order_by[0].target, 1);
    let q = query(&c, "SELECT a + 1 FROM t ORDER BY a + 1");
    assert_eq!(q.order_by[0].target, 0);
    // 同じ項目の重複は 1 つ
    let q = query(&c, "SELECT a FROM t ORDER BY a, a, 1");
    assert_eq!(q.order_by.len(), 1);
    let q = query(&c, "SELECT a FROM t ORDER BY a, a DESC");
    assert_eq!(q.order_by.len(), 2);
    // NULLS の既定
    let q = query(&c, "SELECT a FROM t ORDER BY a DESC, a NULLS FIRST");
    assert!(q.order_by[0].nulls_first && q.order_by[1].nulls_first);
    let q = query(&c, "SELECT a FROM t ORDER BY a");
    assert!(!q.order_by[0].nulls_first);
}

// ----- DISTINCT / DISTINCT ON（5.6.4）---------------------------------------------

#[test]
fn distinct_requires_selected_order_by() {
    let c = cat();
    query(&c, "SELECT DISTINCT a + 1 FROM t ORDER BY a + 1");
    query(&c, "SELECT DISTINCT b AS bb FROM t ORDER BY bb DESC");
    assert_eq!(
        err_full(&c, "SELECT DISTINCT a FROM t ORDER BY a + 1"),
        (
            "42P10",
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list".to_owned(),
            Some(34)
        )
    );
    assert_eq!(err(&c, "SELECT DISTINCT b FROM t ORDER BY a"), "42P10");
    let (_, s) = sel(&c, "SELECT DISTINCT a FROM t");
    assert_eq!(s.distinct, BoundDistinct::All);
}

#[test]
fn distinct_on_cases() {
    let c = cat();
    let on = |sql: &str| -> Vec<usize> {
        let (_, s) = sel(&c, sql);
        match s.distinct {
            BoundDistinct::On(v) => v,
            other => panic!("{sql}: {other:?}"),
        }
    };
    let mismatch = |sql: &str| {
        let (code, msg, _) = err_full(&c, sql);
        assert_eq!(
            (code, msg.as_str()),
            (
                "42P10",
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
            ),
            "{sql}"
        );
    };
    let head = "SELECT DISTINCT ON (a) a, b, c FROM t";
    assert_eq!(on(head), [0]);
    assert_eq!(on(&format!("{head} ORDER BY a")), [0]);
    assert_eq!(on(&format!("{head} ORDER BY a, b DESC")), [0]);
    assert_eq!(on(&format!("{head} ORDER BY a, c, b")), [0]);
    mismatch(&format!("{head} ORDER BY b"));
    mismatch(&format!("{head} ORDER BY b, a"));
    let head2 = "SELECT DISTINCT ON (a, b) a, b, c FROM t";
    mismatch(&format!("{head2} ORDER BY a, c"));
    assert_eq!(on(&format!("{head2} ORDER BY a")), [0, 1]);
    assert_eq!(on(&format!("{head2} ORDER BY a, b, c")), [0, 1]);
    assert_eq!(on(&format!("{head2} ORDER BY b DESC")), [1, 0]);
    assert_eq!(
        on("SELECT DISTINCT ON (b, a) a, b, c FROM t ORDER BY a, b"),
        [0, 1]
    );
    assert_eq!(
        on("SELECT DISTINCT ON (b, a) a, b, c FROM t ORDER BY b, a"),
        [1, 0]
    );
    assert_eq!(on("SELECT DISTINCT ON (a, a) a, b FROM t ORDER BY a"), [0]);
    // 式: ORDER BY の resjunk と同じ式
    let (q, s) = sel(&c, "SELECT DISTINCT ON (a + 1) a FROM t ORDER BY a + 1");
    assert_eq!(s.distinct, BoundDistinct::On(vec![1]));
    assert_eq!(q.order_by[0].target, 1);
    assert_eq!(s.targets.len(), 2);
    // 式だけの DISTINCT ON は resjunk に足す
    let (_, s) = sel(&c, "SELECT DISTINCT ON (b) a FROM t");
    assert_eq!(s.distinct, BoundDistinct::On(vec![1]));
    // 出力列の名前・位置番号
    assert_eq!(on("SELECT DISTINCT ON (x) a AS x, b FROM t"), [0]);
    assert_eq!(on("SELECT DISTINCT ON (2) a, b FROM t"), [1]);
    assert_eq!(
        err_full(&c, "SELECT DISTINCT ON (3) a, b FROM t").1,
        "DISTINCT ON position 3 is not in select list"
    );
    assert_eq!(
        err_full(&c, "SELECT DISTINCT ON (TRUE) a FROM t").1,
        "non-integer constant in DISTINCT ON"
    );
    // 集約
    assert_eq!(err(&c, "SELECT DISTINCT ON (count(*)) a FROM t"), "42803");
    assert_eq!(
        on("SELECT DISTINCT ON (a) count(*) FROM t GROUP BY a ORDER BY a"),
        [1]
    );
}

// ----- LIMIT / OFFSET の変数の検査（5.12.2）----------------------------------------

#[test]
fn limit_must_not_contain_variables() {
    let c = cat();
    assert_eq!(
        err_full(&c, "SELECT a FROM t LIMIT a"),
        (
            "42P10",
            "argument of LIMIT must not contain variables".to_owned(),
            Some(22)
        )
    );
    assert_eq!(
        err_full(&c, "SELECT a FROM t OFFSET a + 1").1,
        "argument of OFFSET must not contain variables"
    );
    let q = query(&c, "SELECT a FROM t LIMIT 1.5 OFFSET 2");
    assert_eq!(q.limit.as_ref().map(|l| l.ty), Some(SqlType::INT8));
    assert_eq!(err(&c, "SELECT a FROM t LIMIT 'x'"), "22P02");
}

// ----- 結合・副問い合わせが要るケース（N1・N3 の実装後に通る）---------------------------

#[test]
fn grouping_over_joins() {
    let c = cat();
    // 結合の別名の展開: USING の `a` は t.a
    sel(&c, "SELECT t.a FROM t JOIN u USING (a) GROUP BY a");
    assert_eq!(
        err(&c, "SELECT u.a FROM t JOIN u USING (a) GROUP BY a"),
        "42803"
    );
    sel(&c, "SELECT a FROM t FULL JOIN u USING (a) GROUP BY a");
    // 従属は RTE ごと（t に主キー情報がない）
    assert_eq!(
        err_full(
            &c,
            "SELECT p.v, t.c FROM p JOIN t ON t.a = p.id GROUP BY p.id"
        )
        .1,
        "column \"t.c\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    sel(
        &c,
        "SELECT p.v, count(t.c) FROM p JOIN t ON t.a = p.id GROUP BY p.id",
    );
    // 曖昧な名前は GROUP BY が先に 42702
    assert_eq!(err(&c, "SELECT t.a FROM t, u GROUP BY a"), "42702");
    // 派生表は従属の対象外
    assert_eq!(
        err_full(&c, "SELECT v FROM (SELECT id, v FROM p) s GROUP BY id").1,
        "column \"s.v\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
}

#[test]
fn grouping_with_subqueries() {
    let c = cat();
    // 副問い合わせの中の未グループ列
    assert_eq!(
        err_full(
            &c,
            "SELECT 1 FROM t HAVING EXISTS (SELECT 1 FROM u WHERE u.a = t.a)"
        )
        .1,
        "subquery uses ungrouped column \"t.a\" from outer query"
    );
    assert_eq!(
        err_full(
            &c,
            "SELECT (SELECT max(u.e + t.b) FROM u) FROM t GROUP BY a"
        )
        .1,
        "subquery uses ungrouped column \"t.b\" from outer query"
    );
    sel(
        &c,
        "SELECT count(*) FROM t GROUP BY a HAVING a IN (SELECT a FROM u)",
    );
    sel(&c, "SELECT id, (SELECT p.v) FROM p GROUP BY id");
    // 副問い合わせの集約は内側のスコープ
    sel(&c, "SELECT sum((SELECT count(*) FROM u)) FROM t");
    sel(&c, "SELECT (SELECT max(u.e + t.b) FROM u) FROM t");
    // 外側のスコープに属する集約は未対応
    assert_eq!(err(&c, "SELECT (SELECT max(t.a)) FROM t"), "0A000");
    assert_eq!(
        err_full(&c, "SELECT (SELECT max(t.a)) FROM t").1,
        "aggregate functions of an outer query level are not supported yet"
    );
    // LIMIT の中の副問い合わせ
    assert_eq!(
        err(
            &c,
            "SELECT a FROM t LIMIT (SELECT a FROM u WHERE u.a = t.a)"
        ),
        "42P10"
    );
    query(&c, "SELECT a FROM t WHERE a IN (SELECT a FROM u LIMIT t.a)");
}
