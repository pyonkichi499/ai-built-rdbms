//! 配列の要素に要る型の性質（`m5/05-types-core.md` §4.1、TY-1）。
//!
//! `catalog::builtin::TYPES` の行から引く。静的表だけを見るので DB には問い合わせない
//! （`types` は `catalog` より下の層だが、`type_display_name` と同じく静的表を引く既存の逆向きの依存）。

use super::{Oid, oid};
use crate::catalog::builtin;
use crate::error::{Error, Result};

/// 配列型の種別。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArrayKind {
    /// `_int4` など。値は `Datum::Array`。
    Regular,
    /// `int2vector`（elem = 21）と `oidvector`（elem = 26）。値は `Int2Vector` / `OidVector`。添字の下限は 0。
    Vector,
}

/// `typcategory = 'A'` かつ `typelem != 0` の型の種別（それ以外は `None`）。
pub fn array_kind(type_oid: Oid) -> Option<ArrayKind> {
    let t = builtin::type_by_oid(type_oid)?;
    if t.category != 'A' || t.elem == 0 {
        return None;
    }
    Some(if matches!(type_oid, oid::INT2VECTOR | oid::OIDVECTOR) {
        ArrayKind::Vector
    } else {
        ArrayKind::Regular
    })
}

/// `get_element_type`: Regular と Vector の要素型。
pub fn element_type(type_oid: Oid) -> Option<Oid> {
    array_kind(type_oid)?;
    builtin::type_by_oid(type_oid).map(|t| t.elem)
}

/// `get_array_type`: `typarray`（行がなければ `None`。`pg_node_tree` や `unknown` は `None`）。
pub fn array_type_of(elem: Oid) -> Option<Oid> {
    let t = builtin::type_by_oid(elem)?;
    (t.array_oid != 0 && builtin::type_by_oid(t.array_oid).is_some()).then_some(t.array_oid)
}

/// 配列の要素にできる型（§3.3 の表の型）。配列型・疑似型・NULL 専用型・`int2vector` / `oidvector` は false。
/// `time` と `interval` は行を足す TD の完了前でも true（`elem_info` は行がなければエラー）。
pub fn is_array_element_type(elem: Oid) -> bool {
    matches!(
        elem,
        oid::BOOL
            | oid::CHAR
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::NUMERIC
            | oid::TEXT
            | oid::VARCHAR
            | oid::BPCHAR
            | oid::BYTEA
            | oid::UUID
            | oid::NAME
            | oid::OID
            | oid::REGPROC
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::XID
            | oid::CID
            | oid::TID
            | oid::DATE
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::INTERVAL
    )
}

/// ユーザーテーブルの列に使えない配列型か（TY-D4。Regular の配列型）。
pub fn is_array_type(type_oid: Oid) -> bool {
    array_kind(type_oid) == Some(ArrayKind::Regular)
}

/// 配列の要素の符号化に要る性質。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ElemInfo {
    pub oid: Oid,
    pub typlen: i16,
    pub byval: bool,
    /// 1、2、4、8（`typalign` の `c` `s` `i` `d`）。
    pub align: u8,
}

/// `typalign` の文字からバイト数。
pub fn align_bytes(typalign: char) -> u8 {
    match typalign {
        'c' => 1,
        's' => 2,
        'i' => 4,
        _ => 8,
    }
}

/// `is_array_element_type` でない型は `Error::internal`（呼び出し側の誤り）。
pub fn elem_info(elem: Oid) -> Result<ElemInfo> {
    if !is_array_element_type(elem) {
        return Err(Error::internal(format!(
            "type {elem} cannot be an array element"
        )));
    }
    let t = builtin::type_by_oid(elem)
        .ok_or_else(|| Error::internal(format!("no pg_type row for array element type {elem}")))?;
    Ok(ElemInfo {
        oid: elem,
        typlen: t.typlen,
        byval: t.typbyval,
        align: align_bytes(t.align),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_element_types() {
        assert_eq!(array_kind(1007), Some(ArrayKind::Regular));
        assert_eq!(array_kind(oid::INT2VECTOR), Some(ArrayKind::Vector));
        assert_eq!(array_kind(oid::OIDVECTOR), Some(ArrayKind::Vector));
        assert_eq!(array_kind(oid::INT4), None);
        assert_eq!(array_kind(oid::ANYARRAY), None);
        assert_eq!(element_type(1007), Some(oid::INT4));
        assert_eq!(element_type(oid::INT2VECTOR), Some(oid::INT2));
        assert_eq!(element_type(oid::OIDVECTOR), Some(oid::OID));
        assert_eq!(element_type(oid::INT4), None);
        assert_eq!(element_type(oid::BYTEA_ARRAY), Some(oid::BYTEA));
        assert!(is_array_type(1009) && !is_array_type(oid::INT2VECTOR) && !is_array_type(25));
    }

    #[test]
    fn array_type_of_follows_typarray() {
        assert_eq!(array_type_of(oid::INT4), Some(1007));
        assert_eq!(array_type_of(oid::TEXT), Some(1009));
        assert_eq!(array_type_of(oid::BYTEA), Some(1001));
        assert_eq!(array_type_of(oid::UUID), Some(2951));
        assert_eq!(array_type_of(oid::NUMERIC), Some(1231));
        assert_eq!(array_type_of(oid::PG_NODE_TREE), None);
        assert_eq!(array_type_of(oid::UNKNOWN), None);
        assert_eq!(array_type_of(oid::ANYARRAY), None);
    }

    #[test]
    fn regular_arrays_round_trip_through_element_type() {
        let mut n = 0;
        for t in builtin::TYPES {
            if array_kind(t.oid) == Some(ArrayKind::Regular) {
                let e = element_type(t.oid).unwrap();
                assert_eq!(array_type_of(e), Some(t.oid), "{}", t.name);
                n += 1;
            }
        }
        // time / interval の行（TD）が入ると 31 になる。
        assert!(n >= 29, "{n}");
    }

    #[test]
    fn elem_info_matches_types_rows() {
        for (e, len, byval, align) in [
            (oid::BOOL, 1, true, 1),
            (oid::CHAR, 1, true, 1),
            (oid::INT2, 2, true, 2),
            (oid::INT4, 4, true, 4),
            (oid::FLOAT4, 4, true, 4),
            (oid::DATE, 4, true, 4),
            (oid::INT8, 8, true, 8),
            (oid::FLOAT8, 8, true, 8),
            (oid::TIMESTAMP, 8, true, 8),
            (oid::TIMESTAMPTZ, 8, true, 8),
            (oid::OID, 4, true, 4),
            (oid::REGPROC, 4, true, 4),
            (oid::REGCLASS, 4, true, 4),
            (oid::REGTYPE, 4, true, 4),
            (oid::XID, 4, true, 4),
            (oid::CID, 4, true, 4),
            (oid::TID, 6, false, 2),
            (oid::NAME, 64, false, 1),
            (oid::UUID, 16, false, 1),
            (oid::INTERVAL, 16, false, 8),
            (oid::TEXT, -1, false, 4),
            (oid::VARCHAR, -1, false, 4),
            (oid::BPCHAR, -1, false, 4),
            (oid::BYTEA, -1, false, 4),
            (oid::NUMERIC, -1, false, 4),
        ] {
            let i = elem_info(e).unwrap();
            assert_eq!((i.typlen, i.byval, i.align), (len, byval, align), "{e}");
        }
        assert!(elem_info(oid::INT2VECTOR).is_err());
        assert!(elem_info(1007).is_err());
        assert!(elem_info(oid::ANYARRAY).is_err());
    }
}
