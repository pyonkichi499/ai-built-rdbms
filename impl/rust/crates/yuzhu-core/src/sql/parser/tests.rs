//! Parser unit tests. Expressions are compared through a compact
//! S-expression rendering; statements through pattern matching and
//! rendered pieces.

#![allow(clippy::format_push_string, clippy::too_many_lines)]

use super::{parse, parse_expr};
use crate::error::{Error, Span, sqlstate};
use crate::sql::ast::{
    BoolTestValue, CastSyntax, ColumnConstraintKind, Distinct, DropBehavior, Expr, InsertSource,
    JoinConstraint, JoinKind, Literal, NullsOrder, ParamTarget, Quantifier, Query, QueryBody,
    Select, SelectItem, SetArg, SetOperator, SetValue, SortDirection, Statement,
    TableConstraintKind, TableElement, TableRef, TransactionKind, TransactionMode, TypeName,
};

// ----- rendering --------------------------------------------------------

fn type_str(t: &TypeName) -> String {
    let mut s = t
        .names
        .iter()
        .map(|n| n.value.clone())
        .collect::<Vec<_>>()
        .join(".");
    if !t.modifiers.is_empty() {
        let m: Vec<String> = t.modifiers.iter().map(sexp).collect();
        s.push_str(&format!("({})", m.join(",")));
    }
    for b in &t.array_bounds {
        match b {
            Some(n) => s.push_str(&format!("[{n}]")),
            None => s.push_str("[]"),
        }
    }
    s
}

fn list(items: &[Expr]) -> String {
    items.iter().map(sexp).collect::<Vec<_>>().join(" ")
}

