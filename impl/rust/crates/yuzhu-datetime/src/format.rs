//! Text output (`EncodeDateOnly`, `EncodeDateTime`, `EncodeInterval`).

use std::fmt::Write as _;

use crate::calendar::{DAYS, Itm, MAX_PRECISION, MONTHS, Tm, date2j, j2day};
use crate::settings::{DateOrder, DateStyle, IntervalStyle};

/// Maximum length of a printed zone abbreviation (`MAXTZLEN`).
const MAXTZLEN: usize = 10;

fn push_zeropad(out: &mut String, v: i64, width: usize) {
    let _ = write!(out, "{:0width$}", v.unsigned_abs(), width = width);
}

fn display_year(year: i32) -> i64 {
    if year > 0 {
        i64::from(year)
    } else {
        -(i64::from(year) - 1)
    }
}

/// `AppendSeconds`.
fn append_seconds(out: &mut String, sec: i32, fsec: i32, precision: usize, fillzeros: bool) {
    if fillzeros {
        push_zeropad(out, i64::from(sec), 2);
    } else {
        let _ = write!(out, "{}", sec.unsigned_abs());
    }
    if fsec != 0 {
        let mut value = fsec.unsigned_abs();
        let mut digits = vec![b'0'; precision];
        let mut end = precision;
        let mut gotnonzero = false;
        for idx in (0..precision).rev() {
            let rem = value % 10;
            value /= 10;
            if rem != 0 {
                gotnonzero = true;
            }
            if gotnonzero {
                #[allow(clippy::cast_possible_truncation)]
                {
                    digits[idx] = b'0' + rem as u8;
                }
            } else {
                end = idx;
            }
        }
        out.push('.');
        if value != 0 {
            let _ = write!(out, "{}", fsec.unsigned_abs());
            return;
        }
        out.push_str(std::str::from_utf8(&digits[..end]).unwrap_or(""));
    }
}

/// `EncodeTimezone` (`tz` is seconds west of UTC).
fn encode_timezone(out: &mut String, tz: i32) {
    let mut sec = tz.unsigned_abs();
    let mut min = sec / 60;
    sec -= min * 60;
    let hour = min / 60;
    min -= hour * 60;
    out.push(if tz <= 0 { '+' } else { '-' });
    if sec != 0 {
        let _ = write!(out, "{hour:02}:{min:02}:{sec:02}");
    } else if min != 0 {
        let _ = write!(out, "{hour:02}:{min:02}");
    } else {
        let _ = write!(out, "{hour:02}");
    }
}

/// `EncodeDateOnly`.
pub(crate) fn encode_date_only(tm: &Tm, style: DateStyle, order: DateOrder) -> String {
    let mut out = String::with_capacity(16);
    let y = display_year(tm.year);
    let m = i64::from(tm.mon);
    let d = i64::from(tm.mday);
    match style {
        DateStyle::Iso => {
            push_zeropad(&mut out, y, 4);
            out.push('-');
            push_zeropad(&mut out, m, 2);
            out.push('-');
            push_zeropad(&mut out, d, 2);
        }
        DateStyle::Sql | DateStyle::Postgres => {
            let sep = if style == DateStyle::Sql { '/' } else { '-' };
            if order == DateOrder::Dmy {
                push_zeropad(&mut out, d, 2);
                out.push(sep);
                push_zeropad(&mut out, m, 2);
            } else {
                push_zeropad(&mut out, m, 2);
                out.push(sep);
                push_zeropad(&mut out, d, 2);
            }
            out.push(sep);
            push_zeropad(&mut out, y, 4);
        }
        DateStyle::German => {
            push_zeropad(&mut out, d, 2);
            out.push('.');
            push_zeropad(&mut out, m, 2);
            out.push('.');
            push_zeropad(&mut out, y, 4);
        }
    }
    if tm.year <= 0 {
        out.push_str(" BC");
    }
    out
}

fn push_tzn(out: &mut String, tzn: &str) {
    out.push(' ');
    out.extend(tzn.chars().take(MAXTZLEN));
}

