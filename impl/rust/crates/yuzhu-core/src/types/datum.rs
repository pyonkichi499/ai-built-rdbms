//! Runtime values and their ordering.

use std::cmp::Ordering;

/// A runtime value. A `Datum` does not carry its SQL type; the type lives
/// on the expression / schema (`SqlType`).
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    /// `numeric`.
    Numeric(yuzhu_numeric::Numeric),
    /// Shared by text, varchar, unknown, name and `pg_node_tree`.
    Text(String),
    /// `oid` and `regproc`.
    Oid(u32),
    /// `"char"`.
    Char(u8),
    /// `xid` (the lower 32 bits of the 64-bit XID).
    Xid(u32),
    /// `cid`.
    Cid(u32),
    /// `tid`.
    Tid(super::Tid),
    /// `oidvector`.
    OidVector(Vec<u32>),
    /// `int4[]` (一次元だけ。M3 でテキスト入出力だけ持つ)。
    Int4Array(Vec<Option<i32>>),
    /// `void`（出力は空文字列）。
    Void,
}

/// A row of values, in column order.
pub type Row = Vec<Datum>;

impl Datum {
    pub fn is_null(&self) -> bool {
        matches!(self, Datum::Null)
    }

    /// `Some(b)` for `Bool(b)`, `None` for NULL or non-bool values.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Datum::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Datum::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Integer value widened to i64 (for any integer variant).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Datum::Int2(v) => Some(i64::from(*v)),
            Datum::Int4(v) => Some(i64::from(*v)),
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    }

    /// Float value widened to f64 (for float variants).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Datum::Float4(v) => Some(f64::from(*v)),
            Datum::Float8(v) => Some(*v),
            _ => None,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Datum::Bool(_) => 0,
            Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => 1,
            Datum::Float4(_) | Datum::Float8(_) => 2,
            Datum::Numeric(_) => 13,
            Datum::Text(_) => 3,
            Datum::Oid(_) => 5,
            Datum::Char(_) => 6,
            Datum::Xid(_) => 7,
            Datum::Cid(_) => 8,
            Datum::Tid(_) => 9,
            Datum::OidVector(_) => 10,
            Datum::Int4Array(_) => 11,
            Datum::Void => 12,
            Datum::Null => 4,
        }
    }
}

/// PostgreSQL float comparison (`float8_cmp_internal`): NaN is larger than
/// every other value and equal to itself; `-0 == +0`.
pub fn cmp_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        // Neither is NaN, so partial_cmp is total here (and -0 == +0).
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Compares two values of the same type (sort order of PostgreSQL's btree
/// opclasses). Text compares by bytes (C collation).
///
/// NULL handling belongs to the caller (NULLS FIRST/LAST); if a NULL does
/// reach this function it sorts after everything else, and two NULLs are
/// equal. Values of different integer widths, or different float widths,
/// or integer vs. float, are compared numerically; other cross-type pairs
/// (a bug in the caller) are ordered by a fixed type rank instead of
/// panicking.
pub fn cmp_datum(a: &Datum, b: &Datum) -> Ordering {
    use Datum::{Bool, Char, Cid, Float4, Float8, Oid, OidVector, Text, Tid, Xid};
    match (a, b) {
        // Unsigned comparison (oid, xid and cid are u32).
        (Oid(x), Oid(y)) | (Xid(x), Xid(y)) | (Cid(x), Cid(y)) => x.cmp(y),
        (Char(x), Char(y)) => x.cmp(y),
        (Tid(x), Tid(y)) => x.cmp(y),
        (OidVector(x), OidVector(y)) => x.cmp(y),
        (Bool(x), Bool(y)) => x.cmp(y),
        (Text(x), Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Float4(x), Float4(y)) => cmp_f64(f64::from(*x), f64::from(*y)),
        (Float8(x), Float8(y)) => cmp_f64(*x, *y),
        (Datum::Numeric(x), Datum::Numeric(y)) => x.cmp(y),
        _ => {
            if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
                return x.cmp(&y);
            }
            let fx = a.as_f64().or_else(|| as_f64_lossy(a));
            let fy = b.as_f64().or_else(|| as_f64_lossy(b));
            if let (Some(x), Some(y)) = (fx, fy) {
                return cmp_f64(x, y);
            }
            a.rank().cmp(&b.rank())
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn as_f64_lossy(d: &Datum) -> Option<f64> {
    d.as_i64().map(|v| v as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Datum::*;

    #[test]
    fn float_semantics() {
        assert_eq!(
            cmp_datum(&Float8(f64::NAN), &Float8(f64::NAN)),
            Ordering::Equal
        );
        assert_eq!(
            cmp_datum(&Float8(f64::NAN), &Float8(f64::INFINITY)),
            Ordering::Greater
        );
        assert_eq!(cmp_datum(&Float8(1.0), &Float8(f64::NAN)), Ordering::Less);
        assert_eq!(cmp_datum(&Float8(-0.0), &Float8(0.0)), Ordering::Equal);
        assert_eq!(
            cmp_datum(&Float4(f32::NAN), &Float4(1.0)),
            Ordering::Greater
        );
        assert_eq!(
            cmp_datum(&Float8(f64::NEG_INFINITY), &Float8(-1e308)),
            Ordering::Less
        );
    }

    #[test]
    fn basic_ordering() {
        assert_eq!(cmp_datum(&Int4(1), &Int4(2)), Ordering::Less);
        assert_eq!(cmp_datum(&Int8(-5), &Int2(-5)), Ordering::Equal);
        assert_eq!(cmp_datum(&Bool(false), &Bool(true)), Ordering::Less);
        // Byte order: uppercase before lowercase, prefix first.
        assert_eq!(
            cmp_datum(&Text("B".into()), &Text("a".into())),
            Ordering::Less
        );
        assert_eq!(
            cmp_datum(&Text("ab".into()), &Text("abc".into())),
            Ordering::Less
        );
        assert_eq!(cmp_datum(&Int4(2), &Float8(1.5)), Ordering::Greater);
        assert_eq!(cmp_datum(&Null, &Int4(1)), Ordering::Greater);
        assert_eq!(cmp_datum(&Null, &Null), Ordering::Equal);
    }

    #[test]
    fn system_type_ordering() {
        // oid / xid / cid compare as unsigned.
        assert_eq!(cmp_datum(&Oid(u32::MAX), &Oid(1)), Ordering::Greater);
        assert_eq!(cmp_datum(&Xid(1), &Xid(2)), Ordering::Less);
        assert_eq!(cmp_datum(&Cid(7), &Cid(7)), Ordering::Equal);
        assert_eq!(cmp_datum(&Char(b'a'), &Char(b'b')), Ordering::Less);
        // tid: block first, then offset.
        let t = |block, offset| Tid(crate::types::Tid { block, offset });
        assert_eq!(cmp_datum(&t(1, 9), &t(2, 1)), Ordering::Less);
        assert_eq!(cmp_datum(&t(2, 3), &t(2, 1)), Ordering::Greater);
        assert_eq!(
            cmp_datum(&OidVector(vec![1, 2]), &OidVector(vec![1, 3])),
            Ordering::Less
        );
    }

    #[test]
    fn accessors() {
        assert!(Null.is_null());
        assert_eq!(Bool(true).as_bool(), Some(true));
        assert_eq!(Int2(3).as_i64(), Some(3));
        assert_eq!(Text("x".into()).as_str(), Some("x"));
        assert_eq!(Float4(0.5).as_f64(), Some(0.5));
    }
}
