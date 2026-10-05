//! Analyzer unit tests (SQL text → parser → analyzer).

use std::sync::Arc;

use super::*;
use crate::catalog::fake::{FakeCatalog as MemoryCatalog, table_def};
use crate::catalog::{CastMethod, SystemColumn, TableDef};
use crate::error::Error;
use crate::types::{Datum, Oid, SqlType, oid};

fn catalog() -> MemoryCatalog {
    let mut c = MemoryCatalog::new("postgres");
    create(
        &mut c,
        "CREATE TABLE t (a int, b text, c float8, v varchar(3), f bool, s smallint, g bigint)",
    );
    c
}

fn run(c: &MemoryCatalog, sql: &str) -> std::result::Result<BoundStatement, Error> {
    let stmts = crate::sql::parse(sql)?;
    assert_eq!(stmts.len(), 1, "one statement: {sql}");
    analyze(&stmts[0], c)
}

fn create(c: &mut MemoryCatalog, sql: &str) {
    let BoundStatement::CreateTable(ct) = run(c, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    else {
        panic!("not CREATE TABLE");
    };
    let oid = c.allocate_oid();
    c.put_table(Arc::new(TableDef {
        schema: ct.schema,
        ..table_def(oid, &ct.name, ct.columns, ct.checks)
    }));
}

#[track_caller]
fn err(c: &MemoryCatalog, sql: &str) -> &'static str {
    match run(c, sql) {
        Ok(s) => panic!("expected error for {sql}, got {s:?}"),
        Err(e) => e.sqlstate.code(),
    }
}

#[track_caller]
fn select(c: &MemoryCatalog, sql: &str) -> BoundSelect {
    match run(c, sql) {
        Ok(BoundStatement::Select(s)) => *s,
        Ok(other) => panic!("not a select: {other:?}"),
        Err(e) => panic!("{sql}: {e:?}"),
    }
}

#[track_caller]
fn first(c: &MemoryCatalog, sql: &str) -> BoundExpr {
    select(c, sql).targets.remove(0)
}

fn types(c: &MemoryCatalog, sql: &str) -> Vec<Oid> {
    select(c, sql).columns.iter().map(|c| c.ty.oid).collect()
}

fn names(c: &MemoryCatalog, sql: &str) -> Vec<String> {
    select(c, sql).columns.into_iter().map(|c| c.name).collect()
}

fn op_oid(e: &BoundExpr) -> Oid {
    match &e.kind {
        BoundExprKind::Operator { op, .. } => op.oid,
        other => panic!("not an operator: {other:?}"),
    }
}

#[test]
fn literals_and_unknowns() {
    let c = catalog();
    assert_eq!(
        types(&c, "SELECT 1, 2147483648, 'a', NULL, true"),
        vec![oid::INT4, oid::INT8, oid::TEXT, oid::TEXT, oid::BOOL]
    );
    // unknown takes the other side's type
    assert_eq!(op_oid(&first(&c, "SELECT '1' + 1")), 551);
    assert_eq!(op_oid(&first(&c, "SELECT 1 + '1'")), 551);
    assert_eq!(op_oid(&first(&c, "SELECT '10' * 2::int8")), 686);
    let e = first(&c, "SELECT '1' + 1");
    let BoundExprKind::Operator { args, .. } = &e.kind else {
        unreachable!()
    };
    assert!(matches!(
        args[0].kind,
        BoundExprKind::Literal(Datum::Int4(1))
    ));
    assert_eq!(err(&c, "SELECT '1.5' + 1"), "22P02");
    assert_eq!(err(&c, "SELECT 'abc' + 1"), "22P02");
    assert_eq!(err(&c, "SELECT '99999999999' + 1"), "22003");
    assert_eq!(err(&c, "SELECT '40000' + 1::int2"), "22003");
    assert_eq!(err(&c, "SELECT '1' + '2'"), "42725");
    assert_eq!(err(&c, "SELECT NULL + NULL"), "42725");
    assert_eq!(op_oid(&first(&c, "SELECT 'a' = 'a'")), 98);
    assert_eq!(op_oid(&first(&c, "SELECT 'a' || 'b'")), 654);
    assert_eq!(first(&c, "SELECT 1.5").ty, SqlType::NUMERIC);
}

#[test]
fn operator_resolution() {
    let c = catalog();
    assert_eq!(op_oid(&first(&c, "SELECT s + s FROM t")), 550);
    assert_eq!(op_oid(&first(&c, "SELECT s + a FROM t")), 552);
    assert_eq!(op_oid(&first(&c, "SELECT a + g FROM t")), 692);
    assert_eq!(op_oid(&first(&c, "SELECT a + c FROM t")), 591);
    assert_eq!(first(&c, "SELECT 1 + 1::float4").ty, SqlType::FLOAT8);
    assert_eq!(op_oid(&first(&c, "SELECT v = v FROM t")), 98);
    assert_eq!(op_oid(&first(&c, "SELECT v = b FROM t")), 98);
    assert_eq!(err(&c, "SELECT 1 = true"), "42883");
    assert_eq!(err(&c, "SELECT 1 = 'a'::text"), "42883");
    assert_eq!(err(&c, "SELECT 'a'::text + 1"), "42883");
    assert_eq!(err(&c, "SELECT - 'a'::text"), "42883");
    assert_eq!(err(&c, "SELECT '1'::float8 % '2'::float8"), "42883");
    assert_eq!(err(&c, "SELECT now()"), "0A000");
    assert_eq!(err(&c, "SELECT txid_current()"), "0A000");
    assert_eq!(op_oid(&first(&c, "SELECT -a FROM t")), 558);
    assert_eq!(op_oid(&first(&c, "SELECT 2 ^ 3")), 965);
    assert_eq!(op_oid(&first(&c, "SELECT +c FROM t")), 1920);
    let e = run(&c, "SELECT 1 = true").unwrap_err();
    assert_eq!(e.message, "operator does not exist: integer = boolean");
}

