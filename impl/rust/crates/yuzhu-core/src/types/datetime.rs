//! `date` / `timestamp` / `timestamptz` と `yuzhu-datetime` の橋渡し。担当 T2。
//! `m4/09-types-functions.md` §3.4〜§3.8、§5。
//!
//! 入出力は `TypeEnv.datetime`（DateStyle・TimeZone・`now`）に従う。キャストと関数の本体は
//! ここに置き、`catalog/builtin.rs` の `CASTS` / `FUNCTIONS` が参照する。

use yuzhu_datetime::{Date, DateTimeEnv, DateTimeError, Timestamp, TimestampTz};

use super::{Datum, TypeEnv, ops};
use crate::error::{Error, Result, SqlState};

/// `DateTimeError` → `Error`（SQLSTATE・DETAIL・HINT をそのまま写す）。
pub(crate) fn dt_err(e: DateTimeError) -> Error {
    let mut err = Error::new(SqlState(e.sqlstate), e.message);
    if let Some(d) = e.detail {
        err = err.with_detail(d);
    }
    if let Some(h) = e.hint {
        err = err.with_hint(h);
    }
    err
}

/// `env.datetime` が `None` なら `Error::internal`。
pub(crate) fn dt_env<'e, 'a>(env: &'e TypeEnv<'a>) -> Result<&'e DateTimeEnv<'a>> {
    env.datetime
        .as_ref()
        .ok_or_else(|| Error::internal("date/time input requires a DateTimeEnv"))
}

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

fn date_of(d: &Datum) -> Result<Date> {
    match d {
        Datum::Date(v) => Ok(*v),
        _ => Err(bad_arg("date function")),
    }
}

fn timestamp_of(d: &Datum) -> Result<Timestamp> {
    match d {
        Datum::Timestamp(v) => Ok(*v),
        _ => Err(bad_arg("timestamp function")),
    }
}

fn timestamptz_of(d: &Datum) -> Result<TimestampTz> {
    match d {
        Datum::TimestampTz(v) => Ok(*v),
        _ => Err(bad_arg("timestamptz function")),
    }
}

// ---------------------------------------------------------------------------
// 入出力
// ---------------------------------------------------------------------------

/// `date_in`。
pub fn date_in(s: &str, env: &TypeEnv<'_>) -> Result<Datum> {
    Date::parse(s, dt_env(env)?)
        .map(Datum::Date)
        .map_err(dt_err)
}

