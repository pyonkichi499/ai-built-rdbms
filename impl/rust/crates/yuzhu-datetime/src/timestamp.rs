//! `timestamp` and `timestamptz`.

use crate::calendar::{
    EPOCH_DIFF_SECS, MAX_PRECISION, MONTHS_PER_YEAR, POSTGRES_EPOCH_JDATE, USECS_PER_DAY,
    USECS_PER_SEC, date2j, days_in_month, is_valid_timestamp, j2date, timestamp2tm_utc,
    tm2timestamp,
};
use crate::date::Date;
use crate::error::{DateTimeError, Result, sqlstate};
use crate::format::encode_date_time;
use crate::interval::Interval;
use crate::parse::{Decoded, MAXDATEFIELDS, MAXDATELEN, decode_date_time, parse_datetime};
use crate::settings::DateTimeEnv;
use crate::tokens::{Abbrev, DTK_DATE, DTK_EARLY, DTK_EPOCH, DTK_LATE, search_abbrev};
use crate::tz::TimeZone;

/// `timestamp without time zone`: microseconds since 2000-01-01 00:00:00
/// (a wall-clock value). `i64::MIN` / `i64::MAX` are `-infinity` /
/// `infinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

/// `timestamp with time zone`: microseconds since 2000-01-01 00:00:00 UTC.
/// `i64::MIN` / `i64::MAX` are `-infinity` / `infinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampTz(pub i64);

/// `SetEpochTimestamp`: 1970-01-01 00:00:00 as a timestamp.
pub(crate) const EPOCH_TIMESTAMP: i64 = -EPOCH_DIFF_SECS * USECS_PER_SEC;

/// `timestamptz_to_time_t`.
pub(crate) fn timestamptz_to_time_t(t: i64) -> i64 {
    t / USECS_PER_SEC + EPOCH_DIFF_SECS
}

/// `AdjustTimestampForTypmod`.
pub(crate) fn adjust_for_typmod(time: i64, typmod: i32) -> Result<i64> {
    const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1000, 100, 10, 1];
    const OFFSETS: [i64; 7] = [500_000, 50_000, 5000, 500, 50, 5, 0];
    if time == i64::MIN || time == i64::MAX || typmod == -1 || typmod == MAX_PRECISION {
        return Ok(time);
    }
    if !(0..=MAX_PRECISION).contains(&typmod) {
        return Err(DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("timestamp({typmod}) precision must be between 0 and {MAX_PRECISION}"),
        ));
    }
    #[allow(clippy::cast_sign_loss)]
    let i = typmod as usize;
    Ok(if time >= 0 {
        ((time + OFFSETS[i]) / SCALES[i]) * SCALES[i]
    } else {
        -((((-time) + OFFSETS[i]) / SCALES[i]) * SCALES[i])
    })
}

fn decode(input: &str, env: &DateTimeEnv<'_>, datatype: &str) -> Result<Decoded> {
    let fields =
        parse_datetime(input, MAXDATELEN + MAXDATEFIELDS).map_err(|e| e.report(input, datatype))?;
    decode_date_time(&fields, env).map_err(|e| e.report(input, datatype))
}

fn out_of_range_input(input: &str) -> DateTimeError {
    DateTimeError::new(
        sqlstate::DATETIME_FIELD_OVERFLOW,
        format!("timestamp out of range: \"{input}\""),
    )
}

/// Add an interval's month and day parts to a broken-down time, the way
/// `timestamp_pl_interval` does. Returns the adjusted value via `rebuild`.
fn add_months_to_tm(tm: &mut crate::calendar::Tm, months: i32) -> Result<()> {
    tm.mon = tm
        .mon
        .checked_add(months)
        .ok_or_else(DateTimeError::timestamp_out_of_range)?;
    if tm.mon > MONTHS_PER_YEAR {
        tm.year += (tm.mon - 1) / MONTHS_PER_YEAR;
        tm.mon = ((tm.mon - 1) % MONTHS_PER_YEAR) + 1;
    } else if tm.mon < 1 {
        tm.year += tm.mon / MONTHS_PER_YEAR - 1;
        tm.mon = tm.mon % MONTHS_PER_YEAR + MONTHS_PER_YEAR;
    }
    let dim = days_in_month(tm.year, tm.mon);
    if tm.mday > dim {
        tm.mday = dim;
    }
    Ok(())
}

fn add_days_to_tm(tm: &mut crate::calendar::Tm, days: i32, min_julian: i32) -> Result<()> {
    let julian = date2j(tm.year, tm.mon, tm.mday)
        .checked_add(days)
        .filter(|j| *j >= min_julian)
        .ok_or_else(DateTimeError::timestamp_out_of_range)?;
    let (y, m, d) = j2date(julian);
    tm.year = y;
    tm.mon = m;
    tm.mday = d;
    Ok(())
}

