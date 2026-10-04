//! Error type shared by every layer. Every error carries a SQLSTATE.

use std::fmt;

/// A five-character SQLSTATE code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SqlState(pub &'static str);

impl SqlState {
    /// The five-character code.
    pub fn code(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// SQLSTATE constants (names follow PostgreSQL's `errcodes.txt`).
pub mod sqlstate {
    use super::SqlState;
    // Class 00 / 01
    pub const SUCCESSFUL_COMPLETION: SqlState = SqlState("00000");
    pub const WARNING: SqlState = SqlState("01000");
    // Class 08 - connection exception
    pub const CONNECTION_EXCEPTION: SqlState = SqlState("08000");
    pub const PROTOCOL_VIOLATION: SqlState = SqlState("08P01");
    // Class 0A
    pub const FEATURE_NOT_SUPPORTED: SqlState = SqlState("0A000");
    // Class 21
    pub const CARDINALITY_VIOLATION: SqlState = SqlState("21000");
    // Class 22 - data exception
    pub const DATA_EXCEPTION: SqlState = SqlState("22000");
    pub const STRING_DATA_RIGHT_TRUNCATION: SqlState = SqlState("22001");
    pub const NUMERIC_VALUE_OUT_OF_RANGE: SqlState = SqlState("22003");
    pub const NULL_VALUE_NOT_ALLOWED: SqlState = SqlState("22004");
    pub const DIVISION_BY_ZERO: SqlState = SqlState("22012");
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: SqlState = SqlState("2201W");
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: SqlState = SqlState("2201X");
    pub const CHARACTER_NOT_IN_REPERTOIRE: SqlState = SqlState("22021");
    pub const INVALID_PARAMETER_VALUE: SqlState = SqlState("22023");
    pub const INVALID_ESCAPE_SEQUENCE: SqlState = SqlState("22025");
    pub const INVALID_TEXT_REPRESENTATION: SqlState = SqlState("22P02");
    // Class 23 - integrity constraint violation
    pub const INTEGRITY_CONSTRAINT_VIOLATION: SqlState = SqlState("23000");
    pub const NOT_NULL_VIOLATION: SqlState = SqlState("23502");
    pub const FOREIGN_KEY_VIOLATION: SqlState = SqlState("23503");
    pub const UNIQUE_VIOLATION: SqlState = SqlState("23505");
    pub const CHECK_VIOLATION: SqlState = SqlState("23514");
    // Class 25 - invalid transaction state
    pub const INVALID_TRANSACTION_STATE: SqlState = SqlState("25000");
    pub const ACTIVE_SQL_TRANSACTION: SqlState = SqlState("25001");
    pub const READ_ONLY_SQL_TRANSACTION: SqlState = SqlState("25006");
    pub const NO_ACTIVE_SQL_TRANSACTION: SqlState = SqlState("25P01");
    pub const IN_FAILED_SQL_TRANSACTION: SqlState = SqlState("25P02");
    // Class 28
    pub const INVALID_AUTHORIZATION_SPECIFICATION: SqlState = SqlState("28000");
    pub const INVALID_PASSWORD: SqlState = SqlState("28P01");
    // Class 3D
    pub const INVALID_CATALOG_NAME: SqlState = SqlState("3D000");
    // Class 3F
    pub const INVALID_SCHEMA_NAME: SqlState = SqlState("3F000");
    // Class 40
    pub const SERIALIZATION_FAILURE: SqlState = SqlState("40001");
    pub const DEADLOCK_DETECTED: SqlState = SqlState("40P01");
    // Class 42 - syntax error or access rule violation
    pub const SYNTAX_ERROR: SqlState = SqlState("42601");
    pub const INSUFFICIENT_PRIVILEGE: SqlState = SqlState("42501");
    pub const INVALID_NAME: SqlState = SqlState("42602");
    pub const NAME_TOO_LONG: SqlState = SqlState("42622");
    pub const UNDEFINED_TABLE: SqlState = SqlState("42P01");
    pub const UNDEFINED_PARAMETER: SqlState = SqlState("42P02");
    pub const UNDEFINED_COLUMN: SqlState = SqlState("42703");
    pub const UNDEFINED_FUNCTION: SqlState = SqlState("42883");
    pub const UNDEFINED_OBJECT: SqlState = SqlState("42704");
    pub const DUPLICATE_TABLE: SqlState = SqlState("42P07");
    pub const DUPLICATE_COLUMN: SqlState = SqlState("42701");
    pub const DUPLICATE_OBJECT: SqlState = SqlState("42710");
    pub const AMBIGUOUS_COLUMN: SqlState = SqlState("42702");
    pub const AMBIGUOUS_FUNCTION: SqlState = SqlState("42725");
    pub const DATATYPE_MISMATCH: SqlState = SqlState("42804");
    pub const WRONG_OBJECT_TYPE: SqlState = SqlState("42809");
    pub const CANNOT_COERCE: SqlState = SqlState("42846");
    pub const INVALID_COLUMN_REFERENCE: SqlState = SqlState("42P10");
    pub const INVALID_TABLE_DEFINITION: SqlState = SqlState("42P16");
    pub const INDETERMINATE_DATATYPE: SqlState = SqlState("42P18");
    pub const GROUPING_ERROR: SqlState = SqlState("42803");
    // Class 53 / 54 - resources / limits
    pub const OUT_OF_MEMORY: SqlState = SqlState("53200");
    pub const TOO_MANY_CONNECTIONS: SqlState = SqlState("53300");
    pub const PROGRAM_LIMIT_EXCEEDED: SqlState = SqlState("54000");
    // Class 55 / 57
    pub const OBJECT_IN_USE: SqlState = SqlState("55006");
    pub const CANT_CHANGE_RUNTIME_PARAM: SqlState = SqlState("55P02");
    pub const QUERY_CANCELED: SqlState = SqlState("57014");
    pub const ADMIN_SHUTDOWN: SqlState = SqlState("57P01");
    // Class XX
    pub const INTERNAL_ERROR: SqlState = SqlState("XX000");
}

/// Message severity (the `S`/`V` fields of `ErrorResponse` / `NoticeResponse`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Fatal,
    Panic,
    Warning,
    Notice,
    Debug,
    Info,
    Log,
}

impl Severity {
    /// The non-localized severity string as sent on the wire (`ERROR`, `WARNING`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Fatal => "FATAL",
            Severity::Panic => "PANIC",
            Severity::Warning => "WARNING",
            Severity::Notice => "NOTICE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Log => "LOG",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A source range in the query text, as **byte** offsets (`start..end`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// The smallest span covering both `self` and `other`.
    #[must_use]
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Error {
    /// Usually `Severity::Error`.
    pub severity: Severity,
    pub sqlstate: SqlState,
    /// Same wording as PostgreSQL (lower-case start, no trailing period).
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// Position in the query string: 1-based, counted in characters.
    pub position: Option<u32>,
    /// Byte offset into the query text recorded by the parser / analyzer.
    /// `Error::resolve_position` turns it into `position` (the session does
    /// this once per query, since only it knows the query text).
    pub cursor_byte: Option<u32>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(sqlstate: SqlState, message: impl Into<String>) -> Self {
        Error {
            severity: Severity::Error,
            sqlstate,
            message: message.into(),
            detail: None,
            hint: None,
            position: None,
            cursor_byte: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }

    #[must_use]
    pub fn with_hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }

    /// Sets the 1-based character position directly.
    #[must_use]
    pub fn with_position(mut self, p: u32) -> Self {
        self.position = Some(p);
        self
    }

    /// Records the start of `span` (a byte offset) as the error cursor,
    /// unless a cursor is already set (the innermost location wins).
    #[must_use]
    pub fn with_span(mut self, span: Span) -> Self {
        if self.cursor_byte.is_none() && self.position.is_none() {
            self.cursor_byte = Some(span.start);
        }
        self
    }

    #[must_use]
    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// Syntax error at a 1-based character position.
    pub fn syntax(position: u32, message: impl Into<String>) -> Self {
        Error::new(sqlstate::SYNTAX_ERROR, message).with_position(position)
    }

    /// Syntax error at a byte span (preferred inside the parser).
    pub fn syntax_at(span: Span, message: impl Into<String>) -> Self {
        Error::new(sqlstate::SYNTAX_ERROR, message).with_span(span)
    }

    /// `0A000`. `what` is the full message, e.g. `"UPDATE is not supported yet"`.
    pub fn not_supported(what: impl Into<String>) -> Self {
        Error::new(sqlstate::FEATURE_NOT_SUPPORTED, what)
    }

    /// `XX000`; use only for broken invariants that are not worth a panic.
    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(sqlstate::INTERNAL_ERROR, message)
    }

