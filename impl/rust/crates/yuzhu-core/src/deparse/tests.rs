//! deparse の単体テスト。`m4/10-explain-copy-compat.md` §4.9 の対応表（PostgreSQL 17.11 [実機]）を、
//! 解析器なしで手組みした式（`Expr<u32, u8>`）で確かめる。

use super::*;
use crate::catalog::builtin::{self, CASTS};
use crate::catalog::fake::FakeCatalog;
use crate::catalog::{BuiltinOperator, CastMethod};
use crate::error::Span;
use crate::expr::{AggCall, BoolTestKind, ExprKind, SubLinkKind};
use crate::sql::ast::SessionValueKind;
use crate::types::io::input_text_env;
use crate::types::{Datum, SqlType};
use yuzhu_datetime::{DateTimeEnv, TimeZone, ZoneDb};

type E = Expr<u32, u8>;

// ----- 道具 ------------------------------------------------------------------

/// 列 0.. の名前と型（§4.9 の表の列）。
fn cols() -> [(&'static str, SqlType); 11] {
    [
        ("i", SqlType::INT4),
        ("bi", SqlType::INT8),
        ("t", SqlType::TEXT),
        ("v", SqlType::varchar(10)),
        ("c", SqlType::new(crate::types::oid::BPCHAR, 7)),
        ("b", SqlType::BOOL),
        (
            "n",
            SqlType::new(crate::types::oid::NUMERIC, ((10 << 16) | 2) + 4),
        ),
        ("f", SqlType::FLOAT8),
        ("d", SqlType::DATE),
        ("ts", SqlType::TIMESTAMP),
        ("tz", SqlType::TIMESTAMPTZ),
    ]
}

struct Names {
    wrap: Option<u32>,
}

impl ColumnNamer<u32> for Names {
    fn name(&self, col: &u32) -> Result<ColText> {
        let (name, _) = cols()[*col as usize];
        Ok(ColText {
            text: name.to_owned(),
            wrap: self.wrap == Some(*col),
        })
    }
}

/// `q` < 10 は `SubPlan q`、10..20 は `InitPlan (q-10)`、20 以上は `hashed SubPlan (q-20)`。
struct Labels;

impl SubLinkRenderer<u8> for Labels {
    fn label(&self, q: &u8) -> Result<SubPlanLabel> {
        Ok(match *q {
            0..=9 => SubPlanLabel {
                name: format!("SubPlan {q}"),
                hashed: false,
                init_plan: false,
            },
            10..=19 => SubPlanLabel {
                name: format!("InitPlan {}", q - 10),
                hashed: false,
                init_plan: true,
            },
            _ => SubPlanLabel {
                name: format!("SubPlan {}", q - 20),
                hashed: true,
                init_plan: false,
            },
        })
    }
}

fn run(e: &E, opts: DeparseOptions, wrap: Option<u32>) -> Result<String> {
    let cat = FakeCatalog::new("db");
    let tz = TimeZone::utc();
    let zones = ZoneDb::without_tzdata();
    let env = TypeEnv {
        datetime: Some(DateTimeEnv::new(&tz, &zones)),
        ..TypeEnv::default()
    };
    let namer = Names { wrap };
    let cx = DeparseCtx::<u32, u8> {
        opts,
        namer: &namer,
        sublinks: Some(&Labels),
        type_env: &env,
        catalog: &cat,
    };
    deparse_expr(e, &cx)
}

fn plan(e: &E) -> String {
    run(e, DeparseOptions::EXPLAIN, None).unwrap()
}

fn stored(e: &E, pretty: bool) -> String {
    run(e, DeparseOptions::stored(pretty), None).unwrap()
}

fn ex(kind: ExprKind<u32, u8>, ty: SqlType) -> E {
    Expr::new(kind, ty, Span::default())
}

fn col(k: u32) -> E {
    ex(ExprKind::Column(k), cols()[k as usize].1)
}

fn lit(d: Datum, ty: SqlType) -> E {
    ex(ExprKind::Literal(d), ty)
}

fn i4(n: i32) -> E {
    lit(Datum::Int4(n), SqlType::INT4)
}

fn i8(n: i64) -> E {
    lit(Datum::Int8(n), SqlType::INT8)
}

fn text(s: &str) -> E {
    lit(Datum::Text(s.to_owned()), SqlType::TEXT)
}

fn tru() -> E {
    lit(Datum::Bool(true), SqlType::BOOL)
}

fn numeric(s: &str) -> E {
    let d = input_text_env(s, SqlType::NUMERIC, &TypeEnv::default()).unwrap();
    lit(d, SqlType::NUMERIC)
}

fn dt(s: &str, ty: SqlType) -> E {
    let tz = TimeZone::utc();
    let zones = ZoneDb::without_tzdata();
    let env = TypeEnv {
        datetime: Some(DateTimeEnv::new(&tz, &zones)),
        ..TypeEnv::default()
    };
    lit(input_text_env(s, ty, &env).unwrap(), ty)
}

fn op(name: &str) -> &'static BuiltinOperator {
    builtin::operators_named(name)
        .first()
        .copied()
        .unwrap_or_else(|| panic!("no operator {name}"))
}

fn is_cmp(name: &str) -> bool {
    matches!(
        name,
        "=" | "<>"
            | "<"
            | ">"
            | "<="
            | ">="
            | "~~"
            | "!~~"
            | "~~*"
            | "!~~*"
            | "~"
            | "!~"
            | "~*"
            | "!~*"
    )
}

fn bin(name: &str, l: E, r: E) -> E {
    let ty = if is_cmp(name) { SqlType::BOOL } else { l.ty };
    ex(
        ExprKind::Operator {
            op: op(name),
            args: vec![l, r],
        },
        ty,
    )
}

fn un(name: &str, a: E) -> E {
    let ty = a.ty;
    ex(
        ExprKind::Operator {
            op: op(name),
            args: vec![a],
        },
        ty,
    )
}

fn func(name: &str, args: Vec<E>, ty: SqlType) -> E {
    let f = builtin::functions_named(name)
        .first()
        .copied()
        .unwrap_or_else(|| panic!("no function {name}"));
    ex(ExprKind::Function { func: f, args }, ty)
}

fn and(v: Vec<E>) -> E {
    ex(ExprKind::And(v), SqlType::BOOL)
}

fn or(v: Vec<E>) -> E {
    ex(ExprKind::Or(v), SqlType::BOOL)
}

fn not(a: E) -> E {
    ex(ExprKind::Not(Box::new(a)), SqlType::BOOL)
}

