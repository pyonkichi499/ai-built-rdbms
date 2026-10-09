//! 配列の式の解析のテスト（`m5/05-types-core.md` §5.4）。

use super::bound::{BoundExpr, BoundExprKind};
use super::tests::{catalog, err, select};
use crate::types::typeinfo::array_type_of;
use crate::types::{Datum, Oid, oid};

fn first(sql: &str) -> BoundExpr {
    select(&catalog(), sql)
        .targets
        .into_iter()
        .next()
        .expect(sql)
}

fn arr(elem: Oid) -> Oid {
    array_type_of(elem).expect("array type")
}

#[test]
fn array_constructor_types() {
    let e = first("SELECT ARRAY[1, 2, 3]");
    assert_eq!(e.ty.oid, arr(oid::INT4));
    assert!(matches!(
        e.kind,
        BoundExprKind::ArrayCtor { elem_type, ref elems } if elem_type == oid::INT4 && elems.len() == 3
    ));
    assert_eq!(first("SELECT ARRAY[1, 2.5]").ty.oid, arr(oid::NUMERIC));
    assert_eq!(first("SELECT ARRAY['a', 'b']").ty.oid, arr(oid::TEXT));
    assert_eq!(first("SELECT ARRAY[NULL, NULL]").ty.oid, arr(oid::TEXT));
}

#[test]
fn array_constructor_errors() {
    let c = catalog();
    assert_eq!(err(&c, "SELECT ARRAY[]"), "42P18");
    assert_eq!(err(&c, "SELECT ARRAY[1, true]"), "42804");
    assert_eq!(err(&c, "SELECT ARRAY[1, 'a']"), "22P02");
    assert_eq!(err(&c, "SELECT ARRAY[[1, 2], [3, 4]]"), "0A000");
    assert_eq!(err(&c, "SELECT ARRAY[ARRAY[1], ARRAY[2]]"), "0A000");
}

#[test]
fn array_constructor_with_cast() {
    let e = first("SELECT ARRAY[]::int[]");
    assert_eq!(e.ty.oid, arr(oid::INT4));
    let e = first("SELECT ARRAY['1', '2']::int[]");
    let BoundExprKind::ArrayCtor { elems, .. } = &e.kind else {
        panic!("not ArrayCtor: {e:?}");
    };
    assert!(matches!(
        elems[0].kind,
        BoundExprKind::Literal(Datum::Int4(1))
    ));
    assert_eq!(first("SELECT ARRAY[1, 2]::text[]").ty.oid, arr(oid::TEXT));
}

#[test]
fn array_cast_between_element_types() {
    let e = first("SELECT ARRAY(SELECT 1)::bigint[]");
    assert_eq!(e.ty.oid, arr(oid::INT8));
    assert!(matches!(e.kind, BoundExprKind::ArrayCoerce { .. }));
    assert_eq!(err(&catalog(), "SELECT ARRAY[1]::int2vector"), "42846");
}

#[test]
fn subscript() {
    let e = first("SELECT (SELECT ARRAY[10, 20, 30])[2]");
    assert_eq!(e.ty.oid, oid::INT4);
    assert!(matches!(e.kind, BoundExprKind::ArraySubscript { .. }));
    assert_eq!(first("SELECT (SELECT ARRAY['a'])['1']").ty.oid, oid::TEXT);
    let c = catalog();
    assert_eq!(err(&c, "SELECT (SELECT ARRAY[1, 2])[1:2]"), "0A000");
    assert_eq!(err(&c, "SELECT (SELECT 1)[1]"), "42804");
    assert_eq!(err(&c, "SELECT (SELECT ARRAY[1, 2])['x']"), "22P02");
}

#[test]
fn quantified_array() {
    let e = first("SELECT 1 = ANY (ARRAY[1, 2])");
    assert_eq!(e.ty.oid, oid::BOOL);
    assert!(matches!(
        e.kind,
        BoundExprKind::ScalarArrayOp { use_or: true, .. }
    ));
    let e = first("SELECT 1 < ALL (ARRAY[1, 2])");
    assert!(matches!(
        e.kind,
        BoundExprKind::ScalarArrayOp { use_or: false, .. }
    ));
    // 右辺が unknown のリテラルは要素型の配列として入力される。
    let e = first("SELECT 2 = ANY ('{1,2}')");
    let BoundExprKind::ScalarArrayOp { array, .. } = &e.kind else {
        panic!("not ScalarArrayOp: {e:?}");
    };
    assert_eq!(array.ty.oid, arr(oid::INT4));
    // 左右の型が違えば右辺は ArrayCoerce で包む。
    let e = first("SELECT 1.5 = ANY (ARRAY[1, 2])");
    let BoundExprKind::ScalarArrayOp { array, .. } = &e.kind else {
        panic!("not ScalarArrayOp: {e:?}");
    };
    assert_eq!(array.ty.oid, arr(oid::NUMERIC));
    assert!(matches!(array.kind, BoundExprKind::ArrayCoerce { .. }));
    let c = catalog();
    assert_eq!(err(&c, "SELECT 1 = ANY (5)"), "42809");
    assert_eq!(err(&c, "SELECT 1 + ANY (ARRAY[1])"), "42809");
}

#[test]
fn array_subquery() {
    let e = first("SELECT ARRAY(SELECT 1)");
    assert_eq!(e.ty.oid, arr(oid::INT4));
    assert!(matches!(
        e.kind,
        BoundExprKind::SubLink {
            kind: crate::expr::SubLinkKind::Array,
            ..
        }
    ));
    assert_eq!(err(&catalog(), "SELECT ARRAY(SELECT 1, 2)"), "42601");
}
