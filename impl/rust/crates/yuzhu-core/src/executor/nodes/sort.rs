//! Executor node: sort (materializes the input, stable sort).
//!
//! Each key compares with `types::cmp::cmp_with_nulls` (C collation for text, NaN largest,
//! `-0 = +0`); NULLs go first or last as the key says (PostgreSQL default:
//! `ASC NULLS LAST`, `DESC NULLS FIRST`).
//!
//! `rewind`: `reusable`（部分木が `Param` / `SubLink` に依存しない）なら整列済みの行を読み直し、
//! そうでなければ溜めた行を捨てて（`release`）子を `rewind` し、次の `next` で作り直す
//! （`m4/02` §3.7.1）。
//!
//! メモリ（`m4/05` §5.8・§8.3、D5-22）: 行ごとに `estimate_row_bytes(row) + estimate_row_bytes(key)` を課金する。
//! `rewindable = false`（`rewind` されない位置のノード）なら、返す行を `std::mem::take` で取り出して複製を避け、
//! 最後の行を返した時点で課金を返す。

use std::cmp::Ordering;

use crate::error::Result;
use crate::executor::mem::estimate_row_bytes;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::physical::SortKey;
use crate::types::cmp::cmp_with_nulls;
use crate::types::{Datum, Row};

pub struct SortExec {
    input: BoxedExecutor,
    keys: Vec<SortKey>,
    /// 子と式が `Param` に依存しないなら、`rewind` で溜めた結果を読み直す。
    reusable: bool,
    /// このノードが `rewind` されうるか（`false` なら読み切った時点で行と課金を手放す）。既定は `true`（安全側）。
    rewindable: bool,
    sorted: Option<Vec<Row>>,
    pos: usize,
    /// `ctx.mem` に課金した合計。
    charged: usize,
}

impl std::fmt::Debug for SortExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortExec")
            .field("keys", &self.keys)
            .field("reusable", &self.reusable)
            .field("rewindable", &self.rewindable)
            .finish_non_exhaustive()
    }
}

impl SortExec {
    /// `reusable = false`（安全側）。`executor::build` が `free_params` の結果で決める。
    pub fn new(input: BoxedExecutor, keys: Vec<SortKey>) -> Self {
        SortExec::with_reusable(input, keys, false)
    }

    pub fn with_reusable(input: BoxedExecutor, keys: Vec<SortKey>, reusable: bool) -> Self {
        SortExec {
            input,
            keys,
            reusable,
            rewindable: true,
            sorted: None,
            pos: 0,
            charged: 0,
        }
    }

    /// `rewindable = false`（根と、`rewind` されない祖先だけの位置）にすると、読み切った時点で課金を返す。
    #[must_use]
    pub fn with_rewindable(mut self, rewindable: bool) -> Self {
        self.rewindable = rewindable;
        self
    }

    fn materialize(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Vec<Row>> {
        let mut entries: Vec<(Vec<Datum>, Row)> = Vec::new();
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            let key = self
                .keys
                .iter()
                .map(|k| eval(&k.expr, &row, ctx))
                .collect::<Result<Vec<_>>>()?;
            let bytes = estimate_row_bytes(&row) + estimate_row_bytes(&key);
            ctx.mem.charge(bytes)?;
            self.charged += bytes;
            entries.push((key, row));
        }
        let keys = &self.keys;
        entries.sort_by(|(a, _), (b, _)| compare_keys(keys, a, b));
        Ok(entries.into_iter().map(|(_, r)| r).collect())
    }
}

