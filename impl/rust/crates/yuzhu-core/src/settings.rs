//! Run-time parameters handled by SET / SHOW / RESET (`m1.md` §5.4).
//!
//! The registry (`SETTINGS`) lists every known parameter with its default,
//! whether changes are reported to the client with `ParameterStatus`, and
//! how values are validated and normalized. `Settings` holds one session's
//! values with PostgreSQL's transactional behaviour:
//!
//! - `SET` (session) survives COMMIT; ROLLBACK restores the value the
//!   parameter had when the transaction started.
//! - `SET LOCAL` lasts until the end of the transaction.
//!
//! Names are case-insensitive. Names containing a `.` are custom
//! ("placeholder") parameters that accept any value.

use std::collections::HashMap;

use crate::error::{Error, Result, sqlstate};

/// The value reported by `version()`.
pub const VERSION_STRING: &str = concat!(
    "PostgreSQL 17.0 (yuzhu ",
    env!("CARGO_PKG_VERSION"),
    ") on x86_64-pc-linux-gnu, compiled by rustc, 64-bit"
);

/// How a parameter's value is validated and normalized.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Shown only; SET gives 55P02.
    ReadOnly,
    /// Changeable only by a reload (`sighup`): SET gives 55P02 "cannot be
    /// changed now".
    Sighup,
    /// Duration in milliseconds with a minimum of 1 ms.
    MillisMin1,
    /// `synchronous_commit`: the listed values plus the Boolean spellings,
    /// which are normalized to `on` / `off`.
    SyncCommit,
    /// Any string.
    Str,
    /// Boolean, normalized to `on` / `off`.
    Bool,
    /// Boolean that must be `on`.
    OnlyOn,
    /// Integer in `min..=max`.
    Int { min: i64, max: i64 },
    /// An amount of memory stored in kB (`'256MB'`, `'1GB'`), shown with the
    /// largest exact unit.
    Mem { min: i64, max: i64 },
    /// Duration in milliseconds (`0`, `1000`, `'5s'`), shown with the
    /// largest exact unit.
    Millis,
    /// One of the listed lower-case values.
    Enum(&'static [&'static str]),
    /// A transaction isolation level. `repeatable read` and `serializable`
    /// are valid names but not supported yet (`0A000`).
    Isolation,
    /// A comma-separated list of identifiers (`search_path`).
    IdentList,
    /// `client_encoding`: UTF8 or LATIN1.
    Encoding,
    /// `DateStyle`.
    DateStyle,
    /// `TimeZone`: a zone name known to the tz database, or an offset.
    TimeZone,
}

/// A registry entry.
#[derive(Debug)]
pub struct SettingDef {
    /// Canonical spelling (`TimeZone`, `DateStyle`, `search_path`).
    pub name: &'static str,
    /// Built-in default (`session_authorization` is filled per session).
    pub default: &'static str,
    /// Changes are sent to the client as `ParameterStatus`.
    pub reported: bool,
    pub description: &'static str,
    kind: Kind,
}

const fn def(
    name: &'static str,
    default: &'static str,
    reported: bool,
    kind: Kind,
    description: &'static str,
) -> SettingDef {
    SettingDef {
        name,
        default,
        reported,
        description,
        kind,
    }
}

const LEVELS: &[&str] = &[
    "debug5", "debug4", "debug3", "debug2", "debug1", "log", "notice", "warning", "error",
];

/// Every known parameter.
pub static SETTINGS: &[SettingDef] = &[
    def(
        "application_name",
        "",
        true,
        Kind::Str,
        "Sets the application name to be reported in statistics and logs.",
    ),
    def(
        "block_size",
        "8192",
        false,
        Kind::ReadOnly,
        "Shows the size of a disk block.",
    ),
    def(
        "checkpoint_timeout",
        "5min",
        false,
        Kind::ReadOnly,
        "Sets the maximum time between automatic WAL checkpoints.",
    ),
    def(
        "data_checksums",
        "on",
        false,
        Kind::ReadOnly,
        "Shows whether data checksums are turned on for this cluster.",
    ),
    def(
        "data_directory",
        "",
        false,
        Kind::ReadOnly,
        "Sets the server's data directory.",
    ),
    def(
        "segment_size",
        "1GB",
        false,
        Kind::ReadOnly,
        "Shows the number of pages per disk file.",
    ),
    def(
        "shared_buffers",
        "128MB",
        false,
        Kind::ReadOnly,
        "Sets the number of shared memory buffers used by the server.",
    ),
    def(
        "bytea_output",
        "hex",
        false,
        Kind::Enum(&["hex", "escape"]),
        "Sets the output format for bytea.",
    ),
    def(
        "client_encoding",
        "UTF8",
        true,
        Kind::Encoding,
        "Sets the client's character set encoding.",
    ),
    def(
        "client_min_messages",
        "notice",
        false,
        Kind::Enum(LEVELS),
        "Sets the message levels that are sent to the client.",
    ),
    def(
        "DateStyle",
        "ISO, MDY",
        true,
        Kind::DateStyle,
        "Sets the display format for date and time values.",
    ),
    def(
        "default_transaction_isolation",
        "read committed",
        false,
        Kind::Isolation,
        "Sets the transaction isolation level of each new transaction.",
    ),
    def(
        "default_transaction_read_only",
        "off",
        true,
        Kind::Bool,
        "Sets the default read-only status of new transactions.",
    ),
    def(
        "extra_float_digits",
        "1",
        false,
        Kind::Int { min: -15, max: 3 },
        "Sets the number of digits displayed for floating-point values.",
    ),
    def(
        "idle_in_transaction_session_timeout",
        "0",
        false,
        Kind::Millis,
        "Sets the maximum allowed idle time between queries, when in a transaction.",
    ),
    def(
        "in_hot_standby",
        "off",
        true,
        Kind::ReadOnly,
        "Shows whether hot standby is currently active.",
    ),
    def(
        "integer_datetimes",
        "on",
        true,
        Kind::ReadOnly,
        "Shows whether datetimes are integer based.",
    ),
    def(
        "IntervalStyle",
        "postgres",
        true,
        Kind::Enum(&["postgres", "postgres_verbose", "sql_standard", "iso_8601"]),
        "Sets the display format for interval values.",
    ),
    def(
        "is_superuser",
        "on",
        true,
        Kind::ReadOnly,
        "Shows whether the current user is a superuser.",
    ),
    def(
        "lc_collate",
        "C",
        false,
        Kind::ReadOnly,
        "Shows the collation order locale.",
    ),
    def(
        "lc_ctype",
        "C",
        false,
        Kind::ReadOnly,
        "Shows the character classification and case conversion locale.",
    ),
    def(
        "lc_messages",
        "C",
        false,
        Kind::Str,
        "Sets the language in which messages are displayed.",
    ),
    def(
        "lock_timeout",
        "0",
        false,
        Kind::Millis,
        "Sets the maximum allowed duration of any wait for a lock.",
    ),
    def(
        "max_connections",
        "100",
        false,
        Kind::ReadOnly,
        "Sets the maximum number of concurrent connections.",
    ),
    def(
        "max_identifier_length",
        "63",
        false,
        Kind::ReadOnly,
        "Shows the maximum identifier length.",
    ),
    def(
        "search_path",
        "\"$user\", public",
        false,
        Kind::IdentList,
        "Sets the schema search order for names that are not schema-qualified.",
    ),
    def(
        "server_encoding",
        "UTF8",
        true,
        Kind::ReadOnly,
        "Shows the server (database) character set encoding.",
    ),
    def(
        "role",
        "none",
        false,
        Kind::ReadOnly,
        "Sets the current role.",
    ),
    def(
        "server_version",
        "17.0",
        true,
        Kind::ReadOnly,
        "Shows the server version.",
    ),
    def(
        "server_version_num",
        "170000",
        false,
        Kind::ReadOnly,
        "Shows the server version as an integer.",
    ),
    def(
        "session_authorization",
        "",
        true,
        Kind::ReadOnly,
        "Sets the session user name.",
    ),
    def(
        "standard_conforming_strings",
        "on",
        true,
        Kind::OnlyOn,
        "Causes '...' strings to treat backslashes literally.",
    ),
    def(
        "yuzhu.query_mem_limit",
        "256MB",
        false,
        Kind::Mem {
            min: 64,
            max: 2_147_483_647,
        },
        "Sets the memory one statement may hold in the executor.",
    ),
    def(
        "yuzhu.validate_plans",
        if cfg!(debug_assertions) { "on" } else { "off" },
        false,
        Kind::Bool,
        "Checks the invariants of every plan after planning.",
    ),
    def(
        "enable_hashjoin",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of hash join plans.",
    ),
    def(
        "enable_nestloop",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of nested-loop join plans.",
    ),
    def(
        "enable_hashagg",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of hashed aggregation plans.",
    ),
    def(
        "enable_sort",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of explicit sort steps.",
    ),
    def(
        "enable_material",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of materialization.",
    ),
    def(
        "enable_seqscan",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of sequential-scan plans.",
    ),
    def(
        "enable_indexscan",
        "on",
        false,
        Kind::Bool,
        "Enables the planner's use of index-scan plans.",
    ),
    def(
        "statement_timeout",
        "0",
        false,
        Kind::Millis,
        "Sets the maximum allowed duration of any statement.",
    ),
    def(
        "TimeZone",
        "UTC",
        true,
        Kind::TimeZone,
        "Sets the time zone for displaying and interpreting time stamps.",
    ),
    def(
        "transaction_isolation",
        "read committed",
        false,
        Kind::Isolation,
        "Sets the current transaction's isolation level.",
    ),
    def(
        "transaction_read_only",
        "off",
        false,
        Kind::Bool,
        "Sets the current transaction's read-only status.",
    ),
    def(
        "transaction_deferrable",
        "off",
        false,
        Kind::Bool,
        "Whether to defer a read-only serializable transaction until it can be executed with no possible serialization failures.",
    ),
    def(
        "default_transaction_deferrable",
        "off",
        false,
        Kind::Bool,
        "Sets the default deferrable status of new transactions.",
    ),
    def(
        "idle_session_timeout",
        "0",
        false,
        Kind::Millis,
        "Sets the maximum allowed idle time between queries, when not in a transaction.",
    ),
    def(
        "deadlock_timeout",
        "1s",
        false,
        Kind::MillisMin1,
        "Sets the time to wait on a lock before checking for deadlock.",
    ),
    def(
        "synchronous_commit",
        "on",
        false,
        Kind::SyncCommit,
        "Sets the current transaction's synchronization level.",
    ),
    def(
        "full_page_writes",
        "on",
        false,
        Kind::Sighup,
        "Writes full pages to WAL when first modified after a checkpoint.",
    ),
    def(
        "wal_segment_size",
        "16MB",
        false,
        Kind::ReadOnly,
        "Shows the size of write ahead log segments.",
    ),
    def(
        "wal_sync_method",
        "fdatasync",
        false,
        Kind::Sighup,
        "Selects the method used for forcing WAL updates to disk.",
    ),
    def(
        "max_wal_size",
        "1GB",
        false,
        Kind::Sighup,
        "Sets the WAL size that triggers a checkpoint.",
    ),
];

/// Looks up a registry entry by (case-insensitive) name.
pub fn lookup(name: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.name.eq_ignore_ascii_case(name))
}

fn is_custom(name: &str) -> bool {
    name.contains('.')
}

fn unrecognized(name: &str) -> Error {
    Error::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

fn exceeds_int_range(name: &str, value: &str) -> Error {
    invalid_value(name, value).with_hint("Value exceeds integer range.")
}

fn fits_i32(v: i64) -> bool {
    i32::try_from(v).is_ok()
}

/// 整数として解釈する。i32 に収まらない（i64 溢れ含む）値は PG と同じく
/// "Value exceeds integer range." 付きの invalid value にする。
fn parse_int32_like(name: &str, num: &str, raw: &str) -> Result<i64> {
    match num.parse::<i64>() {
        Ok(n) if fits_i32(n) => Ok(n),
        Ok(_) => Err(exceeds_int_range(name, raw)),
        Err(_)
            if !num.is_empty()
                && num
                    .trim_start_matches(['-', '+'])
                    .bytes()
                    .all(|b| b.is_ascii_digit()) =>
        {
            Err(exceeds_int_range(name, raw))
        }
        Err(_) if num.contains(['.', 'e', 'E']) => {
            // PostgreSQL の parse_int は小数・指数表記を許し、偶数丸めで整数にする。
            let f: f64 = num.parse().map_err(|_| invalid_value(name, raw))?;
            let r = f.round_ties_even();
            if !r.is_finite() || r.abs() > f64::from(i32::MAX) + 1.0 {
                return Err(exceeds_int_range(name, raw));
            }
            #[allow(clippy::cast_possible_truncation)]
            let n = r as i64;
            if fits_i32(n) {
                Ok(n)
            } else {
                Err(exceeds_int_range(name, raw))
            }
        }
        Err(_) => Err(invalid_value(name, raw)),
    }
}

fn invalid_value(name: &str, value: &str) -> Error {
    Error::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{name}\": \"{value}\""),
    )
}

/// Quotes an identifier like PostgreSQL's `quote_identifier` (keywords are
/// not checked in M1).
pub fn quote_identifier(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if safe {
        s.to_owned()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// PostgreSQL's `parse_bool`: unique prefixes of true/false/yes/no, on/off
/// (at least `on`/`of`), 1/0.
pub fn parse_bool(s: &str) -> Option<bool> {
    let v = s.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let pre = |full: &str| full.starts_with(v.as_str());
    match v.as_str() {
        "1" | "on" => Some(true),
        "0" => Some(false),
        _ if v.len() >= 2 && pre("off") => Some(false),
        _ if pre("true") => Some(true),
        _ if pre("false") => Some(false),
        _ if pre("yes") => Some(true),
        _ if pre("no") => Some(false),
        _ => None,
    }
}

fn parse_millis(name: &str, raw: &str) -> Result<i64> {
    parse_millis_min(name, raw, 0)
}

fn parse_millis_min(name: &str, raw: &str, min: i64) -> Result<i64> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let mult = match unit.trim() {
        "" | "ms" => 1,
        "us" => 0,
        "s" => 1000,
        "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => {
            return Err(invalid_value(name, raw).with_hint(
                "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\".",
            ));
        }
    };
    let v = if num.contains(['.', 'e', 'E']) || mult == 0 {
        // PostgreSQL は小数値・us を最小単位 (ms) へ換算し、偶数丸めで整数にする。
        let n: f64 = num.parse().map_err(|_| invalid_value(name, raw))?;
        let factor = if mult == 0 {
            0.001
        } else {
            f64::from(i32::try_from(mult).unwrap_or(i32::MAX))
        };
        let scaled = (n * factor).round_ties_even();
        if !scaled.is_finite() || scaled.abs() > f64::from(i32::MAX) {
            return Err(exceeds_int_range(name, raw));
        }
        #[allow(clippy::cast_possible_truncation)]
        let scaled = scaled as i64;
        scaled
    } else {
        let n = parse_int32_like(name, num, raw)?;
        n.checked_mul(mult)
            .filter(|v| fits_i32(*v))
            .ok_or_else(|| exceeds_int_range(name, raw))?
    };
    if !(min..=i64::from(i32::MAX)).contains(&v) {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "{v} ms is outside the valid range for parameter \"{name}\" ({min} ms .. 2147483647 ms)"
            ),
        ));
    }
    Ok(v)
}

fn format_millis(v: i64) -> String {
    if v == 0 {
        return "0".into();
    }
    for (unit, m) in [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("min", 60_000),
        ("s", 1000),
    ] {
        if v % m == 0 {
            return format!("{}{unit}", v / m);
        }
    }
    format!("{v}ms")
}

/// Why a `DateStyle` value was rejected.
#[derive(Clone, Copy)]
enum DateStyleError {
    Invalid,
    /// Two different output styles or two different field orders.
    Conflict,
}

fn datestyle_error(name: &str, raw: &str, e: DateStyleError) -> Error {
    let err = invalid_value(name, raw);
    match e {
        DateStyleError::Invalid => err,
        DateStyleError::Conflict => err.with_detail("Conflicting \"datestyle\" specifications."),
    }
}

