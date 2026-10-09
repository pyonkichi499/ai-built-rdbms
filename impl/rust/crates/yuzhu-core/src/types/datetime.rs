//! `date` / `timestamp` / `timestamptz` と `yuzhu-datetime` の橋渡し。担当 T2。
//! `m4/09-types-functions.md` §3.4〜§3.8、§5。
//!
//! 入出力は `TypeEnv.datetime`（DateStyle・TimeZone・`now`）に従う。キャストと関数の本体は
//! ここに置き、`catalog/builtin.rs` の `CASTS` / `FUNCTIONS` が参照する。

use yuzhu_datetime::{Date, DateTimeEnv, DateTimeError, Timestamp, TimestampTz};

use super::binary::RecvBuf;
use super::{Datum, Oid, SqlType, TypeEnv, oid, ops};
use crate::error::{Error, Result, Severity, SqlState, sqlstate};
use crate::session::Notice;

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
        Datum::Time(t) => Ok(yuzhu_datetime::Time(*t).format()),
        Datum::Interval(_) => super::interval::interval_out(d, env),
        _ => return None,
    })
}

/// 日時 5 型（`date`・`time`・`timestamp`・`timestamptz`・`interval`）の `*_in`。
/// `ty.typmod` は入力の解釈中に適用される。日時の型でなければ `Error::internal`。
pub fn input_text(s: &str, ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum> {
    match ty.oid {
        oid::DATE => date_in(s, env),
        oid::TIME => yuzhu_datetime::Time::parse(s, ty.typmod, dt_env(env)?)
            .map(|t| Datum::Time(t.0))
            .map_err(dt_err),
        oid::TIMESTAMP => timestamp_in(s, ty.typmod, env),
        oid::TIMESTAMPTZ => timestamptz_in(s, ty.typmod, env),
        oid::INTERVAL => super::interval::interval_in(s, ty.typmod, env),
        other => Err(Error::internal(format!(
            "type with OID {other} is not a date/time type"
        ))),
    }
}

/// 日時 5 型の `*_out`（`DateStyle`・`TimeZone`・`IntervalStyle` に従う）。
pub fn output_text(d: &Datum, _ty: SqlType, env: &TypeEnv<'_>) -> Result<String> {
    output_any(d, env).unwrap_or_else(|| Err(bad_arg("date/time output")))
}

// ---------------------------------------------------------------------------
// 型修飾子（`pg_type.typmodin` / `typmodout` の実体。TD-D15: すべての入口が `apply_typmod` を通る）
// ---------------------------------------------------------------------------

/// 型修飾子の適用。`date` は typmod なし（そのまま）。`timestamp`・`timestamptz`・`time`・`interval`
/// は丸める（`interval` はフィールドの切り捨ても行う）。無限と NULL、`typmod < 0` はそのまま。
pub fn apply_typmod(d: Datum, ty: SqlType) -> Result<Datum> {
    if ty.typmod < 0 {
        return Ok(d);
    }
    match (ty.oid, d) {
        (oid::TIMESTAMP, Datum::Timestamp(t)) => t
            .with_typmod(ty.typmod)
            .map(Datum::Timestamp)
            .map_err(dt_err),
        (oid::TIMESTAMPTZ, Datum::TimestampTz(t)) => t
            .with_typmod(ty.typmod)
            .map(Datum::TimestampTz)
            .map_err(dt_err),
        (oid::TIME, Datum::Time(t)) => yuzhu_datetime::Time(t)
            .with_typmod(ty.typmod)
            .map(|t| Datum::Time(t.0))
            .map_err(dt_err),
        (oid::INTERVAL, Datum::Interval(v)) => v
            .with_typmod(ty.typmod)
            .map(Datum::Interval)
            .map_err(dt_err),
        (_, other) => Ok(other),
    }
}

/// 型名の修飾子から typmod を作る（`time(p)`・`timestamp(p)`・`timestamptz(p)`・`interval ...`）。
/// `p > 6` の WARNING（22023）は `notices` に積んで 6 にする。
pub fn typmod_in(type_oid: Oid, mods: &[i64], notices: &mut Vec<Notice>) -> Result<i32> {
    let (typmod, warning) = match type_oid {
        oid::TIME | oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let name = match type_oid {
                oid::TIME => "time",
                oid::TIMESTAMP => "timestamp",
                _ => "timestamptz",
            };
            let [p] = *mods else {
                return Err(Error::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("invalid {} type modifier", name.to_ascii_uppercase()),
                ));
            };
            yuzhu_datetime::precision_typmod_in(name, p).map_err(dt_err)?
        }
        oid::INTERVAL => yuzhu_datetime::interval_typmod_in(mods).map_err(dt_err)?,
        other => {
            return Err(Error::internal(format!(
                "type with OID {other} is not a date/time type that takes a type modifier"
            )));
        }
    };
    if let Some(w) = warning {
        notices.push(Notice::new(
            Severity::Warning,
            SqlState(w.sqlstate),
            w.message,
        ));
    }
    Ok(typmod)
}

