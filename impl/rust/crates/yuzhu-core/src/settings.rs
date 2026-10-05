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
    /// `client_encoding`: only UTF8.
    Encoding,
    /// `DateStyle`.
    DateStyle,
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
        Kind::Str,
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
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: i64 = num.parse().map_err(|_| invalid_value(name, raw))?;
    let mult = match unit.trim() {
        "" | "ms" => 1,
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
    let v = n
        .checked_mul(mult)
        .ok_or_else(|| invalid_value(name, raw))?;
    if !(min..=i64::from(i32::MAX)).contains(&v) {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "{v} ms is outside the valid range for parameter \"{name}\" ({min} .. 2147483647)"
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

fn normalize_datestyle(current: &str, raw: &str) -> Option<String> {
    let mut parts = current.split(',').map(str::trim);
    let mut style = parts.next().unwrap_or("ISO").to_owned();
    let mut order = parts.next().unwrap_or("MDY").to_owned();
    for tok in raw.split(',').map(|t| t.trim().to_ascii_lowercase()) {
        match tok.as_str() {
            "iso" => style = "ISO".into(),
            "postgres" => style = "Postgres".into(),
            "sql" => style = "SQL".into(),
            "german" => style = "German".into(),
            "mdy" | "us" | "noneuropean" | "non-european" => order = "MDY".into(),
            "dmy" | "european" | "euro" => order = "DMY".into(),
            "ymd" => order = "YMD".into(),
            "default" => {
                style = "ISO".into();
                order = "MDY".into();
            }
            _ => return None,
        }
    }
    Some(format!("{style}, {order}"))
}

/// The error of a parameter that cannot be changed by `SET`.
fn fixed_error(def: &SettingDef) -> Option<Error> {
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
            return normalize_datestyle(current, &args.join(","))
                .ok_or_else(|| invalid_value(name, &args.join(", ")));
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
            let v: i64 = raw.trim().parse().map_err(|_| invalid_value(name, raw))?;
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
                Err(invalid_value(name, raw)
                    .with_hint(format!("Available values: {}.", values.join(", "))))
            }
        }
        Kind::Encoding => {
            let v = raw.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "utf8" | "utf-8" | "unicode") {
                Ok("UTF8".into())
            } else {
                Err(invalid_value(name, raw))
            }
        }
        Kind::DateStyle => {
            normalize_datestyle(current, raw).ok_or_else(|| invalid_value(name, raw))
        }
    }
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
        for (name, value) in startup {
            let Some(d) = lookup(name) else { continue };
            let current = reset_values.get(&key(d.name)).cloned().unwrap_or_default();
            if let Ok(v) = normalize(d, &current, std::slice::from_ref(value)) {
                reset_values.insert(key(d.name), v);
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
            Some(d) => match args {
                None => {
                    if let Some(e) = fixed_error(d) {
                        return Err(e);
                    }
                    self.reset_values.get(&k).cloned().unwrap_or_default()
                }
                Some(a) => normalize(d, self.get(name), a)?,
            },
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
            self.session = start;
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
        assert!(
            st.set("client_encoding", Some(&s(&["latin1"])), false)
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
