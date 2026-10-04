//! `interval`.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use crate::calendar::{
    DAYS_PER_MONTH, Itm, ItmIn, MAX_PRECISION, MONTHS_PER_YEAR, SECS_PER_DAY, USECS_PER_DAY,
    USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC,
};
use crate::cnum::rint;
use crate::error::{DateTimeError, DtErr, Result, sqlstate};
use crate::format::encode_interval;
use crate::parse::{
    DecodedInterval, INTERVAL_FULL_RANGE, decode_interval, decode_iso8601_interval, interval_mask,
    parse_datetime,
};
use crate::settings::{DateTimeEnv, IntervalStyle};
use crate::tokens::{DAY, HOUR, MINUTE, MONTH, SECOND, YEAR};

/// `interval`: a month count, a day count and microseconds, kept separate
/// as in PostgreSQL. All three at their minimum (maximum) values denote
/// `-infinity` (`infinity`).
///
/// Equality, ordering and hashing follow PostgreSQL: values are compared
/// after converting months to 30 days and days to 24 hours, so
/// `'1 day'` equals `'24 hours'`.
#[derive(Debug, Clone, Copy)]
pub struct Interval {
    /// Months (years are stored as 12 months).
    pub months: i32,
    /// Days.
    pub days: i32,
    /// Microseconds.
    pub micros: i64,
}

const INTERVAL_PRECISION_MASK: i32 = 0xFFFF;
const INTERVAL_FULL_PRECISION: i32 = 0xFFFF;

/// Range bits of an `interval` typmod (the `YEAR TO MONTH` style field
/// restrictions).
pub mod range {
    use super::interval_mask;
    use crate::tokens;
    /// No restriction.
    pub const FULL: i32 = super::INTERVAL_FULL_RANGE;
    /// `YEAR`
    pub const YEAR: i32 = interval_mask(tokens::YEAR);
    /// `MONTH`
    pub const MONTH: i32 = interval_mask(tokens::MONTH);
    /// `DAY`
    pub const DAY: i32 = interval_mask(tokens::DAY);
    /// `HOUR`
    pub const HOUR: i32 = interval_mask(tokens::HOUR);
    /// `MINUTE`
    pub const MINUTE: i32 = interval_mask(tokens::MINUTE);
    /// `SECOND`
    pub const SECOND: i32 = interval_mask(tokens::SECOND);
}

/// Build an `interval` typmod from a precision (`None` = unspecified) and a
/// range (see [`range`]). `interval(3)` is `interval_typmod(Some(3), range::FULL)`.
pub fn interval_typmod(precision: Option<i32>, range: i32) -> i32 {
    let p = precision.unwrap_or(INTERVAL_FULL_PRECISION);
    ((range & 0x7FFF) << 16) | (p & INTERVAL_PRECISION_MASK)
}

fn fits_i32(v: f64) -> bool {
    v >= f64::from(i32::MIN) && v < -f64::from(i32::MIN)
}

#[allow(clippy::cast_precision_loss)]
fn fits_i64(v: f64) -> bool {
    v >= i64::MIN as f64 && v < -(i64::MIN as f64)
}

fn tsround(v: f64) -> f64 {
    rint(v * 1_000_000.0) / 1_000_000.0
}

impl Interval {
    /// `infinity`.
    pub const INFINITY: Interval = Interval {
        months: i32::MAX,
        days: i32::MAX,
        micros: i64::MAX,
    };
    /// `-infinity`.
    pub const NEG_INFINITY: Interval = Interval {
        months: i32::MIN,
        days: i32::MIN,
        micros: i64::MIN,
    };
    /// The zero interval.
    pub const ZERO: Interval = Interval {
        months: 0,
        days: 0,
        micros: 0,
    };

    /// Whether this is `infinity`.
    pub fn is_infinity(&self) -> bool {
        self.months == i32::MAX && self.days == i32::MAX && self.micros == i64::MAX
    }

    /// Whether this is `-infinity`.
    pub fn is_neg_infinity(&self) -> bool {
        self.months == i32::MIN && self.days == i32::MIN && self.micros == i64::MIN
    }

