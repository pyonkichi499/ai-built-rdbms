//! Calendar arithmetic and constants (port of the relevant parts of
//! `datatype/timestamp.h`, `datetime.c` and `timestamp.c`).
//!
//! Everything uses the proleptic Gregorian calendar. Internally year 0 is
//! 1 BC, year -1 is 2 BC and so on (as in PostgreSQL's `pg_tm`).

pub(crate) const USECS_PER_DAY: i64 = 86_400_000_000;
pub(crate) const USECS_PER_HOUR: i64 = 3_600_000_000;
pub(crate) const USECS_PER_MINUTE: i64 = 60_000_000;
pub(crate) const USECS_PER_SEC: i64 = 1_000_000;
pub(crate) const SECS_PER_DAY: i64 = 86_400;
pub(crate) const SECS_PER_HOUR: i64 = 3_600;
pub(crate) const SECS_PER_MINUTE: i64 = 60;
pub(crate) const MINS_PER_HOUR: i64 = 60;
pub(crate) const HOURS_PER_DAY: i32 = 24;
pub(crate) const MONTHS_PER_YEAR: i32 = 12;
pub(crate) const DAYS_PER_MONTH: i32 = 30;

/// Maximum fractional-second precision of timestamp and interval typmods.
pub const MAX_PRECISION: i32 = 6;
pub(crate) const MAX_TZDISP_HOUR: i32 = 15;

pub(crate) const UNIX_EPOCH_JDATE: i32 = 2_440_588;
pub(crate) const POSTGRES_EPOCH_JDATE: i32 = 2_451_545;

pub(crate) const JULIAN_MINYEAR: i32 = -4713;
pub(crate) const JULIAN_MINMONTH: i32 = 11;
pub(crate) const JULIAN_MAXYEAR: i32 = 5_874_898;
pub(crate) const JULIAN_MAXMONTH: i32 = 6;

pub(crate) const DATETIME_MIN_JULIAN: i32 = 0;
pub(crate) const DATE_END_JULIAN: i32 = 2_147_483_494;
pub(crate) const TIMESTAMP_END_JULIAN: i32 = 109_203_528;

pub(crate) const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
pub(crate) const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

/// Seconds between 1970-01-01 and 2000-01-01.
pub(crate) const EPOCH_DIFF_SECS: i64 =
    (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i64 * SECS_PER_DAY;

pub(crate) const DAY_TAB: [[i32; 13]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
];

pub(crate) const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub(crate) const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub(crate) fn isleap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

pub(crate) fn days_in_month(year: i32, mon: i32) -> i32 {
    DAY_TAB[usize::from(isleap(year))][(mon - 1) as usize]
}

pub(crate) fn is_valid_julian(y: i32, m: i32, _d: i32) -> bool {
    (y > JULIAN_MINYEAR || (y == JULIAN_MINYEAR && m >= JULIAN_MINMONTH))
        && (y < JULIAN_MAXYEAR || (y == JULIAN_MAXYEAR && m < JULIAN_MAXMONTH))
}

pub(crate) fn is_valid_date(d: i32) -> bool {
    ((DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE)..(DATE_END_JULIAN - POSTGRES_EPOCH_JDATE))
        .contains(&d)
}

pub(crate) fn is_valid_timestamp(t: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&t)
}

/// `date2j`: calendar date to Julian day number. Uses wrapping 32-bit
/// arithmetic like the C original (callers range-check first).
pub(crate) fn date2j(year: i32, month: i32, day: i32) -> i32 {
    let (mut year, mut month) = (year, month);
    if month > 2 {
        month = month.wrapping_add(1);
        year = year.wrapping_add(4800);
    } else {
        month = month.wrapping_add(13);
        year = year.wrapping_add(4799);
    }
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian.wrapping_add(year / 4 - century + century / 4);
    julian = julian.wrapping_add(7834i32.wrapping_mul(month) / 256 + day);
    julian
}

/// `j2date`: Julian day number to calendar date (unsigned arithmetic as in C).
pub(crate) fn j2date(jd: i32) -> (i32, i32, i32) {
    #[allow(clippy::cast_sign_loss)]
    let mut julian: u32 = jd as u32;
    julian = julian.wrapping_add(32044);
    let mut quad = julian / 146_097;
    let extra = (julian.wrapping_sub(quad.wrapping_mul(146_097)))
        .wrapping_mul(4)
        .wrapping_add(3);
    julian = julian
        .wrapping_add(60)
        .wrapping_add(quad.wrapping_mul(3))
        .wrapping_add(extra / 146_097);
    quad = julian / 1461;
    julian = julian.wrapping_sub(quad.wrapping_mul(1461));
    let mut y = julian.wrapping_mul(4) / 1461;
    julian = if y != 0 {
        (julian + 305) % 365
    } else {
        (julian + 306) % 366
    } + 123;
    y = y.wrapping_add(quad.wrapping_mul(4));
    #[allow(clippy::cast_possible_wrap)]
    let year = (y as i32).wrapping_sub(4800);
    let q = julian.wrapping_mul(2141) / 65536;
    #[allow(clippy::cast_possible_wrap)]
    let day = julian.wrapping_sub(7834u32.wrapping_mul(q) / 256) as i32;
    #[allow(clippy::cast_possible_wrap)]
    let month = ((q + 10) % 12 + 1) as i32;
    (year, month, day)
}

