//! 型をまたぐ式の解決のテスト（N1b。`m4/09` §4.4・§6.1・§4.1・§6.4）。

use super::bound::{BoundExpr, BoundExprKind};
use super::tests::{err, select, types};
use crate::catalog::CastMethod;
use crate::catalog::fake::{FakeCatalog as MemoryCatalog, TableBuilder};
use crate::sql::ast::SessionValueKind;
use crate::types::{Datum, Oid, SqlType, oid};

fn cat() -> MemoryCatalog {
    let mut c = MemoryCatalog::new("postgres");
    c.add(
        &TableBuilder::new("x")
            .column("numeric_col", SqlType::NUMERIC)
            .column("int8_col", SqlType::INT8)
            .column("i", SqlType::INT4)
            .column("ts_col", SqlType::TIMESTAMP)
            .column("date_col", SqlType::DATE)
            .column("char_col", SqlType::of(oid::BPCHAR))
            .column("text_col", SqlType::TEXT)
            .column("name_col", SqlType::NAME)
            .column("n", SqlType::new(oid::NUMERIC, ((10 << 16) | 2) + 4)),
    );
    c
}

#[track_caller]
fn first(c: &MemoryCatalog, sql: &str) -> BoundExpr {
    select(c, sql).targets.into_iter().next().expect(sql)
}

/// 演算子の (左, 右, 結果) の型。
#[track_caller]
fn op_of(e: &BoundExpr) -> (Option<Oid>, Oid, Oid) {
    match &e.kind {
        BoundExprKind::Operator { op, .. } => (op.left, op.right, op.result),
        other => panic!("not an operator: {other:?}"),
    }
}

#[track_caller]
fn arg(e: &BoundExpr, i: usize) -> &BoundExpr {
    match &e.kind {
        BoundExprKind::Operator { args, .. } => &args[i],
        other => panic!("not an operator: {other:?}"),
    }
}

fn is_cast(e: &BoundExpr, to: Oid) -> bool {
    matches!(e.kind, BoundExprKind::Cast { .. }) && e.ty.oid == to
}

#[test]
fn decimal_literal_is_numeric() {
    let c = cat();
    let e = first(&c, "SELECT 1.5");
    assert_eq!(e.ty, SqlType::NUMERIC);
    assert!(matches!(e.kind, BoundExprKind::Literal(Datum::Numeric(_))));
    for q in ["SELECT .5", "SELECT 1.", "SELECT 1e3", "SELECT 1.0e-3"] {
        assert_eq!(types(&c, q), vec![oid::NUMERIC], "{q}");
    }
    // i64 を超える整数は numeric。
    assert_eq!(types(&c, "SELECT 9223372036854775808"), vec![oid::NUMERIC]);
    assert_eq!(types(&c, "SELECT 2147483648"), vec![oid::INT8]);
    assert_eq!(err(&c, "SELECT 1e200000"), "22003");
}

#[test]
fn numeric_plus_int_resolves_to_numeric() {
    let c = cat();
    let e = first(&c, "SELECT 1.5 + 1");
    assert_eq!(op_of(&e), (Some(oid::NUMERIC), oid::NUMERIC, oid::NUMERIC));
    assert_eq!(e.ty.oid, oid::NUMERIC);
    // 1 は int4 のリテラルからの暗黙キャスト（numeric）。
    assert_eq!(arg(&e, 1).ty.oid, oid::NUMERIC);
}

#[test]
fn numeric_plus_float8_resolves_to_float8() {
    let c = cat();
    let e = first(&c, "SELECT 1.5 + 1::float8");
    assert_eq!(op_of(&e), (Some(oid::FLOAT8), oid::FLOAT8, oid::FLOAT8));
    assert!(is_cast(arg(&e, 0), oid::FLOAT8));
    assert!(matches!(
        arg(&e, 0).kind,
        BoundExprKind::Cast {
            implicit: true,
            method: CastMethod::Function(_),
            ..
        }
    ));
}

#[test]
fn numeric_column_equals_int_literal() {
    let c = cat();
    let e = first(&c, "SELECT numeric_col = 2 FROM x");
    assert_eq!(op_of(&e), (Some(oid::NUMERIC), oid::NUMERIC, oid::BOOL));
    assert_eq!(e.ty, SqlType::BOOL);
    assert_eq!(arg(&e, 1).ty.oid, oid::NUMERIC);
}

