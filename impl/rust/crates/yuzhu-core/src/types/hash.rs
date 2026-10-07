//! 値のハッシュ（`m4/00-contracts.md` §12.2、`m4/05` §3.5）。ハッシュ結合・ハッシュ集約・DISTINCT・
//! 集合演算・ハッシュ化 `SubPlan` のキーが共有する。
//!
//! `cmp_datum` が `Equal` を返す 2 値は同じハッシュになる: 整数は幅によらず i64、浮動小数は
//! f64 に広げて NaN どうしと `-0` / `+0` を正規化、`bpchar` は末尾の空白を無視、numeric は
//! `yuzhu_numeric::Numeric` の `Hash`（表示桁数を無視して正規化済み。`1.10` と `1.1` が等しい）、
//! 日時は内部表現の整数（±infinity は番兵値のまま）。比較は同じ型にそろえてから行う前提
//! （整数と浮動小数のように型をまたいで `cmp_datum` が `Equal` になる組は同じハッシュにならない）。

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use super::{Datum, cmp_datum};

/// 浮動小数の正規化されたビット列（NaN は 1 通り、`-0` は `+0`）。
fn float_bits(v: f64) -> u64 {
    if v.is_nan() {
        f64::NAN.to_bits()
    } else if v == 0.0 {
        0f64.to_bits()
    } else {
        v.to_bits()
    }
}

/// `d` を `state` に混ぜる。NULL は固定のタグだけ（NULL どうしは等しい）。
pub fn hash_datum(d: &Datum, state: &mut dyn Hasher) {
    // `&mut dyn Hasher` 自身が `Hasher` を実装する。
    let mut h = state;
    // 変種の「型の族」ごとにタグを付けて、別の族の値が偶然同じバイト列にならないようにする。
    match d {
        Datum::Null => 0u8.hash(&mut h),
        Datum::Bool(b) => (1u8, *b).hash(&mut h),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => {
            (2u8, d.as_i64().unwrap_or_default()).hash(&mut h);
        }
        Datum::Float4(v) => (3u8, float_bits(f64::from(*v))).hash(&mut h),
        Datum::Float8(v) => (3u8, float_bits(*v)).hash(&mut h),
        Datum::Numeric(n) => {
            4u8.hash(&mut h);
            n.hash(&mut h);
        }
        Datum::Text(s) => {
            5u8.hash(&mut h);
            s.hash(&mut h);
        }
        // oid / regproc / xid / cid / "char" は型ごとに別のタグ（cmp_datum も同じ変種どうしだけ比較する）。
        Datum::Oid(v) => (6u8, *v).hash(&mut h),
        Datum::Xid(v) => (7u8, *v).hash(&mut h),
        Datum::Cid(v) => (8u8, *v).hash(&mut h),
        Datum::Char(v) => (9u8, *v).hash(&mut h),
        Datum::Tid(t) => (10u8, t.block, t.offset).hash(&mut h),
        Datum::OidVector(v) => {
            11u8.hash(&mut h);
            v.hash(&mut h);
        }
        Datum::Int4Array(v) => {
            12u8.hash(&mut h);
            v.hash(&mut h);
        }
        Datum::Void => 13u8.hash(&mut h),
        // 末尾の空白は等値判定に影響しない（bpchar_ops）。
        Datum::BpChar(s) => {
            14u8.hash(&mut h);
            s.trim_end_matches(' ').hash(&mut h);
        }
        Datum::Date(v) => (15u8, v.0).hash(&mut h),
        Datum::Timestamp(v) => (16u8, v.0).hash(&mut h),
        Datum::TimestampTz(v) => (17u8, v.0).hash(&mut h),
        Datum::Int2Vector(v) => {
            18u8.hash(&mut h);
            v.hash(&mut h);
        }
    }
}

/// ハッシュ表のキー。`Eq` は各要素が `cmp_datum == Equal`（NULL どうしも等しい）、`Hash` は
/// `hash_datum`。結合キーの NULL は一致しないので、executor が NULL を含む組を入れる前に弾く。
#[derive(Clone, Debug)]
pub struct HashKey(pub Vec<Datum>);

impl PartialEq for HashKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|(a, b)| cmp_datum(a, b) == Ordering::Equal)
    }
}

impl Eq for HashKey {}

