//! FK のキーの抽出と比較（純粋関数。`m5/07` §4.4・§6.2）。
//!
//! 外部キーの子と親の列は型が違ってよい（`bigint` → `int`、`int` → `numeric` など）。ここは
//! 型の組の分類（`classify_pair`）、キーの取り出し・NULL の分類・画像の比較、子のキーを親の型へ
//! 直して検索の値を作る処理を持つ。ストレージにも session にも触れない。

use std::cmp::Ordering;

use crate::catalog::builtin::{CastContext, find_cast};
use crate::error::Result;
use crate::executor::eval::apply_cast;
use crate::types::{Datum, SqlType, TypeEnv, cmp_datum, oid};

/// 列の組ごとの比べ方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiKeyCmp {
    /// 子と親が同じ型（`Datum` の変種が同じ）。`cmp_datum` でそのまま比べる。
    Same,
    /// 子と親がどちらも int2 / int4 / int8。`cmp_datum` が幅の違いを吸収する。
    IntCross,
    /// 子の値を親の型へ暗黙キャストしてから比べる（int → numeric、bpchar → text など）。
    /// 子の索引を引く向きには使えない。
    CastChild { from: SqlType, to: SqlType },
}

impl RiKeyCmp {
    /// 値を変換せずに索引のプローブにできるか（`Same` / `IntCross`）。
    pub fn is_direct(self) -> bool {
        !matches!(self, RiKeyCmp::CastChild { .. })
    }
}

/// `Datum` の変種の区別（型 OID から引く）。同じ値なら `cmp_datum` がそのまま比べられる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VariantClass {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    BpChar,
    Oid,
    Char,
    Xid,
    Cid,
    Tid,
    OidVector,
    Int2Vector,
    Date,
    Timestamp,
    TimestampTz,
    Time,
    Interval,
    Bytea,
    Uuid,
}

fn variant_class(type_oid: u32) -> Option<VariantClass> {
    Some(match type_oid {
        oid::BOOL => VariantClass::Bool,
        oid::INT2 => VariantClass::Int2,
        oid::INT4 => VariantClass::Int4,
        oid::INT8 => VariantClass::Int8,
        oid::FLOAT4 => VariantClass::Float4,
        oid::FLOAT8 => VariantClass::Float8,
        oid::NUMERIC => VariantClass::Numeric,
        oid::TEXT | oid::VARCHAR | oid::UNKNOWN | oid::NAME | oid::PG_NODE_TREE => {
            VariantClass::Text
        }
        oid::BPCHAR => VariantClass::BpChar,
        oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE | oid::REGNAMESPACE => {
            VariantClass::Oid
        }
        oid::CHAR => VariantClass::Char,
        oid::XID => VariantClass::Xid,
        oid::CID => VariantClass::Cid,
        oid::TID => VariantClass::Tid,
        oid::OIDVECTOR => VariantClass::OidVector,
        oid::INT2VECTOR | oid::INT2_ARRAY => VariantClass::Int2Vector,
        oid::DATE => VariantClass::Date,
        oid::TIMESTAMP => VariantClass::Timestamp,
        oid::TIMESTAMPTZ => VariantClass::TimestampTz,
        oid::TIME => VariantClass::Time,
        oid::INTERVAL => VariantClass::Interval,
        oid::BYTEA => VariantClass::Bytea,
        oid::UUID => VariantClass::Uuid,
        _ => return None,
    })
}

fn is_int(c: VariantClass) -> bool {
    matches!(
        c,
        VariantClass::Int2 | VariantClass::Int4 | VariantClass::Int8
    )
}

