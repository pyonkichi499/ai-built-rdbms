//! Authentication primitives for yuzhu: SCRAM-SHA-256 (server side and a test
//! client), `SASLprep` and OS randomness. Depends on no other yuzhu crate.

#![forbid(unsafe_code)]

pub mod digest;
pub mod random;
pub mod saslprep;
pub mod scram;
pub mod scram_client;

use std::fmt;

/// Failure of the authentication library. Mapping to `ErrorResponse` is up to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Authentication failed (proof mismatch, doomed exchange). 28P01. The reason is never shown to the client.
    Failed,
    /// Malformed or out-of-order message. 08P01.
    Protocol {
        message: &'static str,
        detail: Option<String>,
    },
    /// Unsupported feature (authzid, extensions). 0A000.
    Unsupported(&'static str),
    /// Invalid argument (e.g. zero iterations). 22023.
    InvalidParameter(String),
    /// The OS random source failed. XX000.
    Random(String),
}

impl AuthError {
    pub(crate) fn protocol(message: &'static str, detail: impl Into<String>) -> Self {
        AuthError::Protocol {
            message,
            detail: Some(detail.into()),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Failed => f.write_str("authentication failed"),
            AuthError::Protocol {
                message,
                detail: Some(d),
            } => write!(f, "{message}: {d}"),
            AuthError::Protocol {
                message,
                detail: None,
            } => f.write_str(message),
            AuthError::Unsupported(m) => f.write_str(m),
            AuthError::InvalidParameter(m) | AuthError::Random(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AuthError {}
