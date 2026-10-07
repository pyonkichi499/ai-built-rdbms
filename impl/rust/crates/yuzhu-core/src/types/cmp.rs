//! NULL の順序つきの比較（`m4/00-contracts.md` §12.2）。Sort・B+Tree・Unique が共有する。

use std::cmp::Ordering;

use super::{Datum, cmp_datum};

/// NULL を `nulls_first` に従って先頭か末尾に置き、`descending` は非 NULL どうしの比較だけを反転する。
/// NULL どうしは等しい。
pub fn cmp_with_nulls(a: &Datum, b: &Datum, descending: bool, nulls_first: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let o = cmp_datum(a, b);
            if descending { o.reverse() } else { o }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Datum::{Int4, Null, Text};

    #[test]
    fn null_placement_follows_nulls_first() {
        // ASC NULLS LAST（PostgreSQL の既定）。
        assert_eq!(
            cmp_with_nulls(&Null, &Int4(1), false, false),
            Ordering::Greater
        );
        assert_eq!(
            cmp_with_nulls(&Int4(1), &Null, false, false),
            Ordering::Less
        );
        // NULLS FIRST。
        assert_eq!(cmp_with_nulls(&Null, &Int4(1), false, true), Ordering::Less);
        assert_eq!(
            cmp_with_nulls(&Int4(1), &Null, false, true),
            Ordering::Greater
        );
        assert_eq!(cmp_with_nulls(&Null, &Null, true, true), Ordering::Equal);
    }

    #[test]
    fn new_variants_follow_the_btree_order() {
        use yuzhu_datetime::{Date, Timestamp, TimestampTz};
        let n = |s: &str| Datum::Numeric(yuzhu_numeric::Numeric::parse(s).unwrap());
        // numeric: -Inf < finite < +Inf < NaN; 1.10 = 1.1.
        assert_eq!(cmp_datum(&n("-Infinity"), &n("-1e100")), Ordering::Less);
        assert_eq!(cmp_datum(&n("1e100"), &n("Infinity")), Ordering::Less);
        assert_eq!(cmp_datum(&n("Infinity"), &n("NaN")), Ordering::Less);
        assert_eq!(cmp_datum(&n("NaN"), &n("NaN")), Ordering::Equal);
        assert_eq!(cmp_datum(&n("1.10"), &n("1.1")), Ordering::Equal);
        assert_eq!(
            cmp_with_nulls(&n("2"), &n("10"), false, false),
            Ordering::Less
        );
        assert_eq!(
            cmp_with_nulls(&n("NaN"), &n("1"), true, false),
            Ordering::Less
        );
        // bpchar ignores trailing spaces only.
        let b = |s: &str| Datum::BpChar(s.to_owned());
        assert_eq!(cmp_datum(&b("ab  "), &b("ab")), Ordering::Equal);
        assert_eq!(cmp_datum(&b("ab\t"), &b("ab")), Ordering::Greater);
        assert_eq!(cmp_datum(&b("a b"), &b("a")), Ordering::Greater);
        // int2vector: lexicographic.
        assert_eq!(
            cmp_datum(
                &Datum::Int2Vector(vec![1, 2]),
                &Datum::Int2Vector(vec![1, 3])
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp_datum(&Datum::Int2Vector(vec![1]), &Datum::Int2Vector(vec![1, 0])),
            Ordering::Less
        );
        assert_eq!(
            cmp_datum(
                &Datum::Timestamp(Timestamp(5)),
                &Datum::Timestamp(Timestamp(3))
            ),
            Ordering::Greater
        );
        assert_eq!(
            cmp_datum(
                &Datum::TimestampTz(TimestampTz(-1)),
                &Datum::TimestampTz(TimestampTz(0))
            ),
            Ordering::Less
        );
        assert_eq!(
            cmp_datum(&Datum::Date(Date(1)), &Datum::Date(Date(1))),
            Ordering::Equal
        );
    }

    #[test]
    fn descending_reverses_only_non_null_comparisons() {
        assert_eq!(
            cmp_with_nulls(&Int4(1), &Int4(2), true, false),
            Ordering::Greater
        );
        assert_eq!(
            cmp_with_nulls(&Text("b".into()), &Text("a".into()), true, false),
            Ordering::Less
        );
        // DESC でも NULL の位置は nulls_first だけで決まる。
        assert_eq!(
            cmp_with_nulls(&Null, &Int4(1), true, false),
            Ordering::Greater
        );
        assert_eq!(cmp_with_nulls(&Null, &Int4(1), true, true), Ordering::Less);
    }
}