/// 子の列の型と親の列の型の組を分類する（FK-D15）。`None` は組み合わせられない（DDL は 42804）。
///
/// 同じ型、`Datum` の変種が同じ型、整数どうし、子から親への暗黙キャストがある型、の順に調べる。
pub fn classify_pair(child: SqlType, parent: SqlType) -> Option<RiKeyCmp> {
    if child.oid == parent.oid {
        return Some(RiKeyCmp::Same);
    }
    let (cc, pc) = (variant_class(child.oid), variant_class(parent.oid));
    if let (Some(c), Some(p)) = (cc, pc) {
        if c == p {
            return Some(RiKeyCmp::Same);
        }
        if is_int(c) && is_int(p) {
            return Some(RiKeyCmp::IntCross);
        }
    }
    find_cast(child.oid, parent.oid)
        .filter(|c| c.context == CastContext::Implicit)
        .map(|_| RiKeyCmp::CastChild {
            from: child,
            to: parent,
        })
}

/// 行から `cols`（attnum、1 始まり）の値を順に取り出す。範囲外の列は NULL。
pub fn extract_key(row: &[Datum], cols: &[i16]) -> Vec<Datum> {
    cols.iter()
        .map(|&a| {
            usize::try_from(i32::from(a) - 1)
                .ok()
                .and_then(|i| row.get(i))
                .cloned()
                .unwrap_or(Datum::Null)
        })
        .collect()
}

/// キーの NULL の入り方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NullClass {
    NoneNull,
    AllNull,
    Mixed,
}

/// 空のキーは `NoneNull`。
pub fn null_class(key: &[Datum]) -> NullClass {
    let nulls = key.iter().filter(|d| d.is_null()).count();
    if nulls == 0 {
        NullClass::NoneNull
    } else if nulls == key.len() {
        NullClass::AllNull
    } else {
        NullClass::Mixed
    }
}

/// PG の `datum_image_eq` 相当。値が `cmp_datum` で等しくても、表現が違えば `false`
/// （numeric の 1.0 と 1.00、末尾空白の違う bpchar、`0.0` と `-0.0`）。NULL どうしは等しい。
pub fn images_equal(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| image_eq(x, y))
}

fn image_eq(a: &Datum, b: &Datum) -> bool {
    use yuzhu_numeric::Numeric;
    match (a, b) {
        (Datum::Null, Datum::Null) | (Datum::Void, Datum::Void) => true,
        (Datum::Bool(x), Datum::Bool(y)) => x == y,
        (Datum::Int2(x), Datum::Int2(y)) => x == y,
        (Datum::Int4(x), Datum::Int4(y)) => x == y,
        (Datum::Int8(x), Datum::Int8(y)) | (Datum::Time(x), Datum::Time(y)) => x == y,
        (Datum::Float4(x), Datum::Float4(y)) => x.to_bits() == y.to_bits(),
        (Datum::Float8(x), Datum::Float8(y)) => x.to_bits() == y.to_bits(),
        (Datum::Numeric(x), Datum::Numeric(y)) => match (x, y) {
            (Numeric::NaN, Numeric::NaN)
            | (Numeric::PosInf, Numeric::PosInf)
            | (Numeric::NegInf, Numeric::NegInf) => true,
            (Numeric::Finite(x), Numeric::Finite(y)) => {
                x.is_negative() == y.is_negative()
                    && x.weight() == y.weight()
                    && x.dscale() == y.dscale()
                    && x.digits() == y.digits()
            }
            _ => false,
        },
        (Datum::Text(x), Datum::Text(y)) | (Datum::BpChar(x), Datum::BpChar(y)) => x == y,
        (Datum::Oid(x), Datum::Oid(y))
        | (Datum::Xid(x), Datum::Xid(y))
        | (Datum::Cid(x), Datum::Cid(y)) => x == y,
        (Datum::Char(x), Datum::Char(y)) => x == y,
        (Datum::Tid(x), Datum::Tid(y)) => x == y,
        (Datum::OidVector(x), Datum::OidVector(y)) => x == y,
        (Datum::Int2Vector(x), Datum::Int2Vector(y)) => x == y,
        (Datum::Date(x), Datum::Date(y)) => x == y,
        (Datum::Timestamp(x), Datum::Timestamp(y)) => x == y,
        (Datum::TimestampTz(x), Datum::TimestampTz(y)) => x == y,
        (Datum::Interval(x), Datum::Interval(y)) => {
            x.months == y.months && x.days == y.days && x.micros == y.micros
        }
        (Datum::Bytea(x), Datum::Bytea(y)) => x == y,
        (Datum::Uuid(x), Datum::Uuid(y)) => x == y,
        (Datum::Array(x), Datum::Array(y)) => {
            x.elem_type == y.elem_type
                && x.dims == y.dims
                && x.items.len() == y.items.len()
                && x.items.iter().zip(&y.items).all(|(p, q)| image_eq(p, q))
        }
        (Datum::Int4Array(x), Datum::Int4Array(y)) => x == y,
        _ => false,
    }
}

