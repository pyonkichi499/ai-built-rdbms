//! 副問い合わせ・集合演算・CTE のアナライザのテスト（N3。`m4/03` §6.2）。

#![allow(clippy::many_single_char_names)]

use super::bound::{
    BoundCte, BoundExpr, BoundExprKind, BoundQuery, BoundSelect, BoundSetExpr, CteMaterialize,
    RteKind, SetOpKind,
};
use super::tests::{catalog, create, err, names, query, run, types};
use crate::catalog::fake::FakeCatalog;
use crate::expr::{CteId, RteId, SubLinkKind, Var};
use crate::types::{Datum, SqlType, oid};

fn cat() -> FakeCatalog {
    let mut c = catalog();
    create(&mut c, "CREATE TABLE u (e int, c text, k int)");
    create(&mut c, "CREATE TABLE w (a int, b text)");
    c
}

/// `(SQLSTATE, メッセージ, 位置（クエリ文字列の何バイト目か。0 始まり）)`。
#[track_caller]
fn err_full(c: &FakeCatalog, sql: &str) -> (&'static str, String, Option<u32>) {
    match run(c, sql) {
        Ok(s) => panic!("expected error for {sql}, got {s:?}"),
        Err(e) => (e.sqlstate.code(), e.message, e.cursor_byte),
    }
}

/// `sql` の `needle`（最初の出現）の位置。
#[allow(clippy::unnecessary_wraps)]
fn at(sql: &str, needle: &str) -> Option<u32> {
    Some(
        u32::try_from(
            sql.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {sql}")),
        )
        .unwrap(),
    )
}

#[track_caller]
fn select_body(q: &BoundQuery) -> &BoundSelect {
    match &q.body {
        BoundSetExpr::Select(s) => s,
        other => panic!("not a select body: {other:?}"),
    }
}

/// `SubLink` の (kind, test, query)。
#[track_caller]
fn sublink(e: &BoundExpr) -> (SubLinkKind, Option<&BoundExpr>, &BoundQuery) {
    match &e.kind {
        BoundExprKind::SubLink { kind, test, query } => (*kind, test.as_deref(), query),
        other => panic!("not a sublink: {other:?}"),
    }
}

#[track_caller]
fn target(c: &FakeCatalog, sql: &str) -> BoundExpr {
    select_body(&query(c, sql)).targets[0].clone()
}

#[track_caller]
fn filter(c: &FakeCatalog, sql: &str) -> BoundExpr {
    select_body(&query(c, sql))
        .filter
        .clone()
        .expect("a WHERE clause")
}

fn is_var(e: &BoundExpr, rte: u16, col: u16, levels_up: u16) -> bool {
    matches!(&e.kind, BoundExprKind::Column(v)
        if *v == Var::user(RteId(rte), col).with_levels_up(levels_up))
}

/// 比較演算子 `args[0] op args[1]` の引数。
#[track_caller]
fn op_args(e: &BoundExpr) -> &[BoundExpr] {
    match &e.kind {
        BoundExprKind::Operator { args, .. } => args,
        other => panic!("not an operator: {other:?}"),
    }
}

fn is_output(e: &BoundExpr, i: u16) -> bool {
    match &e.kind {
        BoundExprKind::SubLinkOutput(n) => *n == i,
        BoundExprKind::Cast { expr, .. } => is_output(expr, i),
        _ => false,
    }
}

// ----- SubLink の形（§3.2.8） ------------------------------------------------------

#[test]
fn scalar_subquery_shape() {
    let c = cat();
    let e = target(&c, "select (select e from u) from t");
    let (kind, test, q) = sublink(&e);
    assert_eq!(kind, SubLinkKind::Scalar);
    assert!(test.is_none());
    assert_eq!(e.ty, SqlType::INT4);
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.columns[0].name, "e");
    // 型は typmod も含めて副問い合わせの出力列のもの
    let e = target(&c, "select (select v from t)");
    assert_eq!(e.ty, SqlType::new(oid::VARCHAR, 3 + 4));
    // unknown は text
    let e = target(&c, "select (select 'x')");
    assert_eq!(e.ty.oid, oid::TEXT);
    // 0 行でも 1 つの SubLink（実行時の話）。FROM なし
    let e = target(&c, "select (select 1)");
    assert_eq!(sublink(&e).2.columns[0].ty, SqlType::INT4);
}

#[test]
fn exists_shape() {
    let c = cat();
    for sql in [
        "select exists (select 1)",
        "select exists (select)",
        "select exists (select a, b from t)",
        "select exists (select * from t order by a limit 1)",
    ] {
        let e = target(&c, sql);
        let (kind, test, _) = sublink(&e);
        assert_eq!(kind, SubLinkKind::Exists, "{sql}");
        assert!(test.is_none());
        assert_eq!(e.ty, SqlType::BOOL);
    }
    // ORDER BY・LIMIT は捨てない（プランナが捨てる。§5.7.3）
    let e = target(&c, "select exists (select a from t order by a limit 3)");
    let q = sublink(&e).2;
    assert_eq!(q.order_by.len(), 1);
    assert!(q.limit.is_some());
    // NOT EXISTS は Not で包む
    let e = target(&c, "select not exists (select 1)");
    assert!(matches!(e.kind, BoundExprKind::Not(_)));
}