#[test]
fn concat_variants() {
    let c = catalog();
    let e = first(&c, "SELECT 'x' || true");
    assert_eq!(op_oid(&e), 2779);
    let BoundExprKind::Operator { args, .. } = &e.kind else {
        unreachable!()
    };
    assert_eq!(args[1].ty, SqlType::TEXT);
    assert!(matches!(
        args[1].kind,
        BoundExprKind::Cast {
            method: CastMethod::Function(_),
            ..
        }
    ));
    assert_eq!(op_oid(&first(&c, "SELECT 1 || 'v'")), 2780);
    assert_eq!(op_oid(&first(&c, "SELECT v || a FROM t")), 2779);
    assert_eq!(op_oid(&first(&c, "SELECT v || v FROM t")), 654);
    assert_eq!(op_oid(&first(&c, "SELECT NULL || 'a'")), 654);
    assert_eq!(err(&c, "SELECT 1 || 2"), "42883");
    assert_eq!(
        op_oid(&first(
            &c,
            "SELECT upper(current_database()) || '/' || current_user"
        )),
        654
    );
}

#[test]
fn functions() {
    let c = catalog();
    let f = |sql: &str| match first(&c, sql).kind {
        BoundExprKind::Function { func, .. } => func.oid,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(f("SELECT abs('-1.5')"), 1395);
    assert_eq!(f("SELECT abs(-5)"), 1397);
    assert_eq!(f("SELECT abs(s) FROM t"), 1398);
    assert_eq!(f("SELECT length(NULL)"), 1317);
    assert_eq!(f("SELECT length(v) FROM t"), 1317);
    assert_eq!(f("SELECT UPPER('x')"), 871);
    assert_eq!(err(&c, "SELECT abs('abc')"), "22P02");
    assert_eq!(err(&c, "SELECT abs('1'::text)"), "42883");
    assert_eq!(err(&c, "SELECT abs(true)"), "42883");
    assert_eq!(err(&c, "SELECT length(123)"), "42883");
    assert_eq!(err(&c, "SELECT length()"), "42883");
    assert_eq!(err(&c, "SELECT no_such_function(1)"), "42883");
    assert_eq!(err(&c, "SELECT count(*) FROM t"), "0A000");
    let e = first(&c, "SELECT current_database()");
    assert!(matches!(
        e.kind,
        BoundExprKind::SessionValue(SessionValueKind::CurrentCatalog)
    ));
    assert_eq!(e.ty, SqlType::NAME);
    // function-style casts
    assert!(matches!(
        first(&c, "SELECT int4('12')").kind,
        BoundExprKind::Literal(Datum::Int4(12))
    ));
    assert!(matches!(
        first(&c, "SELECT text(1)").kind,
        BoundExprKind::Cast {
            method: CastMethod::InOut,
            ..
        }
    ));
    assert_eq!(f("SELECT float8(1)"), 316);
}

#[test]
fn casts() {
    let c = catalog();
    assert!(matches!(
        first(&c, "SELECT '12'::int").kind,
        BoundExprKind::Literal(Datum::Int4(12))
    ));
    assert_eq!(err(&c, "SELECT 'x'::int"), "22P02");
    assert_eq!(err(&c, "SELECT '1'::float8::bool"), "42846");
    assert_eq!(err(&c, "SELECT 1::int2::bool"), "42846");
    assert_eq!(err(&c, "SELECT true::int8"), "42846");
    assert_eq!(err(&c, "SELECT 1::nosuchtype"), "42704");
    assert_eq!(first(&c, "SELECT 1::numeric").ty, SqlType::NUMERIC);
    assert_eq!(err(&c, "SELECT 1::numeric(0)"), "22023");
    let e = first(&c, "SELECT 12345::varchar(2)");
    assert_eq!(e.ty, SqlType::varchar(2));
    let BoundExprKind::CoerceTypmod { expr, explicit } = e.kind else {
        panic!("typmod")
    };
    assert!(explicit);
    assert!(matches!(
        expr.kind,
        BoundExprKind::Cast {
            method: CastMethod::InOut,
            ..
        }
    ));
    // same type, no typmod: no node
    assert!(matches!(
        first(&c, "SELECT a::int4 FROM t").kind,
        BoundExprKind::ColumnRef { index: 0 }
    ));
    assert!(matches!(
        first(&c, "SELECT v::text FROM t").kind,
        BoundExprKind::Cast {
            method: CastMethod::Binary,
            ..
        }
    ));
    // :: binds tighter than unary minus: -(2147483648::int4)
    assert_eq!(op_oid(&first(&c, "SELECT -2147483648::int4")), 558);
}

#[test]
fn common_types() {
    let c = catalog();
    assert_eq!(
        types(&c, "SELECT CASE WHEN true THEN 1 ELSE '2.5'::float8 END"),
        vec![oid::FLOAT8]
    );
    assert_eq!(
        types(&c, "SELECT CASE WHEN true THEN 1 ELSE '2' END"),
        vec![oid::INT4]
    );
    assert_eq!(
        err(&c, "SELECT CASE WHEN true THEN 1 ELSE 'abc' END"),
        "22P02"
    );
    assert_eq!(
        err(&c, "SELECT CASE WHEN true THEN 1 ELSE 'x'::text END"),
        "42804"
    );
    assert_eq!(err(&c, "SELECT CASE WHEN 1 THEN 'a' END"), "42804");
    assert_eq!(
        types(&c, "SELECT CASE WHEN false THEN 'a' END"),
        vec![oid::TEXT]
    );
    assert_eq!(
        types(&c, "SELECT CASE 2 WHEN 1 THEN 'one' END"),
        vec![oid::TEXT]
    );
    assert_eq!(
        types(&c, "SELECT CASE NULL WHEN NULL THEN 1 END"),
        vec![oid::INT4]
    );
    assert_eq!(
        types(&c, "SELECT COALESCE(NULL, 1, '2.5'::float8)"),
        vec![oid::FLOAT8]
    );
    assert_eq!(types(&c, "SELECT COALESCE(NULL, NULL)"), vec![oid::TEXT]);
    assert_eq!(err(&c, "SELECT COALESCE(1, 'abc')"), "22P02");
    assert_eq!(err(&c, "SELECT COALESCE(1, 'abc'::text)"), "42804");
    assert_eq!(
        types(&c, "SELECT NULLIF(1, 1::int8), NULLIF('a', 'a')"),
        vec![oid::INT4, oid::TEXT]
    );
    assert_eq!(
        types(&c, "SELECT COALESCE(v, v) FROM t"),
        vec![oid::VARCHAR]
    );
    assert_eq!(
        select(&c, "SELECT COALESCE(v, v) FROM t").columns[0]
            .ty
            .typmod,
        7
    );
}

#[test]
fn values_lists() {
    let c = catalog();
    assert_eq!(
        types(&c, "VALUES (1, 'one'), (2, 'two')"),
        vec![oid::INT4, oid::TEXT]
    );
    assert_eq!(names(&c, "VALUES (1, 'one')"), vec!["column1", "column2"]);
    assert_eq!(types(&c, "VALUES (1), (10000000000)"), vec![oid::INT8]);
    assert_eq!(types(&c, "VALUES (1), ('2.5'::float8)"), vec![oid::FLOAT8]);
    assert_eq!(err(&c, "VALUES (1), ('abc')"), "22P02");
    assert_eq!(err(&c, "VALUES (1), ('a'::text)"), "42804");
    assert_eq!(err(&c, "VALUES (1, 2), (3)"), "42601");
    let s = select(&c, "VALUES (3), (1) ORDER BY column1");
    assert_eq!(s.order_by[0].target, 0);
    let s = select(&c, "VALUES (2, 'b'), (1, 'a') ORDER BY 2 DESC LIMIT 2");
    assert_eq!(s.order_by[0].target, 1);
    assert!(s.order_by[0].descending && s.order_by[0].nulls_first);
}

#[test]
fn in_and_between() {
    let c = catalog();
    let e = first(&c, "SELECT 1 IN (1::int8, 2)");
    let BoundExprKind::InList { eq_op, list, .. } = &e.kind else {
        panic!("{e:?}")
    };
    assert_eq!(eq_op.oid, 15);
    assert_eq!(list[1].ty, SqlType::INT8);
    assert!(matches!(
        first(&c, "SELECT 2 IN ('1', '2')").kind,
        BoundExprKind::InList { .. }
    ));
    assert_eq!(err(&c, "SELECT 1 IN (1, 'a')"), "22P02");
    assert_eq!(err(&c, "SELECT 1 IN ('a'::text)"), "42883");
    assert!(matches!(
        first(&c, "SELECT 2 IN (2)").kind,
        BoundExprKind::Operator { .. }
    ));
    assert!(matches!(
        first(&c, "SELECT 10 IN (a, a * 2) FROM t").kind,
        BoundExprKind::Or(_)
    ));
    assert!(matches!(
        first(&c, "SELECT a NOT IN (1, 2, g) FROM t").kind,
        BoundExprKind::And(_)
    ));
    assert!(matches!(
        first(&c, "SELECT 5 BETWEEN 1 AND 3").kind,
        BoundExprKind::And(_)
    ));
    assert!(matches!(
        first(&c, "SELECT 5 NOT BETWEEN 1 AND 3").kind,
        BoundExprKind::Or(_)
    ));
    assert!(matches!(
        first(&c, "SELECT 'abc' NOT LIKE 'a%'").kind,
        BoundExprKind::Like { negated: true, .. }
    ));
    assert_eq!(err(&c, "SELECT 1 LIKE 'a'"), "42883");
}

#[test]
fn boolean_contexts() {
    let c = catalog();
    assert_eq!(err(&c, "SELECT a FROM t WHERE a"), "42804");
    assert_eq!(err(&c, "SELECT a FROM t WHERE 1"), "42804");
    assert_eq!(err(&c, "SELECT 1 WHERE 'a'::text"), "42804");
    assert_eq!(err(&c, "SELECT a FROM t WHERE 'notbool'"), "22P02");
    select(&c, "SELECT a FROM t WHERE 'true'");
    select(&c, "SELECT a FROM t WHERE f");
    assert_eq!(err(&c, "SELECT 1 IS TRUE"), "42804");
    assert_eq!(err(&c, "SELECT 1 AND true"), "42804");
    assert_eq!(err(&c, "SELECT NOT 1"), "42804");
    assert_eq!(
        types(&c, "SELECT 't' AND true, 'no' OR false"),
        vec![oid::BOOL, oid::BOOL]
    );
    let e = run(&c, "SELECT a FROM t WHERE a").unwrap_err();
    assert_eq!(
        e.message,
        "argument of WHERE must be type boolean, not type integer"
    );
}

#[test]
fn names_and_scopes() {
    let c = catalog();
    assert_eq!(
        names(
            &c,
            "SELECT a, a AS x, 1::int4, b::text, 1 + 1, CASE WHEN true THEN 1 END FROM t"
        ),
        vec!["a", "x", "int4", "b", "?column?", "case"]
    );
    assert_eq!(
        names(
            &c,
            "SELECT COALESCE(1, 2), NULLIF(1, 2), current_user, current_database(), true"
        ),
        vec![
            "coalesce",
            "nullif",
            "current_user",
            "current_database",
            "bool"
        ]
    );
    assert_eq!(names(&c, "SELECT * FROM t").len(), 7);
    assert_eq!(names(&c, "SELECT x.* FROM t AS x").len(), 7);
    let s = select(&c, "SELECT b, a + 1 FROM t");
    assert_eq!((s.columns[0].table_oid, s.columns[0].attnum), (16384, 2));
    assert_eq!((s.columns[1].table_oid, s.columns[1].attnum), (0, 0));
    assert_eq!(err(&c, "SELECT t.a FROM t AS x"), "42P01");
    assert_eq!(err(&c, "SELECT other.a FROM t"), "42P01");
    assert_eq!(err(&c, "SELECT other.* FROM t"), "42P01");
    assert_eq!(err(&c, "SELECT nosuch FROM t"), "42703");
    assert_eq!(err(&c, "SELECT t.nosuch FROM t"), "42703");
    assert_eq!(err(&c, "SELECT a"), "42703");
    assert_eq!(err(&c, "SELECT *"), "42601");
    assert_eq!(err(&c, "SELECT * FROM nosuch"), "42P01");
    assert_eq!(err(&c, "SELECT a AS aa FROM t WHERE aa = 1"), "42703");
    select(&c, "SELECT public.t.a FROM public.t");
    let e = run(&c, "SELECT * FROM nosuch").unwrap_err();
    assert_eq!(e.message, "relation \"nosuch\" does not exist");
    assert!(e.cursor_byte.is_some());
}

#[test]
fn order_by_rules() {
    let c = catalog();
    let s = select(&c, "SELECT b, a FROM t ORDER BY 2 DESC NULLS LAST");
    assert_eq!(
        s.order_by[0],
        BoundSortKey {
            target: 1,
            descending: true,
            nulls_first: false
        }
    );
    assert_eq!(err(&c, "SELECT a FROM t ORDER BY 2"), "42P10");
    assert_eq!(err(&c, "SELECT a FROM t ORDER BY 0"), "42P10");
    assert_eq!(err(&c, "SELECT a FROM t ORDER BY 'x'"), "42601");
    assert_eq!(err(&c, "SELECT a AS aa FROM t ORDER BY aa + 1"), "42703");
    assert_eq!(err(&c, "SELECT a AS x, b AS x FROM t ORDER BY x"), "42702");
    select(&c, "SELECT a, a FROM t ORDER BY a");
    // a simple name prefers the output alias
    let s = select(&c, "SELECT -a AS a FROM t ORDER BY a");
    assert_eq!((s.order_by[0].target, s.targets.len()), (0, 1));
    // names inside expressions are input columns
    let s = select(&c, "SELECT -a AS a FROM t ORDER BY a + 0");
    assert_eq!((s.order_by[0].target, s.targets.len()), (1, 2));
    // ORDER BY a non-selected column → resjunk target
    let s = select(&c, "SELECT b FROM t ORDER BY a");
    assert_eq!(
        (s.order_by[0].target, s.columns.len(), s.targets.len()),
        (1, 1, 2)
    );
    // an expression equal to a target reuses it
    let s = select(&c, "SELECT a % 2 FROM t ORDER BY a % 2");
    assert_eq!((s.order_by[0].target, s.targets.len()), (0, 1));
    assert_eq!(err(&c, "SELECT DISTINCT b FROM t ORDER BY a"), "42P10");
    let s = select(&c, "SELECT DISTINCT b AS bb FROM t ORDER BY bb DESC");
    assert!(s.distinct && s.order_by[0].nulls_first);
}

#[test]
fn limit_offset() {
    let c = catalog();
    let s = select(&c, "SELECT a FROM t LIMIT 1 + 1 OFFSET 2");
    assert_eq!(s.limit.unwrap().ty, SqlType::INT8);
    assert_eq!(s.offset.unwrap().ty, SqlType::INT8);
    assert!(select(&c, "SELECT a FROM t LIMIT ALL").limit.is_none());
    select(&c, "SELECT a FROM t LIMIT NULL");
    select(&c, "SELECT a FROM t LIMIT -1");
    assert_eq!(err(&c, "SELECT a FROM t LIMIT 'abc'"), "22P02");
    assert_eq!(err(&c, "SELECT a FROM t LIMIT a"), "42P10");
}

#[test]
fn unsupported_statements() {
    let c = catalog();
    assert_eq!(err(&c, "SELECT a FROM t GROUP BY a"), "0A000");
    assert_eq!(err(&c, "SELECT 1 UNION SELECT 2"), "0A000");
    assert_eq!(err(&c, "SELECT * FROM t, t AS u"), "0A000");
    assert_eq!(err(&c, "SELECT DEFAULT"), "42601");
}

fn insert(c: &MemoryCatalog, sql: &str) -> BoundInsert {
    match run(c, sql) {
        Ok(BoundStatement::Insert(i)) => i,
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn insert_targets() {
    let mut c = catalog();
    create(
        &mut c,
        "CREATE TABLE d (id int DEFAULT 0, name text DEFAULT 'anon', score int DEFAULT 10 * 5, note text)",
    );
    let i = insert(&c, "INSERT INTO d (id) VALUES (1)");
    assert_eq!(i.column_map, vec![Some(0), None, None, None]);
    assert!(i.defaults[0].is_some() && i.defaults[3].is_none());
    let i = insert(&c, "INSERT INTO d VALUES (2, DEFAULT, DEFAULT, 'kw')");
    let BoundFrom::Values { rows, types } = &i.source.from else {
        panic!("values")
    };
    assert_eq!(types.len(), 4);
    assert!(matches!(rows[0][1].kind, BoundExprKind::Literal(Datum::Text(ref s)) if s == "anon"));
    assert_eq!(rows[0][1].ty, SqlType::TEXT);
    let i = insert(&c, "INSERT INTO d VALUES (5, 'carol', 1, DEFAULT)");
    let BoundFrom::Values { rows, .. } = &i.source.from else {
        panic!("values")
    };
    assert!(matches!(
        rows[0][3].kind,
        BoundExprKind::Literal(Datum::Null)
    ));
    let i = insert(&c, "INSERT INTO d DEFAULT VALUES");
    assert_eq!(i.column_map, vec![None; 4]);
    let i = insert(&c, "INSERT INTO d VALUES (7, 'seven')");
    assert_eq!(i.column_map, vec![Some(0), Some(1), None, None]);
    assert_eq!(err(&c, "INSERT INTO d VALUES (1, 'x', 2, 'y', 3)"), "42601");
    assert_eq!(
        err(&c, "INSERT INTO d (id, name) VALUES (8, 'x', 80)"),
        "42601"
    );
    assert_eq!(
        err(&c, "INSERT INTO d (id, name, score) VALUES (8, 'x')"),
        "42601"
    );
    assert_eq!(err(&c, "INSERT INTO d VALUES (8, 'x'), (9)"), "42601");
    assert_eq!(err(&c, "INSERT INTO d (id, id) VALUES (8, 9)"), "42701");
    assert_eq!(err(&c, "INSERT INTO d (id, nosuch) VALUES (8, 9)"), "42703");
    assert_eq!(err(&c, "INSERT INTO nosuch VALUES (1)"), "42P01");
    assert_eq!(err(&c, "INSERT INTO d VALUES (DEFAULT + 1)"), "42601");
    assert_eq!(err(&c, "INSERT INTO d VALUES (1) RETURNING id"), "0A000");
}

#[test]
fn insert_coercion() {
    let c = catalog();
    assert_eq!(err(&c, "INSERT INTO t (a) VALUES ('x')"), "22P02");
    assert_eq!(err(&c, "INSERT INTO t (a) VALUES ('1'::text)"), "42804");
    assert_eq!(err(&c, "INSERT INTO t (a) VALUES (true)"), "42804");
    assert_eq!(err(&c, "INSERT INTO t (f) VALUES (1)"), "42804");
    let e = run(&c, "INSERT INTO t (a) VALUES ('1'::text)").unwrap_err();
    assert_eq!(
        e.message,
        "column \"a\" is of type integer but expression is of type text"
    );
    // int8 → int4 assignment cast (range checked at run time)
    insert(&c, "INSERT INTO t (a) VALUES (3000000000)");
    // int → varchar(3): I/O conversion then length check
    let i = insert(&c, "INSERT INTO t (v) VALUES (123)");
    let BoundFrom::Values { rows, .. } = &i.source.from else {
        panic!("values")
    };
    let BoundExprKind::CoerceTypmod { expr, explicit } = &rows[0][0].kind else {
        panic!("{:?}", rows[0][0])
    };
    assert!(!explicit);
    assert!(matches!(
        expr.kind,
        BoundExprKind::Cast {
            method: CastMethod::InOut,
            ..
        }
    ));
    // explicit varchar(3) cast assigned to varchar(3): no second coercion
    let i = insert(&c, "INSERT INTO t (v) VALUES ('wxyz'::varchar(3))");
    let BoundFrom::Values { rows, .. } = &i.source.from else {
        panic!("values")
    };
    assert!(matches!(
        rows[0][0].kind,
        BoundExprKind::CoerceTypmod { explicit: true, .. }
    ));
}

#[test]
fn insert_select() {
    let c = catalog();
    let i = insert(
        &c,
        "INSERT INTO t (b, a) SELECT b || '!', a * 100 FROM t WHERE a >= 2",
    );
    assert_eq!(i.column_map[0], Some(1));
    assert_eq!(i.column_map[1], Some(0));
    assert_eq!(err(&c, "INSERT INTO t (a) SELECT a, b FROM t"), "42601");
    assert_eq!(err(&c, "INSERT INTO t (a) SELECT b FROM t"), "42804");
    // unknown output columns take the target type
    let i = insert(&c, "INSERT INTO t (a) SELECT '7'");
    assert!(matches!(
        i.source.targets[0].kind,
        BoundExprKind::Literal(Datum::Int4(7))
    ));
    assert!(i.coercions.is_none());
    // a plain query is coerced in its own target list
    let i = insert(&c, "INSERT INTO t (b) SELECT a FROM t WHERE a > 0");
    assert!(i.coercions.is_none());
    assert_eq!(i.source.targets[0].ty, SqlType::TEXT);
    // ORDER BY / LIMIT: the query stays uncoerced (sorts and limits the
    // original values); the coercion runs above it
    let i = insert(
        &c,
        "INSERT INTO t (b) SELECT a FROM t ORDER BY a DESC LIMIT 3",
    );
    assert_eq!(i.source.targets.len(), 1);
    assert_eq!(i.source.targets[0].ty, SqlType::INT4);
    assert_eq!(i.source.columns[0].ty, SqlType::INT4);
    let co = i.coercions.as_ref().expect("coercions");
    assert_eq!(co.len(), 1);
    assert_eq!(co[0].ty, SqlType::TEXT);
    let BoundExprKind::Cast { expr, .. } = &co[0].kind else {
        panic!("{:?}", co[0])
    };
    assert!(matches!(expr.kind, BoundExprKind::ColumnRef { index: 0 }));
    // DISTINCT runs on the uncoerced values (float8 1.2 and 1.4 stay two
    // rows before rounding to int)
    let i = insert(&c, "INSERT INTO t (a) SELECT DISTINCT c FROM t");
    assert!(i.source.distinct);
    assert_eq!(i.source.targets[0].ty, SqlType::FLOAT8);
    assert_eq!(i.coercions.as_ref().unwrap()[0].ty, SqlType::INT4);
    // varchar(n) length check happens above LIMIT
    let i = insert(&c, "INSERT INTO t (v) SELECT b FROM t ORDER BY b LIMIT 1");
    assert_eq!(i.source.targets[0].ty, SqlType::TEXT);
    assert!(matches!(
        i.coercions.as_ref().unwrap()[0].kind,
        BoundExprKind::CoerceTypmod {
            explicit: false,
            ..
        }
    ));
    // an unknown literal is still converted at analysis time
    let i = insert(&c, "INSERT INTO t (a) SELECT '7' LIMIT 1");
    assert!(matches!(
        i.source.targets[0].kind,
        BoundExprKind::Literal(Datum::Int4(7))
    ));
    assert_eq!(err(&c, "INSERT INTO t (a) SELECT 'x' LIMIT 1"), "22P02");
    assert_eq!(
        err(&c, "INSERT INTO t (a) SELECT b FROM t LIMIT 1"),
        "42804"
    );
    // a sort key resolves an unknown output column to text
    assert_eq!(err(&c, "INSERT INTO t (a) SELECT '7' ORDER BY 1"), "42804");
}

#[test]
fn stored_defaults_and_checks() {
    let mut c = catalog();
    create(
        &mut c,
        "CREATE TABLE k (a int DEFAULT '42' CHECK (a > 0), b int, c text CONSTRAINT c_not_empty CHECK (c <> ''), CHECK (b < 100), CONSTRAINT a_lt_b CHECK (a < b), CHECK (1 > 0))",
    );
    let t = c.table(None, "k").unwrap().unwrap();
    assert_eq!(t.columns[0].default.as_ref().unwrap().expr_sql, "'42'");
    let names: Vec<&str> = t.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["k_a_check", "c_not_empty", "k_b_check", "a_lt_b", "k_check"]
    );
    assert_eq!(t.checks[0].expr_sql, "a > 0");
    let i = insert(&c, "INSERT INTO k (b) VALUES (1)");
    assert!(matches!(
        i.defaults[0].as_ref().unwrap().kind,
        BoundExprKind::Literal(Datum::Int4(42))
    ));
    assert_eq!(i.checks.len(), 5);
    assert_eq!(i.checks[0].expr.ty, SqlType::BOOL);
    // a column CHECK referencing another column is named after the column only if it refs
    // exactly one column (otherwise "<table>_check")
    create(
        &mut c,
        "CREATE TABLE k2 (lo int, hi int CHECK (hi >= lo), x int CHECK (x > 0), y int CHECK (y > 0) CHECK (y < 9))",
    );
    let t = c.table(None, "k2").unwrap().unwrap();
    let names: Vec<&str> = t.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["k2_check", "k2_x_check", "k2_y_check", "k2_y_check1"]
    );
}

#[test]
fn create_table_validation() {
    let c = catalog();
    assert_eq!(err(&c, "CREATE TABLE t (z int)"), "42P07");
    match run(&c, "CREATE TABLE IF NOT EXISTS t (z nosuchtype)") {
        Ok(BoundStatement::CreateTable(ct)) => assert!(ct.if_not_exists && ct.columns.is_empty()),
        other => panic!("{other:?}"),
    }
    assert_eq!(err(&c, "CREATE TABLE x (a int, a text)"), "42701");
    assert_eq!(err(&c, "CREATE TABLE x (a nosuchtype)"), "42704");
    assert_eq!(err(&c, "CREATE TABLE x (a numeric(1001))"), "22023");
    assert_eq!(err(&c, "CREATE TABLE x (a varchar(0))"), "22023");
    assert_eq!(err(&c, "CREATE TABLE x (a int(3))"), "42601");
    assert_eq!(err(&c, "CREATE TABLE x (a int DEFAULT 'abc')"), "22P02");
    assert_eq!(err(&c, "CREATE TABLE x (a int DEFAULT true)"), "42804");
    assert_eq!(err(&c, "CREATE TABLE x (a int, b int DEFAULT a)"), "0A000");
    assert_eq!(err(&c, "CREATE TABLE x (a int NULL NOT NULL)"), "42601");
    assert_eq!(err(&c, "CREATE TABLE x (a int PRIMARY KEY)"), "0A000");
    assert_eq!(err(&c, "CREATE TABLE x (a int UNIQUE)"), "0A000");
    assert_eq!(err(&c, "CREATE TABLE x (a int, PRIMARY KEY (a))"), "0A000");
    assert_eq!(err(&c, "CREATE TABLE x (a int CHECK (a + 1))"), "42804");
    assert_eq!(
        err(&c, "CREATE TABLE x (a int CHECK (nosuch > 0))"),
        "42703"
    );
    assert_eq!(err(&c, "CREATE TABLE other.x (a int)"), "3F000");
    match run(
        &c,
        "CREATE TABLE x (v varchar(2) DEFAULT 'abc', n int NOT NULL, d double precision, r real)",
    ) {
        Ok(BoundStatement::CreateTable(ct)) => {
            assert_eq!(ct.columns[0].ty, SqlType::varchar(2));
            assert!(ct.columns[1].not_null);
            assert_eq!(ct.columns[2].ty, SqlType::FLOAT8);
            assert_eq!(ct.columns[3].ty, SqlType::FLOAT4);
            assert_eq!(ct.columns[3].attnum, 4);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn drop_table() {
    let c = catalog();
    assert_eq!(err(&c, "DROP TABLE nosuch"), "42P01");
    assert_eq!(err(&c, "DROP TABLE t, nosuch"), "42P01");
    match run(&c, "DROP TABLE IF EXISTS t, nosuch") {
        Ok(BoundStatement::DropTable(d)) => {
            assert_eq!(d.tables.len(), 1);
            assert_eq!(d.missing, vec!["nosuch".to_owned()]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn object_names() {
    assert_eq!(ddl::make_object_name("t", Some("a"), "check"), "t_a_check");
    assert_eq!(ddl::make_object_name("t", None, "check"), "t_check");
    let long = "x".repeat(70);
    let n = ddl::make_object_name(&long, Some("col"), "check");
    assert_eq!(n.len(), 63);
    assert!(n.ends_with("_col_check"));
}

#[test]
fn builtin_tables_are_consistent() {
    use crate::catalog::builtin::{CASTS, FUNCTIONS, OPERATORS, type_by_oid};
    let known = |o: Oid| type_by_oid(o).is_some();
    for (i, a) in OPERATORS.iter().enumerate() {
        assert!(
            known(a.right) && known(a.result) && a.left.is_none_or(known),
            "{}",
            a.oid
        );
        assert!(
            OPERATORS[i + 1..].iter().all(|b| b.oid != a.oid),
            "dup op {}",
            a.oid
        );
        assert!(
            OPERATORS[i + 1..]
                .iter()
                .all(|b| (b.name, b.left, b.right) != (a.name, a.left, a.right)),
            "dup signature {}",
            a.oid
        );
    }
    for (i, a) in FUNCTIONS.iter().enumerate() {
        assert!(
            a.args.iter().all(|t| known(*t)) && known(a.result),
            "{}",
            a.oid
        );
        assert!(
            FUNCTIONS[i + 1..].iter().all(|b| b.oid != a.oid),
            "dup func {}",
            a.oid
        );
    }
    for (i, a) in CASTS.iter().enumerate() {
        assert!(known(a.source) && known(a.target));
        assert!(
            CASTS[i + 1..]
                .iter()
                .all(|b| (b.source, b.target) != (a.source, a.target))
        );
    }
}

// ----- M2: UPDATE / DELETE / CHECKPOINT / system columns -------------------

fn update(c: &MemoryCatalog, sql: &str) -> BoundUpdate {
    match run(c, sql) {
        Ok(BoundStatement::Update(u)) => u,
        other => panic!("{sql}: {other:?}"),
    }
}

fn delete(c: &MemoryCatalog, sql: &str) -> BoundDelete {
    match run(c, sql) {
        Ok(BoundStatement::Delete(d)) => d,
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn update_basic() {
    let mut c = catalog();
    create(
        &mut c,
        "CREATE TABLE n (a int NOT NULL DEFAULT 7, b int CHECK (b > 0))",
    );
    let u = update(&c, "UPDATE n SET a = DEFAULT, b = a + 1 WHERE b > 0");
    assert_eq!(u.assignments.len(), 2);
    assert_eq!(u.assignments[0].0, 0);
    assert!(matches!(
        &u.assignments[0].1,
        UpdateSource::Default(Some(_))
    ));
    let UpdateSource::Expr(e) = &u.assignments[1].1 else {
        panic!()
    };
    assert_eq!(e.ty, SqlType::INT4);
    assert!(u.filter.is_some());
    assert_eq!(u.not_null, vec![true, false]);
    assert_eq!(u.checks.len(), 1);
    assert!(u.system_columns.is_empty());
    // DEFAULT without a default expression is NULL
    let u = update(&c, "UPDATE n SET b = DEFAULT");
    assert!(matches!(&u.assignments[0].1, UpdateSource::Default(None)));
    // assignment cast of an unknown literal and of int4 to int8
    let u = update(&c, "UPDATE t SET g = a, c = '1.5', v = 'ab'");
    assert_eq!(u.assignments.len(), 3);
    for (_, s) in &u.assignments {
        let UpdateSource::Expr(e) = s else { panic!() };
        assert!(e.ty.oid != oid::UNKNOWN);
    }
    // alias, ONLY, qualified table
    update(
        &c,
        "UPDATE ONLY public.t AS x SET a = x.a + 1 WHERE x.b = 'q'",
    );
}

#[test]
fn update_errors() {
    let c = catalog();
    assert_eq!(err(&c, "UPDATE t SET zzz = 1"), "42703");
    assert_eq!(err(&c, "UPDATE t SET a = zzz"), "42703");
    assert_eq!(err(&c, "UPDATE t SET a = 1 WHERE zzz = 1"), "42703");
    assert_eq!(err(&c, "UPDATE t SET a = 1, a = 2"), "42601");
    assert_eq!(err(&c, "UPDATE t SET ctid = '(0,1)'"), "0A000");
    assert_eq!(err(&c, "UPDATE t SET xmin = 1"), "0A000");
    assert_eq!(err(&c, "UPDATE t SET t.a = 1"), "42703");
    assert_eq!(err(&c, "UPDATE t SET a.x = 1"), "42804");
    assert_eq!(err(&c, "UPDATE t SET a = true"), "42804");
    assert_eq!(err(&c, "UPDATE t SET a = b"), "42804");
    assert_eq!(err(&c, "UPDATE t SET a = 'abc'"), "22P02");
    assert_eq!(err(&c, "UPDATE t SET a = 1 WHERE b"), "42804");
    assert_eq!(err(&c, "UPDATE nope SET a = 1"), "42P01");
    assert_eq!(err(&c, "UPDATE t SET a = 2 RETURNING a"), "0A000");
    assert_eq!(err(&c, "UPDATE t SET a = 1 FROM t AS u"), "0A000");
    assert_eq!(err(&c, "UPDATE x.t SET a = 1"), "42P01");
    let e = run(&c, "UPDATE t SET t.a = 1").unwrap_err();
    assert_eq!(e.message, "column \"t\" of relation \"t\" does not exist");
    assert!(e.hint.is_some());
}

#[test]
fn delete_basic_and_errors() {
    let c = catalog();
    let d = delete(&c, "DELETE FROM t");
    assert!(d.filter.is_none());
    let d = delete(
        &c,
        "DELETE FROM ONLY t AS x WHERE x.a > 1 AND x.xmin IS NOT NULL",
    );
    assert!(d.filter.is_some());
    assert_eq!(d.system_columns, vec![SystemColumn::Xmin]);
    assert_eq!(err(&c, "DELETE FROM t WHERE a"), "42804");
    assert_eq!(err(&c, "DELETE FROM t WHERE zzz = 1"), "42703");
    assert_eq!(err(&c, "DELETE FROM nope"), "42P01");
    assert_eq!(err(&c, "DELETE FROM t RETURNING a"), "0A000");
    assert_eq!(err(&c, "DELETE FROM t USING t AS u"), "0A000");
    assert_eq!(err(&c, "DELETE FROM t AS x WHERE t.a = 1"), "42P01");
}

#[test]
fn checkpoint_statement() {
    let c = catalog();
    assert!(matches!(
        run(&c, "CHECKPOINT").unwrap(),
        BoundStatement::Checkpoint
    ));
}

#[test]
fn system_columns() {
    let mut c = catalog();
    let s = select(
        &c,
        "SELECT xmin, ctid, xmin, tableoid FROM t WHERE cmax IS NULL",
    );
    let BoundFrom::Table { system_columns, .. } = &s.from else {
        panic!()
    };
    // natts of t is 7; first-use order: cmax (WHERE is analyzed after the
    // target list, so xmin, ctid, tableoid come first)
    assert_eq!(
        system_columns,
        &vec![
            SystemColumn::Xmin,
            SystemColumn::Ctid,
            SystemColumn::TableOid,
            SystemColumn::Cmax
        ]
    );
    let idx = |e: &BoundExpr| match e.kind {
        BoundExprKind::ColumnRef { index } => index,
        _ => panic!(),
    };
    assert_eq!(idx(&s.targets[0]), 7);
    assert_eq!(idx(&s.targets[1]), 8);
    assert_eq!(idx(&s.targets[2]), 7);
    assert_eq!(idx(&s.targets[3]), 9);
    assert_eq!(
        s.columns.iter().map(|c| c.ty.oid).collect::<Vec<_>>(),
        vec![oid::XID, oid::TID, oid::XID, oid::OID]
    );
    // `*` excludes them; qualified names work; unreferenced -> empty
    assert_eq!(select(&c, "SELECT * FROM t").columns.len(), 7);
    let s = select(&c, "SELECT t.ctid FROM t");
    assert_eq!(s.columns[0].name, "ctid");
    let BoundFrom::Table { system_columns, .. } = &s.from else {
        panic!()
    };
    assert_eq!(system_columns, &vec![SystemColumn::Ctid]);
    // no FROM: not a column
    assert_eq!(err(&c, "SELECT xmin"), "42703");
    // CREATE TABLE refuses the names
    assert_eq!(err(&c, "CREATE TABLE s (xmin int)"), "42701");
    assert_eq!(
        err(&c, "CREATE TABLE s (a int CHECK (ctid IS NULL))"),
        "42703"
    );
    // UPDATE collects them from WHERE and SET
    create(&mut c, "CREATE TABLE u (a int)");
    let u = update(
        &c,
        "UPDATE u SET a = CASE WHEN cmin IS NULL THEN 1 ELSE 2 END WHERE xmax IS NOT NULL",
    );
    assert_eq!(
        u.system_columns,
        vec![SystemColumn::Xmax, SystemColumn::Cmin]
    );
}

#[test]
fn qualified_functions() {
    let c = catalog();
    select(&c, "SELECT pg_catalog.abs(-1)");
    select(&c, "SELECT pg_catalog.length('a')");
    assert_eq!(err(&c, "SELECT public.abs(-1)"), "42883");
}

#[test]
fn catalog_tables_are_read_only() {
    let mut c = catalog();
    let def = Arc::new(TableDef {
        schema: "pg_catalog".into(),
        ..table_def(1259, "pg_class", vec![], vec![])
    });
    c.put_table(def);
    assert_eq!(err(&c, "UPDATE pg_catalog.pg_class SET x = 1"), "42501");
    assert_eq!(err(&c, "DELETE FROM pg_catalog.pg_class"), "42501");
    assert_eq!(
        err(&c, "INSERT INTO pg_catalog.pg_class DEFAULT VALUES"),
        "42501"
    );
}

#[test]
fn int4_array_and_pg_typeof_and_pg_sleep_literal() {
    let c = catalog();
    assert_eq!(types(&c, "SELECT '{1,2}'::int4[]"), vec![oid::INT4_ARRAY]);
    assert_eq!(err(&c, "SELECT '{1}'::text[]"), "0A000");
    // `||` takes text only with anynonarray, so arrays never match.
    for q in [
        "SELECT '{1,2}'::int4[] || '{3}'",
        "SELECT '{1,2}'::int4[] || 'abc'::text",
        "SELECT 'abc'::text || '{1,2}'::int4[]",
    ] {
        assert_eq!(err(&c, q), "42883", "{q}");
    }
    assert_eq!(err(&c, "CREATE TABLE a (c int4[])"), "0A000");
    assert_eq!(types(&c, "SELECT pg_typeof(1)"), vec![oid::TEXT]);
    assert!(matches!(
        first(&c, "SELECT pg_typeof(1)").kind,
        BoundExprKind::Literal(Datum::Text(ref s)) if s == "integer"
    ));
    assert_eq!(types(&c, "SELECT pg_sleep(0.01)"), vec![oid::VOID]);
}
