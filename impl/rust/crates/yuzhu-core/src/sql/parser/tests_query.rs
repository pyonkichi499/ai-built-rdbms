//! Parser tests for the M4 query syntax (03 §6.1): WITH, functions in FROM,
//! ONLY, FILTER, ANY / ALL, COLLATE, OPERATOR(...), row values, special
//! GROUP BY forms.

#![allow(clippy::format_push_string, clippy::too_many_lines)]

use super::{parse, parse_expr};
use crate::error::{Error, sqlstate};
use crate::sql::ast::{
    Expr, JoinKind, Literal, ObjectName, Quantifier, Query, QueryBody, Select, SelectItem,
    Statement, TableRef,
};

// ----- helpers -------------------------------------------------------------

fn names(n: &ObjectName) -> String {
    n.parts
        .iter()
        .map(|p| p.value.clone())
        .collect::<Vec<_>>()
        .join(".")
}

fn sx_list(items: &[Expr]) -> String {
    items.iter().map(sx).collect::<Vec<_>>().join(" ")
}

/// Compact rendering of the expression shapes used in these tests.
fn sx(e: &Expr) -> String {
    match e {
        Expr::Literal { value, .. } => match value {
            Literal::Integer(s) | Literal::Decimal(s) => s.clone(),
            Literal::String(s) => format!("'{s}'"),
            Literal::Bool(b) => b.to_string(),
            Literal::Null => "null".into(),
        },
        Expr::Column { parts, .. } => parts
            .iter()
            .map(|p| p.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        Expr::BinaryOp {
            op,
            op_schema,
            left,
            right,
            ..
        } => match op_schema {
            Some(s) => format!("({}.{op} {} {})", s.value, sx(left), sx(right)),
            None => format!("({op} {} {})", sx(left), sx(right)),
        },
        Expr::UnaryOp {
            op,
            op_schema,
            expr,
            ..
        } => match op_schema {
            Some(s) => format!("(u{}.{op} {})", s.value, sx(expr)),
            None => format!("(u{op} {})", sx(expr)),
        },
        Expr::And { left, right, .. } => format!("(and {} {})", sx(left), sx(right)),
        Expr::Collate {
            expr, collation, ..
        } => format!("(collate {} {})", sx(expr), names(collation)),
        Expr::Row {
            items, explicit, ..
        } => format!(
            "({} {})",
            if *explicit { "ROW" } else { "row" },
            sx_list(items)
        ),
        Expr::QuantifiedSubquery {
            expr,
            op,
            op_schema,
            quantifier,
            query,
            ..
        } => format!(
            "({}{op}-{} {} [{}])",
            op_schema
                .as_ref()
                .map_or(String::new(), |s| format!("{}.", s.value)),
            if *quantifier == Quantifier::All {
                "all"
            } else {
                "any"
            },
            sx(expr),
            qs(query)
        ),
        Expr::InSubquery {
            expr,
            query,
            negated,
            ..
        } => format!(
            "({}in {} [{}])",
            if *negated { "not" } else { "" },
            sx(expr),
            qs(query)
        ),
        Expr::Function {
            name,
            args,
            filter,
            star,
            ..
        } => {
            let mut s = format!(
                "{}({}{})",
                names(name),
                if *star { "*" } else { "" },
                sx_list(args)
            );
            if let Some(f) = filter {
                s.push_str(&format!(" filter {}", sx(f)));
            }
            s
        }
        Expr::Subquery { query, .. } => format!("(subquery [{}])", qs(query)),
        Expr::Like { .. } => "like".into(),
        other => format!("{other:?}"),
    }
}

fn select_of(q: &Query) -> &Select {
    match &q.body {
        QueryBody::Select(s) => s,
        other => panic!("not a select: {other:?}"),
    }
}

/// `select <targets> from <items>` rendering of a query (WITH included).
fn qs(q: &Query) -> String {
    let mut out = String::new();
    if let Some(w) = &q.with {
        out.push_str(if w.recursive { "with-rec " } else { "with " });
        let ctes: Vec<String> = w
            .ctes
            .iter()
            .map(|c| {
                let mut s = c.name.value.clone();
                if !c.columns.is_empty() {
                    let cols: Vec<&str> = c.columns.iter().map(|i| i.value.as_str()).collect();
                    s.push_str(&format!("({})", cols.join(",")));
                }
                match c.materialized {
                    Some(true) => s.push_str(" mat"),
                    Some(false) => s.push_str(" notmat"),
                    None => {}
                }
                format!("{s} <{}>", qs(&c.query))
            })
            .collect();
        out.push_str(&ctes.join(", "));
        out.push(' ');
    }
    out.push_str(&body(&q.body));
    if !q.order_by.is_empty() {
        out.push_str(" order");
    }
    if let Some(l) = &q.limit {
        out.push_str(&format!(" limit {}", sx(l)));
    }
    out
}

fn body(b: &QueryBody) -> String {
    match b {
        QueryBody::Select(s) => {
            let t: Vec<String> = s
                .targets
                .iter()
                .map(|t| match t {
                    SelectItem::Expr { expr, .. } => sx(expr),
                    SelectItem::Wildcard(_) => "*".into(),
                    SelectItem::QualifiedWildcard(n, _) => format!("{}.*", names(n)),
                })
                .collect();
            let mut out = format!("select {}", t.join(", "));
            if !s.from.is_empty() {
                let f: Vec<String> = s.from.iter().map(tr).collect();
                out.push_str(&format!(" from {}", f.join(", ")));
            }
            if let Some(w) = &s.selection {
                out.push_str(&format!(" where {}", sx(w)));
            }
            if !s.group_by.is_empty() {
                out.push_str(&format!(" group {}", sx_list(&s.group_by)));
            }
            out
        }
        QueryBody::Values(v) => format!("values {}", v.rows.len()),
        QueryBody::SetOp { left, right, .. } => format!("{} setop {}", body(left), body(right)),
        QueryBody::Nested(q) => format!("nested[{}]", qs(q)),
    }
}

fn tr(t: &TableRef) -> String {
    let alias = |a: &Option<crate::sql::ast::TableAlias>| {
        a.as_ref().map_or(String::new(), |a| {
            let mut s = format!(" {}", a.name.value);
            if !a.columns.is_empty() {
                let c: Vec<&str> = a.columns.iter().map(|c| c.value.as_str()).collect();
                s.push_str(&format!("({})", c.join(",")));
            }
            s
        })
    };
    match t {
        TableRef::Table { name, alias: a, .. } => format!("{}{}", names(name), alias(a)),
        TableRef::Subquery {
            query, alias: a, ..
        } => format!("[{}]{}", qs(query), alias(a)),
        TableRef::Function {
            name,
            args,
            alias: a,
            ..
        } => format!("fn:{}({}){}", names(name), sx_list(args), alias(a)),
        TableRef::Join {
            left, right, kind, ..
        } => {
            let k = if *kind == JoinKind::Inner {
                "join"
            } else {
                "other"
            };
            format!("({k} {} {})", tr(left), tr(right))
        }
    }
}

fn query(sql: &str) -> Query {
    let mut v = parse(sql).unwrap_or_else(|e| panic!("parse({sql:?}) failed: {}", e.message));
    assert_eq!(v.len(), 1, "{sql}");
    match v.remove(0) {
        Statement::Query(q) => *q,
        other => panic!("not a query: {other:?}"),
    }
}

fn q(sql: &str) -> String {
    qs(&query(sql))
}

fn e(sql: &str) -> String {
    match parse_expr(sql) {
        Ok(x) => sx(&x),
        Err(err) => panic!("parse_expr({sql:?}) failed: {}", err.message),
    }
}

fn expr(sql: &str) -> Expr {
    parse_expr(sql).unwrap_or_else(|err| panic!("parse_expr({sql:?}) failed: {}", err.message))
}

fn err(sql: &str) -> Error {
    match parse(sql) {
        Ok(v) => panic!("expected an error for {sql:?}, got {v:?}"),
        Err(mut e) => {
            e.resolve_position(sql);
            e
        }
    }
}

/// 0A000 with the exact message and 1-based position.
fn unsupported(sql: &str, message: &str, pos: u32) {
    let e = err(sql);
    assert_eq!(
        e.sqlstate,
        sqlstate::FEATURE_NOT_SUPPORTED,
        "{sql}: {}",
        e.message
    );
    assert_eq!(e.message, message, "{sql}");
    assert_eq!(e.position, Some(pos), "{sql}");
}

fn syntax(sql: &str, near: &str, pos: u32) {
    let e = err(sql);
    assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR, "{sql}: {}", e.message);
    let expected = if near.is_empty() {
        "syntax error at end of input".to_string()
    } else {
        format!("syntax error at or near \"{near}\"")
    };
    assert_eq!(e.message, expected, "{sql}");
    assert_eq!(e.position, Some(pos), "{sql}");
}