fn normalize_datestyle(current: &str, raw: &str) -> std::result::Result<String, DateStyleError> {
    let mut parts = current.split(',').map(str::trim);
    let mut style = parts.next().unwrap_or("ISO").to_owned();
    let mut order = parts.next().unwrap_or("MDY").to_owned();
    let mut german = false;
    let mut have_style = false;
    let mut have_order = false;
    for tok in raw.split(',').map(|t| t.trim().to_ascii_lowercase()) {
        let new_style = match tok.as_str() {
            "iso" => Some("ISO"),
            "postgres" => Some("Postgres"),
            "sql" => Some("SQL"),
            "german" => Some("German"),
            _ => None,
        };
        if let Some(ns) = new_style {
            if have_style && style != ns {
                return Err(DateStyleError::Conflict);
            }
            have_style = true;
            style = ns.into();
            german = ns == "German";
            continue;
        }
        let new_order = match tok.as_str() {
            "mdy" | "us" | "noneuropean" | "non-european" => "MDY",
            "dmy" | "european" | "euro" => "DMY",
            "ymd" => "YMD",
            "default" => {
                style = "ISO".into();
                order = "MDY".into();
                continue;
            }
            _ => return Err(DateStyleError::Invalid),
        };
        if have_order && order != new_order {
            return Err(DateStyleError::Conflict);
        }
        have_order = true;
        order = new_order.into();
    }
    if german && !have_order {
        order = "DMY".into();
    }
    Ok(format!("{style}, {order}"))
}

/// The error of a parameter that cannot be changed by `SET`.
fn fixed_error(def: &SettingDef) -> Option<Error> {
    if matches!(def.kind, Kind::ReadOnly | Kind::Sighup)
        && let Some(r) = restart_lookup(def.name)
    {
        return Some(restart_error(def.name, r.reason));
    }
    let suffix = match def.kind {
        Kind::ReadOnly => "",
        Kind::Sighup => " now",
        _ => return None,
    };
    Some(Error::new(
        sqlstate::CANT_CHANGE_RUNTIME_PARAM,
        format!("parameter \"{}\" cannot be changed{suffix}", def.name),
    ))
}

/// Validates `args` for `def` and returns the normalized value.
fn normalize(def: &SettingDef, current: &str, args: &[String]) -> Result<String> {
    let name = def.name;
    if let Some(e) = fixed_error(def) {
        return Err(e);
    }
    if let Kind::IdentList = def.kind {
        return Ok(args
            .iter()
            .map(|a| quote_identifier(a))
            .collect::<Vec<_>>()
            .join(", "));
    }
    let raw = match args {
        [one] => one.as_str(),
        _ if matches!(def.kind, Kind::DateStyle) => {
            let joined = args.join(", ");
            return normalize_datestyle(current, &args.join(","))
                .map_err(|e| datestyle_error(name, &joined, e));
        }
        _ => {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("SET {} takes only one argument", name.to_ascii_lowercase()),
            ));
        }
    };
    match def.kind {
        Kind::ReadOnly | Kind::Sighup | Kind::IdentList => unreachable!("handled above"),
        Kind::Str => Ok(raw.to_owned()),
        Kind::Bool => match parse_bool(raw) {
            Some(true) => Ok("on".into()),
            Some(false) => Ok("off".into()),
            None => Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" requires a Boolean value"),
            )),
        },
        Kind::OnlyOn => match parse_bool(raw) {
            Some(true) => Ok("on".into()),
            Some(false) => Err(Error::not_supported(format!(
                "parameter \"{name}\" can only be set to on"
            ))),
            None => Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" requires a Boolean value"),
            )),
        },
        Kind::Int { min, max } => {
            let v = parse_int32_like(name, raw.trim(), raw)?;
            if v < min || v > max {
                return Err(Error::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!(
                        "{v} is outside the valid range for parameter \"{name}\" ({min} .. {max})"
                    ),
                ));
            }
            Ok(v.to_string())
        }
        Kind::Mem { min, max } => normalize_mem_kb(name, raw, min, max),
        Kind::Millis => parse_millis(name, raw).map(format_millis),
        Kind::MillisMin1 => parse_millis_min(name, raw, 1).map(format_millis),
        Kind::SyncCommit => {
            const VALUES: &[&str] = &["local", "remote_write", "remote_apply", "on", "off"];
            let v = raw.to_ascii_lowercase();
            match v.as_str() {
                "true" | "yes" | "1" => Ok("on".into()),
                "false" | "no" | "0" => Ok("off".into()),
                _ if VALUES.contains(&v.as_str()) => Ok(v),
                _ => Err(invalid_value(name, raw)
                    .with_hint(format!("Available values: {}.", VALUES.join(", ")))),
            }
        }
        Kind::Isolation => Isolation::parse(name, raw).map(|i| i.as_str().to_owned()),
        Kind::Enum(values) => {
            let v = raw.trim().to_ascii_lowercase();
            if values.contains(&v.as_str()) {
                Ok(v)
            } else {
                // PostgreSQL reports enum parameters by their lower-case name.
                Err(invalid_value(&name.to_ascii_lowercase(), raw)
                    .with_hint(format!("Available values: {}.", values.join(", "))))
            }
        }
        Kind::Encoding => {
            let v = raw.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "utf8" | "utf-8" | "unicode") {
                Ok("UTF8".into())
            } else if matches!(
                v.as_str(),
                "latin1" | "iso88591" | "iso_8859_1" | "iso-8859-1" | "iso8859-1" | "l1"
            ) {
                Ok("LATIN1".into())
            } else {
                Err(invalid_value(name, raw))
            }
        }
        Kind::DateStyle => {
            normalize_datestyle(current, raw).map_err(|e| datestyle_error(name, raw, e))
        }
        Kind::TimeZone => normalize_timezone(raw).ok_or_else(|| invalid_value(name, raw)),
    }
}

/// Where the tz database lives. When it is absent, every well-formed name is
/// accepted since there is nothing to check against.
const ZONEINFO_DIR: &str = "/usr/share/zoneinfo";

/// Validates a `TimeZone` value: an IANA zone name (exact case), `UTC` in any
/// case, a numeric offset (`5`, `-03`, `+05:30`) or a POSIX-style string
/// (`PST8PDT`, `UTC+3`).
fn normalize_timezone(raw: &str) -> Option<String> {
    if raw.eq_ignore_ascii_case("utc") {
        return Some("UTC".into());
    }
    if raw.is_empty() || raw.starts_with("right/") {
        return None;
    }
    (is_numeric_offset(raw) || is_posix_zone(raw) || zone_exists(raw)).then(|| raw.to_owned())
}

fn zone_exists(name: &str) -> bool {
    let dir = std::path::Path::new(ZONEINFO_DIR);
    if name.starts_with('/')
        || name
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return false;
    }
    if !dir.is_dir() {
        return name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b));
    }
    dir.join(name).is_file()
}

/// `[+-]hh[:mm[:ss]]`.
fn is_numeric_offset(s: &str) -> bool {
    let rest = s.strip_prefix(['+', '-']).unwrap_or(s);
    let parts: Vec<&str> = rest.split(':').collect();
    parts.len() <= 3
        && parts
            .iter()
            .all(|p| (1..=2).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `std offset [dst]`: a name of three or more letters followed by an offset
/// and optionally a DST name (`PST8PDT`, `UTC+3`).
fn is_posix_zone(s: &str) -> bool {
    let name_len = s.bytes().take_while(u8::is_ascii_alphabetic).count();
    if name_len < 3 {
        return false;
    }
    let rest = &s[name_len..];
    let offset_len = rest
        .bytes()
        .position(|b| b.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (offset, dst) = rest.split_at(offset_len);
    is_numeric_offset(offset) && dst.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The isolation levels that can run. `REPEATABLE READ` and `SERIALIZABLE`
/// are rejected with `0A000` until M5; `READ UNCOMMITTED` behaves as
/// `READ COMMITTED` but is shown by name (as in PostgreSQL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    #[default]
    ReadCommitted,
    ReadUncommitted,
}

impl Isolation {
    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::ReadCommitted => "read committed",
            Isolation::ReadUncommitted => "read uncommitted",
        }
    }

    /// Parses a level name as `SET transaction_isolation` does
    /// (case-insensitive). Unknown names are `22023`; the names of the
    /// levels that are not supported yet are `0A000`.
    pub fn parse(name: &str, raw: &str) -> Result<Isolation> {
        // An enum value matches exactly (case-insensitive): two spaces are
        // not one. The parser joins the words of `ISOLATION LEVEL` itself.
        match raw.to_ascii_lowercase().as_str() {
            "read committed" => Ok(Isolation::ReadCommitted),
            "read uncommitted" => Ok(Isolation::ReadUncommitted),
            level @ ("repeatable read" | "serializable") => Err(Error::not_supported(format!(
                "transaction isolation level \"{level}\" is not supported yet"
            ))),
            _ => Err(invalid_value(name, raw).with_hint(
                "Available values: serializable, repeatable read, read committed, read uncommitted.",
            )),
        }
    }
}

/// The characteristics of a transaction (`m3.md` §6.11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxnCharacteristics {
    pub isolation: Isolation,
    pub read_only: bool,
    pub deferrable: bool,
}

/// Storage key: lower-cased name.
fn key(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// One session's parameter values.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Values `RESET` returns to (defaults overridden by startup options).
    reset_values: HashMap<String, String>,
    /// Values that survive COMMIT (plain `SET`).
    session: HashMap<String, String>,
    /// Effective values (`session` plus `SET LOCAL`).
    current: HashMap<String, String>,
    /// `session` at transaction start, restored by ROLLBACK.
    txn_start: Option<HashMap<String, String>>,
}

impl Settings {
    /// Creates the settings of a new session. `startup` holds the startup
    /// packet's parameters (e.g. `application_name`, `DateStyle`); invalid
    /// or unknown ones are ignored.
    pub fn new(user: &str, startup: &[(String, String)]) -> Settings {
        let mut reset_values: HashMap<String, String> = SETTINGS
            .iter()
            .map(|d| (key(d.name), d.default.to_owned()))
            .collect();
        reset_values.insert("session_authorization".into(), user.to_owned());
        for g in INERT_GUCS {
            reset_values.insert(key(g.name), inert_default(g));
        }
        for r in RESTART_ONLY_GUCS {
            reset_values
                .entry(key(r.name))
                .or_insert_with(|| r.default.to_owned());
        }
        for (name, value) in startup {
            if let Some(d) = lookup(name) {
                let current = reset_values.get(&key(d.name)).cloned().unwrap_or_default();
                if let Ok(v) = normalize(d, &current, std::slice::from_ref(value)) {
                    reset_values.insert(key(d.name), v);
                }
            } else if let Some(g) = inert_lookup(name)
                && let Ok(v) = normalize_inert(g, std::slice::from_ref(value))
            {
                reset_values.insert(key(g.name), v);
            }
        }
        Settings {
            session: reset_values.clone(),
            current: reset_values.clone(),
            reset_values,
            txn_start: None,
        }
    }

    /// Sets the value shown for a read-only parameter that depends on the
    /// server (`shared_buffers`, `data_directory`, ...). Unknown names are
    /// ignored.
    pub fn set_server_value(&mut self, name: &str, value: &str) {
        if let Some(d) = lookup(name) {
            let k = key(d.name);
            self.reset_values.insert(k.clone(), value.to_owned());
            self.session.insert(k.clone(), value.to_owned());
            self.current.insert(k, value.to_owned());
        }
    }

    /// The effective value of a parameter (`SHOW`). Returns the canonical
    /// name and the value.
    pub fn show(&self, name: &str) -> Result<(String, String)> {
        let canonical = match lookup(name) {
            Some(d) => d.name.to_owned(),
            None if inert_lookup(name).is_some() || restart_lookup(name).is_some() => key(name),
            None if is_custom(name) => key(name),
            None => return Err(unrecognized(name)),
        };
        match self.current.get(&key(name)) {
            Some(v) => Ok((canonical, v.clone())),
            None => Err(unrecognized(name)),
        }
    }

    /// The effective value, or "" if unknown (for internal use).
    pub fn get(&self, name: &str) -> &str {
        self.current.get(&key(name)).map_or("", String::as_str)
    }

    /// `SET [LOCAL] name TO args` (`args = None` means `TO DEFAULT`).
    /// `local` must only be passed inside a transaction block.
    pub fn set(&mut self, name: &str, args: Option<&[String]>, local: bool) -> Result<()> {
        let k = key(name);
        let value = match lookup(name) {
            Some(d) if d.name == "session_authorization" => {
                let user = self.reset_values.get(&k).cloned().unwrap_or_default();
                match args {
                    None => user,
                    Some([a]) if *a == user => user,
                    Some(_) => {
                        return Err(Error::not_supported(
                            "switching the session user is not supported",
                        ));
                    }
                }
            }
            Some(d) if d.name == "role" => match args {
                None => "none".into(),
                Some([a]) if a.eq_ignore_ascii_case("none") => "none".into(),
                Some([a]) if Some(a) == self.reset_values.get("session_authorization") => a.clone(),
                Some(_) => {
                    return Err(Error::not_supported("switching the role is not supported"));
                }
            },
            Some(d) => match args {
                None => {
                    if let Some(e) = fixed_error(d) {
                        return Err(e);
                    }
                    self.reset_values.get(&k).cloned().unwrap_or_default()
                }
                Some(a) => normalize(d, self.get(name), a)?,
            },
            None if inert_lookup(name).is_some() => {
                let g = inert_lookup(name).ok_or_else(|| unrecognized(name))?;
                match args {
                    None => self.reset_values.get(&k).cloned().unwrap_or_default(),
                    Some(a) => normalize_inert(g, a)?,
                }
            }
            None if restart_lookup(name).is_some() => {
                let r = restart_lookup(name).ok_or_else(|| unrecognized(name))?;
                return Err(restart_error(r.name, r.reason));
            }
            None if is_custom(name) => match args {
                None => String::new(),
                Some(a) => a.join(", "),
            },
            None => return Err(unrecognized(name)),
        };
        if !local {
            self.session.insert(k.clone(), value.clone());
        } else if !self.session.contains_key(&k) {
            // A custom parameter first created by SET LOCAL: it stays
            // defined (empty) after the transaction, as in PostgreSQL.
            self.session.insert(k.clone(), String::new());
        }
        self.current.insert(k, value);
        Ok(())
    }

    /// Declares a custom parameter with an empty value if it is unknown
    /// (`SET LOCAL` outside a block still leaves the placeholder, as in
    /// PostgreSQL).
    pub fn declare_custom(&mut self, name: &str) {
        if lookup(name).is_none() && is_custom(name) {
            let k = key(name);
            self.session.entry(k.clone()).or_default();
            self.current.entry(k).or_default();
        }
    }

    /// `RESET name`.
    pub fn reset(&mut self, name: &str) -> Result<()> {
        self.set(name, None, false)
    }

    /// `RESET ALL`: every changeable parameter goes back to its reset value.
    pub fn reset_all(&mut self) {
        for d in SETTINGS {
            if matches!(d.kind, Kind::ReadOnly | Kind::Sighup) {
                continue;
            }
            let k = key(d.name);
            let v = self.reset_values.get(&k).cloned().unwrap_or_default();
            self.session.insert(k.clone(), v.clone());
            self.current.insert(k, v);
        }
        for g in INERT_GUCS {
            let k = key(g.name);
            let v = self.reset_values.get(&k).cloned().unwrap_or_default();
            self.session.insert(k.clone(), v.clone());
            self.current.insert(k, v);
        }
        let customs: Vec<String> = self
            .current
            .keys()
            .filter(|k| is_custom(k))
            .cloned()
            .collect();
        for k in customs {
            self.session.insert(k.clone(), String::new());
            self.current.insert(k, String::new());
        }
    }

    /// Marks the start of a transaction (implicit or explicit).
    pub fn begin(&mut self) {
        if self.txn_start.is_none() {
            self.txn_start = Some(self.session.clone());
        }
    }