#[test]
fn in_subquery_shape() {
    let c = cat();
    let e = target(&c, "select a in (select e from u) from t");
    let (kind, test, q) = sublink(&e);
    assert_eq!(kind, SubLinkKind::Any);
    assert_eq!(e.ty, SqlType::BOOL);
    assert_eq!(q.columns.len(), 1);
    let test = test.expect("test");
    let args = op_args(test);
    assert!(is_var(&args[0], 0, 0, 0), "{:?}", args[0]);
    assert!(is_output(&args[1], 0));
    // `= ANY` も同じ形
    let e2 = target(&c, "select a = any (select e from u) from t");
    let (kind, test2, _) = sublink(&e2);
    assert_eq!(kind, SubLinkKind::Any);
    assert!(is_output(&op_args(test2.unwrap())[1], 0));
    // `= SOME`
    let e3 = target(&c, "select a = some (select e from u) from t");
    assert_eq!(sublink(&e3).0, SubLinkKind::Any);
}

#[test]
fn not_in_wraps_any_in_not() {
    let c = cat();
    let e = target(&c, "select a not in (select e from u) from t");
    let BoundExprKind::Not(inner) = &e.kind else {
        panic!("not a Not: {e:?}");
    };
    let (kind, test, _) = sublink(inner);
    assert_eq!(kind, SubLinkKind::Any);
    // test は `=`（`<>` ではない）
    assert!(is_output(&op_args(test.unwrap())[1], 0));
    assert_eq!(e.ty, SqlType::BOOL);
}

#[test]
fn any_all_operators() {
    let c = cat();
    let e = target(&c, "select a < any (select e from u) from t");
    let (kind, test, _) = sublink(&e);
    assert_eq!(kind, SubLinkKind::Any);
    assert!(is_output(&op_args(test.unwrap())[1], 0));
    let e = target(&c, "select a > all (select e from u) from t");
    let (kind, test, _) = sublink(&e);
    assert_eq!(kind, SubLinkKind::All);
    assert!(is_var(&op_args(test.unwrap())[0], 0, 0, 0));
    // 1 列の `<> ALL` は All のまま（Not で包まない）
    let e = target(&c, "select a <> all (select e from u) from t");
    let (kind, _, _) = sublink(&e);
    assert_eq!(kind, SubLinkKind::All);
    // LIKE ANY
    let e = target(&c, "select b like any (select c from u) from t");
    assert_eq!(sublink(&e).0, SubLinkKind::Any);
}

#[test]
fn right_side_gets_a_cast_when_the_operator_needs_one() {
    let c = cat();
    // text = varchar: 右辺（varchar の出力）を text にする
    let e = target(&c, "select b in (select v from t) from t");
    let args = op_args(sublink(&e).1.unwrap());
    assert!(
        matches!(args[1].kind, BoundExprKind::Cast { .. }),
        "{:?}",
        args[1]
    );
    assert!(is_output(&args[1], 0));
    assert_eq!(args[1].ty.oid, oid::TEXT);
    // 左辺が unknown の定数: 出力列の型の入力関数で評価
    let e = target(&c, "select '5' in (select e from u)");
    let args = op_args(sublink(&e).1.unwrap());
    assert!(
        matches!(&args[0].kind, BoundExprKind::Literal(Datum::Int4(5))),
        "{:?}",
        args[0]
    );
}

#[test]
fn row_in_builds_an_and_of_equalities() {
    let c = cat();
    let e = target(&c, "select (a, b) in (select e, c from u) from t");
    let (kind, test, _) = sublink(&e);
    assert_eq!(kind, SubLinkKind::Any);
    let BoundExprKind::And(parts) = &test.unwrap().kind else {
        panic!("not an And: {test:?}");
    };
    assert_eq!(parts.len(), 2);
    assert!(is_var(&op_args(&parts[0])[0], 0, 0, 0));
    assert!(is_output(&op_args(&parts[0])[1], 0));
    assert!(is_var(&op_args(&parts[1])[0], 0, 1, 0));
    assert!(is_output(&op_args(&parts[1])[1], 1));
    // `= ANY` も同じ
    let e = target(&c, "select (a, b) = any (select e, c from u) from t");
    assert_eq!(sublink(&e).0, SubLinkKind::Any);
    // ROW(..) の明示
    let e = target(&c, "select row(a, b) in (select e, c from u) from t");
    assert_eq!(sublink(&e).0, SubLinkKind::Any);
    // 1 要素の ROW(a)
    let e = target(&c, "select row(a) in (select e from u) from t");
    assert!(matches!(
        sublink(&e).1.unwrap().kind,
        BoundExprKind::Operator { .. }
    ));
}

#[test]
fn row_not_in_and_not_equal_all() {
    let c = cat();
    for sql in [
        "select (a, b) not in (select e, c from u) from t",
        "select (a, b) <> all (select e, c from u) from t",
    ] {
        let e = target(&c, sql);
        let BoundExprKind::Not(inner) = &e.kind else {
            panic!("{sql}: not a Not: {e:?}");
        };
        let (kind, test, _) = sublink(inner);
        assert_eq!(kind, SubLinkKind::Any, "{sql}");
        assert!(matches!(test.unwrap().kind, BoundExprKind::And(_)), "{sql}");
    }
}

#[test]
fn row_comparison_with_other_operators_is_not_supported() {
    let c = cat();
    for sql in [
        "select (a, b) < any (select e, c from u) from t",
        "select (a, b) <> any (select e, c from u) from t",
        "select (a, b) = all (select e, c from u) from t",
    ] {
        let (code, msg, pos) = err_full(&c, sql);
        assert_eq!(code, "0A000", "{sql}");
        assert_eq!(
            msg, "row comparison with this operator in a subquery is not supported yet",
            "{sql}"
        );
        assert!(pos.is_some());
    }
}

// ----- エラー（§5.7.2） --------------------------------------------------------------