#[test]

fn timestamp_vs_timestamptz() {
    let c = cat();
    let e = first(&c, "SELECT ts_col < current_timestamp FROM x");
    assert_eq!(
        op_of(&e),
        (Some(oid::TIMESTAMPTZ), oid::TIMESTAMPTZ, oid::BOOL)
    );
    assert!(matches!(
        arg(&e, 0).kind,
        BoundExprKind::Cast {
            method: CastMethod::Env(_),
            ..
        }
    ));
    assert_eq!(arg(&e, 0).ty.oid, oid::TIMESTAMPTZ);
}

#[test]

fn date_vs_timestamp() {
    let c = cat();
    let e = first(&c, "SELECT date_col = ts_col FROM x");
    assert_eq!(op_of(&e), (Some(oid::TIMESTAMP), oid::TIMESTAMP, oid::BOOL));
    assert!(is_cast(arg(&e, 0), oid::TIMESTAMP));
}

#[test]

fn bpchar_comparisons() {
    let c = cat();
    let e = first(&c, "SELECT char_col = 'abc' FROM x");
    assert_eq!(op_of(&e), (Some(oid::BPCHAR), oid::BPCHAR, oid::BOOL));
    // リテラルは bpchar の定数になる。
    assert!(matches!(
        arg(&e, 1).kind,
        BoundExprKind::Literal(Datum::BpChar(_))
    ));
    let e = first(&c, "SELECT char_col = text_col FROM x");
    assert_eq!(op_of(&e), (Some(oid::TEXT), oid::TEXT, oid::BOOL));
    assert!(is_cast(arg(&e, 0), oid::TEXT));
    assert_eq!(
        types(&c, "SELECT char_col LIKE 'a%' FROM x"),
        vec![oid::BOOL]
    );
    assert_eq!(
        types(&c, "SELECT name_col ~ '^pg_' FROM x"),
        vec![oid::BOOL]
    );
}

#[test]

fn oid_to_regclass() {
    let c = cat();
    let e = first(&c, "SELECT i::oid::regclass FROM x");
    assert_eq!(e.ty, SqlType::REGCLASS);
}

#[test]

fn datetime_literals_are_cast_in_out() {
    let c = cat();
    // 日時のリテラルはアナライザでは評価せず、`Cast(InOut)` を作る（プランナが畳み込む）。
    for (q, t) in [
        ("SELECT date_col = '2024-01-01' FROM x", oid::DATE),
        (
            "SELECT ts_col = '2024-01-01 10:00:00' FROM x",
            oid::TIMESTAMP,
        ),
        ("SELECT '2024-01-01'::date", oid::DATE),
        (
            "SELECT '2024-01-01 10:00:00+09'::timestamptz",
            oid::TIMESTAMPTZ,
        ),
    ] {
        let e = first(&c, q);
        let target = if matches!(e.kind, BoundExprKind::Operator { .. }) {
            arg(&e, 1).clone()
        } else {
            e
        };
        assert_eq!(target.ty.oid, t, "{q}");
        match &target.kind {
            BoundExprKind::Cast {
                expr,
                method: CastMethod::InOut,
                ..
            } => assert!(matches!(expr.kind, BoundExprKind::Literal(Datum::Text(_)))),
            other => panic!("{q}: {other:?}"),
        }
    }
    assert_eq!(err(&c, "SELECT 1::date"), "42846");
}

#[test]

fn typmod_applies_to_new_types() {
    let c = cat();
    for (q, ty) in [
        (
            "SELECT '2024-01-01 10:00:00.6'::timestamp(0)",
            SqlType::new(oid::TIMESTAMP, 0),
        ),
        (
            "SELECT '2024-01-01 10:00:00.6'::timestamptz(3)",
            SqlType::new(oid::TIMESTAMPTZ, 3),
        ),
        (
            "SELECT 1.2345::numeric(5,2)",
            SqlType::new(oid::NUMERIC, ((5 << 16) | 2) + 4),
        ),
        ("SELECT 'ab'::char(5)", SqlType::new(oid::BPCHAR, 5 + 4)),
    ] {
        let e = first(&c, q);
        assert_eq!(e.ty, ty, "{q}");
        assert!(matches!(e.kind, BoundExprKind::CoerceTypmod { .. }), "{q}");
    }
}