/// `EncodeDateTime`. `tz` is `Some((offset_west, abbrev))` for timestamptz.
pub(crate) fn encode_date_time(
    tm: &Tm,
    fsec: i32,
    tz: Option<(i32, &str)>,
    style: DateStyle,
    order: DateOrder,
) -> String {
    let mut out = String::with_capacity(40);
    let tz = if tm.isdst < 0 { None } else { tz };
    let y = display_year(tm.year);
    let time = |out: &mut String| {
        push_zeropad(out, i64::from(tm.hour), 2);
        out.push(':');
        push_zeropad(out, i64::from(tm.min), 2);
        out.push(':');
        append_seconds(out, tm.sec, fsec, MAX_PRECISION as usize, true);
    };
    match style {
        DateStyle::Iso => {
            push_zeropad(&mut out, y, 4);
            out.push('-');
            push_zeropad(&mut out, i64::from(tm.mon), 2);
            out.push('-');
            push_zeropad(&mut out, i64::from(tm.mday), 2);
            out.push(' ');
            time(&mut out);
            if let Some((off, _)) = tz {
                encode_timezone(&mut out, off);
            }
        }
        DateStyle::Sql => {
            if order == DateOrder::Dmy {
                push_zeropad(&mut out, i64::from(tm.mday), 2);
                out.push('/');
                push_zeropad(&mut out, i64::from(tm.mon), 2);
            } else {
                push_zeropad(&mut out, i64::from(tm.mon), 2);
                out.push('/');
                push_zeropad(&mut out, i64::from(tm.mday), 2);
            }
            out.push('/');
            push_zeropad(&mut out, y, 4);
            out.push(' ');
            time(&mut out);
            if let Some((_, tzn)) = tz {
                push_tzn(&mut out, tzn);
            }
        }
        DateStyle::German => {
            push_zeropad(&mut out, i64::from(tm.mday), 2);
            out.push('.');
            push_zeropad(&mut out, i64::from(tm.mon), 2);
            out.push('.');
            push_zeropad(&mut out, y, 4);
            out.push(' ');
            time(&mut out);
            if let Some((_, tzn)) = tz {
                push_tzn(&mut out, tzn);
            }
        }
        DateStyle::Postgres => {
            let wday = j2day(date2j(tm.year, tm.mon, tm.mday));
            out.push_str(&DAYS[wday as usize][..3]);
            out.push(' ');
            if order == DateOrder::Dmy {
                push_zeropad(&mut out, i64::from(tm.mday), 2);
                out.push(' ');
                out.push_str(MONTHS[(tm.mon - 1) as usize]);
            } else {
                out.push_str(MONTHS[(tm.mon - 1) as usize]);
                out.push(' ');
                push_zeropad(&mut out, i64::from(tm.mday), 2);
            }
            out.push(' ');
            time(&mut out);
            out.push(' ');
            push_zeropad(&mut out, y, 4);
            if let Some((_, tzn)) = tz {
                push_tzn(&mut out, tzn);
            }
        }
    }
    if tm.year <= 0 {
        out.push_str(" BC");
    }
    out
}

fn add_postgres_int_part(
    out: &mut String,
    value: i64,
    units: &str,
    is_zero: &mut bool,
    is_before: &mut bool,
) {
    if value == 0 {
        return;
    }
    let _ = write!(
        out,
        "{}{}{} {}{}",
        if *is_zero { "" } else { " " },
        if *is_before && value > 0 { "+" } else { "" },
        value,
        units,
        if value == 1 { "" } else { "s" }
    );
    *is_before = value < 0;
    *is_zero = false;
}

fn add_verbose_int_part(
    out: &mut String,
    value: i64,
    units: &str,
    is_zero: &mut bool,
    is_before: &mut bool,
) {
    if value == 0 {
        return;
    }
    let mut value = value;
    if *is_zero {
        *is_before = value < 0;
        value = value.abs();
    } else if *is_before {
        value = -value;
    }
    let _ = write!(
        out,
        " {} {}{}",
        value,
        units,
        if value == 1 { "" } else { "s" }
    );
    *is_zero = false;
}

fn add_iso_int_part(out: &mut String, value: i64, units: char) {
    if value != 0 {
        let _ = write!(out, "{value}{units}");
    }
}