    /// End of transaction: keep plain SETs, drop SET LOCALs.
    pub fn commit(&mut self) {
        self.txn_start = None;
        self.current = self.session.clone();
    }

    /// Transaction rolled back: restore the values at transaction start.
    pub fn rollback(&mut self) {
        if let Some(start) = self.txn_start.take() {
            // A custom parameter created in the transaction stays defined
            // (empty) after the rollback, as in PostgreSQL.
            let created: Vec<String> = self
                .session
                .keys()
                .filter(|k| is_custom(k) && !start.contains_key(*k))
                .cloned()
                .collect();
            self.session = start;
            for k in created {
                self.session.insert(k, String::new());
            }
        }
        self.current = self.session.clone();
    }

    /// Current values of the reported parameters (canonical names).
    pub fn reported_values(&self) -> Vec<(&'static str, String)> {
        SETTINGS
            .iter()
            .filter(|d| d.reported)
            .map(|d| (d.name, self.get(d.name).to_owned()))
            .collect()
    }

    /// `SHOW ALL`: (name, setting, description), sorted by name.
    pub fn show_all(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<_> = SETTINGS
            .iter()
            .map(|d| {
                (
                    d.name.to_owned(),
                    self.get(d.name).to_owned(),
                    d.description.to_owned(),
                )
            })
            .collect();
        for g in INERT_GUCS {
            v.push((
                g.name.to_owned(),
                self.get(g.name).to_owned(),
                String::new(),
            ));
        }
        for r in RESTART_ONLY_GUCS
            .iter()
            .filter(|r| lookup(r.name).is_none())
        {
            v.push((
                r.name.to_owned(),
                self.get(r.name).to_owned(),
                String::new(),
            ));
        }
        v.sort_by_key(|(n, _, _)| n.to_ascii_lowercase());
        v
    }

    /// `extra_float_digits` as an integer.
    pub fn extra_float_digits(&self) -> i32 {
        self.get("extra_float_digits").parse().unwrap_or(1)
    }

    /// The value of a millisecond-valued parameter; `None` for 0.
    fn duration_setting(&self, name: &str) -> Option<std::time::Duration> {
        let ms = parse_millis(name, self.get(name)).unwrap_or(0);
        u64::try_from(ms)
            .ok()
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
    }

    /// `lock_timeout`; `None` waits forever (value 0).
    pub fn lock_timeout(&self) -> Option<std::time::Duration> {
        self.duration_setting("lock_timeout")
    }

    /// `statement_timeout`; `None` means no limit (value 0).
    pub fn statement_timeout(&self) -> Option<std::time::Duration> {
        self.duration_setting("statement_timeout")
    }

    /// `idle_in_transaction_session_timeout`; `None` means no limit.
    pub fn idle_in_transaction_session_timeout(&self) -> Option<std::time::Duration> {
        self.duration_setting("idle_in_transaction_session_timeout")
    }

    /// `idle_session_timeout`; `None` means no limit.
    pub fn idle_session_timeout(&self) -> Option<std::time::Duration> {
        self.duration_setting("idle_session_timeout")
    }

    /// The `default_transaction_*` parameters: what a new transaction
    /// starts with.
    pub fn default_characteristics(&self) -> TxnCharacteristics {
        TxnCharacteristics {
            isolation: Isolation::parse(
                "default_transaction_isolation",
                self.get("default_transaction_isolation"),
            )
            .unwrap_or_default(),
            read_only: parse_bool(self.get("default_transaction_read_only")) == Some(true),
            deferrable: parse_bool(self.get("default_transaction_deferrable")) == Some(true),
        }
    }

    /// Validates `args` for the parameter `name` and returns the normalized
    /// value without storing it (used for `transaction_*`, which the
    /// session keeps itself).
    pub fn validate(&self, name: &str, args: &[String]) -> Result<String> {
        if let Some(g) = inert_lookup(name) {
            return normalize_inert(g, args);
        }
        if lookup(name).is_none()
            && let Some(r) = restart_lookup(name)
        {
            return Err(restart_error(r.name, r.reason));
        }
        let d = lookup(name).ok_or_else(|| unrecognized(name))?;
        normalize(d, self.get(name), args)
    }

    /// Whether a message of `level` (`notice`, `warning`, ...) passes
    /// `client_min_messages`.
    pub fn client_wants(&self, level: &str) -> bool {
        let rank = |l: &str| LEVELS.iter().position(|x| *x == l);
        match (rank(level), rank(self.get("client_min_messages"))) {
            (Some(a), Some(b)) => a >= b,
            _ => true,
        }
    }

    /// Schema names of `search_path`, unquoted (`"$user"` kept as `$user`).
    pub fn search_path(&self) -> Vec<String> {
        split_ident_list(self.get("search_path"))
    }
}

// ===== S: 受け付けるだけの GUC（`m4/10-explain-copy-compat.md` §9） =====

/// 値の基準の単位。整数・実数の GUC は、この単位の値で保存・検査する。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    None,
    Kb,
    /// 8kB 単位の整数。
    Block8Kb,
    Bytes,
    Ms,
    Sec,
}

