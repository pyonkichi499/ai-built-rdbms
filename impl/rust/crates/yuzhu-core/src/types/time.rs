//! `time`（without time zone）の統合（TD-4）。`m5/06-types-datetime.md` §3.1、§6.1.1、§6.3、§6.5。
//!
//! 値の本体は `yuzhu_datetime::Time`（0 時からのマイクロ秒。`24:00:00` を含む）で、`Datum::Time(i64)` に載る。
//! 日時の意味論（丸め・巻き戻し・範囲検査・エラーの文言）はクレートにあり、ここは呼ぶだけ。
//! 関数の形は `FnKind` に合わせる（`Pure`: `fn(&[Datum]) -> Result<Datum>`、
//! `Env`: `fn(&[Datum], &TypeEnv) -> Result<Datum>`）。引数は NULL でない（strict）。
//! `timetz` は作らない（型名の解決が `0A000`）。

use std::cmp::Ordering;

use yuzhu_datetime::{Date, Interval, NumericValue, Time, Timestamp, TimestampTz};

use super::datetime::{dt_env, dt_err};
use super::{Datum, TypeEnv};
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// ディスク形式（8 バイト、LE の i64。0 時からのマイクロ秒）
// ---------------------------------------------------------------------------

/// ディスク形式。`'04:05:06.789'` は `88 ca 97 6c 03 00 00 00`。
pub fn encode(t: Time) -> [u8; 8] {
    t.0.to_le_bytes()
}

/// [`encode`] の逆。範囲（`0 ..= 24:00:00`）は検査しない（壊れたページは別の層が見つける）。
pub fn decode(b: &[u8; 8]) -> Time {
    Time(i64::from_le_bytes(*b))
}

/// スライスから復元する。長さが 8 でなければ `XX001`。
pub fn decode_slice(b: &[u8]) -> Result<Time> {
    let arr: &[u8; 8] = b
        .try_into()
        .map_err(|_| Error::corrupted(format!("invalid length {} for a stored time", b.len())))?;
    Ok(decode(arr))
}

// ---------------------------------------------------------------------------
// 入出力
// ---------------------------------------------------------------------------

/// `time_in`。`typmod` は `time(p)` の p（`-1` = 指定なし）。DateStyle の日付順は日付つきの入力で効く。
pub fn time_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum> {
    Time::parse(s, typmod, dt_env(env)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `time_out`（`DateStyle` によらない）。
pub fn time_out(d: &Datum) -> Result<String> {
    Ok(time_of(d)?.format())
}

// ---------------------------------------------------------------------------
// 引数の取り出し
// ---------------------------------------------------------------------------

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
    args.get(i).ok_or_else(|| bad_arg("time function"))
}

fn time_of(d: &Datum) -> Result<Time> {
    match d {
        Datum::Time(v) => Ok(Time(*v)),
        _ => Err(bad_arg("time function")),
    }
}

fn time_arg(args: &[Datum], i: usize) -> Result<Time> {
    time_of(arg(args, i)?)
}

fn interval_arg(args: &[Datum], i: usize) -> Result<&Interval> {
    match arg(args, i)? {
        Datum::Interval(v) => Ok(v),
        _ => Err(bad_arg("time function (interval)")),
    }
}

fn date_arg(args: &[Datum], i: usize) -> Result<Date> {
    match arg(args, i)? {
        Datum::Date(v) => Ok(*v),
        _ => Err(bad_arg("time function (date)")),
    }
}

fn timestamp_arg(args: &[Datum], i: usize) -> Result<Timestamp> {
    match arg(args, i)? {
        Datum::Timestamp(v) => Ok(*v),
        _ => Err(bad_arg("time function (timestamp)")),
    }
}

fn timestamptz_arg(args: &[Datum], i: usize) -> Result<TimestampTz> {
    match arg(args, i)? {
        Datum::TimestampTz(v) => Ok(*v),
        _ => Err(bad_arg("time function (timestamptz)")),
    }
}

fn text_arg(args: &[Datum], i: usize) -> Result<&str> {
    match arg(args, i)? {
        Datum::Text(s) => Ok(s),
        _ => Err(bad_arg("time function (text)")),
    }
}

fn typmod_arg(args: &[Datum], i: usize) -> Result<i32> {
    arg(args, i)?
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| bad_arg("time function (integer)"))
}

