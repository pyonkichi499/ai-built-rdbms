//! Text input: a port of PostgreSQL's two-stage date/time parser
//! (`ParseDateTime` splits the string into typed fields, `DecodeDateTime` /
//! `DecodeInterval` / `DecodeISO8601Interval` interpret them).

use crate::calendar::{
    DAY_TAB, DAYS_PER_MONTH, HOURS_PER_DAY, ItmIn, MAX_TZDISP_HOUR, MONTHS_PER_YEAR, Tm,
    USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC, date2j, dt2time, isleap,
    j2date,
};
use crate::cnum::{atoi, rint, strtod, strtoint, strtol};
use crate::error::DtErr;
use crate::settings::{DateOrder, DateTimeEnv, IntervalStyle};
#[allow(clippy::wildcard_imports)]
use crate::tokens::*;
use crate::tz::TimeZone;

/// Maximum number of fields (`MAXDATEFIELDS`).
pub(crate) const MAXDATEFIELDS: usize = 25;
/// `MAXDATELEN`.
pub(crate) const MAXDATELEN: usize = 128;

/// `INTERVAL_FULL_RANGE`.
pub(crate) const INTERVAL_FULL_RANGE: i32 = 0x7FFF;
pub(crate) const fn interval_mask(b: i32) -> i32 {
    1 << b
}

type DtResult<T> = Result<T, DtErr>;

/// One field produced by [`parse_datetime`].
#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub text: Vec<u8>,
    pub ftype: i32,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn is_punct(c: u8) -> bool {
    c.is_ascii_punctuation()
}

/// `ParseDateTime`. `buflen` is the size of the caller's work buffer, which
/// bounds the total length of the fields like in PostgreSQL.
pub(crate) fn parse_datetime(input: &str, buflen: usize) -> DtResult<Vec<Field>> {
    let s = input.as_bytes();
    let at = |i: usize| -> u8 { s.get(i).copied().unwrap_or(0) };
    let mut fields: Vec<Field> = Vec::new();
    let mut cp = 0usize;
    let mut bufp = 0usize;
    while at(cp) != 0 {
        if is_space(at(cp)) {
            cp += 1;
            continue;
        }
        if fields.len() >= MAXDATEFIELDS {
            return Err(DtErr::BadFormat);
        }
        let mut f: Vec<u8> = Vec::new();
        let ftype;
        macro_rules! append {
            ($c:expr) => {{
                if bufp + 1 >= buflen {
                    return Err(DtErr::BadFormat);
                }
                bufp += 1;
                f.push($c);
            }};
        }
        let c = at(cp);
        if c.is_ascii_digit() {
            append!(at(cp));
            cp += 1;
            while at(cp).is_ascii_digit() {
                append!(at(cp));
                cp += 1;
            }
            if at(cp) == b':' {
                ftype = DTK_TIME;
                append!(at(cp));
                cp += 1;
                while at(cp).is_ascii_digit() || at(cp) == b':' || at(cp) == b'.' {
                    append!(at(cp));
                    cp += 1;
                }
            } else if at(cp) == b'-' || at(cp) == b'/' || at(cp) == b'.' {
                let delim = at(cp);
                append!(at(cp));
                cp += 1;
                if at(cp).is_ascii_digit() {
                    let mut ft = if delim == b'.' { DTK_NUMBER } else { DTK_DATE };
                    while at(cp).is_ascii_digit() {
                        append!(at(cp));
                        cp += 1;
                    }
                    if at(cp) == delim {
                        ft = DTK_DATE;
                        append!(at(cp));
                        cp += 1;
                        while at(cp).is_ascii_digit() || at(cp) == delim {
                            append!(at(cp));
                            cp += 1;
                        }
                    }
                    ftype = ft;
                } else {
                    ftype = DTK_DATE;
                    while at(cp).is_ascii_alphanumeric() || at(cp) == delim {
                        append!(at(cp).to_ascii_lowercase());
                        cp += 1;
                    }
                }
            } else {
                ftype = DTK_NUMBER;
            }
        } else if c == b'.' {
            append!(at(cp));
            cp += 1;
            while at(cp).is_ascii_digit() {
                append!(at(cp));
                cp += 1;
            }
            ftype = DTK_NUMBER;
        } else if c.is_ascii_alphabetic() {
            let mut ft = DTK_STRING;
            append!(at(cp).to_ascii_lowercase());
            cp += 1;
            while at(cp).is_ascii_alphabetic() {
                append!(at(cp).to_ascii_lowercase());
                cp += 1;
            }
            let is_date = at(cp) == b'-'
                || at(cp) == b'/'
                || at(cp) == b'.'
                || ((at(cp) == b'+' || at(cp).is_ascii_digit()) && search(&f, DATETKTBL).is_none());
            if is_date {
                ft = DTK_DATE;
                loop {
                    append!(at(cp).to_ascii_lowercase());
                    cp += 1;
                    let n = at(cp);
                    if !(n == b'+'
                        || n == b'-'
                        || n == b'/'
                        || n == b'_'
                        || n == b'.'
                        || n == b':'
                        || n.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            }
            ftype = ft;
        } else if c == b'+' || c == b'-' {
            append!(at(cp));
            cp += 1;
            while is_space(at(cp)) {
                cp += 1;
            }
            if at(cp).is_ascii_digit() {
                ftype = DTK_TZ;
                append!(at(cp));
                cp += 1;
                while at(cp).is_ascii_digit() || at(cp) == b':' || at(cp) == b'.' || at(cp) == b'-'
                {
                    append!(at(cp));
                    cp += 1;
                }
            } else if at(cp).is_ascii_alphabetic() {
                ftype = DTK_SPECIAL;
                append!(at(cp).to_ascii_lowercase());
                cp += 1;
                while at(cp).is_ascii_alphabetic() {
                    append!(at(cp).to_ascii_lowercase());
                    cp += 1;
                }
            } else {
                return Err(DtErr::BadFormat);
            }
        } else if is_punct(c) {
            cp += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        bufp += 1;
        fields.push(Field { text: f, ftype });
    }
    Ok(fields)
}

/// Result of [`decode_date_time`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Decoded {
    pub dtype: i32,
    pub tm: Tm,
    pub fsec: i32,
    /// Offset in PostgreSQL's convention (seconds west of UTC).
    pub tz: i32,
}

fn decode_special(lowtoken: &[u8]) -> (i32, i32) {
    match search(lowtoken, DATETKTBL) {
        Some(t) => (t.ty, t.value),
        None => (UNKNOWN_FIELD, 0),
    }
}

pub(crate) fn decode_units(lowtoken: &[u8]) -> (i32, i32) {
    match search(lowtoken, DELTATKTBL) {
        Some(t) => (t.ty, t.value),
        None => (UNKNOWN_FIELD, 0),
    }
}

/// `DecodeTimezoneAbbrev`: `(type, offset, zone)`.
fn decode_timezone_abbrev(
    lowtoken: &[u8],
    env: &DateTimeEnv<'_>,
) -> DtResult<(i32, i32, Option<TimeZone>)> {
    match search_abbrev(lowtoken) {
        None => Ok((UNKNOWN_FIELD, 0, None)),
        Some((_, Abbrev::Tz(off))) => Ok((TZ, off, None)),
        Some((_, Abbrev::Dtz(off))) => Ok((DTZ, off, None)),
        Some((token, Abbrev::Dyn(zone))) => match env.zones.lookup(zone) {
            Some(tz) => Ok((DYNTZ, 0, Some(tz))),
            None => Err(DtErr::BadZoneAbbrev {
                timezone: zone.to_owned(),
                abbrev: token.to_owned(),
            }),
        },
    }
}

/// `ParseFraction`: `cp` starts at the decimal point.
fn parse_fraction(cp: &[u8]) -> DtResult<f64> {
    if cp.len() == 1 {
        return Ok(0.0);
    }
    let (v, n, erange) = strtod(cp);
    if n != cp.len() || erange {
        return Err(DtErr::BadFormat);
    }
    Ok(v)
}

#[allow(clippy::cast_possible_truncation)]
fn parse_fractional_second(cp: &[u8]) -> DtResult<i32> {
    let frac = parse_fraction(cp)?;
    Ok(rint(frac * 1_000_000.0) as i32)
}

fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=HOURS_PER_DAY).contains(&hour)
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || fsec < 0
        || i64::from(fsec) > USECS_PER_SEC
    {
        return true;
    }
    crate::calendar::time2t(hour, min, sec, fsec) > USECS_PER_DAY
}