/// Compares two key tuples according to `keys`.
fn compare_keys(keys: &[SortKey], a: &[Datum], b: &[Datum]) -> Ordering {
    for ((k, x), y) in keys.iter().zip(a).zip(b) {
        let ord = cmp_with_nulls(x, y, k.descending, k.nulls_first);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

impl Executor for SortExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if self.sorted.is_none() {
            let rows = self.materialize(ctx)?;
            self.sorted = Some(rows);
            self.pos = 0;
        }
        let Some(rows) = self.sorted.as_mut() else {
            return Ok(None);
        };
        if self.pos >= rows.len() {
            return Ok(None);
        }
        let row = if self.rewindable {
            rows[self.pos].clone()
        } else {
            std::mem::take(&mut rows[self.pos])
        };
        self.pos += 1;
        if !self.rewindable && self.pos == rows.len() {
            // 最後の行を返した: 行の置き場と課金を手放す（D5-22）。
            self.sorted = Some(Vec::new());
            self.pos = 0;
            ctx.mem.release(std::mem::take(&mut self.charged));
        }
        Ok(Some(row))
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.sorted.is_some() && self.reusable && self.rewindable {
            self.pos = 0;
            return Ok(());
        }
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.sorted = None;
        self.pos = 0;
        self.input.rewind(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{col, int, lit, null, param, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::planner::physical::PhysExpr;
    use crate::types::SqlType;

    fn key(index: usize, ty: SqlType, descending: bool, nulls_first: bool) -> SortKey {
        SortKey {
            expr: col(index, ty),
            descending,
            nulls_first,
        }
    }

    fn sorted(rows: Vec<Vec<PhysExpr>>, keys: Vec<SortKey>) -> Vec<Row> {
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

    fn sort_over(input: BoxedExecutor, reusable: bool) -> BoxedExecutor {
        Box::new(SortExec::with_reusable(
            input,
            vec![key(0, SqlType::INT4, true, false)],
            reusable,
        ))
    }

    #[test]
    fn rewind_replays_same_rows() {
        for reusable in [true, false] {
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let (input, _, _) = CountingExec::ints(4);
            let mut e = sort_over(Box::new(input), reusable);
            e.rewind(&mut ctx).unwrap();
            let all = drain(&mut e, &mut ctx).unwrap();
            assert_eq!(ints(&all), vec![Some(3), Some(2), Some(1), Some(0)]);
            assert!(e.next(&mut ctx).unwrap().is_none());
            e.rewind(&mut ctx).unwrap();
            assert_eq!(drain(&mut e, &mut ctx).unwrap(), all);
            // 途中まで読んで rewind しても同じ。
            e.rewind(&mut ctx).unwrap();
            e.next(&mut ctx).unwrap();
            e.rewind(&mut ctx).unwrap();
            assert_eq!(drain(&mut e, &mut ctx).unwrap(), all);
        }
    }

    #[test]
    fn rewind_reuses_or_rebuilds_by_reusable() {
        // reusable: 子は 1 回しか読まれない。そうでなければ rewind のたびに子を読み直す。
        for (reusable, want_reads) in [(true, 3), (false, 6)] {
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let (input, reads, rewinds) = CountingExec::ints(3);
            let mut e = sort_over(Box::new(input), reusable);
            drain(&mut e, &mut ctx).unwrap();
            e.rewind(&mut ctx).unwrap();
            drain(&mut e, &mut ctx).unwrap();
            assert_eq!(reads.get(), want_reads, "reusable = {reusable}");
            assert_eq!(rewinds.get(), usize::from(!reusable));
        }
    }

    #[test]
    fn params_change_the_result_only_when_rebuilt() {
        use crate::executor::nodes::ProjectExec;
        for (reusable, second) in [(false, 20), (true, 10)] {
            let mut f = Fixture::with_params(1);
            let mut ctx = f.ctx();
            let (input, _, _) = CountingExec::ints(1);
            let input = ProjectExec::new(Box::new(input), vec![param(0, SqlType::INT4)]);
            let mut e = sort_over(Box::new(input), reusable);
            ctx.params[0] = Datum::Int4(10);
            assert_eq!(ints(&drain(&mut e, &mut ctx).unwrap()), vec![Some(10)]);
            ctx.params[0] = Datum::Int4(20);
            e.rewind(&mut ctx).unwrap();
            assert_eq!(ints(&drain(&mut e, &mut ctx).unwrap()), vec![Some(second)]);
        }
    }

    #[test]
    fn charges_and_releases_the_memory_budget() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(10);
        let mut e = sort_over(Box::new(input), false);
        drain(&mut e, &mut ctx).unwrap();
        assert!(ctx.mem.used() > 0);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        // 上限を超えたら 53200。
        let mut f = Fixture::new();
        f.mem_limit = 10;
        let (input, _, _) = CountingExec::ints(10);
        let mut e = sort_over(Box::new(input), false);
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
    }

    #[test]
    fn long_loop_checks_interrupts_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(100_000);
        let mut e = sort_over(Box::new(input), false);
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(reads.get() <= 1);
    }

    #[test]
    fn charges_row_and_key_and_exact_limit() {
        let one = estimate_row_bytes(&vec![Datum::Int4(0)]) * 2;
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(4);
        let mut e = sort_over(Box::new(input), false);
        drain(&mut e, &mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 4 * one);
        for (limit, ok) in [(4 * one, true), (4 * one - 1, false)] {
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let (input, _, _) = CountingExec::ints(4);
            let mut e = sort_over(Box::new(input), false);
            let r = f.run(&mut e);
            assert_eq!(r.is_ok(), ok, "limit {limit}");
            if let Err(err) = r {
                assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
            }
        }
    }

    #[test]
    fn non_rewindable_sort_gives_back_the_charge_after_the_last_row() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(3);
        let mut e: BoxedExecutor = Box::new(
            SortExec::with_reusable(
                Box::new(input),
                vec![key(0, SqlType::INT4, true, false)],
                true,
            )
            .with_rewindable(false),
        );
        assert_eq!(e.next(&mut ctx).unwrap().unwrap(), vec![Datum::Int4(2)]);
        assert!(ctx.mem.used() > 0);
        assert_eq!(e.next(&mut ctx).unwrap().unwrap(), vec![Datum::Int4(1)]);
        assert_eq!(e.next(&mut ctx).unwrap().unwrap(), vec![Datum::Int4(0)]);
        assert_eq!(ctx.mem.used(), 0);
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert!(e.next(&mut ctx).unwrap().is_none());
    }

    #[test]
    fn serial_sorts_release_everything_when_not_rewindable() {
        let one = estimate_row_bytes(&vec![Datum::Int4(0)]) * 2;
        let mut f = Fixture::new();
        f.mem_limit = 6 * one;
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(3);
        let lower = SortExec::with_reusable(
            Box::new(input),
            vec![key(0, SqlType::INT4, false, false)],
            true,
        )
        .with_rewindable(false);
        let upper = SortExec::with_reusable(
            Box::new(lower),
            vec![key(0, SqlType::INT4, true, false)],
            true,
        )
        .with_rewindable(false);
        let mut e: BoxedExecutor = Box::new(upper);
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 3);
        assert_eq!(ctx.mem.used(), 0);
        // 下は最後の行を返した時点で手放すので、同時に保持するのは 3 + 2 行分。
        assert_eq!(ctx.mem.peak(), 5 * one);
    }
}
