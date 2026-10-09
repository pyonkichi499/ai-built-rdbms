//! `make_date`, `make_timestamp`, `make_timestamptz`, `make_interval`,
//! `to_timestamp(float8)` and `date_bin` (PostgreSQL `date.c` /
//! `timestamp.c`). Error texts were checked against PostgreSQL 17.

use crate::calendar::{
    DATETIME_MIN_JULIAN, POSTGRES_EPOCH_JDATE, SECS_PER_DAY, TIMESTAMP_END_JULIAN, Tm,
    UNIX_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC, date2j,
    days_in_month, is_valid_date, is_valid_julian, is_valid_timestamp, tm2timestamp,
};
use crate::cnum::rint;
use crate::date::Date;
use crate::error::{DateTimeError, DtErr, Result, sqlstate};
use crate::interval::Interval;
use crate::parse::decode_timezone;
use crate::settings::DateTimeEnv;
use crate::timestamp::{Timestamp, TimestampTz};
use crate::tz::TimeZone;

/// C's `%g` (precision 6).
fn format_g(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity" } else { "Infinity" }.to_owned();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let e = format!("{v:.5e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        let mant = if mant.contains('.') {
            mant.trim_end_matches('0').trim_end_matches('.')
        } else {
            mant
        };
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mant}e{sign}{:02}", exp.abs());
    }
    #[allow(clippy::cast_sign_loss)]
    let decimals = (5 - exp).max(0) as usize;
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

/// `%02g`.
fn format_02g(v: f64) -> String {
    let s = format_g(v);
    if s.len() >= 2 { s } else { format!("0{s}") }
}

fn field_overflow(msg: String) -> DateTimeError {
    DateTimeError::new(sqlstate::DATETIME_FIELD_OVERFLOW, msg)
}

/// `ValidateDate` for a non-Julian date given as a positive year plus a BC
/// flag. `Ok` is the astronomical year; `Err` is the year `make_date`
/// prints in its message.
fn validate_date(year: i32, mon: i32, mday: i32, bc: bool) -> std::result::Result<i32, i32> {
    if year <= 0 {
        return Err(year);
    }
    let y = if bc { -(year - 1) } else { year };
    if !(1..=12).contains(&mon) || mday < 1 || mday > days_in_month(y, mon) {
        return Err(y);
    }
    Ok(y)
}

/// `make_date(year, month, day)`. A negative year is BC; year 0 is invalid.
pub fn make_date(year: i32, month: i32, day: i32) -> Result<Date> {
    let (bc, y) = if year < 0 {
        (true, year.wrapping_neg())
    } else {
        (false, year)
    };
    let y = match validate_date(y, month, day, bc) {
        Ok(y) => y,
        Err(shown) => {
            return Err(field_overflow(format!(
                "date field value out of range: {shown}-{month:02}-{day:02}"
            )));
        }
    };
    let out_of_range = || field_overflow(format!("date out of range: {y}-{month:02}-{day:02}"));
    if !is_valid_julian(y, month, day) {
        return Err(out_of_range());
    }
    let date = date2j(y, month, day) - POSTGRES_EPOCH_JDATE;
    if !is_valid_date(date) {
        return Err(out_of_range());
    }
    Ok(Date(date))
}

/// `float_time_overflows`.
fn time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    !(0..=24).contains(&hour)
        || !(0..60).contains(&min)
        || sec.is_nan()
        || !(0.0..=60.0).contains(&sec)
        || (hour == 24 && (min > 0 || sec > 0.0))
}