/// `DecodeTimezone`: a numeric zone such as `+09`, `-0530`, `+05:30:15`.
/// Returns the offset in PostgreSQL's convention (seconds west).
pub(crate) fn decode_timezone(s: &[u8]) -> DtResult<i32> {
    let first = s.first().copied().unwrap_or(0);
    if first != b'+' && first != b'-' {
        return Err(DtErr::BadFormat);
    }
    let (mut hr, n, of) = strtoint(&s[1..]);
    if of {
        return Err(DtErr::TzDispOverflow);
    }
    let mut cp = 1 + n;
    let min;
    let mut sec = 0;
    if s.get(cp) == Some(&b':') {
        let (v, n, of) = strtoint(&s[cp + 1..]);
        if of {
            return Err(DtErr::TzDispOverflow);
        }
        min = v;
        cp += 1 + n;
        if s.get(cp) == Some(&b':') {
            let (v, n, of) = strtoint(&s[cp + 1..]);
            if of {
                return Err(DtErr::TzDispOverflow);
            }
            sec = v;
            cp += 1 + n;
        }
    } else if cp == s.len() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    } else {
        min = 0;
    }
    if !(0..=MAX_TZDISP_HOUR).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(DtErr::TzDispOverflow);
    }
    let mut tz = (hr * 60 + min) * 60 + sec;
    if first == b'-' {
        tz = -tz;
    }
    if cp != s.len() {
        return Err(DtErr::BadFormat);
    }
    Ok(-tz)
}

/// `DecodeTimeCommon`: `(hour, min, sec, usec)`.
fn decode_time_common(s: &[u8], range: i32) -> DtResult<(i64, i32, i32, i32)> {
    let (mut hour, n, of) = strtol(s);
    if of {
        return Err(DtErr::FieldOverflow);
    }
    let mut cp = n;
    if s.get(cp) != Some(&b':') {
        return Err(DtErr::BadFormat);
    }
    let (mut min, n, of) = strtoint(&s[cp + 1..]);
    if of {
        return Err(DtErr::FieldOverflow);
    }
    cp += 1 + n;
    let mut sec;
    let mut fsec = 0;
    if cp == s.len() {
        sec = 0;
        if range == (interval_mask(MINUTE) | interval_mask(SECOND)) {
            if hour > i64::from(i32::MAX) || hour < i64::from(i32::MIN) {
                return Err(DtErr::FieldOverflow);
            }
            sec = min;
            #[allow(clippy::cast_possible_truncation)]
            {
                min = hour as i32;
            }
            hour = 0;
        }
    } else if s[cp] == b'.' {
        fsec = parse_fractional_second(&s[cp..])?;
        if hour > i64::from(i32::MAX) || hour < i64::from(i32::MIN) {
            return Err(DtErr::FieldOverflow);
        }
        sec = min;
        #[allow(clippy::cast_possible_truncation)]
        {
            min = hour as i32;
        }
        hour = 0;
    } else if s[cp] == b':' {
        let (v, n, of) = strtoint(&s[cp + 1..]);
        if of {
            return Err(DtErr::FieldOverflow);
        }
        sec = v;
        cp += 1 + n;
        if cp < s.len() && s[cp] == b'.' {
            fsec = parse_fractional_second(&s[cp..])?;
        } else if cp != s.len() {
            return Err(DtErr::BadFormat);
        }
    } else {
        return Err(DtErr::BadFormat);
    }
    if hour < 0
        || !(0..=59).contains(&min)
        || !(0..=60).contains(&sec)
        || fsec < 0
        || i64::from(fsec) > USECS_PER_SEC
    {
        return Err(DtErr::FieldOverflow);
    }
    Ok((hour, min, sec, fsec))
}

