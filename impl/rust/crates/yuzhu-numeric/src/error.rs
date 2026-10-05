//! Error type carrying a PostgreSQL SQLSTATE.

use std::fmt;

/// SQLSTATE codes produced by this crate.
pub mod sqlstate {
    /// `invalid_text_representation`
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    /// `numeric_value_out_of_range`
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    /// `division_by_zero`
    pub const DIVISION_BY_ZERO: &str = "22012";
    /// `feature_not_supported`
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    /// `invalid_parameter_value`
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    /// `invalid_binary_representation`
    pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
    /// `invalid_argument_for_logarithm`
    pub const INVALID_ARGUMENT_FOR_LOG: &str = "2201E";
    /// `invalid_argument_for_power_function`
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
    /// `protocol_violation`
    pub const PROTOCOL_VIOLATION: &str = "08P01";
}

/// An error raised by a numeric operation, with the SQLSTATE, primary
/// message and optional DETAIL exactly as PostgreSQL reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumericError {
    sqlstate: &'static str,
    message: String,
    detail: Option<String>,
}

impl NumericError {
    pub(crate) fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
            detail: None,
        }
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Five-character SQLSTATE code.
    pub fn sqlstate(&self) -> &'static str {
        self.sqlstate
    }

    /// Primary error message (PostgreSQL's `errmsg`).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Optional detail message (PostgreSQL's `errdetail`).
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub(crate) fn invalid_syntax(input: &str) -> Self {
        Self::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type numeric: \"{input}\""),
        )
    }

    pub(crate) fn overflow() -> Self {
        Self::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value overflows numeric format",
        )
    }

    pub(crate) fn division_by_zero() -> Self {
        Self::new(sqlstate::DIVISION_BY_ZERO, "division by zero")
    }
}

impl fmt::Display for NumericError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NumericError {}
