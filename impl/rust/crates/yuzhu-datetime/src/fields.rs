//! `extract`, `date_part` and `date_trunc`.

use std::fmt;

use crate::calendar::{
    MINS_PER_HOUR, MONTHS_PER_YEAR, POSTGRES_EPOCH_JDATE, SECS_PER_DAY, SECS_PER_HOUR,
    SECS_PER_MINUTE, Tm, UNIX_EPOCH_JDATE, date2isoweek, date2isoyear, date2j, isoweek2date,
    j2date, j2day, timestamp2tm_utc, tm2timestamp,
};
use crate::date::Date;
use crate::error::{DateTimeError, Result, sqlstate};
use crate::interval::Interval;
use crate::parse::decode_units;
use crate::timestamp::{EPOCH_TIMESTAMP, Timestamp, TimestampTz};
#[allow(clippy::wildcard_imports)]
use crate::tokens::*;
use crate::tz::TimeZone;

/// A `numeric` result of `extract`: `mantissa / 10^scale` with exactly
/// `scale` fractional digits displayed, or a signed infinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericValue {
    /// A finite value.
    Finite {
        /// Unscaled value.
        mantissa: i128,
        /// Number of fractional digits.
        scale: u32,
    },
    /// `Infinity`.
    Infinity,
    /// `-Infinity`.
    NegInfinity,
}

impl NumericValue {
    fn int(v: i64) -> Self {
        NumericValue::Finite {
            mantissa: i128::from(v),
            scale: 0,
        }
    }

    fn scaled(v: i128, scale: u32) -> Self {
        NumericValue::Finite { mantissa: v, scale }
    }

    /// Approximate value as `f64`.
    #[allow(clippy::cast_precision_loss)]
    pub fn to_f64(&self) -> f64 {
        match *self {
            NumericValue::Finite { mantissa, scale } => {
                mantissa as f64 / 10f64.powi(i32::try_from(scale).unwrap_or(0))
            }
            NumericValue::Infinity => f64::INFINITY,
            NumericValue::NegInfinity => f64::NEG_INFINITY,
        }
    }
}

impl fmt::Display for NumericValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NumericValue::Infinity => f.write_str("Infinity"),
            NumericValue::NegInfinity => f.write_str("-Infinity"),
            NumericValue::Finite { mantissa, scale } => {
                let digits = mantissa.unsigned_abs().to_string();
                let scale = scale as usize;
                if mantissa < 0 {
                    f.write_str("-")?;
                }
                if scale == 0 {
                    return f.write_str(&digits);
                }
                let padded = if digits.len() <= scale {
                    format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits)
                } else {
                    digits
                };
                let (i, frac) = padded.split_at(padded.len() - scale);
                write!(f, "{i}.{frac}")
            }
        }
    }
}

enum Part {
    Numeric(NumericValue),
    Float(f64),
}

const TS_NAME: &str = "timestamp without time zone";
const TSTZ_NAME: &str = "timestamp with time zone";
const IV_NAME: &str = "interval";
const DATE_NAME: &str = "date";

