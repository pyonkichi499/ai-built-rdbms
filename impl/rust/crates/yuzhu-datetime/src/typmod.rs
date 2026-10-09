//! Type modifier input / output for `time`, `timestamp`, `timestamptz` and
//! `interval` (`timetypmodin`, `timestamptypmodin`, `intervaltypmodin` and
//! the matching `*typmodout` functions of PostgreSQL).

use crate::calendar::MAX_PRECISION;
use crate::error::{DateTimeError, Result, sqlstate};
use crate::interval::{interval_typmod, range};
use crate::parse::{INTERVAL_FULL_RANGE, interval_mask};
use crate::tokens::{DAY, HOUR, MINUTE, MONTH, SECOND, YEAR};

/// `INTERVAL_FULL_PRECISION`: the precision field when none was given.
const FULL_PRECISION: i32 = 0xFFFF;

/// The upper-case type name as the `*typmodin` error messages spell it
/// (`TIME(-1) precision ...`, `TIMESTAMP(-1) WITH TIME ZONE precision ...`).
fn message_prefix(type_name: &str, p: i64) -> String {
    match type_name.to_ascii_lowercase().as_str() {
        "timestamptz" => format!("TIMESTAMP({p}) WITH TIME ZONE"),
        "timetz" => format!("TIME({p}) WITH TIME ZONE"),
        other => format!("{}({p})", other.to_ascii_uppercase()),
    }
}

fn precision_in(prefix: &str, p: i64) -> Result<(i32, Option<DateTimeError>)> {
    if p < 0 {
        return Err(DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("{prefix} precision must not be negative"),
        ));
    }
    if p > i64::from(MAX_PRECISION) {
        let warning = DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("{prefix} precision reduced to maximum allowed, {MAX_PRECISION}"),
        );
        return Ok((MAX_PRECISION, Some(warning)));
    }
    // 0 <= p <= 6.
    #[allow(clippy::cast_possible_truncation)]
    Ok((p as i32, None))
}

/// `time(p)` / `timestamp(p)` / `timestamptz(p)` typmodin. `type_name` is
/// `time`, `timetz`, `timestamp` or `timestamptz`. A negative `p` is an
/// error (22023); `p > 6` gives 6 together with a WARNING to report.
pub fn precision_typmod_in(type_name: &str, p: i64) -> Result<(i32, Option<DateTimeError>)> {
    precision_in(&message_prefix(type_name, p), p)
}

/// `intervaltypmodin`. `mods` is `[]`, `[range]` or `[range, precision]`.
pub fn interval_typmod_in(mods: &[i64]) -> Result<(i32, Option<DateTimeError>)> {
    let invalid = || {
        DateTimeError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid INTERVAL type modifier",
        )
    };
    let Some(&r) = mods.first() else {
        return Ok((-1, None));
    };
    let m = interval_mask;
    let valid = [
        m(YEAR),
        m(MONTH),
        m(DAY),
        m(HOUR),
        m(MINUTE),
        m(SECOND),
        m(YEAR) | m(MONTH),
        m(DAY) | m(HOUR),
        m(DAY) | m(HOUR) | m(MINUTE),
        m(DAY) | m(HOUR) | m(MINUTE) | m(SECOND),
        m(HOUR) | m(MINUTE),
        m(HOUR) | m(MINUTE) | m(SECOND),
        m(MINUTE) | m(SECOND),
        INTERVAL_FULL_RANGE,
    ];
    let r = i32::try_from(r)
        .ok()
        .filter(|r| valid.contains(r))
        .ok_or_else(invalid)?;
    match mods {
        [_] if r == INTERVAL_FULL_RANGE => Ok((-1, None)),
        [_] => Ok((interval_typmod(None, r), None)),
        [_, p] => {
            let (p, warning) = precision_in(&message_prefix("interval", *p), *p)?;
            Ok((interval_typmod(Some(p), r), warning))
        }
        _ => Err(invalid()),
    }
}

/// `time(p)` etc. typmodout: `""` or `"(3)"`.
pub fn precision_typmod_out(typmod: i32) -> String {
    if typmod < 0 {
        String::new()
    } else {
        format!("({typmod})")
    }
}