    /// Converts `cursor_byte` into a 1-based character `position` using the
    /// query text the byte offset refers to. No-op if `position` is set.
    pub fn resolve_position(&mut self, sql: &str) {
        if self.position.is_none()
            && let Some(b) = self.cursor_byte
        {
            self.position = Some(byte_offset_to_position(sql, b));
        }
        self.cursor_byte = None;
    }
}

/// Converts a byte offset into a 1-based character position.
/// Offsets past the end map to `chars + 1` (PostgreSQL's "end of input").
pub fn byte_offset_to_position(sql: &str, byte: u32) -> u32 {
    let byte = byte as usize;
    let chars = sql.char_indices().take_while(|(i, _)| *i < byte).count();
    u32::try_from(chars).unwrap_or(u32::MAX - 1) + 1
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_and_display() {
        let e = Error::new(sqlstate::UNDEFINED_TABLE, "relation \"t\" does not exist")
            .with_detail("d")
            .with_hint("h")
            .with_position(15);
        assert_eq!(e.to_string(), "relation \"t\" does not exist");
        assert_eq!(e.sqlstate.code(), "42P01");
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.detail.as_deref(), Some("d"));
        assert_eq!(e.hint.as_deref(), Some("h"));
        assert_eq!(e.position, Some(15));
    }

    #[test]
    fn helpers() {
        assert_eq!(
            Error::not_supported("x").sqlstate,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        assert_eq!(Error::internal("x").sqlstate, sqlstate::INTERNAL_ERROR);
        let s = Error::syntax(3, "syntax error at or near \"x\"");
        assert_eq!(s.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(s.position, Some(3));
        assert_eq!(Severity::Warning.as_str(), "WARNING");
    }

    #[test]
    fn span_position_resolution() {
        // "é" is two bytes: "x" starts at byte 12 (0-based) = char index 11 -> position 12.
        let sql = "select 'é' x";
        let off = u32::try_from(sql.find('x').unwrap()).unwrap();
        let mut e = Error::syntax_at(Span::new(off, off + 1), "m");
        assert_eq!(e.position, None);
        e.resolve_position(sql);
        assert_eq!(e.position, Some(12));
        assert_eq!(e.cursor_byte, None);
        // Innermost span wins.
        let e = Error::new(sqlstate::SYNTAX_ERROR, "m")
            .with_span(Span::new(1, 2))
            .with_span(Span::new(5, 6));
        assert_eq!(e.cursor_byte, Some(1));
        // End of input.
        assert_eq!(byte_offset_to_position("abc", 3), 4);
        assert_eq!(byte_offset_to_position("abc", 0), 1);
        assert_eq!(Span::new(3, 5).to(Span::new(1, 4)), Span::new(1, 5));
    }
}