fn lower_units(units: &str) -> String {
    let mut s: String = units.to_ascii_lowercase();
    if s.len() > 63 {
        let mut cut = 63;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn not_supported(lowunits: &str, ty: &str) -> DateTimeError {
    DateTimeError::new(
        sqlstate::FEATURE_NOT_SUPPORTED,
        format!("unit \"{lowunits}\" not supported for type {ty}"),
    )
}

fn not_recognized(lowunits: &str, ty: &str) -> DateTimeError {
    DateTimeError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("unit \"{lowunits}\" not recognized for type {ty}"),
    )
}

fn decode_part_units(lowunits: &str) -> (i32, i32) {
    let (ty, val) = decode_units(lowunits.as_bytes());
    if ty == UNKNOWN_FIELD {
        match search(lowunits.as_bytes(), DATETKTBL) {
            Some(t) => (t.ty, t.value),
            None => (UNKNOWN_FIELD, 0),
        }
    } else {
        (ty, val)
    }
}

/// `NonFiniteTimestampTzPart`: `None` means NULL, `Some(true)` +inf.
fn non_finite_timestamp_part(
    ty: i32,
    unit: i32,
    lowunits: &str,
    tyname: &str,
) -> Result<Option<bool>> {
    if ty != UNITS && ty != RESERV {
        return Err(not_recognized(lowunits, tyname));
    }
    match unit {
        DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_HOUR | DTK_DAY | DTK_MONTH
        | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY | DTK_TZ | DTK_TZ_MINUTE
        | DTK_TZ_HOUR => Ok(None),
        DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
        | DTK_EPOCH => Ok(Some(true)),
        _ => Err(not_supported(lowunits, tyname)),
    }
}

fn infinite_part(positive: bool, retnumeric: bool) -> Part {
    match (positive, retnumeric) {
        (true, true) => Part::Numeric(NumericValue::Infinity),
        (false, true) => Part::Numeric(NumericValue::NegInfinity),
        (true, false) => Part::Float(f64::INFINITY),
        (false, false) => Part::Float(f64::NEG_INFINITY),
    }
}

/// Julian day with fraction, as `numeric` (`numeric_add(jd,
/// numeric_div(usec, 86400000000))`).
fn julian_numeric(jd: i64, usec_of_day: i64) -> NumericValue {
    // select_div_scale with NBASE = 10000.
    let (weight1, first1) = if usec_of_day == 0 {
        (0i32, 0i64)
    } else {
        let mut w = 0i32;
        let mut v = usec_of_day;
        while v >= 10_000 {
            v /= 10_000;
            w += 1;
        }
        (w, v)
    };
    let (weight2, first2) = (2i32, 864i64);
    let mut qweight = weight1 - weight2;
    if first1 <= first2 {
        qweight -= 1;
    }
    let rscale = (16 - qweight * 4).clamp(0, 1000);
    #[allow(clippy::cast_sign_loss)]
    let rscale = rscale as u32;
    let den: i128 = 86_400_000_000;
    let pow = 10i128.pow(rscale);
    let num = i128::from(usec_of_day) * pow;
    let q = (num * 2 + den) / (2 * den);
    NumericValue::scaled(i128::from(jd) * pow + q, rscale)
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn timestamp_part(
    units: &str,
    ts: i64,
    tz: Option<&TimeZone>,
    retnumeric: bool,
) -> Result<Option<Part>> {
    let tyname = if tz.is_some() { TSTZ_NAME } else { TS_NAME };
    let lowunits = lower_units(units);
    let (ty, val) = decode_part_units(&lowunits);
    if ts == i64::MIN || ts == i64::MAX {
        return Ok(non_finite_timestamp_part(ty, val, &lowunits, tyname)?
            .map(|_| infinite_part(ts == i64::MAX, retnumeric)));
    }
    let intresult: i64;
    if ty == UNITS {
        let (tm, fsec, tzoff) = match tz {
            None => {
                let (tm, fsec) =
                    timestamp2tm_utc(ts).ok_or_else(DateTimeError::timestamp_out_of_range)?;
                (tm, fsec, 0)
            }
            Some(z) => {
                let l = z
                    .to_local(ts)
                    .ok_or_else(DateTimeError::timestamp_out_of_range)?;
                (l.tm, l.fsec, l.tz)
            }
        };
        let usec_in_min = i64::from(tm.sec) * 1_000_000 + i64::from(fsec);
        match val {
            DTK_TZ if tz.is_some() => intresult = -i64::from(tzoff),
            DTK_TZ_MINUTE if tz.is_some() => {
                intresult = (-i64::from(tzoff) / SECS_PER_MINUTE) % MINS_PER_HOUR;
            }
            DTK_TZ_HOUR if tz.is_some() => intresult = -i64::from(tzoff) / SECS_PER_HOUR,
            DTK_MICROSEC => intresult = usec_in_min,
            DTK_MILLISEC => {
                return Ok(Some(if retnumeric {
                    Part::Numeric(NumericValue::scaled(i128::from(usec_in_min), 3))
                } else {
                    Part::Float(f64::from(tm.sec) * 1000.0 + f64::from(fsec) / 1000.0)
                }));
            }
            DTK_SECOND => {
                return Ok(Some(if retnumeric {
                    Part::Numeric(NumericValue::scaled(i128::from(usec_in_min), 6))
                } else {
                    Part::Float(f64::from(tm.sec) + f64::from(fsec) / 1_000_000.0)
                }));
            }
            DTK_MINUTE => intresult = i64::from(tm.min),
            DTK_HOUR => intresult = i64::from(tm.hour),
            DTK_DAY => intresult = i64::from(tm.mday),
            DTK_MONTH => intresult = i64::from(tm.mon),
            DTK_QUARTER => intresult = i64::from((tm.mon - 1) / 3 + 1),
            DTK_WEEK => intresult = i64::from(date2isoweek(tm.year, tm.mon, tm.mday)),
            DTK_YEAR => {
                intresult = if tm.year > 0 {
                    i64::from(tm.year)
                } else {
                    i64::from(tm.year) - 1
                };
            }
            DTK_DECADE => {
                intresult = if tm.year >= 0 {
                    i64::from(tm.year / 10)
                } else {
                    -i64::from((8 - (tm.year - 1)) / 10)
                };
            }
            DTK_CENTURY => {
                intresult = if tm.year > 0 {
                    i64::from((tm.year + 99) / 100)
                } else {
                    -i64::from((99 - (tm.year - 1)) / 100)
                };
            }
            DTK_MILLENNIUM => {
                intresult = if tm.year > 0 {
                    i64::from((tm.year + 999) / 1000)
                } else {
                    -i64::from((999 - (tm.year - 1)) / 1000)
                };
            }
            DTK_JULIAN => {
                let jd = i64::from(date2j(tm.year, tm.mon, tm.mday));
                let secs = ((i64::from(tm.hour) * MINS_PER_HOUR) + i64::from(tm.min))
                    * SECS_PER_MINUTE
                    + i64::from(tm.sec);
                return Ok(Some(if retnumeric {
                    Part::Numeric(julian_numeric(jd, secs * 1_000_000 + i64::from(fsec)))
                } else {
                    Part::Float(
                        jd as f64
                            + (secs as f64 + f64::from(fsec) / 1_000_000.0) / SECS_PER_DAY as f64,
                    )
                }));
            }
            DTK_ISOYEAR => {
                let mut r = i64::from(date2isoyear(tm.year, tm.mon, tm.mday));
                if r <= 0 {
                    r -= 1;
                }
                intresult = r;
            }
            DTK_DOW | DTK_ISODOW => {
                let mut r = i64::from(j2day(date2j(tm.year, tm.mon, tm.mday)));
                if val == DTK_ISODOW && r == 0 {
                    r = 7;
                }
                intresult = r;
            }
            DTK_DOY => {
                intresult = i64::from(date2j(tm.year, tm.mon, tm.mday) - date2j(tm.year, 1, 1) + 1);
            }
            _ => return Err(not_supported(&lowunits, tyname)),
        }
    } else if ty == RESERV {
        if val == DTK_EPOCH {
            let epoch = EPOCH_TIMESTAMP;
            return Ok(Some(if retnumeric {
                Part::Numeric(NumericValue::scaled(i128::from(ts) - i128::from(epoch), 6))
            } else if ts < i64::MAX + epoch {
                Part::Float((ts - epoch) as f64 / 1_000_000.0)
            } else {
                Part::Float((ts as f64 - epoch as f64) / 1_000_000.0)
            }));
        }
        return Err(not_supported(&lowunits, tyname));
    } else {
        return Err(not_recognized(&lowunits, tyname));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(Some(if retnumeric {
        Part::Numeric(NumericValue::int(intresult))
    } else {
        Part::Float(intresult as f64)
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn numeric(p: Option<Part>) -> Option<NumericValue> {
    match p {
        Some(Part::Numeric(n)) => Some(n),
        Some(Part::Float(f)) => Some(NumericValue::scaled(
            #[allow(clippy::cast_possible_truncation)]
            {
                f as i128
            },
            0,
        )),
        None => None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn float(p: Option<Part>) -> Option<f64> {
    match p {
        Some(Part::Float(f)) => Some(f),
        Some(Part::Numeric(n)) => Some(n.to_f64()),
        None => None,
    }
}

/// `extract(field from timestamp)`; `None` is SQL NULL.
pub fn extract_timestamp(units: &str, ts: Timestamp) -> Result<Option<NumericValue>> {
    timestamp_part(units, ts.0, None, true).map(numeric)
}

/// `date_part(field, timestamp)`.
pub fn date_part_timestamp(units: &str, ts: Timestamp) -> Result<Option<f64>> {
    timestamp_part(units, ts.0, None, false).map(float)
}

/// `extract(field from timestamptz)` in the session zone `tz`.
pub fn extract_timestamptz(
    units: &str,
    ts: TimestampTz,
    tz: &TimeZone,
) -> Result<Option<NumericValue>> {
    timestamp_part(units, ts.0, Some(tz), true).map(numeric)
}

/// `date_part(field, timestamptz)` in the session zone `tz`.
pub fn date_part_timestamptz(units: &str, ts: TimestampTz, tz: &TimeZone) -> Result<Option<f64>> {
    timestamp_part(units, ts.0, Some(tz), false).map(float)
}

/// `date_part(field, date)` (PostgreSQL casts the date to timestamp).
pub fn date_part_date(units: &str, d: Date) -> Result<Option<f64>> {
    date_part_timestamp(units, d.to_timestamp()?)
}

/// `extract(field from date)`.
pub fn extract_date(units: &str, d: Date) -> Result<Option<NumericValue>> {
    let lowunits = lower_units(units);
    let (ty, val) = decode_part_units(&lowunits);
    if !d.is_finite() && (ty == UNITS || ty == RESERV) {
        return match val {
            DTK_DAY | DTK_MONTH | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY => {
                Ok(None)
            }
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
            | DTK_EPOCH => Ok(Some(if d == Date::NEG_INFINITY {
                NumericValue::NegInfinity
            } else {
                NumericValue::Infinity
            })),
            _ => Err(not_supported(&lowunits, DATE_NAME)),
        };
    }
    let intresult: i64 = if ty == UNITS {
        let (year, mon, mday) = j2date(d.0.wrapping_add(POSTGRES_EPOCH_JDATE));
        match val {
            DTK_DAY => i64::from(mday),
            DTK_MONTH => i64::from(mon),
            DTK_QUARTER => i64::from((mon - 1) / 3 + 1),
            DTK_WEEK => i64::from(date2isoweek(year, mon, mday)),
            DTK_YEAR => {
                if year > 0 {
                    i64::from(year)
                } else {
                    i64::from(year) - 1
                }
            }
            DTK_DECADE => {
                if year >= 0 {
                    i64::from(year / 10)
                } else {
                    -i64::from((8 - (year - 1)) / 10)
                }
            }
            DTK_CENTURY => {
                if year > 0 {
                    i64::from((year + 99) / 100)
                } else {
                    -i64::from((99 - (year - 1)) / 100)
                }
            }
            DTK_MILLENNIUM => {
                if year > 0 {
                    i64::from((year + 999) / 1000)
                } else {
                    -i64::from((999 - (year - 1)) / 1000)
                }
            }
            DTK_JULIAN => i64::from(d.0) + i64::from(POSTGRES_EPOCH_JDATE),
            DTK_ISOYEAR => {
                let mut r = i64::from(date2isoyear(year, mon, mday));
                if r <= 0 {
                    r -= 1;
                }
                r
            }
            DTK_DOW | DTK_ISODOW => {
                let mut r = i64::from(j2day(d.0.wrapping_add(POSTGRES_EPOCH_JDATE)));
                if val == DTK_ISODOW && r == 0 {
                    r = 7;
                }
                r
            }
            DTK_DOY => i64::from(date2j(year, mon, mday) - date2j(year, 1, 1) + 1),
            _ => return Err(not_supported(&lowunits, DATE_NAME)),
        }
    } else if ty == RESERV {
        if val == DTK_EPOCH {
            (i64::from(d.0) + i64::from(POSTGRES_EPOCH_JDATE) - i64::from(UNIX_EPOCH_JDATE))
                * SECS_PER_DAY
        } else {
            return Err(not_supported(&lowunits, DATE_NAME));
        }
    } else {
        return Err(not_recognized(&lowunits, DATE_NAME));
    };
    Ok(Some(NumericValue::int(intresult)))
}

#[allow(clippy::cast_precision_loss)]
fn interval_part(units: &str, iv: &Interval, retnumeric: bool) -> Result<Option<Part>> {
    let lowunits = lower_units(units);
    let (ty, val) = decode_part_units(&lowunits);
    if !iv.is_finite() {
        if ty != UNITS && ty != RESERV {
            return Err(not_recognized(&lowunits, IV_NAME));
        }
        return match val {
            DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_MONTH | DTK_QUARTER => {
                Ok(None)
            }
            DTK_HOUR | DTK_DAY | DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM
            | DTK_EPOCH => Ok(Some(infinite_part(iv.is_infinity(), retnumeric))),
            _ => Err(not_supported(&lowunits, IV_NAME)),
        };
    }
    let intresult: i64;
    if ty == UNITS {
        let tm = iv.to_itm();
        let usec_in_min = i64::from(tm.sec) * 1_000_000 + i64::from(tm.usec);
        match val {
            DTK_MICROSEC => intresult = usec_in_min,
            DTK_MILLISEC => {
                return Ok(Some(if retnumeric {
                    Part::Numeric(NumericValue::scaled(i128::from(usec_in_min), 3))
                } else {
                    Part::Float(f64::from(tm.sec) * 1000.0 + f64::from(tm.usec) / 1000.0)
                }));
            }
            DTK_SECOND => {
                return Ok(Some(if retnumeric {
                    Part::Numeric(NumericValue::scaled(i128::from(usec_in_min), 6))
                } else {
                    Part::Float(f64::from(tm.sec) + f64::from(tm.usec) / 1_000_000.0)
                }));
            }
            DTK_MINUTE => intresult = i64::from(tm.min),
            DTK_HOUR => intresult = tm.hour,
            DTK_DAY => intresult = i64::from(tm.mday),
            DTK_MONTH => intresult = i64::from(tm.mon),
            DTK_QUARTER => intresult = i64::from(tm.mon / 3 + 1),
            DTK_YEAR => intresult = i64::from(tm.year),
            DTK_DECADE => intresult = i64::from(tm.year / 10),
            DTK_CENTURY => intresult = i64::from(tm.year / 100),
            DTK_MILLENNIUM => intresult = i64::from(tm.year / 1000),
            _ => return Err(not_supported(&lowunits, IV_NAME)),
        }
    } else if ty == RESERV && val == DTK_EPOCH {
        let months = i64::from(iv.months);
        let my = i64::from(MONTHS_PER_YEAR);
        if retnumeric {
            let secs_from_day_month =
                (1461 * (months / my) + 120 * (months % my) + 4 * i64::from(iv.days))
                    * (SECS_PER_DAY / 4);
            let v = i128::from(secs_from_day_month) * 1_000_000 + i128::from(iv.micros);
            return Ok(Some(Part::Numeric(NumericValue::scaled(v, 6))));
        }
        let mut result = iv.micros as f64 / 1_000_000.0;
        result += (365.25 * SECS_PER_DAY as f64) * (months / my) as f64;
        result += (30.0 * SECS_PER_DAY as f64) * (months % my) as f64;
        result += (SECS_PER_DAY as f64) * f64::from(iv.days);
        return Ok(Some(Part::Float(result)));
    } else {
        return Err(not_recognized(&lowunits, IV_NAME));
    }
    Ok(Some(if retnumeric {
        Part::Numeric(NumericValue::int(intresult))
    } else {
        Part::Float(intresult as f64)
    }))
}

/// `extract(field from interval)`.
pub fn extract_interval(units: &str, iv: &Interval) -> Result<Option<NumericValue>> {
    interval_part(units, iv, true).map(numeric)
}

/// `date_part(field, interval)`.
pub fn date_part_interval(units: &str, iv: &Interval) -> Result<Option<f64>> {
    interval_part(units, iv, false).map(float)
}

/// Shared truncation of a broken-down time. Returns whether the zone
/// offset must be recomputed (`redotz`).
fn trunc_tm(val: i32, tm: &mut Tm, fsec: &mut i32, lowunits: &str, tyname: &str) -> Result<bool> {
    let mut redotz = false;
    match val {
        DTK_WEEK => {
            let woy = date2isoweek(tm.year, tm.mon, tm.mday);
            if woy >= 52 && tm.mon == 1 {
                tm.year -= 1;
            }
            if woy <= 1 && tm.mon == MONTHS_PER_YEAR {
                tm.year += 1;
            }
            let (y, m, d) = isoweek2date(woy, tm.year);
            tm.year = y;
            tm.mon = m;
            tm.mday = d;
            tm.hour = 0;
            tm.min = 0;
            tm.sec = 0;
            *fsec = 0;
            return Ok(true);
        }
        DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR | DTK_QUARTER | DTK_MONTH
        | DTK_DAY | DTK_HOUR | DTK_MINUTE | DTK_SECOND => {
            if val == DTK_MILLENNIUM {
                tm.year = if tm.year > 0 {
                    ((tm.year + 999) / 1000) * 1000 - 999
                } else {
                    -((999 - (tm.year - 1)) / 1000) * 1000 + 1
                };
            }
            if val == DTK_MILLENNIUM || val == DTK_CENTURY {
                tm.year = if tm.year > 0 {
                    ((tm.year + 99) / 100) * 100 - 99
                } else {
                    -((99 - (tm.year - 1)) / 100) * 100 + 1
                };
            }
            if val == DTK_DECADE {
                tm.year = if tm.year > 0 {
                    (tm.year / 10) * 10
                } else {
                    -((8 - (tm.year - 1)) / 10) * 10
                };
            }
            let rank = |v: i32| match v {
                DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR => 0,
                DTK_QUARTER => 1,
                DTK_MONTH => 2,
                DTK_DAY => 3,
                DTK_HOUR => 4,
                DTK_MINUTE => 5,
                _ => 6,
            };
            let r = rank(val);
            if r <= 0 {
                tm.mon = 1;
            }
            if r <= 1 {
                tm.mon = (3 * ((tm.mon - 1) / 3)) + 1;
            }
            if r <= 2 {
                tm.mday = 1;
            }
            if r <= 3 {
                tm.hour = 0;
                redotz = true;
            }
            if r <= 4 {
                tm.min = 0;
            }
            if r <= 5 {
                tm.sec = 0;
            }
            *fsec = 0;
        }
        DTK_MILLISEC => *fsec = (*fsec / 1000) * 1000,
        DTK_MICROSEC => {}
        _ => return Err(not_supported(lowunits, tyname)),
    }
    Ok(redotz)
}

/// `date_trunc(field, timestamp)`.
pub fn date_trunc_timestamp(units: &str, ts: Timestamp) -> Result<Timestamp> {
    if !ts.is_finite() {
        return Ok(ts);
    }
    let lowunits = lower_units(units);
    let (ty, val) = decode_units(lowunits.as_bytes());
    if ty != UNITS {
        return Err(not_recognized(&lowunits, TS_NAME));
    }
    let (mut tm, mut fsec) =
        timestamp2tm_utc(ts.0).ok_or_else(DateTimeError::timestamp_out_of_range)?;
    trunc_tm(val, &mut tm, &mut fsec, &lowunits, TS_NAME)?;
    tm2timestamp(&tm, fsec, None)
        .map(Timestamp)
        .ok_or_else(DateTimeError::timestamp_out_of_range)
}

/// `date_trunc(field, timestamptz)` in zone `tz`.
pub fn date_trunc_timestamptz(units: &str, ts: TimestampTz, tz: &TimeZone) -> Result<TimestampTz> {
    if !ts.is_finite() {
        return Ok(ts);
    }
    let lowunits = lower_units(units);
    let (ty, val) = decode_units(lowunits.as_bytes());
    if ty != UNITS {
        return Err(not_recognized(&lowunits, TSTZ_NAME));
    }
    let l = tz
        .to_local(ts.0)
        .ok_or_else(DateTimeError::timestamp_out_of_range)?;
    let (mut tm, mut fsec, mut off) = (l.tm, l.fsec, l.tz);
    if trunc_tm(val, &mut tm, &mut fsec, &lowunits, TSTZ_NAME)? {
        off = tz.determine_offset(&mut tm);
    }
    tm2timestamp(&tm, fsec, Some(off))
        .map(TimestampTz)
        .ok_or_else(DateTimeError::timestamp_out_of_range)
}

/// `date_trunc(field, interval)`.
pub fn date_trunc_interval(units: &str, iv: &Interval) -> Result<Interval> {
    if !iv.is_finite() {
        return Ok(*iv);
    }
    let lowunits = lower_units(units);
    let (ty, val) = decode_units(lowunits.as_bytes());
    if ty != UNITS {
        return Err(not_recognized(&lowunits, IV_NAME));
    }
    let mut tm = iv.to_itm();
    match val {
        DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR | DTK_QUARTER | DTK_MONTH
        | DTK_DAY | DTK_HOUR | DTK_MINUTE | DTK_SECOND => {
            if val == DTK_MILLENNIUM {
                tm.year = (tm.year / 1000) * 1000;
            }
            if val == DTK_MILLENNIUM || val == DTK_CENTURY {
                tm.year = (tm.year / 100) * 100;
            }
            if val == DTK_MILLENNIUM || val == DTK_CENTURY || val == DTK_DECADE {
                tm.year = (tm.year / 10) * 10;
            }
            let r = match val {
                DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR => 0,
                DTK_QUARTER => 1,
                DTK_MONTH => 2,
                DTK_DAY => 3,
                DTK_HOUR => 4,
                DTK_MINUTE => 5,
                _ => 6,
            };
            if r <= 0 {
                tm.mon = 0;
            }
            if r <= 1 {
                tm.mon = 3 * (tm.mon / 3);
            }
            if r <= 2 {
                tm.mday = 0;
            }
            if r <= 3 {
                tm.hour = 0;
            }
            if r <= 4 {
                tm.min = 0;
            }
            if r <= 5 {
                tm.sec = 0;
            }
            tm.usec = 0;
        }
        DTK_MILLISEC => tm.usec = (tm.usec / 1000) * 1000,
        DTK_MICROSEC => {}
        _ => {
            let e = not_supported(&lowunits, IV_NAME);
            return Err(if val == DTK_WEEK {
                e.with_detail("Months usually have fractional weeks.")
            } else {
                e
            });
        }
    }
    Interval::from_itm(&tm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_display() {
        assert_eq!(NumericValue::scaled(5_000_000, 6).to_string(), "5.000000");
        assert_eq!(NumericValue::scaled(-500, 6).to_string(), "-0.000500");
        assert_eq!(NumericValue::scaled(0, 3).to_string(), "0.000");
        assert_eq!(NumericValue::int(-7).to_string(), "-7");
    }
}
