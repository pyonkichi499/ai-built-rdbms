//! yuzhu-core: core library of the yuzhu RDBMS.
//!
//! Module layout and dependency direction follow `spec/design/m1.md` §2:
//! `error`, `types` ← `sql` ← `catalog` ← `analyzer` ← `planner` ←
//! `executor` ← `session` / `engine`. `storage`, `txn` and `settings`
//! depend only on `error`, `types` and `catalog`.

#![forbid(unsafe_code)]

pub mod analyzer;
pub mod catalog;
pub mod engine;
pub mod error;
pub mod executor;
pub mod planner;
pub mod session;
pub mod settings;
pub mod sql;
pub mod storage;
pub mod txn;
pub mod types;

pub use engine::{Database, DatabaseConfig};
pub use error::{Error, Result, Severity, Span, SqlState, sqlstate};
pub use session::{ColumnDesc, Notice, ResultSink, Session, StartupParams, TransactionStatus};
pub use types::Oid;
