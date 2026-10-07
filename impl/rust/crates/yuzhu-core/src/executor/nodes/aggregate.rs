//! Aggregate ノード（GROUP BY なし）。`m4/05` §5.16。
//!
//! 全入力を `AggSet::accumulate` してから 1 行を返す。入力が空でも 1 行（`count` は 0、ほかは NULL）。
//! 課金は DISTINCT の集合の新しい値と、`min` / `max` の状態の増分だけ（`AggGroup::charged`）。
//! `rewind`: `reuse`（子が `Param` に依存しない）なら結果を再び 1 回返す。そうでなければ状態を捨てて
//! 子を `rewind` する。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use crate::error::Result;
use crate::executor::agg::{AggGroup, AggSet};
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::{Datum, Row};

pub struct AggregateExec {
    input: BoxedExecutor,
    aggs: AggSet,
    reuse: bool,
    rewindable: bool,
    /// 計算済みの結果（再利用のため残す）。
    result: Option<Vec<Datum>>,
    emitted: bool,
    /// DISTINCT の集合などの課金（`rewind` か、`rewindable = false` なら結果を作った時点で返す）。
    group: Option<AggGroup>,
}

impl std::fmt::Debug for AggregateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateExec")
            .field("aggs", &self.aggs.len())
            .field("reuse", &self.reuse)
            .finish_non_exhaustive()
    }
}

impl AggregateExec {
    pub fn new(input: BoxedExecutor, aggs: AggSet, reuse: bool, rewindable: bool) -> Self {
        AggregateExec {
            input,
            aggs,
            reuse,
            rewindable,
            result: None,
            emitted: false,
            group: None,
        }
    }

    fn release_group(&mut self, ctx: &ExecCtx<'_>) {
        if let Some(g) = self.group.take() {
            ctx.mem.release(g.charged());
        }
    }

    fn compute(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        let mut g = self.aggs.new_group();
        let mut failed = None;
        while let Some(row) = self.input.next(ctx)? {
            if let Err(e) = ctx
                .check_interrupts()
                .and_then(|()| self.aggs.accumulate(&mut g, &row, ctx))
            {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            ctx.mem.release(g.charged());
            return Err(e);
        }
        self.result = Some(self.aggs.finish(&g)?);
        self.group = Some(g);
        if !self.rewindable {
            self.release_group(ctx);
        }
        Ok(())
    }
}

impl Executor for AggregateExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.emitted {
            return Ok(None);
        }
        if self.result.is_none() {
            self.compute(ctx)?;
        }
        self.emitted = true;
        Ok(self.result.clone())
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.reuse && self.rewindable && self.result.is_some() {
            self.emitted = false;
            return Ok(());
        }
        self.release_group(ctx);
        self.result = None;
        self.emitted = false;
        self.input.rewind(ctx)
    }
}

/// `Aggregate` を実行する Executor を作る（`BuildEnv` なし。単体テスト用）。
#[cfg(test)]
pub(in crate::executor) fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    build_in(plan, &BuildEnv::default(), true)
}

