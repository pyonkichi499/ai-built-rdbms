//! `interval` の統合（TD-1）。`m5/06-types-datetime.md` §6.2、§6.5。
//!
//! 値の本体は `yuzhu_datetime::Interval`（月・日・マイクロ秒の 3 要素。比較とハッシュは換算値）。
//! ここには、ディスク形式、演算子・キャスト・集約の本体を置く。関数の形は `FnKind` に合わせる
//! （`Pure`: `fn(&[Datum]) -> Result<Datum>`、`Env`: `fn(&[Datum], &TypeEnv) -> Result<Datum>`）。
//! 引数は NULL でない（strict）。

use std::cmp::Ordering;

use yuzhu_datetime::{Date, Interval, Timestamp, TimestampTz};

use super::datetime::{dt_env, dt_err};
use super::{Datum, TypeEnv};
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// ディスク形式（16 バイト、LE: time i64、day i32、month i32）
// ---------------------------------------------------------------------------

/// ディスク形式。PostgreSQL の `Interval` 構造体と同じ順序（`time`、`day`、`month`）。
pub fn encode(iv: &Interval) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&iv.micros.to_le_bytes());
    out[8..12].copy_from_slice(&iv.days.to_le_bytes());
    out[12..].copy_from_slice(&iv.months.to_le_bytes());
    out
}

/// [`encode`] の逆。
pub fn decode(b: &[u8; 16]) -> Interval {
    let micros = i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let days = i32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    let months = i32::from_le_bytes([b[12], b[13], b[14], b[15]]);
    Interval {
        months,
        days,
        micros,
    }
}

/// スライスから復元する。長さが 16 でなければ `XX001`。
pub fn decode_slice(b: &[u8]) -> Result<Interval> {
    let arr: &[u8; 16] = b.try_into().map_err(|_| {
        Error::corrupted(format!("invalid length {} for a stored interval", b.len()))
    })?;
    Ok(decode(arr))
}

// ---------------------------------------------------------------------------
// 入出力
// ---------------------------------------------------------------------------

/// `interval_in`。`typmod` は列の型の値（`-1` = 指定なし）で、入力の解釈中に適用される。
pub fn interval_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum> {
    Interval::parse(s, typmod, dt_env(env)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `interval_out`（`IntervalStyle` に従う）。
pub fn interval_out(d: &Datum, env: &TypeEnv<'_>) -> Result<String> {
    Ok(interval_of(d)?.format(dt_env(env)?.interval_style))
}

// ---------------------------------------------------------------------------
// 引数の取り出し
// ---------------------------------------------------------------------------

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

fn interval_of(d: &Datum) -> Result<&Interval> {
    match d {
        Datum::Interval(v) => Ok(v),
        _ => Err(bad_arg("interval function")),
    }
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| bad_arg("interval function"))
}

fn interval_arg(args: &[Datum], i: usize) -> Result<&Interval> {
    interval_of(arg(args, i)?)
}

fn float8_arg(args: &[Datum], i: usize) -> Result<f64> {
    match arg(args, i)? {
        Datum::Float8(v) => Ok(*v),
        _ => Err(bad_arg("interval function")),
    }
}

fn date_arg(args: &[Datum], i: usize) -> Result<Date> {
    match arg(args, i)? {
        Datum::Date(v) => Ok(*v),
        _ => Err(bad_arg("interval function")),
    }
}

fn timestamp_arg(args: &[Datum], i: usize) -> Result<Timestamp> {
    match arg(args, i)? {
        Datum::Timestamp(v) => Ok(*v),
        _ => Err(bad_arg("interval function")),
    }
}

fn timestamptz_arg(args: &[Datum], i: usize) -> Result<TimestampTz> {
    match arg(args, i)? {
        Datum::TimestampTz(v) => Ok(*v),
        _ => Err(bad_arg("interval function")),
    }
}

// ---------------------------------------------------------------------------
// 比較（換算値。`'1 day' = '24:00:00'`）
// ---------------------------------------------------------------------------

fn cmp_args(args: &[Datum]) -> Result<Ordering> {
    Ok(interval_arg(args, 0)?.cmp(interval_arg(args, 1)?))
}

/// `interval_eq`（1162）。
pub fn interval_eq(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Equal))
}

