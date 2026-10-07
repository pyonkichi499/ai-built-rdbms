//! 結果の比較規則（11 §3.7.2）。
//!
//! - M4 領域の読み取り文は行を多重集合として比べる。全出力列を ORDER BY で並べた文（`ordered`）だけ順序も比べる。
//! - `-0` と `0`（`-0.00` なども）の入れ替わりは許容する（どちらが残るかがプランで変わる）。
//! - 「エラーが出るかどうか」が評価順で変わる式（クラス 21・22 のエラーと行の結果、またはクラス 21・22 どうしでコードが違う）は
//!   inconclusive として数え、差分にしない。

use crate::session::Res;

/// 1 行の中の `-0` / `-0.0…` のフィールドを `0` / `0.0…` にそろえる。
/// 行の各フィールドの末尾の空白を無視する（`--ignore-trailing-space`。char(n) の詰め物の差を別扱いにして他の差分を見るため）。
static IGNORE_TRAILING_SPACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_ignore_trailing_space(on: bool) {
    IGNORE_TRAILING_SPACE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn norm_row(r: &str) -> String {
    let trim = IGNORE_TRAILING_SPACE.load(std::sync::atomic::Ordering::Relaxed);
    r.split('|')
        .map(|f| {
            let f = if trim { f.trim_end_matches(' ') } else { f };
            let neg_zero = f.strip_prefix("-0").is_some_and(|rest| {
                rest.is_empty() || (rest.starts_with('.') && rest[1..].bytes().all(|b| b == b'0'))
            });
            if neg_zero {
                f[1..].to_string()
            } else {
                f.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// 行の一致。`ordered` なら順序も比べ、そうでなければ多重集合として比べる。
pub fn rows_match(a: &[String], b: &[String], ordered: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x: Vec<String> = a.iter().map(|r| norm_row(r)).collect();
    let mut y: Vec<String> = b.iter().map(|r| norm_row(r)).collect();
    if !ordered {
        x.sort();
        y.sort();
    }
    x == y
}

/// 評価順で出る出ないが変わりうるクラス（21: カーディナリティ違反、22: データ例外）。
pub fn is_eval_class(sqlstate: &str) -> bool {
    sqlstate.starts_with("21") || sqlstate.starts_with("22")
}

pub enum Verdict {
    Same,
    Diff(&'static str),
    Inconclusive,
}

/// M4 の読み取り文の比較（PostgreSQL 対 yuzhu）。
pub fn compare_query(pg: &Res, yz: &Res, ordered: bool, no_message: bool) -> Verdict {
    match (pg, yz) {
        (Res::Ok { rows: r1, tag: t1 }, Res::Ok { rows: r2, tag: t2 }) => {
            if !rows_match(r1, r2, ordered) {
                Verdict::Diff("rows")
            } else if t1 != t2 {
                Verdict::Diff("tag")
            } else {
                Verdict::Same
            }
        }
        (
            Res::Err {
                sqlstate: s1,
                message: m1,
            },
            Res::Err {
                sqlstate: s2,
                message: m2,
            },
        ) => {
            if s1 != s2 {
                if is_eval_class(s1) && is_eval_class(s2) {
                    Verdict::Inconclusive
                } else {
                    Verdict::Diff("sqlstate")
                }
            } else if m1 != m2 && !no_message {
                Verdict::Diff("message")
            } else {
                Verdict::Same
            }
        }
        (Res::Ok { .. }, Res::Err { sqlstate, .. }) => {
            if is_eval_class(sqlstate) {
                Verdict::Inconclusive
            } else {
                Verdict::Diff("yuzhu_error_pg_ok")
            }
        }
        (Res::Err { sqlstate, .. }, Res::Ok { .. }) => {
            if is_eval_class(sqlstate) {
                Verdict::Inconclusive
            } else {
                Verdict::Diff("yuzhu_ok_pg_error")
            }
        }
        _ => Verdict::Diff("status"),
    }
}

/// 同じサーバでの基準の結果と変種の結果が同じか（プラン変種・索引の有無）。None なら一致（または判定不能）。
pub fn variant_mismatch(base: &Res, var: &Res, ordered: bool) -> bool {
    match (base, var) {
        (Res::Ok { rows: a, .. }, Res::Ok { rows: b, .. }) => !rows_match(a, b, ordered),
        (Res::Err { sqlstate: a, .. }, Res::Err { sqlstate: b, .. }) => {
            a != b && !(is_eval_class(a) && is_eval_class(b))
        }
        (Res::Ok { .. }, Res::Err { sqlstate, .. })
        | (Res::Err { sqlstate, .. }, Res::Ok { .. }) => !is_eval_class(sqlstate),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(rows: &[&str]) -> Res {
        Res::Ok {
            rows: rows.iter().map(|s| s.to_string()).collect(),
            tag: String::new(),
        }
    }

    fn err(st: &str) -> Res {
        Res::Err {
            sqlstate: st.into(),
            message: "m".into(),
        }
    }

    #[test]
    fn negative_zero_is_normalised() {
        assert_eq!(norm_row("-0|1|-0.00|-0.5|a-0"), "0|1|0.00|-0.5|a-0");
        assert_eq!(norm_row("-0x"), "-0x");
    }

    #[test]
    fn unordered_compares_as_multiset() {
        let a = ["1|a".to_string(), "2|b".to_string(), "1|a".to_string()];
        let b = ["1|a".to_string(), "1|a".to_string(), "2|b".to_string()];
        assert!(rows_match(&a, &b, false));
        assert!(!rows_match(&a, &b, true));
        // 多重度が違えば不一致
        let c = ["1|a".to_string(), "2|b".to_string(), "2|b".to_string()];
        assert!(!rows_match(&a, &c, false));
        assert!(!rows_match(&a, &c[..2], false));
    }

    #[test]
    fn minus_zero_is_tolerated_in_both_modes() {
        let a = ["-0".to_string()];
        let b = ["0".to_string()];
        assert!(rows_match(&a, &b, true));
        assert!(rows_match(&a, &b, false));
    }

    #[test]
    fn eval_order_errors_are_inconclusive() {
        assert!(matches!(
            compare_query(&ok(&["1"]), &err("22012"), false, false),
            Verdict::Inconclusive
        ));
        assert!(matches!(
            compare_query(&err("22003"), &err("22012"), false, false),
            Verdict::Inconclusive
        ));
        assert!(matches!(
            compare_query(&err("21000"), &ok(&[]), false, false),
            Verdict::Inconclusive
        ));
        assert!(matches!(
            compare_query(&ok(&["1"]), &err("42703"), false, false),
            Verdict::Diff("yuzhu_error_pg_ok")
        ));
        assert!(matches!(
            compare_query(&err("42703"), &err("42883"), false, false),
            Verdict::Diff("sqlstate")
        ));
        assert!(matches!(
            compare_query(&err("42703"), &err("42703"), false, false),
            Verdict::Same
        ));
    }

    #[test]
    fn variant_check_ignores_eval_order_errors() {
        assert!(!variant_mismatch(&ok(&["1"]), &err("22012"), false));
        assert!(variant_mismatch(&ok(&["1"]), &err("XX000"), false));
        assert!(variant_mismatch(&ok(&["1"]), &ok(&["2"]), true));
        assert!(!variant_mismatch(&ok(&["1", "2"]), &ok(&["2", "1"]), false));
    }
}
