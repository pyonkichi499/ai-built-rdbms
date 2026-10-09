//! `VACUUM` / `ANALYZE` のオプションの解釈と `BoundAnalyze`（M5 VC。03 §1.1、§6.7、10 KD-8）。
//!
//! 構文木（`sql::ast::Vacuum`）と `BoundVacuum` は M4 の形のまま（オプションは名前と値の文字列）。
//! ここは文字列のオプションを検査して [`VacuumOptions`] にする。メッセージと SQLSTATE は PostgreSQL 17 の実機で確かめた。
//! ロックは取らず、カタログも引かない（名前を OID にするのは `analyzer/ddl_index.rs`）。

use crate::analyzer::bound::{VacuumOption, VacuumTarget};
use crate::ddl::parse_bool;
use crate::error::{Error, Result, sqlstate};

/// `INDEX_CLEANUP`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexCleanup {
    Auto,
    On,
    Off,
}

/// 検査済みの `VACUUM` / `ANALYZE` のオプション（03 §4.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct VacuumOptions {
    pub verbose: bool,
    /// `VACUUM (ANALYZE)` または `ANALYZE` 文。
    pub analyze: bool,
    /// 受け付けるだけ。常に horizon まで凍結するので、有無で挙動は変わらない（KD-8）。
    pub freeze: bool,
    pub skip_locked: bool,
    pub index_cleanup: IndexCleanup,
    pub truncate: bool,
    pub only_database_stats: bool,
    pub skip_database_stats: bool,
    /// 試験用。`None` は `maintenance_work_mem` から決める。
    pub tid_capacity: Option<usize>,
}

impl Default for VacuumOptions {
    fn default() -> Self {
        VacuumOptions {
            verbose: false,
            analyze: false,
            freeze: false,
            skip_locked: false,
            index_cleanup: IndexCleanup::Auto,
            truncate: true,
            only_database_stats: false,
            skip_database_stats: false,
            tid_capacity: None,
        }
    }
}

/// `ANALYZE` 文（`VACUUM` を伴わないもの）の実行用の形（03 §4.2）。
#[derive(Debug, Clone)]
pub struct BoundAnalyze {
    pub verbose: bool,
    pub skip_locked: bool,
    /// 空なら現在のデータベースの全テーブル。
    pub targets: Vec<VacuumTarget>,
}

/// `VACUUM (...)` で受け付けるオプション。
const VACUUM_OPTIONS: &[&str] = &[
    "analyze",
    "verbose",
    "freeze",
    "full",
    "disable_page_skipping",
    "skip_locked",
    "index_cleanup",
    "truncate",
    "parallel",
    "process_main",
    "process_toast",
    "skip_database_stats",
    "only_database_stats",
    "buffer_usage_limit",
];

/// `ANALYZE (...)` で受け付けるオプション。
const ANALYZE_OPTIONS: &[&str] = &["verbose", "skip_locked", "buffer_usage_limit"];

const BUFFER_LIMIT_MESSAGE: &str =
    "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB";

fn syntax(message: impl Into<String>) -> Error {
    Error::new(sqlstate::SYNTAX_ERROR, message.into())
}

/// `defGetBoolean`: 値なしは真。`true`/`false`/`on`/`off`/`1`/`0` など。
fn bool_value(name: &str, value: Option<&str>) -> Result<bool> {
    match value {
        None => Ok(true),
        Some(v) => parse_bool(v).ok_or_else(|| syntax(format!("{name} requires a Boolean value"))),
    }
}

/// `BUFFER_USAGE_LIMIT` の値を kB にする（単位は `kB` `MB` `GB` `TB`、なければ kB）。読めなければ `None`。
fn buffer_limit_kb(v: &str) -> Option<i64> {
    let v = v.trim();
    let digits_end = v
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(v.len(), |(i, _)| i);
    let (num, unit) = v.split_at(digits_end);
    let n: i64 = num.parse().ok()?;
    let factor: i64 = match unit.trim() {
        "" | "kB" => 1,
        "MB" => 1 << 10,
        "GB" => 1 << 20,
        "TB" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(factor)
}

fn check_buffer_limit(value: Option<&str>) -> Result<()> {
    let Some(v) = value else {
        return Err(syntax("buffer_usage_limit requires a parameter"));
    };
    match buffer_limit_kb(v) {
        Some(0) => Ok(()),
        Some(n) if (128..=16_777_216).contains(&n) => Ok(()),
        _ => Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            BUFFER_LIMIT_MESSAGE,
        )),
    }
}