/// 値（`=`）で比べて全列が等しいか。FK 側の更新の判定（`ri_KeysEqual`）に使う。
/// 長さが違えば `false`。NULL を含むキーは呼び出し側が先に除く（NULL どうしは等しい扱い）。
pub fn keys_equal(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| cmp_datum(x, y) == Ordering::Equal)
}

/// 検索のキー（FK の列順）を、索引の列順に並べ直す。`perm[i]` は、索引の列位置 `i` に入れる値が
/// FK のキーの何番目か（`FkPlan::parent_perm` / `child_index.1`）。
pub fn permute_key(key: &[Datum], perm: &[usize]) -> Vec<Datum> {
    perm.iter()
        .map(|&i| key.get(i).cloned().unwrap_or(Datum::Null))
        .collect()
}

/// 子のキーを親の型へ直す（子 → 親の検索のプローブ。`CheckParent`）。`Same` / `IntCross` は
/// そのまま、`CastChild` は暗黙キャストを適用する。キャストが失敗したらそのエラーを返す。
/// NULL はそのまま。
pub fn child_key_to_parent(
    key: &[Datum],
    cmps: &[RiKeyCmp],
    env: &TypeEnv<'_>,
) -> Result<Vec<Datum>> {
    key.iter()
        .zip(cmps)
        .map(|(d, c)| match *c {
            RiKeyCmp::Same | RiKeyCmp::IntCross => Ok(d.clone()),
            RiKeyCmp::CastChild { from, to } => cast_implicit(d, from, to, env),
        })
        .collect()
}

