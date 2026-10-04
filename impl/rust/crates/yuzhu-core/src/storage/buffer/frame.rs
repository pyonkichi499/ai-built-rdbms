//! Buffer frames: header (tag, flags, pin/usage counts), content latch and
//! the I/O condition variable (`m2.md` §6.3 items 1, 4, 5).
//!
//! 担当 C が実装する。
