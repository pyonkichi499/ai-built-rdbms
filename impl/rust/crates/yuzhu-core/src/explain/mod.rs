//! EXPLAIN（`m4/10-explain-copy-compat.md` §3）。
//!
//! - `resolve_options`: `EXPLAIN (...)` の option 列を検査して `ExplainOptions` にする（アナライザが呼ぶ）。
//! - `run`: 表示用の木（`ExplainNode`。`planner::explain_tree` が作る）を `QUERY PLAN` の行にする。
//! - `format`: 行の組み立て（字下げ・コスト欄・ANALYZE の 3 形）。`node`: タイトルなどの部品。

pub mod format;
pub mod node;

pub use format::Timings;

use crate::analyzer::query::ExplainOptions;
use crate::error::{Error, Result, Span, sqlstate};
use crate::executor::instrument::Instrumentation;
use crate::planner::physical::PhysicalQuery;
use crate::sql::ast::{ExplainOption, ExplainValue};

/// `QUERY PLAN` の行を作る。`plan` は `want_explain = true` で作った物理プラン、`span` は EXPLAIN 文の位置。
///
/// `options.analyze` のとき、`instr` は実行を終えた計測値（`Instrumentation::finish` は描画が呼ぶ）。
/// `times.execution` は `ANALYZE` のときだけ要る。
pub fn run(
    options: &ExplainOptions,
    plan: &PhysicalQuery,
    instr: Option<&Instrumentation>,
    times: &Timings,
    span: Span,
) -> Result<Vec<String>> {
    let root = plan.explain.as_ref().ok_or_else(|| {
        Error::internal("EXPLAIN plan was built without an explain tree").with_span(span)
    })?;
    Ok(format::render_plan(root, options, instr, times))
}

/// option 列を解釈して `ExplainOptions` にする（`m4/10` §3.1。PostgreSQL の `ExplainQuery`）。
///
/// `buffers` `wal` `settings` `memory` `serialize` `generic_plan` は値を検査するだけで、出力には影響させない
/// （D10-7）。
pub fn resolve_options(options: &[ExplainOption]) -> Result<ExplainOptions> {
    let mut analyze = false;
    let mut verbose = false;
    let mut costs = true;
    let mut wal = false;
    let mut generic_plan = false;
    let mut serialize = false;
    let mut timing: Option<bool> = None;
    let mut summary: Option<bool> = None;

    for opt in options {
        let name = opt.name.to_ascii_lowercase();
        let value = opt.value.as_ref();
        match name.as_str() {
            "analyze" | "analyse" => analyze = bool_value(&name, value)?,
            "verbose" => verbose = bool_value(&name, value)?,
            "costs" => costs = bool_value(&name, value)?,
            "timing" => timing = Some(bool_value(&name, value)?),
            "summary" => summary = Some(bool_value(&name, value)?),
            "wal" => wal = bool_value(&name, value)?,
            "generic_plan" => generic_plan = bool_value(&name, value)?,
            "buffers" | "settings" | "memory" => {
                bool_value(&name, value)?;
            }
            "format" => check_format(opt, value)?,
            "serialize" => serialize = check_serialize(opt, value)?,
            _ => {
                return Err(Error::new(
                    sqlstate::SYNTAX_ERROR,
                    format!("unrecognized EXPLAIN option \"{}\"", opt.name),
                )
                .with_span(opt.name_span));
            }
        }
    }

    let requires_analyze = |what: &str| {
        Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("EXPLAIN option {what} requires ANALYZE"),
        )
    };
    if wal && !analyze {
        return Err(requires_analyze("WAL"));
    }
    let timing = timing.unwrap_or(analyze);
    if timing && !analyze {
        return Err(requires_analyze("TIMING"));
    }
    if serialize && !analyze {
        return Err(requires_analyze("SERIALIZE"));
    }
    if generic_plan && analyze {
        return Err(Error::not_supported(
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
        ));
    }
    Ok(ExplainOptions {
        analyze,
        verbose,
        costs,
        timing,
        summary: summary.unwrap_or(analyze),
    })
}

/// ブール値の option（PostgreSQL の `defGetBoolean`）。値なしは true、語・文字列は `true` / `false` / `on` /
/// `off`（大文字小文字を区別しない）、整数は `0` / `1`。それ以外は `42601`（位置なし）。
fn bool_value(name: &str, value: Option<&ExplainValue>) -> Result<bool> {
    let bad = || {
        Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("{name} requires a Boolean value"),
        )
    };
    match value {
        None | Some(ExplainValue::Integer(1)) => Ok(true),
        Some(ExplainValue::Integer(0)) => Ok(false),
        Some(ExplainValue::Word(w)) => match w.to_ascii_lowercase().as_str() {
            "true" | "on" => Ok(true),
            "false" | "off" => Ok(false),
            _ => Err(bad()),
        },
        Some(ExplainValue::Integer(_) | ExplainValue::Other(_)) => Err(bad()),
    }
}