/// 暗黙キャスト 1 つ。`classify_pair` が通した組なので、キャストが見つからないのは内部エラー。
pub fn cast_implicit(d: &Datum, from: SqlType, to: SqlType, env: &TypeEnv<'_>) -> Result<Datum> {
    let cast = find_cast(from.oid, to.oid).ok_or_else(|| {
        crate::error::Error::internal(format!(
            "no cast from type {} to type {} for a foreign key",
            from.oid, to.oid
        ))
    })?;
    apply_cast(d.clone(), from, to, cast.method, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yuzhu_numeric::Numeric;

    fn num(s: &str) -> Datum {
        Datum::Numeric(s.parse::<Numeric>().expect("numeric literal"))
    }

    #[test]
    fn classify_same_and_variant() {
        assert_eq!(
            classify_pair(SqlType::INT4, SqlType::INT4),
            Some(RiKeyCmp::Same)
        );
        assert_eq!(
            classify_pair(SqlType::TEXT, SqlType::varchar(5)),
            Some(RiKeyCmp::Same)
        );
        assert_eq!(
            classify_pair(SqlType::varchar(5), SqlType::TEXT),
            Some(RiKeyCmp::Same)
        );
    }

    #[test]
    fn classify_int_cross() {
        assert_eq!(
            classify_pair(SqlType::INT8, SqlType::INT4),
            Some(RiKeyCmp::IntCross)
        );
        assert_eq!(
            classify_pair(SqlType::INT2, SqlType::INT8),
            Some(RiKeyCmp::IntCross)
        );
    }

    #[test]
    fn classify_cast_child() {
        assert_eq!(
            classify_pair(SqlType::INT4, SqlType::NUMERIC),
            Some(RiKeyCmp::CastChild {
                from: SqlType::INT4,
                to: SqlType::NUMERIC
            })
        );
        assert!(matches!(
            classify_pair(SqlType::DATE, SqlType::TIMESTAMP),
            Some(RiKeyCmp::CastChild { .. })
        ));
        assert!(matches!(
            classify_pair(SqlType::BPCHAR, SqlType::TEXT),
            Some(RiKeyCmp::CastChild { .. })
        ));
    }

    #[test]
    fn classify_rejects() {
        assert_eq!(classify_pair(SqlType::NUMERIC, SqlType::INT4), None);
        assert_eq!(classify_pair(SqlType::TEXT, SqlType::INT4), None);
        assert_eq!(classify_pair(SqlType::INT4, SqlType::TEXT), None);
    }

    #[test]
    fn extract_by_attnum() {
        let row = vec![Datum::Int4(1), Datum::Text("a".into()), Datum::Null];
        assert_eq!(
            extract_key(&row, &[2, 1]),
            vec![Datum::Text("a".into()), Datum::Int4(1)]
        );
        assert_eq!(extract_key(&row, &[3, 9]), vec![Datum::Null, Datum::Null]);
        assert!(extract_key(&row, &[]).is_empty());
    }

    #[test]
    fn null_classes() {
        let n = Datum::Null;
        let v = Datum::Int4(1);
        assert_eq!(null_class(&[v.clone(), v.clone()]), NullClass::NoneNull);
        assert_eq!(null_class(&[n.clone(), n.clone()]), NullClass::AllNull);
        assert_eq!(null_class(&[n, v]), NullClass::Mixed);
        assert_eq!(null_class(&[]), NullClass::NoneNull);
    }

    #[test]
    fn images_numeric_scale_differs() {
        let (a, b) = (num("1.0"), num("1.00"));
        assert_eq!(cmp_datum(&a, &b), Ordering::Equal);
        assert!(!images_equal(
            std::slice::from_ref(&a),
            std::slice::from_ref(&b)
        ));
        assert!(keys_equal(std::slice::from_ref(&a), &[b]));
        assert!(images_equal(
            std::slice::from_ref(&a),
            std::slice::from_ref(&a)
        ));
    }

    #[test]
    fn images_bpchar_and_float() {
        assert!(!images_equal(
            &[Datum::BpChar("a ".into())],
            &[Datum::BpChar("a".into())]
        ));
        assert!(!images_equal(&[Datum::Float8(0.0)], &[Datum::Float8(-0.0)]));
        assert!(images_equal(
            &[Datum::Float8(f64::NAN)],
            &[Datum::Float8(f64::NAN)]
        ));
        assert!(images_equal(&[Datum::Null], &[Datum::Null]));
        assert!(!images_equal(&[Datum::Null], &[Datum::Int4(1)]));
        assert!(!images_equal(&[Datum::Int4(1)], &[Datum::Int8(1)]));
        assert!(!images_equal(&[Datum::Int4(1)], &[]));
    }

    #[test]
    fn permute_reorders() {
        let key = vec![Datum::Int4(10), Datum::Int4(20), Datum::Int4(30)];
        assert_eq!(
            permute_key(&key, &[2, 0, 1]),
            vec![Datum::Int4(30), Datum::Int4(10), Datum::Int4(20)]
        );
    }

    #[test]
    fn child_to_parent_probe() {
        let env = TypeEnv::default();
        let cmps = [
            RiKeyCmp::IntCross,
            RiKeyCmp::CastChild {
                from: SqlType::INT4,
                to: SqlType::NUMERIC,
            },
        ];
        let out =
            child_key_to_parent(&[Datum::Int8(5), Datum::Int4(7)], &cmps, &env).expect("cast");
        assert_eq!(out[0], Datum::Int8(5));
        assert_eq!(cmp_datum(&out[1], &num("7")), Ordering::Equal);
        assert!(matches!(out[1], Datum::Numeric(_)));
        let nulls = child_key_to_parent(&[Datum::Null, Datum::Null], &cmps, &env).expect("null");
        assert!(nulls.iter().all(Datum::is_null));
    }
}