impl Hash for HashKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.len().hash(state);
        for d in &self.0 {
            hash_datum(d, state);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::hash::DefaultHasher;

    use super::*;

    fn h(d: &Datum) -> u64 {
        let mut s = DefaultHasher::new();
        hash_datum(d, &mut s);
        s.finish()
    }

    #[test]
    fn equal_values_hash_equal() {
        assert_eq!(h(&Datum::Int2(5)), h(&Datum::Int8(5)));
        assert_eq!(h(&Datum::Int4(-1)), h(&Datum::Int8(-1)));
        assert_eq!(h(&Datum::Float8(-0.0)), h(&Datum::Float8(0.0)));
        assert_eq!(h(&Datum::Float8(f64::NAN)), h(&Datum::Float4(f32::NAN)));
        assert_eq!(h(&Datum::Float4(1.5)), h(&Datum::Float8(1.5)));
        assert_eq!(
            h(&Datum::BpChar("ab  ".into())),
            h(&Datum::BpChar("ab".into()))
        );
        assert_eq!(h(&Datum::Null), h(&Datum::Null));
    }

    #[test]
    fn different_values_hash_differently() {
        assert_ne!(h(&Datum::Int4(1)), h(&Datum::Int4(2)));
        assert_ne!(h(&Datum::Text("a".into())), h(&Datum::Text("b".into())));
        // text の末尾の空白は意味を持つ。
        assert_ne!(h(&Datum::Text("a ".into())), h(&Datum::Text("a".into())));
        assert_ne!(h(&Datum::Null), h(&Datum::Bool(false)));
    }

    #[test]
    fn hash_key_follows_cmp_datum_equality() {
        let mut set = HashSet::new();
        assert!(set.insert(HashKey(vec![Datum::Int4(1), Datum::Null])));
        assert!(!set.insert(HashKey(vec![Datum::Int8(1), Datum::Null])));
        assert!(set.insert(HashKey(vec![Datum::Int8(1), Datum::Text("x".into())])));
        assert!(set.insert(HashKey(vec![Datum::Int8(1)])));
        let f = |v| HashKey(vec![Datum::Float8(v)]);
        let mut fs = HashSet::new();
        assert!(fs.insert(f(0.0)));
        assert!(!fs.insert(f(-0.0)));
        assert!(fs.insert(f(f64::NAN)));
        assert!(!fs.insert(f(f64::NAN)));
    }
}

#[cfg(test)]
mod property_tests {
    use std::collections::{HashMap, HashSet};
    use std::hash::DefaultHasher;

    use yuzhu_datetime::{Date, Timestamp, TimestampTz};
    use yuzhu_numeric::Numeric;

    use super::*;
    use crate::types::Tid;

    fn hash_of(d: &Datum) -> u64 {
        let mut s = DefaultHasher::new();
        hash_datum(d, &mut s);
        s.finish()
    }

    fn key_hash(k: &HashKey) -> u64 {
        let mut s = DefaultHasher::new();
        k.hash(&mut s);
        s.finish()
    }

    fn num(s: &str) -> Datum {
        Datum::Numeric(Numeric::parse(s).unwrap())
    }

    /// 変種ごとの値の一覧（同じ変種の中と、整数どうし・浮動小数どうしの幅違いを含む）。
    fn values() -> Vec<Datum> {
        vec![
            Datum::Null,
            Datum::Bool(false),
            Datum::Bool(true),
            Datum::Int2(0),
            Datum::Int2(5),
            Datum::Int4(0),
            Datum::Int4(5),
            Datum::Int4(-1),
            Datum::Int8(5),
            Datum::Int8(-1),
            Datum::Int8(i64::MAX),
            Datum::Int8(i64::from(i32::MAX)),
            Datum::Int4(i32::MAX),
            Datum::Float4(0.0),
            Datum::Float4(-0.0),
            Datum::Float4(1.5),
            Datum::Float4(f32::NAN),
            Datum::Float4(f32::INFINITY),
            Datum::Float8(0.0),
            Datum::Float8(-0.0),
            Datum::Float8(1.5),
            Datum::Float8(f64::NAN),
            Datum::Float8(-f64::NAN),
            Datum::Float8(f64::INFINITY),
            Datum::Float8(f64::NEG_INFINITY),
            num("1.10"),
            num("1.1"),
            num("1.100"),
            num("0"),
            num("0.00"),
            num("-0"),
            num("10"),
            num("1e1"),
            num("NaN"),
            num("Infinity"),
            num("-Infinity"),
            Datum::Text(String::new()),
            Datum::Text("a".into()),
            Datum::Text("a ".into()),
            Datum::Text("b".into()),
            Datum::BpChar(String::new()),
            Datum::BpChar(" ".into()),
            Datum::BpChar("a".into()),
            Datum::BpChar("a  ".into()),
            Datum::BpChar("b".into()),
            Datum::Date(Date(0)),
            Datum::Date(Date(1)),
            Datum::Date(Date::INFINITY),
            Datum::Date(Date::NEG_INFINITY),
            Datum::Timestamp(Timestamp(0)),
            Datum::Timestamp(Timestamp(1)),
            Datum::Timestamp(Timestamp::INFINITY),
            Datum::Timestamp(Timestamp::NEG_INFINITY),
            Datum::TimestampTz(TimestampTz(0)),
            Datum::TimestampTz(TimestampTz(1)),
            Datum::Oid(1),
            Datum::Oid(2),
            Datum::Xid(1),
            Datum::Cid(1),
            Datum::Char(b'a'),
            Datum::Tid(Tid {
                block: 0,
                offset: 1,
            }),
            Datum::Tid(Tid {
                block: 1,
                offset: 0,
            }),
            Datum::OidVector(vec![]),
            Datum::OidVector(vec![1, 2]),
            Datum::Int2Vector(vec![1, 2]),
            Datum::Int4Array(vec![Some(1), None]),
            Datum::Int4Array(vec![Some(1)]),
            Datum::Int4Array(vec![]),
            Datum::Void,
        ]
    }

