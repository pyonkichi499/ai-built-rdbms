//! 組み込み表の行: numeric の穴埋め（`m5/05-types-core.md` §5.2、TY-2）。
//!
//! 差の洗い出しは `tests/tools/typegap/gap.sh`。ここには `gcd` `lcm` `min_scale` `trim_scale`
//! `width_bucket` `factorial` と、本体が `0A000` の `pow`（TY-D15）を足す。`@`（前置）と
//! `sqrt` / `exp` / `ln` / `log` / `log10` / `power` の numeric 版の行は `core.rs` が持つ。

#![allow(clippy::wildcard_imports)]

use super::*;
use crate::types::numeric as nf;

const NUMERIC: Oid = oid::NUMERIC;

/// 呼べる関数（`FUNCTIONS` に結合される）。
pub(super) static FUNCTIONS: &[BuiltinFunction] = &[
    func(5048, "gcd", &[NUMERIC, NUMERIC], NUMERIC, nf::numeric_gcd),
    func(5049, "lcm", &[NUMERIC, NUMERIC], NUMERIC, nf::numeric_lcm),
    func(
        5042,
        "min_scale",
        &[NUMERIC],
        oid::INT4,
        nf::numeric_min_scale,
    ),
    func(
        5043,
        "trim_scale",
        &[NUMERIC],
        NUMERIC,
        nf::numeric_trim_scale,
    ),
    func(
        2170,
        "width_bucket",
        &[NUMERIC, NUMERIC, NUMERIC, oid::INT4],
        oid::INT4,
        nf::numeric_width_bucket,
    ),
    func(
        320,
        "width_bucket",
        &[oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, oid::INT4],
        oid::INT4,
        nf::float8_width_bucket,
    ),
    func(
        1376,
        "factorial",
        &[oid::INT8],
        NUMERIC,
        nf::numeric_factorial,
    ),
    func(1738, "pow", &[NUMERIC, NUMERIC], NUMERIC, nf::numeric_pow),
];

const fn proc_row(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    prosrc: &'static str,
) -> BuiltinProc {
    BuiltinProc {
        oid,
        name,
        args,
        result,
        strict: true,
        volatility: 'i',
        parallel: 's',
        leakproof: false,
        cost: 1.0,
        prosrc,
    }
}

/// `pg_proc` の行（PostgreSQL 17 の値）。
pub(super) static PROCS: &[BuiltinProc] = &[
    proc_row(5048, "gcd", &[NUMERIC, NUMERIC], NUMERIC, "numeric_gcd"),
    proc_row(5049, "lcm", &[NUMERIC, NUMERIC], NUMERIC, "numeric_lcm"),
    proc_row(
        5042,
        "min_scale",
        &[NUMERIC],
        oid::INT4,
        "numeric_min_scale",
    ),
    proc_row(
        5043,
        "trim_scale",
        &[NUMERIC],
        NUMERIC,
        "numeric_trim_scale",
    ),
    proc_row(
        2170,
        "width_bucket",
        &[NUMERIC, NUMERIC, NUMERIC, oid::INT4],
        oid::INT4,
        "width_bucket_numeric",
    ),
    proc_row(
        320,
        "width_bucket",
        &[oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, oid::INT4],
        oid::INT4,
        "width_bucket_float8",
    ),
    proc_row(1376, "factorial", &[oid::INT8], NUMERIC, "numeric_fac"),
    proc_row(1738, "pow", &[NUMERIC, NUMERIC], NUMERIC, "numeric_power"),
];

/// 差分コーパス: `tests/tools/typegap/pg17_<type>.tsv`（PG17 の実機）と `PROCS` / `FUNCTIONS` /
/// `OPERATORS` / `CASTS` を突き合わせ、PG にあって yuzhu にない行が
/// `expected_missing_<type>.txt`（理由つきの記録）と一致することを確かめる。
/// 一覧を作り直すときは `TYPEGAP_PRINT=1 cargo test -p yuzhu-core typegap -- --nocapture`。
#[cfg(test)]
mod typegap_tests {
    use crate::catalog::builtin as b;
    use crate::types::Oid;
    use std::collections::BTreeSet;

    const DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../tests/tools/typegap"
    );

    fn pg_rows(ty: &str) -> Vec<(String, String, String)> {
        let text = std::fs::read_to_string(format!("{DIR}/pg17_{ty}.tsv")).unwrap();
        text.lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (
                    f[0].to_owned(),
                    f[1].to_owned(),
                    f.get(2).copied().unwrap_or("").to_owned(),
                )
            })
            .filter(|(k, _, _)| k != "amop")
            .collect()
    }

    fn have(kind: &str, id: &str) -> bool {
        match kind {
            "proc" => {
                let o: Oid = id.parse().unwrap();
                b::PROCS.iter().any(|p| p.oid == o) || b::FUNCTIONS.iter().any(|f| f.oid == o)
            }
            "oper" => {
                let o: Oid = id.parse().unwrap();
                b::OPERATORS.iter().any(|p| p.oid == o)
            }
            "cast" => {
                let (s, t) = id.split_once('>').unwrap();
                b::find_cast(s.parse().unwrap(), t.parse().unwrap()).is_some()
            }
            _ => unreachable!(),
        }
    }

    fn check(ty: &str) {
        let missing: BTreeSet<String> = pg_rows(ty)
            .into_iter()
            .filter(|(k, id, _)| !have(k, id))
            .map(|(k, id, _)| format!("{k}\t{id}"))
            .collect();
        if std::env::var_os("TYPEGAP_PRINT").is_some() {
            for (k, id, n) in pg_rows(ty).into_iter().filter(|(k, id, _)| !have(k, id)) {
                println!("{k}\t{id}\t{n}");
            }
        }
        let text = std::fs::read_to_string(format!("{DIR}/expected_missing_{ty}.txt")).unwrap();
        let expected: BTreeSet<String> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split('\t').take(2).collect::<Vec<_>>().join("\t"))
            .filter(|l| !l.starts_with("amop"))
            .collect();
        let new: Vec<_> = missing.difference(&expected).collect();
        let gone: Vec<_> = expected.difference(&missing).collect();
        assert!(
            new.is_empty() && gone.is_empty(),
            "{ty}: PG にあって yuzhu にない行が増えた {new:?} / 実装されたので一覧から消す行 {gone:?}"
        );
    }

    #[test]
    fn numeric_gap_matches_expected() {
        check("numeric");
    }

    #[test]
    fn bpchar_gap_matches_expected() {
        check("bpchar");
    }

    #[test]
    fn ty2_rows_are_consistent() {
        for f in b::FUNCTIONS {
            let p = b::PROCS
                .iter()
                .find(|p| p.oid == f.oid)
                .expect("pg_proc row");
            assert_eq!((p.name, p.args, p.result), (f.name, f.args, f.result));
        }
        for p in b::PROCS {
            assert!(b::proc_by_oid(p.oid).is_some());
        }
    }
}
