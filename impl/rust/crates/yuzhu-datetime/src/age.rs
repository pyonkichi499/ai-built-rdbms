//! `age(timestamp, timestamp)` and `age(timestamptz, timestamptz)`: a port of
//! `timestamp_age` / `timestamptz_age` (PostgreSQL `timestamp.c`).
//!
//! The result is the "symbolic" difference of the two wall-clock values,
//! with borrows propagated upward using the length of the relevant month.
//! `timestamptz` values are broken down in the session zone, so the result
//! is a difference of wall clocks (a DST change does not show up).

use crate::calendar::{
    MONTHS_PER_YEAR, Tm, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC, days_in_month,
    timestamp2tm_utc,
};
use crate::error::{DateTimeError, Result};
use crate::interval::Interval;
use crate::timestamp::{Timestamp, TimestampTz};
use crate::tz::TimeZone;

/// Field-wise difference `tm1 - tm2` with borrows (the body shared by both
/// `age` functions). `less` is `dt1 < dt2`.
fn age_fields(tm1: &Tm, fsec1: i32, tm2: &Tm, fsec2: i32, less: bool) -> Result<Interval> {
    let mut fsec = fsec1 - fsec2;
    let mut sec = tm1.sec - tm2.sec;
    let mut min = tm1.min - tm2.min;
    let mut hour = tm1.hour - tm2.hour;
    let mut mday = tm1.mday - tm2.mday;
    let mut mon = tm1.mon - tm2.mon;
    let mut year = tm1.year - tm2.year;

    if less {
        fsec = -fsec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }

    while fsec < 0 {
        fsec += USECS_PER_SEC as i32;
        sec -= 1;
    }
    while sec < 0 {
        sec += 60;
        min -= 1;
    }
    while min < 0 {
        min += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        mday -= 1;
    }
    while mday < 0 {
        if less {
            mday += days_in_month(tm1.year, tm1.mon);
        } else {
            mday += days_in_month(tm2.year, tm2.mon);
        }
        mon -= 1;
    }
    while mon < 0 {
        mon += MONTHS_PER_YEAR;
        year -= 1;
    }

    if less {
        fsec = -fsec;
        sec = -sec;
        min = -min;
        hour = -hour;
        mday = -mday;
        mon = -mon;
        year = -year;
    }

    itm_to_interval(year, mon, mday, hour, min, sec, fsec)
}

/// `itm2interval`.
fn itm_to_interval(
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    fsec: i32,
) -> Result<Interval> {
    let overflow = DateTimeError::interval_out_of_range;
    let total_months = i64::from(year) * i64::from(MONTHS_PER_YEAR) + i64::from(mon);
    let months = i32::try_from(total_months).map_err(|_| overflow())?;
    let micros = i64::from(hour)
        .checked_mul(USECS_PER_HOUR)
        .and_then(|v| v.checked_add(i64::from(min) * USECS_PER_MINUTE))
        .and_then(|v| v.checked_add(i64::from(sec) * USECS_PER_SEC))
        .and_then(|v| v.checked_add(i64::from(fsec)))
        .ok_or_else(overflow)?;
    Ok(Interval {
        months,
        days: mday,
        micros,
    })
}

impl Timestamp {
    /// `age(timestamp, timestamp)`: `self` is the first argument.
    pub fn age(self, other: Timestamp) -> Result<Interval> {
        if !self.is_finite() || !other.is_finite() {
            return self.sub_timestamp(other);
        }
        let (tm1, f1) =
            timestamp2tm_utc(self.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        let (tm2, f2) =
            timestamp2tm_utc(other.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
        age_fields(&tm1, f1, &tm2, f2, self.0 < other.0)
    }
}

impl TimestampTz {
    /// `age(timestamptz, timestamptz)` with both values broken down in `tz`.
    pub fn age(self, other: TimestampTz, tz: &TimeZone) -> Result<Interval> {
        if !self.is_finite() || !other.is_finite() {
            return self.sub_timestamptz(other);
        }
        let l1 = tz
            .to_local(self.0)
            .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        let l2 = tz
            .to_local(other.0)
            .ok_or_else(DateTimeError::timestamp_out_of_range)?;
        age_fields(&l1.tm, l1.fsec, &l2.tm, l2.fsec, self.0 < other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DateTimeEnv;
    use crate::tz::ZoneDb;

    fn ts(s: &str) -> Timestamp {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        Timestamp::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap()
    }

    fn tstz(s: &str) -> TimestampTz {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        TimestampTz::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap()
    }

    fn show(i: Interval) -> String {
        i.format(crate::settings::IntervalStyle::Postgres)
    }

    #[test]
    fn age_matches_postgres_examples() {
        assert_eq!(
            show(ts("2024-03-15 10:00").age(ts("2000-02-29 12:30")).unwrap()),
            "24 years 14 days 21:30:00"
        );
        assert_eq!(
            show(ts("2024-01-31").age(ts("2024-03-01")).unwrap()),
            "-1 mons -1 days"
        );
        assert_eq!(
            show(
                tstz("2024-03-10 12:00+00")
                    .age(tstz("2024-03-09 12:00+00"), &TimeZone::utc())
                    .unwrap()
            ),
            "1 day"
        );
    }

    #[test]
    fn age_of_equal_values_is_zero() {
        assert_eq!(
            show(ts("2024-05-05").age(ts("2024-05-05")).unwrap()),
            "00:00:00"
        );
    }

    #[test]
    fn age_with_infinity() {
        assert!(
            Timestamp(i64::MAX)
                .age(ts("2024-01-01"))
                .unwrap()
                .is_infinity()
        );
        assert!(Timestamp(i64::MAX).age(Timestamp(i64::MAX)).is_err());
    }
}