/// `EncodeInterval`.
#[allow(clippy::too_many_lines)]
pub(crate) fn encode_interval(itm: &Itm, style: IntervalStyle) -> String {
    let mut out = String::with_capacity(48);
    let mut year = i64::from(itm.year);
    let mut mon = i64::from(itm.mon);
    let mut mday = i64::from(itm.mday);
    let mut hour = itm.hour;
    let mut min = i64::from(itm.min);
    let mut sec = itm.sec;
    let mut fsec = itm.usec;
    let mut is_before = false;
    let mut is_zero = true;
    let prec = MAX_PRECISION as usize;
    match style {
        IntervalStyle::SqlStandard => {
            let has_negative =
                year < 0 || mon < 0 || mday < 0 || hour < 0 || min < 0 || sec < 0 || fsec < 0;
            let has_positive =
                year > 0 || mon > 0 || mday > 0 || hour > 0 || min > 0 || sec > 0 || fsec > 0;
            let has_year_month = year != 0 || mon != 0;
            let has_day_time = mday != 0 || hour != 0 || min != 0 || sec != 0 || fsec != 0;
            let has_day = mday != 0;
            let sql_standard_value =
                !((has_negative && has_positive) || (has_year_month && has_day_time));
            if has_negative && sql_standard_value {
                out.push('-');
                year = -year;
                mon = -mon;
                mday = -mday;
                hour = -hour;
                min = -min;
                sec = -sec;
                fsec = -fsec;
            }
            if !has_negative && !has_positive {
                out.push('0');
            } else if !sql_standard_value {
                let year_sign = if year < 0 || mon < 0 { '-' } else { '+' };
                let day_sign = if mday < 0 { '-' } else { '+' };
                let sec_sign = if hour < 0 || min < 0 || sec < 0 || fsec < 0 {
                    '-'
                } else {
                    '+'
                };
                let _ = write!(
                    out,
                    "{}{}-{} {}{} {}{}:{:02}:",
                    year_sign,
                    year.abs(),
                    mon.abs(),
                    day_sign,
                    mday.abs(),
                    sec_sign,
                    hour.abs(),
                    min.abs()
                );
                append_seconds(&mut out, sec, fsec, prec, true);
            } else if has_year_month {
                let _ = write!(out, "{year}-{mon}");
            } else if has_day {
                let _ = write!(out, "{mday} {hour}:{min:02}:");
                append_seconds(&mut out, sec, fsec, prec, true);
            } else {
                let _ = write!(out, "{hour}:{min:02}:");
                append_seconds(&mut out, sec, fsec, prec, true);
            }
        }
        IntervalStyle::Iso8601 => {
            if year == 0 && mon == 0 && mday == 0 && hour == 0 && min == 0 && sec == 0 && fsec == 0
            {
                return "PT0S".to_owned();
            }
            out.push('P');
            add_iso_int_part(&mut out, year, 'Y');
            add_iso_int_part(&mut out, mon, 'M');
            add_iso_int_part(&mut out, mday, 'D');
            if hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                out.push('T');
            }
            add_iso_int_part(&mut out, hour, 'H');
            add_iso_int_part(&mut out, min, 'M');
            if sec != 0 || fsec != 0 {
                if sec < 0 || fsec < 0 {
                    out.push('-');
                }
                append_seconds(&mut out, sec, fsec, prec, false);
                out.push('S');
            }
        }
        IntervalStyle::Postgres => {
            add_postgres_int_part(&mut out, year, "year", &mut is_zero, &mut is_before);
            add_postgres_int_part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
            add_postgres_int_part(&mut out, mday, "day", &mut is_zero, &mut is_before);
            if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
                let _ = write!(
                    out,
                    "{}{}{:02}:{:02}:",
                    if is_zero { "" } else { " " },
                    if minus {
                        "-"
                    } else if is_before {
                        "+"
                    } else {
                        ""
                    },
                    hour.unsigned_abs(),
                    min.unsigned_abs()
                );
                append_seconds(&mut out, sec, fsec, prec, true);
            }
        }
        IntervalStyle::PostgresVerbose => {
            out.push('@');
            add_verbose_int_part(&mut out, year, "year", &mut is_zero, &mut is_before);
            add_verbose_int_part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
            add_verbose_int_part(&mut out, mday, "day", &mut is_zero, &mut is_before);
            add_verbose_int_part(&mut out, hour, "hour", &mut is_zero, &mut is_before);
            add_verbose_int_part(&mut out, min, "min", &mut is_zero, &mut is_before);
            if sec != 0 || fsec != 0 {
                out.push(' ');
                if sec < 0 || (sec == 0 && fsec < 0) {
                    if is_zero {
                        is_before = true;
                    } else if !is_before {
                        out.push('-');
                    }
                } else if is_before {
                    out.push('-');
                }
                append_seconds(&mut out, sec, fsec, prec, false);
                let _ = write!(
                    out,
                    " sec{}",
                    if sec.abs() != 1 || fsec != 0 { "s" } else { "" }
                );
                is_zero = false;
            }
            if is_zero {
                out.push_str(" 0");
            }
            if is_before {
                out.push_str(" ago");
            }
        }
    }
    out
}

/// PostgreSQL's `float8out` with the default `extra_float_digits = 1`
/// (shortest round-trip representation).
pub fn format_float8(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    // Shortest digits via Rust's formatter in exponent form.
    let e = format!("{v:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let ndigits = i32::try_from(digits.len()).unwrap_or(17);
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..15).contains(&exp) {
        out.push_str(&digits[..1]);
        if ndigits > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    } else if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        #[allow(clippy::cast_sign_loss)]
        let int_len = (exp + 1) as usize;
        if digits.len() <= int_len {
            out.push_str(&digits);
            for _ in digits.len()..int_len {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float8_format() {
        assert_eq!(format_float8(1.5), "1.5");
        assert_eq!(format_float8(1_704_067_200.0), "1704067200");
        assert_eq!(format_float8(1e15), "1e+15");
        assert_eq!(format_float8(0.0001), "0.0001");
        assert_eq!(format_float8(0.00001), "1e-05");
        assert_eq!(format_float8(-2.5e20), "-2.5e+20");
        assert_eq!(format_float8(123_456_789_012_345.6), "123456789012345.6");
    }

    #[test]
    fn seconds() {
        let mut s = String::new();
        append_seconds(&mut s, 5, 500_000, 6, true);
        assert_eq!(s, "05.5");
        let mut s = String::new();
        append_seconds(&mut s, 5, 1, 6, true);
        assert_eq!(s, "05.000001");
    }
}