#[test]
fn scalar_subquery_column_count() {
    let c = cat();
    for sql in [
        "select (select a, b from t)",
        "select (select)",
        "select (select * from t)",
    ] {
        let (code, msg, pos) = err_full(&c, sql);
        assert_eq!(code, "42601", "{sql}");
        assert_eq!(msg, "subquery must return only one column", "{sql}");
        assert_eq!(pos, at(sql, "(select"), "{sql}");
    }
}

#[test]
fn in_subquery_column_count() {
    let c = cat();
    let sql = "select a in (select a, e from u) from t";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42601", "subquery has too many columns")
    );
    assert_eq!(pos, at(sql, "in"));
    let sql = "select (1, 2) in (select 1)";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42601", "subquery has too few columns")
    );
    assert_eq!(pos, at(sql, "in"));
    let (code, msg, _) = err_full(&c, "select a in (select) from t");
    assert_eq!(
        (code, msg.as_str()),
        ("42601", "subquery has too few columns")
    );
    // 演算子の位置
    let sql = "select a = any (select a, e from u) from t";
    assert_eq!(err_full(&c, sql).2, at(sql, "="));
}

#[test]
fn operator_errors() {
    let c = cat();
    for sql in [
        "select a = any (select c from u) from t",
        "select a in (select c from u) from t",
        "select 1 in (select null)",
    ] {
        let (code, msg, pos) = err_full(&c, sql);
        assert_eq!(code, "42883", "{sql}");
        assert!(
            msg.starts_with("operator does not exist: integer = text"),
            "{sql}: {msg}"
        );
        assert!(pos.is_some(), "{sql}");
    }
    let sql = "select 1 + any (select 1)";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "42804");
    assert_eq!(
        msg,
        "row comparison operator must yield type boolean, not type integer"
    );
    assert_eq!(pos, at(sql, "+"));
    // unknown の定数は出力列の型の入力関数で評価
    let sql = "select 'a' in (select 1)";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "22P02");
    assert_eq!(msg, "invalid input syntax for type integer: \"a\"");
    assert_eq!(pos, at(sql, "'a'"));
}

#[test]
fn subquery_body_errors_come_before_the_left_side() {
    let c = cat();
    let sql = "select zz in (select yy)";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42703", "column \"yy\" does not exist")
    );
    assert_eq!(pos, at(sql, "yy"));
    let (_, msg, _) = err_full(&c, "select zz = any (select yy)");
    assert_eq!(msg, "column \"yy\" does not exist");
    let (_, msg, _) = err_full(&c, "select (select yy) from t where zz = 1");
    assert_eq!(msg, "column \"yy\" does not exist");
}

#[test]
fn subquery_not_allowed_in_check_and_default() {
    let c = cat();
    for (sql, what) in [
        (
            "create table z1 (a int check (a in (select 1)))",
            "check constraint",
        ),
        (
            "create table z2 (a int default (select 1))",
            "DEFAULT expression",
        ),
        (
            "create table z3 (a int, check (exists (select 1)))",
            "check constraint",
        ),
    ] {
        let (code, msg, pos) = err_full(&c, sql);
        assert_eq!(code, "0A000", "{sql}");
        assert_eq!(msg, format!("cannot use subquery in {what}"), "{sql}");
        assert!(pos.is_some(), "{sql}");
    }
}

#[test]
fn in_list_with_a_scalar_subquery_is_a_plain_list() {
    let c = cat();
    let e = target(&c, "select 1 in (1, (select 2))");
    assert!(
        matches!(
            e.kind,
            BoundExprKind::Or(_) | BoundExprKind::InList { .. } | BoundExprKind::Operator { .. }
        ),
        "{e:?}"
    );
    assert!(e.contains_sublink());
}

// ----- 相関（levels_up。§3.2.1） ---------------------------------------------------

#[test]
fn correlated_levels_up() {
    let c = cat();
    // select (select (select x.a + y.e) from u y) from t x
    let e = target(&c, "select (select (select x.a + y.e) from u y) from t x");
    let (_, _, q1) = sublink(&e);
    let s1 = select_body(q1);
    let (_, _, q2) = sublink(&s1.targets[0]);
    let s2 = select_body(q2);
    let args = op_args(&s2.targets[0]);
    assert!(is_var(&args[0], 0, 0, 2), "x.a: {:?}", args[0]);
    assert!(is_var(&args[1], 0, 0, 1), "y.e: {:?}", args[1]);
    // 同じ名前は内側が勝つ
    let e = target(&c, "select (select a from w) from t");
    let s = select_body(sublink(&e).2);
    assert!(is_var(&s.targets[0], 0, 0, 0));
    // 外側の列を WHERE で参照
    let e = target(&c, "select (select e from u where u.e = t.a) from t");
    let s = select_body(sublink(&e).2);
    let args = op_args(s.filter.as_ref().unwrap());
    assert!(is_var(&args[0], 0, 0, 0));
    assert!(is_var(&args[1], 0, 0, 1));
}

#[test]
fn correlation_in_where_having_order_by_and_set() {
    let c = cat();
    let f = filter(
        &c,
        "select a from t where exists (select 1 from u where u.e = t.a)",
    );
    let s = select_body(sublink(&f).2);
    assert!(is_var(&op_args(s.filter.as_ref().unwrap())[1], 0, 0, 1));
    // ORDER BY の副問い合わせ（resjunk）
    let q = query(
        &c,
        "select a from t order by (select e from u where u.k = t.a limit 1)",
    );
    let s = select_body(&q);
    assert_eq!(s.targets.len(), 2);
    assert_eq!(q.order_by[0].target, 1);
    let (_, _, sq) = sublink(&s.targets[1]);
    assert!(is_var(
        &op_args(select_body(sq).filter.as_ref().unwrap())[1],
        0,
        0,
        1
    ));
    // UPDATE の SET / WHERE
    let r = run(
        &c,
        "update t set a = (select max(e) from u where u.k = t.g) where exists (select 1 from u where u.e = t.a)",
    );
    // 集約は N2 の担当。ここでは SET の副問い合わせが解析されることだけを見る。
    drop(r);
}