// ---------------------------------------------------------------------------
// 比較（整数）
// ---------------------------------------------------------------------------

fn cmp_args(args: &[Datum]) -> Result<Ordering> {
    Ok(time_arg(args, 0)?.cmp(&time_arg(args, 1)?))
}

/// `time_eq`（1145）。
pub fn time_eq(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Equal))
}

/// `time_ne`（1106）。
pub fn time_ne(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Equal))
}

/// `time_lt`（1102）。
pub fn time_lt(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Less))
}

/// `time_le`（1103）。
pub fn time_le(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Greater))
}

/// `time_gt`（1104）。
pub fn time_gt(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? == Ordering::Greater))
}

/// `time_ge`（1105）。
pub fn time_ge(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Bool(cmp_args(args)? != Ordering::Less))
}

/// `time_cmp`（1107）。
pub fn time_cmp(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Int4(match cmp_args(args)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

/// `time_larger`（1377。`max(time)` の遷移関数）。
pub fn time_larger(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Time(time_arg(args, 0)?.max(time_arg(args, 1)?).0))
}

/// `time_smaller`（1378。`min(time)` の遷移関数）。
pub fn time_smaller(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Time(time_arg(args, 0)?.min(time_arg(args, 1)?).0))
}

// ---------------------------------------------------------------------------
// 演算子
// ---------------------------------------------------------------------------