/// Infinity handling shared by the `+ interval` operators.
fn add_interval_infinities(ts: i64, span: &Interval) -> Option<Result<i64>> {
    if span.is_neg_infinity() {
        return Some(if ts == i64::MAX {
            Err(DateTimeError::timestamp_out_of_range())
        } else {
            Ok(i64::MIN)
        });
    }
    if span.is_infinity() {
        return Some(if ts == i64::MIN {
            Err(DateTimeError::timestamp_out_of_range())
        } else {
            Ok(i64::MAX)
        });
    }
    if ts == i64::MIN || ts == i64::MAX {
        return Some(Ok(ts));
    }
    None
}

fn finish_add(ts: i64, micros: i64) -> Result<i64> {
    let r = ts
        .checked_add(micros)
        .ok_or_else(DateTimeError::timestamp_out_of_range)?;
    if !is_valid_timestamp(r) {
        return Err(DateTimeError::timestamp_out_of_range());
    }
    Ok(r)
}

/// `timestamp_mi` core shared by both timestamp types.
fn timestamp_diff(dt1: i64, dt2: i64) -> Result<Interval> {
    let finite = |t: i64| t != i64::MIN && t != i64::MAX;
    if !finite(dt1) || !finite(dt2) {
        return if dt1 == i64::MIN {
            if dt2 == i64::MIN {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::NEG_INFINITY)
            }
        } else if dt1 == i64::MAX {
            if dt2 == i64::MAX {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::INFINITY)
            }
        } else if dt2 == i64::MIN {
            Ok(Interval::INFINITY)
        } else {
            Ok(Interval::NEG_INFINITY)
        };
    }
    let time = dt1
        .checked_sub(dt2)
        .ok_or_else(DateTimeError::interval_out_of_range)?;
    Interval {
        months: 0,
        days: 0,
        micros: time,
    }
    .justify_hours()
}

/// Resolution of a zone name given to `AT TIME ZONE` (`DecodeTimezoneName`).
enum ZoneSpec {
    /// Fixed offset, seconds east of UTC.
    Fixed(i32),
    Dynamic(TimeZone),
    Zone(TimeZone),
}

fn decode_timezone_name(name: &str, env: &DateTimeEnv<'_>) -> Result<ZoneSpec> {
    let lower: Vec<u8> = name
        .bytes()
        .take(63)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match search_abbrev(&lower) {
        Some((_, Abbrev::Tz(off) | Abbrev::Dtz(off))) => Ok(ZoneSpec::Fixed(off)),
        Some((token, Abbrev::Dyn(zone))) => match env.zones.lookup(zone) {
            Some(tz) => Ok(ZoneSpec::Dynamic(tz)),
            None => Err(crate::error::DtErr::BadZoneAbbrev {
                timezone: zone.to_owned(),
                abbrev: token.to_owned(),
            }
            .report("", "")),
        },
        None => env.zones.lookup(name).map(ZoneSpec::Zone).ok_or_else(|| {
            DateTimeError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("time zone \"{name}\" not recognized"),
            )
        }),
    }
}

impl Timestamp {
    /// `infinity`.
    pub const INFINITY: Timestamp = Timestamp(i64::MAX);
    /// `-infinity`.
    pub const NEG_INFINITY: Timestamp = Timestamp(i64::MIN);

    /// Whether the value is finite.
    pub fn is_finite(self) -> bool {
        self.0 != i64::MIN && self.0 != i64::MAX
    }

    /// `timestamp_in` with a typmod (`-1` for none).
    pub fn parse(input: &str, typmod: i32, env: &DateTimeEnv<'_>) -> Result<Timestamp> {
        let d = decode(input, env, "timestamp")?;
        let v = match d.dtype {
            DTK_DATE => {
                tm2timestamp(&d.tm, d.fsec, None).ok_or_else(|| out_of_range_input(input))?
            }
            DTK_EPOCH => EPOCH_TIMESTAMP,
            DTK_LATE => i64::MAX,
            DTK_EARLY => i64::MIN,
            _ => return Err(crate::error::DtErr::BadFormat.report(input, "timestamp")),
        };
        Ok(Timestamp(adjust_for_typmod(v, typmod)?))
    }

