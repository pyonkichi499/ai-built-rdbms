//! M4 領域の生成器の単体テスト（サーバなしで、生成された SQL の形だけを検査する）。

use super::{generate, Ctx};

const M4: [&str; 6] = ["join", "agg", "subquery", "setop", "index", "ddl"];

/// 生成が KD-1〜KD-27 の未対応構文に触れていないことの目印（大文字小文字は無視）。
const FORBIDDEN: [&str; 14] = [
    "LATERAL",
    "RECURSIVE",
    "IS DISTINCT FROM",
    "ROLLUP",
    "CUBE",
    "GROUPING SETS",
    "FOR UPDATE",
    "FOR SHARE",
    "OVER (",
    "STRING_AGG",
    "ON TRUE",
    "INTERVAL",
    "ARRAY",
    "RETURNING",
];

fn gen(domain: &str, seed: u64, case: u64) -> Ctx {
    let mut c = Ctx::new(seed, case);
    generate(domain, &mut c).unwrap();
    c
}

#[test]
fn deterministic_for_same_seed_and_case() {
    for d in M4 {
        for case in 0..40 {
            assert_eq!(
                gen(d, 7, case).stmts,
                gen(d, 7, case).stmts,
                "{d} case {case}"
            );
        }
    }
}

#[test]
fn statements_are_single_line_balanced_and_terminated() {
    for d in M4 {
        for case in 0..300 {
            for s in gen(d, 1, case).stmts {
                assert!(s.ends_with(';'), "{d}/{case}: {s}");
                assert!(!s.contains('\n'), "{d}/{case}: {s}");
                assert_eq!(s.matches('\'').count() % 2, 0, "{d}/{case}: quotes {s}");
                assert_eq!(
                    s.matches('(').count(),
                    s.matches(')').count(),
                    "{d}/{case}: parens {s}"
                );
            }
        }
    }
}

#[test]
fn queries_and_pairs_point_at_valid_statements() {
    for d in M4 {
        for case in 0..300 {
            let c = gen(d, 3, case);
            for &i in c.q.keys() {
                assert!(i < c.stmts.len());
                let kw = c.stmts[i]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                assert!(
                    matches!(kw.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE")
                        || kw.starts_with('('),
                    "{d}/{case}: {}",
                    c.stmts[i]
                );
            }
            for &(b, v) in &c.pairs {
                assert!(b < v && v < c.stmts.len());
                assert!(c.q.contains_key(&b) && c.q.contains_key(&v));
            }
        }
    }
}

#[test]
fn unsupported_syntax_is_not_generated() {
    for d in M4 {
        for case in 0..300 {
            for s in gen(d, 5, case).stmts {
                let u = s.to_ascii_uppercase();
                for f in FORBIDDEN {
                    assert!(!u.contains(f), "{d}/{case}: {f} in {s}");
                }
            }
        }
    }
}

/// 各領域が狙った構文を実際に出していること。
#[test]
fn each_domain_produces_its_features() {
    let want: [(&str, &[&str]); 6] = [
        (
            "join",
            &[
                "LEFT JOIN",
                "RIGHT JOIN",
                "FULL JOIN",
                "CROSS JOIN",
                "NATURAL",
                "USING (",
                "generate_series",
            ],
        ),
        (
            "agg",
            &[
                "GROUP BY",
                "HAVING",
                "DISTINCT ON",
                "FILTER (WHERE",
                "count(DISTINCT",
                "sum(",
            ],
        ),
        (
            "subquery",
            &[
                " NOT IN (",
                "EXISTS (",
                " ANY (",
                " ALL (",
                "UNION ALL SELECT",
                "WITH c0",
                "MATERIALIZED",
            ],
        ),
        (
            "setop",
            &[
                "UNION ALL",
                "INTERSECT ALL",
                "EXCEPT ALL",
                "EXCEPT ",
                "INTERSECT ",
            ],
        ),
        (
            "index",
            &[
                "CREATE INDEX",
                " DESC",
                "NULLS FIRST",
                "PRIMARY KEY",
                "UNIQUE",
                "BETWEEN",
            ],
        ),
        (
            "ddl",
            &[
                "DROP INDEX",
                "ADD PRIMARY KEY",
                "TRUNCATE",
                "serial",
                "DROP TABLE",
                "pg_class",
                "pg_index",
            ],
        ),
    ];
    for (d, words) in want {
        let all: String = (0..400)
            .flat_map(|c| gen(d, 11, c).stmts)
            .collect::<Vec<_>>()
            .join("\n");
        for w in words {
            assert!(all.contains(w), "{d} が {w} を一度も出さない");
        }
    }
}

#[test]
fn new_column_types_are_used() {
    let all: String = (0..200)
        .flat_map(|c| gen("join", 2, c).stmts)
        .collect::<Vec<_>>()
        .join("\n");
    for t in [
        "numeric(6,2)",
        "char(3)",
        "date",
        "timestamp",
        "bigint",
        "boolean",
    ] {
        assert!(all.contains(t), "{t}");
    }
}

#[test]
fn ordered_flag_means_all_columns_are_ordered() {
    // ordered な読み取り文は ORDER BY を持つ（LIMIT は ordered のときだけ付く）
    for d in M4 {
        for case in 0..300 {
            let c = gen(d, 9, case);
            for (&i, &ordered) in &c.q {
                let s = &c.stmts[i];
                if s.contains(" LIMIT ") && !s.starts_with("SET") {
                    assert!(ordered, "{d}/{case}: LIMIT without full ORDER BY: {s}");
                }
                if ordered
                    && !s.contains("SELECT count(")
                    && !s.contains("min(")
                    && !s.contains("sum(d.c)")
                {
                    assert!(
                        s.contains("ORDER BY")
                            || s.starts_with("SELECT g")
                            || s.contains("GROUP BY")
                            || s.contains("max("),
                        "{d}/{case}: ordered without ORDER BY: {s}"
                    );
                }
            }
        }
    }
}