// ----- WITH ------------------------------------------------------------------

#[test]
fn with_basic() {
    let query = query("WITH x AS (SELECT 1) SELECT * FROM x");
    let w = query.with.as_ref().unwrap();
    assert!(!w.recursive);
    assert_eq!(w.ctes.len(), 1);
    let c = &w.ctes[0];
    assert_eq!(c.name.value, "x");
    assert!(c.columns.is_empty());
    assert_eq!(c.materialized, None);
    assert_eq!(qs(&c.query), "select 1");
    // Spans: the query starts at WITH, the CTE at its name and ends at `)`.
    assert_eq!(query.span.start, 0);
    assert_eq!((c.span.start, c.span.end), (5, 20));
    assert_eq!((w.span.start, w.span.end), (0, 20));
    assert_eq!(qs(&query), "with x <select 1> select * from x");
}

#[test]
fn with_recursive_materialized_columns() {
    assert_eq!(
        q("WITH RECURSIVE x AS (SELECT 1) SELECT * FROM x"),
        "with-rec x <select 1> select * from x"
    );
    assert_eq!(
        q("WITH x AS MATERIALIZED (SELECT 1) SELECT 2"),
        "with x mat <select 1> select 2"
    );
    assert_eq!(
        q("WITH x AS NOT MATERIALIZED (SELECT 1) SELECT 2"),
        "with x notmat <select 1> select 2"
    );
    assert_eq!(
        q("WITH x(a, b) AS (SELECT 1, 2) SELECT a FROM x"),
        "with x(a,b) <select 1, 2> select a from x"
    );
    assert_eq!(
        q("WITH x AS (SELECT 1), y AS (SELECT 2), z(c) AS (SELECT 3) SELECT 4"),
        "with x <select 1>, y <select 2>, z(c) <select 3> select 4"
    );
    // `recursive` as a CTE name.
    let w = query("WITH recursive AS (SELECT 1) SELECT 2");
    let w = w.with.unwrap();
    assert!(!w.recursive);
    assert_eq!(w.ctes[0].name.value, "recursive");
}

