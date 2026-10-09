//! `time` (without time zone): a port of the `TimeADT` parts of
//! PostgreSQL's `date.c`.
//!
//! `timetz` is not supported by this crate (design 06 TD-D5); an input
//! string carrying a time zone is still accepted by [`Time::parse`] and the
//! zone is ignored, exactly like `time_in`.

use crate::calendar::{
    MAX_PRECISION, MINS_PER_HOUR, SECS_PER_MINUTE, USECS_PER_DAY, USECS_PER_SEC, time2t,
};
use crate::cnum::rint;
use crate::date::Date;
use crate::error::{DateTimeError, Result, sqlstate};
use crate::interval::Interval;
use crate::parse::{MAXDATEFIELDS, MAXDATELEN, decode_time_only, parse_datetime};
use crate::settings::DateTimeEnv;
use crate::timestamp::{Timestamp, TimestampTz};
use crate::tz::TimeZone;

/// `time without time zone`: microseconds since midnight, `0 ..=
/// 86_400_000_000` (`24:00:00` is valid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub i64);

/// `AdjustTimeForTypmod`.
fn adjust_for_typmod(time: i64, typmod: i32) -> i64 {
    const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1000, 100, 10, 1];
    const OFFSETS: [i64; 7] = [500_000, 50_000, 5000, 500, 50, 5, 0];
    if !(0..=MAX_PRECISION).contains(&typmod) {
        return time;
    }
    #[allow(clippy::cast_sign_loss)]
    let i = typmod as usize;
    if time >= 0 {
        ((time + OFFSETS[i]) / SCALES[i]) * SCALES[i]
    } else {
        -((((-time) + OFFSETS[i]) / SCALES[i]) * SCALES[i])
    }
}

/// Wrap a microsecond count into `0 .. USECS_PER_DAY` the way
/// `time_pl_interval` does (truncating division, then fix a negative).
fn wrap_day(v: i64) -> i64 {
    let mut r = v - v / USECS_PER_DAY * USECS_PER_DAY;
    if r < 0 {
        r += USECS_PER_DAY;
    }
    r
}

/// C's `%g` (precision 6) for a finite or non-finite `f64`.
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

/// `%02g`: `format_g` padded with zeros to a width of 2.
fn format_02g(v: f64) -> String {
    let s = format_g(v);
    if s.len() >= 2 {
        return s;
    }
    format!("0{s}")
}

impl Time {
    /// `00:00:00`.
    pub const MIN: Time = Time(0);
    /// `24:00:00`.
    pub const MAX: Time = Time(USECS_PER_DAY);

    /// Split into `(hour, minute, second, microsecond)` (`time2tm`).
    pub fn to_hmsu(self) -> (i32, i32, i32, i32) {
        crate::calendar::dt2time(self.0)
    }

    /// `time_in` with a typmod (`-1` for none).
    pub fn parse(input: &str, typmod: i32, env: &DateTimeEnv<'_>) -> Result<Time> {
        let datatype = "time";
        let fields = parse_datetime(input, MAXDATELEN + MAXDATEFIELDS)
            .map_err(|e| e.report(input, datatype))?;
        let (tm, fsec) = decode_time_only(&fields, env).map_err(|e| e.report(input, datatype))?;
        let t = time2t(tm.hour, tm.min, tm.sec, fsec);
        Ok(Time(adjust_for_typmod(t, typmod)))
    }

    /// `time_out` (`EncodeTimeOnly`): `HH:MM:SS[.ffffff]`, independent of
    /// `DateStyle`.
    pub fn format(self) -> String {
        let (h, m, s, fsec) = self.to_hmsu();
        let mut out = format!("{h:02}:{m:02}:{s:02}");
        if fsec != 0 {
            let frac = format!("{fsec:06}");
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
        out
    }

    /// `time(p)` (`AdjustTimeForTypmod`: rounds the fractional seconds).
    #[allow(clippy::unnecessary_wraps)]
    pub fn with_typmod(self, typmod: i32) -> Result<Time> {
        Ok(Time(adjust_for_typmod(self.0, typmod)))
    }

    /// `time + interval`: months and days are ignored; wraps at 24 hours.
    pub fn add_interval(self, span: &Interval) -> Result<Time> {
        if !span.is_finite() {
            return Err(DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "cannot add infinite interval to time",
            ));
        }
        Ok(Time(wrap_day(self.0.wrapping_add(span.micros))))
    }