impl Unit {
    /// エラーメッセージの単位の綴り。
    fn label(self) -> &'static str {
        match self {
            Unit::None => "",
            Unit::Kb => "kB",
            Unit::Block8Kb => "8kB",
            Unit::Bytes => "B",
            Unit::Ms => "ms",
            Unit::Sec => "s",
        }
    }

    /// 入力の単位の綴りから、基準の単位に直す倍率。
    fn factor(self, unit: &str) -> Option<f64> {
        const KB: f64 = 1024.0;
        match self {
            Unit::None => None,
            Unit::Kb | Unit::Block8Kb | Unit::Bytes => {
                let bytes = match unit {
                    "B" => 1.0,
                    "kB" => KB,
                    "MB" => KB * KB,
                    "GB" => KB * KB * KB,
                    "TB" => KB * KB * KB * KB,
                    _ => return None,
                };
                let base = match self {
                    Unit::Kb => KB,
                    Unit::Block8Kb => 8.0 * KB,
                    _ => 1.0,
                };
                Some(bytes / base)
            }
            Unit::Ms | Unit::Sec => {
                let ms = match unit {
                    "us" => 0.001,
                    "ms" => 1.0,
                    "s" => 1000.0,
                    "min" => 60_000.0,
                    "h" => 3_600_000.0,
                    "d" => 86_400_000.0,
                    _ => return None,
                };
                Some(if self == Unit::Sec { ms / 1000.0 } else { ms })
            }
        }
    }

    /// 単位が誤っているときの HINT。
    fn hint(self) -> Option<&'static str> {
        match self {
            Unit::None => None,
            Unit::Kb | Unit::Block8Kb | Unit::Bytes => Some(
                "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".",
            ),
            Unit::Ms | Unit::Sec => Some(
                "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\".",
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum InertKind {
    Bool,
    /// 値の単位（基準の単位の整数で保存する）と範囲（基準の単位で）。
    Int {
        unit: Unit,
        min: i64,
        max: i64,
    },
    Real {
        unit: Unit,
        min: f64,
        max: f64,
    },
    /// 一覧の順（HINT と、同義語の正規名の決定に使う）。
    Enum(&'static [&'static str]),
    Str,
}

/// PostgreSQL 17 にあって yuzhu では意味を持たない設定。`SET` は値の形を検査して保存し、
/// `SHOW` / `current_setting` はそれを返す。
#[derive(Debug)]
pub struct InertGuc {
    pub name: &'static str,
    /// 基準の単位の値（`SHOW` の表示ではない）。
    pub default: &'static str,
    pub kind: InertKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestartReason {
    Postmaster,
    Sighup,
    Internal,
    /// `backend` / `superuser-backend`（接続の開始後は変えられない）。
    AfterConnectionStart,
}

/// PostgreSQL に存在するが `SET` できない名前。`SET` と `set_config` は `55P02`。
#[derive(Debug)]
pub struct RestartOnlyGuc {
    pub name: &'static str,
    pub reason: RestartReason,
    /// `SHOW` が返す値（`postgresql.conf` の既定。表示の形）。
    pub default: &'static str,
}

/// PostgreSQL 17.11 の `pg_settings`（`context` が `user` / `superuser`）から、M1〜M3 の `SETTINGS` と
/// 意味を持つもの（`enable_*` の 7 件と `yuzhu.*`）を除いた 162 件。
#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
pub static INERT_GUCS: &[InertGuc] = &[
    InertGuc { name: "allow_in_place_tablespaces", default: "off", kind: InertKind::Bool },
    InertGuc { name: "allow_system_table_mods", default: "off", kind: InertKind::Bool },
    InertGuc { name: "array_nulls", default: "on", kind: InertKind::Bool },
    InertGuc { name: "backend_flush_after", default: "0", kind: InertKind::Int { unit: Unit::Block8Kb, min: 0, max: 256 } },
    InertGuc { name: "backslash_quote", default: "safe_encoding", kind: InertKind::Enum(&["safe_encoding", "on", "off"]) },
    InertGuc { name: "backtrace_functions", default: "", kind: InertKind::Str },
    InertGuc { name: "check_function_bodies", default: "on", kind: InertKind::Bool },
    InertGuc { name: "client_connection_check_interval", default: "0", kind: InertKind::Int { unit: Unit::Ms, min: 0, max: 2147483647 } },
    InertGuc { name: "commit_delay", default: "0", kind: InertKind::Int { unit: Unit::None, min: 0, max: 100000 } },
    InertGuc { name: "commit_siblings", default: "5", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1000 } },
    InertGuc { name: "compute_query_id", default: "auto", kind: InertKind::Enum(&["auto", "regress", "on", "off"]) },
    InertGuc { name: "constraint_exclusion", default: "partition", kind: InertKind::Enum(&["partition", "on", "off"]) },
    InertGuc { name: "cpu_index_tuple_cost", default: "0.005", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "cpu_operator_cost", default: "0.0025", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "cpu_tuple_cost", default: "0.01", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "createrole_self_grant", default: "", kind: InertKind::Str },
    InertGuc { name: "cursor_tuple_fraction", default: "0.1", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: 1.0 } },
    InertGuc { name: "debug_discard_caches", default: "0", kind: InertKind::Int { unit: Unit::None, min: 0, max: 0 } },
    InertGuc { name: "debug_logical_replication_streaming", default: "buffered", kind: InertKind::Enum(&["buffered", "immediate"]) },
    InertGuc { name: "debug_parallel_query", default: "off", kind: InertKind::Enum(&["off", "on", "regress"]) },
    InertGuc { name: "debug_pretty_print", default: "on", kind: InertKind::Bool },
    InertGuc { name: "debug_print_parse", default: "off", kind: InertKind::Bool },
    InertGuc { name: "debug_print_plan", default: "off", kind: InertKind::Bool },
    InertGuc { name: "debug_print_rewritten", default: "off", kind: InertKind::Bool },
    InertGuc { name: "default_statistics_target", default: "100", kind: InertKind::Int { unit: Unit::None, min: 1, max: 10000 } },
    InertGuc { name: "default_table_access_method", default: "heap", kind: InertKind::Str },
    InertGuc { name: "default_tablespace", default: "", kind: InertKind::Str },
    InertGuc { name: "default_text_search_config", default: "pg_catalog.english", kind: InertKind::Str },
    InertGuc { name: "default_toast_compression", default: "pglz", kind: InertKind::Enum(&["pglz", "lz4"]) },
    InertGuc { name: "dynamic_library_path", default: "$libdir", kind: InertKind::Str },
    InertGuc { name: "effective_cache_size", default: "524288", kind: InertKind::Int { unit: Unit::Block8Kb, min: 1, max: 2147483647 } },
    InertGuc { name: "effective_io_concurrency", default: "1", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1000 } },
    InertGuc { name: "enable_async_append", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_bitmapscan", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_gathermerge", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_group_by_reordering", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_incremental_sort", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_indexonlyscan", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_memoize", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_mergejoin", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_parallel_append", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_parallel_hash", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_partition_pruning", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_partitionwise_aggregate", default: "off", kind: InertKind::Bool },
    InertGuc { name: "enable_partitionwise_join", default: "off", kind: InertKind::Bool },
    InertGuc { name: "enable_presorted_aggregate", default: "on", kind: InertKind::Bool },
    InertGuc { name: "enable_tidscan", default: "on", kind: InertKind::Bool },
    InertGuc { name: "escape_string_warning", default: "on", kind: InertKind::Bool },
    InertGuc { name: "event_triggers", default: "on", kind: InertKind::Bool },
    InertGuc { name: "exit_on_error", default: "off", kind: InertKind::Bool },
    InertGuc { name: "extension_destdir", default: "", kind: InertKind::Str },
    InertGuc { name: "from_collapse_limit", default: "8", kind: InertKind::Int { unit: Unit::None, min: 1, max: 2147483647 } },
    InertGuc { name: "geqo", default: "on", kind: InertKind::Bool },
    InertGuc { name: "geqo_effort", default: "5", kind: InertKind::Int { unit: Unit::None, min: 1, max: 10 } },
    InertGuc { name: "geqo_generations", default: "0", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2147483647 } },
    InertGuc { name: "geqo_pool_size", default: "0", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2147483647 } },
    InertGuc { name: "geqo_seed", default: "0", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: 1.0 } },
    InertGuc { name: "geqo_selection_bias", default: "2", kind: InertKind::Real { unit: Unit::None, min: 1.5, max: 2.0 } },
    InertGuc { name: "geqo_threshold", default: "12", kind: InertKind::Int { unit: Unit::None, min: 2, max: 2147483647 } },
    InertGuc { name: "gin_fuzzy_search_limit", default: "0", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2147483647 } },
    InertGuc { name: "gin_pending_list_limit", default: "4096", kind: InertKind::Int { unit: Unit::Kb, min: 64, max: 2147483647 } },
    InertGuc { name: "hash_mem_multiplier", default: "2", kind: InertKind::Real { unit: Unit::None, min: 1.0, max: 1000.0 } },
    InertGuc { name: "icu_validation_level", default: "warning", kind: InertKind::Enum(&["disabled", "debug5", "debug4", "debug3", "debug2", "debug1", "log", "notice", "warning", "error"]) },
    InertGuc { name: "ignore_checksum_failure", default: "off", kind: InertKind::Bool },
    InertGuc { name: "io_combine_limit", default: "16", kind: InertKind::Int { unit: Unit::Block8Kb, min: 1, max: 32 } },
    InertGuc { name: "jit", default: "on", kind: InertKind::Bool },
    InertGuc { name: "jit_above_cost", default: "100000", kind: InertKind::Real { unit: Unit::None, min: -1.0, max: f64::MAX } },
    InertGuc { name: "jit_dump_bitcode", default: "off", kind: InertKind::Bool },
    InertGuc { name: "jit_expressions", default: "on", kind: InertKind::Bool },
    InertGuc { name: "jit_inline_above_cost", default: "500000", kind: InertKind::Real { unit: Unit::None, min: -1.0, max: f64::MAX } },
    InertGuc { name: "jit_optimize_above_cost", default: "500000", kind: InertKind::Real { unit: Unit::None, min: -1.0, max: f64::MAX } },
    InertGuc { name: "jit_tuple_deforming", default: "on", kind: InertKind::Bool },
    InertGuc { name: "join_collapse_limit", default: "8", kind: InertKind::Int { unit: Unit::None, min: 1, max: 2147483647 } },
    InertGuc { name: "lc_monetary", default: "C", kind: InertKind::Str },
    InertGuc { name: "lc_numeric", default: "C", kind: InertKind::Str },
    InertGuc { name: "lc_time", default: "C", kind: InertKind::Str },
    InertGuc { name: "lo_compat_privileges", default: "off", kind: InertKind::Bool },
    InertGuc { name: "local_preload_libraries", default: "", kind: InertKind::Str },
    InertGuc { name: "log_duration", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_error_verbosity", default: "default", kind: InertKind::Enum(&["terse", "default", "verbose"]) },
    InertGuc { name: "log_executor_stats", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_lock_waits", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_min_duration_sample", default: "-1", kind: InertKind::Int { unit: Unit::Ms, min: -1, max: 2147483647 } },
    InertGuc { name: "log_min_duration_statement", default: "-1", kind: InertKind::Int { unit: Unit::Ms, min: -1, max: 2147483647 } },
    InertGuc { name: "log_min_error_statement", default: "error", kind: InertKind::Enum(&["debug5", "debug4", "debug3", "debug2", "debug1", "info", "notice", "warning", "error", "log", "fatal", "panic"]) },
    InertGuc { name: "log_min_messages", default: "warning", kind: InertKind::Enum(&["debug5", "debug4", "debug3", "debug2", "debug1", "info", "notice", "warning", "error", "log", "fatal", "panic"]) },
    InertGuc { name: "log_parameter_max_length", default: "-1", kind: InertKind::Int { unit: Unit::Bytes, min: -1, max: 1073741823 } },
    InertGuc { name: "log_parameter_max_length_on_error", default: "0", kind: InertKind::Int { unit: Unit::Bytes, min: -1, max: 1073741823 } },
    InertGuc { name: "log_parser_stats", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_planner_stats", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_replication_commands", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_statement", default: "none", kind: InertKind::Enum(&["none", "ddl", "mod", "all"]) },
    InertGuc { name: "log_statement_sample_rate", default: "1", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: 1.0 } },
    InertGuc { name: "log_statement_stats", default: "off", kind: InertKind::Bool },
    InertGuc { name: "log_temp_files", default: "-1", kind: InertKind::Int { unit: Unit::Kb, min: -1, max: 2147483647 } },
    InertGuc { name: "log_transaction_sample_rate", default: "0", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: 1.0 } },
    InertGuc { name: "logical_decoding_work_mem", default: "65536", kind: InertKind::Int { unit: Unit::Kb, min: 64, max: 2147483647 } },
    InertGuc { name: "maintenance_io_concurrency", default: "10", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1000 } },
    InertGuc { name: "maintenance_work_mem", default: "65536", kind: InertKind::Int { unit: Unit::Kb, min: 64, max: 2147483647 } },
    InertGuc { name: "max_parallel_maintenance_workers", default: "2", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1024 } },
    InertGuc { name: "max_parallel_workers", default: "8", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1024 } },
    InertGuc { name: "max_parallel_workers_per_gather", default: "2", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1024 } },
    InertGuc { name: "max_stack_depth", default: "2048", kind: InertKind::Int { unit: Unit::Kb, min: 100, max: 2147483647 } },
    InertGuc { name: "min_parallel_index_scan_size", default: "64", kind: InertKind::Int { unit: Unit::Block8Kb, min: 0, max: 715827882 } },
    InertGuc { name: "min_parallel_table_scan_size", default: "1024", kind: InertKind::Int { unit: Unit::Block8Kb, min: 0, max: 715827882 } },
    InertGuc { name: "output_plugin_libraries", default: "pgoutput, test_decoding", kind: InertKind::Str },
    InertGuc { name: "parallel_leader_participation", default: "on", kind: InertKind::Bool },
    InertGuc { name: "parallel_setup_cost", default: "1000", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "parallel_tuple_cost", default: "0.1", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "password_encryption", default: "scram-sha-256", kind: InertKind::Enum(&["md5", "scram-sha-256"]) },
    InertGuc { name: "plan_cache_mode", default: "auto", kind: InertKind::Enum(&["auto", "force_generic_plan", "force_custom_plan"]) },
    InertGuc { name: "quote_all_identifiers", default: "off", kind: InertKind::Bool },
    InertGuc { name: "random_page_cost", default: "4", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "recursive_worktable_factor", default: "10", kind: InertKind::Real { unit: Unit::None, min: 0.001, max: 1e6 } },
    InertGuc { name: "restrict_nonsystem_relation_kind", default: "", kind: InertKind::Str },
    InertGuc { name: "row_security", default: "on", kind: InertKind::Bool },
    InertGuc { name: "scram_iterations", default: "4096", kind: InertKind::Int { unit: Unit::None, min: 1, max: 2147483647 } },
    InertGuc { name: "seq_page_cost", default: "1", kind: InertKind::Real { unit: Unit::None, min: 0.0, max: f64::MAX } },
    InertGuc { name: "session_preload_libraries", default: "", kind: InertKind::Str },
    InertGuc { name: "session_replication_role", default: "origin", kind: InertKind::Enum(&["origin", "replica", "local"]) },
    InertGuc { name: "stats_fetch_consistency", default: "cache", kind: InertKind::Enum(&["none", "cache", "snapshot"]) },
    InertGuc { name: "synchronize_seqscans", default: "on", kind: InertKind::Bool },
    InertGuc { name: "tcp_keepalives_count", default: "9", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2147483647 } },
    InertGuc { name: "tcp_keepalives_idle", default: "7200", kind: InertKind::Int { unit: Unit::Sec, min: 0, max: 2147483647 } },
    InertGuc { name: "tcp_keepalives_interval", default: "75", kind: InertKind::Int { unit: Unit::Sec, min: 0, max: 2147483647 } },
    InertGuc { name: "tcp_user_timeout", default: "0", kind: InertKind::Int { unit: Unit::Ms, min: 0, max: 2147483647 } },
    InertGuc { name: "temp_buffers", default: "1024", kind: InertKind::Int { unit: Unit::Block8Kb, min: 100, max: 1073741823 } },
    InertGuc { name: "temp_file_limit", default: "-1", kind: InertKind::Int { unit: Unit::Kb, min: -1, max: 2147483647 } },
    InertGuc { name: "temp_tablespaces", default: "", kind: InertKind::Str },
    InertGuc { name: "timezone_abbreviations", default: "Default", kind: InertKind::Str },
    InertGuc { name: "trace_notify", default: "off", kind: InertKind::Bool },
    InertGuc { name: "trace_sort", default: "off", kind: InertKind::Bool },
    InertGuc { name: "track_activities", default: "on", kind: InertKind::Bool },
    InertGuc { name: "track_counts", default: "on", kind: InertKind::Bool },
    InertGuc { name: "track_functions", default: "none", kind: InertKind::Enum(&["none", "pl", "all"]) },
    InertGuc { name: "track_io_timing", default: "off", kind: InertKind::Bool },
    InertGuc { name: "track_wal_io_timing", default: "off", kind: InertKind::Bool },
    InertGuc { name: "transaction_timeout", default: "0", kind: InertKind::Int { unit: Unit::Ms, min: 0, max: 2147483647 } },
    InertGuc { name: "transform_null_equals", default: "off", kind: InertKind::Bool },
    InertGuc { name: "update_process_title", default: "on", kind: InertKind::Bool },
    InertGuc { name: "vacuum_buffer_usage_limit", default: "2048", kind: InertKind::Int { unit: Unit::Kb, min: 0, max: 16777216 } },
    InertGuc { name: "vacuum_cost_delay", default: "0", kind: InertKind::Real { unit: Unit::Ms, min: 0.0, max: 100.0 } },
    InertGuc { name: "vacuum_cost_limit", default: "200", kind: InertKind::Int { unit: Unit::None, min: 1, max: 10000 } },
    InertGuc { name: "vacuum_cost_page_dirty", default: "20", kind: InertKind::Int { unit: Unit::None, min: 0, max: 10000 } },
    InertGuc { name: "vacuum_cost_page_hit", default: "1", kind: InertKind::Int { unit: Unit::None, min: 0, max: 10000 } },
    InertGuc { name: "vacuum_cost_page_miss", default: "2", kind: InertKind::Int { unit: Unit::None, min: 0, max: 10000 } },
    InertGuc { name: "vacuum_failsafe_age", default: "1600000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2100000000 } },
    InertGuc { name: "vacuum_freeze_min_age", default: "50000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1000000000 } },
    InertGuc { name: "vacuum_freeze_table_age", default: "150000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2000000000 } },
    InertGuc { name: "vacuum_multixact_failsafe_age", default: "1600000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2100000000 } },
    InertGuc { name: "vacuum_multixact_freeze_min_age", default: "5000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 1000000000 } },
    InertGuc { name: "vacuum_multixact_freeze_table_age", default: "150000000", kind: InertKind::Int { unit: Unit::None, min: 0, max: 2000000000 } },
    InertGuc { name: "wal_compression", default: "off", kind: InertKind::Enum(&["pglz", "lz4", "zstd", "on", "off"]) },
    InertGuc { name: "wal_consistency_checking", default: "", kind: InertKind::Str },
    InertGuc { name: "wal_init_zero", default: "on", kind: InertKind::Bool },
    InertGuc { name: "wal_recycle", default: "on", kind: InertKind::Bool },
    InertGuc { name: "wal_sender_timeout", default: "60000", kind: InertKind::Int { unit: Unit::Ms, min: 0, max: 2147483647 } },
    InertGuc { name: "wal_skip_threshold", default: "2048", kind: InertKind::Int { unit: Unit::Kb, min: 0, max: 2147483647 } },
    InertGuc { name: "work_mem", default: "4096", kind: InertKind::Int { unit: Unit::Kb, min: 64, max: 2147483647 } },
    InertGuc { name: "xmlbinary", default: "base64", kind: InertKind::Enum(&["base64", "hex"]) },
    InertGuc { name: "xmloption", default: "content", kind: InertKind::Enum(&["content", "document"]) },
    InertGuc { name: "zero_damaged_pages", default: "off", kind: InertKind::Bool },
];

/// PostgreSQL 17.11 の `pg_settings` で `context` が `postmaster` / `sighup` / `internal` の 182 件と、
/// `backend` / `superuser-backend` の 6 件（合計 188 件）。M1〜M3 の `SETTINGS` にあるものは、そちらが優先する。
#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
pub static RESTART_ONLY_GUCS: &[RestartOnlyGuc] = &[
    RestartOnlyGuc { name: "allow_alter_system", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "archive_cleanup_command", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "archive_command", reason: RestartReason::Sighup, default: "(disabled)" },
    RestartOnlyGuc { name: "archive_library", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "archive_mode", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "archive_timeout", reason: RestartReason::Sighup, default: "0" },
    RestartOnlyGuc { name: "authentication_timeout", reason: RestartReason::Sighup, default: "1min" },
    RestartOnlyGuc { name: "autovacuum", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "autovacuum_analyze_scale_factor", reason: RestartReason::Sighup, default: "0.1" },
    RestartOnlyGuc { name: "autovacuum_analyze_threshold", reason: RestartReason::Sighup, default: "50" },
    RestartOnlyGuc { name: "autovacuum_freeze_max_age", reason: RestartReason::Postmaster, default: "200000000" },
    RestartOnlyGuc { name: "autovacuum_max_workers", reason: RestartReason::Postmaster, default: "3" },
    RestartOnlyGuc { name: "autovacuum_multixact_freeze_max_age", reason: RestartReason::Postmaster, default: "400000000" },
    RestartOnlyGuc { name: "autovacuum_naptime", reason: RestartReason::Sighup, default: "1min" },
    RestartOnlyGuc { name: "autovacuum_vacuum_cost_delay", reason: RestartReason::Sighup, default: "2ms" },
    RestartOnlyGuc { name: "autovacuum_vacuum_cost_limit", reason: RestartReason::Sighup, default: "-1" },
    RestartOnlyGuc { name: "autovacuum_vacuum_insert_scale_factor", reason: RestartReason::Sighup, default: "0.2" },
    RestartOnlyGuc { name: "autovacuum_vacuum_insert_threshold", reason: RestartReason::Sighup, default: "1000" },
    RestartOnlyGuc { name: "autovacuum_vacuum_scale_factor", reason: RestartReason::Sighup, default: "0.2" },
    RestartOnlyGuc { name: "autovacuum_vacuum_threshold", reason: RestartReason::Sighup, default: "50" },
    RestartOnlyGuc { name: "autovacuum_work_mem", reason: RestartReason::Sighup, default: "-1" },
    RestartOnlyGuc { name: "bgwriter_delay", reason: RestartReason::Sighup, default: "200ms" },
    RestartOnlyGuc { name: "bgwriter_flush_after", reason: RestartReason::Sighup, default: "512kB" },
    RestartOnlyGuc { name: "bgwriter_lru_maxpages", reason: RestartReason::Sighup, default: "100" },
    RestartOnlyGuc { name: "bgwriter_lru_multiplier", reason: RestartReason::Sighup, default: "2" },
    RestartOnlyGuc { name: "block_size", reason: RestartReason::Internal, default: "8192" },
    RestartOnlyGuc { name: "bonjour", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "bonjour_name", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "checkpoint_completion_target", reason: RestartReason::Sighup, default: "0.9" },
    RestartOnlyGuc { name: "checkpoint_flush_after", reason: RestartReason::Sighup, default: "256kB" },
    RestartOnlyGuc { name: "checkpoint_timeout", reason: RestartReason::Sighup, default: "5min" },
    RestartOnlyGuc { name: "checkpoint_warning", reason: RestartReason::Sighup, default: "30s" },
    RestartOnlyGuc { name: "cluster_name", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "commit_timestamp_buffers", reason: RestartReason::Postmaster, default: "256kB" },
    RestartOnlyGuc { name: "config_file", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "data_checksums", reason: RestartReason::Internal, default: "off" },
    RestartOnlyGuc { name: "data_directory", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "data_directory_mode", reason: RestartReason::Internal, default: "0700" },
    RestartOnlyGuc { name: "data_sync_retry", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "debug_assertions", reason: RestartReason::Internal, default: "off" },
    RestartOnlyGuc { name: "debug_io_direct", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "dynamic_shared_memory_type", reason: RestartReason::Postmaster, default: "posix" },
    RestartOnlyGuc { name: "event_source", reason: RestartReason::Postmaster, default: "PostgreSQL" },
    RestartOnlyGuc { name: "external_pid_file", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "file_extend_method", reason: RestartReason::Sighup, default: "posix_fallocate" },
    RestartOnlyGuc { name: "fsync", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "full_page_writes", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "gss_accept_delegation", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "hba_file", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "hot_standby", reason: RestartReason::Postmaster, default: "on" },
    RestartOnlyGuc { name: "hot_standby_feedback", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "huge_page_size", reason: RestartReason::Postmaster, default: "0" },
    RestartOnlyGuc { name: "huge_pages", reason: RestartReason::Postmaster, default: "try" },
    RestartOnlyGuc { name: "huge_pages_status", reason: RestartReason::Internal, default: "off" },
    RestartOnlyGuc { name: "ident_file", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "ignore_invalid_pages", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "ignore_system_indexes", reason: RestartReason::AfterConnectionStart, default: "off" },
    RestartOnlyGuc { name: "in_hot_standby", reason: RestartReason::Internal, default: "off" },
    RestartOnlyGuc { name: "integer_datetimes", reason: RestartReason::Internal, default: "on" },
    RestartOnlyGuc { name: "jit_debugging_support", reason: RestartReason::AfterConnectionStart, default: "off" },
    RestartOnlyGuc { name: "jit_profiling_support", reason: RestartReason::AfterConnectionStart, default: "off" },
    RestartOnlyGuc { name: "jit_provider", reason: RestartReason::Postmaster, default: "llvmjit" },
    RestartOnlyGuc { name: "krb_caseins_users", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "krb_server_keyfile", reason: RestartReason::Sighup, default: "FILE:/etc/postgresql-common/krb5.keytab" },
    RestartOnlyGuc { name: "listen_addresses", reason: RestartReason::Postmaster, default: "localhost" },
    RestartOnlyGuc { name: "log_autovacuum_min_duration", reason: RestartReason::Sighup, default: "10min" },
    RestartOnlyGuc { name: "log_checkpoints", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "log_connections", reason: RestartReason::AfterConnectionStart, default: "off" },
    RestartOnlyGuc { name: "log_destination", reason: RestartReason::Sighup, default: "stderr" },
    RestartOnlyGuc { name: "log_directory", reason: RestartReason::Sighup, default: "log" },
    RestartOnlyGuc { name: "log_disconnections", reason: RestartReason::AfterConnectionStart, default: "off" },
    RestartOnlyGuc { name: "log_file_mode", reason: RestartReason::Sighup, default: "0600" },
    RestartOnlyGuc { name: "log_filename", reason: RestartReason::Sighup, default: "postgresql-%Y-%m-%d_%H%M%S.log" },
    RestartOnlyGuc { name: "log_hostname", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "log_line_prefix", reason: RestartReason::Sighup, default: "%m [%p] " },
    RestartOnlyGuc { name: "log_recovery_conflict_waits", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "log_rotation_age", reason: RestartReason::Sighup, default: "1d" },
    RestartOnlyGuc { name: "log_rotation_size", reason: RestartReason::Sighup, default: "10MB" },
    RestartOnlyGuc { name: "log_startup_progress_interval", reason: RestartReason::Sighup, default: "10s" },
    RestartOnlyGuc { name: "log_timezone", reason: RestartReason::Sighup, default: "Etc/UTC" },
    RestartOnlyGuc { name: "log_truncate_on_rotation", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "logging_collector", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "max_connections", reason: RestartReason::Postmaster, default: "100" },
    RestartOnlyGuc { name: "max_files_per_process", reason: RestartReason::Postmaster, default: "1000" },
    RestartOnlyGuc { name: "max_function_args", reason: RestartReason::Internal, default: "100" },
    RestartOnlyGuc { name: "max_identifier_length", reason: RestartReason::Internal, default: "63" },
    RestartOnlyGuc { name: "max_index_keys", reason: RestartReason::Internal, default: "32" },
    RestartOnlyGuc { name: "max_locks_per_transaction", reason: RestartReason::Postmaster, default: "64" },
    RestartOnlyGuc { name: "max_logical_replication_workers", reason: RestartReason::Postmaster, default: "4" },
    RestartOnlyGuc { name: "max_notify_queue_pages", reason: RestartReason::Postmaster, default: "1048576" },
    RestartOnlyGuc { name: "max_parallel_apply_workers_per_subscription", reason: RestartReason::Sighup, default: "2" },
    RestartOnlyGuc { name: "max_pred_locks_per_page", reason: RestartReason::Sighup, default: "2" },
    RestartOnlyGuc { name: "max_pred_locks_per_relation", reason: RestartReason::Sighup, default: "-2" },
    RestartOnlyGuc { name: "max_pred_locks_per_transaction", reason: RestartReason::Postmaster, default: "64" },
    RestartOnlyGuc { name: "max_prepared_transactions", reason: RestartReason::Postmaster, default: "0" },
    RestartOnlyGuc { name: "max_replication_slots", reason: RestartReason::Postmaster, default: "10" },
    RestartOnlyGuc { name: "max_slot_wal_keep_size", reason: RestartReason::Sighup, default: "-1" },
    RestartOnlyGuc { name: "max_standby_archive_delay", reason: RestartReason::Sighup, default: "30s" },
    RestartOnlyGuc { name: "max_standby_streaming_delay", reason: RestartReason::Sighup, default: "30s" },
    RestartOnlyGuc { name: "max_sync_workers_per_subscription", reason: RestartReason::Sighup, default: "2" },
    RestartOnlyGuc { name: "max_wal_senders", reason: RestartReason::Postmaster, default: "10" },
    RestartOnlyGuc { name: "max_wal_size", reason: RestartReason::Sighup, default: "1GB" },
    RestartOnlyGuc { name: "max_worker_processes", reason: RestartReason::Postmaster, default: "8" },
    RestartOnlyGuc { name: "min_dynamic_shared_memory", reason: RestartReason::Postmaster, default: "0" },
    RestartOnlyGuc { name: "min_wal_size", reason: RestartReason::Sighup, default: "80MB" },
    RestartOnlyGuc { name: "multixact_member_buffers", reason: RestartReason::Postmaster, default: "256kB" },
    RestartOnlyGuc { name: "multixact_offset_buffers", reason: RestartReason::Postmaster, default: "128kB" },
    RestartOnlyGuc { name: "notify_buffers", reason: RestartReason::Postmaster, default: "128kB" },
    RestartOnlyGuc { name: "port", reason: RestartReason::Postmaster, default: "5432" },
    RestartOnlyGuc { name: "post_auth_delay", reason: RestartReason::AfterConnectionStart, default: "0" },
    RestartOnlyGuc { name: "pre_auth_delay", reason: RestartReason::Sighup, default: "0" },
    RestartOnlyGuc { name: "primary_conninfo", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "primary_slot_name", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "recovery_end_command", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "recovery_init_sync_method", reason: RestartReason::Sighup, default: "fsync" },
    RestartOnlyGuc { name: "recovery_min_apply_delay", reason: RestartReason::Sighup, default: "0" },
    RestartOnlyGuc { name: "recovery_prefetch", reason: RestartReason::Sighup, default: "try" },
    RestartOnlyGuc { name: "recovery_target", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "recovery_target_action", reason: RestartReason::Postmaster, default: "pause" },
    RestartOnlyGuc { name: "recovery_target_inclusive", reason: RestartReason::Postmaster, default: "on" },
    RestartOnlyGuc { name: "recovery_target_lsn", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "recovery_target_name", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "recovery_target_time", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "recovery_target_timeline", reason: RestartReason::Postmaster, default: "latest" },
    RestartOnlyGuc { name: "recovery_target_xid", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "remove_temp_files_after_crash", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "reserved_connections", reason: RestartReason::Postmaster, default: "0" },
    RestartOnlyGuc { name: "restart_after_crash", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "restore_command", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "segment_size", reason: RestartReason::Internal, default: "1GB" },
    RestartOnlyGuc { name: "send_abort_for_crash", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "send_abort_for_kill", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "serializable_buffers", reason: RestartReason::Postmaster, default: "256kB" },
    RestartOnlyGuc { name: "server_encoding", reason: RestartReason::Internal, default: "UTF8" },
    RestartOnlyGuc { name: "server_version", reason: RestartReason::Internal, default: "17.11 (Debian 17.11-1.pgdg12+2)" },
    RestartOnlyGuc { name: "server_version_num", reason: RestartReason::Internal, default: "170011" },
    RestartOnlyGuc { name: "shared_buffers", reason: RestartReason::Postmaster, default: "128MB" },
    RestartOnlyGuc { name: "shared_memory_size", reason: RestartReason::Internal, default: "143MB" },
    RestartOnlyGuc { name: "shared_memory_size_in_huge_pages", reason: RestartReason::Internal, default: "72" },
    RestartOnlyGuc { name: "shared_memory_type", reason: RestartReason::Postmaster, default: "mmap" },
    RestartOnlyGuc { name: "shared_preload_libraries", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "ssl", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "ssl_ca_file", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_cert_file", reason: RestartReason::Sighup, default: "server.crt" },
    RestartOnlyGuc { name: "ssl_ciphers", reason: RestartReason::Sighup, default: "HIGH:MEDIUM:+3DES:!aNULL" },
    RestartOnlyGuc { name: "ssl_crl_dir", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_crl_file", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_dh_params_file", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_ecdh_curve", reason: RestartReason::Sighup, default: "prime256v1" },
    RestartOnlyGuc { name: "ssl_key_file", reason: RestartReason::Sighup, default: "server.key" },
    RestartOnlyGuc { name: "ssl_library", reason: RestartReason::Internal, default: "OpenSSL" },
    RestartOnlyGuc { name: "ssl_max_protocol_version", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_min_protocol_version", reason: RestartReason::Sighup, default: "TLSv1.2" },
    RestartOnlyGuc { name: "ssl_passphrase_command", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "ssl_passphrase_command_supports_reload", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "ssl_prefer_server_ciphers", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "subtransaction_buffers", reason: RestartReason::Postmaster, default: "256kB" },
    RestartOnlyGuc { name: "summarize_wal", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "superuser_reserved_connections", reason: RestartReason::Postmaster, default: "3" },
    RestartOnlyGuc { name: "sync_replication_slots", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "synchronized_standby_slots", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "synchronous_standby_names", reason: RestartReason::Sighup, default: "" },
    RestartOnlyGuc { name: "syslog_facility", reason: RestartReason::Sighup, default: "local0" },
    RestartOnlyGuc { name: "syslog_ident", reason: RestartReason::Sighup, default: "postgres" },
    RestartOnlyGuc { name: "syslog_sequence_numbers", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "syslog_split_messages", reason: RestartReason::Sighup, default: "on" },
    RestartOnlyGuc { name: "trace_connection_negotiation", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "track_activity_query_size", reason: RestartReason::Postmaster, default: "1kB" },
    RestartOnlyGuc { name: "track_commit_timestamp", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "transaction_buffers", reason: RestartReason::Postmaster, default: "256kB" },
    RestartOnlyGuc { name: "unix_socket_directories", reason: RestartReason::Postmaster, default: "/var/run/postgresql" },
    RestartOnlyGuc { name: "unix_socket_group", reason: RestartReason::Postmaster, default: "" },
    RestartOnlyGuc { name: "unix_socket_permissions", reason: RestartReason::Postmaster, default: "0777" },
    RestartOnlyGuc { name: "wal_block_size", reason: RestartReason::Internal, default: "8192" },
    RestartOnlyGuc { name: "wal_buffers", reason: RestartReason::Postmaster, default: "4MB" },
    RestartOnlyGuc { name: "wal_decode_buffer_size", reason: RestartReason::Postmaster, default: "512kB" },
    RestartOnlyGuc { name: "wal_keep_size", reason: RestartReason::Sighup, default: "0" },
    RestartOnlyGuc { name: "wal_level", reason: RestartReason::Postmaster, default: "replica" },
    RestartOnlyGuc { name: "wal_log_hints", reason: RestartReason::Postmaster, default: "off" },
    RestartOnlyGuc { name: "wal_receiver_create_temp_slot", reason: RestartReason::Sighup, default: "off" },
    RestartOnlyGuc { name: "wal_receiver_status_interval", reason: RestartReason::Sighup, default: "10s" },
    RestartOnlyGuc { name: "wal_receiver_timeout", reason: RestartReason::Sighup, default: "1min" },
    RestartOnlyGuc { name: "wal_retrieve_retry_interval", reason: RestartReason::Sighup, default: "5s" },
    RestartOnlyGuc { name: "wal_segment_size", reason: RestartReason::Internal, default: "16MB" },
    RestartOnlyGuc { name: "wal_summary_keep_time", reason: RestartReason::Sighup, default: "10d" },
    RestartOnlyGuc { name: "wal_sync_method", reason: RestartReason::Sighup, default: "fdatasync" },
    RestartOnlyGuc { name: "wal_writer_delay", reason: RestartReason::Sighup, default: "200ms" },
    RestartOnlyGuc { name: "wal_writer_flush_after", reason: RestartReason::Sighup, default: "1MB" },
];

fn inert_lookup(name: &str) -> Option<&'static InertGuc> {
    INERT_GUCS
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(name))
}

fn restart_lookup(name: &str) -> Option<&'static RestartOnlyGuc> {
    RESTART_ONLY_GUCS
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(name))
}

/// `SET` できるか否かによらず、PostgreSQL にある（または yuzhu が持つ）名前か。
pub fn is_known(name: &str) -> bool {
    lookup(name).is_some() || inert_lookup(name).is_some() || restart_lookup(name).is_some()
}

/// `SET` が `55P02` になる理由の文言（PostgreSQL の `set_config_with_handle`）。
fn restart_error(name: &str, reason: RestartReason) -> Error {
    let msg = match reason {
        RestartReason::Postmaster => {
            format!("parameter \"{name}\" cannot be changed without restarting the server")
        }
        RestartReason::Sighup => format!("parameter \"{name}\" cannot be changed now"),
        RestartReason::Internal => format!("parameter \"{name}\" cannot be changed"),
        RestartReason::AfterConnectionStart => {
            format!("parameter \"{name}\" cannot be set after connection start")
        }
    };
    Error::new(sqlstate::CANT_CHANGE_RUNTIME_PARAM, msg)
}

/// 失敗の種類。`Invalid` は `invalid value for parameter`（HINT つきのことがある）。
enum NumError {
    Invalid(Option<&'static str>),
    /// 数としては読めたが範囲外（メッセージ用の値の文字列つき）。
    Range(String),
}

/// 数の先頭の部分。`strtol(value, &end, 0)`（0x の 16 進、0 で始まる 8 進）と、
/// `.` `e` `E` が続くときの `strtod`。
#[allow(clippy::many_single_char_names)]
fn split_number(s: &str) -> std::result::Result<(f64, &str), NumError> {
    let b = s.as_bytes();
    let (neg, mut i) = match b.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let rest = &s[i..];
    let (radix, digits_at) = if rest.len() > 2
        && (rest.starts_with("0x") || rest.starts_with("0X"))
        && rest.as_bytes()[2].is_ascii_hexdigit()
    {
        (16, i + 2)
    } else if rest.starts_with('0') {
        (8, i)
    } else {
        (10, i)
    };
    let mut j = digits_at;
    while j < b.len() && (b[j] as char).is_digit(radix) {
        j += 1;
    }
    if j == i {
        // 数字がない。`.5` と `inf` / `nan` は実数として読む。
        return split_float(s).ok_or(NumError::Invalid(None));
    }
    // 0 だけ、または 8 進で 8・9 が続く場合（`08`）は、読めた部分までが整数。
    let after = &s[j..];
    if matches!(after.as_bytes().first(), Some(b'.' | b'e' | b'E')) {
        return split_float(s).ok_or(NumError::Invalid(None));
    }
    let digits = &s[digits_at..j];
    let mut v: i128 = 0;
    for c in digits.chars() {
        v = v * i128::from(radix) + i128::from(c.to_digit(radix).unwrap_or(0));
        if v > i128::from(i64::MAX) + 1 {
            return Err(NumError::Invalid(None));
        }
    }
    if neg {
        v = -v;
    }
    i = j;
    #[allow(clippy::cast_precision_loss)]
    Ok((v as f64, &s[i..]))
}

/// `[+-]digits[.digits][e[+-]digits]`、`inf` `infinity` `nan`（大文字小文字を区別しない）。
#[allow(clippy::many_single_char_names)]
fn split_float(s: &str) -> Option<(f64, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'-' | b'+')) {
        i += 1;
    }
    let word = |t: &str| s[i..].len() >= t.len() && s[i..i + t.len()].eq_ignore_ascii_case(t);
    let end = if word("infinity") {
        i + 8
    } else if word("inf") || word("nan") {
        i + 3
    } else {
        let mut j = i;
        let int_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let mut digits = j - int_start;
        if j < b.len() && b[j] == b'.' {
            j += 1;
            let f = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            digits += j - f;
        }
        if digits == 0 {
            return None;
        }
        if j < b.len() && matches!(b[j], b'e' | b'E') {
            let mut k = j + 1;
            if k < b.len() && matches!(b[k], b'+' | b'-') {
                k += 1;
            }
            let es = k;
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            if k > es {
                j = k;
            }
        }
        j
    };
    let text = &s[..end];
    let v: f64 = text.parse().ok().or_else(|| {
        // Rust は `+inf` `infinity` を受け付けるが、`nan` の符号などは受け付けない。
        let t = text.trim_start_matches(['+', '-']).to_ascii_lowercase();
        let neg = text.starts_with('-');
        match t.as_str() {
            "inf" | "infinity" => Some(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }),
            "nan" => Some(f64::NAN),
            _ => None,
        }
    })?;
    let had_inf = text.to_ascii_lowercase().contains("inf");
    if v.is_infinite() && !had_inf {
        // `1e400`: strtod の ERANGE。
        return None;
    }
    Some((v, &s[end..]))
}

/// 数の後ろ（空白と単位）を読んで、基準の単位への倍率を返す。
fn parse_unit_suffix(rest: &str, unit: Unit) -> std::result::Result<f64, NumError> {
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Ok(1.0);
    }
    let end = rest
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(rest.len());
    let (u, tail) = rest.split_at(end);
    if !tail.trim().is_empty() {
        return Err(NumError::Invalid(unit.hint()));
    }
    unit.factor(u).ok_or(NumError::Invalid(unit.hint()))
}