/// `typmodout`: `format_type` が型名に続ける文字列（`"(3)"`、`" year to month"`、`""`）。
/// 日時の型でなければ空文字列。
pub fn typmod_out(type_oid: Oid, typmod: i32) -> String {
    match type_oid {
        oid::TIME | oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            yuzhu_datetime::precision_typmod_out(typmod)
        }
        oid::INTERVAL => yuzhu_datetime::interval_typmod_out(typmod),
        _ => String::new(),
    }
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
// バイナリ形式（`m5/06` §6.7）。ネットワークバイトオーダー、integer_datetimes = on
// ---------------------------------------------------------------------------

/// `date_in` が受け付ける日数の範囲（4714-11-24 BC 〜 5874897-12-31。2000-01-01 からの日数）。
const DATE_MIN_DAYS: i32 = -2_451_545;
const DATE_MAX_DAYS: i32 = 2_147_483_493 - 2_451_545;
/// `IS_VALID_TIMESTAMP`（4714-11-24 BC 〜 294276-12-31）。`yuzhu-datetime` の `calendar.rs` と同じ値。
const TS_MIN_MICROS: i64 = -211_813_488_000_000_000;
const TS_END_MICROS: i64 = 9_223_371_331_200_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn out_of_range(what: &str) -> Error {
    Error::new(
        sqlstate::DATETIME_FIELD_OVERFLOW,
        format!("{what} out of range"),
    )
}

/// 日時 5 型の `*_send`。型と Datum が合わなければ内部エラー。
pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>> {
    Ok(match (ty.oid, d) {
        (oid::DATE, Datum::Date(v)) => v.0.to_be_bytes().to_vec(),
        (oid::TIME, Datum::Time(v)) => v.to_be_bytes().to_vec(),
        (oid::TIMESTAMP, Datum::Timestamp(v)) => v.0.to_be_bytes().to_vec(),
        (oid::TIMESTAMPTZ, Datum::TimestampTz(v)) => v.0.to_be_bytes().to_vec(),
        (oid::INTERVAL, Datum::Interval(iv)) => {
            let mut out = Vec::with_capacity(16);
            out.extend_from_slice(&iv.micros.to_be_bytes());
            out.extend_from_slice(&iv.days.to_be_bytes());
            out.extend_from_slice(&iv.months.to_be_bytes());
            out
        }
        _ => {
            return Err(Error::internal(format!(
                "binary_send: datum {d:?} does not match type oid {}",
                ty.oid
            )));
        }
    })
}

/// 日時 5 型の `*_recv`（範囲検査つき。typmod は適用しない）。
pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum> {
    Ok(match ty.oid {
        oid::DATE => {
            let v = buf.i32()?;
            if v != i32::MAX && v != i32::MIN && !(DATE_MIN_DAYS..=DATE_MAX_DAYS).contains(&v) {
                return Err(out_of_range("date"));
            }
            Datum::Date(Date(v))
        }
        oid::TIME => {
            let v = buf.i64()?;
            if !(0..=MICROS_PER_DAY).contains(&v) {
                return Err(out_of_range("time"));
            }
            Datum::Time(v)
        }
        oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let v = buf.i64()?;
            if v != i64::MAX && v != i64::MIN && !(TS_MIN_MICROS..TS_END_MICROS).contains(&v) {
                return Err(out_of_range("timestamp"));
            }
            if ty.oid == oid::TIMESTAMP {
                Datum::Timestamp(Timestamp(v))
            } else {
                Datum::TimestampTz(TimestampTz(v))
            }
        }
        oid::INTERVAL => {
            let micros = buf.i64()?;
            let days = buf.i32()?;
            let months = buf.i32()?;
            Datum::Interval(yuzhu_datetime::Interval {
                months,
                days,
                micros,
            })
        }
        _ => {
            return Err(Error::internal(format!(
                "binary_recv: not a date/time type: oid {}",
                ty.oid
            )));
        }
    })
}