/// `make_timestamp(year, month, mday, hour, min, sec)`.
pub fn make_timestamp(
    year: i32,
    month: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: f64,
) -> Result<Timestamp> {
    let (bc, ay) = if year < 0 {
        (true, year.wrapping_neg())
    } else {
        (false, year)
    };
    let Ok(y) = validate_date(ay, month, mday, bc) else {
        return Err(field_overflow(format!(
            "date field value out of range: {year}-{month:02}-{mday:02}"
        )));
    };
    if !is_valid_julian(y, month, mday) {
        return Err(field_overflow(format!(
            "date out of range: {year}-{month:02}-{mday:02}"
        )));
    }
    if time_overflows(hour, min, sec) {
        return Err(field_overflow(format!(
            "time field value out of range: {hour}:{min:02}:{}",
            format_02g(sec)
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    let usec = rint(sec * USECS_PER_SEC as f64) as i64;
    let tm = Tm {
        year: y,
        mon: month,
        mday,
        hour,
        min,
        sec: (usec / USECS_PER_SEC) as i32,
        yday: 0,
        isdst: -1,
    };
    let fsec = (usec % USECS_PER_SEC) as i32;
    tm2timestamp(&tm, fsec, None).map(Timestamp).ok_or_else(|| {
        field_overflow(format!(
            "timestamp out of range: {year}-{month:02}-{mday:02} {hour}:{min:02}:{}",
            format_02g(sec)
        ))
    })
}

/// `make_timestamptz(year, month, mday, hour, min, sec[, timezone])`.
/// Without `zone` the session zone is used. A numeric offset in `zone`
/// follows the ISO sign convention (`'-03'` is UTC-3); names, abbreviations
/// and POSIX strings behave as in `AT TIME ZONE`.
#[allow(clippy::too_many_arguments)]
pub fn make_timestamptz(
    year: i32,
    month: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: f64,
    zone: Option<&str>,
    env: &DateTimeEnv<'_>,
) -> Result<TimestampTz> {
    let local = make_timestamp(year, month, mday, hour, min, sec)?;
    let Some(zone) = zone else {
        return local.to_timestamptz(env.time_zone);
    };
    if zone.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("invalid input syntax for type numeric time zone: \"{zone}\""),
        )
        .with_hint("Numeric time zones must have \"-\" or \"+\" as first character."));
    }
    match decode_timezone(zone.as_bytes()) {
        Ok(west) => {
            let r = local.0.wrapping_add(i64::from(west) * USECS_PER_SEC);
            if is_valid_timestamp(r) {
                Ok(TimestampTz(r))
            } else {
                Err(DateTimeError::timestamp_out_of_range())
            }
        }
        Err(DtErr::TzDispOverflow) => Err(DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("numeric time zone \"{zone}\" out of range"),
        )),
        Err(_) => local.at_time_zone(zone, env),
    }
}

/// `make_interval(years, months, weeks, days, hours, mins, secs)`.
pub fn make_interval(
    years: i32,
    months: i32,
    weeks: i32,
    days: i32,
    hours: i32,
    mins: i32,
    secs: f64,
) -> Result<Interval> {
    let overflow = DateTimeError::interval_out_of_range;
    if !secs.is_finite() {
        return Err(overflow());
    }
    let months = years
        .checked_mul(12)
        .and_then(|v| v.checked_add(months))
        .ok_or_else(overflow)?;
    let days = weeks
        .checked_mul(7)
        .and_then(|v| v.checked_add(days))
        .ok_or_else(overflow)?;
    let secs_us = rint(secs * USECS_PER_SEC as f64);
    #[allow(clippy::cast_precision_loss)]
    if !(secs_us >= i64::MIN as f64 && secs_us < -(i64::MIN as f64)) {
        return Err(overflow());
    }
    #[allow(clippy::cast_possible_truncation)]
    let micros = (i64::from(hours) * USECS_PER_HOUR)
        .checked_add(i64::from(mins) * USECS_PER_MINUTE)
        .and_then(|v| v.checked_add(secs_us as i64))
        .ok_or_else(overflow)?;
    let r = Interval {
        months,
        days,
        micros,
    };
    if r.is_finite() {
        Ok(r)
    } else {
        Err(overflow())
    }
}