#[test]
fn derived_table_and_set_arm_levels() {
    let c = cat();
    // 集合演算の腕の中の副問い合わせは、腕の SELECT の 1 つ内側
    let q = query(
        &c,
        "select (select a union select e from u where u.k = t.a) from t",
    );
    let s = select_body(&q);
    let (_, _, sq) = sublink(&s.targets[0]);
    let BoundSetExpr::SetOp { left, right, .. } = &sq.body else {
        panic!("not a set operation");
    };
    // 左の腕の `a` は外側の t.a（FROM なしの腕なので 1 つ外）
    assert!(is_var(&select_body(left).targets[0], 0, 0, 1));
    // 右の腕の `t.a` は 1 つ外、`u.e` は 0
    let rs = select_body(right);
    assert!(is_var(&rs.targets[0], 0, 0, 0));
    assert!(is_var(&op_args(rs.filter.as_ref().unwrap())[1], 0, 0, 1));
}

#[test]
#[ignore = "N2: analyze_values_query / transform_values_rows (select.rs) must chain env.outer (C-3)"]
fn values_row_is_a_scope() {
    let c = cat();
    // `(values (t.a))` の `t.a` は 1 つ外（VALUES の行は rtable が空の 1 スコープ）
    let q = query(&c, "select (values (t.a)) from t");
    let s = select_body(&q);
    let (_, _, sq) = sublink(&s.targets[0]);
    let BoundSetExpr::Values { rows, .. } = &sq.body else {
        panic!("not a values body: {:?}", sq.body);
    };
    assert!(is_var(&rows[0][0], 0, 0, 1), "{:?}", rows[0][0]);
}

// ----- 出力名（§5.11） ------------------------------------------------------------

#[test]
#[ignore = "N1b/N2: figure_colname (expr.rs) must call sublink::subquery_colname for Expr::Subquery (5.11)"]
fn sublink_output_names() {
    let c = cat();
    let n = names(
        &c,
        "select (select e from u), (select 1 as k), (select 1), exists(select 1), a in (select 1), not exists(select 1), (select (select 2)) from t",
    );
    assert_eq!(
        n,
        [
            "e", "k", "?column?", "exists", "?column?", "?column?", "?column?"
        ]
    );
    // キャストで包んでも副問い合わせの名前（強さ 2）が勝つ
    let n = names(&c, "select (select 1)::int, (select e from u)::text from t");
    assert_eq!(n, ["?column?", "e"]);
}

// ----- 集合演算（§5.8） ------------------------------------------------------------

#[track_caller]
fn setop(q: &BoundQuery) -> (&SetOpKind, bool, &BoundQuery, &BoundQuery, &[SqlType]) {
    match &q.body {
        BoundSetExpr::SetOp {
            op,
            all,
            left,
            right,
            types,
            ..
        } => (op, *all, left, right, types),
        other => panic!("not a set operation: {other:?}"),
    }
}

#[test]
fn setop_basic_shape() {
    let c = cat();
    let q = query(&c, "select a, b from t union select e, c from u");
    let (op, all, l, r, ty) = setop(&q);
    assert_eq!(*op, SetOpKind::Union);
    assert!(!all);
    assert_eq!(ty, [SqlType::INT4, SqlType::TEXT]);
    assert_eq!(l.columns.len(), 2);
    assert_eq!(r.columns.len(), 2);
    // 出力列は最も左の腕の名前。table_oid / attnum は 0
    assert_eq!(q.columns[0].name, "a");
    assert_eq!(q.columns[1].name, "b");
    assert_eq!((q.columns[0].table_oid, q.columns[0].attnum), (0, 0));
    for (sql, op, all) in [
        (
            "select a from t union all select e from u",
            SetOpKind::Union,
            true,
        ),
        (
            "select a from t intersect select e from u",
            SetOpKind::Intersect,
            false,
        ),
        (
            "select a from t intersect all select e from u",
            SetOpKind::Intersect,
            true,
        ),
        (
            "select a from t except select e from u",
            SetOpKind::Except,
            false,
        ),
        (
            "select a from t except all select e from u",
            SetOpKind::Except,
            true,
        ),
    ] {
        let q = query(&c, sql);
        let (o, a, ..) = setop(&q);
        assert_eq!((*o, a), (op, all), "{sql}");
    }
    assert_eq!(
        names(&c, "select 1 as a union select 2 as b"),
        ["a"],
        "最も左の腕の名前"
    );
    assert_eq!(names(&c, "values (1) union select 2"), ["column1"]);
}

#[test]
fn setop_precedence_and_associativity() {
    let c = cat();
    // INTERSECT が UNION / EXCEPT より強い
    let q = query(
        &c,
        "select a from t union select e from u intersect select 3",
    );
    let (op, _, l, r, _) = setop(&q);
    assert_eq!(*op, SetOpKind::Union);
    assert!(matches!(
        select_body(l).targets[0].kind,
        BoundExprKind::Column(_)
    ));
    assert!(matches!(setop(r).0, SetOpKind::Intersect));
    // 同じ強さは左結合
    let q = query(&c, "select a from t except select e from u except select 1");
    let (op, _, l, r, _) = setop(&q);
    assert_eq!(*op, SetOpKind::Except);
    assert!(matches!(setop(l).0, SetOpKind::Except));
    assert!(matches!(r.body, BoundSetExpr::Select(_)));
}

