//! typmod の解釈と適用（`numeric(p,s)`、`char(n)`、`timestamp(p)`）。担当 T2。
//! `m4/09-types-functions.md` §3.3。

use super::datetime::dt_err;
use super::{Datum, Oid, SqlType, VARHDRSZ, bpchar, oid, ops};
use crate::error::{Error, Result, sqlstate};

/// この型が typmod を持てるか（`CoerceTypmod` が要りうる型）。
pub fn takes_typmod(type_oid: Oid) -> bool {
    matches!(
        type_oid,
        oid::VARCHAR | oid::BPCHAR | oid::NUMERIC | oid::TIMESTAMP | oid::TIMESTAMPTZ
    )
}

fn invalid_mod(msg: impl Into<String>) -> Error {
    Error::new(sqlstate::INVALID_PARAMETER_VALUE, msg)
}

/// 型名の修飾子（`numeric(10,2)` の `[10, 2]`）から typmod を作る。
///
/// `varchar` は `varchar(n)` = n + 4（n >= 1）。修飾子なしの型は `SqlType::of(oid)` を使うこと
/// （この関数は呼ばない）。
pub fn typmod_in(type_oid: Oid, mods: &[i64]) -> Result<i32> {
    let clamp = |m: i64| i32::try_from(m).unwrap_or(if m < 0 { i32::MIN } else { i32::MAX });
    match type_oid {
        oid::NUMERIC => {
            let mods: Vec<i32> = mods.iter().map(|m| clamp(*m)).collect();
            Ok(yuzhu_numeric::make_typmod(&mods)?)
        }
        oid::BPCHAR | oid::VARCHAR => {
            let name = if type_oid == oid::BPCHAR {
                "char"
            } else {
                "varchar"
            };
            let [n] = *mods else {
                return Err(invalid_mod("invalid type modifier"));
            };
            if n < 1 {
                return Err(invalid_mod(format!(
                    "length for type {name} must be at least 1"
                )));
            }
            if n > bpchar::MAX_BPCHAR_LEN {
                return Err(invalid_mod(format!(
                    "length for type {name} cannot exceed {}",
                    bpchar::MAX_BPCHAR_LEN
                )));
            }
            // MAX_BPCHAR_LEN + 4 は i32 に収まる。
            Ok(clamp(n) + VARHDRSZ)
        }
        oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let [p] = *mods else {
                return Err(invalid_mod("invalid type modifier"));
            };
            if p < 0 {
                return Err(invalid_mod(format!(
                    "TIMESTAMP({p}) precision must not be negative"
                )));
            }
            // 7 以上は 6 にする（D-9-10。PG の WARNING は出さない）。
            Ok(clamp(p.min(i64::from(yuzhu_datetime::MAX_PRECISION))))
        }
        other => Err(Error::internal(format!(
            "type with OID {other} does not take a type modifier"
        ))),
    }
}

/// 値に typmod を適用する。`CoerceTypmod` の評価、INSERT / UPDATE の代入、COPY が使う。
///
/// `explicit` は明示キャスト（切り詰めを許す）。`typmod < 0`、または型が typmod を持たなければ
/// `d` をそのまま返す。NULL はそのまま。
pub fn apply_typmod(d: Datum, ty: SqlType, explicit: bool) -> Result<Datum> {
    if ty.typmod < 0 || !takes_typmod(ty.oid) || d.is_null() {
        return Ok(d);
    }
    match (ty.oid, d) {
        (oid::NUMERIC, Datum::Numeric(n)) => Ok(Datum::Numeric(n.apply_typmod(ty.typmod)?)),
        (oid::BPCHAR, Datum::BpChar(s) | Datum::Text(s)) => {
            bpchar::bpchar_coerce(s, ty.typmod, explicit).map(Datum::BpChar)
        }
        (oid::VARCHAR, Datum::Text(s)) => {
            ops::varchar_coerce(s, ty.typmod, explicit).map(Datum::Text)
        }
        (oid::TIMESTAMP, Datum::Timestamp(t)) => t
            .with_typmod(ty.typmod)
            .map(Datum::Timestamp)
            .map_err(dt_err),
        (oid::TIMESTAMPTZ, Datum::TimestampTz(t)) => t
            .with_typmod(ty.typmod)
            .map(Datum::TimestampTz)
            .map_err(dt_err),
        (_, other) => Ok(other),
    }
}