fn is_null(a: E) -> E {
    ex(ExprKind::IsNull(Box::new(a)), SqlType::BOOL)
}

fn is_not_null(a: E) -> E {
    ex(ExprKind::IsNotNull(Box::new(a)), SqlType::BOOL)
}

fn bool_test(a: E, test: BoolTestKind) -> E {
    ex(
        ExprKind::BoolTest {
            expr: Box::new(a),
            test,
        },
        SqlType::BOOL,
    )
}

#[derive(Clone, Copy)]
enum Cm {
    Func,
    Bin,
    Io,
}

fn cast(a: E, ty: SqlType, m: Cm, implicit: bool) -> E {
    let method = match m {
        Cm::Func => CASTS
            .iter()
            .find_map(|c| matches!(c.method, CastMethod::Function(_)).then_some(c.method))
            .expect("a function cast"),
        Cm::Bin => CastMethod::Binary,
        Cm::Io => CastMethod::InOut,
    };
    ex(
        ExprKind::Cast {
            expr: Box::new(a),
            method,
            implicit,
        },
        ty,
    )
}

fn coalesce(v: Vec<E>) -> E {
    let ty = v[0].ty;
    ex(ExprKind::Coalesce(v), ty)
}

fn case(arms: Vec<(E, E)>, else_result: Option<E>) -> E {
    let ty = arms[0].1.ty;
    ex(
        ExprKind::Case {
            arms,
            else_result: else_result.map(Box::new),
        },
        ty,
    )
}

fn like(e: E, p: E, esc: Option<E>, negated: bool, ci: bool) -> E {
    ex(
        ExprKind::Like {
            expr: Box::new(e),
            pattern: Box::new(p),
            escape: esc.map(Box::new),
            negated,
            case_insensitive: ci,
        },
        SqlType::BOOL,
    )
}

fn in_list(e: E, list: Vec<E>, negated: bool) -> E {
    ex(
        ExprKind::InList {
            expr: Box::new(e),
            list,
            eq_op: op("="),
            negated,
        },
        SqlType::BOOL,
    )
}

fn session(v: SessionValueKind, ty: SqlType) -> E {
    ex(ExprKind::SessionValue(v), ty)
}

fn agg(name: &str, args: Vec<E>, distinct: bool, filter: Option<E>) -> E {
    let a = builtin::aggregates_named(name)
        .into_iter()
        .find(|a| a.args.len() == args.len())
        .unwrap_or_else(|| panic!("no aggregate {name}"));
    ex(
        ExprKind::Aggregate(Box::new(AggCall {
            func: a,
            args,
            distinct,
            filter,
            order_by: Vec::new(),
        })),
        SqlType::INT8,
    )
}

fn sublink(kind: SubLinkKind, test: Option<E>, q: u8) -> E {
    ex(
        ExprKind::SubLink {
            kind,
            test: test.map(Box::new),
            query: q,
        },
        SqlType::BOOL,
    )
}

// ----- §4.9 の CHECK の表（Stored） --------------------------------------------------

struct Row {
    sql: &'static str,
    e: E,
    plain: &'static str,
    /// `None` は pretty が非 pretty と同じ。
    pretty: Option<&'static str>,
}

fn row(sql: &'static str, e: E, plain: &'static str, pretty: &'static str) -> Row {
    Row {
        sql,
        e,
        plain,
        pretty: Some(pretty),
    }
}

fn same(sql: &'static str, e: E, s: &'static str) -> Row {
    Row {
        sql,
        e,
        plain: s,
        pretty: None,
    }
}

fn gt(l: E, r: E) -> E {
    bin(">", l, r)
}