/// 文字列として読んだ値（`defGetString`）。
fn value_string(value: &ExplainValue) -> String {
    match value {
        ExplainValue::Word(w) | ExplainValue::Other(w) => w.clone(),
        ExplainValue::Integer(i) => i.to_string(),
    }
}

fn unrecognized_value(opt: &ExplainOption, option: &str, value: &str) -> Error {
    Error::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("unrecognized value for EXPLAIN option \"{option}\": \"{value}\""),
    )
    .with_span(opt.name_span)
}

/// `FORMAT`: `text` だけ受け付ける。`json` / `xml` / `yaml` は `0A000`（yuzhu のみ）。
fn check_format(opt: &ExplainOption, value: Option<&ExplainValue>) -> Result<()> {
    let Some(value) = value else {
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            "format requires a parameter",
        ));
    };
    let s = value_string(value);
    match s.to_ascii_lowercase().as_str() {
        "text" => Ok(()),
        "json" | "xml" | "yaml" => Err(Error::not_supported(format!(
            "EXPLAIN option FORMAT with value \"{}\" is not supported yet",
            s.to_ascii_lowercase()
        ))
        .with_span(opt.name_span)),
        _ => Err(unrecognized_value(opt, "format", &s)),
    }
}

/// `SERIALIZE`: `none` / `text` / `binary`。戻り値は「`none` 以外か」。値なしは `text`。
fn check_serialize(opt: &ExplainOption, value: Option<&ExplainValue>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(true);
    };
    let s = value_string(value);
    match s.to_ascii_lowercase().as_str() {
        "none" => Ok(false),
        "text" | "binary" => Ok(true),
        _ => Err(unrecognized_value(opt, "serialize", &s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::physical::{ExplainNode, PhysicalQuery};

    fn opt(name: &str, value: Option<ExplainValue>) -> ExplainOption {
        ExplainOption {
            name: name.to_owned(),
            value,
            name_span: Span { start: 10, end: 14 },
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn word(w: &str) -> Option<ExplainValue> {
        Some(ExplainValue::Word(w.to_owned()))
    }

    #[allow(clippy::needless_pass_by_value)]
    fn resolve(opts: Vec<ExplainOption>) -> Result<ExplainOptions> {
        resolve_options(&opts)
    }

    fn err(opts: Vec<ExplainOption>) -> Error {
        resolve(opts).unwrap_err()
    }

    #[test]
    fn defaults() {
        let o = resolve(vec![]).unwrap();
        assert_eq!(
            o,
            ExplainOptions {
                analyze: false,
                verbose: false,
                costs: true,
                timing: false,
                summary: false,
            }
        );
    }

    #[test]
    fn analyze_enables_timing_and_summary_by_default() {
        let o = resolve(vec![opt("analyze", None)]).unwrap();
        assert!(o.analyze && o.timing && o.summary && o.costs);
        let o = resolve(vec![opt("analyze", None), opt("timing", word("off"))]).unwrap();
        assert!(o.analyze && !o.timing && o.summary);
        let o = resolve(vec![
            opt("analyze", None),
            opt("summary", word("false")),
            opt("costs", word("off")),
            opt("verbose", None),
        ])
        .unwrap();
        assert!(o.analyze && o.timing && !o.summary && !o.costs && o.verbose);
    }

    #[test]
    fn summary_without_analyze() {
        let o = resolve(vec![opt("summary", word("on"))]).unwrap();
        assert!(o.summary && !o.analyze && !o.timing);
        // TIMING OFF は ANALYZE なしでも可。
        assert!(resolve(vec![opt("timing", word("off"))]).is_ok());
    }

    #[test]
    fn boolean_spellings() {
        for (v, expect) in [
            (word("true"), true),
            (word("TRUE"), true),
            (word("On"), true),
            (word("false"), false),
            (word("OFF"), false),
            (Some(ExplainValue::Integer(1)), true),
            (Some(ExplainValue::Integer(0)), false),
            (None, true),
        ] {
            let o = resolve(vec![opt("verbose", v.clone())]).unwrap();
            assert_eq!(o.verbose, expect, "{v:?}");
        }
    }

    #[test]
    fn invalid_boolean_values() {
        for v in [
            word("foo"),
            word("t"),
            word("y"),
            word("maybe"),
            Some(ExplainValue::Integer(2)),
            Some(ExplainValue::Integer(-1)),
            Some(ExplainValue::Other("1.5".into())),
        ] {
            let e = err(vec![opt("analyze", v.clone())]);
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR, "{v:?}");
            assert_eq!(e.message, "analyze requires a Boolean value");
            assert_eq!(e.cursor_byte, None);
        }
    }

    #[test]
    fn unknown_option_has_name_position() {
        let e = err(vec![opt("foo", None)]);
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "unrecognized EXPLAIN option \"foo\"");
        assert_eq!(e.cursor_byte, Some(10));
    }

    #[test]
    fn last_value_wins() {
        let o = resolve(vec![opt("costs", None), opt("costs", word("off"))]).unwrap();
        assert!(!o.costs);
        let o = resolve(vec![opt("costs", word("off")), opt("costs", None)]).unwrap();
        assert!(o.costs);
    }

    #[test]
    fn names_are_case_folded() {
        let o = resolve(vec![opt("ANALYZE", None), opt("Verbose", None)]).unwrap();
        assert!(o.analyze && o.verbose);
        assert!(resolve(vec![opt("analyse", None)]).unwrap().analyze);
    }

    #[test]
    fn format_values() {
        assert!(resolve(vec![opt("format", word("text"))]).is_ok());
        assert!(resolve(vec![opt("format", word("TEXT"))]).is_ok());
        for w in ["json", "XML", "Yaml"] {
            let e = err(vec![opt("format", word(w))]);
            assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
            assert_eq!(
                e.message,
                format!(
                    "EXPLAIN option FORMAT with value \"{}\" is not supported yet",
                    w.to_ascii_lowercase()
                )
            );
            assert_eq!(e.cursor_byte, Some(10));
        }
        let e = err(vec![opt("format", word("foo"))]);
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "unrecognized value for EXPLAIN option \"format\": \"foo\""
        );
        assert_eq!(e.cursor_byte, Some(10));
        let e = err(vec![opt("format", Some(ExplainValue::Integer(0)))]);
        assert_eq!(
            e.message,
            "unrecognized value for EXPLAIN option \"format\": \"0\""
        );
        assert_eq!(
            err(vec![opt("format", None)]).message,
            "format requires a parameter"
        );
    }

    #[test]
    fn serialize_values_and_requires_analyze() {
        let a = || opt("analyze", None);
        assert!(resolve(vec![a(), opt("serialize", None)]).is_ok());
        assert!(resolve(vec![a(), opt("serialize", word("binary"))]).is_ok());
        assert!(resolve(vec![opt("serialize", word("none"))]).is_ok());
        let e = err(vec![a(), opt("serialize", word("foo"))]);
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "unrecognized value for EXPLAIN option \"serialize\": \"foo\""
        );
        assert_eq!(e.cursor_byte, Some(10));
        let e = err(vec![opt("serialize", word("text"))]);
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "EXPLAIN option SERIALIZE requires ANALYZE");
    }

    #[test]
    fn requires_analyze_errors() {
        let e = err(vec![opt("timing", word("on"))]);
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "EXPLAIN option TIMING requires ANALYZE");
        assert_eq!(e.cursor_byte, None);
        let e = err(vec![opt("timing", None)]);
        assert_eq!(e.message, "EXPLAIN option TIMING requires ANALYZE");
        let e = err(vec![opt("wal", None)]);
        assert_eq!(e.message, "EXPLAIN option WAL requires ANALYZE");
        // ANALYZE があれば通る。後ろに書いても同じ。
        assert!(resolve(vec![opt("wal", None), opt("analyze", None)]).is_ok());
        assert!(resolve(vec![opt("wal", word("off"))]).is_ok());
    }

    #[test]
    fn generic_plan_conflicts_with_analyze() {
        let e = err(vec![opt("analyze", None), opt("generic_plan", None)]);
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together"
        );
        assert_eq!(e.cursor_byte, None);
        // ANALYZE なしなら受け付けて無視する。
        let o = resolve(vec![opt("generic_plan", None)]).unwrap();
        assert!(!o.analyze);
    }

    #[test]
    fn ignored_options_are_still_validated() {
        for name in ["buffers", "settings", "memory"] {
            assert!(resolve(vec![opt(name, word("on"))]).is_ok(), "{name}");
            let e = err(vec![opt(name, word("zzz"))]);
            assert_eq!(e.message, format!("{name} requires a Boolean value"));
        }
    }

    #[test]
    fn first_error_wins() {
        let e = err(vec![opt("foo", None), opt("analyze", word("bad"))]);
        assert_eq!(e.message, "unrecognized EXPLAIN option \"foo\"");
        let e = err(vec![opt("analyze", word("bad")), opt("foo", None)]);
        assert_eq!(e.message, "analyze requires a Boolean value");
    }

    fn explain_query() -> PhysicalQuery {
        let mut q = PhysicalQuery::empty();
        q.explain = Some(ExplainNode {
            title: "Result".to_owned(),
            details: vec![],
            output: vec![],
            children: vec![],
            exec_id: 0,
            width: 4,
        });
        q
    }

    #[test]
    fn run_renders_the_explain_tree() {
        let o = ExplainOptions {
            analyze: false,
            verbose: false,
            costs: true,
            timing: false,
            summary: false,
        };
        let rows = run(
            &o,
            &explain_query(),
            None,
            &Timings::default(),
            Span { start: 0, end: 7 },
        )
        .unwrap();
        assert_eq!(rows, vec!["Result  (cost=0.00..0.00 rows=0 width=4)"]);
    }

    #[test]
    fn run_without_explain_tree_is_internal_error_with_position() {
        let q = PhysicalQuery::empty();
        let e = run(
            &ExplainOptions::default(),
            &q,
            None,
            &Timings::default(),
            Span { start: 3, end: 9 },
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert_eq!(e.cursor_byte, Some(3));
    }
}
