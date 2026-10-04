//! Session settings that influence date/time I/O: `DateStyle`,
//! `IntervalStyle` and `TimeZone`.

use crate::error::{DateTimeError, Result, sqlstate};
use crate::timestamp::TimestampTz;
use crate::tz::{TimeZone, ZoneDb};

/// Output style part of `DateStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateStyle {
    /// `ISO` (default): `2024-01-02 03:04:05+09`.
    #[default]
    Iso,
    /// `SQL`: `01/02/2024 03:04:05 JST`.
    Sql,
    /// `Postgres`: `Tue Jan 02 03:04:05 2024 JST`.
    Postgres,
    /// `German`: `02.01.2024 03:04:05 JST`.
    German,
}

/// Field-order part of `DateStyle` (used for input and some outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DateOrder {
    /// Year-month-day.
    Ymd,
    /// Day-month-year.
    Dmy,
    /// Month-day-year (default).
    #[default]
    Mdy,
}

/// `IntervalStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IntervalStyle {
    /// `postgres` (default): `1 year 2 mons 3 days 04:05:06`.
    #[default]
    Postgres,
    /// `postgres_verbose`: `@ 1 year 2 mons 3 days 4 hours 5 mins 6 secs`.
    PostgresVerbose,
    /// `sql_standard`: `+1-2 +3 +4:05:06`.
    SqlStandard,
    /// `iso_8601`: `P1Y2M3DT4H5M6S`.
    Iso8601,
}

/// Everything date/time I/O and arithmetic needs from the session.
#[derive(Debug, Clone, Copy)]
pub struct DateTimeEnv<'a> {
    /// Output style.
    pub date_style: DateStyle,
    /// Input (and some output) field order.
    pub date_order: DateOrder,
    /// Interval output style (`sql_standard` also affects input).
    pub interval_style: IntervalStyle,
    /// Session time zone.
    pub time_zone: &'a TimeZone,
    /// Zone database for zone names appearing in input.
    pub zones: &'a ZoneDb,
    /// Transaction start time (for `now`, `today`, ...).
    pub now: TimestampTz,
}

impl<'a> DateTimeEnv<'a> {
    /// Default settings (`ISO, MDY`, `postgres`) with the given zone.
    pub fn new(time_zone: &'a TimeZone, zones: &'a ZoneDb) -> Self {
        Self {
            date_style: DateStyle::Iso,
            date_order: DateOrder::Mdy,
            interval_style: IntervalStyle::Postgres,
            time_zone,
            zones,
            now: TimestampTz(0),
        }
    }
}

fn invalid_value(param: &str, value: &str) -> DateTimeError {
    DateTimeError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{param}\": \"{value}\""),
    )
}

/// Parse a `DateStyle` value (`check_datestyle`). Unspecified parts keep the
/// given current values. `DEFAULT` means `ISO, MDY`.
pub fn parse_datestyle(
    value: &str,
    current: (DateStyle, DateOrder),
) -> Result<(DateStyle, DateOrder)> {
    let (mut style, mut order) = current;
    let mut have_style = false;
    let mut have_order = false;
    let mut ok = true;
    for raw in value.split(',') {
        let tok = raw.trim();
        let tok = tok
            .strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .unwrap_or(tok);
        let lower = tok.to_ascii_lowercase();
        let mut set_style = |s: DateStyle, have_style: &mut bool| {
            if *have_style && style != s {
                ok = false;
            }
            style = s;
            *have_style = true;
        };
        if lower == "iso" {
            set_style(DateStyle::Iso, &mut have_style);
        } else if lower == "sql" {
            set_style(DateStyle::Sql, &mut have_style);
        } else if lower.starts_with("postgres") {
            set_style(DateStyle::Postgres, &mut have_style);
        } else if lower == "german" {
            set_style(DateStyle::German, &mut have_style);
            if !have_order {
                order = DateOrder::Dmy;
            }
        } else if lower == "ymd" {
            if have_order && order != DateOrder::Ymd {
                ok = false;
            }
            order = DateOrder::Ymd;
            have_order = true;
        } else if lower == "dmy" || lower.starts_with("euro") {
            if have_order && order != DateOrder::Dmy {
                ok = false;
            }
            order = DateOrder::Dmy;
            have_order = true;
        } else if lower == "mdy" || lower == "us" || lower.starts_with("noneuro") {
            if have_order && order != DateOrder::Mdy {
                ok = false;
            }
            order = DateOrder::Mdy;
            have_order = true;
        } else if lower == "default" {
            if !have_style {
                style = DateStyle::Iso;
            }
            if !have_order {
                order = DateOrder::Mdy;
            }
        } else {
            return Err(invalid_value("DateStyle", value)
                .with_detail(format!("Unrecognized key word: \"{tok}\".")));
        }
    }
    if !ok {
        return Err(invalid_value("DateStyle", value)
            .with_detail("Conflicting \"datestyle\" specifications."));
    }
    Ok((style, order))
}