/// `to_timestamp(float8)` (`float8_timestamptz`): seconds since the UNIX
/// epoch. `NaN` is an error; infinities map to `±infinity`.
pub fn to_timestamp_float(secs: f64) -> Result<TimestampTz> {
    if secs.is_nan() {
        return Err(field_overflow("timestamp cannot be NaN".to_owned()));
    }
    if secs.is_infinite() {
        return Ok(if secs < 0.0 {
            TimestampTz::NEG_INFINITY
        } else {
            TimestampTz::INFINITY
        });
    }
    let out_of_range = || field_overflow(format!("timestamp out of range: \"{}\"", format_g(secs)));
    #[allow(clippy::cast_precision_loss)]
    let (lo, hi) = (
        (SECS_PER_DAY * i64::from(DATETIME_MIN_JULIAN - UNIX_EPOCH_JDATE)) as f64,
        (SECS_PER_DAY * i64::from(TIMESTAMP_END_JULIAN - UNIX_EPOCH_JDATE)) as f64,
    );
    if secs < lo || secs >= hi {
        return Err(out_of_range());
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let us = rint(
        (secs - (i64::from(POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * SECS_PER_DAY) as f64)
            * USECS_PER_SEC as f64,
    ) as i64;
    if is_valid_timestamp(us) {
        Ok(TimestampTz(us))
    } else {
        Err(out_of_range())
    }
}

/// The zone named by the optional third argument of `date_add` /
/// `date_subtract` (`pg_tzset`: a zone name or POSIX string, no
/// abbreviation table).
pub fn zone_by_name(name: &str, env: &DateTimeEnv<'_>) -> Result<TimeZone> {
    env.zones.lookup(name).ok_or_else(|| {
        DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("time zone \"{name}\" not recognized"),
        )
    })
}

/// Shared body of `date_bin` over the raw microsecond values.
fn bin(stride: &Interval, source: i64, origin: i64) -> Result<i64> {
    let overflow = DateTimeError::interval_out_of_range;
    if source == i64::MIN || source == i64::MAX {
        return Ok(source);
    }
    if origin == i64::MIN || origin == i64::MAX {
        return Err(field_overflow("origin out of range".to_owned()));
    }
    if !stride.is_finite() {
        return Err(field_overflow(
            "timestamps cannot be binned into infinite intervals".to_owned(),
        ));
    }
    if stride.months != 0 {
        return Err(DateTimeError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "timestamps cannot be binned into intervals containing months or years",
        ));
    }
    let stride_us = i64::from(stride.days)
        .checked_mul(USECS_PER_DAY)
        .and_then(|v| v.checked_add(stride.micros))
        .ok_or_else(overflow)?;
    if stride_us <= 0 {
        return Err(field_overflow(
            "stride must be greater than zero".to_owned(),
        ));
    }
    let diff = source.checked_sub(origin).ok_or_else(overflow)?;
    let mut delta = diff - diff % stride_us;
    if diff < 0 && diff % stride_us != 0 {
        delta = delta.checked_sub(stride_us).ok_or_else(overflow)?;
    }
    let r = origin
        .checked_add(delta)
        .filter(|v| is_valid_timestamp(*v))
        .ok_or_else(DateTimeError::timestamp_out_of_range)?;
    Ok(r)
}

/// `date_bin(stride, timestamp, origin)`.
pub fn date_bin_timestamp(
    stride: &Interval,
    source: Timestamp,
    origin: Timestamp,
) -> Result<Timestamp> {
    bin(stride, source.0, origin.0).map(Timestamp)
}

