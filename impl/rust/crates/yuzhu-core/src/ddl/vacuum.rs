//! `VACUUM` / `ANALYZE`（何もせず成功する。`m4/07-catalog-ddl.md` §5.7）。
//!
//! ライターロックも XID も取らず、カタログにも WAL にも触らない。

use super::{DdlCtx, parse_bool};
use crate::analyzer::query::{BoundVacuum, VacuumOption};
use crate::error::{Error, Result, Severity, sqlstate};

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

/// `VACUUM` / `ANALYZE` のオプションの検査（意味は持たない）。
pub fn validate_vacuum_options(vacuum: bool, options: &[VacuumOption]) -> Result<()> {
    let (allowed, command) = if vacuum {
        (VACUUM_OPTIONS, "VACUUM")
    } else {
        (ANALYZE_OPTIONS, "ANALYZE")
    };
    let syntax = |m: String| Error::new(sqlstate::SYNTAX_ERROR, m);
    let mut parallel = 0_i64;
    let mut full = false;
    for o in options {
        let name = o.name.as_str();
        if !allowed.contains(&name) {
            return Err(syntax(format!("unrecognized {command} option \"{name}\"")));
        }
        let value = o.value.as_deref();
        match name {
            "index_cleanup" => {
                let ok =
                    value.is_none_or(|v| v.eq_ignore_ascii_case("auto") || parse_bool(v).is_some());
                if !ok {
                    return Err(syntax("index_cleanup requires a Boolean value".to_owned()));
                }
            }
            "parallel" => {
                let Some(v) = value else {
                    return Err(syntax(
                        "parallel option requires a value between 0 and 1024".to_owned(),
                    ));
                };
                let n: i64 = v
                    .trim()
                    .parse()
                    .map_err(|_| syntax("parallel requires an integer value".to_owned()))?;
                if !(0..=1024).contains(&n) {
                    return Err(syntax(
                        "parallel workers for vacuum must be between 0 and 1024".to_owned(),
                    ));
                }
                parallel = n;
            }
            "buffer_usage_limit" => {}
            _ => {
                let on = match value {
                    None => true,
                    Some(v) => parse_bool(v)
                        .ok_or_else(|| syntax(format!("{name} requires a Boolean value")))?,
                };
                if name == "full" {
                    full = on;
                }
            }
        }
    }
    if full && parallel > 0 {
        return Err(Error::not_supported(
            "VACUUM FULL cannot be performed in parallel",
        ));
    }
    Ok(())
}

