//! query 領域の拡張（ラウンド 3）: 型混在の VALUES / UNION なしの型推論、BETWEEN SYMMETRIC、LIKE、WITH TIES、
//! 別名と式の ORDER BY、前ラウンドの型（date/timestamp/real/char/bytea/numeric）の入力を増やす。

use super::query::{dir, limit_offset};
use super::{Ctx, Table};

const LITS: &[&str] = &[
    "1",
    "2",
    "-1",
    "0",
    "NULL",
    "2.5",
    "0.10",
    "1.0e0",
    "'a'",
    "'B'",
    "''",
    "'abc'",
    "true",
    "false",
    "10000000000",
    "2147483648",
    "'2024-02-29'",
    "0.5::numeric(3,1)",
    "(-7)::int2",
    "1::int2",
    "1::bigint",
    "1.5::real",
    "'NaN'::float8",
    "'Infinity'::float8",
    "-0.0",
    "'x'::varchar(2)",
    "123456789.123456789",
    "1e300",
    "'1'",
    "'t'::bool",
    "1e-5",
    "'-Infinity'::float8",
];

fn lit(ctx: &mut Ctx) -> &'static str {
    ctx.rng.pick(LITS)
}

pub fn r3_stmt(ctx: &mut Ctx, t: &Table) {
    let c = ctx.rng.pick(&t.cols).name.clone();
    let c2 = ctx.rng.pick(&t.cols).name.clone();
    let tn = t.name.clone();
    let all = t.positions();
    let (l1, l2, l3) = (lit(ctx), lit(ctx), lit(ctx));
    let mut sql = match ctx.rng.below(30) {
        0 => format!("SELECT {l1} AS a, {l2} AS b, pg_typeof({l1})::text, pg_typeof({l2})::text;"),
        1 => format!("VALUES ({l1}), ({l2}), ({l3}) ORDER BY 1{}", dir(ctx)),
        2 => format!("SELECT pg_typeof({l1})::text, pg_typeof({l2})::text, {l1} AS x, {l2} AS y ORDER BY 3, 4"),
        3 => format!("VALUES ({l1}, {l2}), ({l2}, {l3}) ORDER BY 1{}, 2{}", dir(ctx), dir(ctx)),
        4 => format!("SELECT CASE WHEN {c} IS NULL THEN {l1} WHEN {c2} IS NULL THEN {l2} ELSE {l3} END AS k, pg_typeof(CASE WHEN {c} IS NULL THEN {l1} ELSE {l2} END)::text FROM {tn} ORDER BY 1{}, 2", dir(ctx)),
        5 => format!("SELECT COALESCE({l1}, {l2}, {l3}) AS k FROM {tn} ORDER BY 1{}", dir(ctx)),
        6 => format!("SELECT {c} FROM {tn} WHERE {c} BETWEEN SYMMETRIC {l1} AND {l2} ORDER BY 1{}", dir(ctx)),
        7 => format!("SELECT {c} FROM {tn} WHERE {c} NOT BETWEEN {l1} AND {l2} ORDER BY 1{}", dir(ctx)),
        8 => format!("SELECT {c}, {c2} FROM {tn} WHERE {c}::text LIKE '%1%' ORDER BY {all}"),
        9 => format!("SELECT {c}, {c2} FROM {tn} WHERE {c}::text NOT ILIKE '_' ORDER BY {all}"),
        10 => format!("SELECT {c} FROM {tn} ORDER BY {c}{} LIMIT {}", dir(ctx), ctx.rng.range(0, 3)),
        11 => format!("SELECT {c} FROM {tn} ORDER BY {c}{} OFFSET {} ROWS FETCH FIRST {} ROW ONLY", dir(ctx), ctx.rng.range(0, 3), ctx.rng.range(1, 3)),
        12 => format!("SELECT {c} IS NULL AS n, {c2} IS NOT NULL AS m FROM {tn} ORDER BY 1, 2{}", dir(ctx)),
        13 => format!("SELECT {c} AS a FROM {tn} ORDER BY a + 1;"),
        14 => format!("SELECT {c} AS a FROM {tn} ORDER BY a::text{}, 1", dir(ctx)),
        15 => format!("SELECT {c} AS {c2}, {c2} AS {c} FROM {tn} ORDER BY {c}{}, {c2}{}", dir(ctx), dir(ctx)),
        16 => format!("SELECT DISTINCT {c} IS NULL, {c2} IS NULL FROM {tn} ORDER BY 1, 2"),
        17 => format!("SELECT {c}, {c2} FROM {tn} ORDER BY {c}{}, {c2}{}", dir(ctx), dir(ctx)),
        18 => format!("SELECT {l1} = {l2}, {l1} < {l2}, {l1} IS DISTINCT FROM {l2};"),
        19 => format!("SELECT {l1} IN ({l2}, {l3}), {l1} NOT IN ({l2}, NULL);"),
        20 => format!("SELECT {l1} AS \"x y\", {l2} AS \"\", {l3};"),
        21 => format!("SELECT {l1}, {l1}, {l2} AS c, {l2} AS c FROM {tn} ORDER BY 1, 2, 3, 4 LIMIT 1"),
        22 => format!("SELECT {c} FROM {tn} WHERE {c} IN ({l1}, {l2}, {c2}) ORDER BY 1"),
        23 => format!("SELECT {c} FROM {tn} WHERE {c} NOT IN ({l1}, {l3}, {c2}) ORDER BY 1"),
        24 => format!("SELECT {c}, {c2} FROM {tn} WHERE {c} = {l1} AND {c2} = {l2} ORDER BY {all}"),
        25 => format!("SELECT {c} IS NULL, {c2} IS NOT NULL, {l1} IS NULL FROM {tn} ORDER BY 1, 2, 3"),
        26 => format!("SELECT {c}, {c2} FROM {tn} ORDER BY {c} IS NULL{}, {all}", dir(ctx)),
        27 => format!("SELECT NULLIF({l1}, {l2}), GREATEST({l1}, {l2}), LEAST({l2}, {l3});"),
        28 => format!("SELECT {l1}::text, {l2}::text, ({l3})::text;"),
        _ => format!("SELECT * FROM {tn} WHERE {c} = {l1} OR {c2} IS DISTINCT FROM {l2} ORDER BY {all}"),
    };
    if sql.ends_with(';') {
        sql.pop();
    }
    limit_offset(ctx, &mut sql);
    sql.push(';');
    ctx.push(sql);
}
