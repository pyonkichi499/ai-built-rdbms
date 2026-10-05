//! yuzhu-core: core library of the yuzhu RDBMS.
//!
//! Module layout and dependency direction follow `spec/design/m2.md` §2
//! (M1: `spec/design/m1.md` §2): `error`, `types`, `util`, `interrupt` ←
//! `sql` ← `catalog` (types and static tables) ← `storage` / `control` /
//! `datadir` / `txn` ← `catalog` (store, cache, reader) ← `analyzer` ←
//! `planner` ← `executor` ← `checkpoint` / `recovery` / `bootstrap` / `engine` /
//! `session`.

#![forbid(unsafe_code)]

pub mod analyzer;
pub mod bootstrap;
pub mod catalog;
pub mod checkpoint;
pub mod control;
pub mod datadir;
pub mod debug_knobs;
pub mod engine;
pub mod error;
pub mod executor;
pub mod interrupt;
pub mod planner;
pub mod recovery;
pub mod session;
pub mod settings;
pub mod sql;
pub mod storage;
pub mod testing;
pub mod txn;
pub mod types;
pub mod util;
pub mod wal;

pub use debug_knobs::DebugKnobs;
pub use engine::{Cluster, ClusterOptions, DatabaseHandle};
pub use error::{Error, Result, Severity, Span, SqlState, sqlstate};
pub use interrupt::InterruptFlag;
pub use session::{ColumnDesc, Notice, ResultSink, Session, StartupParams, TransactionStatus};
pub use types::Oid;