/// `date_bin(stride, timestamptz, origin)`.
pub fn date_bin_timestamptz(
    stride: &Interval,
    source: TimestampTz,
    origin: TimestampTz,
) -> Result<TimestampTz> {
    bin(stride, source.0, origin.0).map(TimestampTz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::IntervalStyle;
    use crate::tz::ZoneDb;

    fn msg<T>(r: Result<T>) -> String {
        r.err().expect("expected an error").message
    }

    fn with_env<R>(f: impl FnOnce(&DateTimeEnv<'_>) -> R) -> R {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        f(&DateTimeEnv::new(&tz, &zones))
    }

    fn show_ts(t: Timestamp) -> String {
        with_env(|e| t.format(e).unwrap())
    }

    fn show_tstz(t: TimestampTz) -> String {
        with_env(|e| t.format(e).unwrap())
    }

    #[test]
    fn make_date_cases() {
        with_env(|e| {
            assert_eq!(make_date(2024, 2, 29).unwrap().format(e), "2024-02-29");
            assert_eq!(make_date(-44, 3, 15).unwrap().format(e), "0044-03-15 BC");
        });
        assert_eq!(
            msg(make_date(2024, 2, 30)),
            "date field value out of range: 2024-02-30"
        );
        assert_eq!(
            msg(make_date(0, 1, 1)),
            "date field value out of range: 0-01-01"
        );
        assert_eq!(
            msg(make_date(-44, 2, 30)),
            "date field value out of range: -43-02-30"
        );
        assert_eq!(
            msg(make_date(2024, 13, 1)),
            "date field value out of range: 2024-13-01"
        );
        assert_eq!(
            msg(make_date(300_000_000, 1, 1)),
            "date out of range: 300000000-01-01"
        );
        assert_eq!(
            msg(make_date(-294_276, 1, 1)),
            "date out of range: -294275-01-01"
        );
    }

    #[test]
    fn make_timestamp_cases() {
        assert_eq!(
            show_ts(make_timestamp(2024, 1, 1, 0, 0, 60.0).unwrap()),
            "2024-01-01 00:01:00"
        );
        assert_eq!(
            show_ts(make_timestamp(2024, 1, 1, 24, 0, 0.0).unwrap()),
            "2024-01-02 00:00:00"
        );
        assert_eq!(
            show_ts(make_timestamp(-1, 1, 1, 0, 0, 0.0).unwrap()),
            "0001-01-01 00:00:00 BC"
        );
        assert_eq!(
            show_ts(make_timestamp(2024, 1, 1, 0, 0, 59.999_999_6).unwrap()),
            "2024-01-01 00:01:00"
        );
        assert_eq!(
            msg(make_timestamp(2024, 2, 30, 25, 0, 0.0)),
            "date field value out of range: 2024-02-30"
        );
        assert_eq!(
            msg(make_timestamp(-44, 2, 30, 0, 0, 0.0)),
            "date field value out of range: -44-02-30"
        );
        assert_eq!(
            msg(make_timestamp(2024, 1, 1, 25, 0, 0.0)),
            "time field value out of range: 25:00:00"
        );
        assert_eq!(
            msg(make_timestamp(2024, 1, 1, 24, 0, 0.5)),
            "time field value out of range: 24:00:0.5"
        );
        assert_eq!(
            msg(make_timestamp(2024, 1, 1, 0, 0, f64::NAN)),
            "time field value out of range: 0:00:NaN"
        );
        assert_eq!(
            msg(make_timestamp(294_277, 1, 1, 0, 0, 0.0)),
            "timestamp out of range: 294277-01-01 0:00:00"
        );
        assert_eq!(
            msg(make_timestamp(294_276, 12, 31, 24, 0, 0.0)),
            "timestamp out of range: 294276-12-31 24:00:00"
        );
        assert_eq!(
            msg(make_timestamp(-4714, 11, 23, 0, 0, 0.0)),
            "timestamp out of range: -4714-11-23 0:00:00"
        );
        assert_eq!(
            msg(make_timestamp(2_147_483_647, 1, 1, 0, 0, 0.0)),
            "date out of range: 2147483647-01-01"
        );
    }

    #[test]
    fn make_timestamptz_zones() {
        with_env(|e| {
            let f = |z: Option<&str>| make_timestamptz(2024, 1, 1, 12, 0, 0.0, z, e);
            assert_eq!(show_tstz(f(None).unwrap()), "2024-01-01 12:00:00+00");
            assert_eq!(show_tstz(f(Some("-03")).unwrap()), "2024-01-01 15:00:00+00");
            assert_eq!(
                show_tstz(f(Some("+09:30")).unwrap()),
                "2024-01-01 02:30:00+00"
            );
            assert_eq!(
                show_tstz(f(Some("UTC+3")).unwrap()),
                "2024-01-01 15:00:00+00"
            );
            assert_eq!(show_tstz(f(Some("JST")).unwrap()), "2024-01-01 03:00:00+00");
            assert_eq!(
                msg(f(Some("9"))),
                "invalid input syntax for type numeric time zone: \"9\""
            );
            assert_eq!(
                msg(f(Some("+99"))),
                "numeric time zone \"+99\" out of range"
            );
            assert_eq!(msg(f(Some(""))), "time zone \"\" not recognized");
            assert_eq!(
                msg(f(Some("Foo/Bar"))),
                "time zone \"Foo/Bar\" not recognized"
            );
        });
    }

    #[test]
    fn make_interval_cases() {
        let show = |i: Interval| i.format(IntervalStyle::Postgres);
        assert_eq!(
            show(make_interval(1, 2, 0, 0, 0, 0, 0.0).unwrap()),
            "1 year 2 mons"
        );
        assert_eq!(
            show(make_interval(0, 0, 0, 0, 0, 0, 0.0).unwrap()),
            "00:00:00"
        );
        assert_eq!(
            show(make_interval(0, 0, 1, 1, 2, 3, 4.5).unwrap()),
            "8 days 02:03:04.5"
        );
        assert_eq!(
            show(make_interval(0, 0, 306_783_378, 0, 0, 0, 0.0).unwrap()),
            "2147483646 days"
        );
        assert!(make_interval(0, 0, 306_783_379, 0, 0, 0, 0.0).is_err());
        assert!(make_interval(2_147_483_647, 0, 0, 0, 0, 0, 0.0).is_err());
        assert!(make_interval(0, 0, 0, 0, 0, 0, f64::NAN).is_err());
        assert!(make_interval(0, 0, 0, 0, 0, 0, f64::INFINITY).is_err());
        assert!(make_interval(0, 0, 0, 0, 0, 0, 9.3e12).is_err());
        assert_eq!(
            show(make_interval(0, 0, 0, 0, 0, 0, 9.2e12).unwrap()),
            "2555555555:33:20"
        );
    }

    #[test]
    fn to_timestamp_float_cases() {
        assert_eq!(
            show_tstz(to_timestamp_float(1_700_000_000.123_456).unwrap()),
            "2023-11-14 22:13:20.123456+00"
        );
        assert_eq!(
            to_timestamp_float(f64::INFINITY).unwrap(),
            TimestampTz::INFINITY
        );
        assert_eq!(
            to_timestamp_float(f64::NEG_INFINITY).unwrap(),
            TimestampTz::NEG_INFINITY
        );
        assert_eq!(msg(to_timestamp_float(f64::NAN)), "timestamp cannot be NaN");
        assert_eq!(
            msg(to_timestamp_float(1e20)),
            "timestamp out of range: \"1e+20\""
        );
        assert_eq!(
            msg(to_timestamp_float(-1e12)),
            "timestamp out of range: \"-1e+12\""
        );
    }

    #[test]
    fn date_bin_cases() {
        with_env(|e| {
            let iv = |s: &str| Interval::parse(s, -1, e).unwrap();
            let ts = |s: &str| Timestamp::parse(s, -1, e).unwrap();
            let bin = |s: &str, src: &str, org: &str| {
                show_ts(date_bin_timestamp(&iv(s), ts(src), ts(org)).unwrap())
            };
            assert_eq!(
                bin("15 minutes", "2024-03-09 15:44:05", "2000-01-01"),
                "2024-03-09 15:30:00"
            );
            assert_eq!(
                bin("15 minutes", "1999-12-31 23:50:00", "2000-01-01"),
                "1999-12-31 23:45:00"
            );
            assert_eq!(
                bin("1 day 2 hours", "2024-03-09 15:44:05", "2000-01-01"),
                "2024-03-09 14:00:00"
            );
            let err =
                |s: &str, src: &str, org: &str| msg(date_bin_timestamp(&iv(s), ts(src), ts(org)));
            assert_eq!(
                err("1 month", "2024-01-01", "2000-01-01"),
                "timestamps cannot be binned into intervals containing months or years"
            );
            assert_eq!(
                err("0 sec", "2024-01-01", "2000-01-01"),
                "stride must be greater than zero"
            );
            assert_eq!(
                err("-1 sec", "2024-01-01", "2000-01-01"),
                "stride must be greater than zero"
            );
            assert_eq!(
                err("infinity", "2024-01-01", "2000-01-01"),
                "timestamps cannot be binned into infinite intervals"
            );
            assert_eq!(
                err("1 day", "2024-01-01", "infinity"),
                "origin out of range"
            );
            assert_eq!(
                date_bin_timestamp(&iv("1 day"), Timestamp(i64::MAX), ts("2000-01-01")).unwrap(),
                Timestamp(i64::MAX)
            );
        });
    }

    #[test]
    fn zone_by_name_reports_unknown() {
        with_env(|e| {
            assert!(zone_by_name("UTC", e).is_ok());
            assert_eq!(
                msg(zone_by_name("Foo/Bar", e)),
                "time zone \"Foo/Bar\" not recognized"
            );
        });
    }
}