#[test]
fn with_nested() {
    assert_eq!(
        q("WITH x AS (WITH y AS (SELECT 1) SELECT * FROM y) SELECT * FROM x"),
        "with x <with y <select 1> select * from y> select * from x"
    );
    assert_eq!(
        q("SELECT * FROM (WITH y AS (SELECT 1) SELECT * FROM y) s"),
        "select * from [with y <select 1> select * from y] s"
    );
    assert_eq!(
        e("(WITH y AS (SELECT 1) SELECT * FROM y)"),
        "(subquery [with y <select 1> select * from y])"
    );
    assert_eq!(
        e("1 IN (WITH y AS (SELECT 1) SELECT * FROM y)"),
        "(in 1 [with y <select 1> select * from y])"
    );
    // A parenthesized WITH query as an operand keeps its WITH.
    assert_eq!(
        q("(WITH y AS (SELECT 1) SELECT * FROM y) UNION SELECT 2"),
        "nested[with y <select 1> select * from y] setop select 2"
    );
}

#[test]
fn with_over_set_operations_values_table() {
    assert_eq!(
        q("WITH x AS (SELECT 1 AS v) SELECT v FROM x UNION ALL SELECT v FROM x"),
        "with x <select 1> select v from x setop select v from x"
    );
    assert_eq!(
        q("WITH x AS (VALUES (1), (2)) SELECT * FROM x"),
        "with x <values 2> select * from x"
    );
    assert_eq!(
        q("WITH x AS (SELECT 1) VALUES (1)"),
        "with x <select 1> values 1"
    );
    assert_eq!(
        q("WITH x AS (SELECT 1) TABLE x"),
        "with x <select 1> select * from x"
    );
    assert_eq!(
        q("WITH x AS (SELECT 1) SELECT * FROM x ORDER BY 1 LIMIT 1"),
        "with x <select 1> select * from x order limit 1"
    );
}