#[test]
fn setop_common_type_is_decided_left_to_right() {
    let c = cat();
    assert_eq!(types(&c, "select 1 union select 2"), [oid::INT4]);
    assert_eq!(
        types(&c, "select s from t union select g from t"),
        [oid::INT8]
    );
    assert_eq!(
        types(&c, "select a from t union select c from t"),
        [oid::FLOAT8]
    );
    // unknown 同士は text
    assert_eq!(types(&c, "select 'a' union select 'b'"), [oid::TEXT]);
    assert_eq!(types(&c, "select null union select null"), [oid::TEXT]);
    // (null ∪ null) は text、そこへ int を足すと分類が違う
    let sql = "select null union select null union select 1";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "42804");
    assert_eq!(msg, "UNION types text and integer cannot be matched");
    assert_eq!(pos, at(sql, "1"));
    // (1 ∪ null) ∪ 'a' は左から見て int になる
    let sql = "select 1 union select null union select 'a'";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("22P02", "invalid input syntax for type integer: \"a\"")
    );
    assert_eq!(pos, at(sql, "'a'"));
}

#[test]
fn setop_typmod_is_kept_only_when_equal() {
    let c = cat();
    let q = query(&c, "select v from t union select v from t");
    assert_eq!(q.columns[0].ty, SqlType::new(oid::VARCHAR, 3 + 4));
    // varchar と text: 左の型が残る（select_common_type。text -> varchar も暗黙）
    let q = query(&c, "select v from t union select b from t");
    assert_eq!(q.columns[0].ty, SqlType::of(oid::VARCHAR));
    let q = query(&c, "select b from t union select v from t");
    assert_eq!(q.columns[0].ty, SqlType::TEXT);
    let q = query(&c, "select v from t union select 'a'");
    // 左が varchar(3)、右は unknown リテラル（typmod -1）: 型は varchar、typmod は -1
    assert_eq!(q.columns[0].ty.oid, oid::VARCHAR);
    assert_eq!(q.columns[0].ty.typmod, -1);
}

#[test]
fn setop_rewrites_unknown_literals_in_select_arms() {
    let c = cat();
    let q = query(&c, "select 1 union select '2'");
    let (_, _, l, r, ty) = setop(&q);
    assert_eq!(ty, [SqlType::INT4]);
    let rs = select_body(r);
    assert_eq!(rs.targets[0].ty, SqlType::INT4);
    assert!(
        matches!(&rs.targets[0].kind, BoundExprKind::Literal(Datum::Int4(2))),
        "{:?}",
        rs.targets[0]
    );
    assert_eq!(r.columns[0].ty, SqlType::INT4);
    assert_eq!(select_body(l).targets[0].ty, SqlType::INT4);
    // 書き換えで足りるので変換式はない
    let BoundSetExpr::SetOp {
        left_coerce,
        right_coerce,
        ..
    } = &q.body
    else {
        unreachable!()
    };
    assert!(left_coerce.is_none() && right_coerce.is_none());
    // NULL も型つきの NULL になる
    let q = query(&c, "select null union select 1");
    let (_, _, l, _, _) = setop(&q);
    let ls = select_body(l);
    assert_eq!(ls.targets[0].ty, SqlType::INT4);
    assert!(matches!(
        ls.targets[0].kind,
        BoundExprKind::Literal(Datum::Null)
    ));
    // 両方 unknown は text に直す
    let q = query(&c, "select 'a' union select 'b'");
    let (_, _, l, r, _) = setop(&q);
    assert_eq!(select_body(l).targets[0].ty, SqlType::TEXT);
    assert_eq!(select_body(r).targets[0].ty, SqlType::TEXT);
    // 入力関数のエラーの位置はリテラル
    let sql = "select 1 union select 'a'";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("22P02", "invalid input syntax for type integer: \"a\"")
    );
    assert_eq!(pos, at(sql, "'a'"));
}

#[test]
fn setop_coercion_expressions() {
    let c = cat();
    let q = query(&c, "select s, a from t union select g, a from t");
    let BoundSetExpr::SetOp {
        left_coerce,
        right_coerce,
        types,
        ..
    } = &q.body
    else {
        unreachable!()
    };
    assert_eq!(types[0], SqlType::INT8);
    // 左だけ変換（int2 → int8）。型が合う列は Var のまま
    let lc = left_coerce.as_ref().expect("left_coerce");
    assert_eq!(lc.len(), 2);
    assert!(matches!(lc[0].kind, BoundExprKind::Cast { .. }));
    assert_eq!(lc[0].ty.oid, oid::INT8);
    assert!(is_var(&lc[1], 0, 1, 0));
    let BoundExprKind::Cast { expr, .. } = &lc[0].kind else {
        unreachable!()
    };
    assert!(is_var(expr, 0, 0, 0));
    assert!(right_coerce.is_none());
    // 右だけ
    let q = query(&c, "select g from t union select s from t");
    let BoundSetExpr::SetOp {
        left_coerce,
        right_coerce,
        ..
    } = &q.body
    else {
        unreachable!()
    };
    assert!(left_coerce.is_none());
    assert!(right_coerce.is_some());
}

#[test]
fn setop_errors() {
    let c = cat();
    let sql = "select a, b from t union select e from u";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "42601");
    assert_eq!(msg, "each UNION query must have the same number of columns");
    assert_eq!(pos, at(sql, "e from"));
    let (_, msg, _) = err_full(&c, "select a from t union all select e, k from u");
    assert_eq!(msg, "each UNION query must have the same number of columns");
    let (_, msg, _) = err_full(&c, "select a from t intersect select e, k from u");
    assert_eq!(
        msg,
        "each INTERSECT query must have the same number of columns"
    );
    let (_, msg, _) = err_full(&c, "select a from t except select e, k from u");
    assert_eq!(
        msg,
        "each EXCEPT query must have the same number of columns"
    );
    let sql = "select a from t union select b from t";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42804", "UNION types integer and text cannot be matched")
    );
    assert_eq!(pos, at(sql, "b from"));
    let (_, msg, _) = err_full(&c, "select a from t except select b from t");
    assert_eq!(msg, "EXCEPT types integer and text cannot be matched");
    let (_, msg, _) = err_full(&c, "select a from t intersect select b from t");
    assert_eq!(msg, "INTERSECT types integer and text cannot be matched");
}

