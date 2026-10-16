//! Small shared helpers.

pub mod clock;
pub mod crc32c;
pub mod sync;
pub mod sync_point;

/// Writes a `WARNING:` line to stderr. yuzhu-core has no logging facility yet,
/// so this is the single place that is allowed to print.
#[allow(clippy::print_stderr)]
pub(crate) fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}