/// `interval_ne`（1163）。
pub fn interval_ne(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Equal))
}

/// `interval_lt`（1164）。
pub fn interval_lt(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Less))
}

/// `interval_le`（1165）。
pub fn interval_le(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Greater))
}

/// `interval_ge`（1166）。
pub fn interval_ge(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Less))
}

/// `interval_gt`（1167）。
pub fn interval_gt(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Greater))
}

/// `interval_cmp`（1315。B+Tree の比較関数）。
pub fn interval_cmp(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(match cmp_args(args)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

/// `interval_smaller`（1198）。等しいときは後の引数（PostgreSQL と同じ）。
pub fn interval_smaller(args: &[Datum]) -> Result<Datum> {
    Ok(if cmp_args(args)? == Ordering::Less {
        arg(args, 0)?.clone()
    } else {
        arg(args, 1)?.clone()
    })
}

/// `interval_larger`（1197）。等しいときは後の引数。
pub fn interval_larger(args: &[Datum]) -> Result<Datum> {
    Ok(if cmp_args(args)? == Ordering::Greater {
        arg(args, 0)?.clone()
    } else {
        arg(args, 1)?.clone()
    })
}

// ---------------------------------------------------------------------------
// interval どうしの演算
// ---------------------------------------------------------------------------

/// 単項 `-`（`interval_um`。1168）。
pub fn interval_um(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .negate()
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `interval + interval`（`interval_pl`。1169）。
pub fn interval_pl(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .add(interval_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `interval - interval`（`interval_mi`。1170）。
pub fn interval_mi(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .sub(interval_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `interval * float8`（`interval_mul`。1618）。
pub fn interval_mul(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .mul(float8_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `float8 * interval`（`mul_d_interval`。1624）。
pub fn mul_d_interval(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 1)?
        .mul(float8_arg(args, 0)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `interval / float8`（`interval_div`。1326）。
pub fn interval_div(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .div(float8_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `justify_days`（1295）。
pub fn interval_justify_days(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .justify_days()
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `justify_hours`（1175）。
pub fn interval_justify_hours(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .justify_hours()
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `justify_interval`（2711）。
pub fn interval_justify_interval(args: &[Datum]) -> Result<Datum> {
    interval_arg(args, 0)?
        .justify_interval()
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `isfinite(interval)`（1390）。
pub fn interval_finite(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(interval_arg(args, 0)?.is_finite()))
}

// ---------------------------------------------------------------------------
// 型修飾子のキャスト（pg_cast 1200: `interval(interval, int4)`）
// ---------------------------------------------------------------------------

/// `interval_scale`（1200）。引数は `(interval, typmod)`。
pub fn interval_scale(args: &[Datum]) -> Result<Datum> {
    let typmod = match arg(args, 1)? {
        Datum::Int4(v) => *v,
        _ => return Err(bad_arg("interval_scale")),
    };
    interval_arg(args, 0)?
        .with_typmod(typmod)
        .map(Datum::Interval)
        .map_err(dt_err)
}

// ---------------------------------------------------------------------------
// 日時との演算
// ---------------------------------------------------------------------------

/// `BuiltinOperator.func`（純粋な関数）に置く、`TypeEnv` が要る演算子（`timestamptz ± interval`）の
/// 代わりの本体。呼ばれたら内部エラー。実行は同じ `pg_proc` の `FnKind::Env` の関数で行う。
pub fn needs_env(_args: &[Datum]) -> Result<Datum> {
    Err(Error::internal(
        "this operator depends on the session time zone and must be evaluated with a TypeEnv",
    ))
}

/// `date + interval`（`date_pl_interval`。2071）。
pub fn date_pl_interval(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .add_interval(interval_arg(args, 1)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `date - interval`（`date_mi_interval`。2072）。
pub fn date_mi_interval(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .sub_interval(interval_arg(args, 1)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `interval + date`（`interval_pl_date`。2546）。
pub fn interval_pl_date(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 1)?
        .add_interval(interval_arg(args, 0)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamp + interval`（`timestamp_pl_interval`。2032）。
pub fn timestamp_pl_interval(args: &[Datum]) -> Result<Datum> {
    timestamp_arg(args, 0)?
        .add_interval(interval_arg(args, 1)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamp - interval`（`timestamp_mi_interval`。2033）。
pub fn timestamp_mi_interval(args: &[Datum]) -> Result<Datum> {
    timestamp_arg(args, 0)?
        .sub_interval(interval_arg(args, 1)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `interval + timestamp`（`interval_pl_timestamp`。2548）。
pub fn interval_pl_timestamp(args: &[Datum]) -> Result<Datum> {
    timestamp_arg(args, 1)?
        .add_interval(interval_arg(args, 0)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamp - timestamp`（`timestamp_mi`。2031）。日を月に繰り上げない。
pub fn timestamp_mi(args: &[Datum]) -> Result<Datum> {
    timestamp_arg(args, 0)?
        .sub_timestamp(timestamp_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `timestamptz - timestamptz`（`timestamptz_mi`。1188）。ゾーンによらない。
pub fn timestamptz_mi(args: &[Datum]) -> Result<Datum> {
    timestamptz_arg(args, 0)?
        .sub_timestamptz(timestamptz_arg(args, 1)?)
        .map(Datum::Interval)
        .map_err(dt_err)
}

/// `timestamptz + interval`（`timestamptz_pl_interval`。1189）。月と日はセッションのゾーンの
/// 壁時計で足す（stable。`FnKind::Env`）。
pub fn timestamptz_pl_interval(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamptz_arg(args, 0)?
        .add_interval(interval_arg(args, 1)?, dt_env(env)?.time_zone)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `timestamptz - interval`（`timestamptz_mi_interval`。1190）。
pub fn timestamptz_mi_interval(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamptz_arg(args, 0)?
        .sub_interval(interval_arg(args, 1)?, dt_env(env)?.time_zone)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `interval + timestamptz`（`interval_pl_timestamptz`。2549）。
pub fn interval_pl_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamptz_arg(args, 1)?
        .add_interval(interval_arg(args, 0)?, dt_env(env)?.time_zone)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

// ---------------------------------------------------------------------------
// 集約の状態（`sum(interval)`、`avg(interval)`。min / max は `interval_smaller` / `interval_larger`
// か、汎用の `cmp_datum` で動く）
// ---------------------------------------------------------------------------

/// `sum(interval)`（2113）の状態。空入力は `None`（SQL の NULL）。
#[derive(Debug, Clone, Default)]
pub struct IntervalSum {
    sum: Option<Interval>,
}

impl IntervalSum {
    pub fn new() -> Self {
        Self::default()
    }

    /// 非 NULL の入力を 1 つ足す（`interval_pl`。溢れは 22008）。
    pub fn accumulate(&mut self, v: &Datum) -> Result<()> {
        let v = interval_of(v)?;
        self.sum = Some(match &self.sum {
            None => *v,
            Some(s) => s.add(v).map_err(dt_err)?,
        });
        Ok(())
    }

    /// 結果。入力が無ければ `Datum::Null`。
    pub fn finish(&self) -> Datum {
        self.sum.map_or(Datum::Null, Datum::Interval)
    }
}

/// `avg(interval)`（2106）の状態。
#[derive(Debug, Clone, Default)]
pub struct IntervalAvg {
    sum: Option<Interval>,
    count: i64,
}

impl IntervalAvg {
    pub fn new() -> Self {
        Self::default()
    }

    /// 非 NULL の入力を 1 つ足す。
    pub fn accumulate(&mut self, v: &Datum) -> Result<()> {
        let v = interval_of(v)?;
        self.sum = Some(match &self.sum {
            None => *v,
            Some(s) => s.add(v).map_err(dt_err)?,
        });
        self.count += 1;
        Ok(())
    }

    /// 結果。`interval_div(sum, count::float8)`。入力が無ければ `Datum::Null`。
    #[allow(clippy::cast_precision_loss)]
    pub fn finish(&self) -> Result<Datum> {
        match &self.sum {
            None => Ok(Datum::Null),
            Some(s) => s
                .div(self.count as f64)
                .map(Datum::Interval)
                .map_err(dt_err),
        }
    }
}

#[cfg(test)]
mod tests {
    use yuzhu_datetime::{DateTimeEnv, TimeZone, ZoneDb};

    use super::*;

    fn with_env<R>(tz: &TimeZone, zones: &ZoneDb, f: impl FnOnce(&TypeEnv<'_>) -> R) -> R {
        let env = TypeEnv {
            datetime: Some(DateTimeEnv::new(tz, zones)),
            ..TypeEnv::default()
        };
        f(&env)
    }

    fn utc<R>(f: impl FnOnce(&TypeEnv<'_>) -> R) -> R {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        with_env(&tz, &zones, f)
    }

    fn iv(s: &str) -> Datum {
        utc(|env| interval_in(s, -1, env)).unwrap()
    }

    fn show(d: &Datum) -> String {
        utc(|env| interval_out(d, env)).unwrap()
    }

    fn ts(s: &str) -> Datum {
        utc(|env| super::super::datetime::timestamp_in(s, -1, env)).unwrap()
    }

    fn code(e: &Error) -> &str {
        e.sqlstate.code()
    }

    #[test]
    fn disk_roundtrip() {
        let v = Interval {
            months: 14,
            days: 3,
            micros: 14_706_789_000,
        };
        let b = encode(&v);
        assert_eq!(
            b,
            [
                0x88, 0xCA, 0x97, 0x6C, 0x03, 0, 0, 0, // time
                3, 0, 0, 0, // day
                14, 0, 0, 0, // month
            ]
        );
        assert!(decode(&b).fields_eq(&v));
        for v in [Interval::INFINITY, Interval::NEG_INFINITY, Interval::ZERO] {
            assert!(decode_slice(&encode(&v)).unwrap().fields_eq(&v));
        }
        assert_eq!(code(&decode_slice(&[0; 15]).unwrap_err()), "XX001");
    }

    #[test]
    fn io_styles() {
        let d = iv("1 year 2 mons 3 days 04:05:06.789");
        assert_eq!(show(&d), "1 year 2 mons 3 days 04:05:06.789");
        assert_eq!(show(&iv("@ 1 minute ago")), "-00:01:00");
        assert_eq!(show(&iv("P1Y2M3DT4H5M6S")), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(show(&iv("1.5 years")), "1 year 6 mons");
        assert_eq!(show(&iv("infinity")), "infinity");
        let e = utc(|env| interval_in("garbage", -1, env)).unwrap_err();
        assert_eq!(code(&e), "22007");
    }

    #[test]
    fn input_with_typmod() {
        let tm = yuzhu_datetime::interval_typmod(Some(1), yuzhu_datetime::interval_range::SECOND);
        let d = utc(|env| interval_in("1:2:3.45", tm, env)).unwrap();
        assert_eq!(show(&d), "01:02:03.5");
        let tm = yuzhu_datetime::interval_typmod(None, yuzhu_datetime::interval_range::YEAR);
        let d = utc(|env| interval_in("1 year 2 months", tm, env)).unwrap();
        assert_eq!(show(&d), "1 year");
    }

    #[test]
    fn comparison_uses_converted_value() {
        let a = iv("1 day");
        let b = iv("24:00:00");
        assert_eq!(
            interval_eq(&[a.clone(), b.clone()]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            interval_cmp(&[a.clone(), b.clone()]).unwrap(),
            Datum::Int4(0)
        );
        assert_eq!(
            interval_lt(&[iv("1 mon"), iv("31 days")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            interval_ge(&[iv("1 mon"), iv("30 days")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            interval_ne(&[iv("1 mon"), iv("30 days")]).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(
            interval_gt(&[iv("infinity"), iv("100 years")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            interval_le(&[iv("-infinity"), iv("100 years")]).unwrap(),
            Datum::Bool(true)
        );
        // 等しいときは後の引数
        assert_eq!(
            show(&interval_smaller(&[a.clone(), b.clone()]).unwrap()),
            "24:00:00"
        );
        assert_eq!(show(&interval_larger(&[a, b]).unwrap()), "24:00:00");
    }

    #[test]
    fn arithmetic() {
        assert_eq!(
            show(&interval_um(&[iv("1 day 02:00")]).unwrap()),
            "-1 days -02:00:00"
        );
        assert_eq!(
            show(&interval_pl(&[iv("1 day"), iv("25 hours")]).unwrap()),
            "1 day 25:00:00"
        );
        assert_eq!(
            show(&interval_mi(&[iv("1 day"), iv("25 hours")]).unwrap()),
            "1 day -25:00:00"
        );
        assert_eq!(
            show(&interval_mul(&[iv("1 month"), Datum::Float8(1.5)]).unwrap()),
            "1 mon 15 days"
        );
        assert_eq!(
            show(&mul_d_interval(&[Datum::Float8(2.0), iv("1 day")]).unwrap()),
            "2 days"
        );
        assert_eq!(
            show(&interval_div(&[iv("1 day"), Datum::Float8(2.0)]).unwrap()),
            "12:00:00"
        );
        let e = interval_div(&[iv("1 day"), Datum::Float8(0.0)]).unwrap_err();
        assert_eq!(code(&e), "22012");
        let e = interval_mul(&[iv("1 day"), Datum::Float8(f64::NAN)]).unwrap_err();
        assert_eq!(code(&e), "22008");
        assert_eq!(e.message, "interval out of range");
        let e = interval_pl(&[iv("infinity"), iv("-infinity")]).unwrap_err();
        assert_eq!(code(&e), "22008");
        let big = Datum::Interval(Interval {
            months: i32::MAX - 1,
            days: 0,
            micros: 0,
        });
        assert_eq!(
            code(&interval_pl(&[big.clone(), big]).unwrap_err()),
            "22008"
        );
    }

    #[test]
    fn justify_and_finite() {
        assert_eq!(
            show(&interval_justify_days(&[iv("35 days")]).unwrap()),
            "1 mon 5 days"
        );
        assert_eq!(
            show(&interval_justify_hours(&[iv("50 hours")]).unwrap()),
            "2 days 02:00:00"
        );
        assert_eq!(
            show(&interval_justify_interval(&[iv("1 mon -1 hour")]).unwrap()),
            "29 days 23:00:00"
        );
        assert_eq!(interval_finite(&[iv("1 day")]).unwrap(), Datum::Bool(true));
        assert_eq!(
            interval_finite(&[iv("-infinity")]).unwrap(),
            Datum::Bool(false)
        );
    }

    #[test]
    fn scale_cast() {
        let year = yuzhu_datetime::interval_typmod(None, yuzhu_datetime::interval_range::YEAR);
        let r = interval_scale(&[iv("1 year 2 months"), Datum::Int4(year)]).unwrap();
        assert_eq!(show(&r), "1 year");
        let ds = yuzhu_datetime::interval_typmod(
            Some(1),
            yuzhu_datetime::interval_range::DAY
                | yuzhu_datetime::interval_range::HOUR
                | yuzhu_datetime::interval_range::MINUTE
                | yuzhu_datetime::interval_range::SECOND,
        );
        let r = interval_scale(&[iv("1 day 04:05:06.789"), Datum::Int4(ds)]).unwrap();
        assert_eq!(show(&r), "1 day 04:05:06.8");
        let r = interval_scale(&[iv("1 day"), Datum::Int4(-1)]).unwrap();
        assert_eq!(show(&r), "1 day");
    }

    #[test]
    fn datetime_arithmetic() {
        assert_eq!(
            super::super::datetime::timestamp_out(
                &timestamp_pl_interval(&[ts("2024-01-31 10:00"), iv("1 month")]).unwrap(),
                &TypeEnv {
                    datetime: Some(DateTimeEnv::new(
                        &TimeZone::utc(),
                        &ZoneDb::without_tzdata()
                    )),
                    ..TypeEnv::default()
                }
            )
            .unwrap(),
            "2024-02-29 10:00:00"
        );
        let d = timestamp_mi(&[ts("2024-03-01"), ts("2023-03-01")]).unwrap();
        assert_eq!(show(&d), "366 days");
        let d = timestamp_mi(&[ts("2024-01-01 12:00"), ts("2024-01-01 00:00:01")]).unwrap();
        assert_eq!(show(&d), "11:59:59");
        let r = date_pl_interval(&[Datum::Date(Date(0)), iv("1 day 02:00")]).unwrap();
        assert_eq!(r, ts("2000-01-02 02:00"));
        let r = date_mi_interval(&[Datum::Date(Date(0)), iv("1 day")]).unwrap();
        assert_eq!(r, ts("1999-12-31"));
        let r = interval_pl_date(&[iv("1 mon"), Datum::Date(Date(0))]).unwrap();
        assert_eq!(r, ts("2000-02-01"));
        let r = interval_pl_timestamp(&[iv("1 hour"), ts("2000-01-01")]).unwrap();
        assert_eq!(r, ts("2000-01-01 01:00"));
        let r = timestamp_mi_interval(&[ts("2000-01-01"), iv("1 hour")]).unwrap();
        assert_eq!(r, ts("1999-12-31 23:00"));
        let e = timestamp_pl_interval(&[ts("infinity"), iv("-infinity")]).unwrap_err();
        assert_eq!(code(&e), "22008");
    }

    #[test]
    fn timestamptz_arithmetic_uses_session_zone() {
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        with_env(&tz, &zones, |env| {
            let t = super::super::datetime::timestamptz_in("2024-03-09 12:00+00", -1, env).unwrap();
            let r = timestamptz_pl_interval(&[t.clone(), iv("1 day")], env).unwrap();
            assert_eq!(
                super::super::datetime::timestamptz_out(&r, env).unwrap(),
                "2024-03-10 12:00:00+00"
            );
            let r = timestamptz_mi_interval(&[t.clone(), iv("24:00:00")], env).unwrap();
            assert_eq!(
                super::super::datetime::timestamptz_out(&r, env).unwrap(),
                "2024-03-08 12:00:00+00"
            );
            let r = interval_pl_timestamptz(&[iv("1 mon"), t.clone()], env).unwrap();
            assert_eq!(
                super::super::datetime::timestamptz_out(&r, env).unwrap(),
                "2024-04-09 12:00:00+00"
            );
            let u = super::super::datetime::timestamptz_in("2024-03-08 12:00+00", -1, env).unwrap();
            let d = timestamptz_mi(&[t, u]).unwrap();
            assert_eq!(interval_out(&d, env).unwrap(), "1 day");
        });
        // TypeEnv に日時の環境が無ければ内部エラー
        let e = timestamptz_pl_interval(
            &[Datum::TimestampTz(TimestampTz(0)), iv("1 day")],
            &TypeEnv::default(),
        )
        .unwrap_err();
        assert_eq!(code(&e), "XX000");
    }

    #[test]
    fn aggregates() {
        let mut s = IntervalSum::new();
        assert_eq!(s.finish(), Datum::Null);
        let mut a = IntervalAvg::new();
        assert_eq!(a.finish().unwrap(), Datum::Null);
        for v in ["1 day", "25 hours", "1 mon"] {
            s.accumulate(&iv(v)).unwrap();
            a.accumulate(&iv(v)).unwrap();
        }
        assert_eq!(show(&s.finish()), "1 mon 1 day 25:00:00");
        assert_eq!(show(&a.finish().unwrap()), "10 days 16:20:00");
        let mut s = IntervalSum::new();
        s.accumulate(&iv("infinity")).unwrap();
        assert_eq!(code(&s.accumulate(&iv("-infinity")).unwrap_err()), "22008");
    }
}