/// `SqlType` の表示名（typmod つき）。`format_type` と同じ規則:
/// `numeric(10,2)`、`character(3)`、`timestamp(3) without time zone`。
pub fn display_with_typmod(ty: SqlType) -> String {
    let m = ty.typmod;
    match ty.oid {
        oid::NUMERIC => match yuzhu_numeric::typmod_precision_scale(m) {
            Some((p, s)) => format!("numeric({p},{s})"),
            None => "numeric".to_owned(),
        },
        oid::BPCHAR if m >= VARHDRSZ => format!("character({})", m - VARHDRSZ),
        oid::BPCHAR => "bpchar".to_owned(),
        oid::VARCHAR if m > VARHDRSZ => format!("character varying({})", m - VARHDRSZ),
        oid::VARCHAR => "character varying".to_owned(),
        oid::TIMESTAMP if m >= 0 => format!("timestamp({m}) without time zone"),
        oid::TIMESTAMP => "timestamp without time zone".to_owned(),
        oid::TIMESTAMPTZ if m >= 0 => format!("timestamp({m}) with time zone"),
        oid::TIMESTAMPTZ => "timestamp with time zone".to_owned(),
        _ => super::format_type(ty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yuzhu_datetime as dt;

    fn code(e: &Error) -> &str {
        e.sqlstate.code()
    }

    #[test]
    fn takes_typmod_types() {
        for t in [
            oid::VARCHAR,
            oid::BPCHAR,
            oid::NUMERIC,
            oid::TIMESTAMP,
            oid::TIMESTAMPTZ,
        ] {
            assert!(takes_typmod(t));
        }
        assert!(!takes_typmod(oid::INT4));
        assert!(!takes_typmod(oid::TEXT));
        assert!(!takes_typmod(oid::DATE));
    }

    #[test]
    fn typmod_in_numeric() {
        assert_eq!(
            typmod_in(oid::NUMERIC, &[10, 2]).unwrap(),
            (10 << 16 | 2) + 4
        );
        assert_eq!(typmod_in(oid::NUMERIC, &[5]).unwrap(), (5 << 16) + 4);
        // s > p は許す（PG15 以降）。
        assert!(typmod_in(oid::NUMERIC, &[3, 5]).is_ok());
        let e = typmod_in(oid::NUMERIC, &[0]).unwrap_err();
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "NUMERIC precision 0 must be between 1 and 1000");
        let e = typmod_in(oid::NUMERIC, &[10, 1001]).unwrap_err();
        assert_eq!(
            e.message,
            "NUMERIC scale 1001 must be between -1000 and 1000"
        );
        let e = typmod_in(oid::NUMERIC, &[1, 2, 3]).unwrap_err();
        assert_eq!(e.message, "invalid NUMERIC type modifier");
        // 負のスケールは通る。
        assert!(typmod_in(oid::NUMERIC, &[5, -1]).is_ok());
    }

    #[test]
    fn typmod_in_bpchar_and_timestamp() {
        assert_eq!(typmod_in(oid::BPCHAR, &[3]).unwrap(), 7);
        assert_eq!(typmod_in(oid::BPCHAR, &[10_485_760]).unwrap(), 10_485_764);
        let e = typmod_in(oid::BPCHAR, &[0]).unwrap_err();
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "length for type char must be at least 1");
        let e = typmod_in(oid::BPCHAR, &[10_485_761]).unwrap_err();
        assert_eq!(e.message, "length for type char cannot exceed 10485760");
        assert_eq!(code(&typmod_in(oid::BPCHAR, &[1, 2]).unwrap_err()), "22023");
        assert_eq!(typmod_in(oid::TIMESTAMP, &[3]).unwrap(), 3);
        assert_eq!(typmod_in(oid::TIMESTAMPTZ, &[0]).unwrap(), 0);
        assert_eq!(typmod_in(oid::TIMESTAMP, &[9]).unwrap(), 6);
        assert_eq!(
            code(&typmod_in(oid::TIMESTAMP, &[-1]).unwrap_err()),
            "22023"
        );
        assert_eq!(code(&typmod_in(oid::TIMESTAMP, &[]).unwrap_err()), "22023");
        assert_eq!(typmod_in(oid::VARCHAR, &[5]).unwrap(), 9);
        assert!(typmod_in(oid::INT4, &[1]).is_err());
    }

    #[test]
    fn apply_numeric() {
        let ty = SqlType::new(oid::NUMERIC, typmod_in(oid::NUMERIC, &[7, 2]).unwrap());
        let n = |s: &str| Datum::Numeric(yuzhu_numeric::Numeric::parse(s).unwrap());
        assert_eq!(
            apply_typmod(n("12345.6789"), ty, false).unwrap(),
            n("12345.68")
        );
        let ty2 = SqlType::new(oid::NUMERIC, typmod_in(oid::NUMERIC, &[2, 0]).unwrap());
        let e = apply_typmod(n("99.5"), ty2, false).unwrap_err();
        assert_eq!(code(&e), "22003");
        assert_eq!(e.message, "numeric field overflow");
        // 明示でも代入でも同じ。
        assert!(apply_typmod(n("99.5"), ty2, true).is_err());
        let ty3 = SqlType::new(oid::NUMERIC, typmod_in(oid::NUMERIC, &[5, -1]).unwrap());
        assert_eq!(apply_typmod(n("123.456"), ty3, false).unwrap(), n("120"));
        assert_eq!(apply_typmod(n("NaN"), ty, false).unwrap(), n("NaN"));
        assert!(apply_typmod(n("Infinity"), ty, false).is_err());
        // typmod なしなら何もしない。
        assert_eq!(
            apply_typmod(n("1.234"), SqlType::NUMERIC, false).unwrap(),
            n("1.234")
        );
    }

    #[test]
    fn apply_bpchar_and_varchar() {
        let ty = SqlType::new(oid::BPCHAR, 7);
        let bp = |s: &str| Datum::BpChar(s.into());
        assert_eq!(apply_typmod(bp("ab"), ty, false).unwrap(), bp("ab "));
        assert_eq!(apply_typmod(bp("abc  "), ty, false).unwrap(), bp("abc"));
        assert_eq!(apply_typmod(bp("abcdef"), ty, true).unwrap(), bp("abc"));
        let e = apply_typmod(bp("abcdef"), ty, false).unwrap_err();
        assert_eq!(code(&e), "22001");
        assert_eq!(e.message, "value too long for type character(3)");
        assert_eq!(apply_typmod(bp("é"), ty, false).unwrap(), bp("é  "));
        assert_eq!(
            apply_typmod(bp("ab "), SqlType::BPCHAR, false).unwrap(),
            bp("ab ")
        );
        assert_eq!(
            apply_typmod(Datum::Text("abcdef".into()), SqlType::varchar(3), true).unwrap(),
            Datum::Text("abc".into())
        );
        assert!(apply_typmod(Datum::Text("abcdef".into()), SqlType::varchar(3), false).is_err());
        assert_eq!(apply_typmod(Datum::Null, ty, false).unwrap(), Datum::Null);
        assert_eq!(
            apply_typmod(Datum::Int4(1), SqlType::INT4, false).unwrap(),
            Datum::Int4(1)
        );
    }

    #[test]
    fn apply_timestamp_rounds() {
        // 00:00:00.6 → typmod 0 で 00:00:01
        let ts = Datum::Timestamp(dt::Timestamp(600_000));
        let r = apply_typmod(ts, SqlType::new(oid::TIMESTAMP, 0), false).unwrap();
        assert_eq!(r, Datum::Timestamp(dt::Timestamp(1_000_000)));
        let tz = Datum::TimestampTz(dt::TimestampTz(1_234_567));
        let r = apply_typmod(tz, SqlType::new(oid::TIMESTAMPTZ, 3), false).unwrap();
        assert_eq!(r, Datum::TimestampTz(dt::TimestampTz(1_235_000)));
        // infinity はそのまま。
        let inf = Datum::Timestamp(dt::Timestamp::INFINITY);
        assert_eq!(
            apply_typmod(inf.clone(), SqlType::new(oid::TIMESTAMP, 0), false).unwrap(),
            inf
        );
    }

    #[test]
    fn display_names() {
        let numeric = SqlType::new(oid::NUMERIC, typmod_in(oid::NUMERIC, &[10, 2]).unwrap());
        assert_eq!(display_with_typmod(numeric), "numeric(10,2)");
        assert_eq!(display_with_typmod(SqlType::NUMERIC), "numeric");
        assert_eq!(
            display_with_typmod(SqlType::new(oid::BPCHAR, 7)),
            "character(3)"
        );
        assert_eq!(display_with_typmod(SqlType::BPCHAR), "bpchar");
        assert_eq!(
            display_with_typmod(SqlType::new(oid::TIMESTAMP, 3)),
            "timestamp(3) without time zone"
        );
        assert_eq!(
            display_with_typmod(SqlType::TIMESTAMPTZ),
            "timestamp with time zone"
        );
        assert_eq!(
            display_with_typmod(SqlType::new(oid::TIMESTAMPTZ, 0)),
            "timestamp(0) with time zone"
        );
        assert_eq!(
            display_with_typmod(SqlType::varchar(3)),
            "character varying(3)"
        );
        assert_eq!(display_with_typmod(SqlType::INT4), "integer");
    }
}