#[cfg(test)]
mod binary_tests {
    use super::*;
    use yuzhu_datetime::Interval;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn send(d: Datum, o: u32) -> String {
        hex(&binary_send(&d, SqlType::of(o)).unwrap())
    }
    fn recv(h: &str, o: u32) -> Result<Datum> {
        let b = unhex(h);
        binary_recv(&mut RecvBuf::new(&b), SqlType::of(o))
    }

    #[test]
    fn send_fixed_values() {
        assert_eq!(send(Datum::Date(Date(8767)), oid::DATE), "0000223f");
        assert_eq!(send(Datum::Date(Date(-10957)), oid::DATE), "ffffd533");
        assert_eq!(send(Datum::Date(Date(-2451507)), oid::DATE), "ffda97cd");
        assert_eq!(send(Datum::Date(Date(i32::MAX)), oid::DATE), "7fffffff");
        assert_eq!(send(Datum::Date(Date(i32::MIN)), oid::DATE), "80000000");
        assert_eq!(
            send(Datum::Time(14_706_789_000), oid::TIME),
            "000000036c97ca88"
        );
        assert_eq!(
            send(Datum::Time(86_400_000_000), oid::TIME),
            "000000141dd76000"
        );
        assert_eq!(
            send(Datum::Timestamp(Timestamp(0)), oid::TIMESTAMP),
            "0000000000000000"
        );
        assert_eq!(
            send(
                Datum::Timestamp(Timestamp(-946_684_800_000_000)),
                oid::TIMESTAMP
            ),
            "fffca2fec4c82000"
        );
        assert_eq!(
            send(
                Datum::TimestampTz(TimestampTz(0x0002b0ec85204f35)),
                oid::TIMESTAMPTZ
            ),
            "0002b0ec85204f35"
        );
        assert_eq!(
            send(Datum::Timestamp(Timestamp(i64::MAX)), oid::TIMESTAMP),
            "7fffffffffffffff"
        );
        let iv = Interval {
            months: 14,
            days: 3,
            micros: 14_706_789_000,
        };
        assert_eq!(
            send(Datum::Interval(iv), oid::INTERVAL),
            "000000036c97ca8800000003".to_owned() + "0000000e"
        );
        let iv = Interval {
            months: 0,
            days: -1,
            micros: -3_600_000_000,
        };
        assert_eq!(
            send(Datum::Interval(iv), oid::INTERVAL),
            "ffffffff296c5c00ffffffff00000000"
        );
    }

    #[test]
    fn roundtrip() {
        for (h, o) in [
            ("0000223f", oid::DATE),
            ("7fffffff", oid::DATE),
            ("ffda97a7", oid::DATE),
            ("000000141dd76000", oid::TIME),
            ("0002b0ec85204f35", oid::TIMESTAMP),
            ("8000000000000000", oid::TIMESTAMPTZ),
            ("7fffffffffffffff7fffffff7fffffff", oid::INTERVAL),
            ("ffffffff296c5c00ffffffff00000000", oid::INTERVAL),
        ] {
            let d = recv(h, o).unwrap();
            assert_eq!(send(d, o), h);
        }
    }

    #[test]
    fn recv_range_errors() {
        for (h, o, msg) in [
            ("7ffffff0", oid::DATE, "date out of range"),
            ("ffda97a6", oid::DATE, "date out of range"),
            ("7fffffffffffffff", oid::TIME, "time out of range"),
            ("ffffffffffffffff", oid::TIME, "time out of range"),
            ("000000141dd76001", oid::TIME, "time out of range"),
            ("7ffffffffffffff0", oid::TIMESTAMP, "timestamp out of range"),
            (
                "8000000000000001",
                oid::TIMESTAMPTZ,
                "timestamp out of range",
            ),
        ] {
            let e = recv(h, o).unwrap_err();
            assert_eq!(e.sqlstate.0, "22008", "{h}");
            assert_eq!(e.message, msg);
        }
        assert!(recv("ffda97a7", oid::DATE).is_ok());
        assert!(recv("7fffff7fffffffff", oid::TIMESTAMP).is_err());
    }

    #[test]
    fn short_input_is_error() {
        assert!(recv("0000", oid::DATE).is_err());
        assert!(recv("00000000000000", oid::INTERVAL).is_err());
    }
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

/// 日時の関数の本体（TD-3。`m5/06` §6.5.3）: `extract` / `date_part` / `date_trunc`、`age`、`make_*`、
/// `to_timestamp(float8)`、`date_bin`、`date_add` / `date_subtract`、`isfinite`、`generate_series(timestamp)`。
/// `catalog/builtin/datetime.rs` の `TD3_FUNCTIONS` が参照する。引数の NULL は strict なので来ない。
pub mod fns {
    use yuzhu_datetime::{
        Date, DateTimeEnv, Interval, NumericValue, Time, TimeZone, Timestamp, TimestampTz,
    };