fn num_error(name: &str, raw: &str, e: NumError) -> Error {
    match e {
        NumError::Invalid(hint) => {
            let err = invalid_value(name, raw);
            match hint {
                Some(h) => err.with_hint(h),
                None => err,
            }
        }
        NumError::Range(msg) => Error::new(sqlstate::INVALID_PARAMETER_VALUE, msg),
    }
}

/// 小数点以下の末尾の 0（と、残った `.`）を取る。
fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s.to_owned()
    }
}

/// `printf("%g")`（有効数字 6 桁）。
fn fmt_g(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sci = format!("{v:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let e: i32 = exp.parse().unwrap_or(0);
    if (-4..6).contains(&e) {
        let decimals = usize::try_from(5 - e).unwrap_or(0);
        strip_zeros(&format!("{v:.decimals$}"))
    } else {
        let sign = if e < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_zeros(mantissa), e.abs())
    }
}

fn range_message(name: &str, unit: Unit, shown: &str, min: &str, max: &str) -> NumError {
    let u = unit.label();
    let sp = if u.is_empty() { "" } else { " " };
    NumError::Range(format!(
        "{shown}{sp}{u} is outside the valid range for parameter \"{name}\" ({min}{sp}{u} .. {max}{sp}{u})"
    ))
}

/// 整数の GUC の値を、基準の単位の整数にする。小数は `rint`（半端は偶数へ）で丸め、丸めた後の値で
/// 範囲を検査する。32 ビットに収まらないものは `Value exceeds integer range.`。
fn parse_int_guc(
    name: &str,
    raw: &str,
    unit: Unit,
    min: i64,
    max: i64,
) -> std::result::Result<i64, NumError> {
    let (value, rest) = split_number(raw.trim_start())?;
    if value.is_nan() {
        return Err(NumError::Invalid(None));
    }
    let factor = parse_unit_suffix(rest, unit)?;
    let base = (value * factor).round_ties_even();
    if !base.is_finite() || base > f64::from(i32::MAX) || base < f64::from(i32::MIN) {
        return Err(NumError::Invalid(Some("Value exceeds integer range.")));
    }
    #[allow(clippy::cast_possible_truncation)]
    let v = base as i64;
    if v < min || v > max {
        return Err(range_message(
            name,
            unit,
            &v.to_string(),
            &min.to_string(),
            &max.to_string(),
        ));
    }
    Ok(v)
}

