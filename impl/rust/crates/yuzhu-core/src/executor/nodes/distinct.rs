//! Executor node: distinct (keeps the first occurrence of each row, so the
//! input order — e.g. from a Sort below — is preserved).
//!
//! Rows are equal when every column is equal by `cmp_datum` semantics;
//! NULLs are equal to each other (`IS NOT DISTINCT FROM`).

use std::collections::HashSet;

use crate::error::Result;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::types::{Datum, Row};

pub struct DistinctExec {
    input: BoxedExecutor,
    seen: HashSet<Vec<KeyDatum>>,
}

impl std::fmt::Debug for DistinctExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistinctExec")
            .field("seen", &self.seen.len())
            .finish_non_exhaustive()
    }
}

/// A hashable image of a datum consistent with `cmp_datum` equality:
/// integers widen to i64, floats normalize NaN and `-0`.
#[derive(Hash, PartialEq, Eq, Debug)]
enum KeyDatum {
    Null,
    Bool(bool),
    Int(i64),
    Float(u64),
    Numeric(yuzhu_numeric::Numeric),
    Text(String),
    /// oid, xid, cid, "char", tid and oidvector: a tag and the words.
    Words(u8, Vec<u64>),
}

fn key_of(d: &Datum) -> KeyDatum {
    let float = |v: f64| {
        if v.is_nan() {
            KeyDatum::Float(f64::NAN.to_bits())
        } else if v == 0.0 {
            KeyDatum::Float(0f64.to_bits())
        } else {
            KeyDatum::Float(v.to_bits())
        }
    };
    match d {
        Datum::Null => KeyDatum::Null,
        Datum::Bool(b) => KeyDatum::Bool(*b),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => {
            KeyDatum::Int(d.as_i64().unwrap_or_default())
        }
        Datum::Float4(v) => float(f64::from(*v)),
        Datum::Float8(v) => float(*v),
        Datum::Numeric(n) => KeyDatum::Numeric(n.clone()),
        Datum::Text(s) => KeyDatum::Text(s.clone()),
        Datum::Oid(v) => KeyDatum::Words(0, vec![u64::from(*v)]),
        Datum::Xid(v) => KeyDatum::Words(1, vec![u64::from(*v)]),
        Datum::Cid(v) => KeyDatum::Words(2, vec![u64::from(*v)]),
        Datum::Char(v) => KeyDatum::Words(3, vec![u64::from(*v)]),
        Datum::Tid(t) => KeyDatum::Words(4, vec![u64::from(t.block), u64::from(t.offset)]),
        Datum::OidVector(v) => KeyDatum::Words(5, v.iter().map(|x| u64::from(*x)).collect()),
        // NULL 要素は u32 に収まらない値で表す。
        Datum::Int4Array(v) => KeyDatum::Words(
            6,
            v.iter()
                .map(|e| e.map_or(u64::MAX, |x| u64::from(x.cast_unsigned())))
                .collect(),
        ),
        Datum::Void => KeyDatum::Words(7, vec![]),
    }
}

impl DistinctExec {
    pub fn new(input: BoxedExecutor) -> Self {
        DistinctExec {
            input,
            seen: HashSet::new(),
        }
    }
}

impl Executor for DistinctExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            if self.seen.insert(row.iter().map(key_of).collect()) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, lit, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::SqlType;

    #[test]
    fn removes_duplicates_keeping_order() {
        let mut f = Fixture::new();
        let fl = |v: f64| lit(Datum::Float8(v), SqlType::FLOAT8);
        let input = Box::new(ValuesExec::new(vec![
            vec![int(2), text("a")],
            vec![int(1), null(SqlType::TEXT)],
            vec![int(2), text("a")],
            vec![int(1), null(SqlType::TEXT)],
            vec![int(2), text("b")],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Int4(2), Datum::Text("a".into())],
                vec![Datum::Int4(1), Datum::Null],
                vec![Datum::Int4(2), Datum::Text("b".into())],
            ]
        );
        let input = Box::new(ValuesExec::new(vec![
            vec![fl(0.0)],
            vec![fl(-0.0)],
            vec![fl(f64::NAN)],
            vec![fl(-f64::NAN)],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        assert_eq!(f.run(&mut e).unwrap().len(), 2);
    }
}