    use super::{Datum, TypeEnv, bad_arg, dt_env, dt_err};
    use crate::error::{Error, Result, sqlstate};

    fn arg(args: &[Datum], i: usize) -> Result<&Datum> {
        args.get(i).ok_or_else(|| bad_arg("date/time function"))
    }

    fn text(args: &[Datum], i: usize) -> Result<&str> {
        match arg(args, i)? {
            Datum::Text(s) => Ok(s),
            _ => Err(bad_arg("date/time function (text)")),
        }
    }

    fn int(args: &[Datum], i: usize) -> Result<i32> {
        arg(args, i)?
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| bad_arg("date/time function (integer)"))
    }

    fn float(args: &[Datum], i: usize) -> Result<f64> {
        arg(args, i)?
            .as_f64()
            .ok_or_else(|| bad_arg("date/time function (float8)"))
    }

    fn interval(args: &[Datum], i: usize) -> Result<Interval> {
        match arg(args, i)? {
            Datum::Interval(v) => Ok(*v),
            _ => Err(bad_arg("date/time function (interval)")),
        }
    }

    fn date(args: &[Datum], i: usize) -> Result<Date> {
        match arg(args, i)? {
            Datum::Date(v) => Ok(*v),
            _ => Err(bad_arg("date/time function (date)")),
        }
    }

    fn timestamp(args: &[Datum], i: usize) -> Result<Timestamp> {
        match arg(args, i)? {
            Datum::Timestamp(v) => Ok(*v),
            _ => Err(bad_arg("date/time function (timestamp)")),
        }
    }

    fn timestamptz(args: &[Datum], i: usize) -> Result<TimestampTz> {
        match arg(args, i)? {
            Datum::TimestampTz(v) => Ok(*v),
            _ => Err(bad_arg("date/time function (timestamptz)")),
        }
    }

    fn zone<'e, 'a>(env: &'e TypeEnv<'a>) -> Result<(&'a TimeZone, &'e DateTimeEnv<'a>)> {
        let dt = dt_env(env)?;
        Ok((dt.time_zone, dt))
    }

    fn as_float(r: yuzhu_datetime::Result<Option<f64>>) -> Result<Datum> {
        Ok(r.map_err(dt_err)?.map_or(Datum::Null, Datum::Float8))
    }

    /// `extract` の結果（`NumericValue`）を `numeric` にする。`Infinity` / `-Infinity` は特殊値。
    fn as_numeric(r: yuzhu_datetime::Result<Option<NumericValue>>) -> Result<Datum> {
        match r.map_err(dt_err)? {
            None => Ok(Datum::Null),
            Some(nv) => yuzhu_numeric::Numeric::parse(&nv.to_string())
                .map(Datum::Numeric)
                .map_err(Error::from),
        }
    }

    // ----- date_part (float8) / extract (numeric) --------------------------------