/// `time + interval`（1747。months と days は無視し、24 時間で巻き戻す）。
pub fn time_pl_interval(args: &[Datum]) -> Result<Datum> {
    time_arg(args, 0)?
        .add_interval(interval_arg(args, 1)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `time - interval`（1748）。
pub fn time_mi_interval(args: &[Datum]) -> Result<Datum> {
    time_arg(args, 0)?
        .sub_interval(interval_arg(args, 1)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `interval + time`（1848）。
pub fn interval_pl_time(args: &[Datum]) -> Result<Datum> {
    time_arg(args, 1)?
        .add_interval(interval_arg(args, 0)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `time - time`（1690。巻き戻さない）。
pub fn time_mi_time(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Interval(
        time_arg(args, 0)?.sub_time(time_arg(args, 1)?),
    ))
}

/// `date + time`（`datetime_pl`、1272）。
pub fn datetime_pl(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .add_time(time_arg(args, 1)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `time + date`（`timedate_pl`、1296）。
pub fn timedate_pl(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 1)?
        .add_time(time_arg(args, 0)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

// ---------------------------------------------------------------------------
// キャスト
// ---------------------------------------------------------------------------

/// `time(time, int4)`（1968。型修飾子の適用。`time → time` の暗黙キャスト）。
pub fn time_scale(args: &[Datum]) -> Result<Datum> {
    time_arg(args, 0)?
        .with_typmod(typmod_arg(args, 1)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `interval(time)`（1370。`time → interval`、暗黙）。
pub fn time_to_interval(args: &[Datum]) -> Result<Datum> {
    Ok(Datum::Interval(time_arg(args, 0)?.to_interval()))
}

/// `time(interval)`（1419。`interval → time`、代入）。無限は 22008。
pub fn interval_to_time(args: &[Datum]) -> Result<Datum> {
    Time::from_interval(interval_arg(args, 0)?)
        .map(|t| Datum::Time(t.0))
        .map_err(dt_err)
}

/// `time(timestamp)`（1316。代入）。無限は NULL。
pub fn timestamp_to_time(args: &[Datum]) -> Result<Datum> {
    Ok(timestamp_arg(args, 0)?
        .to_time()
        .map_or(Datum::Null, |t| Datum::Time(t.0)))
}

/// `time(timestamptz)`（2019。代入）。セッションのゾーンの壁時計。無限は NULL。
pub fn timestamptz_to_time(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    let tz = dt_env(env)?.time_zone;
    let t = timestamptz_arg(args, 0)?.to_time(tz).map_err(dt_err)?;
    Ok(t.map_or(Datum::Null, |t| Datum::Time(t.0)))
}

/// `timestamp(date, time)`（2025）。
pub fn timestamp_date_time(args: &[Datum]) -> Result<Datum> {
    datetime_pl(args)
}

/// `timestamptz(date, time)`（1176）。セッションのゾーンで解釈する。
pub fn timestamptz_date_time(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    let tz = dt_env(env)?.time_zone;
    date_arg(args, 0)?
        .add_time(time_arg(args, 1)?)
        .and_then(|ts| ts.to_timestamptz(tz))
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

// ---------------------------------------------------------------------------
// date_part / extract
// ---------------------------------------------------------------------------

/// `date_part(text, time)`（1385）。
pub fn date_part_time(args: &[Datum]) -> Result<Datum> {
    let v =
        yuzhu_datetime::date_part_time(text_arg(args, 0)?, time_arg(args, 1)?).map_err(dt_err)?;
    Ok(v.map_or(Datum::Null, Datum::Float8))
}

/// `extract(text, time)`（6200）。結果は numeric。
pub fn extract_time(args: &[Datum]) -> Result<Datum> {
    let v: Option<NumericValue> =
        yuzhu_datetime::extract_time(text_arg(args, 0)?, time_arg(args, 1)?).map_err(dt_err)?;
    match v {
        None => Ok(Datum::Null),
        Some(nv) => yuzhu_numeric::Numeric::parse(&nv.to_string())
            .map(Datum::Numeric)
            .map_err(Error::from),
    }
}

#[cfg(test)]
mod tests {
    use yuzhu_datetime::{DateTimeEnv, TimeZone, ZoneDb};

    use super::*;

    fn t(s: &str) -> Datum {
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        Datum::Time(
            Time::parse(s, -1, &DateTimeEnv::new(&tz, &zones))
                .unwrap()
                .0,
        )
    }

    fn iv(s: &str) -> Datum {
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        Datum::Interval(Interval::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap())
    }

    fn ts(s: &str) -> Datum {
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        Datum::Timestamp(Timestamp::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap())
    }

    fn txt(s: &str) -> Datum {
        Datum::Text(s.to_owned())
    }

    fn out(d: &Datum) -> String {
        time_out(d).unwrap()
    }

    #[test]
    fn disk_format_matches_the_spec_examples() {
        let Datum::Time(v) = t("04:05:06.789") else {
            panic!()
        };
        assert_eq!(v, 14_706_789_000);
        assert_eq!(encode(Time(v)), [0x88, 0xca, 0x97, 0x6c, 0x03, 0, 0, 0]);
        assert_eq!(
            encode(Time(86_400_000_000)),
            [0x00, 0x60, 0xd7, 0x1d, 0x14, 0, 0, 0]
        );
        assert_eq!(decode(&encode(Time(v))), Time(v));
        assert_eq!(
            decode_slice(&[1, 2, 3]).unwrap_err().sqlstate.code(),
            "XX001"
        );
    }

    #[test]
    fn comparison_and_extremes() {
        let (a, b) = (t("01:00"), t("23:00"));
        assert_eq!(time_lt(&[a.clone(), b.clone()]).unwrap(), Datum::Bool(true));
        assert_eq!(
            time_ge(&[a.clone(), b.clone()]).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(time_eq(&[a.clone(), a.clone()]).unwrap(), Datum::Bool(true));
        assert_eq!(time_ne(&[a.clone(), b.clone()]).unwrap(), Datum::Bool(true));
        assert_eq!(
            time_le(&[b.clone(), a.clone()]).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(time_gt(&[b.clone(), a.clone()]).unwrap(), Datum::Bool(true));
        assert_eq!(time_cmp(&[a.clone(), b.clone()]).unwrap(), Datum::Int4(-1));
        assert_eq!(time_cmp(&[b.clone(), a.clone()]).unwrap(), Datum::Int4(1));
        assert_eq!(time_larger(&[a.clone(), b.clone()]).unwrap(), b);
        assert_eq!(time_smaller(&[a.clone(), b]).unwrap(), a);
    }

    #[test]
    fn arithmetic_wraps_like_postgres() {
        let r = |d: Datum| out(&d);
        // months と days は無視する
        assert_eq!(
            r(time_pl_interval(&[t("01:00"), iv("1 month 2 days 3 hours")]).unwrap()),
            "04:00:00"
        );
        assert_eq!(
            r(time_pl_interval(&[t("23:00"), iv("2 hours")]).unwrap()),
            "01:00:00"
        );
        assert_eq!(
            r(time_mi_interval(&[t("01:00"), iv("2 hours 30 min")]).unwrap()),
            "22:30:00"
        );
        assert_eq!(
            r(time_pl_interval(&[t("24:00:00"), iv("1 sec")]).unwrap()),
            "00:00:01"
        );
        assert_eq!(
            r(interval_pl_time(&[iv("2 hours"), t("23:00")]).unwrap()),
            "01:00:00"
        );
        // time - time は巻き戻さない
        let Datum::Interval(d) = time_mi_time(&[t("01:00"), t("23:00")]).unwrap() else {
            panic!()
        };
        assert_eq!(d.micros, -22 * 3_600_000_000);
        let Datum::Interval(d) = time_mi_time(&[t("24:00:00"), t("00:00")]).unwrap() else {
            panic!()
        };
        assert_eq!((d.months, d.days, d.micros), (0, 0, 86_400_000_000));
    }

    #[test]
    fn date_plus_time() {
        let d = Datum::Date(Date::from_ymd(2024, 1, 2).unwrap());
        let want = ts("2024-01-02 03:04:00");
        assert_eq!(datetime_pl(&[d.clone(), t("03:04")]).unwrap(), want.clone());
        assert_eq!(timedate_pl(&[t("03:04"), d.clone()]).unwrap(), want.clone());
        assert_eq!(timestamp_date_time(&[d, t("03:04")]).unwrap(), want.clone());
    }

    #[test]
    fn casts() {
        let r = |d: Datum| out(&d);
        let Datum::Interval(i) = time_to_interval(&[t("04:05:06")]).unwrap() else {
            panic!()
        };
        assert_eq!(i.micros, 14_706_000_000);
        assert_eq!(
            r(interval_to_time(&[iv("1 day 2 hours")]).unwrap()),
            "02:00:00"
        );
        assert_eq!(r(interval_to_time(&[iv("-1 hour")]).unwrap()), "23:00:00");
        assert_eq!(r(interval_to_time(&[iv("25:30:00")]).unwrap()), "01:30:00");
        assert_eq!(
            interval_to_time(&[iv("infinity")])
                .unwrap_err()
                .sqlstate
                .code(),
            "22008"
        );
        assert_eq!(
            r(time_scale(&[t("23:59:59.9999995"), Datum::Int4(0)]).unwrap()),
            "24:00:00"
        );
        assert_eq!(
            r(time_scale(&[t("04:05:06.789"), Datum::Int4(1)]).unwrap()),
            "04:05:06.8"
        );
        assert_eq!(
            r(time_scale(&[t("04:05:06.789"), Datum::Int4(-1)]).unwrap()),
            "04:05:06.789"
        );
        assert_eq!(
            r(timestamp_to_time(&[ts("2024-03-09 15:04:05")]).unwrap()),
            "15:04:05"
        );
        assert_eq!(
            timestamp_to_time(&[Datum::Timestamp(Timestamp(i64::MAX))]).unwrap(),
            Datum::Null
        );
    }

    #[test]
    fn extract_and_date_part() {
        let v = t("04:05:06.789");
        let num = |u: &str| match extract_time(&[txt(u), v.clone()]).unwrap() {
            Datum::Numeric(n) => n.to_string(),
            d => panic!("{d:?}"),
        };
        assert_eq!(num("hour"), "4");
        assert_eq!(num("second"), "6.789000");
        assert_eq!(num("epoch"), "14706.789000");
        assert_eq!(
            date_part_time(&[txt("minute"), v.clone()]).unwrap(),
            Datum::Float8(5.0)
        );
        let e = extract_time(&[txt("day"), v.clone()]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "0A000");
        let e = date_part_time(&[txt("foo"), v]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
    }

    #[test]
    fn rejects_wrong_arguments() {
        assert!(time_eq(&[Datum::Int4(1), Datum::Int4(1)]).is_err());
        assert!(time_eq(&[]).is_err());
    }
}
