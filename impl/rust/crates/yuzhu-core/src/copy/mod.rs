//! `COPY ... FROM STDIN`（テキスト形式。`m4/10-explain-copy-compat.md` §5）。
//!
//! - [`analyze_copy`]: AST の `Copy` を検査して [`BoundCopy`] にする（対象表・列・option。§5.1）。
//!   `analyzer::analyze` の `Statement::Copy` の分岐から呼ぶ。
//! - [`begin`]: `BoundCopy` から [`CopyIn`] を作る。session は `CopyData` ごとに `CopyIn::push_data`、
//!   `CopyDone` で `CopyIn::finish`、`CopyFail` で `CopyIn::fail` を呼ぶ（§5.3）。

pub mod exec;
pub mod text;

use std::sync::Arc;

pub use exec::CopyIn;

use crate::analyzer::{analyze_column_default_in_table, analyze_table_checks_bound};
use crate::catalog::{CatalogReader, RelKind, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::lower_single_rel;
use crate::planner::physical::{PhysCheck, PhysExpr};
use crate::sql::ast::{Copy, CopyDirection, CopyOption, CopyOptionValue, CopySource, ObjectName};
use crate::types::SqlType;

/// COPY の option（§5.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOptions {
    pub delimiter: u8,
    /// 既定は `\N`。
    pub null_string: Vec<u8>,
    pub default_string: Option<Vec<u8>>,
    /// true なら最初の 1 行を捨てる。
    pub header: bool,
    pub freeze: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            delimiter: b'\t',
            null_string: b"\\N".to_vec(),
            default_string: None,
            header: false,
            freeze: false,
        }
    }
}

/// 解析済みの `COPY ... FROM STDIN`（§5.1）。
#[derive(Debug, Clone)]
pub struct BoundCopy {
    pub table: Arc<TableDef>,
    /// 入力の i 番目のフィールドが入る列（`attnum - 1`）。列リストなしは `0..ncols`。
    pub columns: Vec<usize>,
    /// 全列の型（`attnum` 順）。
    pub col_types: Vec<SqlType>,
    /// 全列の DEFAULT（なければ `None`）。`lower_single_rel` 済み。
    pub defaults: Vec<Option<PhysExpr>>,
    pub checks: Vec<PhysCheck>,
    pub not_null: Vec<bool>,
    pub options: CopyOptions,
}

/// COPY IN を始める（`RelHandle` の組み立てなど）。
pub fn begin(copy: &BoundCopy) -> Result<CopyIn> {
    CopyIn::new(copy)
}

fn not_supported_at(msg: impl Into<String>, span: Span) -> Error {
    Error::not_supported(msg).with_span(span)
}

fn invalid_at(msg: impl Into<String>, span: Span) -> Error {
    Error::new(sqlstate::INVALID_PARAMETER_VALUE, msg).with_span(span)
}