    pub fn date_part_interval(args: &[Datum]) -> Result<Datum> {
        as_float(yuzhu_datetime::date_part_interval(
            text(args, 0)?,
            &interval(args, 1)?,
        ))
    }
    pub fn date_part_date(args: &[Datum]) -> Result<Datum> {
        as_float(yuzhu_datetime::date_part_date(
            text(args, 0)?,
            date(args, 1)?,
        ))
    }
    pub fn date_part_timestamp(args: &[Datum]) -> Result<Datum> {
        as_float(yuzhu_datetime::date_part_timestamp(
            text(args, 0)?,
            timestamp(args, 1)?,
        ))
    }
    pub fn date_part_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, _) = zone(env)?;
        as_float(yuzhu_datetime::date_part_timestamptz(
            text(args, 0)?,
            timestamptz(args, 1)?,
            tz,
        ))
    }
    pub fn extract_interval(args: &[Datum]) -> Result<Datum> {
        as_numeric(yuzhu_datetime::extract_interval(
            text(args, 0)?,
            &interval(args, 1)?,
        ))
    }
    pub fn extract_date(args: &[Datum]) -> Result<Datum> {
        as_numeric(yuzhu_datetime::extract_date(text(args, 0)?, date(args, 1)?))
    }
    pub fn extract_timestamp(args: &[Datum]) -> Result<Datum> {
        as_numeric(yuzhu_datetime::extract_timestamp(
            text(args, 0)?,
            timestamp(args, 1)?,
        ))
    }
    pub fn extract_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, _) = zone(env)?;
        as_numeric(yuzhu_datetime::extract_timestamptz(
            text(args, 0)?,
            timestamptz(args, 1)?,
            tz,
        ))
    }

    // ----- date_trunc ------------------------------------------------------------

    pub fn date_trunc_timestamp(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::date_trunc_timestamp(text(args, 0)?, timestamp(args, 1)?)
            .map(Datum::Timestamp)
            .map_err(dt_err)
    }
    pub fn date_trunc_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, _) = zone(env)?;
        yuzhu_datetime::date_trunc_timestamptz(text(args, 0)?, timestamptz(args, 1)?, tz)
            .map(Datum::TimestampTz)
            .map_err(dt_err)
    }
    /// `date_trunc(text, timestamptz, text)`: 3 つ目のゾーンで切り捨てる。
    pub fn date_trunc_timestamptz_zone(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (_, dt) = zone(env)?;
        let tz = yuzhu_datetime::zone_by_name(text(args, 2)?, dt).map_err(dt_err)?;
        yuzhu_datetime::date_trunc_timestamptz(text(args, 0)?, timestamptz(args, 1)?, &tz)
            .map(Datum::TimestampTz)
            .map_err(dt_err)
    }
    pub fn date_trunc_interval(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::date_trunc_interval(text(args, 0)?, &interval(args, 1)?)
            .map(Datum::Interval)
            .map_err(dt_err)
    }

    // ----- date_bin / date_add / date_subtract -----------------------------------

    pub fn date_bin_timestamp(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::date_bin_timestamp(
            &interval(args, 0)?,
            timestamp(args, 1)?,
            timestamp(args, 2)?,
        )
        .map(Datum::Timestamp)
        .map_err(dt_err)
    }
    pub fn date_bin_timestamptz(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::date_bin_timestamptz(
            &interval(args, 0)?,
            timestamptz(args, 1)?,
            timestamptz(args, 2)?,
        )
        .map(Datum::TimestampTz)
        .map_err(dt_err)
    }

    fn add_in(args: &[Datum], env: &TypeEnv<'_>, sign: i32, zoned: bool) -> Result<Datum> {
        let (session_tz, dt) = zone(env)?;
        let named;
        let tz = if zoned {
            named = yuzhu_datetime::zone_by_name(text(args, 2)?, dt).map_err(dt_err)?;
            &named
        } else {
            session_tz
        };
        let (ts, span) = (timestamptz(args, 0)?, interval(args, 1)?);
        let r = if sign >= 0 {
            ts.add_interval(&span, tz)
        } else {
            ts.sub_interval(&span, tz)
        };
        r.map(Datum::TimestampTz).map_err(dt_err)
    }
    pub fn date_add(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        add_in(args, env, 1, false)
    }
    pub fn date_add_zone(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        add_in(args, env, 1, true)
    }
    pub fn date_subtract(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        add_in(args, env, -1, false)
    }
    pub fn date_subtract_zone(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        add_in(args, env, -1, true)
    }

    // ----- age -------------------------------------------------------------------

    pub fn age_timestamp(args: &[Datum]) -> Result<Datum> {
        timestamp(args, 0)?
            .age(timestamp(args, 1)?)
            .map(Datum::Interval)
            .map_err(dt_err)
    }
    pub fn age_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, _) = zone(env)?;
        timestamptz(args, 0)?
            .age(timestamptz(args, 1)?, tz)
            .map(Datum::Interval)
            .map_err(dt_err)
    }
    /// `age(timestamp)` = `age(current_date::timestamp, $1)`。今日はトランザクション開始時刻の現地日付。
    pub fn age_timestamp_1(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, dt) = zone(env)?;
        let today = dt
            .now
            .to_date(tz)
            .and_then(Date::to_timestamp)
            .map_err(dt_err)?;
        today
            .age(timestamp(args, 0)?)
            .map(Datum::Interval)
            .map_err(dt_err)
    }
    /// `age(timestamptz)` = `age(current_date::timestamptz, $1)`。
    pub fn age_timestamptz_1(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        let (tz, dt) = zone(env)?;
        let today = dt
            .now
            .to_date(tz)
            .and_then(|d| d.to_timestamptz(tz))
            .map_err(dt_err)?;
        today
            .age(timestamptz(args, 0)?, tz)
            .map(Datum::Interval)
            .map_err(dt_err)
    }

    // ----- make_* / to_timestamp(float8) -----------------------------------------

    pub fn make_date(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::make_date(int(args, 0)?, int(args, 1)?, int(args, 2)?)
            .map(Datum::Date)
            .map_err(dt_err)
    }
    pub fn make_time(args: &[Datum]) -> Result<Datum> {
        Time::make(int(args, 0)?, int(args, 1)?, float(args, 2)?)
            .map(|t| Datum::Time(t.0))
            .map_err(dt_err)
    }
    pub fn make_timestamp(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::make_timestamp(
            int(args, 0)?,
            int(args, 1)?,
            int(args, 2)?,
            int(args, 3)?,
            int(args, 4)?,
            float(args, 5)?,
        )
        .map(Datum::Timestamp)
        .map_err(dt_err)
    }
    fn make_timestamptz_in(args: &[Datum], env: &TypeEnv<'_>, zoned: bool) -> Result<Datum> {
        let (_, dt) = zone(env)?;
        let zone_name = if zoned { Some(text(args, 6)?) } else { None };
        yuzhu_datetime::make_timestamptz(
            int(args, 0)?,
            int(args, 1)?,
            int(args, 2)?,
            int(args, 3)?,
            int(args, 4)?,
            float(args, 5)?,
            zone_name,
            dt,
        )
        .map(Datum::TimestampTz)
        .map_err(dt_err)
    }
    pub fn make_timestamptz(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        make_timestamptz_in(args, env, false)
    }
    pub fn make_timestamptz_zone(args: &[Datum], env: &TypeEnv<'_>) -> Result<Datum> {
        make_timestamptz_in(args, env, true)
    }
    /// `make_interval(years, months, weeks, days, hours, mins, secs)`。位置引数 7 個（名前つき引数の
    /// 既定値の補完はアナライザの仕事）。
    pub fn make_interval(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::make_interval(
            int(args, 0)?,
            int(args, 1)?,
            int(args, 2)?,
            int(args, 3)?,
            int(args, 4)?,
            int(args, 5)?,
            float(args, 6)?,
        )
        .map(Datum::Interval)
        .map_err(dt_err)
    }
    pub fn to_timestamp_float(args: &[Datum]) -> Result<Datum> {
        yuzhu_datetime::to_timestamp_float(float(args, 0)?)
            .map(Datum::TimestampTz)
            .map_err(dt_err)
    }

    // ----- isfinite --------------------------------------------------------------

    pub fn isfinite_date(args: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(date(args, 0)?.is_finite()))
    }
    pub fn isfinite_timestamp(args: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(timestamp(args, 0)?.is_finite()))
    }
    pub fn isfinite_timestamptz(args: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(timestamptz(args, 0)?.is_finite()))
    }

    // ----- generate_series(timestamp, timestamp, interval) -----------------------

    struct TimestampSeries {
        cur: i64,
        stop: i64,
        step: Interval,
        forward: bool,
        done: bool,
    }

    impl crate::catalog::SetIter for TimestampSeries {
        fn next(&mut self) -> Result<Option<Datum>> {
            if self.done {
                return Ok(None);
            }
            let go = if self.forward {
                self.cur <= self.stop
            } else {
                self.cur >= self.stop
            };
            if !go {
                self.done = true;
                return Ok(None);
            }
            let out = self.cur;
            // PostgreSQL は返す前に次の値を作る（範囲外はこの行の代わりにエラー）。
            match Timestamp(self.cur).add_interval(&self.step) {
                Ok(n) => self.cur = n.0,
                Err(e) => {
                    self.done = true;
                    return Err(dt_err(e));
                }
            }
            Ok(Some(Datum::Timestamp(Timestamp(out))))
        }
    }

    /// `generate_series(timestamp, timestamp, interval)`（938）。`cur = cur + step` を繰り返す。
    pub fn generate_series_timestamp(args: &[Datum]) -> Result<Box<dyn crate::catalog::SetIter>> {
        let (start, end, step) = (timestamp(args, 0)?, timestamp(args, 1)?, interval(args, 2)?);
        if !step.is_finite() {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "step size cannot be infinite",
            ));
        }
        if step == Interval::ZERO {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "step size cannot equal zero",
            ));
        }
        let forward = step > Interval::ZERO;
        Ok(Box::new(TimestampSeries {
            cur: start.0,
            stop: end.0,
            step,
            forward,
            // 開始が無限なら 0 行
            done: !start.is_finite(),
        }))
    }
}

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