/// `intervaltypmodout`: `""`, `"(3)"`, `" year to month"`, `" second(3)"`.
pub fn interval_typmod_out(typmod: i32) -> String {
    if typmod < 0 {
        return String::new();
    }
    let fields = (typmod >> 16) & 0x7FFF;
    let precision = typmod & 0xFFFF;
    let m = interval_mask;
    let names: &[(i32, &str)] = &[
        (m(YEAR), " year"),
        (m(MONTH), " month"),
        (m(DAY), " day"),
        (m(HOUR), " hour"),
        (m(MINUTE), " minute"),
        (m(SECOND), " second"),
        (m(YEAR) | m(MONTH), " year to month"),
        (m(DAY) | m(HOUR), " day to hour"),
        (m(DAY) | m(HOUR) | m(MINUTE), " day to minute"),
        (m(DAY) | m(HOUR) | m(MINUTE) | m(SECOND), " day to second"),
        (m(HOUR) | m(MINUTE), " hour to minute"),
        (m(HOUR) | m(MINUTE) | m(SECOND), " hour to second"),
        (m(MINUTE) | m(SECOND), " minute to second"),
        (range::FULL, ""),
    ];
    let field_str = names
        .iter()
        .find(|(mask, _)| *mask == fields)
        .map_or("?", |(_, s)| s);
    if precision == FULL_PRECISION {
        field_str.to_owned()
    } else {
        format!("{field_str}({precision})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_in_range_and_warning() {
        assert_eq!(precision_typmod_in("time", 3).unwrap(), (3, None));
        assert_eq!(precision_typmod_in("timestamp", 0).unwrap(), (0, None));
        let (p, w) = precision_typmod_in("time", 7).unwrap();
        assert_eq!(p, 6);
        let w = w.unwrap();
        assert_eq!(w.sqlstate, "22023");
        assert_eq!(w.message, "TIME(7) precision reduced to maximum allowed, 6");
        let e = precision_typmod_in("time", -1).unwrap_err();
        assert_eq!(e.sqlstate, "22023");
        assert_eq!(e.message, "TIME(-1) precision must not be negative");
        let e = precision_typmod_in("timestamptz", -1).unwrap_err();
        assert_eq!(
            e.message,
            "TIMESTAMP(-1) WITH TIME ZONE precision must not be negative"
        );
    }

    #[test]
    fn interval_typmod_in_matches_postgres() {
        assert_eq!(interval_typmod_in(&[]).unwrap().0, -1);
        assert_eq!(interval_typmod_in(&[0x7FFF]).unwrap().0, -1);
        assert_eq!(interval_typmod_in(&[4]).unwrap().0, 327_679);
        assert_eq!(interval_typmod_in(&[0x7FFF, 3]).unwrap().0, 2_147_418_115);
        let (t, w) = interval_typmod_in(&[4096, 9]).unwrap();
        assert_eq!(t, interval_typmod(Some(6), range::SECOND));
        assert_eq!(
            w.unwrap().message,
            "INTERVAL(9) precision reduced to maximum allowed, 6"
        );
        let e = interval_typmod_in(&[0x7FFF, -1]).unwrap_err();
        assert_eq!(e.message, "INTERVAL(-1) precision must not be negative");
        let e = interval_typmod_in(&[5]).unwrap_err();
        assert_eq!(e.message, "invalid INTERVAL type modifier");
        assert_eq!(
            interval_typmod_in(&[1, 2, 3]).unwrap_err().sqlstate,
            "22023"
        );
    }

    #[test]
    fn typmod_out_strings() {
        assert_eq!(precision_typmod_out(-1), "");
        assert_eq!(precision_typmod_out(3), "(3)");
        assert_eq!(interval_typmod_out(-1), "");
        assert_eq!(
            interval_typmod_out(interval_typmod(Some(3), range::FULL)),
            "(3)"
        );
        assert_eq!(
            interval_typmod_out(interval_typmod(None, range::YEAR | range::MONTH)),
            " year to month"
        );
        assert_eq!(
            interval_typmod_out(interval_typmod(Some(3), range::SECOND)),
            " second(3)"
        );
        assert_eq!(
            interval_typmod_out(interval_typmod(
                Some(2),
                range::DAY | range::HOUR | range::MINUTE | range::SECOND
            )),
            " day to second(2)"
        );
        assert_eq!(
            interval_typmod_out(interval_typmod(None, range::DAY)),
            " day"
        );
    }

    #[test]
    fn roundtrip_all_ranges() {
        for mods in [
            vec![i64::from(range::YEAR)],
            vec![i64::from(range::MONTH)],
            vec![i64::from(range::HOUR | range::MINUTE)],
            vec![i64::from(range::MINUTE | range::SECOND), 4],
        ] {
            let (t, _) = interval_typmod_in(&mods).unwrap();
            assert!(!interval_typmod_out(t).contains('?'));
        }
    }
}