fn parse_real_guc(
    name: &str,
    raw: &str,
    unit: Unit,
    min: f64,
    max: f64,
) -> std::result::Result<f64, NumError> {
    let (value, rest) = split_number(raw.trim_start())?;
    if value.is_nan() {
        return Err(NumError::Invalid(None));
    }
    let v = value * parse_unit_suffix(rest, unit)?;
    if v < min || v > max {
        return Err(range_message(
            name,
            unit,
            &fmt_g(v),
            &fmt_g(min),
            &fmt_g(max),
        ));
    }
    Ok(v)
}

/// 値を割り切れる最大の単位で表示する（メモリ: B → kB → MB → GB → TB。時間: ms → s → min → h → d）。
fn format_int_unit(name: &str, unit: Unit, v: i64) -> String {
    // pg が接続のソケットから読み直すもの。単位をつけない。
    if matches!(
        name,
        "tcp_keepalives_idle" | "tcp_keepalives_interval" | "tcp_user_timeout"
    ) {
        return v.to_string();
    }
    if v <= 0 {
        return v.to_string();
    }
    let steps: &[(&str, i64)] = match unit {
        Unit::None => return v.to_string(),
        Unit::Kb | Unit::Block8Kb => {
            &[("TB", 1 << 30), ("GB", 1 << 20), ("MB", 1 << 10), ("kB", 1)]
        }
        Unit::Bytes => &[
            ("TB", 1 << 40),
            ("GB", 1 << 30),
            ("MB", 1 << 20),
            ("kB", 1 << 10),
            ("B", 1),
        ],
        Unit::Ms => &[
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("min", 60_000),
            ("s", 1000),
            ("ms", 1),
        ],
        Unit::Sec => &[("d", 86_400), ("h", 3_600), ("min", 60), ("s", 1)],
    };
    let v = if unit == Unit::Block8Kb { v * 8 } else { v };
    for (label, size) in steps {
        if v % size == 0 {
            return format!("{}{label}", v / size);
        }
    }
    v.to_string()
}

fn format_real(unit: Unit, v: f64) -> String {
    if unit != Unit::Ms || v == 0.0 {
        return fmt_g(v);
    }
    for (label, ms) in [
        ("d", 86_400_000.0),
        ("h", 3_600_000.0),
        ("min", 60_000.0),
        ("s", 1000.0),
        ("ms", 1.0),
    ] {
        let q = v / ms;
        if q.fract() == 0.0 {
            return format!("{}{label}", fmt_g(q));
        }
    }
    format!("{}us", fmt_g(v * 1000.0))
}

/// 値が別名のとき、その enum の表で最初に載っている名前（`wal_compression` の `on` は `pglz`）。
const ENUM_CANONICAL: &[(&str, &str, &str)] = &[("wal_compression", "on", "pglz")];

fn normalize_enum(name: &str, names: &[&'static str], raw: &str) -> Result<String> {
    let v = raw.trim().to_ascii_lowercase();
    let canonical = |n: &str| {
        ENUM_CANONICAL
            .iter()
            .find(|(g, from, _)| *g == name && *from == n)
            .map_or_else(|| n.to_owned(), |(_, _, to)| (*to).to_owned())
    };
    if names.contains(&v.as_str()) {
        return Ok(canonical(&v));
    }
    // `on` と `off` を持つ enum は、隠れた同義語（true / yes / 1 と false / no / 0）を受け付ける。
    if names.contains(&"on") && names.contains(&"off") {
        match v.as_str() {
            "true" | "yes" | "1" => return Ok(canonical("on")),
            "false" | "no" | "0" => return Ok(canonical("off")),
            _ => {}
        }
    }
    Err(invalid_value(name, raw).with_hint(format!("Available values: {}.", names.join(", "))))
}

/// `args` を検査して、保存する値（`SHOW` の表示）にする。
fn normalize_inert(g: &InertGuc, args: &[String]) -> Result<String> {
    let name = g.name;
    let raw = match args {
        [one] => one.as_str(),
        _ if matches!(g.kind, InertKind::Str) => return Ok(args.join(", ")),
        _ => {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("SET {name} takes only one argument"),
            ));
        }
    };
    match g.kind {
        InertKind::Bool => parse_bool(raw)
            .map(|b| on_off_str(b).to_owned())
            .ok_or_else(|| {
                Error::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("parameter \"{name}\" requires a Boolean value"),
                )
            }),
        InertKind::Int { unit, min, max } => parse_int_guc(name, raw, unit, min, max)
            .map(|v| format_int_unit(name, unit, v))
            .map_err(|e| num_error(name, raw, e)),
        InertKind::Real { unit, min, max } => parse_real_guc(name, raw, unit, min, max)
            .map(|v| format_real(unit, v))
            .map_err(|e| num_error(name, raw, e)),
        InertKind::Enum(names) => normalize_enum(name, names, raw),
        InertKind::Str => Ok(raw.to_owned()),
    }
}

fn on_off_str(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// `g` の既定値の表示。
fn inert_default(g: &InertGuc) -> String {
    normalize_inert(g, &[g.default.to_owned()]).unwrap_or_else(|_| g.default.to_owned())
}

/// `yuzhu.query_mem_limit` など、メモリの量（kB 単位で保存）の値の検査。
fn normalize_mem_kb(name: &str, raw: &str, min: i64, max: i64) -> Result<String> {
    parse_int_guc(name, raw, Unit::Kb, min, max)
        .map(|v| format_int_unit(name, Unit::Kb, v))
        .map_err(|e| num_error(name, raw, e))
}

/// `max_stack_depth` などの、`Unit::Kb` の表示を kB の整数に戻す。
fn mem_kb_value(name: &str, shown: &str) -> Option<i64> {
    parse_int_guc(name, shown, Unit::Kb, i64::MIN, i64::MAX).ok()
}

impl Settings {
    /// プランナの設定（`enable_*`、`yuzhu.validate_plans`）。
    #[allow(clippy::field_reassign_with_default)]
    pub fn planner_settings(&self) -> crate::planner::PlannerSettings {
        let on = |n: &str| parse_bool(self.get(n)) == Some(true);
        let mut p = crate::planner::PlannerSettings::default();
        p.enable_seqscan = on("enable_seqscan");
        p.enable_indexscan = on("enable_indexscan");
        p.enable_hashjoin = on("enable_hashjoin");
        p.enable_nestloop = on("enable_nestloop");
        p.enable_hashagg = on("enable_hashagg");
        p.enable_sort = on("enable_sort");
        p.enable_material = on("enable_material");
        p.query_mem_limit = self.query_mem_limit();
        p.validate_plans = on("yuzhu.validate_plans");
        p
    }

    /// `yuzhu.query_mem_limit`（バイト）。
    pub fn query_mem_limit(&self) -> usize {
        let name = "yuzhu.query_mem_limit";
        mem_kb_value(name, self.get(name))
            .and_then(|kb| usize::try_from(kb).ok())
            .map_or(crate::executor::mem::DEFAULT_QUERY_MEM_LIMIT, |kb| {
                kb.saturating_mul(1024)
            })
    }
}

// ===== T2: 日時の設定と TypeEnv の組み立て（`m4/09-types-functions.md` §5.1、C-18） =====

/// 文ごとに作る日時の設定。`TimeZone` の解決結果を持つ（`DateTimeEnv` / `TypeEnv` が借りるため）。
#[derive(Clone, Debug)]
pub struct DateTimeSettings {
    pub time_zone: yuzhu_datetime::TimeZone,
    pub date_style: yuzhu_datetime::DateStyle,
    pub date_order: yuzhu_datetime::DateOrder,
    pub interval_style: yuzhu_datetime::IntervalStyle,
}

impl DateTimeSettings {
    /// `now` = `Transaction.started_at`（PostgreSQL のエポックからのマイクロ秒）。
    pub fn env<'a>(
        &'a self,
        zones: &'a yuzhu_datetime::ZoneDb,
        now: i64,
    ) -> yuzhu_datetime::DateTimeEnv<'a> {
        yuzhu_datetime::DateTimeEnv {
            date_style: self.date_style,
            date_order: self.date_order,
            interval_style: self.interval_style,
            time_zone: &self.time_zone,
            zones,
            now: yuzhu_datetime::TimestampTz(now),
        }
    }
}

impl Settings {
    /// `TimeZone` `DateStyle` `IntervalStyle` の現在値から作る。解決できなければ UTC・ISO, MDY・postgres
    /// （保存時に検証済みなので起きない）。
    pub fn datetime_settings(&self, zones: &yuzhu_datetime::ZoneDb) -> DateTimeSettings {
        let time_zone = yuzhu_datetime::parse_timezone_setting(zones, self.get("TimeZone"))
            .unwrap_or_else(|_| yuzhu_datetime::TimeZone::utc());
        let (date_style, date_order) = yuzhu_datetime::parse_datestyle(
            self.get("DateStyle"),
            (
                yuzhu_datetime::DateStyle::Iso,
                yuzhu_datetime::DateOrder::Mdy,
            ),
        )
        .unwrap_or((
            yuzhu_datetime::DateStyle::Iso,
            yuzhu_datetime::DateOrder::Mdy,
        ));
        let interval_style = yuzhu_datetime::parse_intervalstyle(self.get("IntervalStyle"))
            .unwrap_or(yuzhu_datetime::IntervalStyle::Postgres);
        DateTimeSettings {
            time_zone,
            date_style,
            date_order,
            interval_style,
        }
    }

    /// 入出力が参照する設定（`extra_float_digits`、日時、reg* の名前）。C-18 の署名。
    pub fn type_env<'a>(
        &'a self,
        ds: &'a DateTimeSettings,
        zones: &'a yuzhu_datetime::ZoneDb,
        now: i64,
        names: Option<&'a dyn crate::types::OidNames>,
    ) -> crate::types::TypeEnv<'a> {
        crate::types::TypeEnv {
            extra_float_digits: self.extra_float_digits(),
            datetime: Some(ds.env(zones, now)),
            names,
        }
    }
}

/// `SET TimeZone` の検証。正式名（`utc` → `UTC`、`asia/tokyo` → `Asia/Tokyo`）を返す。
/// 不正なら `22023 invalid value for parameter "TimeZone": "Foo/Bar"`。
pub fn check_timezone(zones: &yuzhu_datetime::ZoneDb, raw: &str) -> Result<String> {
    yuzhu_datetime::parse_timezone_setting(zones, raw)
        .map(|tz| tz.name().to_owned())
        .map_err(crate::types::datetime::dt_err)
}

/// `SET DateStyle` の検証。`current` は現在値（`ISO, MDY`）。出力形式は `ISO` だけを受け付け
/// （`SQL` `Postgres` `German` は `0A000`）、日付順 `MDY` `DMY` `YMD` は受け付ける。
/// 指定のない部分は現在値を保つ。正規の綴り（`ISO, DMY`）を返す。
pub fn check_datestyle(current: &str, raw: &str) -> Result<String> {
    use yuzhu_datetime::{DateOrder, DateStyle};
    let default = (DateStyle::Iso, DateOrder::Mdy);
    let cur = yuzhu_datetime::parse_datestyle(current, default).unwrap_or(default);
    let (style, order) =
        yuzhu_datetime::parse_datestyle(raw, cur).map_err(crate::types::datetime::dt_err)?;
    if style != DateStyle::Iso {
        return Err(Error::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            format!(
                "DateStyle \"{}\" is not supported yet; only the ISO output format is available",
                yuzhu_datetime::format_datestyle(style, order)
            ),
        ));
    }
    Ok(yuzhu_datetime::format_datestyle(style, order))
}

