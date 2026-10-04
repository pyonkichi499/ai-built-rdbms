//! `date`.

use crate::calendar::{
    POSTGRES_EPOCH_JDATE, TIMESTAMP_END_JULIAN, Tm, USECS_PER_DAY, USECS_PER_SEC, date2j,
    is_valid_date, is_valid_julian, is_valid_timestamp, j2date,
};
use crate::error::{DateTimeError, DtErr, Result, sqlstate};
use crate::format::encode_date_only;
use crate::interval::Interval;
use crate::parse::{MAXDATELEN, decode_date_time, parse_datetime};
use crate::settings::DateTimeEnv;
use crate::timestamp::{Timestamp, TimestampTz};
use crate::tokens::{DTK_DATE, DTK_EARLY, DTK_EPOCH, DTK_LATE};
use crate::tz::TimeZone;

/// `date`: days since 2000-01-01. `i32::MIN` / `i32::MAX` are
/// `-infinity` / `infinity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i32);

fn out_of_range_for_timestamp() -> DateTimeError {
    DateTimeError::new(
        sqlstate::DATETIME_FIELD_OVERFLOW,
        "date out of range for timestamp",
    )
}

impl Date {
    /// `infinity`.
    pub const INFINITY: Date = Date(i32::MAX);
    /// `-infinity`.
    pub const NEG_INFINITY: Date = Date(i32::MIN);

    /// Whether the value is finite.
    pub fn is_finite(self) -> bool {
        self.0 != i32::MIN && self.0 != i32::MAX
    }

    /// Build from a calendar date (`year` uses astronomical numbering:
    /// 0 = 1 BC). Returns `None` when out of range.
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Date> {
        let (m, d) = (i32::try_from(month).ok()?, i32::try_from(day).ok()?);
        if !(1..=12).contains(&m) || d < 1 || d > crate::calendar::days_in_month(year, m) {
            return None;
        }
        if !is_valid_julian(year, m, d) {
            return None;
        }
        let v = date2j(year, m, d) - POSTGRES_EPOCH_JDATE;
        is_valid_date(v).then_some(Date(v))
    }

    /// Calendar date `(year, month, day)` (astronomical year numbering).
    pub fn to_ymd(self) -> (i32, u32, u32) {
        let (y, m, d) = j2date(self.0.wrapping_add(POSTGRES_EPOCH_JDATE));
        #[allow(clippy::cast_sign_loss)]
        (y, m as u32, d as u32)
    }

    /// `date_in`.
    pub fn parse(input: &str, env: &DateTimeEnv<'_>) -> Result<Date> {
        let fields = parse_datetime(input, MAXDATELEN + 1).map_err(|e| e.report(input, "date"))?;
        let d = decode_date_time(&fields, env).map_err(|e| e.report(input, "date"))?;
        let tm = match d.dtype {
            DTK_DATE => d.tm,
            DTK_EPOCH => Tm {
                year: 1970,
                mon: 1,
                mday: 1,
                ..Tm::default()
            },
            DTK_LATE => return Ok(Date::INFINITY),
            DTK_EARLY => return Ok(Date::NEG_INFINITY),
            _ => return Err(DtErr::BadFormat.report(input, "date")),
        };
        let range_err = || {
            DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                format!("date out of range: \"{input}\""),
            )
        };
        if !is_valid_julian(tm.year, tm.mon, tm.mday) {
            return Err(range_err());
        }
        let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
        if !is_valid_date(date) {
            return Err(range_err());
        }
        Ok(Date(date))
    }

    /// `date_out` using the session `DateStyle`.
    pub fn format(self, env: &DateTimeEnv<'_>) -> String {
        match self.0 {
            i32::MIN => "-infinity".to_owned(),
            i32::MAX => "infinity".to_owned(),
            d => {
                let (year, mon, mday) = j2date(d.wrapping_add(POSTGRES_EPOCH_JDATE));
                let tm = Tm {
                    year,
                    mon,
                    mday,
                    ..Tm::default()
                };
                encode_date_only(&tm, env.date_style, env.date_order)
            }
        }
    }

    /// `date + integer`.
    pub fn add_days(self, days: i32) -> Result<Date> {
        if !self.is_finite() {
            return Ok(self);
        }
        let result = self.0.wrapping_add(days);
        let overflow = if days >= 0 {
            result < self.0
        } else {
            result > self.0
        };
        if overflow || !is_valid_date(result) {
            return Err(DateTimeError::date_out_of_range());
        }
        Ok(Date(result))
    }

    /// `date - integer`.
    pub fn sub_days(self, days: i32) -> Result<Date> {
        if !self.is_finite() {
            return Ok(self);
        }
        let result = self.0.wrapping_sub(days);
        let overflow = if days >= 0 {
            result > self.0
        } else {
            result < self.0
        };
        if overflow || !is_valid_date(result) {
            return Err(DateTimeError::date_out_of_range());
        }
        Ok(Date(result))
    }

    /// `date - date` (number of days).
    pub fn sub_date(self, other: Date) -> Result<i32> {
        if !self.is_finite() || !other.is_finite() {
            return Err(DateTimeError::new(
                sqlstate::DATETIME_FIELD_OVERFLOW,
                "cannot subtract infinite dates",
            ));
        }
        Ok(self.0.wrapping_sub(other.0))
    }

    /// `date::timestamp`.
    pub fn to_timestamp(self) -> Result<Timestamp> {
        match self.0 {
            i32::MIN => Ok(Timestamp::NEG_INFINITY),
            i32::MAX => Ok(Timestamp::INFINITY),
            d => {
                if d >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE {
                    return Err(out_of_range_for_timestamp());
                }
                Ok(Timestamp(i64::from(d) * USECS_PER_DAY))
            }
        }
    }

    /// `date::timestamptz` (midnight in `tz`).
    pub fn to_timestamptz(self, tz: &TimeZone) -> Result<TimestampTz> {
        match self.0 {
            i32::MIN => Ok(TimestampTz::NEG_INFINITY),
            i32::MAX => Ok(TimestampTz::INFINITY),
            d => {
                if d >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE {
                    return Err(out_of_range_for_timestamp());
                }
                let (year, mon, mday) = j2date(d + POSTGRES_EPOCH_JDATE);
                let mut tm = Tm {
                    year,
                    mon,
                    mday,
                    ..Tm::default()
                };
                let off = tz.determine_offset(&mut tm);
                let result = i64::from(d) * USECS_PER_DAY + i64::from(off) * USECS_PER_SEC;
                if !is_valid_timestamp(result) {
                    return Err(out_of_range_for_timestamp());
                }
                Ok(TimestampTz(result))
            }
        }
    }

    /// `date + interval` (a timestamp).
    pub fn add_interval(self, span: &Interval) -> Result<Timestamp> {
        self.to_timestamp()?.add_interval(span)
    }

    /// `date - interval` (a timestamp).
    pub fn sub_interval(self, span: &Interval) -> Result<Timestamp> {
        self.to_timestamp()?.sub_interval(span)
    }
}
