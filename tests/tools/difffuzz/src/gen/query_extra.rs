//! query 領域の拡張（ラウンド 2）: 前ラウンドの型（date/timestamp/real/char/bytea など）と構造の追加ケース。

use super::query::{dir, limit_offset};
use super::{Ctx, Table};

const XCOLS: &[(&str, &[&str])] = &[
    ("real", &["1.5", "-0.25", "NULL", "3", "'NaN'", "100.125"]),
    ("numeric", &["1.50", "-0.1", "NULL", "100", "0.000", "123456789.123456789"]),
    ("float8", &["1.5", "-0", "NULL", "'Infinity'", "'NaN'", "1e300"]),
    ("int2", &["1", "-5", "NULL", "32767", "0"]),
    ("varchar", &["'b'", "'B'", "NULL", "'a b'", "''"]),
];

pub fn xtable_scenario(ctx: &mut Ctx) {
    let name = ctx.fresh_table_name();
    let n = ctx.rng.range(2, 4) as usize;
    let cols: Vec<usize> = (0..n).map(|_| ctx.rng.below(XCOLS.len() as u64) as usize).collect();
    let defs: Vec<String> = cols.iter().enumerate().map(|(i, &k)| format!("x{i} {}", XCOLS[k].0)).collect();
    ctx.push(format!("CREATE TABLE {name} ({});", defs.join(", ")));
    for _ in 0..ctx.rng.range(1, 2) {
        let nr = ctx.rng.range(2, 6);
        let rows: Vec<String> = (0..nr)
            .map(|_| {
                let v: Vec<&str> = cols.iter().map(|&k| *ctx.rng.pick(XCOLS[k].1)).collect();
                format!("({})", v.join(", "))
            })
            .collect();
        ctx.push(format!("INSERT INTO {name} VALUES {};", rows.join(", ")));
    }
    let pos: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
    let all = pos.join(", ");
    for _ in 0..ctx.rng.range(4, 8) {
        let c = ctx.rng.below(n as u64) as usize;
        let c2 = ctx.rng.below(n as u64) as usize;
        let lit = *ctx.rng.pick(XCOLS[cols[c]].1);
        let mut sql = match ctx.rng.below(12) {
            0 => format!("SELECT DISTINCT x{c} FROM {name} ORDER BY 1{}", dir(ctx)),
            1 => format!("SELECT DISTINCT ON (x{c}) x{c}, x{c2} FROM {name} ORDER BY x{c}{}, 2", dir(ctx)),
            2 => format!("SELECT * FROM {name} WHERE x{c} = {lit} ORDER BY {all}"),
            3 => format!("SELECT * FROM {name} WHERE x{c} IS DISTINCT FROM {lit} ORDER BY {all}"),
            4 => format!("SELECT * FROM {name} WHERE x{c} < {lit} OR x{c} >= {lit} ORDER BY {all}"),
            5 => format!("SELECT * FROM {name} WHERE x{c} IN ({lit}, NULL) ORDER BY {all}"),
            6 => format!(
                "SELECT pg_typeof(x{c})::text, pg_typeof(x{c2})::text, x{c}, x{c2} FROM {name} ORDER BY 3{}, 4, 1",
                dir(ctx)
            ),
            7 => format!("SELECT x{c}, x{c2} FROM {name} ORDER BY x{c}{}, x{c2}{}", dir(ctx), dir(ctx)),
            8 => format!("SELECT COALESCE(x{c}, {lit}) AS c, x{c2} FROM {name} ORDER BY 1{}, 2", dir(ctx)),
            9 => format!("SELECT x{c} = x{c2}, x{c} < x{c2}, x{c} IS NULL FROM {name} ORDER BY 1, 2, 3"),
            10 => format!(
                "SELECT CASE WHEN x{c} IS NULL THEN {lit} ELSE x{c} END AS k, x{c2} FROM {name} ORDER BY 1{}, 2",
                dir(ctx)
            ),
            _ => format!("SELECT GREATEST(x{c}, {lit}), LEAST(x{c}, {lit}), x{c2} FROM {name} ORDER BY 1, 2, 3"),
        };
        limit_offset(ctx, &mut sql);
        sql.push(';');
        ctx.push(sql);
    }
}

pub fn structure_extra(ctx: &mut Ctx, t: &Table) {
    let c = ctx.rng.pick(&t.cols).name.clone();
    let c2 = ctx.rng.pick(&t.cols).name.clone();
    let n = t.cols.len();
    let tn = &t.name;
    let all = t.positions();
    let sql = match ctx.rng.below(18) {
        0 => format!("SELECT {c} AS a, {c2} AS a FROM {tn} ORDER BY a;"),
        1 => format!("SELECT {c} AS a FROM {tn} ORDER BY {}, 1;", n + 3),
        2 => format!("SELECT DISTINCT {c} FROM {tn} ORDER BY {c2};"),
        3 => format!("SELECT DISTINCT ON ({c}) * FROM {tn} ORDER BY {c2};"),
        4 => format!("SELECT DISTINCT ON ({c}) * FROM {tn} ORDER BY {c}{}, {all};", dir(ctx)),
        5 => format!("SELECT {c} AS x, {c2} AS y FROM {tn} ORDER BY y{}, x{}, {all};", dir(ctx), dir(ctx)),
        6 => format!("SELECT {c} FROM {tn} ORDER BY 0;"),
        7 => format!("SELECT {c} FROM {tn} ORDER BY -1;"),
        8 => format!("SELECT {c} FROM {tn} ORDER BY 1.5;"),
        9 => format!("SELECT {c} FROM {tn} ORDER BY 'a';"),
        10 => format!("SELECT * FROM {tn} LIMIT 1 OFFSET 1 ORDER BY 1;"),
        11 => format!("SELECT * FROM {tn} ORDER BY {all} LIMIT 'a';"),
        12 => format!("SELECT * FROM {tn} ORDER BY {all} LIMIT 1.5;"),
        13 => format!("SELECT * FROM {tn} ORDER BY {all} LIMIT {c};"),
        14 => format!("SELECT {c} FROM {tn} WHERE {c2};"),
        15 => format!("SELECT {c} AS \"A\", {c2} AS a FROM {tn} ORDER BY \"A\", a, {all};"),
        16 => format!("SELECT {c} FROM {tn} ORDER BY {c} USING <;"),
        _ => format!(
            "SELECT {tn}.{c}, {tn}.* FROM {tn} ORDER BY {};",
            (1..=n + 1).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        ),
    };
    ctx.push(sql);
}

pub fn extra_values(ctx: &mut Ctx) -> String {
    const LITS: &[&str] = &[
        "2.5::numeric", "(-1)::bigint", "1.5::real", "'x'::varchar(2)", "1::int2",
        "1.5e0::float8", "0.0", "-0.0", "1e0", "'NaN'::float8", "NULL", "'a'", "1", "10000000000", "2.50",
        "true", "'1'::text", "CAST(1 AS bigint)",
    ];
    let nrow = ctx.rng.range(2, 4);
    let rows: Vec<String> = (0..nrow).map(|_| format!("({})", ctx.rng.pick(LITS))).collect();
    let mut sql = match ctx.rng.below(3) {
        0 => format!("VALUES {} ORDER BY 1{}", rows.join(", "), dir(ctx)),
        1 => format!("VALUES {}", rows.join(", ")),
        _ => format!("VALUES {} ORDER BY 1{} OFFSET 1", rows.join(", "), dir(ctx)),
    };
    limit_offset(ctx, &mut sql);
    sql.push(';');
    sql
}