/// `build_scoped` の分岐から呼ぶ入口。`rewindable` は 05 §3.2 の規則どおり。
pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::Aggregate { input, aggs } = plan else {
        return Box::new(super::UnsupportedExec::new("Aggregate"));
    };
    let reuse = free_params(plan, env.query).is_empty();
    Box::new(AggregateExec::new(
        build_scoped(input, env, rewindable),
        AggSet::new(aggs),
        reuse,
        rewindable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::AggKind;
    use crate::executor::agg::test_support::agg;
    use crate::executor::eval::tests::{GT, col, int, op, param};
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::executor::nodes::{ProjectExec, ValuesExec};
    use crate::planner::physical::PhysAgg;
    use crate::types::SqlType;

    fn node(
        input: BoxedExecutor,
        aggs: &[PhysAgg],
        reuse: bool,
        rewindable: bool,
    ) -> BoxedExecutor {
        Box::new(AggregateExec::new(
            input,
            AggSet::new(aggs),
            reuse,
            rewindable,
        ))
    }

    fn ints(vals: &[Option<i32>]) -> BoxedExecutor {
        Box::new(ValuesExec::new(
            vals.iter()
                .map(|v| {
                    vec![match v {
                        Some(i) => int(*i),
                        None => crate::executor::eval::tests::null(SqlType::INT4),
                    }]
                })
                .collect(),
        ))
    }

    fn all_aggs() -> Vec<PhysAgg> {
        let c = || vec![col(0, SqlType::INT4)];
        vec![
            agg(AggKind::CountStar, vec![], false, None),
            agg(AggKind::Count, c(), false, None),
            agg(AggKind::SumInt4, c(), false, None),
            agg(AggKind::AvgInt4, c(), false, None),
            agg(AggKind::Min, c(), false, None),
            agg(AggKind::Max, c(), false, None),
        ]
    }

    #[test]
    fn empty_input_gives_one_row() {
        let mut f = Fixture::new();
        let mut e = node(ints(&[]), &all_aggs(), true, false);
        let rows = f.run(&mut e).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Datum::Int8(0),
                Datum::Int8(0),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null
            ]]
        );
    }

    #[test]
    fn nulls_are_skipped_but_count_star_counts_rows() {
        let mut f = Fixture::new();
        let mut e = node(ints(&[Some(1), None, Some(5)]), &all_aggs(), true, false);
        let rows = f.run(&mut e).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(&r[..3], &[Datum::Int8(3), Datum::Int8(2), Datum::Int8(6)]);
        assert_eq!(r[4], Datum::Int4(1));
        assert_eq!(r[5], Datum::Int4(5));
        match &r[3] {
            Datum::Numeric(n) => assert_eq!(n.to_string(), "3.0000000000000000"),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn filter_and_distinct() {
        let aggs = vec![
            agg(
                AggKind::CountStar,
                vec![],
                false,
                Some(op(&GT, col(0, SqlType::INT4), int(1))),
            ),
            agg(AggKind::Count, vec![col(0, SqlType::INT4)], true, None),
            agg(AggKind::SumInt4, vec![col(0, SqlType::INT4)], true, None),
        ];
        let mut f = Fixture::new();
        let mut e = node(
            ints(&[Some(1), Some(2), Some(2), Some(3), None]),
            &aggs,
            true,
            false,
        );
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int8(3), Datum::Int8(3), Datum::Int8(6)]]
        );
    }

    #[test]
    fn fused_after_the_single_row() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, reads, _) = CountingExec::ints(3);
        let mut e = node(
            Box::new(input),
            &[agg(AggKind::CountStar, vec![], false, None)],
            true,
            true,
        );
        assert!(e.next(&mut ctx).unwrap().is_some());
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn rewind_reuses_or_recomputes() {
        for (reuse, want_reads, want_rewinds) in [(true, 3, 0), (false, 6, 1)] {
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let (input, reads, rewinds) = CountingExec::ints(3);
            let mut e = node(
                Box::new(input),
                &[agg(AggKind::CountStar, vec![], false, None)],
                reuse,
                true,
            );
            let first = drain(&mut e, &mut ctx).unwrap();
            e.rewind(&mut ctx).unwrap();
            assert_eq!(drain(&mut e, &mut ctx).unwrap(), first);
            assert_eq!(reads.get(), want_reads);
            assert_eq!(rewinds.get(), want_rewinds);
        }
    }

    #[test]
    fn rewind_with_changed_param_recomputes() {
        let mut f = Fixture::with_params(1);
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(1);
        let input = ProjectExec::new(Box::new(input), vec![param(0, SqlType::INT4)]);
        let mut e = node(
            Box::new(input),
            &[agg(
                AggKind::SumInt4,
                vec![col(0, SqlType::INT4)],
                false,
                None,
            )],
            false,
            true,
        );
        ctx.params[0] = Datum::Int4(10);
        assert_eq!(
            drain(&mut e, &mut ctx).unwrap(),
            vec![vec![Datum::Int8(10)]]
        );
        ctx.params[0] = Datum::Int4(20);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(
            drain(&mut e, &mut ctx).unwrap(),
            vec![vec![Datum::Int8(20)]]
        );
    }

    #[test]
    fn distinct_charge_is_returned_on_rewind_or_after_the_row() {
        let aggs = [agg(AggKind::Count, vec![col(0, SqlType::INT4)], true, None)];
        // rewindable: rewind で返す。
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(5);
        let mut e = node(Box::new(input), &aggs, false, true);
        drain(&mut e, &mut ctx).unwrap();
        assert!(ctx.mem.used() > 0);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        // rewindable = false: 結果を作った時点で返す。
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, _, _) = CountingExec::ints(5);
        let mut e = node(Box::new(input), &aggs, false, false);
        drain(&mut e, &mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        assert!(ctx.mem.peak() > 0);
    }

    #[test]
    fn memory_limit_gives_53200() {
        let aggs = [agg(AggKind::Count, vec![col(0, SqlType::INT4)], true, None)];
        let mut f = Fixture::new();
        f.mem_limit = 100;
        let (input, _, _) = CountingExec::ints(100);
        let mut e = node(Box::new(input), &aggs, false, false);
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
    }

    #[test]
    fn interrupts_are_checked_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(100_000);
        let mut e = node(
            Box::new(input),
            &[agg(AggKind::CountStar, vec![], false, None)],
            true,
            false,
        );
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(reads.get() <= 1);
    }

    #[test]
    fn build_from_plan() {
        let plan = PhysicalPlan::Aggregate {
            input: Box::new(PhysicalPlan::Values {
                rows: vec![vec![int(1)], vec![int(2)]],
            }),
            aggs: vec![agg(
                AggKind::SumInt4,
                vec![col(0, SqlType::INT4)],
                false,
                None,
            )],
        };
        let mut f = Fixture::new();
        let mut e = build(&plan);
        assert_eq!(f.run(&mut e).unwrap(), vec![vec![Datum::Int8(3)]]);
    }
}