    /// `timestamp_out` using the session `DateStyle`.
    pub fn format(self, env: &DateTimeEnv<'_>) -> Result<String> {
        if self.0 == i64::MIN {
            return Ok("-infinity".to_owned());
        }
        if self.0 == i64::MAX {
            return Ok("infinity".to_owned());
        }
        let (tm, fsec) =
            timestamp2tm_utc(self.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        Ok(encode_date_time(
            &tm,
            fsec,
            None,
            env.date_style,
            env.date_order,
        ))
    }

    /// Apply a typmod (fractional-second precision), as `timestamp(p)` does.
    pub fn with_typmod(self, typmod: i32) -> Result<Timestamp> {
        adjust_for_typmod(self.0, typmod).map(Timestamp)
    }

    /// `timestamp + interval`.
    pub fn add_interval(self, span: &Interval) -> Result<Timestamp> {
        if let Some(r) = add_interval_infinities(self.0, span) {
            return r.map(Timestamp);
        }
        let mut ts = self.0;
        if span.months != 0 {
            let (mut tm, fsec) =
                timestamp2tm_utc(ts).ok_or_else(DateTimeError::timestamp_out_of_range)?;
            add_months_to_tm(&mut tm, span.months)?;
            ts = tm2timestamp(&tm, fsec, None).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        }
        if span.days != 0 {
            let (mut tm, fsec) =
                timestamp2tm_utc(ts).ok_or_else(DateTimeError::timestamp_out_of_range)?;
            add_days_to_tm(&mut tm, span.days, 0)?;
            ts = tm2timestamp(&tm, fsec, None).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        }
        finish_add(ts, span.micros).map(Timestamp)
    }

    /// `timestamp - interval`.
    pub fn sub_interval(self, span: &Interval) -> Result<Timestamp> {
        self.add_interval(&span.negate()?)
    }

    /// `timestamp - timestamp`.
    pub fn sub_timestamp(self, other: Timestamp) -> Result<Interval> {
        timestamp_diff(self.0, other.0)
    }

    /// `timestamp::timestamptz` in the given session zone.
    pub fn to_timestamptz(self, tz: &TimeZone) -> Result<TimestampTz> {
        if !self.is_finite() {
            return Ok(TimestampTz(self.0));
        }
        let (mut tm, _) =
            timestamp2tm_utc(self.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        let off = tz.determine_offset(&mut tm);
        let r = self.0.wrapping_add(i64::from(off) * USECS_PER_SEC);
        if is_valid_timestamp(r) {
            Ok(TimestampTz(r))
        } else {
            Err(DateTimeError::timestamp_out_of_range())
        }
    }

    /// `timestamp::date`.
    pub fn to_date(self) -> Result<Date> {
        match self.0 {
            i64::MIN => Ok(Date::NEG_INFINITY),
            i64::MAX => Ok(Date::INFINITY),
            t => {
                let (tm, _) =
                    timestamp2tm_utc(t).ok_or_else(DateTimeError::timestamp_out_of_range)?;
                Ok(Date(
                    date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE,
                ))
            }
        }
    }

    /// `timestamp AT TIME ZONE zone` (result is a timestamptz).
    pub fn at_time_zone(self, zone: &str, env: &DateTimeEnv<'_>) -> Result<TimestampTz> {
        if !self.is_finite() {
            return Ok(TimestampTz(self.0));
        }
        let result = match decode_timezone_name(zone, env)? {
            ZoneSpec::Fixed(off) => self.0 - i64::from(off) * USECS_PER_SEC,
            ZoneSpec::Dynamic(tzp) => {
                let (mut tm, _) =
                    timestamp2tm_utc(self.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
                let tz = -tzp.determine_abbrev_offset(&mut tm, zone.as_bytes());
                self.0 - i64::from(tz) * USECS_PER_SEC
            }
            ZoneSpec::Zone(tzp) => {
                let (mut tm, fsec) =
                    timestamp2tm_utc(self.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
                let tz = tzp.determine_offset(&mut tm);
                tm2timestamp(&tm, fsec, Some(tz))
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?
            }
        };
        if !is_valid_timestamp(result) {
            return Err(DateTimeError::timestamp_out_of_range());
        }
        Ok(TimestampTz(result))
    }
}

impl TimestampTz {
    /// `infinity`.
    pub const INFINITY: TimestampTz = TimestampTz(i64::MAX);
    /// `-infinity`.
    pub const NEG_INFINITY: TimestampTz = TimestampTz(i64::MIN);

    /// Whether the value is finite.
    pub fn is_finite(self) -> bool {
        self.0 != i64::MIN && self.0 != i64::MAX
    }

    /// Microseconds since the Unix epoch (`1970-01-01 00:00:00 UTC`).
    pub fn from_unix_micros(us: i64) -> TimestampTz {
        TimestampTz(us - EPOCH_DIFF_SECS * USECS_PER_SEC)
    }

    /// `timestamptz_in` with a typmod (`-1` for none).
    pub fn parse(input: &str, typmod: i32, env: &DateTimeEnv<'_>) -> Result<TimestampTz> {
        let datatype = "timestamp with time zone";
        let d = decode(input, env, datatype)?;
        let v = match d.dtype {
            DTK_DATE => {
                tm2timestamp(&d.tm, d.fsec, Some(d.tz)).ok_or_else(|| out_of_range_input(input))?
            }
            DTK_EPOCH => EPOCH_TIMESTAMP,
            DTK_LATE => i64::MAX,
            DTK_EARLY => i64::MIN,
            _ => return Err(crate::error::DtErr::BadFormat.report(input, datatype)),
        };
        Ok(TimestampTz(adjust_for_typmod(v, typmod)?))
    }

    /// `timestamptz_out` using the session `DateStyle` and `TimeZone`.
    pub fn format(self, env: &DateTimeEnv<'_>) -> Result<String> {
        if self.0 == i64::MIN {
            return Ok("-infinity".to_owned());
        }
        if self.0 == i64::MAX {
            return Ok("infinity".to_owned());
        }
        let l = env
            .time_zone
            .to_local(self.0)
            .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        Ok(encode_date_time(
            &l.tm,
            l.fsec,
            Some((l.tz, l.abbrev)),
            env.date_style,
            env.date_order,
        ))
    }

    /// Apply a typmod (fractional-second precision).
    pub fn with_typmod(self, typmod: i32) -> Result<TimestampTz> {
        adjust_for_typmod(self.0, typmod).map(TimestampTz)
    }

    /// `timestamptz + interval`: months and days are added in local time of
    /// `tz`, the time part as absolute time.
    pub fn add_interval(self, span: &Interval, tz: &TimeZone) -> Result<TimestampTz> {
        if let Some(r) = add_interval_infinities(self.0, span) {
            return r.map(TimestampTz);
        }
        let mut ts = self.0;
        if span.months != 0 {
            let l = tz
                .to_local(ts)
                .ok_or_else(DateTimeError::timestamp_out_of_range)?;
            let (mut tm, fsec) = (l.tm, l.fsec);
            add_months_to_tm(&mut tm, span.months)?;
            let off = tz.determine_offset(&mut tm);
            ts = tm2timestamp(&tm, fsec, Some(off))
                .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        }
        if span.days != 0 {
            let l = tz
                .to_local(ts)
                .ok_or_else(DateTimeError::timestamp_out_of_range)?;
            let (mut tm, fsec) = (l.tm, l.fsec);
            add_days_to_tm(&mut tm, span.days, -1)?;
            let off = tz.determine_offset(&mut tm);
            ts = tm2timestamp(&tm, fsec, Some(off))
                .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        }
        finish_add(ts, span.micros).map(TimestampTz)
    }

    /// `timestamptz - interval`.
    pub fn sub_interval(self, span: &Interval, tz: &TimeZone) -> Result<TimestampTz> {
        self.add_interval(&span.negate()?, tz)
    }

    /// `timestamptz - timestamptz`.
    pub fn sub_timestamptz(self, other: TimestampTz) -> Result<Interval> {
        timestamp_diff(self.0, other.0)
    }

    /// `timestamptz::timestamp` (local time in `tz`).
    pub fn to_timestamp(self, tz: &TimeZone) -> Result<Timestamp> {
        if !self.is_finite() {
            return Ok(Timestamp(self.0));
        }
        let l = tz
            .to_local(self.0)
            .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        tm2timestamp(&l.tm, l.fsec, None)
            .map(Timestamp)
            .ok_or_else(DateTimeError::timestamp_out_of_range)
    }

    /// `timestamptz::date` (local date in `tz`).
    pub fn to_date(self, tz: &TimeZone) -> Result<Date> {
        match self.0 {
            i64::MIN => Ok(Date::NEG_INFINITY),
            i64::MAX => Ok(Date::INFINITY),
            t => {
                let l = tz
                    .to_local(t)
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?;
                Ok(Date(
                    date2j(l.tm.year, l.tm.mon, l.tm.mday) - POSTGRES_EPOCH_JDATE,
                ))
            }
        }
    }

    /// `timestamptz AT TIME ZONE zone` (result is a timestamp).
    pub fn at_time_zone(self, zone: &str, env: &DateTimeEnv<'_>) -> Result<Timestamp> {
        if !self.is_finite() {
            return Ok(Timestamp(self.0));
        }
        let result = match decode_timezone_name(zone, env)? {
            ZoneSpec::Fixed(off) => self.0 + i64::from(off) * USECS_PER_SEC,
            ZoneSpec::Dynamic(tzp) => {
                let (tz, _) = tzp
                    .determine_abbrev_offset_ts(self.0, zone.as_bytes())
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?;
                self.0 - i64::from(tz) * USECS_PER_SEC
            }
            ZoneSpec::Zone(tzp) => {
                let l = tzp
                    .to_local(self.0)
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?;
                tm2timestamp(&l.tm, l.fsec, None)
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?
            }
        };
        if !is_valid_timestamp(result) {
            return Err(DateTimeError::timestamp_out_of_range());
        }
        Ok(Timestamp(result))
    }
}

/// Microseconds per day, re-exported for crate users that convert dates.
pub const MICROS_PER_DAY: i64 = USECS_PER_DAY;
