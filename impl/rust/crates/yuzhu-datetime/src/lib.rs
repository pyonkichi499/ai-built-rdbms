//! yuzhu-datetime: PostgreSQL-compatible `date`, `timestamp`,
//! `timestamptz` and `interval`.
//!
//! * Internal representations match PostgreSQL exactly: `date` is days
//!   since 2000-01-01 (`i32`), `timestamp`/`timestamptz` are microseconds
//!   since 2000-01-01 00:00:00 (`i64`, UTC for `timestamptz`), `interval`
//!   is `(months: i32, days: i32, micros: i64)`. The extreme values encode
//!   `-infinity` / `infinity`.
//! * Text input is a hand-written port of PostgreSQL's `ParseDateTime` /
//!   `DecodeDateTime` / `DecodeInterval` / `DecodeISO8601Interval`,
//!   honouring the `DateStyle` field order (MDY/DMY/YMD).
//! * Output supports every `DateStyle` (ISO, SQL, Postgres, German) and
//!   `IntervalStyle`.
//! * Time zones are read from `TZif` files (`/usr/share/zoneinfo` by
//!   default) by a hand-written parser that ports PostgreSQL's
//!   `localtime.c`, through the [`ZoneSource`] abstraction. Without tzdata,
//!   UTC, POSIX zone strings and fixed offsets still work.
//! * Arithmetic, `extract` / `date_part` / `date_trunc` and typmod rounding
//!   follow `timestamp.c` / `date.c`.
//!
//! Errors carry PostgreSQL's SQLSTATE and message text.

#![forbid(unsafe_code)]
// This crate is a line-by-line port of PostgreSQL's C date/time code, whose
// exact integer semantics (truncating casts between int widths, wrapping
// Julian-day arithmetic) are part of the observable behaviour, and whose
// long decode functions and short C variable names are kept recognisable
// on purpose.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]

mod calendar;
mod cnum;
mod date;
mod error;
mod fields;
mod format;
mod interval;
mod parse;
mod settings;
mod timestamp;
mod tokens;
mod tz;

pub use calendar::MAX_PRECISION;
pub use date::Date;
pub use error::{DateTimeError, Result, sqlstate};
pub use fields::{
    NumericValue, date_part_date, date_part_interval, date_part_timestamp, date_part_timestamptz,
    date_trunc_interval, date_trunc_timestamp, date_trunc_timestamptz, extract_date,
    extract_interval, extract_timestamp, extract_timestamptz,
};
pub use format::format_float8;
pub use interval::{Interval, interval_typmod, range as interval_range};
pub use settings::{
    DateOrder, DateStyle, DateTimeEnv, IntervalStyle, format_datestyle, format_intervalstyle,
    parse_datestyle, parse_intervalstyle, parse_timezone_setting,
};
pub use timestamp::{MICROS_PER_DAY, Timestamp, TimestampTz};
pub use tz::{FsZoneSource, MemZoneSource, TimeZone, ZoneDb, ZoneSource};