#[allow(clippy::too_many_lines)]
fn check_rows() -> Vec<Row> {
    let i = || col(0);
    let bi = || col(1);
    let t = || col(2);
    let b = || col(5);
    let v_text = || cast(col(3), SqlType::TEXT, Cm::Bin, true);
    vec![
        row("i > 0", gt(i(), i4(0)), "CHECK ((i > 0))", "CHECK (i > 0)"),
        row(
            "i + 1 + 2 > 0",
            gt(bin("+", bin("+", i(), i4(1)), i4(2)), i4(0)),
            "CHECK ((((i + 1) + 2) > 0))",
            "CHECK ((i + 1 + 2) > 0)",
        ),
        row(
            "i + 1 * 2 > 3",
            gt(bin("+", i(), bin("*", i4(1), i4(2))), i4(3)),
            "CHECK (((i + (1 * 2)) > 3))",
            "CHECK ((i + 1 * 2) > 3)",
        ),
        row(
            "(i + 1) * 2 > 3",
            gt(bin("*", bin("+", i(), i4(1)), i4(2)), i4(3)),
            "CHECK ((((i + 1) * 2) > 3))",
            "CHECK (((i + 1) * 2) > 3)",
        ),
        row(
            "i - (1 - 2) > 0",
            gt(bin("-", i(), bin("-", i4(1), i4(2))), i4(0)),
            "CHECK (((i - (1 - 2)) > 0))",
            "CHECK ((i - (1 - 2)) > 0)",
        ),
        row(
            "i - 1 - 2 > 0",
            gt(bin("-", bin("-", i(), i4(1)), i4(2)), i4(0)),
            "CHECK ((((i - 1) - 2) > 0))",
            "CHECK ((i - 1 - 2) > 0)",
        ),
        row(
            "-i < 0",
            bin("<", un("-", i()), i4(0)),
            "CHECK (((- i) < 0))",
            "CHECK ((- i) < 0)",
        ),
        row(
            "i / (2 * 3) > 0",
            gt(bin("/", i(), bin("*", i4(2), i4(3))), i4(0)),
            "CHECK (((i / (2 * 3)) > 0))",
            "CHECK ((i / (2 * 3)) > 0)",
        ),
        row(
            "i % 2 * 3 > 0",
            gt(bin("*", bin("%", i(), i4(2)), i4(3)), i4(0)),
            "CHECK ((((i % 2) * 3) > 0))",
            "CHECK ((i % 2 * 3) > 0)",
        ),
        row(
            "i = bi + 1",
            bin("=", i(), bin("+", bi(), i4(1))),
            "CHECK ((i = (bi + 1)))",
            "CHECK (i = (bi + 1))",
        ),
        row(
            "t = 'abc'",
            bin("=", t(), text("abc")),
            "CHECK ((t = 'abc'::text))",
            "CHECK (t = 'abc'::text)",
        ),
        row(
            "v = 'x'",
            bin("=", v_text(), text("x")),
            "CHECK (((v)::text = 'x'::text))",
            "CHECK (v::text = 'x'::text)",
        ),
        row(
            "c = 'ab'",
            bin(
                "=",
                col(4),
                lit(Datum::BpChar("ab ".into()), SqlType::BPCHAR),
            ),
            "CHECK ((c = 'ab '::bpchar))",
            "CHECK (c = 'ab '::bpchar)",
        ),
        row(
            "t || 'x' = 'yx'",
            bin("=", bin("||", t(), text("x")), text("yx")),
            "CHECK (((t || 'x'::text) = 'yx'::text))",
            "CHECK ((t || 'x'::text) = 'yx'::text)",
        ),
        row(
            "lower(t) = 'abc'",
            bin("=", func("lower", vec![t()], SqlType::TEXT), text("abc")),
            "CHECK ((lower(t) = 'abc'::text))",
            "CHECK (lower(t) = 'abc'::text)",
        ),
        row(
            "lower(t || 'a') = 'x'",
            bin(
                "=",
                func("lower", vec![bin("||", t(), text("a"))], SqlType::TEXT),
                text("x"),
            ),
            "CHECK ((lower((t || 'a'::text)) = 'x'::text))",
            "CHECK (lower(t || 'a'::text) = 'x'::text)",
        ),
        row(
            "t LIKE 'a%'",
            like(t(), text("a%"), None, false, false),
            "CHECK ((t ~~ 'a%'::text))",
            "CHECK (t ~~ 'a%'::text)",
        ),
        row(
            "t NOT ILIKE 'a%'",
            like(t(), text("a%"), None, true, true),
            "CHECK ((t !~~* 'a%'::text))",
            "CHECK (t !~~* 'a%'::text)",
        ),
        row(
            "t LIKE 'a!%' ESCAPE '!'",
            like(t(), text("a!%"), Some(text("!")), false, false),
            "CHECK ((t ~~ like_escape('a!%'::text, '!'::text)))",
            "CHECK (t ~~ like_escape('a!%'::text, '!'::text))",
        ),
        row(
            "t ~ '^a'",
            bin("~", t(), text("^a")),
            "CHECK ((t ~ '^a'::text))",
            "CHECK (t ~ '^a'::text)",
        ),
        row(
            "i IN (1,2,3)",
            in_list(i(), vec![i4(1), i4(2), i4(3)], false),
            "CHECK ((i = ANY (ARRAY[1, 2, 3])))",
            "CHECK (i = ANY (ARRAY[1, 2, 3]))",
        ),
        row(
            "i NOT IN (1,2,3)",
            in_list(i(), vec![i4(1), i4(2), i4(3)], true),
            "CHECK ((i <> ALL (ARRAY[1, 2, 3])))",
            "CHECK (i <> ALL (ARRAY[1, 2, 3]))",
        ),
        row(
            "i IN (1)",
            in_list(i(), vec![i4(1)], false),
            "CHECK ((i = 1))",
            "CHECK (i = 1)",
        ),
        row(
            "i IN (1, bi)",
            in_list(i(), vec![i4(1), bi()], false),
            "CHECK (((i = 1) OR (i = bi)))",
            "CHECK (i = 1 OR i = bi)",
        ),
        row(
            "i IN (1, NULL)",
            in_list(i(), vec![i4(1), lit(Datum::Null, SqlType::INT4)], false),
            "CHECK ((i = ANY (ARRAY[1, NULL::integer])))",
            "CHECK (i = ANY (ARRAY[1, NULL::integer]))",
        ),
        row(
            "i BETWEEN 1 AND 5",
            and(vec![bin(">=", i(), i4(1)), bin("<=", i(), i4(5))]),
            "CHECK (((i >= 1) AND (i <= 5)))",
            "CHECK (i >= 1 AND i <= 5)",
        ),
        row(
            "i NOT BETWEEN 1 AND 5",
            or(vec![bin("<", i(), i4(1)), gt(i(), i4(5))]),
            "CHECK (((i < 1) OR (i > 5)))",
            "CHECK (i < 1 OR i > 5)",
        ),
        row(
            "t IS NULL",
            is_null(t()),
            "CHECK ((t IS NULL))",
            "CHECK (t IS NULL)",
        ),
        row(
            "t IS NOT NULL",
            is_not_null(t()),
            "CHECK ((t IS NOT NULL))",
            "CHECK (t IS NOT NULL)",
        ),
        row(
            "b IS NOT TRUE",
            bool_test(b(), BoolTestKind::IsNotTrue),
            "CHECK ((b IS NOT TRUE))",
            "CHECK (b IS NOT TRUE)",
        ),
        same("b", b(), "CHECK (b)"),
        row("NOT b", not(b()), "CHECK ((NOT b))", "CHECK (NOT b)"),
        row(
            "b AND i > 1",
            and(vec![b(), gt(i(), i4(1))]),
            "CHECK ((b AND (i > 1)))",
            "CHECK (b AND i > 1)",
        ),
        row(
            "(b AND i > 1) OR i < 0",
            or(vec![and(vec![b(), gt(i(), i4(1))]), bin("<", i(), i4(0))]),
            "CHECK (((b AND (i > 1)) OR (i < 0)))",
            "CHECK (b AND i > 1 OR i < 0)",
        ),
        row(
            "b AND (i > 1 OR i < 0)",
            and(vec![b(), or(vec![gt(i(), i4(1)), bin("<", i(), i4(0))])]),
            "CHECK ((b AND ((i > 1) OR (i < 0))))",
            "CHECK (b AND (i > 1 OR i < 0))",
        ),
        row(
            "NOT (b AND i > 1)",
            not(and(vec![b(), gt(i(), i4(1))])),
            "CHECK ((NOT (b AND (i > 1))))",
            "CHECK (NOT (b AND i > 1))",
        ),
        row(
            "NOT (i > 1)",
            not(gt(i(), i4(1))),
            "CHECK ((NOT (i > 1)))",
            "CHECK (NOT i > 1)",
        ),
        row(
            "NOT b AND b",
            and(vec![not(b()), b()]),
            "CHECK (((NOT b) AND b))",
            "CHECK (NOT b AND b)",
        ),
        row(
            "NOT (NOT b)",
            not(not(b())),
            "CHECK ((NOT (NOT b)))",
            "CHECK (NOT (NOT b))",
        ),
        row(
            "b AND (b AND b)",
            and(vec![b(), and(vec![b(), b()])]),
            "CHECK ((b AND (b AND b)))",
            "CHECK (b AND b AND b)",
        ),
        row(
            "(b OR b) AND (b OR b)",
            and(vec![or(vec![b(), b()]), or(vec![b(), b()])]),
            "CHECK (((b OR b) AND (b OR b)))",
            "CHECK ((b OR b) AND (b OR b))",
        ),
        row(
            "(i > 0) = (bi > 0)",
            bin("=", gt(i(), i4(0)), gt(bi(), i4(0))),
            "CHECK (((i > 0) = (bi > 0)))",
            "CHECK ((i > 0) = (bi > 0))",
        ),
        row(
            "(i + 1) IS NULL",
            is_null(bin("+", i(), i4(1))),
            "CHECK (((i + 1) IS NULL))",
            "CHECK ((i + 1) IS NULL)",
        ),
        row(
            "(i + 1)::text IS NULL",
            is_null(cast(bin("+", i(), i4(1)), SqlType::TEXT, Cm::Io, false)),
            "CHECK ((((i + 1))::text IS NULL))",
            "CHECK (((i + 1)::text) IS NULL)",
        ),
        row(
            "COALESCE(b AND b, true) = b",
            bin("=", coalesce(vec![and(vec![b(), b()]), tru()]), b()),
            "CHECK ((COALESCE((b AND b), true) = b))",
            "CHECK (COALESCE(b AND b, true) = b)",
        ),
        row(
            "COALESCE(t, v, 'z') = 'q'",
            bin("=", coalesce(vec![t(), v_text(), text("z")]), text("q")),
            "CHECK ((COALESCE(t, (v)::text, 'z'::text) = 'q'::text))",
            "CHECK (COALESCE(t, v::text, 'z'::text) = 'q'::text)",
        ),
        row(
            "NULLIF(i, 0) = 1",
            bin(
                "=",
                ex(
                    ExprKind::NullIf {
                        left: Box::new(i()),
                        right: Box::new(i4(0)),
                        eq_op: op("="),
                    },
                    SqlType::INT4,
                ),
                i4(1),
            ),
            "CHECK ((NULLIF(i, 0) = 1))",
            "CHECK (NULLIF(i, 0) = 1)",
        ),
        row(
            "i::bigint > 1",
            gt(cast(i(), SqlType::INT8, Cm::Func, false), i4(1)),
            "CHECK (((i)::bigint > 1))",
            "CHECK (i::bigint > 1)",
        ),
        row(
            "(i + 1)::bigint > 0",
            gt(
                cast(bin("+", i(), i4(1)), SqlType::INT8, Cm::Func, false),
                i4(0),
            ),
            "CHECK ((((i + 1))::bigint > 0))",
            "CHECK ((i + 1)::bigint > 0)",
        ),
        row(
            "i::bigint::text = t",
            bin(
                "=",
                cast(
                    cast(i(), SqlType::INT8, Cm::Func, false),
                    SqlType::TEXT,
                    Cm::Io,
                    false,
                ),
                t(),
            ),
            "CHECK ((((i)::bigint)::text = t))",
            "CHECK (i::bigint::text = t)",
        ),
        row(
            "i::numeric > 1.5",
            gt(cast(i(), SqlType::NUMERIC, Cm::Func, false), numeric("1.5")),
            "CHECK (((i)::numeric > 1.5))",
            "CHECK (i::numeric > 1.5)",
        ),
        row(
            "n > 1",
            gt(col(6), cast(i4(1), SqlType::NUMERIC, Cm::Func, true)),
            "CHECK ((n > (1)::numeric))",
            "CHECK (n > 1::numeric)",
        ),
        row(
            "n = 1.50",
            bin("=", col(6), numeric("1.50")),
            "CHECK ((n = 1.50))",
            "CHECK (n = 1.50)",
        ),
        row(
            "f > 1",
            gt(col(7), cast(i4(1), SqlType::FLOAT8, Cm::Func, true)),
            "CHECK ((f > (1)::double precision))",
            "CHECK (f > 1::double precision)",
        ),
        row(
            "f > 'Infinity'",
            gt(col(7), lit(Datum::Float8(f64::INFINITY), SqlType::FLOAT8)),
            "CHECK ((f > 'Infinity'::double precision))",
            "CHECK (f > 'Infinity'::double precision)",
        ),
        same("i > -1", gt(i(), i4(-1)), "CHECK ((i > '-1'::integer))"),
        row(
            "i > 2147483648",
            gt(i(), i8(2_147_483_648)),
            "CHECK ((i > '2147483648'::bigint))",
            "CHECK (i > '2147483648'::bigint)",
        ),
        row(
            "i > 1::bigint",
            gt(i(), cast(i4(1), SqlType::INT8, Cm::Func, false)),
            "CHECK ((i > (1)::bigint))",
            "CHECK (i > 1::bigint)",
        ),
        row(
            "d > '2020-01-01'",
            gt(col(8), dt("2020-01-01", SqlType::DATE)),
            "CHECK ((d > '2020-01-01'::date))",
            "CHECK (d > '2020-01-01'::date)",
        ),
        same(
            "ts > '2020-01-01 10:00'",
            gt(col(9), dt("2020-01-01 10:00", SqlType::TIMESTAMP)),
            "CHECK ((ts > '2020-01-01 10:00:00'::timestamp without time zone))",
        ),
        same(
            "tz > '2020-01-01 10:00+00'",
            gt(col(10), dt("2020-01-01 10:00+00", SqlType::TIMESTAMPTZ)),
            "CHECK ((tz > '2020-01-01 10:00:00+00'::timestamp with time zone))",
        ),
        same(
            "tz > current_timestamp",
            gt(
                col(10),
                session(
                    SessionValueKind::CurrentTimestamp { precision: -1 },
                    SqlType::TIMESTAMPTZ,
                ),
            ),
            "CHECK ((tz > CURRENT_TIMESTAMP))",
        ),
        same(
            "d > current_date",
            gt(
                col(8),
                session(SessionValueKind::CurrentDate, SqlType::DATE),
            ),
            "CHECK ((d > CURRENT_DATE))",
        ),
        row(
            "ts::date = d",
            bin("=", cast(col(9), SqlType::DATE, Cm::Func, false), col(8)),
            "CHECK (((ts)::date = d))",
            "CHECK (ts::date = d)",
        ),
        same(
            "current_user = t",
            bin(
                "=",
                session(SessionValueKind::CurrentUser, SqlType::NAME),
                t(),
            ),
            "CHECK ((CURRENT_USER = t))",
        ),
        same(
            "current_schema() = t",
            bin(
                "=",
                session(SessionValueKind::CurrentSchema, SqlType::NAME),
                t(),
            ),
            "CHECK ((\"current_schema\"() = t))",
        ),
        same("t = ''", bin("=", t(), text("")), "CHECK ((t = ''::text))"),
        same(
            "t = 'it''s'",
            bin("=", t(), text("it's")),
            "CHECK ((t = 'it''s'::text))",
        ),
        same(
            "t = 'a\\b'",
            bin("=", t(), text("a\\b")),
            "CHECK ((t = 'a\\b'::text))",
        ),
        row(
            "case when i > 1 then 'a' else 'b' end = t",
            bin(
                "=",
                case(vec![(gt(i(), i4(1)), text("a"))], Some(text("b"))),
                t(),
            ),
            "CHECK ((\nCASE\n    WHEN (i > 1) THEN 'a'::text\n    ELSE 'b'::text\nEND = t))",
            "CHECK (\nCASE\n    WHEN i > 1 THEN 'a'::text\n    ELSE 'b'::text\nEND = t)",
        ),
        same(
            "case when i > 1 then 1 end = 1",
            bin("=", case(vec![(gt(i(), i4(1)), i4(1))], None), i4(1)),
            "CHECK ((\nCASE\n    WHEN (i > 1) THEN 1\n    ELSE NULL::integer\nEND = 1))",
        ),
    ]
}