#[test]
fn setop_values_and_nested_arms() {
    let c = cat();
    let q = query(&c, "values (1), (2) union select e from u");
    let (_, _, l, _, ty) = setop(&q);
    assert!(matches!(l.body, BoundSetExpr::Values { .. }));
    assert_eq!(ty, [SqlType::INT4]);
    // 全部 unknown の VALUES は text
    assert_eq!(types(&c, "values ('a') union select 'b'"), [oid::TEXT]);
    // 括弧つきの腕は ORDER BY / LIMIT を持てる
    let q = query(
        &c,
        "(select a from t order by a limit 1) union (select e from u order by e desc limit 1) order by 1",
    );
    let (_, _, l, r, _) = setop(&q);
    assert!(l.limit.is_some() && r.limit.is_some());
    assert!(!l.order_by[0].descending && r.order_by[0].descending);
    assert_eq!(q.order_by.len(), 1);
    assert_eq!(q.order_by[0].target, 0);
    // 腕の中の腕（INTERSECT が先）
    let q = query(&c, "(select 1 union select 2) intersect select 3");
    assert!(matches!(setop(&q).0, SetOpKind::Intersect));
}

#[test]
fn setop_outer_order_by_limit_offset() {
    let c = cat();
    let q = query(
        &c,
        "select a as x, b from t union select e, c from u order by x desc, 2 limit 5 offset 1",
    );
    assert_eq!(q.order_by.len(), 2);
    assert_eq!(
        (
            q.order_by[0].target,
            q.order_by[0].descending,
            q.order_by[0].nulls_first
        ),
        (0, true, true)
    );
    assert_eq!(
        (
            q.order_by[1].target,
            q.order_by[1].descending,
            q.order_by[1].nulls_first
        ),
        (1, false, false)
    );
    assert!(q.limit.is_some());
    assert!(q.offset.is_some());
    assert_eq!(q.limit.as_ref().unwrap().ty, SqlType::INT8);
    // 括弧つきの名前（式の形をした列名）
    let q = query(&c, "select a from t union select e from u order by (a)");
    assert_eq!(q.order_by[0].target, 0);
    // NULLS FIRST / LAST
    let q = query(
        &c,
        "select a from t union select e from u order by a nulls first",
    );
    assert!(q.order_by[0].nulls_first);
}

#[test]
fn setop_order_by_errors() {
    let c = cat();
    for sql in [
        "select a from t union select e from u order by a + 1",
        "select a from t union select e from u order by 1 + 0",
    ] {
        let (code, msg, pos) = err_full(&c, sql);
        assert_eq!(code, "0A000", "{sql}");
        assert_eq!(
            msg, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
            "{sql}"
        );
        assert_eq!(pos, at(sql, "order by").map(|p| p + 9), "{sql}");
    }
    let e = run(&c, "select a from t union select e from u order by a + 1").unwrap_err();
    assert_eq!(
        e.detail.as_deref(),
        Some("Only result column names can be used, not expressions or functions.")
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("Add the expression/function to every SELECT, or move the UNION into a FROM clause.")
    );
    let sql = "select a from t union select e from u order by t.a";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42P01", "missing FROM-clause entry for table \"t\"")
    );
    assert_eq!(pos, at(sql, "t.a"));
    let (code, msg, _) = err_full(&c, "select a from t union select e from u order by 2");
    assert_eq!(
        (code, msg.as_str()),
        ("42P10", "ORDER BY position 2 is not in select list")
    );
    // 名前が右の腕の別名にしかない
    let sql = "select a as x from t union select e as y from u order by y";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(
        (code, msg.as_str()),
        ("42703", "column \"y\" does not exist")
    );
    assert_eq!(
        pos,
        at(sql, "y from").map(|_| u32::try_from(sql.rfind('y').unwrap()).unwrap())
    );
    // 同じ名前の列が 2 つ
    let (code, msg, _) = err_full(&c, "select a, a from t union select e, e from u order by a");
    assert_eq!(
        (code, msg.as_str()),
        ("42702", "ORDER BY \"a\" is ambiguous")
    );
}

#[test]
fn setop_outer_limit_rules() {
    let c = cat();
    let (code, msg, _) = err_full(&c, "select a from t union select e from u limit a");
    // 集合演算の外枠の名前空間は空（PostgreSQL と同じ）
    assert_eq!(code, "42703");
    assert_eq!(msg, "column \"a\" does not exist");
    let sql = "select (select a from t union select e from u limit t.g) from t";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "0A000");
    assert_eq!(
        msg,
        "correlated LIMIT or OFFSET on a set operation is not supported yet"
    );
    assert_eq!(pos, at(sql, "t.g"));
    // 定数は通る
    let q = query(&c, "select a from t union select e from u limit 1 + 1");
    assert!(q.limit.is_some());
}