    /// Whether the value is finite.
    pub fn is_finite(&self) -> bool {
        !self.is_infinity() && !self.is_neg_infinity()
    }

    /// Bit-for-bit field equality (unlike `==`, which compares spans).
    pub fn fields_eq(&self, other: &Interval) -> bool {
        self.months == other.months && self.days == other.days && self.micros == other.micros
    }

    /// `interval_cmp_value`: the span as microseconds with 30-day months.
    pub fn cmp_value(&self) -> i128 {
        let days = i64::from(self.months) * 30 + i64::from(self.days);
        i128::from(self.micros) + i128::from(days) * i128::from(USECS_PER_DAY)
    }

    /// `interval_in` with a typmod (`-1` for none).
    pub fn parse(input: &str, typmod: i32, env: &DateTimeEnv<'_>) -> Result<Interval> {
        let range = if typmod >= 0 {
            (typmod >> 16) & 0x7FFF
        } else {
            INTERVAL_FULL_RANGE
        };
        let mut res = parse_datetime(input, 256)
            .and_then(|fields| decode_interval(&fields, range, env.interval_style));
        if matches!(res, Err(DtErr::BadFormat)) {
            res = decode_iso8601_interval(input).map(DecodedInterval::Finite);
        }
        let decoded = res.map_err(|e| {
            let e = if e == DtErr::FieldOverflow {
                DtErr::IntervalOverflow
            } else {
                e
            };
            e.report(input, "interval")
        })?;
        let iv = match decoded {
            DecodedInterval::Infinite { negative: false } => Interval::INFINITY,
            DecodedInterval::Infinite { negative: true } => Interval::NEG_INFINITY,
            DecodedInterval::Finite(itm) => from_itm_in(&itm)?,
        };
        iv.with_typmod(typmod)
    }

    /// `interval_out` in the given style.
    pub fn format(&self, style: IntervalStyle) -> String {
        if self.is_neg_infinity() {
            return "-infinity".to_owned();
        }
        if self.is_infinity() {
            return "infinity".to_owned();
        }
        encode_interval(&self.to_itm(), style)
    }

    /// `interval2itm`.
    pub(crate) fn to_itm(self) -> Itm {
        let mut time = self.micros;
        let hour = time / USECS_PER_HOUR;
        time -= hour * USECS_PER_HOUR;
        let min = time / USECS_PER_MINUTE;
        time -= min * USECS_PER_MINUTE;
        let sec = time / USECS_PER_SEC;
        time -= sec * USECS_PER_SEC;
        #[allow(clippy::cast_possible_truncation)]
        Itm {
            year: self.months / MONTHS_PER_YEAR,
            mon: self.months % MONTHS_PER_YEAR,
            mday: self.days,
            hour,
            min: min as i32,
            sec: sec as i32,
            usec: time as i32,
        }
    }

    /// `itm2interval`.
    pub(crate) fn from_itm(itm: &Itm) -> Result<Interval> {
        let total_months = i64::from(itm.year) * i64::from(MONTHS_PER_YEAR) + i64::from(itm.mon);
        let months =
            i32::try_from(total_months).map_err(|_| DateTimeError::interval_out_of_range())?;
        let micros = itm
            .hour
            .checked_mul(USECS_PER_HOUR)
            .and_then(|v| v.checked_add(i64::from(itm.min) * USECS_PER_MINUTE))
            .and_then(|v| v.checked_add(i64::from(itm.sec) * USECS_PER_SEC))
            .and_then(|v| v.checked_add(i64::from(itm.usec)))
            .ok_or_else(DateTimeError::interval_out_of_range)?;
        let r = Interval {
            months,
            days: itm.mday,
            micros,
        };
        if !r.is_finite() {
            return Err(DateTimeError::interval_out_of_range());
        }
        Ok(r)
    }