#[test]
fn stored_check_table_matches_postgresql() {
    let mut failures = Vec::new();
    for r in check_rows() {
        let got = format!("CHECK ({})", stored(&r.e, false));
        if got != r.plain {
            failures.push(format!(
                "[plain] {}\n  want {:?}\n  got  {:?}",
                r.sql, r.plain, got
            ));
        }
        if let Some(p) = r.pretty {
            let got = format!("CHECK ({})", stored(&r.e, true));
            if got != p {
                failures.push(format!(
                    "[pretty] {}\n  want {:?}\n  got  {:?}",
                    r.sql, p, got
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn stored_pretty_without_a_pretty_column_equals_plain_for_parens_free_rows() {
    // 括弧のない式は、pretty でも同じ。
    for r in check_rows()
        .into_iter()
        .filter(|r| r.pretty.is_none() && r.sql == "b")
    {
        assert_eq!(stored(&r.e, true), "b");
    }
}

#[test]
fn nested_case_indentation() {
    let b = col(5);
    let inner = case(vec![(b, i4(1))], Some(i4(2)));
    let outer = case(vec![(gt(inner, i4(1)), text("a"))], Some(text("b")));
    let e = bin("=", outer, text("a"));
    assert_eq!(
        format!("CHECK ({})", stored(&e, false)),
        "CHECK ((\nCASE\n    WHEN (\n    CASE\n        WHEN b THEN 1\n        ELSE 2\n    END > 1) THEN 'a'::text\n    ELSE 'b'::text\nEND = 'a'::text))"
    );
}

#[test]
fn case_is_one_line_in_plan() {
    let e = case(vec![(gt(col(0), i4(1)), text("a"))], Some(text("b")));
    assert_eq!(
        plan(&e),
        "CASE WHEN (i > 1) THEN 'a'::text ELSE 'b'::text END"
    );
    let e = case(
        vec![(gt(col(0), i4(1)), i4(1)), (bin("<", col(0), i4(0)), i4(2))],
        None,
    );
    assert_eq!(
        plan(&e),
        "CASE WHEN (i > 1) THEN 1 WHEN (i < 0) THEN 2 ELSE NULL::integer END"
    );
}

#[test]
fn case_in_the_right_operand_drops_the_trailing_space() {
    let e = bin(
        "=",
        col(2),
        case(vec![(col(5), text("a"))], Some(text("b"))),
    );
    assert_eq!(
        stored(&e, false),
        "(t =\nCASE\n    WHEN b THEN 'a'::text\n    ELSE 'b'::text\nEND)"
    );
}

// ----- §4.9 の EXPLAIN の表（Plan） ------------------------------------------------------

#[test]
fn plan_filter_table_matches_postgresql() {
    let i = || col(0);
    let t = || col(2);
    let n = || col(6);
    let f = || col(7);
    let cases: Vec<(&str, E, &str)> = vec![
        (
            "i + 1 * 2 > 3",
            gt(bin("+", i(), i4(2)), i4(3)),
            "((i + 2) > 3)",
        ),
        (
            "i - (1 - 2) > 0",
            gt(bin("-", i(), i4(-1)), i4(0)),
            "((i - '-1'::integer) > 0)",
        ),
        ("t = 'abc'", bin("=", t(), text("abc")), "(t = 'abc'::text)"),
        (
            "v = 'x'",
            bin("=", cast(col(3), SqlType::TEXT, Cm::Bin, true), text("x")),
            "((v)::text = 'x'::text)",
        ),
        (
            "t LIKE 'a!%' ESCAPE '!'",
            like(t(), text("a!%"), Some(text("!")), false, false),
            "(t ~~ 'a\\%'::text)",
        ),
        (
            "i IN (1,2,3)",
            in_list(i(), vec![i4(1), i4(2), i4(3)], false),
            "(i = ANY ('{1,2,3}'::integer[]))",
        ),
        (
            "i NOT IN (1,2,3)",
            in_list(i(), vec![i4(1), i4(2), i4(3)], true),
            "(i <> ALL ('{1,2,3}'::integer[]))",
        ),
        (
            "t IN ('a','b')",
            in_list(t(), vec![text("a"), text("b")], false),
            "(t = ANY ('{a,b}'::text[]))",
        ),
        (
            "i IN (1, NULL)",
            in_list(i(), vec![i4(1), lit(Datum::Null, SqlType::INT4)], false),
            "(i = ANY ('{1,NULL}'::integer[]))",
        ),
        (
            "i IN (1, bi)",
            in_list(i(), vec![i4(1), col(1)], false),
            "((i = 1) OR (i = bi))",
        ),
        (
            "i NOT IN (1, bi)",
            in_list(i(), vec![i4(1), col(1)], true),
            "((i <> 1) AND (i <> bi))",
        ),
        ("i IN (1)", in_list(i(), vec![i4(1)], false), "(i = 1)"),
        ("i NOT IN (1)", in_list(i(), vec![i4(1)], true), "(i <> 1)"),
        ("n > 1", gt(n(), numeric("1")), "(n > '1'::numeric)"),
        ("n > 1.5", gt(n(), numeric("1.5")), "(n > 1.5)"),
        (
            "f > 1.5",
            gt(f(), lit(Datum::Float8(1.5), SqlType::FLOAT8)),
            "(f > '1.5'::double precision)",
        ),
        (
            "f > 1e10",
            gt(f(), lit(Datum::Float8(1e10), SqlType::FLOAT8)),
            "(f > '10000000000'::double precision)",
        ),
        (
            "f > 1e100",
            gt(f(), lit(Datum::Float8(1e100), SqlType::FLOAT8)),
            "(f > '1e+100'::double precision)",
        ),
        ("bi > 5", gt(col(1), i4(5)), "(bi > 5)"),
        (
            "bi > 2147483648",
            gt(col(1), i8(2_147_483_648)),
            "(bi > '2147483648'::bigint)",
        ),
        ("i > -1", gt(i(), i4(-1)), "(i > '-1'::integer)"),
    ];
    for (sql, e, want) in cases {
        assert_eq!(plan(&e), want, "{sql}");
    }
}

#[test]
fn plan_like_escape_with_non_constants_stays_a_call() {
    let e = like(col(2), col(2), Some(text("!")), false, false);
    assert_eq!(plan(&e), "(t ~~ like_escape(t, '!'::text))");
    // パターンがエスケープ文字で終わるときも畳み込まない。
    let e = like(col(2), text("a!"), Some(text("!")), false, false);
    assert_eq!(plan(&e), "(t ~~ like_escape('a!'::text, '!'::text))");
}

#[test]
fn array_constant_in_plan_uses_the_element_type() {
    let e = in_list(col(1), vec![i8(1), i8(2)], false);
    assert_eq!(plan(&e), "(bi = ANY ('{1,2}'::bigint[]))");
    let e = in_list(col(2), vec![text("a b"), text("NULL")], false);
    assert_eq!(plan(&e), "(t = ANY ('{\"a b\",\"NULL\"}'::text[]))");
    let e = in_list(
        col(2),
        vec![text("a b"), lit(Datum::Null, SqlType::TEXT)],
        false,
    );
    assert_eq!(
        stored(&e, false),
        "(t = ANY (ARRAY['a b'::text, NULL::text]))"
    );
}

// ----- Plan と Stored の違い --------------------------------------------------------------

#[test]
fn root_implicit_cast_is_hidden_only_in_stored() {
    let e = cast(i4(1), SqlType::INT8, Cm::Func, true);
    assert_eq!(stored(&e, false), "1");
    assert_eq!(stored(&e, true), "1");
    assert_eq!(plan(&e), "(1)::bigint");
    // 明示のキャストは隠さない。
    let e = cast(i4(1), SqlType::INT8, Cm::Func, false);
    assert_eq!(stored(&e, false), "(1)::bigint");
    assert_eq!(stored(&e, true), "1::bigint");
    // 連続する暗黙のキャストは繰り返し隠す。根以外は隠さない。
    let e = cast(
        cast(i4(1), SqlType::INT8, Cm::Func, true),
        SqlType::NUMERIC,
        Cm::Func,
        true,
    );
    assert_eq!(stored(&e, false), "1");
    let e = bin("+", cast(i4(1), SqlType::INT8, Cm::Func, true), i8(2));
    assert_eq!(stored(&e, false), "((1)::bigint + '2'::bigint)");
}

#[test]
fn implicit_typmod_coercion_is_hidden_at_the_root() {
    let ty = SqlType::varchar(5);
    let e = ex(
        ExprKind::CoerceTypmod {
            expr: Box::new(lit(Datum::Text("ab".into()), SqlType::VARCHAR)),
            explicit: false,
        },
        ty,
    );
    assert_eq!(stored(&e, false), "'ab'::character varying");
    assert_eq!(plan(&e), "('ab'::character varying)::character varying(5)");
    let e = ex(
        ExprKind::CoerceTypmod {
            expr: Box::new(lit(Datum::Text("ab".into()), SqlType::VARCHAR)),
            explicit: true,
        },
        ty,
    );
    assert_eq!(
        stored(&e, false),
        "('ab'::character varying)::character varying(5)"
    );
}

#[test]
fn stored_default_examples() {
    // §4.8 の DEFAULT の例。
    assert_eq!(
        stored(&lit(Datum::Text("ab".into()), SqlType::VARCHAR), false),
        "'ab'::character varying"
    );
    assert_eq!(stored(&text("a"), false), "'a'::text");
    assert_eq!(stored(&i4(-1), false), "'-1'::integer");
    let sum = bin("+", i4(1), i4(2));
    assert_eq!(stored(&sum, false), "(1 + 2)");
    assert_eq!(stored(&sum, true), "1 + 2");
    let cat = bin("||", text("a"), text("b"));
    assert_eq!(stored(&cat, false), "('a'::text || 'b'::text)");
    assert_eq!(stored(&cat, true), "'a'::text || 'b'::text");
}

// ----- 式の変種 ---------------------------------------------------------------------------

#[test]
fn functions_and_session_values() {
    assert_eq!(
        plan(&func("lower", vec![col(2)], SqlType::TEXT)),
        "lower(t)"
    );
    let cases = [
        (SessionValueKind::CurrentUser, "CURRENT_USER"),
        (SessionValueKind::SessionUser, "SESSION_USER"),
        (SessionValueKind::User, "USER"),
        (SessionValueKind::CurrentRole, "CURRENT_ROLE"),
        (SessionValueKind::CurrentCatalog, "CURRENT_CATALOG"),
        (SessionValueKind::CurrentSchema, "\"current_schema\"()"),
        (SessionValueKind::CurrentDate, "CURRENT_DATE"),
        (
            SessionValueKind::CurrentTimestamp { precision: -1 },
            "CURRENT_TIMESTAMP",
        ),
        (
            SessionValueKind::CurrentTimestamp { precision: 3 },
            "CURRENT_TIMESTAMP(3)",
        ),
        (
            SessionValueKind::LocalTimestamp { precision: -1 },
            "LOCALTIMESTAMP",
        ),
        (
            SessionValueKind::LocalTimestamp { precision: 0 },
            "LOCALTIMESTAMP(0)",
        ),
    ];
    for (v, want) in cases {
        assert_eq!(plan(&session(v, SqlType::TEXT)), want);
    }
}

#[test]
fn boolean_tests_and_distinct() {
    let kinds = [
        (BoolTestKind::IsTrue, "IS TRUE"),
        (BoolTestKind::IsNotTrue, "IS NOT TRUE"),
        (BoolTestKind::IsFalse, "IS FALSE"),
        (BoolTestKind::IsNotFalse, "IS NOT FALSE"),
        (BoolTestKind::IsUnknown, "IS UNKNOWN"),
        (BoolTestKind::IsNotUnknown, "IS NOT UNKNOWN"),
    ];
    for (k, s) in kinds {
        assert_eq!(plan(&bool_test(col(5), k)), format!("(b {s})"));
    }
    let d = |negated| {
        ex(
            ExprKind::DistinctFrom {
                left: Box::new(col(0)),
                right: Box::new(i4(1)),
                eq_op: op("="),
                negated,
            },
            SqlType::BOOL,
        )
    };
    assert_eq!(plan(&d(false)), "(i IS DISTINCT FROM 1)");
    assert_eq!(plan(&d(true)), "(i IS NOT DISTINCT FROM 1)");
    let m = ex(
        ExprKind::MinMax {
            greatest: true,
            args: vec![col(0), i4(3)],
            cmp: op(">"),
        },
        SqlType::INT4,
    );
    assert_eq!(plan(&m), "GREATEST(i, 3)");
}

#[test]
fn aggregates() {
    let count_star = agg("count", vec![], false, None);
    assert_eq!(plan(&count_star), "count(*)");
    let sum = agg("sum", vec![col(0)], false, None);
    assert_eq!(plan(&sum), "sum(i)");
    let cd = agg("count", vec![col(1)], true, None);
    assert_eq!(plan(&cd), "count(DISTINCT bi)");
    let mx = agg("max", vec![bin("+", col(0), i4(1))], false, None);
    assert_eq!(plan(&mx), "max((i + 1))");
    let filtered = agg("count", vec![], false, Some(gt(col(1), i4(2))));
    assert_eq!(plan(&filtered), "count(*) FILTER (WHERE (bi > 2))");
}

#[test]
fn sublinks_follow_the_explain_forms() {
    let eq_out = |l: E, i: u16| bin("=", l, ex(ExprKind::SubLinkOutput(i), SqlType::INT4));
    // スカラー: SubPlan は `(SubPlan 1)`、InitPlan は `(InitPlan 1).col1`。
    let s = bin("=", col(1), sublink(SubLinkKind::Scalar, None, 1));
    assert_eq!(plan(&s), "(bi = (SubPlan 1))");
    let s = bin("=", col(1), sublink(SubLinkKind::Scalar, None, 11));
    assert_eq!(plan(&s), "(bi = (InitPlan 1).col1)");
    // EXISTS。
    assert_eq!(
        plan(&sublink(SubLinkKind::Exists, None, 1)),
        "EXISTS(SubPlan 1)"
    );
    assert_eq!(
        plan(&sublink(SubLinkKind::Exists, None, 11)),
        "(InitPlan 1).col1"
    );
    assert_eq!(
        plan(&not(sublink(SubLinkKind::Exists, None, 2))),
        "(NOT EXISTS(SubPlan 2))"
    );
    assert_eq!(
        plan(&or(vec![
            sublink(SubLinkKind::Exists, None, 1),
            bin("=", col(1), i4(1))
        ])),
        "(EXISTS(SubPlan 1) OR (bi = 1))"
    );
    // ANY / ALL。
    let any = sublink(SubLinkKind::Any, Some(eq_out(col(0), 0)), 1);
    assert_eq!(plan(&any), "(ANY (i = (SubPlan 1).col1))");
    assert_eq!(plan(&not(any)), "(NOT (ANY (i = (SubPlan 1).col1)))");
    let hashed = sublink(SubLinkKind::Any, Some(eq_out(col(0), 0)), 21);
    assert_eq!(plan(&hashed), "(ANY (i = (hashed SubPlan 1).col1))");
    let multi = sublink(
        SubLinkKind::Any,
        Some(and(vec![eq_out(col(0), 0), eq_out(col(1), 1)])),
        21,
    );
    assert_eq!(
        plan(&multi),
        "(ANY ((i = (hashed SubPlan 1).col1) AND (bi = (hashed SubPlan 1).col2)))"
    );
    let all = sublink(
        SubLinkKind::All,
        Some(bin(
            ">",
            col(1),
            ex(ExprKind::SubLinkOutput(0), SqlType::INT4),
        )),
        1,
    );
    assert_eq!(plan(&all), "(ALL (bi > (SubPlan 1).col1))");
}

#[test]
fn sublinks_are_not_allowed_in_stored_expressions() {
    let e = sublink(SubLinkKind::Exists, None, 1);
    let cat = FakeCatalog::new("db");
    let env = TypeEnv::default();
    let namer = Names { wrap: None };
    let cx = DeparseCtx::<u32, u8> {
        opts: DeparseOptions::stored(false),
        namer: &namer,
        sublinks: None,
        type_env: &env,
        catalog: &cat,
    };
    let err = deparse_expr(&e, &cx).unwrap_err();
    assert_eq!(err.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
}

#[test]
fn computed_columns_are_wrapped() {
    let e = bin("+", col(0), i4(1));
    let wrapped = run(&e, DeparseOptions::EXPLAIN, Some(0)).unwrap();
    assert_eq!(wrapped, "((i) + 1)");
    let only = run(&col(0), DeparseOptions::EXPLAIN, Some(0)).unwrap();
    assert_eq!(only, "(i)");
}

#[test]
fn deparse_list_deparses_each_element() {
    let cat = FakeCatalog::new("db");
    let env = TypeEnv::default();
    let namer = Names { wrap: None };
    let cx = DeparseCtx::<u32, u8> {
        opts: DeparseOptions::EXPLAIN,
        namer: &namer,
        sublinks: None,
        type_env: &env,
        catalog: &cat,
    };
    let es = [col(0), bin("+", col(0), i4(1)), text("x")];
    assert_eq!(
        deparse_list(&es, &cx).unwrap(),
        vec!["i", "(i + 1)", "'x'::text"]
    );
}

#[test]
fn errors_carry_the_span() {
    struct Failing;
    impl ColumnNamer<u32> for Failing {
        fn name(&self, _: &u32) -> Result<ColText> {
            Err(crate::error::Error::internal("no name"))
        }
    }
    let cat = FakeCatalog::new("db");
    let env = TypeEnv::default();
    let cx = DeparseCtx::<u32, u8> {
        opts: DeparseOptions::EXPLAIN,
        namer: &Failing,
        sublinks: None,
        type_env: &env,
        catalog: &cat,
    };
    let mut e = col(0);
    e.span = Span { start: 7, end: 8 };
    let err = deparse_expr(&e, &cx).unwrap_err();
    assert_eq!(err.cursor_byte, Some(7));
}

#[test]
fn options_constants() {
    assert_eq!(DeparseOptions::EXPLAIN.mode, DeparseMode::Plan);
    let s = DeparseOptions::stored(false);
    assert!(s.indent && !s.pretty_paren && s.mode == DeparseMode::Stored);
    let p = DeparseOptions::stored(true);
    assert!(p.indent && p.pretty_paren);
}

// ----- is_simple（pretty の括弧）の組み合わせ ---------------------------------------------

#[test]
fn pretty_arithmetic_precedence_and_associativity() {
    let i = || col(0);
    let cases: Vec<(E, &str)> = vec![
        // 子が高優先度・親が低優先度: 括弧なし。
        (bin("+", i(), bin("*", i4(1), i4(2))), "i + 1 * 2"),
        // 子が低優先度・親が高優先度: 括弧あり。
        (bin("*", bin("+", i(), i4(1)), i4(2)), "(i + 1) * 2"),
        (bin("*", i4(2), bin("+", i(), i4(1))), "2 * (i + 1)"),
        // 同じ優先度: 左の子だけ括弧なし。
        (bin("-", bin("-", i(), i4(1)), i4(2)), "i - 1 - 2"),
        (bin("-", i(), bin("-", i4(1), i4(2))), "i - (1 - 2)"),
        (bin("+", i(), bin("+", i4(1), i4(2))), "i + (1 + 2)"),
        (bin("/", i(), bin("*", i4(2), i4(3))), "i / (2 * 3)"),
        (bin("*", bin("%", i(), i4(2)), i4(3)), "i % 2 * 3"),
        // 比較の下の算術は括弧あり、`||` の下の算術も括弧あり。
        (bin("=", i(), bin("+", i4(1), i4(2))), "i = (1 + 2)"),
        (
            bin("||", text("a"), bin("||", text("b"), text("c"))),
            "'a'::text || ('b'::text || 'c'::text)",
        ),
    ];
    for (e, want) in cases {
        assert_eq!(stored(&e, true), want);
    }
}

#[test]
fn pretty_boolean_nesting() {
    let b = || col(5);
    let cases: Vec<(E, &str)> = vec![
        (and(vec![and(vec![b(), b()]), b()]), "b AND b AND b"),
        (or(vec![and(vec![b(), b()]), b()]), "b AND b OR b"),
        (and(vec![or(vec![b(), b()]), b()]), "(b OR b) AND b"),
        (or(vec![or(vec![b(), b()]), b()]), "b OR b OR b"),
        (not(and(vec![b(), b()])), "NOT (b AND b)"),
        (not(or(vec![b(), b()])), "NOT (b OR b)"),
        (and(vec![not(b()), b()]), "NOT b AND b"),
        (not(not(b())), "NOT (NOT b)"),
        // 比較・IS NULL は AND / OR / NOT の下で括弧なし。
        (
            and(vec![is_null(col(2)), bin("=", col(0), i4(1))]),
            "t IS NULL AND i = 1",
        ),
        // 関数の引数の中の BoolExpr は括弧なし。
        (
            func("lower", vec![and(vec![b(), b()])], SqlType::TEXT),
            "lower(b AND b)",
        ),
    ];
    for (e, want) in cases {
        assert_eq!(stored(&e, true), want);
    }
}

#[test]
fn pretty_parents_that_force_parentheses() {
    // IS NULL の親は、比較・演算を括弧で包む。
    assert_eq!(
        stored(&is_null(bin("+", col(0), i4(1))), true),
        "(i + 1) IS NULL"
    );
    assert_eq!(
        stored(&bool_test(gt(col(0), i4(1)), BoolTestKind::IsTrue), true),
        "(i > 1) IS TRUE"
    );
    // IS NULL の子の IS NULL。
    assert_eq!(
        stored(&is_null(is_null(col(2))), true),
        "(t IS NULL) IS NULL"
    );
    // 比較の子の IS NULL・NOT。
    assert_eq!(
        stored(&bin("=", is_null(col(2)), tru()), true),
        "(t IS NULL) = true"
    );
    // ANY の左辺。
    let any = in_list(bin("+", col(0), i4(1)), vec![i4(1), i4(2)], false);
    assert_eq!(stored(&any, true), "(i + 1) = ANY (ARRAY[1, 2])");
    // ANY は AND の下でも括弧で包む。
    let e = and(vec![any, col(5)]);
    assert_eq!(stored(&e, true), "((i + 1) = ANY (ARRAY[1, 2])) AND b");
    // キャストの引数: 関数形のキャストは括弧なしの子、変換の引数が演算なら包む。
    let c = cast(bin("+", col(0), i4(1)), SqlType::INT8, Cm::Func, false);
    assert_eq!(stored(&c, true), "(i + 1)::bigint");
    let nested = bin(
        "=",
        cast(
            cast(col(0), SqlType::INT8, Cm::Func, false),
            SqlType::TEXT,
            Cm::Io,
            false,
        ),
        col(2),
    );
    assert_eq!(stored(&nested, true), "i::bigint::text = t");
    // CASE と COALESCE は括弧なし。
    let e = bin("+", coalesce(vec![col(0), i4(0)]), i4(1));
    assert_eq!(stored(&e, true), "COALESCE(i, 0) + 1");
}

#[test]
fn non_pretty_always_parenthesizes_operators() {
    let e = bin("+", i4(1), bin("*", i4(2), i4(3)));
    assert_eq!(stored(&e, false), "(1 + (2 * 3))");
    let e = and(vec![col(5), col(5), col(5)]);
    assert_eq!(stored(&e, false), "(b AND b AND b)");
    let e = func("lower", vec![bin("||", col(2), text("a"))], SqlType::TEXT);
    assert_eq!(stored(&e, false), "lower((t || 'a'::text))");
}

#[test]
fn unknown_literal_and_null_labels() {
    let u = lit(Datum::Text("abc".into()), SqlType::UNKNOWN);
    assert_eq!(plan(&u), "'abc'");
    assert_eq!(plan(&lit(Datum::Null, SqlType::TEXT)), "NULL::text");
    assert_eq!(plan(&lit(Datum::Null, SqlType::BOOL)), "NULL::boolean");
    assert_eq!(plan(&lit(Datum::Null, SqlType::INT4)), "NULL::integer");
}