/// `DecodeTime` (timestamp flavour).
fn decode_time(s: &[u8], range: i32, tm: &mut Tm, fsec: &mut i32) -> DtResult<i32> {
    let (hour, min, sec, usec) = decode_time_common(s, range)?;
    if hour > i64::from(i32::MAX) {
        return Err(DtErr::FieldOverflow);
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        tm.hour = hour as i32;
    }
    tm.min = min;
    tm.sec = sec;
    *fsec = usec;
    Ok(DTK_TIME_M)
}

/// `DecodeNumberField`: returns `(DTK_DATE | DTK_TIME, tmask)`.
fn decode_number_field(
    s: &[u8],
    fmask: i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> DtResult<(i32, i32)> {
    let mut s = s;
    if let Some(dot) = s.iter().position(|&c| c == b'.') {
        if dot + 1 == s.len() {
            *fsec = 0;
        } else {
            let (frac, _, erange) = strtod(&s[dot..]);
            if erange {
                return Err(DtErr::BadFormat);
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                *fsec = rint(frac * 1_000_000.0) as i32;
            }
        }
        s = &s[..dot];
    } else if (fmask & DTK_DATE_M) != DTK_DATE_M && s.len() >= 6 {
        let len = s.len();
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok((DTK_DATE, DTK_DATE_M));
    }
    if (fmask & DTK_TIME_M) != DTK_TIME_M {
        if s.len() == 6 {
            tm.sec = atoi(&s[4..]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok((DTK_TIME, DTK_TIME_M));
        } else if s.len() == 4 {
            tm.sec = 0;
            tm.min = atoi(&s[2..]);
            tm.hour = atoi(&s[..2]);
            return Ok((DTK_TIME, DTK_TIME_M));
        }
    }
    Err(DtErr::BadFormat)
}

/// `DecodeNumber`: returns tmask.
#[allow(clippy::too_many_arguments)]
fn decode_number(
    s: &[u8],
    have_text_month: bool,
    fmask: i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
    order: DateOrder,
) -> DtResult<i32> {
    let flen = s.len();
    let (val, n, of) = strtoint(s);
    if of {
        return Err(DtErr::FieldOverflow);
    }
    if n == 0 {
        return Err(DtErr::BadFormat);
    }
    if n < s.len() && s[n] == b'.' {
        if n > 2 {
            let (_, tmask) = decode_number_field(s, fmask | DTK_DATE_M, tm, fsec, is2digits)?;
            return Ok(tmask);
        }
        *fsec = parse_fractional_second(&s[n..])?;
    } else if n != s.len() {
        return Err(DtErr::BadFormat);
    }
    if flen == 3 && (fmask & DTK_DATE_M) == dtk_m(YEAR) && (1..=366).contains(&val) {
        tm.yday = val;
        return Ok(dtk_m(DOY) | dtk_m(MONTH) | dtk_m(DAY));
    }
    let tmask;
    match fmask & DTK_DATE_M {
        0 => {
            if flen >= 3 || order == DateOrder::Ymd {
                tmask = dtk_m(YEAR);
                tm.year = val;
            } else if order == DateOrder::Dmy {
                tmask = dtk_m(DAY);
                tm.mday = val;
            } else {
                tmask = dtk_m(MONTH);
                tm.mon = val;
            }
        }
        m if m == dtk_m(YEAR) => {
            tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == dtk_m(MONTH) => {
            if have_text_month {
                if flen >= 3 || order == DateOrder::Ymd {
                    tmask = dtk_m(YEAR);
                    tm.year = val;
                } else {
                    tmask = dtk_m(DAY);
                    tm.mday = val;
                }
            } else {
                tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == (dtk_m(YEAR) | dtk_m(MONTH)) => {
            if have_text_month {
                if flen >= 3 && *is2digits {
                    tmask = dtk_m(DAY);
                    tm.mday = tm.year;
                    tm.year = val;
                    *is2digits = false;
                } else {
                    tmask = dtk_m(DAY);
                    tm.mday = val;
                }
            } else {
                tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == dtk_m(DAY) => {
            tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == (dtk_m(MONTH) | dtk_m(DAY)) => {
            tmask = dtk_m(YEAR);
            tm.year = val;
        }
        m if m == (dtk_m(YEAR) | dtk_m(MONTH) | dtk_m(DAY)) => {
            let (_, tmask) = decode_number_field(s, fmask, tm, fsec, is2digits)?;
            return Ok(tmask);
        }
        _ => return Err(DtErr::BadFormat),
    }
    if tmask == dtk_m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(tmask)
}

/// `DecodeDate`: returns tmask.
fn decode_date(
    s: &[u8],
    fmask: i32,
    is2digits: &mut bool,
    tm: &mut Tm,
    order: DateOrder,
) -> DtResult<i32> {
    let mut fmask = fmask;
    let mut tmask = 0;
    let mut have_text_month = false;
    let mut fields: Vec<Option<&[u8]>> = Vec::new();
    let mut p = 0usize;
    while p < s.len() && fields.len() < MAXDATEFIELDS {
        while p < s.len() && !s[p].is_ascii_alphanumeric() {
            p += 1;
        }
        if p >= s.len() {
            return Err(DtErr::BadFormat);
        }
        let start = p;
        if s[p].is_ascii_digit() {
            while p < s.len() && s[p].is_ascii_digit() {
                p += 1;
            }
        } else if s[p].is_ascii_alphabetic() {
            while p < s.len() && s[p].is_ascii_alphabetic() {
                p += 1;
            }
        }
        fields.push(Some(&s[start..p]));
        if p < s.len() {
            p += 1;
        }
    }
    for f in &mut fields {
        let Some(text) = *f else { continue };
        if text[0].is_ascii_alphabetic() {
            let (ty, val) = decode_special(text);
            if ty == IGNORE_DTF {
                continue;
            }
            let dmask = dtk_m(ty);
            if ty == MONTH {
                tm.mon = val;
                have_text_month = true;
            } else {
                return Err(DtErr::BadFormat);
            }
            if fmask & dmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= dmask;
            tmask |= dmask;
            *f = None;
        }
    }
    for text in fields.iter().flatten() {
        if text.is_empty() {
            return Err(DtErr::BadFormat);
        }
        let mut fsec = 0;
        let dmask = decode_number(
            text,
            have_text_month,
            fmask,
            tm,
            &mut fsec,
            is2digits,
            order,
        )?;
        if fmask & dmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= dmask;
        tmask |= dmask;
    }
    if (fmask & !(dtk_m(DOY) | dtk_m(TZ))) != DTK_DATE_M {
        return Err(DtErr::BadFormat);
    }
    Ok(tmask)
}

/// `ValidateDate`.
fn validate_date(
    fmask: i32,
    isjulian: bool,
    is2digits: bool,
    bc: bool,
    tm: &mut Tm,
) -> DtResult<()> {
    if fmask & dtk_m(YEAR) != 0 {
        if isjulian {
            // keep
        } else if bc {
            if tm.year <= 0 {
                return Err(DtErr::FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return Err(DtErr::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(DtErr::FieldOverflow);
        }
    }
    if fmask & dtk_m(DOY) != 0 {
        let (y, m, d) = j2date(date2j(tm.year, 1, 1).wrapping_add(tm.yday).wrapping_sub(1));
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
    }
    if fmask & dtk_m(MONTH) != 0 && !(1..=MONTHS_PER_YEAR).contains(&tm.mon) {
        return Err(DtErr::MdFieldOverflow);
    }
    if fmask & dtk_m(DAY) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(DtErr::MdFieldOverflow);
    }
    if (fmask & DTK_DATE_M) == DTK_DATE_M
        && tm.mday > DAY_TAB[usize::from(isleap(tm.year))][(tm.mon - 1) as usize]
    {
        return Err(DtErr::FieldOverflow);
    }
    Ok(())
}

/// Local broken-down time of the transaction start in the session zone
/// (`GetCurrentTimeUsec`).
fn current_time(env: &DateTimeEnv<'_>) -> (Tm, i32, i32) {
    match env.time_zone.to_local(env.now.0) {
        Some(l) => (l.tm, l.fsec, l.tz),
        None => (Tm::default(), 0, 0),
    }
}

/// `DecodeDateTime`.
#[allow(clippy::too_many_lines)]
pub(crate) fn decode_date_time(fields: &[Field], env: &DateTimeEnv<'_>) -> DtResult<Decoded> {
    let mut fmask: i32 = 0;
    let mut tmask: i32;
    let mut ptype: i32 = 0;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named_tz: Option<TimeZone> = None;
    let mut abbrev_tz: Option<TimeZone> = None;
    let mut abbrev: &[u8] = &[];
    let mut dtype = DTK_DATE;
    let mut tm = Tm {
        isdst: -1,
        ..Tm::default()
    };
    let mut fsec: i32 = 0;
    let mut tz: i32 = 0;
    let order = env.date_order;

    for field in fields {
        let text: &[u8] = &field.text;
        match field.ftype {
            DTK_DATE => {
                if ptype == DTK_JULIAN {
                    let (jday, n, of) = strtoint(text);
                    if of || jday < 0 {
                        return Err(DtErr::FieldOverflow);
                    }
                    let (y, m, d) = j2date(jday);
                    tm.year = y;
                    tm.mon = m;
                    tm.mday = d;
                    isjulian = true;
                    tz = decode_timezone(&text[n..])?;
                    tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                    ptype = 0;
                } else if ptype != 0
                    || (fmask & (dtk_m(MONTH) | dtk_m(DAY))) == (dtk_m(MONTH) | dtk_m(DAY))
                {
                    if text[0].is_ascii_digit() || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME {
                                return Err(DtErr::BadFormat);
                            }
                            ptype = 0;
                        }
                        if (fmask & DTK_TIME_M) == DTK_TIME_M {
                            return Err(DtErr::BadFormat);
                        }
                        let Some(dash) = text.iter().position(|&c| c == b'-') else {
                            return Err(DtErr::BadFormat);
                        };
                        tz = decode_timezone(&text[dash..])?;
                        let (_, tm2) = decode_number_field(
                            &text[..dash],
                            fmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        tmask = tm2 | dtk_m(TZ);
                    } else {
                        let name = String::from_utf8_lossy(text).into_owned();
                        match env.zones.lookup(&name) {
                            Some(z) => named_tz = Some(z),
                            None => return Err(DtErr::BadTimezone(name)),
                        }
                        tmask = dtk_m(TZ);
                    }
                } else {
                    tmask = decode_date(text, fmask, &mut is2digits, &mut tm, order)?;
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(DtErr::BadFormat);
                    }
                    ptype = 0;
                }
                tmask = decode_time(text, INTERVAL_FULL_RANGE, &mut tm, &mut fsec)?;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return Err(DtErr::FieldOverflow);
                }
            }
            DTK_TZ => {
                tz = decode_timezone(text)?;
                tmask = dtk_m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, n, of) = strtoint(text);
                    if of {
                        return Err(DtErr::FieldOverflow);
                    }
                    if n < text.len() && text[n] != b'.' {
                        return Err(DtErr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(DtErr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            let (y, m, d) = j2date(value);
                            tm.year = y;
                            tm.mon = m;
                            tm.mday = d;
                            isjulian = true;
                            if n < text.len() && text[n] == b'.' {
                                let time = parse_fraction(&text[n..])?;
                                #[allow(clippy::cast_possible_truncation)]
                                let (h, mi, s, fs) = dt2time((time * USECS_PER_DAY as f64) as i64);
                                tm.hour = h;
                                tm.min = mi;
                                tm.sec = s;
                                fsec = fs;
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME => {
                            let (_, tm2) = decode_number_field(
                                text,
                                fmask | DTK_DATE_M,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            tmask = tm2;
                            if tmask != DTK_TIME_M {
                                return Err(DtErr::BadFormat);
                            }
                        }
                        _ => return Err(DtErr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = text.len();
                    let dot = text.iter().position(|&c| c == b'.');
                    if dot.is_some() && fmask & DTK_DATE_M == 0 {
                        tmask = decode_date(text, fmask, &mut is2digits, &mut tm, order)?;
                    } else if dot.is_some_and(|d| d > 2)
                        || (flen >= 6 && (fmask & DTK_DATE_M == 0 || fmask & DTK_TIME_M == 0))
                    {
                        let (_, tm2) =
                            decode_number_field(text, fmask, &mut tm, &mut fsec, &mut is2digits)?;
                        tmask = tm2;
                    } else {
                        tmask = decode_number(
                            text,
                            have_text_month,
                            fmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                            order,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (mut ty, mut val, valtz) = decode_timezone_abbrev(text, env)?;
                if ty == UNKNOWN_FIELD {
                    (ty, val) = decode_special(text);
                }
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = dtk_m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE;
                            let (cur, cfsec, ctz) = current_time(env);
                            tm = cur;
                            fsec = cfsec;
                            tz = ctz;
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DTK_DATE_M;
                            dtype = DTK_DATE;
                            let (cur, _, _) = current_time(env);
                            let delta = match val {
                                DTK_YESTERDAY => -1,
                                DTK_TOMORROW => 1,
                                _ => 0,
                            };
                            let (y, m, d) = j2date(date2j(cur.year, cur.mon, cur.mday) + delta);
                            tm.year = y;
                            tm.mon = m;
                            tm.mday = d;
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE;
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                            tz = 0;
                        }
                        DTK_EPOCH | DTK_LATE | DTK_EARLY => {
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = val;
                        }
                        _ => return Err(DtErr::BadFormat),
                    },
                    MONTH => {
                        if (fmask & dtk_m(MONTH)) != 0
                            && !have_text_month
                            && (fmask & dtk_m(DAY)) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = dtk_m(DAY);
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    DTZMOD => {
                        tmask |= dtk_m(DTZ);
                        tm.isdst = 1;
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= dtk_m(TZ);
                        tm.isdst = 1;
                        tz = -val;
                    }
                    TZ => {
                        tm.isdst = 0;
                        tz = -val;
                    }
                    DYNTZ => {
                        tmask |= dtk_m(TZ);
                        abbrev_tz = valtz;
                        abbrev = text;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => {}
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        tmask = 0;
                        if (fmask & DTK_DATE_M) != DTK_DATE_M {
                            return Err(DtErr::BadFormat);
                        }
                        if ptype != 0 {
                            return Err(DtErr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        let name = String::from_utf8_lossy(text).into_owned();
                        match env.zones.lookup(&name) {
                            Some(z) => named_tz = Some(z),
                            None => return Err(DtErr::BadFormat),
                        }
                        tmask = dtk_m(TZ);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }

    if ptype != 0 {
        return Err(DtErr::BadFormat);
    }

    if dtype == DTK_DATE {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        if mer != HR24 && tm.hour > HOURS_PER_DAY / 2 {
            return Err(DtErr::FieldOverflow);
        }
        if mer == AM && tm.hour == HOURS_PER_DAY / 2 {
            tm.hour = 0;
        } else if mer == PM && tm.hour != HOURS_PER_DAY / 2 {
            tm.hour += HOURS_PER_DAY / 2;
        }
        if (fmask & DTK_DATE_M) != DTK_DATE_M {
            return Err(DtErr::BadFormat);
        }
        if let Some(z) = &named_tz {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            tz = z.determine_offset(&mut tm);
        }
        if let Some(z) = &abbrev_tz {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            tz = z.determine_abbrev_offset(&mut tm, abbrev);
        }
        if fmask & dtk_m(TZ) == 0 {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(DtErr::BadFormat);
            }
            tz = env.time_zone.determine_offset(&mut tm);
        }
    }
    Ok(Decoded {
        dtype,
        tm,
        fsec,
        tz,
    })
}

// ---------------------------------------------------------------------------
// Interval input
// ---------------------------------------------------------------------------

fn adjust_fract_microseconds(frac: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    #[allow(clippy::cast_precision_loss)]
    let mut frac = frac * scale as f64;
    #[allow(clippy::cast_possible_truncation)]
    let mut usec = frac as i64;
    #[allow(clippy::cast_precision_loss)]
    {
        frac -= usec as f64;
    }
    if frac > 0.5 {
        usec += 1;
    } else if frac < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(v) => {
            itm.usec = v;
            true
        }
        None => false,
    }
}

fn adjust_fract_days(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let mut frac = frac * f64::from(scale);
    #[allow(clippy::cast_possible_truncation)]
    let extra_days = frac as i32;
    match itm.mday.checked_add(extra_days) {
        Some(v) => itm.mday = v,
        None => return false,
    }
    frac -= f64::from(extra_days);
    adjust_fract_microseconds(frac, USECS_PER_DAY, itm)
}

fn adjust_fract_years(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    #[allow(clippy::cast_possible_truncation)]
    let extra_months = rint(frac * f64::from(scale) * f64::from(MONTHS_PER_YEAR)) as i32;
    match itm.mon.checked_add(extra_months) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_microseconds(val: i64, fval: f64, scale: i64, itm: &mut ItmIn) -> bool {
    match val.checked_mul(scale).and_then(|p| itm.usec.checked_add(p)) {
        Some(v) => itm.usec = v,
        None => return false,
    }
    adjust_fract_microseconds(fval, scale, itm)
}

fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    let Ok(v) = i32::try_from(val) else {
        return false;
    };
    match v.checked_mul(scale).and_then(|d| itm.mday.checked_add(d)) {
        Some(r) => {
            itm.mday = r;
            true
        }
        None => false,
    }
}

fn adjust_months(val: i64, itm: &mut ItmIn) -> bool {
    let Ok(v) = i32::try_from(val) else {
        return false;
    };
    match itm.mon.checked_add(v) {
        Some(r) => {
            itm.mon = r;
            true
        }
        None => false,
    }
}

fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    let Ok(v) = i32::try_from(val) else {
        return false;
    };
    match v.checked_mul(scale).and_then(|y| itm.year.checked_add(y)) {
        Some(r) => {
            itm.year = r;
            true
        }
        None => false,
    }
}

/// `DecodeTimeForInterval`.
fn decode_time_for_interval(s: &[u8], range: i32, itm: &mut ItmIn) -> DtResult<()> {
    let (hour, min, sec, usec) = decode_time_common(s, range)?;
    itm.usec = i64::from(usec);
    let ok = hour
        .checked_mul(USECS_PER_HOUR)
        .and_then(|v| itm.usec.checked_add(v))
        .map(|v| itm.usec = v)
        .is_some()
        && i64::from(min)
            .checked_mul(USECS_PER_MINUTE)
            .and_then(|v| itm.usec.checked_add(v))
            .map(|v| itm.usec = v)
            .is_some()
        && i64::from(sec)
            .checked_mul(USECS_PER_SEC)
            .and_then(|v| itm.usec.checked_add(v))
            .map(|v| itm.usec = v)
            .is_some();
    if ok {
        Ok(())
    } else {
        Err(DtErr::FieldOverflow)
    }
}

/// Result of interval decoding.
pub(crate) enum DecodedInterval {
    Finite(ItmIn),
    Infinite { negative: bool },
}

/// `DecodeInterval`.
#[allow(clippy::too_many_lines)]
pub(crate) fn decode_interval(
    fields: &[Field],
    range: i32,
    style: IntervalStyle,
) -> DtResult<DecodedInterval> {
    let nf = fields.len();
    let mut force_negative = false;
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask: i32 = 0;
    let mut ty = IGNORE_DTF;
    let mut itm = ItmIn::default();
    let mut dtype = DTK_DELTA;

    if style == IntervalStyle::SqlStandard && nf > 0 && fields[0].text.first() == Some(&b'-') {
        force_negative = true;
        for f in &fields[1..] {
            let c = f.text.first().copied().unwrap_or(0);
            if c == b'-' || c == b'+' {
                force_negative = false;
                break;
            }
        }
    }

    for i in (0..nf).rev() {
        let text: &[u8] = &fields[i].text;
        let mut tmask: i32;
        let mut ftype = fields[i].ftype;
        if ftype == DTK_TIME {
            decode_time_for_interval(text, range, &mut itm)?;
            if force_negative && itm.usec > 0 {
                itm.usec = -itm.usec;
            }
            ty = DTK_DAY;
            parsing_unit_val = false;
            tmask = DTK_TIME_M;
            if tmask & fmask != 0 {
                return Err(DtErr::BadFormat);
            }
            fmask |= tmask;
            continue;
        }
        if ftype == DTK_TZ {
            if text[1..].contains(&b':') && {
                // DecodeTimeForInterval may update the accumulator even
                // when it fails part-way; mirror that.
                let mut probe = itm;
                let r = decode_time_for_interval(&text[1..], range, &mut probe);
                itm = probe;
                r.is_ok()
            } {
                if text[0] == b'-' {
                    if itm.usec == i64::MIN {
                        return Err(DtErr::FieldOverflow);
                    }
                    itm.usec = -itm.usec;
                }
                if force_negative && itm.usec > 0 {
                    itm.usec = -itm.usec;
                }
                ty = DTK_DAY;
                parsing_unit_val = false;
                tmask = DTK_TIME_M;
                if tmask & fmask != 0 {
                    return Err(DtErr::BadFormat);
                }
                fmask |= tmask;
                continue;
            }
            ftype = DTK_NUMBER;
        }
        match ftype {
            DTK_DATE | DTK_NUMBER => {
                if ty == IGNORE_DTF {
                    ty = match range {
                        r if r == interval_mask(YEAR) => DTK_YEAR,
                        r if r == interval_mask(MONTH)
                            || r == (interval_mask(YEAR) | interval_mask(MONTH)) =>
                        {
                            DTK_MONTH
                        }
                        r if r == interval_mask(DAY) => DTK_DAY,
                        r if r == interval_mask(HOUR)
                            || r == (interval_mask(DAY) | interval_mask(HOUR)) =>
                        {
                            DTK_HOUR
                        }
                        r if r == interval_mask(MINUTE)
                            || r == (interval_mask(HOUR) | interval_mask(MINUTE))
                            || r == (interval_mask(DAY)
                                | interval_mask(HOUR)
                                | interval_mask(MINUTE)) =>
                        {
                            DTK_MINUTE
                        }
                        _ => DTK_SECOND,
                    };
                }
                let (mut val, n, of) = strtol(text);
                if of {
                    return Err(DtErr::FieldOverflow);
                }
                let mut cp = n;
                let mut fval: f64;
                if cp < text.len() && text[cp] == b'-' {
                    let (mut val2, n2, of2) = strtoint(&text[cp + 1..]);
                    if of2 || !(0..MONTHS_PER_YEAR).contains(&val2) {
                        return Err(DtErr::FieldOverflow);
                    }
                    cp += 1 + n2;
                    if cp != text.len() {
                        return Err(DtErr::BadFormat);
                    }
                    ty = DTK_MONTH;
                    if text[0] == b'-' {
                        val2 = -val2;
                    }
                    val = val
                        .checked_mul(i64::from(MONTHS_PER_YEAR))
                        .and_then(|v| v.checked_add(i64::from(val2)))
                        .ok_or(DtErr::FieldOverflow)?;
                    fval = 0.0;
                } else if cp < text.len() && text[cp] == b'.' {
                    fval = parse_fraction(&text[cp..])?;
                    if text[0] == b'-' {
                        fval = -fval;
                    }
                } else if cp == text.len() {
                    fval = 0.0;
                } else {
                    return Err(DtErr::BadFormat);
                }
                if force_negative {
                    if val > 0 {
                        val = -val;
                    }
                    if fval > 0.0 {
                        fval = -fval;
                    }
                }
                let ok;
                match ty {
                    DTK_MICROSEC => {
                        ok = adjust_microseconds(val, fval, 1, &mut itm);
                        tmask = dtk_m(MICROSECOND);
                    }
                    DTK_MILLISEC => {
                        ok = adjust_microseconds(val, fval, 1000, &mut itm);
                        tmask = dtk_m(MILLISECOND);
                    }
                    DTK_SECOND => {
                        ok = adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm);
                        tmask = if fval == 0.0 {
                            dtk_m(SECOND)
                        } else {
                            DTK_ALL_SECS_M
                        };
                    }
                    DTK_MINUTE => {
                        ok = adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm);
                        tmask = dtk_m(MINUTE);
                    }
                    DTK_HOUR => {
                        ok = adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm);
                        tmask = dtk_m(HOUR);
                        ty = DTK_DAY;
                    }
                    DTK_DAY => {
                        ok = adjust_days(val, 1, &mut itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm);
                        tmask = dtk_m(DAY);
                    }
                    DTK_WEEK => {
                        ok = adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm);
                        tmask = dtk_m(WEEK);
                    }
                    DTK_MONTH => {
                        ok = adjust_months(val, &mut itm)
                            && adjust_fract_days(fval, DAYS_PER_MONTH, &mut itm);
                        tmask = dtk_m(MONTH);
                    }
                    DTK_YEAR => {
                        ok =
                            adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm);
                        tmask = dtk_m(YEAR);
                    }
                    DTK_DECADE => {
                        ok = adjust_years(val, 10, &mut itm)
                            && adjust_fract_years(fval, 10, &mut itm);
                        tmask = dtk_m(DECADE);
                    }
                    DTK_CENTURY => {
                        ok = adjust_years(val, 100, &mut itm)
                            && adjust_fract_years(fval, 100, &mut itm);
                        tmask = dtk_m(CENTURY);
                    }
                    DTK_MILLENNIUM => {
                        ok = adjust_years(val, 1000, &mut itm)
                            && adjust_fract_years(fval, 1000, &mut itm);
                        tmask = dtk_m(MILLENNIUM);
                    }
                    _ => return Err(DtErr::BadFormat),
                }
                if !ok {
                    return Err(DtErr::FieldOverflow);
                }
                parsing_unit_val = false;
            }
            DTK_STRING | DTK_SPECIAL => {
                if parsing_unit_val {
                    return Err(DtErr::BadFormat);
                }
                let (mut t, mut uval) = decode_units(text);
                if t == UNKNOWN_FIELD {
                    (t, uval) = decode_special(text);
                }
                if t == IGNORE_DTF {
                    continue;
                }
                tmask = 0;
                match t {
                    UNITS => {
                        ty = uval;
                        parsing_unit_val = true;
                    }
                    AGO => {
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        is_before = true;
                        ty = uval;
                    }
                    RESERV => {
                        tmask = DTK_DATE_M | DTK_TIME_M;
                        if uval != DTK_LATE && uval != DTK_EARLY {
                            return Err(DtErr::BadFormat);
                        }
                        if i != nf - 1 {
                            return Err(DtErr::BadFormat);
                        }
                        dtype = uval;
                    }
                    _ => return Err(DtErr::BadFormat),
                }
            }
            _ => return Err(DtErr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(DtErr::BadFormat);
        }
        fmask |= tmask;
    }

    if fmask == 0 {
        return Err(DtErr::BadFormat);
    }
    if parsing_unit_val {
        return Err(DtErr::BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN
            || itm.mday == i32::MIN
            || itm.mon == i32::MIN
            || itm.year == i32::MIN
        {
            return Err(DtErr::FieldOverflow);
        }
        itm.usec = -itm.usec;
        itm.mday = -itm.mday;
        itm.mon = -itm.mon;
        itm.year = -itm.year;
    }
    match dtype {
        DTK_LATE => Ok(DecodedInterval::Infinite { negative: false }),
        DTK_EARLY => Ok(DecodedInterval::Infinite { negative: true }),
        _ => Ok(DecodedInterval::Finite(itm)),
    }
}

/// `ParseISO8601Number`: `(ipart, fpart, consumed)`.
fn parse_iso8601_number(s: &[u8]) -> DtResult<(i64, f64, usize)> {
    let c = s.first().copied().unwrap_or(0);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(DtErr::BadFormat);
    }
    let (val, n, erange) = strtod(s);
    if n == 0 || erange {
        return Err(DtErr::BadFormat);
    }
    if val.is_nan() || !(-1.0e15..=1.0e15).contains(&val) {
        return Err(DtErr::FieldOverflow);
    }
    #[allow(clippy::cast_possible_truncation)]
    let ipart = if val >= 0.0 {
        val.floor() as i64
    } else {
        -((-val).floor() as i64)
    };
    #[allow(clippy::cast_precision_loss)]
    let fpart = val - ipart as f64;
    Ok((ipart, fpart, n))
}

fn iso8601_integer_width(s: &[u8]) -> usize {
    let s = s.strip_prefix(b"-").unwrap_or(s);
    s.iter().take_while(|c| c.is_ascii_digit()).count()
}

/// `DecodeISO8601Interval` (operates on the raw input string).
#[allow(clippy::too_many_lines)]
pub(crate) fn decode_iso8601_interval(input: &str) -> DtResult<ItmIn> {
    let s = input.as_bytes();
    let mut itm = ItmIn::default();
    if s.len() < 2 || s[0] != b'P' {
        return Err(DtErr::BadFormat);
    }
    let at = |i: usize| -> u8 { s.get(i).copied().unwrap_or(0) };
    let mut p = 1usize;
    let mut datepart = true;
    let mut havefield = false;
    macro_rules! chk {
        ($e:expr) => {
            if !$e {
                return Err(DtErr::FieldOverflow);
            }
        };
    }
    while at(p) != 0 {
        if at(p) == b'T' {
            datepart = false;
            havefield = false;
            p += 1;
            continue;
        }
        let fieldstart = p;
        let (val, fval, n) = parse_iso8601_number(&s[p..])?;
        p += n;
        let unit = at(p);
        p += 1;
        if datepart {
            match unit {
                b'Y' => {
                    chk!(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm));
                }
                b'M' => chk!(
                    adjust_months(val, &mut itm)
                        && adjust_fract_days(fval, DAYS_PER_MONTH, &mut itm)
                ),
                b'W' => chk!(adjust_days(val, 7, &mut itm) && adjust_fract_days(fval, 7, &mut itm)),
                b'D' => chk!(
                    adjust_days(val, 1, &mut itm)
                        && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                ),
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso8601_integer_width(&s[fieldstart..]) == 8 && !havefield {
                        chk!(
                            adjust_years(val / 10000, 1, &mut itm)
                                && adjust_months((val / 100) % 100, &mut itm)
                                && adjust_days(val % 100, 1, &mut itm)
                                && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                        );
                        if unit == 0 {
                            return Ok(itm);
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    chk!(adjust_years(val, 1, &mut itm) && adjust_fract_years(fval, 1, &mut itm));
                    if unit == 0 {
                        return Ok(itm);
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (val, fval, n) = parse_iso8601_number(&s[p..])?;
                    p += n;
                    chk!(
                        adjust_months(val, &mut itm)
                            && adjust_fract_days(fval, DAYS_PER_MONTH, &mut itm)
                    );
                    if at(p) == 0 {
                        return Ok(itm);
                    }
                    if at(p) == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    if at(p) != b'-' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;
                    let (val, fval, n) = parse_iso8601_number(&s[p..])?;
                    p += n;
                    chk!(
                        adjust_days(val, 1, &mut itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, &mut itm)
                    );
                    if at(p) == 0 {
                        return Ok(itm);
                    }
                    if at(p) == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        } else {
            match unit {
                b'H' => chk!(adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm)),
                b'M' => chk!(adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm)),
                b'S' => chk!(adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm)),
                0 | b':' => {
                    if unit == 0 && iso8601_integer_width(&s[fieldstart..]) == 6 && !havefield {
                        chk!(
                            adjust_microseconds(val / 10000, 0.0, USECS_PER_HOUR, &mut itm)
                                && adjust_microseconds(
                                    (val / 100) % 100,
                                    0.0,
                                    USECS_PER_MINUTE,
                                    &mut itm
                                )
                                && adjust_microseconds(val % 100, 0.0, USECS_PER_SEC, &mut itm)
                                && adjust_fract_microseconds(fval, 1, &mut itm)
                        );
                        return Ok(itm);
                    }
                    if havefield {
                        return Err(DtErr::BadFormat);
                    }
                    chk!(adjust_microseconds(val, fval, USECS_PER_HOUR, &mut itm));
                    if unit == 0 {
                        return Ok(itm);
                    }
                    let (val, fval, n) = parse_iso8601_number(&s[p..])?;
                    p += n;
                    chk!(adjust_microseconds(val, fval, USECS_PER_MINUTE, &mut itm));
                    if at(p) == 0 {
                        return Ok(itm);
                    }
                    if at(p) != b':' {
                        return Err(DtErr::BadFormat);
                    }
                    p += 1;
                    let (val, fval, n) = parse_iso8601_number(&s[p..])?;
                    p += n;
                    chk!(adjust_microseconds(val, fval, USECS_PER_SEC, &mut itm));
                    if at(p) == 0 {
                        return Ok(itm);
                    }
                    return Err(DtErr::BadFormat);
                }
                _ => return Err(DtErr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok(itm)
}