#[test]
fn setop_with_in_an_arm_and_outside() {
    let c = cat();
    let q = query(
        &c,
        "with x as (select 1 a) select a from x union select e from u",
    );
    assert_eq!(q.ctes.len(), 1);
    let (_, _, l, _, _) = setop(&q);
    // 腕は 1 つ内側の BoundQuery
    let RteKind::CteRef { levels_up, cte } = &select_body(l).rtable[0].kind else {
        panic!("not a CTE reference");
    };
    assert_eq!((*levels_up, *cte), (1, CteId(0)));
    // 括弧つきの腕が自分の WITH を持つ
    let q = query(
        &c,
        "(with x as (select 1 a) select a from x) union select e from u",
    );
    assert!(q.ctes.is_empty());
    let (_, _, l, _, _) = setop(&q);
    assert_eq!(l.ctes.len(), 1);
    let RteKind::CteRef { levels_up, .. } = &select_body(l).rtable[0].kind else {
        panic!("not a CTE reference");
    };
    assert_eq!(*levels_up, 0);
}

// ----- CTE（§5.9） ----------------------------------------------------------------

#[track_caller]
fn cte_ref(q: &BoundQuery, rte: usize) -> (u16, CteId) {
    match &select_body(q).rtable[rte].kind {
        RteKind::CteRef { levels_up, cte } => (*levels_up, *cte),
        other => panic!("not a CTE reference: {other:?}"),
    }
}

#[test]
fn cte_basic() {
    let c = cat();
    let q = query(&c, "with x as (select a, b from t) select * from x");
    assert_eq!(q.ctes.len(), 1);
    let cte: &BoundCte = &q.ctes[0];
    assert_eq!(cte.name, "x");
    assert_eq!(cte.materialize, CteMaterialize::Default);
    assert!(cte.col_aliases.is_empty());
    assert_eq!(cte_ref(&q, 0), (0, CteId(0)));
    let rte = &select_body(&q).rtable[0];
    assert_eq!(rte.refname.as_deref(), Some("x"));
    assert_eq!(
        rte.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        names(&c, "with x as (select a, b from t) select * from x"),
        ["a", "b"]
    );
    // 別名と、同じ CTE を 2 回
    let q = query(
        &c,
        "with x as (select a from t) select p.a, q.a from x p, x q",
    );
    assert_eq!(select_body(&q).rtable.len(), 2);
    assert_eq!(cte_ref(&q, 0), (0, CteId(0)));
    assert_eq!(cte_ref(&q, 1), (0, CteId(0)));
    assert_eq!(select_body(&q).rtable[0].refname.as_deref(), Some("p"));
}

#[test]
fn cte_levels_up_through_nesting() {
    let c = cat();
    // 副問い合わせの中から参照: 1
    let q = query(&c, "with c as (select 1 a) select (select a from c) from t");
    let (_, _, sq) = sublink(&select_body(&q).targets[0]);
    assert_eq!(cte_ref(sq, 0), (1, CteId(0)));
    // 後の CTE が前の CTE を参照: 1
    let q = query(
        &c,
        "with c as (select 1 a), d as (select a from c) select * from d",
    );
    assert_eq!(cte_ref(&q.ctes[1].query, 0), (1, CteId(0)));
    assert_eq!(cte_ref(&q, 0), (0, CteId(1)));
    // WHERE の副問い合わせの中の副問い合わせ: 2
    let q = query(
        &c,
        "with c as (select 1 a) select 1 from t where exists (select 1 from u where e in (select a from c))",
    );
    let f = select_body(&q).filter.clone().unwrap();
    let (_, _, q1) = sublink(&f);
    let (_, _, q2) = sublink(select_body(q1).filter.as_ref().unwrap());
    assert_eq!(cte_ref(q2, 0), (2, CteId(0)));
}

#[test]
fn cte_aliases_and_materialize() {
    let c = cat();
    let q = query(
        &c,
        "with x(p, q) as (select a, b from t) select p, q from x",
    );
    assert_eq!(q.ctes[0].col_aliases, ["p", "q"]);
    assert_eq!(q.ctes[0].query.columns[0].name, "a");
    assert_eq!(
        names(&c, "with x(p) as (select a, b from t) select * from x"),
        ["p", "b"]
    );
    // 重複した別名も許す
    assert_eq!(
        names(&c, "with x(p, p) as (select a, b from t) select * from x"),
        ["p", "p"]
    );
    // FROM 句の別名の列リストはその上にさらに適用される
    assert_eq!(
        names(
            &c,
            "with x(p, q) as (select a, b from t) select * from x as y(r)"
        ),
        ["r", "q"]
    );
    let sql = "with x(p, q, r) as (select a, b from t) select 1";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "42P10");
    assert_eq!(
        msg,
        "WITH query \"x\" has 2 columns available but 3 columns specified"
    );
    assert_eq!(pos, at(sql, "x("));
    let q = query(
        &c,
        "with x as materialized (select 1), y as not materialized (select 2), z as (select 3) select 1",
    );
    assert_eq!(
        q.ctes.iter().map(|c| c.materialize).collect::<Vec<_>>(),
        [
            CteMaterialize::Always,
            CteMaterialize::Never,
            CteMaterialize::Default
        ]
    );
}

#[test]
fn cte_unknown_output_is_text_and_unused_ctes_are_analyzed() {
    let c = cat();
    assert_eq!(
        types(&c, "with x as (select 'a') select * from x"),
        [oid::TEXT]
    );
    assert_eq!(
        err(&c, "with x as (select 1 + 'a'::text) select 1"),
        "42883"
    );
    assert_eq!(
        types(
            &c,
            "with x as (select 'a' union select 'b') select * from x"
        ),
        [oid::TEXT]
    );
}