    /// 同じ型の族の中だけで、`cmp_datum == Equal` なら同じハッシュ（型をまたぐ比較は前提の外。§3.5）。
    fn same_family(a: &Datum, b: &Datum) -> bool {
        use Datum::{Float4, Float8, Int2, Int4, Int8};
        match (a, b) {
            (Int2(_) | Int4(_) | Int8(_), Int2(_) | Int4(_) | Int8(_))
            | (Float4(_) | Float8(_), Float4(_) | Float8(_)) => true,
            // `cmp_datum` は `int4[]` の中身を比べない（同じ変種どうしを Equal とする）ので、性質の対象外にする。
            (Datum::Int4Array(_), _) => false,
            _ => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }

    #[test]
    fn cmp_equal_implies_same_hash_for_all_pairs() {
        let vals = values();
        let mut checked = 0;
        let mut equal_pairs = 0;
        for a in &vals {
            for b in &vals {
                if !same_family(a, b) {
                    continue;
                }
                checked += 1;
                if cmp_datum(a, b) == Ordering::Equal {
                    equal_pairs += 1;
                    assert_eq!(hash_of(a), hash_of(b), "{a:?} vs {b:?}");
                }
            }
        }
        assert!(
            checked > 100 && equal_pairs > vals.len(),
            "{checked} {equal_pairs}"
        );
    }

    #[test]
    fn named_equalities_and_differences() {
        assert_eq!(hash_of(&num("1.10")), hash_of(&num("1.1")));
        assert_eq!(hash_of(&num("1.100")), hash_of(&num("1.1")));
        assert_eq!(hash_of(&num("0.00")), hash_of(&num("0")));
        assert_eq!(hash_of(&num("-0")), hash_of(&num("0")));
        assert_eq!(hash_of(&num("1e1")), hash_of(&num("10")));
        assert_eq!(hash_of(&num("NaN")), hash_of(&num("NaN")));
        assert_ne!(hash_of(&num("1.1")), hash_of(&num("1.2")));
        assert_ne!(hash_of(&num("Infinity")), hash_of(&num("-Infinity")));
        assert_eq!(
            hash_of(&Datum::BpChar("a  ".into())),
            hash_of(&Datum::BpChar("a".into()))
        );
        assert_eq!(
            hash_of(&Datum::Date(Date::INFINITY)),
            hash_of(&Datum::Date(Date::INFINITY))
        );
        assert_ne!(
            hash_of(&Datum::Date(Date::INFINITY)),
            hash_of(&Datum::Date(Date::NEG_INFINITY))
        );
        assert_ne!(
            hash_of(&Datum::Int4Array(vec![None])),
            hash_of(&Datum::Int4Array(vec![Some(0)]))
        );
        assert_ne!(
            hash_of(&Datum::Int4Array(vec![Some(1), None])),
            hash_of(&Datum::Int4Array(vec![Some(1)]))
        );
        assert_ne!(
            hash_of(&Datum::Tid(Tid {
                block: 0,
                offset: 1
            })),
            hash_of(&Datum::Tid(Tid {
                block: 1,
                offset: 0
            }))
        );
    }

    #[test]
    fn hash_key_eq_and_hash_agree() {
        let vals = values();
        for a in &vals {
            for b in &vals {
                if !same_family(a, b) {
                    continue;
                }
                let (ka, kb) = (HashKey(vec![a.clone()]), HashKey(vec![b.clone()]));
                if ka == kb {
                    assert_eq!(key_hash(&ka), key_hash(&kb), "{a:?} vs {b:?}");
                }
                assert_eq!(ka == kb, cmp_datum(a, b) == Ordering::Equal);
            }
        }
        // 長さが違えば等しくない。NULL どうしは等しい。
        assert_ne!(
            HashKey(vec![Datum::Null]),
            HashKey(vec![Datum::Null, Datum::Null])
        );
        assert_eq!(HashKey(vec![Datum::Null]), HashKey(vec![Datum::Null]));
        // HashMap のキーとして使える。
        let mut m: HashMap<HashKey, usize> = HashMap::new();
        for (i, v) in [num("1.10"), num("1.1"), num("2")].into_iter().enumerate() {
            *m.entry(HashKey(vec![v])).or_default() += i + 1;
        }
        assert_eq!(m.len(), 2);
        assert_eq!(m[&HashKey(vec![num("1.1")])], 3);
        let mut s = HashSet::new();
        assert!(s.insert(HashKey(vec![Datum::BpChar("x ".into())])));
        assert!(!s.insert(HashKey(vec![Datum::BpChar("x".into())])));
    }
}