/// `COPY` の対象と列と option を検査する。エラーの順序は PostgreSQL の `DoCopy` に合わせる:
/// COPY の種類 → option → 対象表 → 列リスト → `WHERE`。
pub fn analyze_copy(catalog: &dyn CatalogReader, stmt: &Copy) -> Result<BoundCopy> {
    if stmt.direction == CopyDirection::To {
        return Err(not_supported_at("COPY TO is not supported yet", stmt.span));
    }
    match &stmt.source {
        CopySource::Stdin => {}
        CopySource::File(_) => {
            return Err(not_supported_at(
                "COPY from a file is not supported",
                stmt.span,
            ));
        }
        CopySource::Program(_) => {
            return Err(not_supported_at(
                "COPY from a program is not supported",
                stmt.span,
            ));
        }
    }
    let options = analyze_options(&stmt.options)?;
    let table = resolve_copy_table(catalog, &stmt.table)?;
    let columns = copy_columns(&table, stmt)?;
    if stmt.where_clause.is_some() {
        return Err(Error::not_supported(
            "COPY FROM ... WHERE is not supported yet",
        ));
    }

    let defaults = table
        .columns
        .iter()
        .map(|c| {
            analyze_column_default_in_table(catalog, &table, c)?
                .map(|e| lower_single_rel(&e))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let checks = analyze_table_checks_bound(catalog, &table)?
        .into_iter()
        .map(|c| {
            Ok(PhysCheck {
                name: c.name,
                expr: lower_single_rel(&c.expr)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(BoundCopy {
        col_types: table.columns.iter().map(|c| c.ty).collect(),
        not_null: table.columns.iter().map(|c| c.not_null).collect(),
        table,
        columns,
        defaults,
        checks,
        options,
    })
}

fn display_name(name: &ObjectName) -> String {
    name.parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// 位置なしのエラー（PostgreSQL の `COPY` は対象表の位置を報告しない。R-22）。
fn resolve_copy_table(catalog: &dyn CatalogReader, name: &ObjectName) -> Result<Arc<TableDef>> {
    if name.parts.len() > 3 {
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            format!(
                "improper relation name (too many dotted names): {}",
                display_name(name)
            ),
        ));
    }
    if name.parts.len() == 3 && name.parts[0].value != catalog.current_database() {
        return Err(Error::not_supported(format!(
            "cross-database references are not implemented: {}",
            display_name(name)
        )));
    }
    let schema = name.schema().map(|s| s.value.as_str());
    let table = catalog.table(schema, &name.name().value)?.ok_or_else(|| {
        Error::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{}\" does not exist", display_name(name)),
        )
    })?;
    match table.kind {
        RelKind::Table => {}
        RelKind::Sequence | RelKind::Index => {
            let what = if table.kind == RelKind::Sequence {
                "sequence"
            } else {
                "index"
            };
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot copy to {what} \"{}\"", table.name),
            ));
        }
    }
    if table.is_system_catalog() {
        return Err(Error::new(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for table {}", table.name),
        ));
    }
    Ok(table)
}

fn copy_columns(table: &TableDef, stmt: &Copy) -> Result<Vec<usize>> {
    if stmt.columns.is_empty() {
        return Ok((0..table.columns.len()).collect());
    }
    let mut out: Vec<usize> = Vec::with_capacity(stmt.columns.len());
    for c in &stmt.columns {
        let Some(idx) = table.column_index(&c.value) else {
            return Err(Error::new(
                sqlstate::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    c.value, table.name
                ),
            ));
        };
        if out.contains(&idx) {
            return Err(Error::new(
                sqlstate::DUPLICATE_COLUMN,
                format!("column \"{}\" specified more than once", c.value),
            ));
        }
        out.push(idx);
    }
    Ok(out)
}

// ----- option -----------------------------------------------------------------

/// PostgreSQL の `parse_bool`（前方一致を許す）。
fn parse_bool(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    let is_prefix = |full: &str, min: usize| l.len() >= min && full.starts_with(&l);
    if is_prefix("true", 1) || is_prefix("yes", 1) || is_prefix("on", 2) || l == "1" {
        Some(true)
    } else if is_prefix("false", 1) || is_prefix("no", 1) || is_prefix("off", 2) || l == "0" {
        Some(false)
    } else {
        None
    }
}

/// `defGetBoolean`。値なしは true。
fn option_bool(o: &CopyOption) -> Result<bool> {
    let bad = || {
        Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("{} requires a Boolean value", o.name),
        )
        .with_span(o.name_span)
    };
    match &o.value {
        None | Some(CopyOptionValue::Integer(1)) => Ok(true),
        Some(CopyOptionValue::Word(s) | CopyOptionValue::String(s)) => {
            parse_bool(s).ok_or_else(bad)
        }
        Some(CopyOptionValue::Integer(0)) => Ok(false),
        Some(_) => Err(bad()),
    }
}