/// `j2day`: day of week (0 = Sunday).
pub(crate) fn j2day(date: i32) -> i32 {
    let mut d = date.wrapping_add(1) % 7;
    if d < 0 {
        d += 7;
    }
    d
}

pub(crate) fn isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4 - 1);
    ((week - 1) * 7) + (day4 - day0)
}

pub(crate) fn isoweek2date(woy: i32, year: i32) -> (i32, i32, i32) {
    j2date(isoweek2j(year, woy))
}

pub(crate) fn date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
    }
    let mut result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            result = (dayn - (day4 - day0)) / 7 + 1;
        }
    }
    result
}

pub(crate) fn date2isoyear(year: i32, mon: i32, mday: i32) -> i32 {
    let mut year = year;
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
        year -= 1;
    }
    let result = (dayn - (day4 - day0)) / 7 + 1;
    if result >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            year += 1;
        }
    }
    year
}

/// Broken-down time (`struct pg_tm`). `year` is the full year (0 = 1 BC),
/// `mon` is 1-based.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Tm {
    pub year: i32,
    pub mon: i32,
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    pub yday: i32,
    pub isdst: i32,
}

/// `struct pg_itm`: broken-down interval for output.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Itm {
    pub usec: i32,
    pub sec: i32,
    pub min: i32,
    pub hour: i64,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
}

/// `struct pg_itm_in`: broken-down interval for input.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ItmIn {
    pub usec: i64,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
}

/// `dt2time`: split microseconds of the day into h/m/s/µs.
pub(crate) fn dt2time(jd: i64) -> (i32, i32, i32, i32) {
    let mut time = jd;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    let fsec = time - sec * USECS_PER_SEC;
    #[allow(clippy::cast_possible_truncation)]
    (hour as i32, min as i32, sec as i32, fsec as i32)
}

/// `time2t`.
pub(crate) fn time2t(hour: i32, min: i32, sec: i32, fsec: i32) -> i64 {
    (((i64::from(hour) * MINS_PER_HOUR + i64::from(min)) * SECS_PER_MINUTE + i64::from(sec))
        * USECS_PER_SEC)
        + i64::from(fsec)
}

/// `timestamp2tm` without time zone conversion. Returns `None` when out of
/// range for the Julian-day routines.
pub(crate) fn timestamp2tm_utc(dt: i64) -> Option<(Tm, i32)> {
    let mut time = dt;
    let mut date = time / USECS_PER_DAY;
    if date != 0 {
        time -= date * USECS_PER_DAY;
    }
    if time < 0 {
        time += USECS_PER_DAY;
        date -= 1;
    }
    date += i64::from(POSTGRES_EPOCH_JDATE);
    if date < 0 || date > i64::from(i32::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let (year, mon, mday) = j2date(date as i32);
    let (hour, min, sec, fsec) = dt2time(time);
    Some((
        Tm {
            year,
            mon,
            mday,
            hour,
            min,
            sec,
            yday: 0,
            isdst: -1,
        },
        fsec,
    ))
}

/// `tm2timestamp`. `tz` is PostgreSQL's sign convention (seconds *west* of
/// UTC), applied when given.
pub(crate) fn tm2timestamp(tm: &Tm, fsec: i32, tz: Option<i32>) -> Option<i64> {
    if !is_valid_julian(tm.year, tm.mon, tm.mday) {
        return None;
    }
    let date = i64::from(date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE);
    let time = time2t(tm.hour, tm.min, tm.sec, fsec);
    let mut result = date.wrapping_mul(USECS_PER_DAY).wrapping_add(time);
    if (result.wrapping_sub(time)) / USECS_PER_DAY != date {
        return None;
    }
    if (result < 0 && date > 0) || (result > 0 && date < -1) {
        return None;
    }
    if let Some(tz) = tz {
        result = result.wrapping_add(i64::from(tz) * USECS_PER_SEC);
    }
    if !is_valid_timestamp(result) {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn julian_roundtrip() {
        assert_eq!(date2j(2000, 1, 1), POSTGRES_EPOCH_JDATE);
        assert_eq!(date2j(1970, 1, 1), UNIX_EPOCH_JDATE);
        assert_eq!(date2j(294_277, 1, 1), TIMESTAMP_END_JULIAN);
        assert_eq!(date2j(JULIAN_MAXYEAR, 1, 1), DATE_END_JULIAN);
        for jd in [0, 1, 100, 1_721_426, 2_451_545, 2_460_000, 100_000_000] {
            let (y, m, d) = j2date(jd);
            assert_eq!(date2j(y, m, d), jd);
        }
        assert_eq!(j2day(POSTGRES_EPOCH_JDATE), 6); // 2000-01-01 was a Saturday
    }
}
