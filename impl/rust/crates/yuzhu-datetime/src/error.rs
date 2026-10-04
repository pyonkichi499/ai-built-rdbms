//! Errors carrying a SQLSTATE, mirroring the `ereport` calls of PostgreSQL's
//! date/time code.

use std::fmt;

/// SQLSTATE codes produced by this crate.
pub mod sqlstate {
    /// `invalid_datetime_format`
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    /// `datetime_field_overflow` (also `datetime_value_out_of_range`)
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    /// `invalid_time_zone_displacement_value`
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
    /// `interval_field_overflow`
    pub const INTERVAL_FIELD_OVERFLOW: &str = "22015";
    /// `division_by_zero`
    pub const DIVISION_BY_ZERO: &str = "22012";
    /// `invalid_parameter_value`
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    /// `feature_not_supported`
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    /// `internal_error`
    pub const INTERNAL_ERROR: &str = "XX000";
    /// `config_file_error`
    pub const CONFIG_FILE_ERROR: &str = "F0000";
}

/// A date/time error with PostgreSQL-compatible SQLSTATE and wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateTimeError {
    /// Five-character SQLSTATE.
    pub sqlstate: &'static str,
    /// Primary message (PostgreSQL wording).
    pub message: String,
    /// Optional DETAIL.
    pub detail: Option<String>,
    /// Optional HINT.
    pub hint: Option<String>,
}

impl DateTimeError {
    pub(crate) fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
            detail: None,
            hint: None,
        }
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub(crate) fn timestamp_out_of_range() -> Self {
        Self::new(sqlstate::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
    }

    pub(crate) fn interval_out_of_range() -> Self {
        Self::new(sqlstate::DATETIME_FIELD_OVERFLOW, "interval out of range")
    }

    pub(crate) fn date_out_of_range() -> Self {
        Self::new(sqlstate::DATETIME_FIELD_OVERFLOW, "date out of range")
    }
}

impl fmt::Display for DateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DateTimeError {}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, DateTimeError>;

/// Internal decoding error codes (`DTERR_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DtErr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    IntervalOverflow,
    TzDispOverflow,
    BadTimezone(String),
    BadZoneAbbrev { timezone: String, abbrev: String },
}

impl DtErr {
    /// `DateTimeParseError`.
    pub(crate) fn report(self, input: &str, datatype: &str) -> DateTimeError {
        match self {
            DtErr::FieldOverflow => DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                format!("date/time field value out of range: \"{input}\""),
            ),
            DtErr::MdFieldOverflow => DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                format!("date/time field value out of range: \"{input}\""),
            )
            .with_hint("Perhaps you need a different \"datestyle\" setting."),
            DtErr::IntervalOverflow => DateTimeError::new(
                sqlstate::INTERVAL_FIELD_OVERFLOW,
                format!("interval field value out of range: \"{input}\""),
            ),
            DtErr::TzDispOverflow => DateTimeError::new(
                sqlstate::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
                format!("time zone displacement out of range: \"{input}\""),
            ),
            DtErr::BadTimezone(tz) => DateTimeError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("time zone \"{tz}\" not recognized"),
            ),
            DtErr::BadZoneAbbrev { timezone, abbrev } => DateTimeError::new(
                sqlstate::CONFIG_FILE_ERROR,
                format!("time zone \"{timezone}\" not recognized"),
            )
            .with_detail(format!(
                "This time zone name appears in the configuration file for time zone abbreviation \"{abbrev}\"."
            )),
            DtErr::BadFormat => DateTimeError::new(
                sqlstate::INVALID_DATETIME_FORMAT,
                format!("invalid input syntax for type {datatype}: \"{input}\""),
            ),
        }
    }
}