#[test]
fn with_in_insert_select() {
    let Statement::Insert(i) = parse("INSERT INTO t WITH x AS (SELECT 1) SELECT * FROM x")
        .unwrap()
        .remove(0)
    else {
        panic!("not an insert");
    };
    let crate::sql::ast::InsertSource::Query(src) = i.source else {
        panic!("not a query source");
    };
    assert_eq!(qs(&src), "with x <select 1> select * from x");
    assert!(parse("INSERT INTO t (a) WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
}

#[test]
fn with_not_supported_and_errors() {
    unsupported(
        "WITH x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x",
        "data-modifying statements in WITH is not supported yet",
        12,
    );
    unsupported(
        "WITH x AS (UPDATE t SET a = 1) SELECT 1",
        "data-modifying statements in WITH is not supported yet",
        12,
    );
    unsupported(
        "WITH x AS (DELETE FROM t) SELECT 1",
        "data-modifying statements in WITH is not supported yet",
        12,
    );
    for (sql, pos) in [
        ("WITH x AS (SELECT 1) INSERT INTO t VALUES (1)", 22),
        ("WITH x AS (SELECT 1) UPDATE t SET a = 1", 22),
        ("WITH x AS (SELECT 1) DELETE FROM t", 22),
    ] {
        unsupported(
            sql,
            "WITH clause on INSERT, UPDATE or DELETE is not supported yet",
            pos,
        );
    }
    unsupported(
        "WITH x AS (SELECT 1) SEARCH DEPTH FIRST BY a SET o SELECT 1",
        "SEARCH and CYCLE clauses is not supported yet",
        22,
    );
    unsupported(
        "WITH x AS (SELECT 1) CYCLE a SET c USING p SELECT 1",
        "SEARCH and CYCLE clauses is not supported yet",
        22,
    );
    syntax("WITH x AS SELECT 1", "SELECT", 11);
    syntax("WITH x AS (SELECT 1)", "", 21);
}

// ----- FROM: functions, ONLY ---------------------------------------------------

#[test]
fn from_function() {
    assert_eq!(
        q("SELECT * FROM generate_series(1, 3)"),
        "select * from fn:generate_series(1 3)"
    );
    assert_eq!(
        q("SELECT * FROM pg_catalog.generate_series(1,2) AS g(x)"),
        "select * from fn:pg_catalog.generate_series(1 2) g(x)"
    );
    assert_eq!(q("SELECT * FROM f()"), "select * from fn:f()");
    assert_eq!(q("SELECT * FROM f() g"), "select * from fn:f() g");
    assert_eq!(
        q("SELECT * FROM generate_series(1, 2 + 3), t"),
        "select * from fn:generate_series(1 (+ 2 3)), t"
    );
    assert_eq!(
        q("SELECT * FROM generate_series(1, (SELECT 2)) g"),
        "select * from fn:generate_series(1 (subquery [select 2])) g"
    );
    assert_eq!(
        q("SELECT * FROM t JOIN generate_series(1, 2) g ON true"),
        "select * from (join t fn:generate_series(1 2) g)"
    );
    let query = query("SELECT * FROM generate_series(1, 3) AS g(x)");
    let TableRef::Function { span, args, .. } = &select_of(&query).from[0] else {
        panic!("not a function");
    };
    assert_eq!((span.start, span.end), (14, 43));
    assert_eq!(args.len(), 2);
}

#[test]
fn from_function_errors() {
    unsupported(
        "SELECT * FROM generate_series(1, 3) WITH ORDINALITY",
        "WITH ORDINALITY is not supported yet",
        37,
    );
    unsupported(
        "SELECT * FROM ROWS FROM (generate_series(1, 3))",
        "ROWS FROM is not supported yet",
        15,
    );
    // `rows` is an ordinary table name.
    assert_eq!(q("SELECT * FROM rows"), "select * from rows");
    assert_eq!(q("SELECT * FROM rows r"), "select * from rows r");
    syntax("SELECT * FROM g(1) AS g(x int)", "int", 27);
    unsupported(
        "SELECT * FROM t, LATERAL generate_series(1, t.a)",
        "LATERAL is not supported yet",
        18,
    );
}

#[test]
fn from_only_and_star() {
    assert_eq!(q("SELECT * FROM ONLY t"), "select * from t");
    assert_eq!(q("SELECT * FROM ONLY (t)"), "select * from t");
    assert_eq!(q("SELECT * FROM ONLY (s.t) x"), "select * from s.t x");
    assert_eq!(q("SELECT * FROM t *"), "select * from t");
    assert_eq!(q("SELECT * FROM t * x"), "select * from t x");
    assert!(matches!(
        parse("DELETE FROM ONLY t WHERE a = 1").unwrap().remove(0),
        Statement::Delete(_)
    ));
}

// ----- FILTER ------------------------------------------------------------------

#[test]
fn filter_clause() {
    assert_eq!(
        e("count(*) FILTER (WHERE a > 1)"),
        "count(*) filter (> a 1)"
    );
    assert_eq!(
        e("sum(a) FILTER (WHERE a > 1 AND b)"),
        "sum(a) filter (and (> a 1) b)"
    );
    assert_eq!(e("count(*)"), "count(*)");
    let Expr::Function { filter, span, .. } = expr("count(*) FILTER (WHERE a)") else {
        panic!();
    };
    assert!(filter.is_some());
    assert_eq!((span.start, span.end), (0, 25));
    // `filter` as a column alias / name is unaffected.
    assert_eq!(q("SELECT filter FROM t"), "select filter from t");
    assert!(parse("SELECT count(*) AS filter FROM t").is_ok());
    syntax("SELECT count(*) FILTER (a > 1) FROM t", "a", 25);
    unsupported(
        "SELECT sum(a) FILTER (WHERE a > 1) OVER () FROM t",
        "window functions is not supported yet",
        36,
    );
}

// ----- ANY / ALL ---------------------------------------------------------------

#[test]
fn quantified_subqueries() {
    assert_eq!(
        e("a = ANY (SELECT b FROM t)"),
        "(=-any a [select b from t])"
    );
    assert_eq!(
        e("a = SOME (SELECT b FROM t)"),
        "(=-any a [select b from t])"
    );
    assert_eq!(e("a < ALL (VALUES (1))"), "(<-all a [values 1])");
    assert_eq!(e("a <> ALL ((SELECT 1))"), "(<>-all a [select 1])");
    assert_eq!(
        e("a = ANY ((SELECT 1) UNION SELECT 2)"),
        "(=-any a [select 1 setop select 2])"
    );
    assert_eq!(
        e("a = ANY (WITH y AS (SELECT 1) SELECT * FROM y)"),
        "(=-any a [with y <select 1> select * from y])"
    );
    assert_eq!(e("a = ANY (TABLE t)"), "(=-any a [select * from t])");
    assert_eq!(e("1 + 2 = ANY (SELECT 1)"), "(=-any (+ 1 2) [select 1])");
    assert_eq!(
        e("a LIKE ANY (SELECT b FROM t)"),
        "(~~-any a [select b from t])"
    );
    assert_eq!(
        e("a ILIKE ANY (SELECT b FROM t)"),
        "(~~*-any a [select b from t])"
    );
    assert_eq!(e("a NOT LIKE ALL (SELECT 'x')"), "(!~~-all a [select 'x'])");
    assert_eq!(
        e("a NOT ILIKE ALL (SELECT 'x')"),
        "(!~~*-all a [select 'x'])"
    );
    assert_eq!(
        e("a OPERATOR(pg_catalog.=) ANY (SELECT 1)"),
        "(pg_catalog.=-any a [select 1])"
    );
    assert_eq!(e("a OPERATOR(=) ALL (SELECT 1)"), "(=-all a [select 1])");
    // IN stays an InSubquery.
    assert_eq!(e("a IN (SELECT 1)"), "(in a [select 1])");
    assert_eq!(e("a NOT IN (SELECT 1)"), "(notin a [select 1])");
    // Combines with surrounding operators.
    assert_eq!(
        e("a = ANY (SELECT 1) AND b"),
        "(and (=-any a [select 1]) b)"
    );
}

#[test]
fn quantified_subquery_spans() {
    let x = expr("1 + 2 = ANY (SELECT 1)");
    let Expr::QuantifiedSubquery { span, .. } = x else {
        panic!();
    };
    assert_eq!((span.start, span.end), (6, 22));
    let x = expr("a NOT LIKE ALL (SELECT 'x')");
    let Expr::QuantifiedSubquery { span, .. } = x else {
        panic!();
    };
    assert_eq!(span.start, 2);
}

#[test]
fn quantified_array_forms_are_not_supported() {
    unsupported(
        "SELECT a = ANY (ARRAY[1]) FROM t",
        "ANY/ALL with an array is not supported yet",
        12,
    );
    unsupported(
        "SELECT a = ANY (1) FROM t",
        "ANY/ALL with an array is not supported yet",
        12,
    );
    unsupported(
        "SELECT a < ALL (1, 2) FROM t",
        "ANY/ALL with an array is not supported yet",
        12,
    );
    unsupported(
        "SELECT a LIKE SOME (x) FROM t",
        "ANY/ALL with an array is not supported yet",
        15,
    );
    unsupported(
        "SELECT a = ANY ((1)) FROM t",
        "ANY/ALL with an array is not supported yet",
        12,
    );
}

// ----- COLLATE -----------------------------------------------------------------

#[test]
fn collate() {
    assert_eq!(e("'a' COLLATE \"C\""), "(collate 'a' C)");
    assert_eq!(
        e("'a' COLLATE pg_catalog.\"default\""),
        "(collate 'a' pg_catalog.default)"
    );
    // COLLATE binds tighter than `||` and `+`.
    assert_eq!(e("a || 'x' COLLATE \"C\""), "(|| a (collate 'x' C))");
    assert_eq!(
        e("a OPERATOR(pg_catalog.~) 'x' COLLATE pg_catalog.default"),
        "(pg_catalog.~ a (collate 'x' pg_catalog.default))"
    );
    assert_eq!(e("1 + a COLLATE x"), "(+ 1 (collate a x))");
    assert_eq!(e("a COLLATE x COLLATE y"), "(collate (collate a x) y)");
    assert_eq!(e("a COLLATE \"C\" = b"), "(= (collate a C) b)");
    let x = expr("a COLLATE \"C\"");
    let Expr::Collate {
        span, collation, ..
    } = x
    else {
        panic!();
    };
    assert_eq!((span.start, span.end), (2, 13));
    assert_eq!(collation.name().value, "C");
    // `default` is a reserved word.
    syntax("SELECT 'a' COLLATE default", "default", 20);
    assert!(parse("SELECT 'a' COLLATE pg_catalog.default").is_ok());
}

// ----- OPERATOR ----------------------------------------------------------------

#[test]
fn operator_syntax() {
    assert_eq!(e("1 OPERATOR(pg_catalog.+) 2"), "(pg_catalog.+ 1 2)");
    assert_eq!(e("1 OPERATOR(+) 2"), "(+ 1 2)");
    assert_eq!(e("a OPERATOR(pg_catalog.=) b"), "(pg_catalog.= a b)");
    assert_eq!(e("OPERATOR(pg_catalog.-) 1"), "(upg_catalog.- 1)");
    assert_eq!(e("OPERATOR(pg_catalog.~) a"), "(upg_catalog.~ a)");
    assert_eq!(e("a OPERATOR(pg_catalog.*) b"), "(pg_catalog.* a b)");
    // Precedence: the "other operator" level (9), between `*` and comparison.
    assert_eq!(
        e("1 OPERATOR(pg_catalog.+) 2 * 3"),
        "(pg_catalog.+ 1 (* 2 3))"
    );
    assert_eq!(
        e("1 + 2 OPERATOR(pg_catalog.*) 3"),
        "(pg_catalog.* (+ 1 2) 3)"
    );
    assert_eq!(
        e("1 OPERATOR(pg_catalog.+) 2 OPERATOR(pg_catalog.+) 3"),
        "(pg_catalog.+ (pg_catalog.+ 1 2) 3)"
    );
    assert_eq!(
        e("a = 1 OPERATOR(pg_catalog.+) 2"),
        "(= a (pg_catalog.+ 1 2))"
    );
    // `operator` is an ordinary name when no `(` follows.
    assert_eq!(q("SELECT operator FROM t"), "select operator from t");
    assert_eq!(q("SELECT a AS operator FROM t"), "select a from t");
    let x = expr("1 OPERATOR(pg_catalog.+) 2");
    let Expr::BinaryOp {
        span, op_schema, ..
    } = x
    else {
        panic!();
    };
    assert_eq!((span.start, span.end), (2, 26));
    assert_eq!(op_schema.unwrap().value, "pg_catalog");
    unsupported(
        "SELECT 1 OPERATOR(a.b.+) 2",
        "OPERATOR with a database-qualified name is not supported yet",
        19,
    );
    syntax("SELECT 1 OPERATOR(pg_catalog.) 2", ")", 30);
}

// ----- row values --------------------------------------------------------------

#[test]
fn row_values() {
    assert_eq!(e("(a, b)"), "(row a b)");
    assert_eq!(e("(a, b, c)"), "(row a b c)");
    assert_eq!(e("ROW(a, b)"), "(ROW a b)");
    assert_eq!(e("ROW()"), "(ROW )");
    assert_eq!(e("ROW(a)"), "(ROW a)");
    assert_eq!(e("(a)"), "a");
    assert_eq!(e("((a, b), c)"), "(row (row a b) c)");
    assert_eq!(e("((SELECT 1), 2)"), "(row (subquery [select 1]) 2)");
    assert_eq!(e("(a, b) IN (SELECT 1, 2)"), "(in (row a b) [select 1, 2])");
    assert_eq!(
        e("(a, b) = ANY (SELECT 1, 2)"),
        "(=-any (row a b) [select 1, 2])"
    );
    assert_eq!(
        e("ROW(a, b) <> ALL (SELECT 1, 2)"),
        "(<>-all (ROW a b) [select 1, 2])"
    );
    let Expr::Row { span, explicit, .. } = expr("(a, b)") else {
        panic!();
    };
    assert!(!explicit);
    assert_eq!((span.start, span.end), (0, 6));
    let Expr::BinaryOp { right, .. } = expr("1 = ROW(a)") else {
        panic!();
    };
    let Expr::Row { span, explicit, .. } = *right else {
        panic!();
    };
    assert!(explicit);
    assert_eq!((span.start, span.end), (4, 10));
}

// ----- GROUP BY special forms ---------------------------------------------------

#[test]
fn group_by_special_forms() {
    const MSG: &str = "GROUPING SETS / ROLLUP / CUBE / empty grouping sets are not supported yet";
    unsupported("SELECT 1 GROUP BY ()", MSG, 19);
    unsupported("SELECT a FROM t GROUP BY ROLLUP (a)", MSG, 26);
    unsupported("SELECT a FROM t GROUP BY a, CUBE (b)", MSG, 29);
    unsupported("SELECT a FROM t GROUP BY GROUPING SETS ((a))", MSG, 26);
    unsupported("SELECT a FROM t GROUP BY a, b, ()", MSG, 32);
    // Accepted: ALL / DISTINCT quantifiers, and names that look like the
    // special forms but are ordinary columns.
    assert_eq!(
        q("SELECT a FROM t GROUP BY ALL a"),
        "select a from t group a"
    );
    assert_eq!(
        q("SELECT a FROM t GROUP BY DISTINCT a, b"),
        "select a from t group a b"
    );
    assert_eq!(
        q("SELECT a FROM t GROUP BY rollup"),
        "select a from t group rollup"
    );
    assert_eq!(
        q("SELECT a FROM t GROUP BY a, cube"),
        "select a from t group a cube"
    );
}

// ----- regressions: M1 rejections that stay ------------------------------------

#[test]
fn unchanged_rejections() {
    unsupported(
        "SELECT * FROM t FOR UPDATE",
        "FOR UPDATE/SHARE is not supported yet",
        17,
    );
    unsupported(
        "SELECT * FROM t TABLESAMPLE system (10)",
        "TABLESAMPLE is not supported yet",
        17,
    );
    unsupported(
        "SELECT * FROM a JOIN b USING (x) AS j",
        "JOIN USING aliases is not supported yet",
        34,
    );
    unsupported(
        "SELECT * FROM (a JOIN b ON true) AS j",
        "aliases for parenthesized joins is not supported yet",
        34,
    );
    assert_eq!(
        err("SELECT count(*) OVER () FROM t").sqlstate,
        sqlstate::FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        err("SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY a) FROM t").sqlstate,
        sqlstate::FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        err("SELECT a FROM t WINDOW w AS ()").sqlstate,
        sqlstate::FEATURE_NOT_SUPPORTED
    );
}

#[test]
fn in_list_with_parenthesized_set_operation() {
    assert!(parse("SELECT 1 WHERE 1 IN ((SELECT 1) UNION ALL SELECT 2)").is_ok());
    assert!(
        parse("SELECT 1 WHERE 1 IN ((SELECT 1 UNION SELECT 2) EXCEPT SELECT 3 LIMIT 1)").is_ok()
    );
    assert!(parse("SELECT 1 WHERE 1 IN ((SELECT 1), 2)").is_ok());
}
