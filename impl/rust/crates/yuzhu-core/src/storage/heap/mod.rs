//! Heap access: tuple format, visibility, insertion target selection and
//! scans (`m2.md` §6.5, §6.6).
//!
//! 担当 D が実装する。

pub mod hio;
pub mod lock;
pub mod modstat;
pub mod prune;
pub mod scan;
pub mod tuple;
pub mod vacuum;
pub mod visibility;
pub mod wal;
pub mod wal2;
pub mod xmax;