/// Splits a `search_path`-style list into unquoted identifiers.
pub fn split_ident_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut item = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        item.push('"');
                    } else {
                        break;
                    }
                } else {
                    item.push(c);
                }
            }
            while chars.peek().is_some_and(|c| *c != ',') {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                item.push(c);
                chars.next();
            }
            item = item.trim().to_ascii_lowercase();
        }
        out.push(item);
        if chars.next().is_none() {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn m3_parameters_follow_postgresql() {
        let mut st = Settings::new("alice", &[]);
        let e = st
            .set("deadlock_timeout", Some(&s(&["0"])), false)
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        st.set("deadlock_timeout", Some(&s(&["1ms"])), false)
            .unwrap();
        st.set("synchronous_commit", Some(&s(&["true"])), false)
            .unwrap();
        assert_eq!(st.get("synchronous_commit"), "on");
        st.set("synchronous_commit", Some(&s(&["no"])), false)
            .unwrap();
        assert_eq!(st.get("synchronous_commit"), "off");
        let e = st
            .set("synchronous_commit", Some(&s(&["x"])), false)
            .unwrap_err();
        assert_eq!(
            e.hint.as_deref(),
            Some("Available values: local, remote_write, remote_apply, on, off.")
        );
        for n in ["full_page_writes", "wal_sync_method", "max_wal_size"] {
            let e = st.set(n, Some(&s(&["on"])), false).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::CANT_CHANGE_RUNTIME_PARAM);
            assert_eq!(
                e.message,
                format!("parameter \"{n}\" cannot be changed now")
            );
        }
        let e = st
            .set("wal_segment_size", Some(&s(&["1"])), false)
            .unwrap_err();
        assert_eq!(
            e.message,
            "parameter \"wal_segment_size\" cannot be changed"
        );
        let e = st
            .set(
                "default_transaction_isolation",
                Some(&s(&["read  committed"])),
                false,
            )
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        st.set(
            "default_transaction_isolation",
            Some(&s(&["READ COMMITTED"])),
            false,
        )
        .unwrap();
    }

    #[test]
    fn defaults_and_case_insensitive_names() {
        let st = Settings::new("alice", &[]);
        assert_eq!(
            st.show("TIMEZONE").unwrap(),
            ("TimeZone".into(), "UTC".into())
        );
        assert_eq!(st.show("search_path").unwrap().1, "\"$user\", public");
        assert_eq!(st.show("session_authorization").unwrap().1, "alice");
        assert_eq!(st.show("server_version_num").unwrap().1, "170000");
        assert_eq!(st.show("max_identifier_length").unwrap().1, "63");
        assert_eq!(
            st.show("nope").unwrap_err().sqlstate,
            sqlstate::UNDEFINED_OBJECT
        );
        assert_eq!(st.extra_float_digits(), 1);
    }

    #[test]
    fn set_validation() {
        let mut st = Settings::new("u", &[]);
        st.set("extra_float_digits", Some(&s(&["0"])), false)
            .unwrap();
        assert_eq!(st.extra_float_digits(), 0);
        for bad in ["4", "-16", "abc"] {
            let e = st
                .set("extra_float_digits", Some(&s(&[bad])), false)
                .unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE, "{bad}");
        }
        let e = st
            .set("server_version", Some(&s(&["1"])), false)
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::CANT_CHANGE_RUNTIME_PARAM);
        assert_eq!(
            st.set("no_such", Some(&s(&["1"])), false)
                .unwrap_err()
                .sqlstate,
            sqlstate::UNDEFINED_OBJECT
        );
        st.set("search_path", Some(&s(&["myschema", "public"])), false)
            .unwrap();
        assert_eq!(st.get("search_path"), "myschema, public");
        st.set("search_path", Some(&s(&["$user", "public"])), false)
            .unwrap();
        assert_eq!(st.get("search_path"), "\"$user\", public");
        assert_eq!(st.search_path(), s(&["$user", "public"]));
        st.set("statement_timeout", Some(&s(&["90000"])), false)
            .unwrap();
        assert_eq!(st.get("statement_timeout"), "90s");
        st.set("statement_timeout", Some(&s(&["1min"])), false)
            .unwrap();
        assert_eq!(st.get("statement_timeout"), "1min");
        st.set("datestyle", Some(&s(&["iso"])), false).unwrap();
        assert_eq!(st.get("DateStyle"), "ISO, MDY");
        st.set("client_encoding", Some(&s(&["utf-8"])), false)
            .unwrap();
        st.set("client_encoding", Some(&s(&["latin1"])), false)
            .unwrap();
        assert_eq!(st.get("client_encoding"), "LATIN1");
        assert!(
            st.set("client_encoding", Some(&s(&["sjis"])), false)
                .is_err()
        );
        assert!(
            st.set("standard_conforming_strings", Some(&s(&["off"])), false)
                .is_err()
        );
        st.set("client_min_messages", Some(&s(&["WARNING"])), false)
            .unwrap();
        assert!(!st.client_wants("notice"));
        assert!(st.client_wants("warning"));
        assert_eq!(
            st.set("extra_float_digits", Some(&s(&["1", "2"])), false)
                .unwrap_err()
                .message,
            "SET extra_float_digits takes only one argument"
        );
    }

    #[test]
    fn custom_parameters() {
        let mut st = Settings::new("u", &[]);
        assert!(st.show("my.p").is_err());
        st.set("my.p", Some(&s(&["hello"])), false).unwrap();
        assert_eq!(st.show("MY.P").unwrap().1, "hello");
        st.reset("my.p").unwrap();
        assert_eq!(st.show("my.p").unwrap().1, "");
    }

    #[test]
    fn transactional_behaviour() {
        let mut st = Settings::new("u", &[("application_name".into(), "psql".into())]);
        st.begin();
        st.set("application_name", Some(&s(&["x"])), false).unwrap();
        st.rollback();
        assert_eq!(st.get("application_name"), "psql");

        st.begin();
        st.set("application_name", Some(&s(&["kept"])), false)
            .unwrap();
        st.set("extra_float_digits", Some(&s(&["0"])), true)
            .unwrap();
        assert_eq!(st.extra_float_digits(), 0);
        st.commit();
        assert_eq!(st.get("application_name"), "kept");
        assert_eq!(st.extra_float_digits(), 1);

        st.reset("application_name").unwrap();
        assert_eq!(st.get("application_name"), "psql");
        st.set("extra_float_digits", Some(&s(&["2"])), false)
            .unwrap();
        st.reset_all();
        assert_eq!(st.extra_float_digits(), 1);
    }

    #[test]
    fn helpers() {
        assert_eq!(quote_identifier("public"), "public");
        assert_eq!(quote_identifier("$user"), "\"$user\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(parse_bool("o"), None);
        assert_eq!(parse_bool("of"), Some(false));
        assert_eq!(parse_bool("Y"), Some(true));
        assert_eq!(split_ident_list("\"a,b\", C"), s(&["a,b", "c"]));
        assert!(VERSION_STRING.starts_with("PostgreSQL 17.0 (yuzhu "));
    }
}

#[cfg(test)]
mod datetime_settings_tests {
    use super::*;
    use yuzhu_datetime::{DateOrder, DateStyle, IntervalStyle, ZoneDb};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn defaults_build_a_type_env() {
        let st = Settings::new("alice", &[]);
        let zones = ZoneDb::without_tzdata();
        let ds = st.datetime_settings(&zones);
        assert_eq!(ds.time_zone.name(), "UTC");
        assert_eq!(
            (ds.date_style, ds.date_order),
            (DateStyle::Iso, DateOrder::Mdy)
        );
        assert_eq!(ds.interval_style, IntervalStyle::Postgres);
        let env = st.type_env(&ds, &zones, 1234, None);
        assert_eq!(env.extra_float_digits, 1);
        assert!(env.names.is_none());
        let dt = env.datetime.unwrap();
        assert_eq!(dt.now.0, 1234);
        assert_eq!(dt.time_zone.name(), "UTC");
    }

    #[test]
    fn follows_set_values() {
        let mut st = Settings::new("alice", &[]);
        st.set("TimeZone", Some(&s(&["+9"])), false).unwrap();
        st.set("DateStyle", Some(&s(&["ISO", "DMY"])), false)
            .unwrap();
        st.set("IntervalStyle", Some(&s(&["iso_8601"])), false)
            .unwrap();
        st.set("extra_float_digits", Some(&s(&["3"])), false)
            .unwrap();
        let zones = ZoneDb::without_tzdata();
        let ds = st.datetime_settings(&zones);
        assert_eq!(ds.date_order, DateOrder::Dmy);
        assert_eq!(ds.interval_style, IntervalStyle::Iso8601);
        // +9 は POSIX 形式の固定オフセットとして解決される。
        let env = st.type_env(&ds, &zones, 0, None);
        assert_eq!(env.extra_float_digits, 3);
        let d = crate::types::datetime::timestamptz_in("2024-01-01 00:00:00", -1, &env).unwrap();
        // 現地 00:00 は +09 なので UTC では前日の 15:00。
        let utc = yuzhu_datetime::TimeZone::utc();
        let utc_env = crate::types::TypeEnv {
            datetime: Some(yuzhu_datetime::DateTimeEnv::new(&utc, &zones)),
            ..env
        };
        assert_eq!(
            crate::types::datetime::timestamptz_out(&d, &utc_env).unwrap(),
            "2023-12-31 15:00:00+00"
        );
        // DMY で日を先に読む。
        let d = crate::types::datetime::date_in("02/03/2024", &env).unwrap();
        assert_eq!(
            crate::types::datetime::date_out(&d, &env).unwrap(),
            "2024-03-02"
        );
    }

    #[test]
    fn check_timezone_normalizes_and_rejects() {
        let zones = ZoneDb::without_tzdata();
        assert_eq!(check_timezone(&zones, "utc").unwrap(), "UTC");
        let e = check_timezone(&zones, "Foo/Bar").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(
            e.message,
            "invalid value for parameter \"TimeZone\": \"Foo/Bar\""
        );
        // tzdata がなければ地域名は受け付けない。
        let e = check_timezone(&zones, "Asia/Tokyo").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        if std::path::Path::new("/usr/share/zoneinfo/Asia/Tokyo").exists() {
            let zones = ZoneDb::system();
            assert_eq!(check_timezone(&zones, "asia/tokyo").unwrap(), "Asia/Tokyo");
            assert_eq!(check_timezone(&zones, "UTC").unwrap(), "UTC");
            let mut st = Settings::new("alice", &[]);
            st.set("TimeZone", Some(&s(&["Asia/Tokyo"])), false)
                .unwrap();
            let ds = st.datetime_settings(&zones);
            assert_eq!(ds.time_zone.name(), "Asia/Tokyo");
        }
    }

    #[test]
    fn check_datestyle_accepts_only_iso_output() {
        assert_eq!(check_datestyle("ISO, MDY", "ISO").unwrap(), "ISO, MDY");
        assert_eq!(check_datestyle("ISO, MDY", "DMY").unwrap(), "ISO, DMY");
        assert_eq!(check_datestyle("ISO, DMY", "ISO").unwrap(), "ISO, DMY");
        assert_eq!(check_datestyle("ISO, MDY", "ymd").unwrap(), "ISO, YMD");
        assert_eq!(check_datestyle("ISO, DMY", "DEFAULT").unwrap(), "ISO, MDY");
        for bad in ["SQL", "Postgres, DMY", "German"] {
            let e = check_datestyle("ISO, MDY", bad).unwrap_err();
            assert_eq!(e.sqlstate.code(), "0A000", "{bad}");
        }
        let e = check_datestyle("ISO, MDY", "bogus").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(
            e.message,
            "invalid value for parameter \"DateStyle\": \"bogus\""
        );
    }
}