    /// `AdjustIntervalForTypmod`.
    pub fn with_typmod(self, typmod: i32) -> Result<Interval> {
        const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1000, 100, 10, 1];
        const OFFSETS: [i64; 7] = [500_000, 50_000, 5000, 500, 50, 5, 0];
        if !self.is_finite() || typmod < 0 {
            return Ok(self);
        }
        let mut iv = self;
        let rng = (typmod >> 16) & 0x7FFF;
        let precision = typmod & INTERVAL_PRECISION_MASK;
        let m = interval_mask;
        if rng == INTERVAL_FULL_RANGE {
        } else if rng == m(YEAR) {
            iv.months = (iv.months / MONTHS_PER_YEAR) * MONTHS_PER_YEAR;
            iv.days = 0;
            iv.micros = 0;
        } else if rng == m(MONTH) || rng == (m(YEAR) | m(MONTH)) {
            iv.days = 0;
            iv.micros = 0;
        } else if rng == m(DAY) {
            iv.micros = 0;
        } else if rng == m(HOUR) || rng == (m(DAY) | m(HOUR)) {
            iv.micros = (iv.micros / USECS_PER_HOUR) * USECS_PER_HOUR;
        } else if rng == m(MINUTE)
            || rng == (m(DAY) | m(HOUR) | m(MINUTE))
            || rng == (m(HOUR) | m(MINUTE))
        {
            iv.micros = (iv.micros / USECS_PER_MINUTE) * USECS_PER_MINUTE;
        } else if rng == m(SECOND)
            || rng == (m(DAY) | m(HOUR) | m(MINUTE) | m(SECOND))
            || rng == (m(HOUR) | m(MINUTE) | m(SECOND))
            || rng == (m(MINUTE) | m(SECOND))
        {
        } else {
            return Err(DateTimeError::new(
                sqlstate::INTERNAL_ERROR,
                format!("unrecognized interval typmod: {typmod}"),
            ));
        }
        if precision != INTERVAL_FULL_PRECISION {
            if !(0..=MAX_PRECISION).contains(&precision) {
                return Err(DateTimeError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!(
                        "interval({precision}) precision must be between 0 and {MAX_PRECISION}"
                    ),
                ));
            }
            #[allow(clippy::cast_sign_loss)]
            let i = precision as usize;
            if iv.micros >= 0 {
                iv.micros = iv
                    .micros
                    .checked_add(OFFSETS[i])
                    .ok_or_else(DateTimeError::interval_out_of_range)?;
            } else {
                iv.micros = iv
                    .micros
                    .checked_sub(OFFSETS[i])
                    .ok_or_else(DateTimeError::interval_out_of_range)?;
            }
            iv.micros -= iv.micros % SCALES[i];
        }
        Ok(iv)
    }

    /// Unary minus.
    pub fn negate(&self) -> Result<Interval> {
        if self.is_neg_infinity() {
            return Ok(Interval::INFINITY);
        }
        if self.is_infinity() {
            return Ok(Interval::NEG_INFINITY);
        }
        let r = Interval {
            months: 0i32
                .checked_sub(self.months)
                .ok_or_else(DateTimeError::interval_out_of_range)?,
            days: 0i32
                .checked_sub(self.days)
                .ok_or_else(DateTimeError::interval_out_of_range)?,
            micros: 0i64
                .checked_sub(self.micros)
                .ok_or_else(DateTimeError::interval_out_of_range)?,
        };
        if !r.is_finite() {
            return Err(DateTimeError::interval_out_of_range());
        }
        Ok(r)
    }

    /// `interval + interval`.
    pub fn add(&self, other: &Interval) -> Result<Interval> {
        if self.is_neg_infinity() {
            return if other.is_infinity() {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::NEG_INFINITY)
            };
        }
        if self.is_infinity() {
            return if other.is_neg_infinity() {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::INFINITY)
            };
        }
        if !other.is_finite() {
            return Ok(*other);
        }
        CheckedInterval {
            months: self.months.checked_add(other.months),
            days: self.days.checked_add(other.days),
            micros: self.micros.checked_add(other.micros),
        }
        .ok()
    }

    /// `interval - interval`.
    pub fn sub(&self, other: &Interval) -> Result<Interval> {
        if self.is_neg_infinity() {
            return if other.is_neg_infinity() {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::NEG_INFINITY)
            };
        }
        if self.is_infinity() {
            return if other.is_infinity() {
                Err(DateTimeError::interval_out_of_range())
            } else {
                Ok(Interval::INFINITY)
            };
        }
        if other.is_neg_infinity() {
            return Ok(Interval::INFINITY);
        }
        if other.is_infinity() {
            return Ok(Interval::NEG_INFINITY);
        }
        CheckedInterval {
            months: self.months.checked_sub(other.months),
            days: self.days.checked_sub(other.days),
            micros: self.micros.checked_sub(other.micros),
        }
        .ok()
    }

    fn sign(&self) -> i32 {
        match self.cmp_value().cmp(&0) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    /// `interval * float8`.
    pub fn mul(&self, factor: f64) -> Result<Interval> {
        let oor = DateTimeError::interval_out_of_range;
        if factor.is_nan() {
            return Err(oor());
        }
        if !self.is_finite() {
            if factor == 0.0 {
                return Err(oor());
            }
            return if factor < 0.0 {
                self.negate()
            } else {
                Ok(*self)
            };
        }
        if factor.is_infinite() {
            let isign = self.sign();
            if isign == 0 {
                return Err(oor());
            }
            return Ok(if factor * f64::from(isign) < 0.0 {
                Interval::NEG_INFINITY
            } else {
                Interval::INFINITY
            });
        }
        self.scale(|v| v * factor)
    }

    /// `interval / float8`.
    pub fn div(&self, factor: f64) -> Result<Interval> {
        let oor = DateTimeError::interval_out_of_range;
        if factor == 0.0 {
            return Err(DateTimeError::new(
                sqlstate::DIVISION_BY_ZERO,
                "division by zero",
            ));
        }
        if factor.is_nan() {
            return Err(oor());
        }
        if !self.is_finite() {
            if factor.is_infinite() {
                return Err(oor());
            }
            return if factor < 0.0 {
                self.negate()
            } else {
                Ok(*self)
            };
        }
        self.scale(|v| v / factor)
    }

    /// Common part of `interval_mul` / `interval_div`.
    fn scale(&self, op: impl Fn(f64) -> f64) -> Result<Interval> {
        let oor = DateTimeError::interval_out_of_range;
        let orig_month = self.months;
        let orig_day = self.days;
        let rd = op(f64::from(self.months));
        if rd.is_nan() || !fits_i32(rd) {
            return Err(oor());
        }
        #[allow(clippy::cast_possible_truncation)]
        let month = rd as i32;
        let rd = op(f64::from(self.days));
        if rd.is_nan() || !fits_i32(rd) {
            return Err(oor());
        }
        #[allow(clippy::cast_possible_truncation)]
        let mut day = rd as i32;
        let mut month_remainder_days =
            (op(f64::from(orig_month)) - f64::from(month)) * f64::from(DAYS_PER_MONTH);
        month_remainder_days = tsround(month_remainder_days);
        #[allow(clippy::cast_possible_truncation)]
        let mrd_int = month_remainder_days as i32;
        #[allow(clippy::cast_precision_loss)]
        let mut sec_remainder = (op(f64::from(orig_day)) - f64::from(day) + month_remainder_days
            - f64::from(mrd_int))
            * SECS_PER_DAY as f64;
        sec_remainder = tsround(sec_remainder);
        #[allow(clippy::cast_precision_loss)]
        let spd = SECS_PER_DAY as f64;
        if sec_remainder.abs() >= spd {
            #[allow(clippy::cast_possible_truncation)]
            let extra = (sec_remainder / spd) as i32;
            day = day.checked_add(extra).ok_or_else(oor)?;
            sec_remainder -= f64::from(extra) * spd;
        }
        day = day.checked_add(mrd_int).ok_or_else(oor)?;
        #[allow(clippy::cast_precision_loss)]
        let rd = rint(op(self.micros as f64) + sec_remainder * USECS_PER_SEC as f64);
        if rd.is_nan() || !fits_i64(rd) {
            return Err(oor());
        }
        #[allow(clippy::cast_possible_truncation)]
        let micros = rd as i64;
        let r = Interval {
            months: month,
            days: day,
            micros,
        };
        if !r.is_finite() {
            return Err(oor());
        }
        Ok(r)
    }

    /// `justify_hours`.
    pub fn justify_hours(&self) -> Result<Interval> {
        if !self.is_finite() {
            return Ok(*self);
        }
        let mut r = *self;
        let wholeday = r.micros / USECS_PER_DAY;
        r.micros -= wholeday * USECS_PER_DAY;
        r.days = i32::try_from(i64::from(r.days) + wholeday)
            .map_err(|_| DateTimeError::interval_out_of_range())?;
        if r.days > 0 && r.micros < 0 {
            r.micros += USECS_PER_DAY;
            r.days -= 1;
        } else if r.days < 0 && r.micros > 0 {
            r.micros -= USECS_PER_DAY;
            r.days += 1;
        }
        Ok(r)
    }

    /// `justify_days`.
    pub fn justify_days(&self) -> Result<Interval> {
        if !self.is_finite() {
            return Ok(*self);
        }
        let mut r = *self;
        let wholemonth = r.days / DAYS_PER_MONTH;
        r.days -= wholemonth * DAYS_PER_MONTH;
        r.months = r
            .months
            .checked_add(wholemonth)
            .ok_or_else(DateTimeError::interval_out_of_range)?;
        if r.months > 0 && r.days < 0 {
            r.days += DAYS_PER_MONTH;
            r.months -= 1;
        } else if r.months < 0 && r.days > 0 {
            r.days -= DAYS_PER_MONTH;
            r.months += 1;
        }
        Ok(r)
    }

    /// `justify_interval`.
    pub fn justify_interval(&self) -> Result<Interval> {
        if !self.is_finite() {
            return Ok(*self);
        }
        let oor = DateTimeError::interval_out_of_range;
        let mut r = *self;
        if (r.days > 0 && r.micros > 0) || (r.days < 0 && r.micros < 0) {
            let wholemonth = r.days / DAYS_PER_MONTH;
            r.days -= wholemonth * DAYS_PER_MONTH;
            r.months = r.months.checked_add(wholemonth).ok_or_else(oor)?;
        }
        let wholeday = r.micros / USECS_PER_DAY;
        r.micros -= wholeday * USECS_PER_DAY;
        #[allow(clippy::cast_possible_truncation)]
        {
            r.days = r.days.wrapping_add(wholeday as i32);
        }
        let wholemonth = r.days / DAYS_PER_MONTH;
        r.days -= wholemonth * DAYS_PER_MONTH;
        r.months = r.months.checked_add(wholemonth).ok_or_else(oor)?;
        if r.months > 0 && (r.days < 0 || (r.days == 0 && r.micros < 0)) {
            r.days += DAYS_PER_MONTH;
            r.months -= 1;
        } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.micros > 0)) {
            r.days -= DAYS_PER_MONTH;
            r.months += 1;
        }
        if r.days > 0 && r.micros < 0 {
            r.micros += USECS_PER_DAY;
            r.days -= 1;
        } else if r.days < 0 && r.micros > 0 {
            r.micros -= USECS_PER_DAY;
            r.days += 1;
        }
        Ok(r)
    }
}

/// Helper for checked field-wise arithmetic.
struct CheckedInterval {
    months: Option<i32>,
    days: Option<i32>,
    micros: Option<i64>,
}

impl CheckedInterval {
    fn ok(self) -> Result<Interval> {
        match (self.months, self.days, self.micros) {
            (Some(months), Some(days), Some(micros)) => {
                let r = Interval {
                    months,
                    days,
                    micros,
                };
                if r.is_finite() {
                    Ok(r)
                } else {
                    Err(DateTimeError::interval_out_of_range())
                }
            }
            _ => Err(DateTimeError::interval_out_of_range()),
        }
    }
}

/// `itmin2interval`.
fn from_itm_in(itm: &ItmIn) -> Result<Interval> {
    let total_months = i64::from(itm.year) * i64::from(MONTHS_PER_YEAR) + i64::from(itm.mon);
    let months = i32::try_from(total_months).map_err(|_| DateTimeError::interval_out_of_range())?;
    Ok(Interval {
        months,
        days: itm.mday,
        micros: itm.usec,
    })
}

impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_value() == other.cmp_value()
    }
}

impl Eq for Interval {}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_value().cmp(&other.cmp_value())
    }
}

impl Hash for Interval {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.cmp_value().hash(state);
    }
}