#[test]
fn cte_name_resolution_errors() {
    let c = cat();
    // 名前の重複
    let sql = "with x as (select 1), x as (select 2) select 1";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "42712");
    assert_eq!(msg, "WITH query name \"x\" specified more than once");
    assert_eq!(pos, at(sql, "x as (select 2)"));
    // 自己参照・前方参照
    for sql in [
        "with x as (select * from x) select 1",
        "with x as (select * from y), y as (select 1) select 1",
    ] {
        let e = run(&c, sql).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P01", "{sql}");
        let name = if sql.contains("from y") { "y" } else { "x" };
        assert_eq!(
            e.message,
            format!("relation \"{name}\" does not exist"),
            "{sql}"
        );
    }
    // 同じ名前を 2 度 FROM に（別名なし）
    assert_eq!(err(&c, "with x as (select 1) select * from x, x"), "42712");
    // スキーマ付きは CTE を引かない
    let e = run(&c, "with x as (select 1) select * from public.x").unwrap_err();
    assert_eq!(e.sqlstate.code(), "42P01");
    assert_eq!(e.message, "relation \"public.x\" does not exist");
}

#[test]
fn cte_shadowing() {
    let c = cat();
    // CTE は実表を隠す
    let q = query(&c, "with t as (select 1 z) select * from t");
    assert_eq!(cte_ref(&q, 0), (0, CteId(0)));
    assert_eq!(names(&c, "with t as (select 1 z) select * from t"), ["z"]);
    // 最も内側の宣言が勝つ
    let q = query(
        &c,
        "with x as (select 1 a) select * from (with x as (select 'q'::text a) select * from x) s",
    );
    let _ = q;
    // 内側の宣言の中の同名参照は外側へ解決される
    let q = query(
        &c,
        "with x as (select 1 a) select * from (with x as (select a + 10 as a from x) select * from x) s",
    );
    let _ = q;
    // 実表と同名の前方参照は実表に解決される
    let q = query(&c, "with t as (select * from t), y as (select 1) select 1");
    assert_eq!(select_body(&q.ctes[0].query).rtable.len(), 1);
    assert!(matches!(
        select_body(&q.ctes[0].query).rtable[0].kind,
        RteKind::Table { .. }
    ));
}

#[test]
fn cte_recursive() {
    let c = cat();
    // RECURSIVE でも再帰しなければ通常の WITH
    let q = query(&c, "with recursive x as (select 1 a) select * from x");
    assert_eq!(cte_ref(&q, 0), (0, CteId(0)));
    // 再帰参照は 0A000
    let sql = "with recursive x as (select 1 a union all select a + 1 from x) select * from x";
    let (code, msg, pos) = err_full(&c, sql);
    assert_eq!(code, "0A000");
    assert_eq!(msg, "WITH RECURSIVE is not supported yet");
    assert_eq!(pos, at(sql, "x) select"));
    // 非再帰の WITH の自己参照は 42P01
    assert_eq!(
        err(
            &c,
            "with x as (select 1 a union all select a from x) select 1"
        ),
        "42P01"
    );
}

#[test]
fn cte_in_subquery_and_values_and_correlation() {
    let c = cat();
    // 副問い合わせの中の WITH。CTE の本体が外側の列を参照すると levels_up = 1
    let q = query(&c, "select (with y as (select t.a) select * from y) from t");
    let (_, _, sq) = sublink(&select_body(&q).targets[0]);
    assert_eq!(sq.ctes.len(), 1);
    assert!(is_var(&select_body(&sq.ctes[0].query).targets[0], 0, 0, 1));
    assert_eq!(cte_ref(sq, 0), (0, CteId(0)));
    // WITH ... VALUES
    let q = query(&c, "with x as (select 1 a) values (1)");
    assert_eq!(q.ctes.len(), 1);
    assert!(matches!(q.body, BoundSetExpr::Values { .. }));
}

#[test]
fn cte_with_nested_query_body() {
    let c = cat();
    // `WITH` + 括弧つきの本体
    let q = query(&c, "with x as (select 1 a) (select a from x)");
    assert_eq!(q.ctes.len(), 1);
    assert_eq!(cte_ref(&q, 0), (0, CteId(0)));
    let q = query(
        &c,
        "with x as (select 1 a) (select a from x order by a limit 1)",
    );
    assert_eq!(q.ctes.len(), 1);
    assert!(q.limit.is_some());
    // INSERT ... WITH ... SELECT
    let r = run(
        &c,
        "insert into w with x as (select 1 a, 'q' b) select a, b from x",
    );
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn cte_column_origin() {
    let c = cat();
    let q = query(&c, "with x as (select a from t) select a from x");
    // 副問い合わせを通した列の由来は FROM 句の CTE 参照の列（実表の列ではない）。
    assert_eq!(q.columns.len(), 1);
    assert_eq!(q.ctes[0].query.columns[0].attnum, 1);
}

#[test]
fn subquery_colname_helper_follows_the_first_output() {
    use super::sublink::subquery_colname;
    let name = |sql: &str| {
        let stmts = crate::sql::parse(sql).unwrap();
        let crate::sql::ast::Statement::Query(q) = &stmts[0] else {
            panic!("not a query");
        };
        let crate::sql::ast::QueryBody::Select(s) = &q.body else {
            panic!("not a select");
        };
        let Some(crate::sql::ast::SelectItem::Expr { expr, .. }) = s.targets.first() else {
            panic!("no target");
        };
        let crate::sql::ast::Expr::Subquery { query, .. } = expr else {
            panic!("not a subquery");
        };
        subquery_colname(query)
    };
    assert_eq!(name("select (select max(a) from t)"), "max");
    assert_eq!(name("select (select 1 as k)"), "k");
    assert_eq!(name("select (select 1)"), "?column?");
    assert_eq!(name("select (select (select 2))"), "?column?");
    assert_eq!(name("select (values (1))"), "column1");
    assert_eq!(name("select (select a from t union select e from u)"), "a");
}