fn sexp(e: &Expr) -> String {
    match e {
        Expr::Literal { value, .. } => match value {
            Literal::Integer(s) => s.clone(),
            Literal::Decimal(s) => format!("{s}d"),
            Literal::String(s) => format!("'{s}'"),
            Literal::Bool(b) => b.to_string(),
            Literal::Null => "null".into(),
        },
        Expr::Column { parts, .. } => parts
            .iter()
            .map(|p| {
                if p.quoted {
                    format!("\"{}\"", p.value)
                } else {
                    p.value.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("."),
        Expr::Parameter { index, .. } => format!("${index}"),
        Expr::BinaryOp {
            op, left, right, ..
        } => format!("({op} {} {})", sexp(left), sexp(right)),
        Expr::UnaryOp { op, expr, .. } => format!("({op} {})", sexp(expr)),
        Expr::And { left, right, .. } => format!("(and {} {})", sexp(left), sexp(right)),
        Expr::Or { left, right, .. } => format!("(or {} {})", sexp(left), sexp(right)),
        Expr::Not { expr, .. } => format!("(not {})", sexp(expr)),
        Expr::IsNull { expr, negated, .. } => {
            format!(
                "({} {})",
                if *negated { "notnull" } else { "isnull" },
                sexp(expr)
            )
        }
        Expr::IsBool {
            expr,
            value,
            negated,
            ..
        } => {
            let v = match value {
                BoolTestValue::True => "true",
                BoolTestValue::False => "false",
                BoolTestValue::Unknown => "unknown",
            };
            format!(
                "(is{} {v} {})",
                if *negated { "not" } else { "" },
                sexp(expr)
            )
        }
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
            ..
        } => format!(
            "({}distinct {} {})",
            if *negated { "not" } else { "" },
            sexp(left),
            sexp(right)
        ),
        Expr::Cast {
            expr,
            type_name,
            syntax,
            ..
        } => {
            let kw = match syntax {
                CastSyntax::Cast => "cast",
                CastSyntax::DoubleColon => "::",
                CastSyntax::TypedLiteral => "typed",
            };
            format!("({kw} {} {})", sexp(expr), type_str(type_name))
        }
        Expr::Function {
            name,
            args,
            distinct,
            star,
            ..
        } => {
            let n = name
                .parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            let mut inner = String::new();
            if *distinct {
                inner.push_str("distinct ");
            }
            if *star {
                inner.push('*');
            }
            inner.push_str(&list(args));
            format!("{n}({inner})")
        }
        Expr::Case {
            operand,
            whens,
            else_result,
            ..
        } => {
            let mut s = "(case".to_string();
            if let Some(o) = operand {
                s.push_str(&format!(" {}", sexp(o)));
            }
            for w in whens {
                s.push_str(&format!(
                    " (when {} {})",
                    sexp(&w.condition),
                    sexp(&w.result)
                ));
            }
            if let Some(e) = else_result {
                s.push_str(&format!(" (else {})", sexp(e)));
            }
            s.push(')');
            s
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
            symmetric,
            ..
        } => format!(
            "({}between{} {} {} {})",
            if *negated { "not" } else { "" },
            if *symmetric { "-sym" } else { "" },
            sexp(expr),
            sexp(low),
            sexp(high)
        ),
        Expr::InList {
            expr,
            list: items,
            negated,
            ..
        } => format!(
            "({}in {} [{}])",
            if *negated { "not" } else { "" },
            sexp(expr),
            list(items)
        ),
        Expr::InSubquery {
            expr,
            query,
            negated,
            ..
        } => format!(
            "({}in {} {})",
            if *negated { "not" } else { "" },
            sexp(expr),
            query_str(query)
        ),
        Expr::Exists { query, .. } => format!("(exists {})", query_str(query)),
        Expr::Subquery { query, .. } => format!("(subquery {})", query_str(query)),
        Expr::Like {
            expr,
            pattern,
            escape,
            negated,
            case_insensitive,
            ..
        } => {
            let mut s = format!(
                "({}{} {} {}",
                if *negated { "not" } else { "" },
                if *case_insensitive { "ilike" } else { "like" },
                sexp(expr),
                sexp(pattern)
            );
            if let Some(e) = escape {
                s.push_str(&format!(" (escape {})", sexp(e)));
            }
            s.push(')');
            s
        }
        Expr::Coalesce { args, .. } => format!("(coalesce {})", list(args)),
        Expr::MinMax { greatest, args, .. } => {
            format!(
                "({} {})",
                if *greatest { "greatest" } else { "least" },
                list(args)
            )
        }
        Expr::NullIf { left, right, .. } => format!("(nullif {} {})", sexp(left), sexp(right)),
        Expr::SessionValue { kind, .. } => format!("<{}>", kind.column_name()),
        Expr::Default { .. } => "DEFAULT".into(),
        Expr::QuantifiedSubquery {
            expr,
            op,
            quantifier,
            query,
            ..
        } => format!(
            "({op}-{} {} {})",
            if *quantifier == Quantifier::All {
                "all"
            } else {
                "any"
            },
            sexp(expr),
            query_str(query)
        ),
        Expr::Collate {
            expr, collation, ..
        } => format!(
            "(collate {} {})",
            sexp(expr),
            collation
                .parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(".")
        ),
        Expr::Row { items, .. } => format!("(row {})", list(items)),
    }
}

fn select_str(s: &Select) -> String {
    let mut out = "select".to_string();
    match &s.distinct {
        Some(Distinct::All) => out.push_str(" distinct"),
        Some(Distinct::On(e)) => out.push_str(&format!(" distinct-on[{}]", list(e))),
        None => {}
    }
    let targets: Vec<String> = s
        .targets
        .iter()
        .map(|t| match t {
            SelectItem::Wildcard(_) => "*".to_string(),
            SelectItem::QualifiedWildcard(n, _) => format!(
                "{}.*",
                n.parts
                    .iter()
                    .map(|p| p.value.clone())
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            SelectItem::Expr { expr, alias, .. } => match alias {
                Some(a) => format!("{} as {}", sexp(expr), a.value),
                None => sexp(expr),
            },
        })
        .collect();
    out.push_str(&format!(" {}", targets.join(", ")));
    if !s.from.is_empty() {
        let f: Vec<String> = s.from.iter().map(table_str).collect();
        out.push_str(&format!(" from {}", f.join(", ")));
    }
    if let Some(w) = &s.selection {
        out.push_str(&format!(" where {}", sexp(w)));
    }
    if !s.group_by.is_empty() {
        out.push_str(&format!(" group [{}]", list(&s.group_by)));
    }
    if let Some(h) = &s.having {
        out.push_str(&format!(" having {}", sexp(h)));
    }
    out
}

fn table_str(t: &TableRef) -> String {
    match t {
        TableRef::Table { name, alias, .. } => {
            let mut s = name
                .parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(".");
            if let Some(a) = alias {
                s.push_str(&format!(" {}", a.name.value));
                if !a.columns.is_empty() {
                    let c: Vec<String> = a.columns.iter().map(|c| c.value.clone()).collect();
                    s.push_str(&format!("({})", c.join(",")));
                }
            }
            s
        }
        TableRef::Function { name, args, .. } => format!("{}({})", name.name().value, list(args)),
        TableRef::Subquery { query, alias, .. } => format!(
            "[{}]{}",
            query_str(query),
            alias
                .as_ref()
                .map(|a| format!(" {}", a.name.value))
                .unwrap_or_default()
        ),
        TableRef::Join {
            left,
            right,
            kind,
            constraint,
            ..
        } => {
            let k = match kind {
                JoinKind::Inner => "join",
                JoinKind::Left => "left",
                JoinKind::Right => "right",
                JoinKind::Full => "full",
                JoinKind::Cross => "cross",
            };
            let c = match constraint {
                JoinConstraint::On(e) => format!(" on {}", sexp(e)),
                JoinConstraint::Using(cols) => format!(
                    " using({})",
                    cols.iter()
                        .map(|c| c.value.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                JoinConstraint::Natural => " natural".into(),
                JoinConstraint::None => String::new(),
            };
            format!("({k} {} {}{c})", table_str(left), table_str(right))
        }
    }
}

fn body_str(b: &QueryBody) -> String {
    match b {
        QueryBody::Select(s) => select_str(s),
        QueryBody::Values(v) => {
            let rows: Vec<String> = v.rows.iter().map(|r| format!("({})", list(r))).collect();
            format!("values {}", rows.join(" "))
        }
        QueryBody::SetOp {
            op,
            all,
            left,
            right,
            ..
        } => {
            let o = match op {
                SetOperator::Union => "union",
                SetOperator::Intersect => "intersect",
                SetOperator::Except => "except",
            };
            format!(
                "{{{} {o}{} {}}}",
                body_str(left),
                if *all { " all" } else { "" },
                body_str(right)
            )
        }
        QueryBody::Nested(q) => format!("<{}>", query_str(q)),
    }
}

fn query_str(q: &Query) -> String {
    let mut s = body_str(&q.body);
    if !q.order_by.is_empty() {
        let items: Vec<String> = q
            .order_by
            .iter()
            .map(|o| {
                let mut x = sexp(&o.expr);
                match o.direction {
                    Some(SortDirection::Asc) => x.push_str(" asc"),
                    Some(SortDirection::Desc) => x.push_str(" desc"),
                    None => {}
                }
                match o.nulls {
                    Some(NullsOrder::First) => x.push_str(" nulls first"),
                    Some(NullsOrder::Last) => x.push_str(" nulls last"),
                    None => {}
                }
                x
            })
            .collect();
        s.push_str(&format!(" order {}", items.join(", ")));
    }
    if let Some(l) = &q.limit {
        s.push_str(&format!(" limit {}", sexp(l)));
    }
    if let Some(o) = &q.offset {
        s.push_str(&format!(" offset {}", sexp(o)));
    }
    s
}

// ----- helpers -----------------------------------------------------------

fn e(sql: &str) -> String {
    match parse_expr(sql) {
        Ok(x) => sexp(&x),
        Err(err) => panic!("parse_expr({sql:?}) failed: {}", err.message),
    }
}

fn one(sql: &str) -> Statement {
    let mut v = parse(sql).unwrap_or_else(|err| panic!("parse({sql:?}) failed: {}", err.message));
    assert_eq!(v.len(), 1, "{sql}");
    v.remove(0)
}

fn q(sql: &str) -> String {
    match one(sql) {
        Statement::Query(query) => query_str(&query),
        other => panic!("not a query: {other:?}"),
    }
}

/// Parse error with its position resolved (1-based characters).
fn err(sql: &str) -> Error {
    match parse(sql) {
        Ok(v) => panic!("expected an error for {sql:?}, got {v:?}"),
        Err(mut e) => {
            e.resolve_position(sql);
            e
        }
    }
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

fn unsupported(sql: &str) {
    let e = err(sql);
    assert_eq!(
        e.sqlstate,
        sqlstate::FEATURE_NOT_SUPPORTED,
        "{sql}: {}",
        e.message
    );
}

// ----- expressions ---------------------------------------------------------

#[test]
fn literals() {
    assert_eq!(e("1"), "1");
    assert_eq!(e("1.5"), "1.5d");
    assert_eq!(e("'a''b'"), "'a'b'");
    assert_eq!(e("TRUE"), "true");
    assert_eq!(e("false"), "false");
    assert_eq!(e("NULL"), "null");
    assert_eq!(e("E'x\\ty'"), "'x\ty'");
    assert_eq!(e("'a'\n'b'"), "'ab'");
    assert_eq!(e("2147483648"), "2147483648");
    assert_eq!(e("0x1F"), "31");
    assert_eq!(e("1_000"), "1000");
    assert_eq!(e("$1"), "$1");
}

#[test]
fn negative_literal_folding() {
    assert_eq!(e("-1"), "-1");
    assert_eq!(e("- 1.5"), "-1.5d");
    assert_eq!(e("-(1)"), "-1");
    assert_eq!(e("- -1"), "1");
    assert_eq!(e("-0"), "0");
    assert_eq!(e("+1"), "(+ 1)");
    assert_eq!(e("-a"), "(- a)");
    // `::` binds tighter than unary minus: not folded.
    assert_eq!(e("-2147483648::int4"), "(- (:: 2147483648 int4))");
    assert_eq!(e("(-2147483648)::int4"), "(:: -2147483648 int4)");
    // The folded literal starts at the minus sign.
    let x = parse_expr("  - 5").unwrap();
    assert_eq!(x.span(), Span::new(2, 5));
}

#[test]
fn arithmetic_precedence() {
    assert_eq!(e("2 + 3 * 4"), "(+ 2 (* 3 4))");
    assert_eq!(e("(2 + 3) * 4"), "(* (+ 2 3) 4)");
    assert_eq!(e("10 - 3 - 2"), "(- (- 10 3) 2)");
    assert_eq!(e("100 / 10 / 5"), "(/ (/ 100 10) 5)");
    assert_eq!(e("2 * 7 % 4"), "(% (* 2 7) 4)");
    assert_eq!(e("2 ^ 3 ^ 2"), "(^ (^ 2 3) 2)");
    assert_eq!(e("-2 * 3"), "(* -2 3)");
    assert_eq!(e("- 2 + 3"), "(+ -2 3)");
    assert_eq!(e("-a ^ 2"), "(^ (- a) 2)");
    assert_eq!(e("1 / 2::float8"), "(/ 1 (:: 2 float8))");
    assert_eq!(e("a::int::text"), "(:: (:: a int4) text)");
    assert_eq!(e("1+-2"), "(+ 1 -2)");
}

#[test]
fn concat_and_generic_operators() {
    assert_eq!(e("'a' || 1 + 2"), "(|| 'a' (+ 1 2))");
    assert_eq!(e("'ab' || 'c' = 'abc'"), "(= (|| 'ab' 'c') 'abc')");
    assert_eq!(e("a || b || c"), "(|| (|| a b) c)");
    assert_eq!(e("@ -5"), "(@ -5)");
    assert_eq!(e("|/ 25 + 1"), "(|/ (+ 25 1))");
    assert_eq!(e("a != b"), "(<> a b)");
}

#[test]
fn boolean_precedence() {
    assert_eq!(e("1 + 1 = 2"), "(= (+ 1 1) 2)");
    assert_eq!(e("NOT 1 = 2"), "(not (= 1 2))");
    assert_eq!(e("NOT false AND false"), "(and (not false) false)");
    assert_eq!(e("true OR true AND false"), "(or true (and true false))");
    assert_eq!(e("false AND false OR true"), "(or (and false false) true)");
    assert_eq!(e("NOT NOT a"), "(not (not a))");
    assert_eq!(e("1 + NOT a = b"), "(+ 1 (not (= a b)))");
    assert_eq!(e("a AND NOT b OR c"), "(or (and a (not b)) c)");
}

#[test]
fn is_tests() {
    assert_eq!(e("1 = 1 IS TRUE"), "(is true (= 1 1))");
    assert_eq!(e("NULL = 1 IS NULL"), "(isnull (= null 1))");
    assert_eq!(e("a IS NOT NULL"), "(notnull a)");
    assert_eq!(e("a ISNULL"), "(isnull a)");
    assert_eq!(e("a NOTNULL"), "(notnull a)");
    assert_eq!(e("a IS NOT FALSE"), "(isnot false a)");
    assert_eq!(e("a IS UNKNOWN"), "(is unknown a)");
    assert_eq!(e("NOT a IS NULL"), "(not (isnull a))");
    assert_eq!(e("a IS NULL = b"), "(= (isnull a) b)");
    assert_eq!(e("a IS NULL IS NULL"), "(isnull (isnull a))");
    assert_eq!(e("a IS DISTINCT FROM b + 1"), "(distinct a (+ b 1))");
    assert_eq!(e("a IS NOT DISTINCT FROM b"), "(notdistinct a b)");
    // The span starts at IS (PostgreSQL's location).
    let x = parse_expr("a IS NULL").unwrap();
    assert_eq!(x.span(), Span::new(2, 9));
}

#[test]
fn non_associative_comparisons() {
    syntax("SELECT 1 < 2 < 3", "<", 14);
    syntax("SELECT 1 = 1 = true", "=", 14);
    syntax("SELECT 1 < 2 + 3 < 4", "<", 18);
    syntax("SELECT a LIKE b LIKE c", "LIKE", 17);
    syntax("SELECT a BETWEEN 1 AND 2 BETWEEN 3 AND 4", "BETWEEN", 26);
    syntax("SELECT a IS DISTINCT FROM b IS NULL", "IS", 29);
    assert_eq!(e("(1 < 2) = true"), "(= (< 1 2) true)");
    // IN ends with ')' so it can be followed by another IN.
    assert_eq!(e("1 IN (1) IN (true)"), "(in (in 1 [1]) [true])");
}

#[test]
fn between_in_like() {
    assert_eq!(
        e("5 BETWEEN 1 + 1 AND 2 * 3"),
        "(between 5 (+ 1 1) (* 2 3))"
    );
    assert_eq!(e("a NOT BETWEEN 1 AND 2"), "(notbetween a 1 2)");
    assert_eq!(e("a BETWEEN SYMMETRIC 2 AND 1"), "(between-sym a 2 1)");
    assert_eq!(e("a BETWEEN 1 AND 2 AND b"), "(and (between a 1 2) b)");
    assert_eq!(e("a BETWEEN 1 AND 2 = true"), "(= (between a 1 2) true)");
    assert_eq!(e("a IN (1, 2, 3)"), "(in a [1 2 3])");
    assert_eq!(e("a NOT IN ('x')"), "(notin a ['x'])");
    assert_eq!(e("a = b IN (1)"), "(= a (in b [1]))");
    assert_eq!(e("a LIKE 'x%'"), "(like a 'x%')");
    assert_eq!(
        e("a NOT ILIKE 'x' ESCAPE '!'"),
        "(notilike a 'x' (escape '!'))"
    );
    assert_eq!(
        e("a || 'b' LIKE 'c' || 'd'"),
        "(like (|| a 'b') (|| 'c' 'd'))"
    );
    assert_eq!(e("NOT a LIKE b"), "(not (like a b))");
    let x = parse_expr("a NOT IN (1)").unwrap();
    assert_eq!(x.span(), Span::new(2, 12));
    syntax("SELECT a IN ()", ")", 14);
    syntax("SELECT a BETWEEN 1", "", 19);
}

#[test]
fn case_cast_coalesce() {
    assert_eq!(
        e("CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END"),
        "(case (when a 1) (when b 2) (else 3))"
    );
    assert_eq!(e("CASE x WHEN 1 THEN 'a' END"), "(case x (when 1 'a'))");
    assert_eq!(e("CAST(1 AS text)"), "(cast 1 text)");
    assert_eq!(e("CAST('1' AS integer)"), "(cast '1' int4)");
    assert_eq!(e("COALESCE(a, 1, 'x')"), "(coalesce a 1 'x')");
    assert_eq!(e("NULLIF(a, 0)"), "(nullif a 0)");
    syntax("SELECT CASE END", "END", 13);
    syntax("SELECT COALESCE()", ")", 17);
}

#[test]
fn type_names() {
    let cases = [
        ("int", "int4"),
        ("integer", "int4"),
        ("smallint", "int2"),
        ("bigint", "int8"),
        ("real", "float4"),
        ("float", "float8"),
        ("float(24)", "float4"),
        ("float(25)", "float8"),
        ("double precision", "float8"),
        ("boolean", "bool"),
        ("bool", "bool"),
        ("int4", "int4"),
        ("text", "text"),
        ("\"integer\"", "integer"),
        ("varchar", "varchar"),
        ("varchar(10)", "varchar(10)"),
        ("character varying(5)", "varchar(5)"),
        ("char varying", "varchar"),
        ("char", "bpchar(1)"),
        ("character(3)", "bpchar(3)"),
        ("national character(2)", "bpchar(2)"),
        ("numeric(10, 2)", "numeric(10,2)"),
        ("decimal", "numeric"),
        ("timestamp", "timestamp"),
        ("timestamp(3) with time zone", "timestamptz(3)"),
        ("timestamp without time zone", "timestamp"),
        ("time with time zone", "timetz"),
        ("interval", "interval"),
        ("int[]", "int4[]"),
        ("int[3][]", "int4[3][]"),
        ("int array", "int4[]"),
        ("int array[4]", "int4[4]"),
        ("pg_catalog.int4", "pg_catalog.int4"),
        ("bit", "bit(1)"),
        ("bit varying(4)", "varbit(4)"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            e(&format!("x::{input}")),
            format!("(:: x {expected})"),
            "{input}"
        );
    }
    let err0 = parse("SELECT 1::float(0)").unwrap_err();
    assert_eq!(err0.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
    assert_eq!(
        err0.message,
        "precision for type float must be at least 1 bit"
    );
    let e54 = err("SELECT 1::float(54)");
    assert_eq!(
        e54.message,
        "precision for type float must be less than 54 bits"
    );
    assert_eq!(e54.position, Some(17));
    syntax("SELECT 1::double", "", 17);
    syntax("SELECT 1::varchar(-1)", "-", 19);
    syntax("SELECT 1::varchar(99999999999)", "99999999999", 19);
    syntax("SELECT 1::select", "select", 11);
}

#[test]
fn typed_literals() {
    assert_eq!(e("int4 '1'"), "(typed '1' int4)");
    assert_eq!(e("integer '1'"), "(typed '1' int4)");
    assert_eq!(e("double precision '1.5'"), "(typed '1.5' float8)");
    assert_eq!(e("varchar(3) 'abcd'"), "(typed 'abcd' varchar(3))");
    assert_eq!(e("char 'abc'"), "(typed 'abc' bpchar)");
    assert_eq!(e("boolean 't'"), "(typed 't' bool)");
    assert_eq!(e("pg_catalog.text 'x'"), "(typed 'x' pg_catalog.text)");
    assert_eq!(e("N'abc'"), "(typed 'abc' bpchar)");
    assert_eq!(e("int4(5) '1'"), "(typed '1' int4(5))");
    // Column-name keywords that are type names are columns otherwise.
    assert_eq!(e("time"), "time");
    assert_eq!(e("int + 1"), "(+ int 1)");
    // The literal keeps its own span (input errors point at it).
    let Expr::Cast { expr, syntax, .. } = parse_expr("int4 'x'").unwrap() else {
        panic!()
    };
    assert_eq!(syntax, CastSyntax::TypedLiteral);
    assert_eq!(expr.span(), Span::new(5, 8));
}

#[test]
fn columns_and_functions() {
    assert_eq!(e("a"), "a");
    assert_eq!(e("T.A"), "t.a");
    assert_eq!(e("\"T\".\"A\""), "\"T\".\"A\"");
    assert_eq!(e("s.t.c"), "s.t.c");
    assert_eq!(e("t.select"), "t.select");
    assert_eq!(e("name"), "name");
    assert_eq!(e("lower('X')"), "lower('X')");
    assert_eq!(e("pg_catalog.lower(a)"), "pg_catalog.lower(a)");
    assert_eq!(e("version()"), "version()");
    assert_eq!(e("count(*)"), "count(*)");
    assert_eq!(e("count(DISTINCT a)"), "count(distinct a)");
    assert_eq!(e("left('abc', 2)"), "left('abc' 2)");
    assert_eq!(e("current_schema()"), "current_schema()");
    assert_eq!(e("current_database()"), "current_database()");
    assert_eq!(e("\"lower\"('A')"), "lower('A')");
    assert_eq!(e("substring('abc' FROM 2 FOR 1)"), "substring('abc' 2 1)");
    assert_eq!(e("substring('abc' FOR 2)"), "substring('abc' 1 2)");
    assert_eq!(e("substring('abc', 2)"), "substring('abc' 2)");
    assert_eq!(e("position('b' IN 'abc')"), "position('abc' 'b')");
    assert_eq!(e("trim(both 'x' FROM 'xax')"), "btrim('xax' 'x')");
    assert_eq!(e("trim(leading FROM ' a')"), "ltrim(' a')");
    assert_eq!(e("trim(' a ')"), "btrim(' a ')");
    assert_eq!(e("extract(year FROM x)"), "extract('year' x)");
    syntax("SELECT left FROM t", "left", 8);
    syntax("SELECT int(1)", "(", 11);
}

#[test]
fn session_values() {
    assert_eq!(e("current_user"), "<current_user>");
    assert_eq!(e("CURRENT_ROLE"), "<current_role>");
    assert_eq!(e("session_user"), "<session_user>");
    assert_eq!(e("user"), "<current_user>");
    assert_eq!(e("current_catalog"), "<current_catalog>");
    assert_eq!(e("current_schema"), "<current_schema>");
    syntax("SELECT current_user()", "(", 20);
}

#[test]
fn subqueries() {
    assert_eq!(e("(SELECT 1)"), "(subquery select 1)");
    assert_eq!(e("((SELECT 1))"), "(subquery select 1)");
    assert_eq!(e("EXISTS (SELECT 1)"), "(exists select 1)");
    assert_eq!(e("a IN (SELECT b FROM t)"), "(in a select b from t)");
    assert_eq!(
        e("((SELECT 1) UNION SELECT 2)"),
        "(subquery {select 1 union select 2})"
    );
    assert_eq!(e("(SELECT 1) + 1"), "(+ (subquery select 1) 1)");
    syntax("SELECT EXISTS (1)", "1", 16);
}

#[test]
fn not_supported_expressions() {
    unsupported("SELECT a[1] FROM t");
    unsupported("SELECT ARRAY[1]");
    unsupported("SELECT count(*) OVER () FROM t");
    unsupported("SELECT f(x => 1)");
    unsupported("SELECT a SIMILAR TO 'x' FROM t");
    unsupported("SELECT count(t.*) FROM t");
}

#[test]
fn default_keyword_context() {
    // DEFAULT is allowed only at the top level of INSERT ... VALUES rows
    // and UPDATE ... SET.
    let e1 = err("INSERT INTO t VALUES (DEFAULT + 1)");
    assert_eq!(e1.sqlstate, sqlstate::SYNTAX_ERROR);
    assert_eq!(e1.message, "DEFAULT is not allowed in this context");
    assert_eq!(e1.position, Some(23));
    let e2 = err("SELECT DEFAULT");
    assert_eq!(e2.message, "DEFAULT is not allowed in this context");
    assert_eq!(e2.position, Some(8));
    assert!(parse("INSERT INTO t VALUES (DEFAULT, 1), (2, DEFAULT)").is_ok());
    assert!(parse("INSERT INTO t VALUES ((DEFAULT))").is_ok());
    assert!(parse("UPDATE t SET a = DEFAULT").is_ok());
    assert_eq!(
        err("INSERT INTO t SELECT DEFAULT").message,
        "DEFAULT is not allowed in this context"
    );
    assert_eq!(
        err("VALUES (DEFAULT)").message,
        "DEFAULT is not allowed in this context"
    );
    // A syntax error elsewhere in the string wins.
    syntax("SELECT DEFAULT; SELEC 1", "SELEC", 17);
}

// ----- syntax errors -------------------------------------------------------

#[test]
fn syntax_error_messages_and_positions() {
    syntax("SELEC 1", "SELEC", 1);
    syntax("SELECT 1 +", "", 11);
    syntax("SELECT 1 +   ", "", 14);
    syntax("SELECT (1", "", 10);
    syntax("SELECT 1)", ")", 9);
    syntax("SELECT * FROM", "", 14);
    syntax("INSERT INTO err_t VALUES", "", 25);
    syntax("CREATE TABLE err_bad (a int", "", 28);
    syntax("SELECT a b c FROM err_t", "c", 12);
    syntax("SELECT 1 FROM err_t WHERE", "", 26);
    syntax("SELECT 1 ORDER BY", "", 18);
    syntax("DROP TABLE", "", 11);
    syntax("CREATE TABLE t (select int)", "select", 17);
    syntax("CREATE TABLE t (a int,)", ")", 23);
    syntax("SELECT 'a' 'b'", "'b'", 12);
    syntax("SELECT 1 FROM t WHERE a = 1 b", "b", 29);
    syntax("SELECT é FROM", "", 14);
    syntax("SELECT 'é' x y", "y", 14);
    syntax("SELECT 1; SELECT (", "", 19);
    syntax("CREATE TABLE t (a int NULL NOT)", ")", 31);
    syntax("SELECT $ 1", "$", 8);
    syntax("BEGIN; SELEC 1", "SELEC", 8);
    syntax("SELECT 1 FROM t LIMIT 1 LIMIT 2", "LIMIT", 25);
    syntax("SELECT DISTINCT FROM t", "FROM", 17);
}

#[test]
fn lexer_errors_are_reported_lazily() {
    let e1 = err("SELECT 'unterminated");
    assert_eq!(
        e1.message,
        "unterminated quoted string at or near \"'unterminated\""
    );
    assert_eq!(e1.position, Some(8));
    // A syntax error before the lexer error wins.
    syntax("SELEC 'unterminated", "SELEC", 1);
    let e2 = err("SELECT 1 AS \"\"");
    assert_eq!(
        e2.message,
        "zero-length delimited identifier at or near \"\"\"\""
    );
}

#[test]
fn limit_errors() {
    let e1 = err("SELECT 1 LIMIT 1, 2");
    assert_eq!(e1.message, "LIMIT #,# syntax is not supported");
    assert_eq!(
        e1.hint.as_deref(),
        Some("Use separate LIMIT and OFFSET clauses.")
    );
    assert_eq!(e1.position, Some(10));
    let e2 = err("(SELECT 1 LIMIT 1) LIMIT 2");
    assert_eq!(e2.message, "multiple LIMIT clauses not allowed");
    assert_eq!(e2.position, Some(26));
}

// ----- statements ----------------------------------------------------------

#[test]
fn empty_and_multiple_statements() {
    assert!(parse("").unwrap().is_empty());
    assert!(
        parse("  -- only a comment\n /* and /* nested */ */ ")
            .unwrap()
            .is_empty()
    );
    assert!(parse(";;").unwrap().is_empty());
    assert_eq!(parse("SELECT 1; SELECT 2;").unwrap().len(), 2);
    assert_eq!(
        parse("BEGIN; INSERT INTO t VALUES (1); COMMIT")
            .unwrap()
            .len(),
        3
    );
    let stmts = parse("SELECT 1 ; SELECT 2").unwrap();
    assert_eq!(stmts[0].span(), Span::new(0, 8));
    assert_eq!(stmts[1].span(), Span::new(11, 19));
}

#[test]
fn select_basics() {
    assert_eq!(q("SELECT 1"), "select 1");
    assert_eq!(q("SELECT"), "select ");
    assert_eq!(q("SELECT FROM t"), "select  from t");
    assert_eq!(
        q("SELECT *, t.*, s.t.* FROM s.t"),
        "select *, t.*, s.t.* from s.t"
    );
    assert_eq!(
        q("SELECT a AS x, b y, c AS \"Z\", d FROM t"),
        "select a as x, b as y, c as Z, d from t"
    );
    assert_eq!(
        q("SELECT 1 AS select, 2 AS from"),
        "select 1 as select, 2 as from"
    );
    assert_eq!(q("SELECT 1 name, 2 value"), "select 1 as name, 2 as value");
    assert_eq!(q("SELECT DISTINCT a FROM t"), "select distinct a from t");
    assert_eq!(q("SELECT ALL a FROM t"), "select a from t");
    assert_eq!(
        q("SELECT DISTINCT ON (a) a, b FROM t"),
        "select distinct-on[a] a, b from t"
    );
    assert_eq!(
        q("SELECT a FROM t WHERE a > 1"),
        "select a from t where (> a 1)"
    );
    assert_eq!(q("SELECT a FROM t AS x"), "select a from t x");
    assert_eq!(q("SELECT a FROM t x(c, d)"), "select a from t x(c,d)");
    assert_eq!(q("SELECT a FROM public.t"), "select a from public.t");
    assert_eq!(q("TABLE t"), "select * from t");
    assert_eq!(q("SELECT 1 FROM a, b"), "select 1 from a, b");
    syntax("SELECT 1 FROM t AS select", "select", 20);
}

#[test]
fn order_limit_offset() {
    assert_eq!(
        q("SELECT a FROM t ORDER BY a DESC NULLS FIRST, 2, b ASC NULLS LAST, c NULLS LAST"),
        "select a from t order a desc nulls first, 2, b asc nulls last, c nulls last"
    );
    assert_eq!(q("SELECT 1 LIMIT 2 OFFSET 3"), "select 1 limit 2 offset 3");
    assert_eq!(q("SELECT 1 OFFSET 3 LIMIT 2"), "select 1 limit 2 offset 3");
    assert_eq!(q("SELECT 1 LIMIT ALL"), "select 1");
    assert_eq!(q("SELECT 1 LIMIT NULL"), "select 1 limit null");
    assert_eq!(q("SELECT 1 LIMIT -1"), "select 1 limit -1");
    assert_eq!(
        q("SELECT 1 OFFSET 1 ROWS FETCH FIRST 2 ROWS ONLY"),
        "select 1 limit 2 offset 1"
    );
    assert_eq!(q("SELECT 1 FETCH NEXT ROW ONLY"), "select 1 limit 1");
    assert_eq!(
        q("SELECT a AS b FROM t ORDER BY b + 1"),
        "select a as b from t order (+ b 1)"
    );
    unsupported("SELECT 1 FETCH FIRST 1 ROW WITH TIES");
    unsupported("SELECT * FROM t FOR UPDATE");
}

#[test]
fn values_and_set_operations() {
    assert_eq!(q("VALUES (1, 'a'), (2, 'b')"), "values (1 'a') (2 'b')");
    assert_eq!(
        q("VALUES (1) ORDER BY 1 LIMIT 1"),
        "values (1) order 1 limit 1"
    );
    assert_eq!(q("SELECT 1 UNION SELECT 2"), "{select 1 union select 2}");
    assert_eq!(
        q("SELECT 1 UNION ALL SELECT 2 INTERSECT SELECT 3"),
        "{select 1 union all {select 2 intersect select 3}}"
    );
    assert_eq!(
        q("SELECT 1 EXCEPT SELECT 2 UNION SELECT 3 ORDER BY 1"),
        "{{select 1 except select 2} union select 3} order 1"
    );
    assert_eq!(q("(SELECT 1)"), "select 1");
    assert_eq!(q("((SELECT 1)) ORDER BY 1"), "select 1 order 1");
    assert_eq!(
        q("(SELECT 1 ORDER BY 1 LIMIT 1) UNION (SELECT 2)"),
        "{<select 1 order 1 limit 1> union select 2}"
    );
    assert_eq!(
        q("(SELECT 1 ORDER BY 1) LIMIT 2"),
        "select 1 order 1 limit 2"
    );
    syntax("VALUES ()", ")", 9);
    syntax("SELECT 1 UNION", "", 15);
}

#[test]
fn joins_group_by_subqueries() {
    assert_eq!(
        q("SELECT * FROM a JOIN b ON a.x = b.x LEFT OUTER JOIN c USING (x)"),
        "select * from (left (join a b on (= a.x b.x)) c using(x))"
    );
    assert_eq!(
        q("SELECT * FROM a CROSS JOIN b NATURAL FULL JOIN c"),
        "select * from (full (cross a b) c natural)"
    );
    assert_eq!(
        q("SELECT * FROM a JOIN b JOIN c ON p ON q"),
        "select * from (join a (join b c on p) on q)"
    );
    assert_eq!(
        q("SELECT * FROM a INNER JOIN b ON true RIGHT JOIN c ON false"),
        "select * from (right (join a b on true) c on false)"
    );
    assert_eq!(
        q("SELECT * FROM (a JOIN b ON true)"),
        "select * from (join a b on true)"
    );
    assert_eq!(
        q("SELECT * FROM (SELECT 1) AS s"),
        "select * from [select 1] s"
    );
    assert_eq!(
        q("SELECT * FROM (SELECT 1) s"),
        "select * from [select 1] s"
    );
    assert_eq!(
        q("SELECT * FROM ((SELECT 1)) s"),
        "select * from [select 1] s"
    );
    assert_eq!(
        q("SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1"),
        "select a, count(*) from t group [a] having (> count(*) 1)"
    );
    syntax("SELECT * FROM a JOIN b", "", 23);
    syntax("SELECT * FROM (a)", ")", 17);
    unsupported("SELECT * FROM t, LATERAL (SELECT 1) s");
}

#[test]
fn create_table() {
    let Statement::CreateTable(ct) = one("CREATE TABLE IF NOT EXISTS public.T (\
           id int NOT NULL, \
           name varchar(10) DEFAULT 'x' || 'y' NULL, \
           n int DEFAULT -1 NOT NULL CHECK (n > 0), \
           CONSTRAINT pos CHECK ( (n < 100) ), \
           f float8 CONSTRAINT nn NOT NULL)")
    else {
        panic!()
    };
    assert!(ct.if_not_exists);
    assert_eq!(ct.name.parts.len(), 2);
    assert_eq!(ct.name.name().value, "t");
    assert_eq!(ct.elements.len(), 5);
    let TableElement::Column(id) = &ct.elements[0] else {
        panic!()
    };
    assert_eq!(id.name.value, "id");
    assert_eq!(type_str(&id.type_name), "int4");
    assert_eq!(id.constraints[0].kind, ColumnConstraintKind::NotNull);
    let TableElement::Column(name) = &ct.elements[1] else {
        panic!()
    };
    assert_eq!(type_str(&name.type_name), "varchar(10)");
    let ColumnConstraintKind::Default(d) = &name.constraints[0].kind else {
        panic!()
    };
    assert_eq!(d.text, "'x' || 'y'");
    assert_eq!(sexp(&d.expr), "(|| 'x' 'y')");
    assert_eq!(name.constraints[1].kind, ColumnConstraintKind::Null);
    let TableElement::Column(n) = &ct.elements[2] else {
        panic!()
    };
    let ColumnConstraintKind::Default(d) = &n.constraints[0].kind else {
        panic!()
    };
    assert_eq!(d.text, "-1");
    assert_eq!(n.constraints[1].kind, ColumnConstraintKind::NotNull);
    let ColumnConstraintKind::Check(c) = &n.constraints[2].kind else {
        panic!()
    };
    assert_eq!(c.text, "n > 0");
    let TableElement::Constraint(tc) = &ct.elements[3] else {
        panic!()
    };
    assert_eq!(tc.name.as_ref().unwrap().value, "pos");
    let TableConstraintKind::Check(c) = &tc.kind else {
        panic!()
    };
    assert_eq!(c.text, "(n < 100)");
    // The stored text re-parses to the same expression.
    assert_eq!(sexp(&parse_expr(&c.text).unwrap()), sexp(&c.expr));
    let TableElement::Column(f) = &ct.elements[4] else {
        panic!()
    };
    assert_eq!(f.constraints[0].name.as_ref().unwrap().value, "nn");
}

#[test]
fn create_table_variants() {
    let Statement::CreateTable(ct) = one("CREATE TABLE t ()") else {
        panic!()
    };
    assert!(ct.elements.is_empty());
    // DEFAULT takes a b_expr: `NULL NOT NULL` is a default plus a constraint.
    let Statement::CreateTable(ct) = one("CREATE TABLE t (a int DEFAULT NULL NOT NULL)") else {
        panic!()
    };
    let TableElement::Column(a) = &ct.elements[0] else {
        panic!()
    };
    assert_eq!(a.constraints.len(), 2);
    assert_eq!(a.constraints[1].kind, ColumnConstraintKind::NotNull);
    // Non-reserved keywords as column names; quoted reserved words.
    assert!(
        parse("CREATE TABLE t (name text, value int, type int, key int, text text, time int)")
            .is_ok()
    );
    assert!(parse("CREATE TABLE t (\"select\" int, \"from\" text)").is_ok());
    assert!(parse("CREATE TABLE if (a int)").is_ok());
    // PRIMARY KEY / UNIQUE / REFERENCES parse (rejected by the analyzer).
    let Statement::CreateTable(ct) = one(
        "CREATE TABLE t (a int PRIMARY KEY, b int UNIQUE REFERENCES u (x) ON DELETE CASCADE, \
         c int, PRIMARY KEY (a, b), UNIQUE (c), FOREIGN KEY (c) REFERENCES u)",
    ) else {
        panic!()
    };
    let TableElement::Column(a) = &ct.elements[0] else {
        panic!()
    };
    assert!(matches!(
        a.constraints[0].kind,
        ColumnConstraintKind::PrimaryKey(_)
    ));
    let TableElement::Column(b) = &ct.elements[1] else {
        panic!()
    };
    assert!(matches!(
        b.constraints[0].kind,
        ColumnConstraintKind::Unique(_)
    ));
    assert!(matches!(
        b.constraints[1].kind,
        ColumnConstraintKind::References { .. }
    ));
    let TableElement::Constraint(pk) = &ct.elements[3] else {
        panic!()
    };
    assert!(matches!(&pk.kind, TableConstraintKind::PrimaryKey(c) if c.columns.len() == 2));
    assert!(
        matches!(&ct.elements[5], TableElement::Constraint(c) if matches!(c.kind, TableConstraintKind::ForeignKey { .. }))
    );
    // Both NULL and NOT NULL parse (the analyzer reports the conflict).
    assert!(parse("CREATE TABLE t (a int NULL NOT NULL)").is_ok());
    // Column reference in DEFAULT parses (the analyzer rejects it).
    assert!(parse("CREATE TABLE t (a int, b int DEFAULT a)").is_ok());
    syntax("CREATE TABLE t (a int DEFAULT 1 AND true)", "AND", 33);
    syntax("CREATE TABLE t (a)", ")", 18);
    syntax("CREATE TABLE t (a int CONSTRAINT c)", ")", 35);
    unsupported("CREATE TEMP TABLE t (a int)");
    unsupported("CREATE TABLE t AS SELECT 1");
    unsupported("CREATE TABLE t (a int GENERATED ALWAYS AS (1) STORED)");
    syntax("CREATE foo", "foo", 8);
}

#[test]
fn drop_table() {
    let Statement::DropTable(d) = one("DROP TABLE IF EXISTS a, s.b CASCADE") else {
        panic!()
    };
    assert!(d.if_exists);
    assert_eq!(d.names.len(), 2);
    assert_eq!(d.behavior, Some(DropBehavior::Cascade));
    let Statement::DropTable(d) = one("DROP TABLE a") else {
        panic!()
    };
    assert!(!d.if_exists);
    assert_eq!(d.behavior, None);
}

#[test]
fn insert() {
    let Statement::Insert(i) = one("INSERT INTO t (a, b) VALUES (1, DEFAULT), (2, 'x')") else {
        panic!()
    };
    assert_eq!(i.table.name().value, "t");
    assert_eq!(i.columns.len(), 2);
    let InsertSource::Query(src) = &i.source else {
        panic!()
    };
    assert_eq!(query_str(src), "values (1 DEFAULT) (2 'x')");
    let Statement::Insert(i) = one("INSERT INTO t DEFAULT VALUES") else {
        panic!()
    };
    assert_eq!(i.source, InsertSource::DefaultValues);
    let Statement::Insert(i) = one("INSERT INTO t SELECT * FROM u") else {
        panic!()
    };
    assert!(i.columns.is_empty());
    let Statement::Insert(i) = one("INSERT INTO t (SELECT 1)") else {
        panic!()
    };
    assert!(i.columns.is_empty());
    let Statement::Insert(i) = one("INSERT INTO t AS x (a) VALUES (1) RETURNING a, *") else {
        panic!()
    };
    assert_eq!(i.alias.unwrap().value, "x");
    assert_eq!(i.returning.len(), 2);
    syntax("INSERT INTO t", "", 14);
    syntax("INSERT t VALUES (1)", "t", 8);
    unsupported("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING");
}

#[test]
fn update_delete() {
    let Statement::Update(u) = one("UPDATE t SET a = 1, b = DEFAULT WHERE c > 0 RETURNING a")
    else {
        panic!()
    };
    assert_eq!(u.assignments.len(), 2);
    assert_eq!(sexp(&u.assignments[1].value), "DEFAULT");
    assert!(u.selection.is_some());
    assert_eq!(u.returning.len(), 1);
    let Statement::Update(u) = one("UPDATE t x SET a = 1 FROM u WHERE x.a = u.a") else {
        panic!()
    };
    assert_eq!(u.alias.unwrap().value, "x");
    assert_eq!(u.from.len(), 1);
    let Statement::Update(u) = one("UPDATE t SET a = 1") else {
        panic!()
    };
    assert!(u.alias.is_none());
    let Statement::Delete(d) = one("DELETE FROM t AS x USING u WHERE x.a = u.a") else {
        panic!()
    };
    assert_eq!(d.alias.unwrap().value, "x");
    assert_eq!(d.using.len(), 1);
    let Statement::Delete(d) = one("DELETE FROM t") else {
        panic!()
    };
    assert!(d.selection.is_none());
    syntax("UPDATE t SET a", "", 15);
    syntax("DELETE t", "t", 8);
}

#[test]
fn transactions() {
    let kinds = [
        ("BEGIN", TransactionKind::Begin),
        ("BEGIN WORK", TransactionKind::Begin),
        ("begin transaction", TransactionKind::Begin),
        ("START TRANSACTION", TransactionKind::StartTransaction),
        ("COMMIT", TransactionKind::Commit),
        ("COMMIT WORK", TransactionKind::Commit),
        ("COMMIT TRANSACTION AND NO CHAIN", TransactionKind::Commit),
        ("END", TransactionKind::End),
        ("END TRANSACTION", TransactionKind::End),
        ("ROLLBACK", TransactionKind::Rollback),
        ("ROLLBACK WORK", TransactionKind::Rollback),
        ("ABORT", TransactionKind::Abort),
    ];
    for (sql, kind) in kinds {
        let Statement::Transaction(t) = one(sql) else {
            panic!("{sql}")
        };
        assert_eq!(t.kind, kind, "{sql}");
    }
    let Statement::Transaction(t) =
        one("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY NOT DEFERRABLE")
    else {
        panic!()
    };
    assert_eq!(
        t.modes,
        vec![
            TransactionMode::IsolationLevel("repeatable read".into()),
            TransactionMode::ReadOnly,
            TransactionMode::NotDeferrable
        ]
    );
    syntax("BEGIN READ", "", 11);
    syntax("BEGIN ISOLATION LEVEL READ ONLY", "ONLY", 28);
    syntax("START", "", 6);
    let chain = |sql: &str| match one(sql) {
        Statement::Transaction(t) => (t.kind, t.chain),
        other => panic!("not a transaction: {other:?}"),
    };
    assert_eq!(chain("COMMIT AND CHAIN"), (TransactionKind::Commit, true));
    assert_eq!(chain("END WORK AND CHAIN"), (TransactionKind::End, true));
    assert_eq!(
        chain("ROLLBACK AND CHAIN"),
        (TransactionKind::Rollback, true)
    );
    assert_eq!(chain("ABORT AND NO CHAIN"), (TransactionKind::Abort, false));
    assert_eq!(chain("COMMIT"), (TransactionKind::Commit, false));
    syntax("COMMIT AND", "", 11);
    syntax("COMMIT AND NO", "", 14);
    let sp = |sql: &str| match one(sql) {
        Statement::Transaction(t) => t.kind,
        other => panic!("not a transaction: {other:?}"),
    };
    assert_eq!(sp("SAVEPOINT a"), TransactionKind::Savepoint("a".into()));
    assert_eq!(
        sp("SAVEPOINT \"A b\""),
        TransactionKind::Savepoint("A b".into())
    );
    assert_eq!(sp("RELEASE a"), TransactionKind::Release("a".into()));
    assert_eq!(
        sp("RELEASE SAVEPOINT a"),
        TransactionKind::Release("a".into())
    );
    assert_eq!(sp("ROLLBACK TO a"), TransactionKind::RollbackTo("a".into()));
    assert_eq!(
        sp("ROLLBACK WORK TO SAVEPOINT a"),
        TransactionKind::RollbackTo("a".into())
    );
    syntax("SAVEPOINT", "", 10);
    assert_eq!(
        sp("RELEASE SAVEPOINT"),
        TransactionKind::Release("savepoint".into())
    );
    assert_eq!(
        sp("ROLLBACK TO SAVEPOINT"),
        TransactionKind::RollbackTo("savepoint".into())
    );
    syntax("ROLLBACK TO", "", 12);
    unsupported("COMMIT PREPARED 'x'");
}

fn set_of(sql: &str) -> (bool, String, SetValue) {
    match one(sql) {
        Statement::Set(s) => (s.local, s.name, s.value),
        other => panic!("not SET: {other:?}"),
    }
}

#[test]
fn set_statements() {
    let s = |v: &str| SetArg::String(v.into());
    let w = |v: &str| SetArg::Word(v.into());
    let n = |v: &str| SetArg::Number(v.into());
    assert_eq!(
        set_of("SET application_name = 'a'"),
        (
            false,
            "application_name".into(),
            SetValue::Values(vec![s("a")])
        )
    );
    assert_eq!(
        set_of("SET APPLICATION_NAME TO 'with space'"),
        (
            false,
            "application_name".into(),
            SetValue::Values(vec![s("with space")])
        )
    );
    assert_eq!(
        set_of("SET extra_float_digits = -15").2,
        SetValue::Values(vec![n("-15")])
    );
    assert_eq!(
        set_of("SET extra_float_digits TO 3").2,
        SetValue::Values(vec![n("3")])
    );
    assert_eq!(set_of("SET x = 2.5").2, SetValue::Values(vec![n("2.5")]));
    assert_eq!(
        set_of("SET search_path = myschema, public").2,
        SetValue::Values(vec![w("myschema"), w("public")])
    );
    assert_eq!(
        set_of("SET search_path = \"$user\", public").2,
        SetValue::Values(vec![w("$user"), w("public")])
    );
    assert_eq!(set_of("SET search_path TO DEFAULT").2, SetValue::Default);
    assert_eq!(set_of("SET x = on").2, SetValue::Values(vec![w("on")]));
    assert_eq!(set_of("SET x = TRUE").2, SetValue::Values(vec![w("true")]));
    assert_eq!(
        set_of("SET datestyle = ISO, MDY").2,
        SetValue::Values(vec![w("iso"), w("mdy")])
    );
    assert_eq!(
        set_of("SET LOCAL extra_float_digits = 0"),
        (
            true,
            "extra_float_digits".into(),
            SetValue::Values(vec![n("0")])
        )
    );
    assert_eq!(set_of("SET SESSION timezone TO 'UTC'").1, "timezone");
    assert_eq!(
        set_of("SET TIME ZONE 'UTC'"),
        (false, "timezone".into(), SetValue::Values(vec![s("UTC")]))
    );
    assert_eq!(set_of("SET TIME ZONE LOCAL").2, SetValue::Default);
    assert_eq!(
        set_of("SET TIME ZONE -5").2,
        SetValue::Values(vec![n("-5")])
    );
    assert_eq!(
        set_of("SET yuzhu_test.my_param = 'hello'").1,
        "yuzhu_test.my_param"
    );
    assert_eq!(set_of("SET NAMES 'UTF8'").1, "client_encoding");
    assert_eq!(set_of("SET SCHEMA 'public'").1, "search_path");
    assert_eq!(
        set_of("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
        (
            false,
            "transaction_isolation".into(),
            SetValue::Values(vec![s("serializable")])
        )
    );
    assert_eq!(
        set_of("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").1,
        "default_transaction_read_only"
    );
    assert_eq!(set_of("SET local = 1").1, "local");
    syntax("SET x", "", 6);
    syntax("SET x = ", "", 9);
    syntax("SET x = select", "select", 9);
}

#[test]
fn show_reset() {
    let show = |sql: &str| match one(sql) {
        Statement::Show(s) => s.target,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        show("SHOW DateStyle"),
        ParamTarget::Name("datestyle".into())
    );
    assert_eq!(show("SHOW ALL"), ParamTarget::All);
    assert_eq!(show("SHOW TIME ZONE"), ParamTarget::Name("timezone".into()));
    assert_eq!(
        show("SHOW TRANSACTION ISOLATION LEVEL"),
        ParamTarget::Name("transaction_isolation".into())
    );
    assert_eq!(
        show("SHOW yuzhu_test.my_param"),
        ParamTarget::Name("yuzhu_test.my_param".into())
    );
    let reset = |sql: &str| match one(sql) {
        Statement::Reset(s) => s.target,
        other => panic!("{other:?}"),
    };
    assert_eq!(reset("RESET ALL"), ParamTarget::All);
    assert_eq!(
        reset("RESET timezone"),
        ParamTarget::Name("timezone".into())
    );
    assert_eq!(
        reset("RESET TIME ZONE"),
        ParamTarget::Name("timezone".into())
    );
    syntax("SHOW", "", 5);
}

#[test]
fn explain() {
    let Statement::Explain(x) = one("EXPLAIN SELECT 1") else {
        panic!()
    };
    assert!(x.options.is_empty());
    assert!(matches!(*x.statement, Statement::Query(_)));
    let Statement::Explain(x) = one("EXPLAIN ANALYZE VERBOSE INSERT INTO t VALUES (1)") else {
        panic!()
    };
    assert_eq!(x.options.len(), 2);
    let Statement::Explain(x) = one("EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) SELECT 1") else {
        panic!()
    };
    assert_eq!(x.options[0].name, "analyze");
    syntax("EXPLAIN BEGIN", "BEGIN", 9);
}

#[test]
fn unsupported_statements() {
    unsupported("GRANT SELECT ON t TO u");
}

#[test]
fn identifiers() {
    let Statement::Query(query) = one("SELECT Abc, \"Abc\", \"a\"\"b\" FROM \"T\"") else {
        panic!()
    };
    let QueryBody::Select(s) = &query.body else {
        panic!()
    };
    let names: Vec<String> = s
        .targets
        .iter()
        .map(|t| match t {
            SelectItem::Expr { expr, .. } => sexp(expr),
            _ => String::new(),
        })
        .collect();
    assert_eq!(names, vec!["abc", "\"Abc\"", "\"a\"b\""]);
    let TableRef::Table { name, .. } = &s.from[0] else {
        panic!()
    };
    assert!(name.parts[0].quoted);
    assert_eq!(name.parts[0].value, "T");
}

#[test]
fn spans_of_nodes() {
    let x = parse_expr("1 + 2").unwrap();
    assert_eq!(x.span(), Span::new(2, 5));
    let x = parse_expr("'a'::int4").unwrap();
    assert_eq!(x.span(), Span::new(3, 9));
    let Expr::Cast { expr, .. } = x else { panic!() };
    assert_eq!(expr.span(), Span::new(0, 3));
    let x = parse_expr("lower(a)").unwrap();
    assert_eq!(x.span(), Span::new(0, 8));
    let x = parse_expr("CAST(a AS int)").unwrap();
    assert_eq!(x.span(), Span::new(0, 14));
    let x = parse_expr("t.a").unwrap();
    assert_eq!(x.span(), Span::new(0, 3));
}

// ----- nesting depth guard (54001) -------------------------------------------

/// Runs `f` on a thread with the default 2 MB stack, so a missing depth
/// guard shows up as a stack overflow (process abort) rather than passing
/// on a bigger test-harness stack.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

fn assert_too_deep(sql: &str) {
    match parse(sql) {
        Ok(_) => panic!("expected 54001 for a {}-byte query", sql.len()),
        Err(e) => {
            assert_eq!(e.sqlstate.code(), "54001", "{}", e.message);
            assert_eq!(e.message, "stack depth limit exceeded");
            assert!(e.hint.as_deref().unwrap_or("").contains("max_stack_depth"));
        }
    }
}

#[test]
fn deep_nesting_is_rejected_with_54001() {
    on_small_stack(|| {
        let n = 100_000;
        assert_too_deep(&format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n)));
        assert_too_deep(&format!("SELECT {}true", "NOT ".repeat(n)));
        assert_too_deep(&format!("SELECT {}1", "- ".repeat(n)));
        assert_too_deep(&format!("SELECT 1{}", "+1".repeat(n)));
        assert_too_deep(&format!("SELECT true{}", " OR true".repeat(n)));
        // A chain ending in a syntax error: the guard fires first, and the
        // partial tree never gets deep enough to overflow when dropped.
        assert_too_deep(&format!("SELECT 1{} +", "+1".repeat(200_000)));
        assert_too_deep(&format!("SELECT f({}1{})", "f(".repeat(n), ")".repeat(n)));
        assert_too_deep(&format!("SELECT 1{}", " UNION SELECT 1".repeat(n)));
        assert_too_deep(&format!(
            "SELECT {}SELECT 1{}",
            "(".repeat(n),
            ")".repeat(n)
        ));
        assert_too_deep(&format!("SELECT * FROM a{}", " JOIN a ON true".repeat(n)));
        assert_too_deep(&format!(
            "SELECT * FROM a{}{}",
            " JOIN a".repeat(n),
            " ON true".repeat(n)
        ));
        assert_too_deep(&format!(
            "SELECT * FROM {}a JOIN b ON true{}",
            "(".repeat(n),
            ")".repeat(n)
        ));
        // Nested chains: each level is short but the tree is deep.
        let mut s = "1".to_string();
        for _ in 0..100 {
            s = format!("({s}{})", "+1".repeat(20));
        }
        assert_too_deep(&format!("SELECT {s}"));
        assert!(matches!(
            parse_expr(&format!("{}1", "- ".repeat(n))),
            Err(e) if e.sqlstate.code() == "54001"
        ));
    });
}

#[test]
fn moderate_nesting_still_parses() {
    on_small_stack(|| {
        // Chains do not recurse, so they reach the height limit on any stack.
        let n = 900;
        for sql in [
            format!("SELECT 1{}", "+1".repeat(n)),
            format!("SELECT 1{}", " UNION SELECT 1".repeat(n)),
            format!("SELECT * FROM a{}", " JOIN a ON true".repeat(n)),
            format!("SELECT {}1{}", "(".repeat(50), ")".repeat(50)),
            format!("SELECT {}true", "NOT ".repeat(50)),
        ] {
            assert!(parse(&sql).is_ok(), "{}", &sql[..40]);
        }
        // Many statements / siblings do not add up.
        let wide = format!("SELECT {}", vec!["1+1+1+1+1"; 2000].join(", "));
        assert!(parse(&wide).is_ok());
        let many = format!("SELECT 1{};", "+1".repeat(500)).repeat(10);
        assert_eq!(parse(&many).unwrap().len(), 10);
    });
}

#[test]
fn nesting_limit_on_a_big_stack() {
    // With enough stack, recursion is bounded by MAX_NESTING_DEPTH (5000
    // levels; `SELECT` and the target expression take two of them).
    std::thread::Builder::new()
        .stack_size(512 << 20)
        .spawn(|| {
            crate::sql::set_stack_budget(480 << 20);
            let parens = |n: usize| format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n));
            assert!(parse(&parens(4990)).is_ok());
            assert_too_deep(&parens(5000));
            assert!(parse(&format!("SELECT {}true", "NOT ".repeat(4990))).is_ok());
            assert_too_deep(&format!("SELECT {}true", "NOT ".repeat(5000)));
            assert!(parse(&format!("SELECT 1{}", "+1".repeat(4990))).is_ok());
            assert_too_deep(&format!("SELECT 1{}", "+1".repeat(5000)));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn m2_checkpoint_only_and_set_targets() {
    assert!(matches!(one("CHECKPOINT"), Statement::Checkpoint(_)));
    assert!(matches!(one("checkpoint;"), Statement::Checkpoint(_)));
    syntax("CHECKPOINT 1", "1", 12);

    let Statement::Update(u) = one("UPDATE ONLY t x SET a = 1") else {
        panic!()
    };
    assert_eq!(u.table.name().value, "t");
    assert_eq!(u.alias.unwrap().value, "x");
    let Statement::Update(u) = one("UPDATE ONLY (s.t) SET a = 1") else {
        panic!()
    };
    assert_eq!(u.table.parts.len(), 2);
    let Statement::Update(u) = one("UPDATE t * SET a = 1") else {
        panic!()
    };
    assert!(u.alias.is_none());
    let Statement::Delete(d) = one("DELETE FROM ONLY t WHERE a = 1") else {
        panic!()
    };
    assert!(d.selection.is_some());

    let Statement::Update(u) = one("UPDATE t SET t.a = 1") else {
        panic!()
    };
    assert_eq!(u.assignments[0].column.value, "t");
    assert_eq!(u.assignments[0].fields[0].value, "a");
    unsupported("UPDATE t SET a[1] = 1");
    unsupported("UPDATE t SET (a, b) = (1, 2)");
    unsupported("DELETE FROM t WHERE CURRENT OF c");
}

#[test]
fn m2_qualified_names_and_e_strings() {
    let Statement::Query(_) = one("SELECT pg_catalog.abs(-1) FROM pg_catalog.pg_class") else {
        panic!()
    };
    assert_eq!(
        q("SELECT E'a\\nb\\t\\\\\\x41\\101\\'' "),
        "select 'a\nb\t\\AA''"
    );
}

#[test]
fn set_transaction_keeps_all_modes() {
    let tr = |sql: &str| match one(sql) {
        Statement::Set(s) => s.transaction.expect("transaction modes"),
        other => panic!("not SET: {other:?}"),
    };
    let t = tr("SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ ONLY");
    assert!(!t.session_characteristics);
    assert_eq!(
        t.modes,
        vec![
            TransactionMode::IsolationLevel("read committed".into()),
            TransactionMode::ReadOnly
        ]
    );
    let t = tr("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE NOT DEFERRABLE");
    assert!(t.session_characteristics);
    assert_eq!(
        t.modes,
        vec![TransactionMode::ReadWrite, TransactionMode::NotDeferrable]
    );
    // A plain parameter assignment is not a transaction statement.
    match one("SET transaction_read_only = on") {
        Statement::Set(s) => assert!(s.transaction.is_none()),
        other => panic!("not SET: {other:?}"),
    }
    syntax("SET TRANSACTION", "", 16);
}