/// `timestamp_in`（含まれるタイムゾーンは黙って無視される）。
pub fn timestamp_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum> {
    Timestamp::parse(s, typmod, dt_env(env)?)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamptz_in`（タイムゾーンがなければセッションの `TimeZone` で解釈する）。
pub fn timestamptz_in(s: &str, typmod: i32, env: &TypeEnv<'_>) -> Result<Datum> {
    TimestampTz::parse(s, typmod, dt_env(env)?)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `date_out`。
pub fn date_out(d: &Datum, env: &TypeEnv<'_>) -> Result<String> {
    Ok(date_of(d)?.format(dt_env(env)?))
}

/// `timestamp_out`。
pub fn timestamp_out(d: &Datum, env: &TypeEnv<'_>) -> Result<String> {
    timestamp_of(d)?.format(dt_env(env)?).map_err(dt_err)
}

/// `timestamptz_out`。
pub fn timestamptz_out(d: &Datum, env: &TypeEnv<'_>) -> Result<String> {
    timestamptz_of(d)?.format(dt_env(env)?).map_err(dt_err)
}

/// 日時の変種を文字列にする。日時でなければ `None`。
pub fn output_any(d: &Datum, env: &TypeEnv<'_>) -> Option<Result<String>> {
    Some(match d {
        Datum::Date(_) => date_out(d, env),
        Datum::Timestamp(_) => timestamp_out(d, env),
        Datum::TimestampTz(_) => timestamptz_out(d, env),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// ディスク上の形式（`m4/09` §3.5。varlena ではなく固定長で、`tuple.rs` が整列する）
// ---------------------------------------------------------------------------

fn fixed<const N: usize>(b: &[u8], what: &str) -> Result<[u8; N]> {
    b.try_into()
        .map_err(|_| Error::corrupted(format!("invalid length {} for a stored {what}", b.len())))
}

/// `date`: `i32` LE（2000-01-01 からの日数）。
pub fn encode_date(d: Date) -> [u8; 4] {
    d.0.to_le_bytes()
}

/// `date` を復元する。長さが 4 でなければ `XX001`。
pub fn decode_date(b: &[u8]) -> Result<Date> {
    Ok(Date(i32::from_le_bytes(fixed(b, "date")?)))
}

/// `timestamp`: `i64` LE（2000-01-01 00:00:00 からのマイクロ秒）。
pub fn encode_timestamp(t: Timestamp) -> [u8; 8] {
    t.0.to_le_bytes()
}

/// `timestamp` を復元する。長さが 8 でなければ `XX001`。
pub fn decode_timestamp(b: &[u8]) -> Result<Timestamp> {
    Ok(Timestamp(i64::from_le_bytes(fixed(b, "timestamp")?)))
}

/// `timestamptz`: `i64` LE（UTC）。
pub fn encode_timestamptz(t: TimestampTz) -> [u8; 8] {
    t.0.to_le_bytes()
}

/// `timestamptz` を復元する。長さが 8 でなければ `XX001`。
pub fn decode_timestamptz(b: &[u8]) -> Result<TimestampTz> {
    Ok(TimestampTz(i64::from_le_bytes(fixed(b, "timestamptz")?)))
}

// ---------------------------------------------------------------------------
// キャストと関数の共有本体（引数は非 NULL。D-9-4）
// ---------------------------------------------------------------------------

/// `date` → `timestamp`（純粋。`env` は使わない）。
pub fn date_to_timestamp(d: &Datum, _env: Option<&DateTimeEnv<'_>>) -> Result<Datum> {
    date_of(d)?
        .to_timestamp()
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamp` → `date`（純粋）。
pub fn timestamp_to_date(d: &Datum) -> Result<Datum> {
    timestamp_of(d)?.to_date().map(Datum::Date).map_err(dt_err)
}

/// `date` → `timestamptz`（セッションの `TimeZone` の 0 時）。
pub fn date_to_timestamptz(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum> {
    date_of(d)?
        .to_timestamptz(env.time_zone)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `timestamp` → `timestamptz`。
pub fn timestamp_to_timestamptz(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum> {
    timestamp_of(d)?
        .to_timestamptz(env.time_zone)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `timestamptz` → `timestamp`（セッションの `TimeZone` の現地時刻）。
pub fn timestamptz_to_timestamp(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum> {
    timestamptz_of(d)?
        .to_timestamp(env.time_zone)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

/// `timestamptz` → `date`（セッションの `TimeZone` の現地日付）。
pub fn timestamptz_to_date(d: &Datum, env: &DateTimeEnv<'_>) -> Result<Datum> {
    timestamptz_of(d)?
        .to_date(env.time_zone)
        .map(Datum::Date)
        .map_err(dt_err)
}

fn first(args: &[Datum]) -> Result<&Datum> {
    args.first().ok_or_else(|| bad_arg("date/time cast"))
}

/// `BuiltinFn` 形式: `date → timestamp`（`CASTS` の 2024 と `timestamp(date)` 関数）。
pub fn cast_date_to_timestamp(args: &[Datum]) -> Result<Datum> {
    date_to_timestamp(first(args)?, None)
}

/// `BuiltinFn` 形式: `timestamp → date`（`CASTS` の 2029 と `date(timestamp)` 関数）。
pub fn cast_timestamp_to_date(args: &[Datum]) -> Result<Datum> {
    timestamp_to_date(first(args)?)
}

/// `CastMethod::Env` 用: `date → timestamptz`（1174）。
pub fn cast_date_to_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    date_to_timestamptz(first(args)?, dt_env(env)?)
}

/// `CastMethod::Env` 用: `timestamp → timestamptz`（2028）。
pub fn cast_timestamp_to_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamp_to_timestamptz(first(args)?, dt_env(env)?)
}

/// `CastMethod::Env` 用: `timestamptz → timestamp`（2027）。
pub fn cast_timestamptz_to_timestamp(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamptz_to_timestamp(first(args)?, dt_env(env)?)
}

/// `CastMethod::Env` 用: `timestamptz → date`（1178）。
pub fn cast_timestamptz_to_date(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
    timestamptz_to_date(first(args)?, dt_env(env)?)
}

// ---------------------------------------------------------------------------
// 実行時の関数（`FnKind::Runtime`）
// ---------------------------------------------------------------------------

// TODO(T2): `RuntimeInfo` に `transaction_timestamp` などが入るまでコンパイルしない（issues 参照）。
#[cfg(any())]
mod runtime_fns {
    use super::*;
    use crate::executor::RuntimeInfo;

    /// `now()` / `transaction_timestamp()`: トランザクション開始時刻。
    pub fn now(_args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        Ok(Datum::TimestampTz(TimestampTz(rt.transaction_timestamp())))
    }

    /// `statement_timestamp()`。
    pub fn statement_timestamp(_args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        Ok(Datum::TimestampTz(TimestampTz(rt.statement_timestamp())))
    }

    /// `clock_timestamp()`: 呼んだ時点。
    pub fn clock_timestamp(_args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        Ok(Datum::TimestampTz(TimestampTz(rt.clock_timestamp())))
    }

    /// `date(timestamptz)`（1178）。
    pub fn rt_timestamptz_to_date(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        timestamptz_to_date(first(args)?, &rt.datetime_env())
    }

    /// `timestamp(timestamptz)`（2027）。
    pub fn rt_timestamptz_to_timestamp(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        timestamptz_to_timestamp(first(args)?, &rt.datetime_env())
    }

    /// `timestamptz(date)`（1174）。
    pub fn rt_date_to_timestamptz(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        date_to_timestamptz(first(args)?, &rt.datetime_env())
    }

    /// `timestamptz(timestamp)`（2028）。
    pub fn rt_timestamp_to_timestamptz(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        timestamp_to_timestamptz(first(args)?, &rt.datetime_env())
    }
}

// ---------------------------------------------------------------------------
// SQL 値関数（`CURRENT_DATE` など。§6.4）。`session_value` が現在時刻とセッションの設定を渡す。
// ---------------------------------------------------------------------------

/// `CURRENT_DATE`: トランザクション開始時刻の現地日付。
pub fn current_date(now: i64, env: &DateTimeEnv<'_>) -> Result<Datum> {
    timestamptz_to_date(&Datum::TimestampTz(TimestampTz(now)), env)
}

/// `CURRENT_TIMESTAMP[(p)]`（`precision < 0` は指定なし）。
pub fn current_timestamp(now: i64, precision: i32) -> Result<Datum> {
    TimestampTz(now)
        .with_typmod(precision)
        .map(Datum::TimestampTz)
        .map_err(dt_err)
}

/// `LOCALTIMESTAMP[(p)]`: セッションの `TimeZone` の現地時刻。
pub fn local_timestamp(now: i64, precision: i32, env: &DateTimeEnv<'_>) -> Result<Datum> {
    let t = TimestampTz(now)
        .to_timestamp(env.time_zone)
        .map_err(dt_err)?;
    t.with_typmod(precision)
        .map(Datum::Timestamp)
        .map_err(dt_err)
}

// ---------------------------------------------------------------------------
// 演算子
// ---------------------------------------------------------------------------

fn int4_arg(args: &[Datum], i: usize) -> Result<i32> {
    args.get(i)
        .and_then(Datum::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| bad_arg("date operator"))
}

fn date_arg(args: &[Datum], i: usize) -> Result<Date> {
    args.get(i)
        .ok_or_else(|| bad_arg("date operator"))
        .and_then(date_of)
}

/// `date + int4`（1100）。
pub fn date_pli(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .add_days(int4_arg(args, 1)?)
        .map(Datum::Date)
        .map_err(dt_err)
}

/// `int4 + date`（2555）。
pub fn integer_pl_date(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 1)?
        .add_days(int4_arg(args, 0)?)
        .map(Datum::Date)
        .map_err(dt_err)
}

/// `date - int4`（1101）。
pub fn date_mii(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .sub_days(int4_arg(args, 1)?)
        .map(Datum::Date)
        .map_err(dt_err)
}

/// `date - date`（1099。日数の `int4`）。
pub fn date_mi(args: &[Datum]) -> Result<Datum> {
    date_arg(args, 0)?
        .sub_date(date_arg(args, 1)?)
        .map(Datum::Int4)
        .map_err(dt_err)
}

/// 比較演算子の本体は `ops::cmp_*`（`cmp_datum` が日時の変種を整数で比べる）。
pub use ops::{cmp_eq, cmp_ge, cmp_gt, cmp_le, cmp_lt, cmp_ne};

#[cfg(test)]
mod tests {
    use super::*;
    use yuzhu_datetime::{TimeZone, ZoneDb};

    fn with_env<R>(tz: &TimeZone, zones: &ZoneDb, f: impl FnOnce(&TypeEnv<'_>) -> R) -> R {
        let mut dt = DateTimeEnv::new(tz, zones);
        // 2024-03-10 12:00:00 UTC
        dt.now = TimestampTz(
            i64::from(Date::from_ymd(2024, 3, 10).unwrap().0) * yuzhu_datetime::MICROS_PER_DAY
                + 12 * 3_600_000_000,
        );
        let env = TypeEnv {
            datetime: Some(dt),
            ..TypeEnv::default()
        };
        f(&env)
    }

    fn utc_env<R>(f: impl FnOnce(&TypeEnv<'_>) -> R) -> R {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        with_env(&tz, &zones, f)
    }

    fn tokyo() -> Option<(TimeZone, ZoneDb)> {
        if !std::path::Path::new("/usr/share/zoneinfo/Asia/Tokyo").exists() {
            return None;
        }
        let zones = ZoneDb::system();
        let tz = zones.lookup("Asia/Tokyo")?;
        Some((tz, zones))
    }

    #[test]
    fn input_output_roundtrip() {
        utc_env(|env| {
            let d = date_in("2024-01-31", env).unwrap();
            assert_eq!(date_out(&d, env).unwrap(), "2024-01-31");
            let t = timestamp_in("2024-01-01 00:00:01.5", -1, env).unwrap();
            assert_eq!(timestamp_out(&t, env).unwrap(), "2024-01-01 00:00:01.5");
            // timestamp は含まれるタイムゾーンを黙って無視する。
            let t = timestamp_in("2024-01-01 00:00+09", -1, env).unwrap();
            assert_eq!(timestamp_out(&t, env).unwrap(), "2024-01-01 00:00:00");
            let tz = timestamptz_in("2024-01-01 09:00:00+09", -1, env).unwrap();
            assert_eq!(timestamptz_out(&tz, env).unwrap(), "2024-01-01 00:00:00+00");
            assert_eq!(
                date_out(&date_in("infinity", env).unwrap(), env).unwrap(),
                "infinity"
            );
            assert_eq!(
                date_out(&date_in("0044-03-15 BC", env).unwrap(), env).unwrap(),
                "0044-03-15 BC"
            );
            // typmod 付きの入力は丸める。
            let t = timestamp_in("2024-01-01 10:00:00.6", 0, env).unwrap();
            assert_eq!(timestamp_out(&t, env).unwrap(), "2024-01-01 10:00:01");
        });
    }

    #[test]
    fn input_errors_carry_sqlstate_and_hint() {
        utc_env(|env| {
            let e = timestamp_in("garbage", -1, env).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22007");
            assert_eq!(
                e.message,
                "invalid input syntax for type timestamp: \"garbage\""
            );
            let e = date_in("2024-13-01", env).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22008");
            assert_eq!(
                e.message,
                "date/time field value out of range: \"2024-13-01\""
            );
            assert_eq!(
                e.hint.as_deref(),
                Some("Perhaps you need a different \"datestyle\" setting.")
            );
            let e = timestamp_in("294277-01-01", -1, env).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22008");
            assert_eq!(e.message, "timestamp out of range: \"294277-01-01\"");
            let e = timestamptz_in("2024-01-01 00:00+16", -1, env).unwrap_err();
            assert_eq!(e.sqlstate.code(), "22009");
        });
    }

    #[test]
    fn missing_env_is_internal_error() {
        let env = TypeEnv::default();
        let e = date_in("2024-01-01", &env).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX000");
        let e = date_out(&Datum::Date(Date(0)), &env).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX000");
    }

    #[test]
    fn now_keywords_use_transaction_time() {
        utc_env(|env| {
            let t = timestamp_in("now", -1, env).unwrap();
            assert_eq!(timestamp_out(&t, env).unwrap(), "2024-03-10 12:00:00");
            let d = date_in("tomorrow", env).unwrap();
            assert_eq!(date_out(&d, env).unwrap(), "2024-03-11");
            let d = date_in("yesterday", env).unwrap();
            assert_eq!(date_out(&d, env).unwrap(), "2024-03-09");
        });
    }

    #[test]
    fn pure_casts() {
        let d = Datum::Date(Date::from_ymd(2024, 1, 1).unwrap());
        let t = cast_date_to_timestamp(std::slice::from_ref(&d)).unwrap();
        assert_eq!(
            t,
            Datum::Timestamp(Timestamp(8766 * yuzhu_datetime::MICROS_PER_DAY))
        );
        assert_eq!(cast_timestamp_to_date(&[t]).unwrap(), d);
        let inf = Datum::Date(Date::INFINITY);
        assert_eq!(
            cast_date_to_timestamp(&[inf]).unwrap(),
            Datum::Timestamp(Timestamp::INFINITY)
        );
        let max = Datum::Date(Date(i32::MAX - 1));
        let e = cast_date_to_timestamp(&[max]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22008");
        assert_eq!(e.message, "date out of range for timestamp");
    }

    #[test]
    fn env_casts_follow_the_time_zone() {
        // UTC
        utc_env(|env| {
            let d = Datum::Date(Date::from_ymd(2024, 1, 1).unwrap());
            let tz = cast_date_to_timestamptz(std::slice::from_ref(&d), env).unwrap();
            assert_eq!(timestamptz_out(&tz, env).unwrap(), "2024-01-01 00:00:00+00");
            let back = cast_timestamptz_to_date(std::slice::from_ref(&tz), env).unwrap();
            assert_eq!(back, d);
            let ts = cast_timestamptz_to_timestamp(&[tz], env).unwrap();
            assert_eq!(timestamp_out(&ts, env).unwrap(), "2024-01-01 00:00:00");
            let tz2 = cast_timestamp_to_timestamptz(&[ts], env).unwrap();
            assert_eq!(
                timestamptz_out(&tz2, env).unwrap(),
                "2024-01-01 00:00:00+00"
            );
        });
        // Asia/Tokyo（tzdata がなければ飛ばす）
        if let Some((tz, zones)) = tokyo() {
            with_env(&tz, &zones, |env| {
                let d = Datum::Date(Date::from_ymd(2024, 1, 1).unwrap());
                let v = cast_date_to_timestamptz(&[d], env).unwrap();
                assert_eq!(timestamptz_out(&v, env).unwrap(), "2024-01-01 00:00:00+09");
                // 2023-12-31 16:00 UTC は東京で 2024-01-01 01:00
                let v = timestamptz_in("2023-12-31 16:00:00+00", -1, env).unwrap();
                let date = cast_timestamptz_to_date(std::slice::from_ref(&v), env).unwrap();
                assert_eq!(date_out(&date, env).unwrap(), "2024-01-01");
                let ts = cast_timestamptz_to_timestamp(&[v], env).unwrap();
                assert_eq!(timestamp_out(&ts, env).unwrap(), "2024-01-01 01:00:00");
            });
        }
    }

    #[test]
    fn sql_value_functions() {
        utc_env(|env| {
            let dt = env.datetime.as_ref().unwrap();
            let now = dt.now.0;
            let d = current_date(now, dt).unwrap();
            assert_eq!(date_out(&d, env).unwrap(), "2024-03-10");
            let t = current_timestamp(now + 123_456, 3).unwrap();
            assert_eq!(
                timestamptz_out(&t, env).unwrap(),
                "2024-03-10 12:00:00.123+00"
            );
            let t = current_timestamp(now + 123_456, -1).unwrap();
            assert_eq!(
                timestamptz_out(&t, env).unwrap(),
                "2024-03-10 12:00:00.123456+00"
            );
            let t = local_timestamp(now + 600_000, 0, dt).unwrap();
            assert_eq!(timestamp_out(&t, env).unwrap(), "2024-03-10 12:00:01");
        });
        if let Some((tz, zones)) = tokyo() {
            with_env(&tz, &zones, |env| {
                let dt = env.datetime.as_ref().unwrap();
                // 12:00 UTC は東京で 21:00、日付は同じ。15 時間後は翌日。
                let d = current_date(dt.now.0 + 15 * 3_600_000_000, dt).unwrap();
                assert_eq!(date_out(&d, env).unwrap(), "2024-03-11");
                let t = local_timestamp(dt.now.0, -1, dt).unwrap();
                assert_eq!(timestamp_out(&t, env).unwrap(), "2024-03-10 21:00:00");
            });
        }
    }

    #[test]
    fn date_operators() {
        let d = |y, m, dd| Datum::Date(Date::from_ymd(y, m, dd).unwrap());
        assert_eq!(
            date_pli(&[d(2024, 1, 31), Datum::Int4(1)]).unwrap(),
            d(2024, 2, 1)
        );
        assert_eq!(
            integer_pl_date(&[Datum::Int4(1), d(2024, 2, 28)]).unwrap(),
            d(2024, 2, 29)
        );
        assert_eq!(
            date_mii(&[d(2024, 3, 1), Datum::Int4(1)]).unwrap(),
            d(2024, 2, 29)
        );
        assert_eq!(
            date_mi(&[d(2024, 3, 1), d(2024, 1, 1)]).unwrap(),
            Datum::Int4(60)
        );
        let e = date_mi(&[Datum::Date(Date::INFINITY), d(2024, 1, 1)]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22008");
        assert_eq!(e.message, "cannot subtract infinite dates");
        let e = date_pli(&[Datum::Date(Date(i32::MAX - 1)), Datum::Int4(5)]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22008");
        assert_eq!(e.message, "date out of range");
        // infinity はそのまま。
        assert_eq!(
            date_pli(&[Datum::Date(Date::INFINITY), Datum::Int4(1)]).unwrap(),
            Datum::Date(Date::INFINITY)
        );
        assert_eq!(
            cmp_lt(&[d(2024, 1, 1), Datum::Date(Date::INFINITY)]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            cmp_eq(&[
                Datum::Timestamp(Timestamp(5)),
                Datum::Timestamp(Timestamp(5))
            ])
            .unwrap(),
            Datum::Bool(true)
        );
    }

    #[test]
    fn disk_format() {
        // 2024-01-01 = 8766 日 = 0x223E
        assert_eq!(encode_date(Date(8766)), [0x3E, 0x22, 0, 0]);
        assert_eq!(decode_date(&[0x3E, 0x22, 0, 0]).unwrap(), Date(8766));
        assert_eq!(encode_date(Date::NEG_INFINITY), i32::MIN.to_le_bytes());
        // 2000-01-01 00:00:01.5 = 1500000 = 0x16E360
        let b = encode_timestamp(Timestamp(1_500_000));
        assert_eq!(b, [0x60, 0xE3, 0x16, 0, 0, 0, 0, 0]);
        assert_eq!(decode_timestamp(&b).unwrap(), Timestamp(1_500_000));
        let b = encode_timestamptz(TimestampTz(i64::MAX));
        assert_eq!(decode_timestamptz(&b).unwrap(), TimestampTz::INFINITY);
        assert_eq!(
            decode_date(&[1, 2, 3]).unwrap_err().sqlstate.code(),
            "XX001"
        );
        assert_eq!(
            decode_timestamp(&[0; 4]).unwrap_err().sqlstate.code(),
            "XX001"
        );
        assert_eq!(
            decode_timestamptz(&[]).unwrap_err().sqlstate.code(),
            "XX001"
        );
    }
}