pub fn vacuum(ctx: &mut DdlCtx<'_>, b: &BoundVacuum) -> Result<String> {
    // 1.
    if b.vacuum && ctx.in_transaction_block {
        return Err(Error::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "VACUUM cannot run inside a transaction block",
        ));
    }
    // 2.
    validate_vacuum_options(b.vacuum, &b.options)?;
    // 3.
    let analyze_option = b.options.iter().any(|o| {
        o.name == "analyze"
            && o.value
                .as_deref()
                .is_none_or(|v| parse_bool(v) == Some(true))
    });
    if b.targets.iter().any(|t| !t.columns.is_empty()) && !(b.analyze || analyze_option) {
        return Err(Error::not_supported(
            "ANALYZE option must be specified when a column list is provided",
        ));
    }
    let verb = if b.vacuum { "vacuum" } else { "analyze" };
    for t in &b.targets {
        if t.table.is_none() {
            ctx.notice(
                Severity::Warning,
                sqlstate::WARNING,
                format!(
                    "skipping \"{}\" --- cannot {verb} non-tables or special system tables",
                    t.name
                ),
                None,
            );
        }
    }
    // 4.
    Ok(if b.vacuum { "VACUUM" } else { "ANALYZE" }.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::{BoundDdl, VacuumTarget};
    use crate::catalog::fake::table_def;
    use crate::ddl::testkit::{Harness, col};
    use crate::types::SqlType;
    use std::sync::Arc;

    fn opt(name: &str, value: Option<&str>) -> VacuumOption {
        VacuumOption {
            name: name.into(),
            value: value.map(str::to_owned),
        }
    }

    fn bound(vacuum: bool, analyze: bool, options: Vec<VacuumOption>) -> BoundVacuum {
        BoundVacuum {
            vacuum,
            analyze,
            options,
            targets: Vec::new(),
        }
    }

    fn run(h: &mut Harness, b: BoundVacuum) -> Result<String> {
        h.exec(BoundDdl::Vacuum(b))
    }

    #[test]
    fn vacuum_and_analyze_return_their_tags_without_a_transaction_id() {
        let mut h = Harness::new();
        assert_eq!(run(&mut h, bound(true, false, vec![])).unwrap(), "VACUUM");
        assert_eq!(run(&mut h, bound(true, true, vec![])).unwrap(), "VACUUM");
        assert_eq!(run(&mut h, bound(false, true, vec![])).unwrap(), "ANALYZE");
        assert!(h.txn.xid.is_none(), "no XID is taken");
        assert!(!h.txn.catalog_dirty);
        assert!(!h.txn.cid_used);
    }

    #[test]
    fn only_vacuum_is_refused_in_a_transaction_block() {
        let mut h = Harness::new();
        h.in_block = true;
        let e = run(&mut h, bound(true, false, vec![])).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ACTIVE_SQL_TRANSACTION);
        assert_eq!(e.message, "VACUUM cannot run inside a transaction block");
        assert_eq!(run(&mut h, bound(false, true, vec![])).unwrap(), "ANALYZE");
    }

    fn option_error(vacuum: bool, o: VacuumOption) -> Error {
        validate_vacuum_options(vacuum, &[o]).unwrap_err()
    }

    #[test]
    fn option_names_depend_on_the_command() {
        for name in VACUUM_OPTIONS {
            let value = match *name {
                "parallel" => Some("2"),
                "buffer_usage_limit" => Some("1MB"),
                _ => None,
            };
            validate_vacuum_options(true, &[opt(name, value)]).unwrap();
        }
        let e = option_error(true, opt("foo", None));
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "unrecognized VACUUM option \"foo\"");
        let e = option_error(false, opt("foo", None));
        assert_eq!(e.message, "unrecognized ANALYZE option \"foo\"");
        // ANALYZE に `analyze` を渡しても同じ。
        let e = option_error(false, opt("analyze", None));
        assert_eq!(e.message, "unrecognized ANALYZE option \"analyze\"");
        validate_vacuum_options(
            false,
            &[opt("verbose", Some("on")), opt("skip_locked", None)],
        )
        .unwrap();
    }

    #[test]
    fn option_values_are_checked() {
        let e = option_error(true, opt("verbose", Some("maybe")));
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "verbose requires a Boolean value");
        for v in ["on", "off", "true", "false", "1", "0"] {
            validate_vacuum_options(true, &[opt("freeze", Some(v))]).unwrap();
        }
        validate_vacuum_options(true, &[opt("index_cleanup", Some("auto"))]).unwrap();
        validate_vacuum_options(true, &[opt("index_cleanup", Some("off"))]).unwrap();
        let e = option_error(true, opt("index_cleanup", Some("zz")));
        assert_eq!(e.message, "index_cleanup requires a Boolean value");
        let e = option_error(true, opt("parallel", Some("1025")));
        assert_eq!(
            e.message,
            "parallel workers for vacuum must be between 0 and 1024"
        );
        let e = option_error(true, opt("parallel", Some("-1")));
        assert_eq!(
            e.message,
            "parallel workers for vacuum must be between 0 and 1024"
        );
        validate_vacuum_options(true, &[opt("parallel", Some("0")), opt("full", None)]).unwrap();
        let e = validate_vacuum_options(true, &[opt("full", None), opt("parallel", Some("2"))])
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "VACUUM FULL cannot be performed in parallel");
    }

    #[test]
    fn non_tables_are_skipped_with_a_warning_and_column_lists_need_analyze() {
        let mut h = Harness::new();
        let table = Arc::new(table_def(
            16400,
            "v1",
            vec![col("a", 1, SqlType::INT4)],
            vec![],
        ));
        let target = |name: &str, table: Option<Arc<_>>, columns: &[&str]| VacuumTarget {
            name: name.into(),
            table,
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        };
        let mut b = bound(true, false, vec![]);
        b.targets = vec![
            target("v1", Some(Arc::clone(&table)), &[]),
            target("v1_a", None, &[]),
        ];
        assert_eq!(run(&mut h, b).unwrap(), "VACUUM");
        assert_eq!(h.notices.len(), 1);
        let n = &h.notices[0];
        assert_eq!(n.severity, Severity::Warning);
        assert_eq!(n.sqlstate, sqlstate::WARNING);
        assert_eq!(
            n.message,
            "skipping \"v1_a\" --- cannot vacuum non-tables or special system tables"
        );
        h.notices.clear();
        let mut b = bound(false, true, vec![]);
        b.targets = vec![target("v1_a", None, &[])];
        run(&mut h, b).unwrap();
        assert_eq!(
            h.notices[0].message,
            "skipping \"v1_a\" --- cannot analyze non-tables or special system tables"
        );
        // 列リストは ANALYZE がなければ 0A000。
        let mut b = bound(true, false, vec![]);
        b.targets = vec![target("v1", Some(Arc::clone(&table)), &["a"])];
        let e = run(&mut h, b).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "ANALYZE option must be specified when a column list is provided"
        );
        let mut b = bound(true, true, vec![]);
        b.targets = vec![target("v1", Some(Arc::clone(&table)), &["a"])];
        assert_eq!(run(&mut h, b).unwrap(), "VACUUM");
        let mut b = bound(true, false, vec![opt("analyze", None)]);
        b.targets = vec![target("v1", Some(table), &["a"])];
        assert_eq!(run(&mut h, b).unwrap(), "VACUUM");
    }
}