/// `defGetString`。
fn option_string(o: &CopyOption) -> Result<String> {
    match &o.value {
        Some(CopyOptionValue::Word(s) | CopyOptionValue::String(s)) => Ok(s.clone()),
        Some(CopyOptionValue::Integer(n)) => Ok(n.to_string()),
        Some(CopyOptionValue::Star) => Ok("*".to_owned()),
        Some(CopyOptionValue::List(_)) | None => Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("{} requires a parameter", o.name),
        )
        .with_span(o.name_span)),
    }
}

const ENCODINGS: &[&str] = &[
    "sqlascii",
    "latin1",
    "latin2",
    "latin3",
    "latin4",
    "latin5",
    "latin6",
    "latin7",
    "latin8",
    "latin9",
    "latin10",
    "win1250",
    "win1251",
    "win1252",
    "win1253",
    "win1254",
    "win1255",
    "win1256",
    "win1257",
    "win1258",
    "win866",
    "win874",
    "koi8r",
    "koi8u",
    "euckr",
    "euccn",
    "eucjp",
    "euctw",
    "sjis",
    "big5",
    "gbk",
    "gb18030",
    "uhc",
    "johab",
    "iso88595",
    "iso88596",
    "iso88597",
    "iso88598",
    "muleinternal",
    "eucjis2004",
    "shiftjis2004",
];

/// `encoding` option の検査。`UTF8` だけを受け付ける。
fn check_encoding(o: &CopyOption) -> Result<()> {
    let name = option_string(o)?;
    let norm: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    if norm == "utf8" || norm == "unicode" {
        Ok(())
    } else if ENCODINGS.contains(&norm.as_str()) {
        Err(not_supported_at(
            format!("COPY encoding \"{name}\" is not supported yet"),
            o.name_span,
        ))
    } else {
        Err(invalid_at(
            "argument to option \"encoding\" must be a valid encoding name",
            o.name_span,
        ))
    }
}

#[allow(clippy::too_many_lines)]
fn analyze_options(opts: &[CopyOption]) -> Result<CopyOptions> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = CopyOptions::default();
    let mut delimiter: Option<(String, Span)> = None;
    let mut null: Option<(Vec<u8>, Span)> = None;
    let mut default: Option<(Vec<u8>, Span)> = None;
    let mut csv_only: Vec<(&'static str, usize)> = Vec::new();
    for (pos, o) in opts.iter().enumerate() {
        const KNOWN: &[&str] = &[
            "format",
            "freeze",
            "delimiter",
            "null",
            "default",
            "header",
            "encoding",
            "log_verbosity",
            "on_error",
            "quote",
            "escape",
            "force_quote",
            "force_not_null",
            "force_null",
        ];
        let name = o.name.as_str();
        if !KNOWN.contains(&name) {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                format!("option \"{name}\" not recognized"),
            )
            .with_span(o.name_span));
        }
        if seen.contains(&name) {
            return Err(
                Error::new(sqlstate::SYNTAX_ERROR, "conflicting or redundant options")
                    .with_span(o.name_span),
            );
        }
        seen.push(name);
        match name {
            "format" => {
                let f = option_string(o)?;
                match f.as_str() {
                    "text" => {}
                    "csv" | "binary" => {
                        return Err(not_supported_at(
                            format!("COPY format \"{f}\" is not supported yet"),
                            o.name_span,
                        ));
                    }
                    _ => {
                        return Err(invalid_at(
                            format!("COPY format \"{f}\" not recognized"),
                            o.name_span,
                        ));
                    }
                }
            }
            "freeze" => out.freeze = option_bool(o)?,
            "delimiter" => delimiter = Some((option_string(o)?, o.name_span)),
            "null" => null = Some((option_string(o)?.into_bytes(), o.name_span)),
            "default" => default = Some((option_string(o)?.into_bytes(), o.name_span)),
            "header" => match &o.value {
                Some(CopyOptionValue::Word(s) | CopyOptionValue::String(s))
                    if s.eq_ignore_ascii_case("match") =>
                {
                    return Err(not_supported_at(
                        "COPY HEADER MATCH is not supported yet",
                        o.name_span,
                    ));
                }
                _ => {
                    out.header = option_bool(o).map_err(|e| {
                        Error::new(e.sqlstate, "header requires a Boolean value or \"match\"")
                            .with_span(o.name_span)
                    })?;
                }
            },
            "encoding" => check_encoding(o)?,
            "log_verbosity" => {
                let v = option_string(o)?;
                if !matches!(v.as_str(), "default" | "verbose" | "terse") {
                    return Err(invalid_at(
                        format!("COPY LOG_VERBOSITY \"{v}\" not recognized"),
                        o.name_span,
                    ));
                }
            }
            "on_error" => {
                let v = option_string(o)?;
                match v.as_str() {
                    "stop" => {}
                    "ignore" => {
                        return Err(not_supported_at(
                            "COPY ON_ERROR \"ignore\" is not supported yet",
                            o.name_span,
                        ));
                    }
                    _ => {
                        return Err(invalid_at(
                            format!("COPY ON_ERROR \"{v}\" not recognized"),
                            o.name_span,
                        ));
                    }
                }
            }
            "quote" => csv_only.push(("QUOTE", pos)),
            "escape" => csv_only.push(("ESCAPE", pos)),
            "force_quote" => csv_only.push(("FORCE_QUOTE", pos)),
            "force_not_null" => csv_only.push(("FORCE_NOT_NULL", pos)),
            "force_null" => csv_only.push(("FORCE_NULL", pos)),
            _ => unreachable!("checked against KNOWN"),
        }
    }
    finish_options(
        &mut out,
        delimiter.as_ref(),
        null.as_ref(),
        default.as_ref(),
        &csv_only,
    )?;
    Ok(out)
}

