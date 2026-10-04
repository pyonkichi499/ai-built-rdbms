//! PostgreSQL frontend/backend protocol (version 3.0).
//!
//! Protocol message names (StartupMessage, SSLRequest, ...) appear in docs
//! as plain words, as in the PostgreSQL documentation.

#![allow(clippy::doc_markdown)]

pub mod codec;
pub mod messages;