#[test]

fn assignment_casts() {
    let c = cat();
    // numeric → int4 列、float8 → numeric 列、timestamptz → timestamp 列は代入で通る。
    for q in [
        "INSERT INTO x (i) VALUES (1.5)",
        "INSERT INTO x (numeric_col) VALUES (1::float8)",
        "INSERT INTO x (ts_col) VALUES (current_timestamp)",
        "INSERT INTO x (char_col) VALUES (12)",
        "INSERT INTO x (date_col) VALUES ('2024-01-01')",
        "INSERT INTO x (n) VALUES (1.2345)",
    ] {
        super::tests::run(&c, q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
    }
    assert_eq!(err(&c, "INSERT INTO x (i) VALUES ('a'::text)"), "42804");
    assert_eq!(err(&c, "INSERT INTO x (date_col) VALUES (1)"), "42804");
}

#[test]
fn pg_typeof_folds_to_type_name() {
    let c = cat();
    for (q, name) in [
        ("SELECT pg_typeof(1)", "integer"),
        ("SELECT pg_typeof(1.5)", "numeric"),
        ("SELECT pg_typeof('a')", "unknown"),
        ("SELECT pg_typeof(numeric_col) FROM x", "numeric"),
        ("SELECT pg_typeof(1.5 + 1::float8)", "double precision"),
    ] {
        let e = first(&c, q);
        assert_eq!(e.ty, SqlType::REGTYPE, "{q}");
        assert!(
            matches!(&e.kind, BoundExprKind::Literal(Datum::Oid(o)) if crate::analyzer::coerce::tname(*o) == name)
                || matches!(&e.kind, BoundExprKind::Case { .. }),
            "{q}"
        );
    }
    // The argument is not evaluated (`1/0` does not fail at analysis or folding).
    assert_eq!(types(&c, "SELECT pg_typeof(1/0)"), vec![oid::REGTYPE]);
}

#[test]

fn regclass_and_regtype_literals() {
    let c = cat();
    let e = first(&c, "SELECT 'x'::regclass");
    assert_eq!(e.ty, SqlType::REGCLASS);
    assert!(matches!(e.kind, BoundExprKind::Literal(Datum::Oid(_))));
    assert_eq!(err(&c, "SELECT 'no_such_table'::regclass"), "42P01");
    let e = first(&c, "SELECT 'integer'::regtype");
    assert!(matches!(e.kind, BoundExprKind::Literal(Datum::Oid(23))));
    assert_eq!(err(&c, "SELECT 'no_such_type'::regtype"), "42704");
    let e = first(&c, "SELECT '1259'::regclass");
    assert!(matches!(e.kind, BoundExprKind::Literal(Datum::Oid(1259))));
}

#[test]
fn session_value_types() {
    let c = cat();
    for (q, ty) in [
        ("SELECT current_date", SqlType::DATE),
        ("SELECT current_timestamp", SqlType::TIMESTAMPTZ),
        (
            "SELECT current_timestamp(3)",
            SqlType::new(oid::TIMESTAMPTZ, 3),
        ),
        (
            "SELECT current_timestamp(9)",
            SqlType::new(oid::TIMESTAMPTZ, 6),
        ),
        ("SELECT localtimestamp", SqlType::TIMESTAMP),
        ("SELECT localtimestamp(2)", SqlType::new(oid::TIMESTAMP, 2)),
        ("SELECT current_user", SqlType::NAME),
    ] {
        let e = first(&c, q);
        assert_eq!(e.ty, ty, "{q}");
        assert!(matches!(e.kind, BoundExprKind::SessionValue(_)), "{q}");
    }
    assert!(matches!(
        first(&c, "SELECT current_date").kind,
        BoundExprKind::SessionValue(SessionValueKind::CurrentDate)
    ));
}

#[test]
fn pg_sleep_takes_numeric_literal() {
    let c = cat();
    assert_eq!(types(&c, "SELECT pg_sleep(0.01)"), vec![oid::VOID]);
}