    /// `time - interval`.
    pub fn sub_interval(self, span: &Interval) -> Result<Time> {
        if !span.is_finite() {
            return Err(DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "cannot subtract infinite interval from time",
            ));
        }
        Ok(Time(wrap_day(self.0.wrapping_sub(span.micros))))
    }

    /// `time - time` (not wrapped: `24:00:00 - 00:00:00 = 24:00:00`).
    pub fn sub_time(self, other: Time) -> Interval {
        Interval {
            months: 0,
            days: 0,
            micros: self.0 - other.0,
        }
    }

    /// `time::interval`.
    pub fn to_interval(self) -> Interval {
        Interval {
            months: 0,
            days: 0,
            micros: self.0,
        }
    }

    /// `interval::time`: the microseconds wrapped into a day (a negative
    /// value wraps forward).
    pub fn from_interval(iv: &Interval) -> Result<Time> {
        if !iv.is_finite() {
            return Err(DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "cannot convert infinite interval to time",
            ));
        }
        Ok(Time(wrap_day(iv.micros)))
    }

    /// `make_time(hour, min, sec)`.
    pub fn make(hour: i32, min: i32, sec: f64) -> Result<Time> {
        if hour < 0
            || min < 0
            || min > MINS_PER_HOUR as i32 - 1
            || sec.is_nan()
            || sec < 0.0
            || sec > SECS_PER_MINUTE as f64
            || hour > 24
            || (hour == 24 && (min > 0 || sec > 0.0))
        {
            return Err(DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                format!(
                    "time field value out of range: {hour}:{min:02}:{}",
                    format_02g(sec)
                ),
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let usec = rint(sec * USECS_PER_SEC as f64) as i64;
        Ok(Time(
            ((i64::from(hour) * MINS_PER_HOUR + i64::from(min)) * SECS_PER_MINUTE) * USECS_PER_SEC
                + usec,
        ))
    }
}

impl Timestamp {
    /// `timestamp::time`; `None` (SQL NULL) for an infinite timestamp.
    pub fn to_time(self) -> Option<Time> {
        if !self.is_finite() {
            return None;
        }
        Some(Time(self.0.rem_euclid(USECS_PER_DAY)))
    }
}

impl TimestampTz {
    /// `timestamptz::time` in the session zone; `None` (SQL NULL) for an
    /// infinite value.
    pub fn to_time(self, tz: &TimeZone) -> Result<Option<Time>> {
        if self.0 == i64::MIN || self.0 == i64::MAX {
            return Ok(None);
        }
        let l = tz
            .to_local(self.0)
            .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        Ok(Some(Time(time2t(l.tm.hour, l.tm.min, l.tm.sec, l.fsec))))
    }
}

impl Date {
    /// `date + time` (`datetime_timestamp`); an infinite date stays
    /// infinite.
    pub fn add_time(self, t: Time) -> Result<Timestamp> {
        let ts = self.to_timestamp()?;
        if !ts.is_finite() {
            return Ok(ts);
        }
        let r =
            ts.0.checked_add(t.0)
                .filter(|&v| crate::calendar::is_valid_timestamp(v))
                .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        Ok(Timestamp(r))
    }
}

/// The error for the unsupported `timetz` type (`0A000`).
pub fn timetz_unsupported() -> DateTimeError {
    DateTimeError::new(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "type timetz is not supported yet",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tz::ZoneDb;

    fn with_env<T>(f: impl FnOnce(&DateTimeEnv<'_>) -> T) -> T {
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        let env = DateTimeEnv::new(&tz, &zones);
        f(&env)
    }

    fn p(s: &str) -> Result<Time> {
        with_env(|env| Time::parse(s, -1, env))
    }

    fn out(s: &str) -> String {
        p(s).map_or_else(
            |e| format!("ERR {} {}", e.sqlstate, e.message),
            Time::format,
        )
    }

    #[test]
    fn basic_input_output() {
        assert_eq!(out("04:05:06.789"), "04:05:06.789");
        assert_eq!(out("04:05"), "04:05:00");
        assert_eq!(out("040506"), "04:05:06");
        assert_eq!(out("4:05 PM"), "16:05:00");
        assert_eq!(out("12:00 AM"), "00:00:00");
        assert_eq!(out("24:00:00"), "24:00:00");
        assert_eq!(out("23:59:60"), "24:00:00");
        assert_eq!(out("allballs"), "00:00:00");
        assert_eq!(out("2020-01-01 04:05:06+09"), "04:05:06");
        assert_eq!(out("04:05:06 PST"), "04:05:06");
        assert_eq!(out("T04:05:06"), "04:05:06");
        assert_eq!(
            out("12:34 EST5EDT"),
            "ERR 22007 invalid input syntax for type time: \"12:34 EST5EDT\""
        );
        assert_eq!(out("2003-04-12 04:05:06 EST5EDT"), "04:05:06");
        assert_eq!(
            out("12:34 foo/bar"),
            "ERR 22023 time zone \"foo/bar\" not recognized"
        );
    }

    #[test]
    fn input_errors() {
        assert_eq!(
            out("25:00"),
            "ERR 22008 date/time field value out of range: \"25:00\""
        );
        assert_eq!(
            out("24:00:01"),
            "ERR 22008 date/time field value out of range: \"24:00:01\""
        );
        assert_eq!(
            out("garbage"),
            "ERR 22007 invalid input syntax for type time: \"garbage\""
        );
        assert_eq!(
            out("infinity"),
            "ERR 22007 invalid input syntax for type time: \"infinity\""
        );
    }

    #[test]
    fn typmod_rounds() {
        let t = p("04:05:06.789").unwrap();
        assert_eq!(t.0, 14_706_789_000);
        assert_eq!(t.with_typmod(2).unwrap().format(), "04:05:06.79");
        assert_eq!(t.with_typmod(0).unwrap().format(), "04:05:07");
        assert_eq!(
            p("23:59:59.9999999")
                .unwrap()
                .with_typmod(6)
                .unwrap()
                .format(),
            "24:00:00"
        );
        assert_eq!(t.with_typmod(-1).unwrap(), t);
    }

    #[test]
    fn interval_arithmetic() {
        let iv = |micros| Interval {
            months: 5,
            days: 7,
            micros,
        };
        let t = p("23:00").unwrap();
        assert_eq!(
            t.add_interval(&iv(2 * 3_600_000_000)).unwrap().format(),
            "01:00:00"
        );
        assert_eq!(
            t.sub_interval(&iv(25 * 3_600_000_000)).unwrap().format(),
            "22:00:00"
        );
        assert_eq!(Time::MAX.add_interval(&iv(0)).unwrap().format(), "00:00:00");
        assert_eq!(Time::MAX.sub_time(Time::MIN).micros, USECS_PER_DAY);
        assert_eq!(
            Time::from_interval(&iv(-3_600_000_000)).unwrap().format(),
            "23:00:00"
        );
        assert_eq!(
            Time::from_interval(&iv(USECS_PER_DAY)).unwrap().format(),
            "00:00:00"
        );
        assert!(t.add_interval(&Interval::INFINITY).is_err());
        assert!(Time::from_interval(&Interval::NEG_INFINITY).is_err());
        assert_eq!(t.to_interval().micros, 82_800_000_000);
    }

    #[test]
    fn make_time_range() {
        assert_eq!(Time::make(8, 15, 23.5).unwrap().format(), "08:15:23.5");
        assert_eq!(Time::make(24, 0, 0.0).unwrap(), Time::MAX);
        let e = Time::make(24, 0, 0.5).unwrap_err();
        assert_eq!(e.sqlstate, "22008");
        assert_eq!(e.message, "time field value out of range: 24:00:0.5");
        assert_eq!(
            Time::make(25, 1, 5.0).unwrap_err().message,
            "time field value out of range: 25:01:05"
        );
    }

    #[test]
    fn g_format() {
        assert_eq!(format_02g(5.0), "05");
        assert_eq!(format_02g(5.5), "5.5");
        assert_eq!(format_02g(61.0), "61");
        assert_eq!(format_02g(-1.0), "-1");
        assert_eq!(format_02g(1e7), "1e+07");
        assert_eq!(format_02g(123_456_789.0), "1.23457e+08");
        assert_eq!(format_02g(0.00001), "1e-05");
        assert_eq!(format_02g(0.0), "00");
    }

    #[test]
    fn extract_fields() {
        use crate::fields::{date_part_time, extract_time};
        let t = p("04:05:06.789").unwrap();
        let ex = |u: &str| extract_time(u, t).map(|v| v.map(|n| n.to_string()));
        assert_eq!(ex("hour").unwrap().as_deref(), Some("4"));
        assert_eq!(ex("minute").unwrap().as_deref(), Some("5"));
        assert_eq!(ex("second").unwrap().as_deref(), Some("6.789000"));
        assert_eq!(ex("milliseconds").unwrap().as_deref(), Some("6789.000"));
        assert_eq!(ex("microseconds").unwrap().as_deref(), Some("6789000"));
        assert_eq!(ex("epoch").unwrap().as_deref(), Some("14706.789000"));
        assert_eq!(ex("day").unwrap_err().sqlstate, "0A000");
        assert_eq!(
            ex("day").unwrap_err().message,
            "unit \"day\" not supported for type time without time zone"
        );
        assert_eq!(ex("foo").unwrap_err().sqlstate, "22023");
        assert_eq!(date_part_time("hour", t).unwrap(), Some(4.0));
        assert_eq!(date_part_time("epoch", t).unwrap(), Some(14706.789));
    }

    #[test]
    fn conversions() {
        assert_eq!(Timestamp(86_400_000_000 * 3 + 5).to_time(), Some(Time(5)));
        assert_eq!(Timestamp(-1).to_time(), Some(Time(USECS_PER_DAY - 1)));
        assert_eq!(Timestamp::INFINITY.to_time(), None);
        let d = Date::from_ymd(2024, 1, 1).unwrap();
        let ts = d.add_time(p("04:05:06").unwrap()).unwrap();
        assert_eq!(ts.0 % USECS_PER_DAY, 14_706_000_000);
        assert_eq!(
            Date::INFINITY.add_time(Time(1)).unwrap(),
            Timestamp::INFINITY
        );
    }
}