/// `VACUUM` / `ANALYZE` のオプションを検査して [`VacuumOptions`] にする。
///
/// `vacuum` が偽なら `ANALYZE` 文。`targets` は列リストと `ONLY_DATABASE_STATS` の検査に使う。
/// 検査の順序は PostgreSQL の `ExecVacuum` に合わせる: オプションごと（出現順）→ `PARALLEL` と `FULL` →
/// 列リスト → `ONLY_DATABASE_STATS` → yuzhu 独自の `FULL` の拒否（`0A000`）。
#[allow(clippy::too_many_lines)]
pub fn resolve_options(
    vacuum: bool,
    options: &[VacuumOption],
    targets: &[VacuumTarget],
) -> Result<VacuumOptions> {
    let (allowed, command) = if vacuum {
        (VACUUM_OPTIONS, "VACUUM")
    } else {
        (ANALYZE_OPTIONS, "ANALYZE")
    };
    let mut out = VacuumOptions {
        analyze: !vacuum,
        ..VacuumOptions::default()
    };
    let (mut full, mut parallel, mut disable_page_skipping) = (false, 0_i64, false);
    let mut buffer_limit: Option<Option<&str>> = None;
    for o in options {
        let name = o.name.as_str();
        if !allowed.contains(&name) {
            return Err(syntax(format!("unrecognized {command} option \"{name}\"")));
        }
        let value = o.value.as_deref();
        match name {
            "verbose" => out.verbose = bool_value(name, value)?,
            "analyze" => out.analyze = bool_value(name, value)?,
            "freeze" => out.freeze = bool_value(name, value)?,
            "full" => full = bool_value(name, value)?,
            "skip_locked" => out.skip_locked = bool_value(name, value)?,
            "truncate" => out.truncate = bool_value(name, value)?,
            "only_database_stats" => out.only_database_stats = bool_value(name, value)?,
            "skip_database_stats" => out.skip_database_stats = bool_value(name, value)?,
            "disable_page_skipping" => disable_page_skipping = bool_value(name, value)?,
            // 受け付けて無視する。真偽値としての検査だけ行う。
            "process_main" | "process_toast" => {
                bool_value(name, value)?;
            }
            "index_cleanup" => {
                out.index_cleanup = match value {
                    Some(v) if v.eq_ignore_ascii_case("auto") => IndexCleanup::Auto,
                    v => {
                        if bool_value(name, v)? {
                            IndexCleanup::On
                        } else {
                            IndexCleanup::Off
                        }
                    }
                };
            }
            "parallel" => {
                let Some(v) = value else {
                    return Err(syntax(
                        "parallel option requires a value between 0 and 1024",
                    ));
                };
                let n: i64 = v
                    .trim()
                    .parse()
                    .map_err(|_| syntax("parallel requires an integer value"))?;
                if !(0..=1024).contains(&n) {
                    return Err(syntax(
                        "parallel workers for vacuum must be between 0 and 1024",
                    ));
                }
                parallel = n;
            }
            "buffer_usage_limit" => buffer_limit = Some(value),
            _ => unreachable!("{name} is in the allowed list"),
        }
    }
    if let Some(v) = buffer_limit {
        check_buffer_limit(v)?;
    }
    if !vacuum {
        return Ok(out);
    }
    if full && parallel > 0 {
        return Err(Error::not_supported(
            "VACUUM FULL cannot be performed in parallel",
        ));
    }
    if targets.iter().any(|t| !t.columns.is_empty()) && !out.analyze {
        return Err(Error::not_supported(
            "ANALYZE option must be specified when a column list is provided",
        ));
    }
    if out.only_database_stats {
        if !targets.is_empty() {
            return Err(Error::not_supported(
                "ONLY_DATABASE_STATS cannot be specified with a list of tables",
            ));
        }
        // `verbose` / `process_main` / `process_toast` 以外とは併用できない。
        if out.analyze
            || out.freeze
            || full
            || out.skip_locked
            || out.skip_database_stats
            || disable_page_skipping
        {
            return Err(Error::not_supported(
                "ONLY_DATABASE_STATS cannot be specified with other VACUUM options",
            ));
        }
    }
    if full {
        return Err(Error::not_supported("VACUUM FULL is not supported yet"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(name: &str, value: Option<&str>) -> VacuumOption {
        VacuumOption {
            name: name.into(),
            value: value.map(str::to_owned),
        }
    }

    fn target(columns: &[&str]) -> VacuumTarget {
        VacuumTarget {
            name: "t".into(),
            table: None,
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn vac(options: &[VacuumOption]) -> Result<VacuumOptions> {
        resolve_options(true, options, &[])
    }

    fn err(options: &[VacuumOption]) -> Error {
        vac(options).unwrap_err()
    }

    #[test]
    fn defaults() {
        let o = vac(&[]).unwrap();
        assert_eq!(o, VacuumOptions::default());
        assert!(o.truncate && !o.analyze && o.index_cleanup == IndexCleanup::Auto);
        let a = resolve_options(false, &[], &[]).unwrap();
        assert!(a.analyze && !a.verbose);
    }

    #[test]
    fn every_option_is_accepted_and_read() {
        let o = vac(&[
            opt("verbose", None),
            opt("analyze", Some("on")),
            opt("freeze", Some("true")),
            opt("skip_locked", Some("1")),
            opt("truncate", Some("off")),
            opt("skip_database_stats", Some("yes")),
            opt("index_cleanup", Some("off")),
        ])
        .unwrap();
        assert!(o.verbose && o.analyze && o.freeze && o.skip_locked && o.skip_database_stats);
        assert!(!o.truncate);
        assert_eq!(o.index_cleanup, IndexCleanup::Off);
        // 受け付けて無視するもの。
        vac(&[
            opt("disable_page_skipping", None),
            opt("process_main", Some("false")),
            opt("process_toast", Some("off")),
            opt("parallel", Some("0")),
            opt("buffer_usage_limit", Some("1MB")),
        ])
        .unwrap();
        // 後勝ち。
        assert!(
            !vac(&[opt("verbose", None), opt("verbose", Some("off"))])
                .unwrap()
                .verbose
        );
    }

    #[test]
    fn index_cleanup_values() {
        for (v, want) in [
            (None, IndexCleanup::On),
            (Some("on"), IndexCleanup::On),
            (Some("true"), IndexCleanup::On),
            (Some("auto"), IndexCleanup::Auto),
            (Some("AUTO"), IndexCleanup::Auto),
            (Some("off"), IndexCleanup::Off),
            (Some("0"), IndexCleanup::Off),
        ] {
            assert_eq!(
                vac(&[opt("index_cleanup", v)]).unwrap().index_cleanup,
                want,
                "{v:?}"
            );
        }
        for v in ["zz", "2"] {
            let e = err(&[opt("index_cleanup", Some(v))]);
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
            assert_eq!(e.message, "index_cleanup requires a Boolean value");
        }
    }

    #[test]
    fn unknown_options_depend_on_the_command() {
        let e = err(&[opt("foo", None)]);
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "unrecognized VACUUM option \"foo\"");
        for name in ["full", "analyze", "freeze", "index_cleanup", "parallel"] {
            let e = resolve_options(false, &[opt(name, None)], &[]).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
            assert_eq!(e.message, format!("unrecognized ANALYZE option \"{name}\""));
        }
        let a = resolve_options(
            false,
            &[
                opt("verbose", Some("on")),
                opt("skip_locked", None),
                opt("buffer_usage_limit", Some("0")),
            ],
            &[],
        )
        .unwrap();
        assert!(a.verbose && a.skip_locked && a.analyze);
    }

    #[test]
    fn boolean_values() {
        for v in ["on", "off", "true", "false", "1", "0", "t", "f"] {
            vac(&[opt("verbose", Some(v))]).unwrap();
        }
        for name in ["verbose", "truncate", "freeze", "process_main"] {
            for v in ["maybe", "2", "auto"] {
                let e = err(&[opt(name, Some(v))]);
                assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
                assert_eq!(e.message, format!("{name} requires a Boolean value"));
            }
        }
    }

    #[test]
    fn parallel_values() {
        let e = err(&[opt("parallel", None)]);
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(
            e.message,
            "parallel option requires a value between 0 and 1024"
        );
        for v in ["a", "1.5"] {
            assert_eq!(
                err(&[opt("parallel", Some(v))]).message,
                "parallel requires an integer value"
            );
        }
        for v in ["-1", "1025"] {
            let e = err(&[opt("parallel", Some(v))]);
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
            assert_eq!(
                e.message,
                "parallel workers for vacuum must be between 0 and 1024"
            );
        }
        vac(&[opt("parallel", Some("1000"))]).unwrap();
        vac(&[opt("parallel", Some("1024"))]).unwrap();
    }

    #[test]
    fn buffer_usage_limit_values() {
        for v in ["0", "128", "128kB", "1MB", "16GB", "16777216", "0kB"] {
            vac(&[opt("buffer_usage_limit", Some(v))]).unwrap();
        }
        for v in ["100", "127kB", "17GB", "abc", "-1", "1mb", "16777217"] {
            let e = err(&[opt("buffer_usage_limit", Some(v))]);
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE, "{v}");
            assert_eq!(e.message, BUFFER_LIMIT_MESSAGE, "{v}");
        }
        let e = err(&[opt("buffer_usage_limit", None)]);
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "buffer_usage_limit requires a parameter");
        // ANALYZE でも検査する。
        let e = resolve_options(false, &[opt("buffer_usage_limit", Some("1"))], &[]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
    }

    #[test]
    fn full_is_refused_and_parallel_with_full_has_its_own_message() {
        for o in [vec![opt("full", None)], vec![opt("full", Some("true"))]] {
            let e = err(&o);
            assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
            assert_eq!(e.message, "VACUUM FULL is not supported yet");
        }
        // 偽なら受け付ける。
        vac(&[opt("full", Some("off"))]).unwrap();
        vac(&[opt("full", Some("false")), opt("parallel", Some("2"))]).unwrap();
        let e = err(&[opt("full", None), opt("parallel", Some("2"))]);
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "VACUUM FULL cannot be performed in parallel");
        vac(&[opt("full", None), opt("parallel", Some("0"))]).unwrap_err();
        // オプションの検査が FULL の拒否より先。
        let e = err(&[opt("full", None), opt("verbose", Some("x"))]);
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
    }

    #[test]
    fn column_list_needs_analyze() {
        let t = [target(&["a"])];
        let e = resolve_options(true, &[], &t).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "ANALYZE option must be specified when a column list is provided"
        );
        resolve_options(true, &[opt("analyze", None)], &t).unwrap();
        resolve_options(true, &[opt("analyze", Some("off"))], &t).unwrap_err();
        resolve_options(true, &[], &[target(&[])]).unwrap();
        // ANALYZE 文は列リストを持てる。
        resolve_options(false, &[], &t).unwrap();
    }

    #[test]
    fn only_database_stats_rules() {
        let only = opt("only_database_stats", None);
        let o = vac(std::slice::from_ref(&only)).unwrap();
        assert!(o.only_database_stats);
        vac(&[only.clone(), opt("verbose", None)]).unwrap();
        vac(&[only.clone(), opt("process_main", Some("false"))]).unwrap();
        let e = resolve_options(true, std::slice::from_ref(&only), &[target(&[])]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "ONLY_DATABASE_STATS cannot be specified with a list of tables"
        );
        for other in [
            opt("freeze", None),
            opt("analyze", None),
            opt("skip_locked", None),
            opt("skip_database_stats", None),
            opt("disable_page_skipping", None),
        ] {
            let e = vac(&[only.clone(), other.clone()]).unwrap_err();
            assert_eq!(
                e.sqlstate,
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{}",
                other.name
            );
            assert_eq!(
                e.message,
                "ONLY_DATABASE_STATS cannot be specified with other VACUUM options"
            );
        }
        // FULL は「他のオプション」として先に拒否される（PG と同じ文言）。
        let e = vac(&[only, opt("full", None)]).unwrap_err();
        assert_eq!(
            e.message,
            "ONLY_DATABASE_STATS cannot be specified with other VACUUM options"
        );
        // 偽なら併用できる。
        vac(&[opt("only_database_stats", Some("off")), opt("freeze", None)]).unwrap();
    }
}
