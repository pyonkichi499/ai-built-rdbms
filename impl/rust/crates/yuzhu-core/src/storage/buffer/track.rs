//! Thread-local tracking of pins, latches and the storage barrier, used to
//! detect leaks and re-entrancy in debug builds (`m2.md` §6.3 item 7,
//! §2 convention 9).
//!
//! 担当 C が実装する。