#[cfg(test)]
mod inert_guc_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    fn set_show(st: &mut Settings, name: &str, v: &str) -> String {
        st.set(name, Some(&s(&[v])), false).unwrap();
        st.show(name).unwrap().1
    }

    fn set_err(st: &mut Settings, name: &str, v: &str) -> Error {
        st.set(name, Some(&s(&[v])), false).unwrap_err()
    }

    #[test]
    fn table_sizes_follow_the_design() {
        assert_eq!(INERT_GUCS.len(), 162);
        assert_eq!(RESTART_ONLY_GUCS.len(), 188);
        let count = |f: fn(&InertKind) -> bool| INERT_GUCS.iter().filter(|g| f(&g.kind)).count();
        assert_eq!(count(|k| matches!(k, InertKind::Bool)), 55);
        assert_eq!(count(|k| matches!(k, InertKind::Int { .. })), 53);
        assert_eq!(count(|k| matches!(k, InertKind::Real { .. })), 18);
        assert_eq!(count(|k| matches!(k, InertKind::Enum(_))), 19);
        assert_eq!(count(|k| matches!(k, InertKind::Str)), 17);
        let after = RESTART_ONLY_GUCS
            .iter()
            .filter(|r| r.reason == RestartReason::AfterConnectionStart)
            .count();
        assert_eq!(after, 6);
    }

    #[test]
    fn names_are_unique_and_do_not_overlap() {
        let mut seen = std::collections::HashSet::new();
        for n in INERT_GUCS
            .iter()
            .map(|g| g.name)
            .chain(RESTART_ONLY_GUCS.iter().map(|r| r.name))
        {
            assert_eq!(n, n.to_ascii_lowercase(), "{n}");
            assert!(seen.insert(n), "duplicate {n}");
        }
        // 意味を持つ設定は一覧に入れない。
        for d in SETTINGS {
            let lower = d.name.to_ascii_lowercase();
            assert!(
                !INERT_GUCS.iter().any(|g| g.name == lower),
                "{} is both meaningful and inert",
                d.name
            );
        }
        for n in [
            "enable_seqscan",
            "enable_indexscan",
            "enable_hashjoin",
            "enable_nestloop",
            "enable_hashagg",
            "enable_sort",
            "enable_material",
            "yuzhu.query_mem_limit",
            "yuzhu.validate_plans",
        ] {
            assert!(lookup(n).is_some(), "{n}");
        }
        for n in ["work_mem", "maintenance_work_mem", "transaction_timeout"] {
            assert!(inert_lookup(n).is_some(), "{n}");
            assert!(lookup(n).is_none(), "{n}");
        }
    }

    #[test]
    fn every_default_is_accepted_and_shown() {
        let st = Settings::new("u", &[]);
        for g in INERT_GUCS {
            let shown = st.show(g.name).unwrap().1;
            assert_eq!(shown, inert_default(g), "{}", g.name);
            // 表示した値をそのまま SET し直せる（tcp_keepalives_* の単位なし表示を含む）。
            let mut st2 = Settings::new("u", &[]);
            let v = shown;
            st2.set(g.name, Some(std::slice::from_ref(&v)), false)
                .unwrap_or_else(|e| panic!("{}: {v}: {}", g.name, e.message));
            assert_eq!(st2.get(g.name), v, "{}", g.name);
        }
        // PostgreSQL 17.11 の SHOW の実機の値。
        for (n, v) in [
            ("work_mem", "4MB"),
            ("maintenance_work_mem", "64MB"),
            ("effective_cache_size", "4GB"),
            ("temp_buffers", "8MB"),
            ("max_stack_depth", "2MB"),
            ("wal_sender_timeout", "1min"),
            ("tcp_keepalives_idle", "7200"),
            ("tcp_keepalives_interval", "75"),
            ("tcp_keepalives_count", "9"),
            ("log_min_duration_statement", "-1"),
            ("log_parameter_max_length", "-1"),
            ("log_temp_files", "-1"),
            ("random_page_cost", "4"),
            ("cpu_tuple_cost", "0.01"),
            ("vacuum_cost_delay", "0"),
            ("transaction_timeout", "0"),
            ("wal_compression", "off"),
            ("password_encryption", "scram-sha-256"),
            ("default_text_search_config", "pg_catalog.english"),
            ("geqo", "on"),
            ("vacuum_buffer_usage_limit", "2MB"),
            ("gin_pending_list_limit", "4MB"),
            ("wal_skip_threshold", "2MB"),
            ("io_combine_limit", "128kB"),
        ] {
            assert_eq!(st.show(n).unwrap().1, v, "{n}");
        }
    }

    #[test]
    fn integers_round_to_even_then_check_the_range() {
        let mut st = Settings::new("u", &[]);
        let n = "default_statistics_target";
        assert_eq!(set_show(&mut st, n, "5.5"), "6");
        assert_eq!(set_show(&mut st, n, "5.4"), "5");
        assert_eq!(set_show(&mut st, n, "2.5"), "2");
        assert_eq!(set_show(&mut st, n, "1.5"), "2");
        assert_eq!(set_show(&mut st, n, " 7 "), "7");
        assert_eq!(set_show(&mut st, "work_mem", "100.4"), "100kB");
        assert_eq!(set_show(&mut st, "work_mem", "1.5MB"), "1536kB");
        assert_eq!(set_show(&mut st, "work_mem", "5 MB"), "5MB");
        assert_eq!(set_show(&mut st, "work_mem", "1e3"), "1000kB");
        assert_eq!(set_show(&mut st, "work_mem", "0x100"), "256kB");
        assert_eq!(set_show(&mut st, "work_mem", "1500"), "1500kB");
        assert_eq!(set_show(&mut st, "work_mem", "1024"), "1MB");
        assert_eq!(set_show(&mut st, "work_mem", "2GB"), "2GB");
        assert_eq!(set_show(&mut st, "backend_flush_after", "1"), "8kB");
        assert_eq!(set_show(&mut st, "backend_flush_after", "1MB"), "1MB");
        assert_eq!(set_show(&mut st, "effective_cache_size", "100"), "800kB");
        assert_eq!(set_show(&mut st, "transaction_timeout", "1.5"), "2ms");
        assert_eq!(
            set_show(
                &mut st,
                "transaction_timeout",
                "'0.4ms'"[1..7].trim_end_matches('\'')
            ),
            "0"
        );
        assert_eq!(set_show(&mut st, "transaction_timeout", "1s"), "1s");
        assert_eq!(set_show(&mut st, "transaction_timeout", "90000"), "90s");
        assert_eq!(set_show(&mut st, "log_min_duration_statement", "1h"), "1h");
        assert_eq!(set_show(&mut st, "log_parameter_max_length", "1kB"), "1kB");
        assert_eq!(set_show(&mut st, "log_parameter_max_length", "-1"), "-1");
        assert_eq!(set_show(&mut st, "tcp_keepalives_idle", "1min"), "60");

        let e = set_err(&mut st, "work_mem", "63");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "63 kB is outside the valid range for parameter \"work_mem\" (64 kB .. 2147483647 kB)"
        );
        let e = set_err(&mut st, n, "0");
        assert_eq!(
            e.message,
            "0 is outside the valid range for parameter \"default_statistics_target\" (1 .. 10000)"
        );
        // 丸めた後の値で範囲を検査する。
        let e = set_err(&mut st, n, "0.4");
        assert!(e.message.starts_with("0 is outside"), "{}", e.message);
        let e = set_err(&mut st, "io_combine_limit", "4kB");
        assert_eq!(
            e.message,
            "0 8kB is outside the valid range for parameter \"io_combine_limit\" (1 8kB .. 32 8kB)"
        );
        let e = set_err(&mut st, "log_temp_files", "-5");
        assert_eq!(
            e.message,
            "-5 kB is outside the valid range for parameter \"log_temp_files\" (-1 kB .. 2147483647 kB)"
        );
    }

    #[test]
    fn malformed_integers_are_invalid_values() {
        let mut st = Settings::new("u", &[]);
        let e = set_err(&mut st, "work_mem", "abc");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "invalid value for parameter \"work_mem\": \"abc\""
        );
        assert_eq!(e.hint, None);
        let e = set_err(&mut st, "work_mem", "5xx");
        assert_eq!(
            e.hint.as_deref(),
            Some("Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".")
        );
        let e = set_err(&mut st, "wal_sender_timeout", "5xx");
        assert_eq!(
            e.hint.as_deref(),
            Some(
                "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\"."
            )
        );
        // 単位のない設定に単位をつけても、HINT はつかない。
        let e = set_err(&mut st, "default_statistics_target", "5MB");
        assert_eq!(
            e.message,
            "invalid value for parameter \"default_statistics_target\": \"5MB\""
        );
        assert_eq!(e.hint, None);
        for bad in ["", "true", "MB", "1 2"] {
            let e = set_err(&mut st, "default_statistics_target", bad);
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE, "{bad}");
        }
        for over in ["9999999999", "4TB"] {
            let e = set_err(&mut st, "work_mem", over);
            assert_eq!(
                e.hint.as_deref(),
                Some("Value exceeds integer range."),
                "{over}"
            );
        }
        let e = st
            .set("work_mem", Some(&s(&["1", "2"])), false)
            .unwrap_err();
        assert_eq!(e.message, "SET work_mem takes only one argument");
    }

    #[test]
    fn reals_and_units() {
        let mut st = Settings::new("u", &[]);
        assert_eq!(set_show(&mut st, "random_page_cost", "1.1"), "1.1");
        assert_eq!(
            set_show(&mut st, "random_page_cost", "0.1234567891"),
            "0.123457"
        );
        assert_eq!(set_show(&mut st, "cpu_tuple_cost", "1e-3"), "0.001");
        assert_eq!(set_show(&mut st, "jit_above_cost", "-1"), "-1");
        assert_eq!(set_show(&mut st, "vacuum_cost_delay", "0.5"), "500us");
        assert_eq!(set_show(&mut st, "vacuum_cost_delay", "1500us"), "1500us");
        assert_eq!(set_show(&mut st, "vacuum_cost_delay", "2"), "2ms");
        let e = set_err(&mut st, "random_page_cost", "-1");
        assert_eq!(
            e.message,
            "-1 is outside the valid range for parameter \"random_page_cost\" (0 .. 1.79769e+308)"
        );
        let e = set_err(&mut st, "geqo_seed", "2");
        assert_eq!(
            e.message,
            "2 is outside the valid range for parameter \"geqo_seed\" (0 .. 1)"
        );
        let e = set_err(&mut st, "recursive_worktable_factor", "0");
        assert_eq!(
            e.message,
            "0 is outside the valid range for parameter \"recursive_worktable_factor\" (0.001 .. 1e+06)"
        );
        let e = set_err(&mut st, "vacuum_cost_delay", "1s");
        assert_eq!(
            e.message,
            "1000 ms is outside the valid range for parameter \"vacuum_cost_delay\" (0 ms .. 100 ms)"
        );
        let e = set_err(&mut st, "cpu_tuple_cost", "inf");
        assert_eq!(
            e.message,
            "Infinity is outside the valid range for parameter \"cpu_tuple_cost\" (0 .. 1.79769e+308)"
        );
        for bad in ["x", "NaN", "1e400"] {
            let e = set_err(&mut st, "cpu_tuple_cost", bad);
            assert_eq!(
                e.message,
                format!("invalid value for parameter \"cpu_tuple_cost\": \"{bad}\"")
            );
        }
    }

    #[test]
    fn fmt_g_matches_printf() {
        for (v, want) in [
            (0.0, "0"),
            (1.0, "1"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123_456.0, "123456"),
            (1_234_567.0, "1.23457e+06"),
            (1e6, "1e+06"),
            (f64::MAX, "1.79769e+308"),
            (0.123_456_789_1, "0.123457"),
            (-2.5, "-2.5"),
            (100.0, "100"),
        ] {
            assert_eq!(fmt_g(v), want, "{v}");
        }
    }

    #[test]
    fn booleans() {
        let mut st = Settings::new("u", &[]);
        for (v, want) in [
            ("off", "off"),
            ("tr", "on"),
            ("YES", "on"),
            ("0", "off"),
            ("1", "on"),
            ("of", "off"),
            ("f", "off"),
        ] {
            assert_eq!(set_show(&mut st, "geqo", v), want, "{v}");
        }
        for bad in ["2", "o", "maybe", ""] {
            let e = set_err(&mut st, "geqo", bad);
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
            assert_eq!(e.message, "parameter \"geqo\" requires a Boolean value");
        }
        st.set("geqo", None, false).unwrap();
        assert_eq!(st.get("geqo"), "on");
    }

    #[test]
    fn enums_are_stored_by_their_canonical_name() {
        let mut st = Settings::new("u", &[]);
        // PostgreSQL 17.11 の実機。同義語は、同じ値を持つ最初の名前に正規化される。
        for (n, v, want) in [
            ("wal_compression", "on", "pglz"),
            ("wal_compression", "yes", "pglz"),
            ("wal_compression", "TRUE", "pglz"),
            ("wal_compression", "1", "pglz"),
            ("wal_compression", "off", "off"),
            ("wal_compression", "no", "off"),
            ("wal_compression", "0", "off"),
            ("wal_compression", "LZ4", "lz4"),
            ("backslash_quote", "true", "on"),
            ("backslash_quote", "false", "off"),
            ("backslash_quote", "safe_encoding", "safe_encoding"),
            ("constraint_exclusion", "yes", "on"),
            ("constraint_exclusion", "Partition", "partition"),
            ("compute_query_id", "1", "on"),
            ("debug_parallel_query", "0", "off"),
            ("log_statement", "ddl", "ddl"),
            ("password_encryption", "md5", "md5"),
        ] {
            assert_eq!(set_show(&mut st, n, v), want, "{n} = {v}");
        }
        // `on` / `off` を持たない enum は同義語を受け付けない。
        let e = set_err(&mut st, "log_statement", "on");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "invalid value for parameter \"log_statement\": \"on\""
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Available values: none, ddl, mod, all.")
        );
        let e = set_err(&mut st, "wal_compression", "foo");
        assert_eq!(
            e.hint.as_deref(),
            Some("Available values: pglz, lz4, zstd, on, off.")
        );
        // RESET は既定値の正規名に戻す。
        st.set("wal_compression", None, false).unwrap();
        assert_eq!(st.get("wal_compression"), "off");
    }

    #[test]
    fn strings_are_not_checked() {
        let mut st = Settings::new("u", &[]);
        assert_eq!(set_show(&mut st, "lc_time", "ja_JP.UTF-8"), "ja_JP.UTF-8");
        assert_eq!(set_show(&mut st, "temp_tablespaces", ""), "");
        assert_eq!(
            st.show("output_plugin_libraries").unwrap().1,
            "pgoutput, test_decoding"
        );
        st.set("session_preload_libraries", Some(&s(&["a", "b"])), false)
            .unwrap();
        assert_eq!(st.get("session_preload_libraries"), "a, b");
    }

    #[test]
    fn restart_only_names_cannot_be_set() {
        let mut st = Settings::new("u", &[]);
        for (n, msg) in [
            (
                "max_connections",
                "parameter \"max_connections\" cannot be changed without restarting the server",
            ),
            (
                "autovacuum",
                "parameter \"autovacuum\" cannot be changed now",
            ),
            ("block_size", "parameter \"block_size\" cannot be changed"),
            (
                "log_connections",
                "parameter \"log_connections\" cannot be set after connection start",
            ),
            (
                "post_auth_delay",
                "parameter \"post_auth_delay\" cannot be set after connection start",
            ),
            (
                "shared_buffers",
                "parameter \"shared_buffers\" cannot be changed without restarting the server",
            ),
            (
                "max_wal_size",
                "parameter \"max_wal_size\" cannot be changed now",
            ),
        ] {
            let e = set_err(&mut st, n, "1");
            assert_eq!(e.sqlstate, sqlstate::CANT_CHANGE_RUNTIME_PARAM, "{n}");
            assert_eq!(e.message, msg, "{n}");
            // RESET も同じ。
            assert_eq!(st.reset(n).unwrap_err().message, msg, "{n}");
        }
        // SHOW は既定値を返す。
        assert_eq!(st.show("autovacuum").unwrap().1, "on");
        assert_eq!(st.show("log_connections").unwrap().1, "off");
        assert_eq!(st.show("port").unwrap().1, "5432");
        assert_eq!(st.show("wal_level").unwrap().1, "replica");
        assert_eq!(st.show("data_directory").unwrap().1, "");
    }

    #[test]
    fn every_restart_only_name_is_rejected() {
        let mut st = Settings::new("u", &[]);
        for r in RESTART_ONLY_GUCS {
            let e = st.set(r.name, Some(&s(&["1"])), false).expect_err(r.name);
            assert_eq!(
                e.sqlstate,
                sqlstate::CANT_CHANGE_RUNTIME_PARAM,
                "{}",
                r.name
            );
            let want = restart_error(r.name, r.reason);
            assert_eq!(e.message, want.message, "{}", r.name);
        }
    }

    #[test]
    fn unknown_names() {
        let mut st = Settings::new("u", &[]);
        for n in ["nosuch_guc", "enable_foo", "enable_"] {
            let e = set_err(&mut st, n, "on");
            assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT, "{n}");
            assert_eq!(
                e.message,
                format!("unrecognized configuration parameter \"{n}\"")
            );
            assert!(st.show(n).is_err());
        }
        // カスタム名は任意。
        assert_eq!(set_show(&mut st, "my.custom", "1"), "1");
        assert!(is_known("WORK_MEM"));
        assert!(is_known("autovacuum"));
        assert!(!is_known("enable_foo"));
    }

    #[test]
    fn names_are_case_insensitive_and_canonical() {
        let mut st = Settings::new("u", &[]);
        assert_eq!(set_show(&mut st, "GEQO", "off"), "off");
        assert_eq!(
            st.show("Work_Mem").unwrap(),
            ("work_mem".into(), "4MB".into())
        );
    }

    #[test]
    fn transactional_and_reset() {
        let mut st = Settings::new("u", &[]);
        st.begin();
        st.set("work_mem", Some(&s(&["8MB"])), false).unwrap();
        st.set("geqo", Some(&s(&["off"])), true).unwrap();
        assert_eq!(st.get("geqo"), "off");
        st.rollback();
        assert_eq!(st.get("work_mem"), "4MB");
        assert_eq!(st.get("geqo"), "on");
        st.begin();
        st.set("work_mem", Some(&s(&["8MB"])), false).unwrap();
        st.set("geqo", Some(&s(&["off"])), true).unwrap();
        st.commit();
        assert_eq!(st.get("work_mem"), "8MB");
        assert_eq!(st.get("geqo"), "on");
        st.reset("work_mem").unwrap();
        assert_eq!(st.get("work_mem"), "4MB");
        st.set("random_page_cost", Some(&s(&["1.1"])), false)
            .unwrap();
        st.set("wal_compression", Some(&s(&["lz4"])), false)
            .unwrap();
        st.reset_all();
        assert_eq!(st.get("random_page_cost"), "4");
        assert_eq!(st.get("wal_compression"), "off");
    }

    #[test]
    fn startup_options_apply_to_inert_gucs() {
        let st = Settings::new(
            "u",
            &[
                ("work_mem".into(), "16MB".into()),
                ("geqo".into(), "off".into()),
                ("random_page_cost".into(), "bad".into()),
                ("max_connections".into(), "5".into()),
            ],
        );
        assert_eq!(st.get("work_mem"), "16MB");
        assert_eq!(st.get("geqo"), "off");
        assert_eq!(st.get("random_page_cost"), "4");
        // RESET の戻り先は起動時の値。
        let mut st = st;
        st.set("work_mem", Some(&s(&["1MB"])), false).unwrap();
        st.reset("work_mem").unwrap();
        assert_eq!(st.get("work_mem"), "16MB");
    }

    #[test]
    fn transaction_timeout_is_accepted() {
        let mut st = Settings::new("u", &[]);
        assert_eq!(set_show(&mut st, "transaction_timeout", "5s"), "5s");
        assert_eq!(set_show(&mut st, "transaction_timeout", "0"), "0");
        let e = set_err(&mut st, "transaction_timeout", "-1");
        assert_eq!(
            e.message,
            "-1 ms is outside the valid range for parameter \"transaction_timeout\" (0 ms .. 2147483647 ms)"
        );
    }

    #[test]
    fn show_all_lists_every_name() {
        let st = Settings::new("u", &[]);
        let all = st.show_all();
        let names: std::collections::HashSet<_> = all.iter().map(|(n, _, _)| n.as_str()).collect();
        for g in INERT_GUCS {
            assert!(names.contains(g.name), "{}", g.name);
        }
        assert!(names.contains("autovacuum"));
        assert!(names.contains("yuzhu.query_mem_limit"));
        let sorted: Vec<_> = all.iter().map(|(n, _, _)| n.to_ascii_lowercase()).collect();
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
        assert!(all.len() >= 162 + 188);
    }

    #[test]
    fn planner_settings_follow_the_enable_gucs() {
        let mut st = Settings::new("u", &[]);
        let p = st.planner_settings();
        assert!(p.enable_seqscan && p.enable_indexscan && p.enable_hashjoin);
        assert!(p.enable_nestloop && p.enable_hashagg && p.enable_sort && p.enable_material);
        assert_eq!(p.query_mem_limit, 256 * 1024 * 1024);
        assert_eq!(p.validate_plans, cfg!(debug_assertions));
        for n in [
            "enable_seqscan",
            "enable_indexscan",
            "enable_hashjoin",
            "enable_nestloop",
            "enable_hashagg",
            "enable_sort",
            "enable_material",
        ] {
            st.set(n, Some(&s(&["off"])), false).unwrap();
        }
        st.set("yuzhu.query_mem_limit", Some(&s(&["1MB"])), false)
            .unwrap();
        st.set("yuzhu.validate_plans", Some(&s(&["on"])), false)
            .unwrap();
        let p = st.planner_settings();
        assert!(!p.enable_seqscan && !p.enable_indexscan && !p.enable_hashjoin);
        assert!(!p.enable_nestloop && !p.enable_hashagg && !p.enable_sort && !p.enable_material);
        assert_eq!(p.query_mem_limit, 1024 * 1024);
        assert!(p.validate_plans);
        assert_eq!(st.get("yuzhu.query_mem_limit"), "1MB");
        let e = set_err(&mut st, "yuzhu.query_mem_limit", "1kB");
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        st.reset_all();
        assert_eq!(st.planner_settings().query_mem_limit, 256 * 1024 * 1024);
    }
}