/// Canonical `DateStyle` string, e.g. `ISO, MDY`.
pub fn format_datestyle(style: DateStyle, order: DateOrder) -> String {
    let s = match style {
        DateStyle::Iso => "ISO",
        DateStyle::Sql => "SQL",
        DateStyle::German => "German",
        DateStyle::Postgres => "Postgres",
    };
    let o = match order {
        DateOrder::Ymd => "YMD",
        DateOrder::Dmy => "DMY",
        DateOrder::Mdy => "MDY",
    };
    format!("{s}, {o}")
}

/// Parse an `IntervalStyle` value.
pub fn parse_intervalstyle(value: &str) -> Result<IntervalStyle> {
    match value.trim().to_ascii_lowercase().as_str() {
        "postgres" => Ok(IntervalStyle::Postgres),
        "postgres_verbose" => Ok(IntervalStyle::PostgresVerbose),
        "sql_standard" => Ok(IntervalStyle::SqlStandard),
        "iso_8601" => Ok(IntervalStyle::Iso8601),
        _ => Err(invalid_value("IntervalStyle", value)
            .with_hint("Available values: postgres, postgres_verbose, sql_standard, iso_8601.")),
    }
}

/// Canonical `IntervalStyle` string.
pub fn format_intervalstyle(style: IntervalStyle) -> &'static str {
    match style {
        IntervalStyle::Postgres => "postgres",
        IntervalStyle::PostgresVerbose => "postgres_verbose",
        IntervalStyle::SqlStandard => "sql_standard",
        IntervalStyle::Iso8601 => "iso_8601",
    }
}

/// Resolve a `TimeZone` setting (`check_timezone`): a zone name, a POSIX
/// specification, a number of hours (`+9`, `-3.5`; ISO sign convention) or
/// `INTERVAL '...'`.
pub fn parse_timezone_setting(zones: &ZoneDb, value: &str) -> Result<TimeZone> {
    let bad = || invalid_value("TimeZone", value);
    let tz = if value.len() >= 8 && value[..8].eq_ignore_ascii_case("interval") {
        let rest = value[8..].trim_start();
        let inner = rest
            .strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .filter(|r| !r.contains('\''))
            .ok_or_else(bad)?;
        let utc = TimeZone::utc();
        let env = DateTimeEnv::new(&utc, zones);
        let iv = crate::interval::Interval::parse(inner, -1, &env)?;
        if iv.months != 0 {
            return Err(bad().with_detail("Cannot specify months in time zone interval."));
        }
        if iv.days != 0 {
            return Err(bad().with_detail("Cannot specify days in time zone interval."));
        }
        let gmtoffset = -(iv.micros / crate::calendar::USECS_PER_SEC);
        TimeZone::from_posix_offset(gmtoffset)
    } else {
        let (hours, n, _) = crate::cnum::strtod(value.as_bytes());
        if n > 0 && n == value.len() {
            if hours.is_finite() {
                let gmtoffset = (-hours * 3600.0) as i64;
                TimeZone::from_posix_offset(gmtoffset)
            } else {
                None
            }
        } else {
            let tz = zones.lookup(value).ok_or_else(bad)?;
            if tz.has_leap_seconds() {
                return Err(DateTimeError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("time zone \"{value}\" appears to use leap seconds"),
                )
                .with_detail("PostgreSQL does not support leap seconds."));
            }
            Some(tz)
        }
    };
    tz.ok_or_else(|| bad().with_detail("UTC timezone offset is out of range."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datestyle_parsing() {
        let cur = (DateStyle::Iso, DateOrder::Mdy);
        assert_eq!(
            parse_datestyle("Postgres, DMY", cur).unwrap(),
            (DateStyle::Postgres, DateOrder::Dmy)
        );
        assert_eq!(
            parse_datestyle("german", cur).unwrap(),
            (DateStyle::German, DateOrder::Dmy)
        );
        assert_eq!(
            parse_datestyle("ymd", cur).unwrap(),
            (DateStyle::Iso, DateOrder::Ymd)
        );
        assert!(parse_datestyle("iso, sql", cur).is_err());
        assert!(parse_datestyle("foo", cur).is_err());
        assert_eq!(
            format_datestyle(DateStyle::Postgres, DateOrder::Dmy),
            "Postgres, DMY"
        );
    }
}
