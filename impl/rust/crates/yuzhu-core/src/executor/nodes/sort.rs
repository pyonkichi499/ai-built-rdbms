//! Executor node: sort (materializes the input, stable sort).
//!
//! Each key compares with `cmp_datum` (C collation for text, NaN largest,
//! `-0 = +0`); NULLs go first or last as the key says (PostgreSQL default:
//! `ASC NULLS LAST`, `DESC NULLS FIRST`).

use std::cmp::Ordering;

use crate::error::Result;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::SortKey;
use crate::types::{Datum, Row, cmp_datum};

pub struct SortExec {
    input: BoxedExecutor,
    keys: Vec<SortKey>,
    sorted: Option<std::vec::IntoIter<Row>>,
}

impl std::fmt::Debug for SortExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortExec")
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl SortExec {
    pub fn new(input: BoxedExecutor, keys: Vec<SortKey>) -> Self {
        SortExec {
            input,
            keys,
            sorted: None,
        }
    }

    fn materialize(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Vec<Row>> {
        let mut entries: Vec<(Vec<Datum>, Row)> = Vec::new();
        while let Some(row) = self.input.next(ctx)? {
            let key = self
                .keys
                .iter()
                .map(|k| eval(&k.expr, &row, ctx))
                .collect::<Result<Vec<_>>>()?;
            entries.push((key, row));
        }
        let keys = &self.keys;
        entries.sort_by(|(a, _), (b, _)| compare_keys(keys, a, b));
        Ok(entries.into_iter().map(|(_, r)| r).collect())
    }
}

/// Compares two key tuples according to `keys`.
pub fn compare_keys(keys: &[SortKey], a: &[Datum], b: &[Datum]) -> Ordering {
    for ((k, x), y) in keys.iter().zip(a).zip(b) {
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if k.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if k.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = cmp_datum(x, y);
                if k.descending { o.reverse() } else { o }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

impl Executor for SortExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.sorted.is_none() {
            let rows = self.materialize(ctx)?;
            self.sorted = Some(rows.into_iter());
        }
        Ok(self.sorted.as_mut().and_then(Iterator::next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{col, int, lit, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::SqlType;

    fn key(index: usize, ty: SqlType, descending: bool, nulls_first: bool) -> SortKey {
        SortKey {
            expr: col(index, ty),
            descending,
            nulls_first,
        }
    }

    fn sorted(rows: Vec<Vec<crate::analyzer::BoundExpr>>, keys: Vec<SortKey>) -> Vec<Row> {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(SortExec::new(Box::new(ValuesExec::new(rows)), keys));
        f.run(&mut e).unwrap()
    }

    fn ints(rows: &[Row]) -> Vec<Option<i64>> {
        rows.iter().map(|r| r[0].as_i64()).collect()
    }

    #[test]
    fn null_ordering_defaults_and_overrides() {
        let rows = || {
            vec![
                vec![int(2)],
                vec![null(SqlType::INT4)],
                vec![int(1)],
                vec![int(3)],
            ]
        };
        let t = SqlType::INT4;
        assert_eq!(
            ints(&sorted(rows(), vec![key(0, t, false, false)])),
            vec![Some(1), Some(2), Some(3), None]
        );
        assert_eq!(
            ints(&sorted(rows(), vec![key(0, t, true, true)])),
            vec![None, Some(3), Some(2), Some(1)]
        );
        assert_eq!(
            ints(&sorted(rows(), vec![key(0, t, false, true)])),
            vec![None, Some(1), Some(2), Some(3)]
        );
        assert_eq!(
            ints(&sorted(rows(), vec![key(0, t, true, false)])),
            vec![Some(3), Some(2), Some(1), None]
        );
    }

    #[test]
    fn multi_key_stable_bytes_and_nan() {
        let rows = vec![
            vec![text("b"), int(1)],
            vec![text("B"), int(2)],
            vec![text("a"), int(3)],
            vec![text("b"), int(0)],
        ];
        let out = sorted(
            rows,
            vec![
                key(0, SqlType::TEXT, false, false),
                key(1, SqlType::INT4, true, true),
            ],
        );
        let got: Vec<(String, i64)> = out
            .iter()
            .map(|r| (r[0].as_str().unwrap().to_owned(), r[1].as_i64().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("B".into(), 2),
                ("a".into(), 3),
                ("b".into(), 1),
                ("b".into(), 0)
            ]
        );
        let f = |v: f64| lit(Datum::Float8(v), SqlType::FLOAT8);
        let out = sorted(
            vec![
                vec![f(f64::NAN)],
                vec![f(1.0)],
                vec![f(f64::INFINITY)],
                vec![f(-0.0)],
            ],
            vec![key(0, SqlType::FLOAT8, false, false)],
        );
        let got: Vec<f64> = out.iter().map(|r| r[0].as_f64().unwrap()).collect();
        assert!(got[3].is_nan());
        assert_eq!(&got[..3], &[0.0, 1.0, f64::INFINITY]);
        // Stability: equal keys keep input order.
        let out = sorted(
            vec![
                vec![int(1), int(10)],
                vec![int(0), int(20)],
                vec![int(1), int(30)],
            ],
            vec![key(0, SqlType::INT4, false, false)],
        );
        assert_eq!(
            out.iter()
                .map(|r| r[1].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![20, 10, 30]
        );
    }
}