/// option の組み合わせの検査（PostgreSQL の `ProcessCopyOptions` の後半）。
fn finish_options(
    out: &mut CopyOptions,
    delimiter: Option<&(String, Span)>,
    null: Option<&(Vec<u8>, Span)>,
    default: Option<&(Vec<u8>, Span)>,
    csv_only: &[(&'static str, usize)],
) -> Result<()> {
    if let Some((d, _)) = &delimiter {
        let &[b] = d.as_bytes() else {
            return Err(Error::not_supported(
                "COPY delimiter must be a single one-byte character",
            ));
        };
        if b == b'\n' || b == b'\r' {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "COPY delimiter cannot be newline or carriage return",
            ));
        }
        out.delimiter = b;
    }
    if let Some((n, _)) = &null {
        if n.iter().any(|b| *b == b'\n' || *b == b'\r') {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "COPY null representation cannot use newline or carriage return",
            ));
        }
        out.null_string.clone_from(n);
    }
    if let Some((d, _)) = &default {
        if d.iter().any(|b| *b == b'\n' || *b == b'\r') {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "COPY default representation cannot use newline or carriage return",
            ));
        }
        out.default_string = Some(d.clone());
    }
    let bad_delim = b"\\.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    if delimiter.is_some() && bad_delim.contains(&out.delimiter) {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("COPY delimiter cannot be \"{}\"", char::from(out.delimiter)),
        ));
    }
    if let Some(&(name, _)) = csv_only.first() {
        return Err(Error::not_supported(format!(
            "COPY {name} requires CSV mode"
        )));
    }
    if out.null_string.contains(&out.delimiter) {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY delimiter character must not appear in the NULL specification",
        ));
    }
    if let Some(d) = &out.default_string {
        if *d == out.null_string {
            return Err(Error::not_supported(
                "NULL specification and DEFAULT specification cannot be the same",
            ));
        }
        if d.contains(&out.delimiter) {
            return Err(Error::not_supported(
                "COPY delimiter character must not appear in the DEFAULT specification",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, table_def};
    use crate::catalog::{CheckDef, ColumnDef};
    use crate::sql::ast::Statement;

    const T: u32 = 16384;

    fn catalog() -> FakeCatalog {
        let c = |name: &str, attnum, ty, not_null| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: None,
            identity: None,
        };
        let mut cat = FakeCatalog::new("postgres");
        cat.put_table(Arc::new(table_def(
            T,
            "cp",
            vec![
                c("a", 1, SqlType::INT4, true),
                c("b", 2, SqlType::TEXT, false),
                c("c", 3, SqlType::INT4, false),
            ],
            vec![CheckDef {
                name: "cp_c_check".into(),
                expr_sql: "c < 100".into(),
                no_inherit: false,
            }],
        )));
        cat
    }

    fn analyze(sql: &str) -> Result<BoundCopy> {
        let stmts = crate::sql::parse(sql).unwrap();
        let Statement::Copy(c) = &stmts[0] else {
            panic!("not a COPY: {sql}");
        };
        analyze_copy(&catalog(), c)
    }

    fn err(sql: &str) -> Error {
        analyze(sql).unwrap_err()
    }

    #[test]
    fn analyzes_a_plain_copy() {
        let b = analyze("COPY cp FROM STDIN").unwrap();
        assert_eq!(b.columns, vec![0, 1, 2]);
        assert_eq!(b.not_null, vec![true, false, false]);
        assert_eq!(b.checks.len(), 1);
        assert_eq!(b.checks[0].name, "cp_c_check");
        assert_eq!(b.options, CopyOptions::default());
        let b = analyze("COPY public.cp (c, a) FROM STDIN WITH (freeze, header 'on')").unwrap();
        assert_eq!(b.columns, vec![2, 0]);
        assert!(b.options.freeze && b.options.header);
    }

    #[test]
    fn begin_builds_a_copy_in() {
        let b = analyze("COPY cp (b, a) FROM STDIN").unwrap();
        let c = begin(&b).unwrap();
        assert_eq!(c.ncols(), 2);
        assert_eq!(c.rows(), 0);
    }

    #[test]
    fn target_and_column_errors() {
        let e = err("COPY nosuch FROM STDIN");
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
        assert_eq!(e.message, "relation \"nosuch\" does not exist");
        assert!(e.cursor_byte.is_none() && e.position.is_none());
        let e = err("COPY public.nosuch (a) FROM STDIN");
        assert_eq!(e.message, "relation \"public.nosuch\" does not exist");
        let e = err("COPY cp (zz) FROM STDIN");
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(e.message, "column \"zz\" of relation \"cp\" does not exist");
        let e = err("COPY cp (a, a) FROM STDIN");
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_COLUMN);
        assert_eq!(e.message, "column \"a\" specified more than once");
    }

    #[test]
    fn unsupported_forms() {
        let e = err("COPY cp TO STDOUT");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY TO is not supported yet");
        assert_eq!(
            err("COPY cp FROM '/tmp/x'").message,
            "COPY from a file is not supported"
        );
        assert_eq!(
            err("COPY cp FROM PROGRAM 'true'").message,
            "COPY from a program is not supported"
        );
        assert_eq!(
            err("COPY cp FROM STDIN WHERE a > 1").message,
            "COPY FROM ... WHERE is not supported yet"
        );
        let e = err("COPY cp FROM STDIN (format csv)");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY format \"csv\" is not supported yet");
    }

    #[test]
    fn option_errors() {
        let e = err("COPY cp FROM STDIN (foo)");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "option \"foo\" not recognized");
        let e = err("COPY cp FROM STDIN (freeze on, freeze off)");
        assert_eq!(e.message, "conflicting or redundant options");
        let e = err("COPY cp FROM STDIN (format foo)");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY format \"foo\" not recognized");
        let e = err("COPY cp FROM STDIN (header 'x')");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "header requires a Boolean value or \"match\"");
        let e = err("COPY cp FROM STDIN (header match)");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("COPY cp FROM STDIN (freeze 'maybe')");
        assert_eq!(e.message, "freeze requires a Boolean value");
        let e = err("COPY cp FROM STDIN (on_error ignore)");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("COPY cp FROM STDIN (on_error foo)");
        assert_eq!(e.message, "COPY ON_ERROR \"foo\" not recognized");
        let e = err("COPY cp FROM STDIN (log_verbosity foo)");
        assert_eq!(e.message, "COPY LOG_VERBOSITY \"foo\" not recognized");
        assert!(analyze("COPY cp FROM STDIN (log_verbosity verbose)").is_ok());
        let e = err("COPY cp FROM STDIN (quote 'x')");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "COPY QUOTE requires CSV mode");
        let e = err("COPY cp FROM STDIN (force_not_null (a))");
        assert_eq!(e.message, "COPY FORCE_NOT_NULL requires CSV mode");
    }

    #[test]
    fn encoding_option() {
        assert!(analyze("COPY cp FROM STDIN (encoding 'UTF-8')").is_ok());
        assert!(analyze("COPY cp FROM STDIN (encoding 'utf8')").is_ok());
        let e = err("COPY cp FROM STDIN (encoding 'latin1')");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("COPY cp FROM STDIN (encoding 'nosuch')");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "argument to option \"encoding\" must be a valid encoding name"
        );
    }

    #[test]
    fn delimiter_null_default() {
        let b = analyze("COPY cp FROM STDIN (delimiter '|', null 'NA', default 'D')").unwrap();
        assert_eq!(b.options.delimiter, b'|');
        assert_eq!(b.options.null_string, b"NA");
        assert_eq!(b.options.default_string.as_deref(), Some(&b"D"[..]));
        // 旧構文
        let b = analyze("COPY cp FROM STDIN WITH DELIMITER AS ',' NULL AS ''").unwrap();
        assert_eq!(b.options.delimiter, b',');
        assert_eq!(b.options.null_string, b"");

        let e = err("COPY cp FROM STDIN (delimiter 'ab')");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "COPY delimiter must be a single one-byte character"
        );
        let e = err("COPY cp FROM STDIN (delimiter '')");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("COPY cp FROM STDIN (delimiter E'\\n')");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "COPY delimiter cannot be newline or carriage return"
        );
        for d in [".", "a", "Z", "5"] {
            let e = err(&format!("COPY cp FROM STDIN (delimiter '{d}')"));
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE, "{d}");
            assert!(e.message.starts_with("COPY delimiter cannot be \""), "{d}");
        }
        let e = err("COPY cp FROM STDIN (delimiter E'\\\\')");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "COPY delimiter cannot be \"\\\"");
        let e = err("COPY cp FROM STDIN (null E'a\\nb')");
        assert_eq!(
            e.message,
            "COPY null representation cannot use newline or carriage return"
        );
        let e = err("COPY cp FROM STDIN (delimiter '|', null 'a|b')");
        assert_eq!(
            e.message,
            "COPY delimiter character must not appear in the NULL specification"
        );
        let e = err("COPY cp FROM STDIN (null 'x', default 'x')");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "NULL specification and DEFAULT specification cannot be the same"
        );
        let e = err("COPY cp FROM STDIN (delimiter '|', default 'a|b')");
        assert_eq!(
            e.message,
            "COPY delimiter character must not appear in the DEFAULT specification"
        );
    }

    #[test]
    fn parse_bool_follows_postgres() {
        for s in ["t", "TRUE", "y", "yes", "on", "1"] {
            assert_eq!(parse_bool(s), Some(true), "{s}");
        }
        for s in ["f", "False", "n", "no", "off", "of", "0"] {
            assert_eq!(parse_bool(s), Some(false), "{s}");
        }
        for s in ["", "o", "maybe", "2", "truee"] {
            assert_eq!(parse_bool(s), None, "{s}");
        }
    }
}
