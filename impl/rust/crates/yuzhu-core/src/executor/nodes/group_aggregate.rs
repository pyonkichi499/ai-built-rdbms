//! GroupAggregate ノード。`m4/05` §5.18。
//!
//! 入力は `keys` で整列済み（planner が Sort を置く）。出力は HashAggregate と同じ形（`keys ++ aggs の結果`）で、
//! 順序は入力の整列順。ストリーミングで、課金は現在のグループの DISTINCT の集合などだけ
//! （グループを出すときに返す）。入力が空なら 0 行。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::cmp::Ordering;

use crate::error::Result;
use crate::executor::agg::{AggGroup, AggSet};
use crate::executor::build::{BuildEnv, build_scoped};
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::physical::{PhysExpr, PhysicalPlan};
use crate::types::{Datum, Row, cmp_datum};

pub struct GroupAggregateExec {
    input: BoxedExecutor,
    keys: Vec<PhysExpr>,
    aggs: AggSet,
    cur: Option<(Vec<Datum>, AggGroup)>,
    done: bool,
}

impl std::fmt::Debug for GroupAggregateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupAggregateExec")
            .field("keys", &self.keys.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl GroupAggregateExec {
    pub fn new(input: BoxedExecutor, keys: Vec<PhysExpr>, aggs: AggSet) -> Self {
        GroupAggregateExec {
            input,
            keys,
            aggs,
            cur: None,
            done: false,
        }
    }

    /// グループを出力の行にして、課金を返す。
    fn emit(&self, group: (Vec<Datum>, AggGroup), ctx: &ExecCtx<'_>) -> Result<Row> {
        let (mut row, g) = group;
        row.extend(self.aggs.finish(&g)?);
        ctx.mem.release(g.charged());
        Ok(row)
    }
}

fn same_key(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| cmp_datum(x, y) == Ordering::Equal)
}

impl Executor for GroupAggregateExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        loop {
            ctx.check_interrupts()?;
            let Some(row) = self.input.next(ctx)? else {
                self.done = true;
                return match self.cur.take() {
                    Some(g) => Ok(Some(self.emit(g, ctx)?)),
                    None => Ok(None),
                };
            };
            let key = self
                .keys
                .iter()
                .map(|e| eval(e, &row, ctx))
                .collect::<Result<Vec<_>>>()?;
            let mut out = None;
            let same = matches!(&self.cur, Some((k, _)) if same_key(k, &key));
            if !same {
                let prev = self.cur.replace((key, self.aggs.new_group()));
                if let Some(g) = prev {
                    out = Some(self.emit(g, ctx)?);
                }
            }
            if let Some((_, g)) = self.cur.as_mut() {
                self.aggs.accumulate(g, &row, ctx)?;
            }
            if out.is_some() {
                return Ok(out);
            }
        }
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if let Some((_, g)) = self.cur.take() {
            ctx.mem.release(g.charged());
        }
        self.done = false;
        self.input.rewind(ctx)
    }
}

pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::GroupAggregate {
        input, keys, aggs, ..
    } = plan
    else {
        return Box::new(super::UnsupportedExec::new("GroupAggregate"));
    };
    Box::new(GroupAggregateExec::new(
        build_scoped(input, env, rewindable),
        keys.clone(),
        AggSet::new(aggs),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::AggKind;
    use crate::executor::agg::test_support::agg;
    use crate::executor::eval::tests::{col, int, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::hash_aggregate::HashAggregateExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::SqlType;

    fn set() -> AggSet {
        AggSet::new(&[
            agg(AggKind::CountStar, vec![], false, None),
            agg(AggKind::Count, vec![col(1, SqlType::INT4)], true, None),
        ])
    }

    fn sorted_data() -> BoxedExecutor {
        Box::new(ValuesExec::new(vec![
            vec![text("a"), int(1)],
            vec![text("a"), int(1)],
            vec![text("a"), int(2)],
            vec![text("b"), int(5)],
            vec![null(SqlType::TEXT), int(7)],
            vec![null(SqlType::TEXT), int(7)],
        ]))
    }

    fn node(input: BoxedExecutor) -> BoxedExecutor {
        Box::new(GroupAggregateExec::new(
            input,
            vec![col(0, SqlType::TEXT)],
            set(),
        ))
    }

    #[test]
    fn groups_in_input_order_and_null_keys_group_together() {
        let mut f = Fixture::new();
        let mut e = node(sorted_data());
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Text("a".into()), Datum::Int8(3), Datum::Int8(2)],
                vec![Datum::Text("b".into()), Datum::Int8(1), Datum::Int8(1)],
                vec![Datum::Null, Datum::Int8(2), Datum::Int8(1)],
            ]
        );
    }

    #[test]
    fn same_result_as_hash_aggregate() {
        let mut f = Fixture::new();
        let mut g = node(sorted_data());
        let mut h: BoxedExecutor = Box::new(HashAggregateExec::new(
            sorted_data(),
            vec![col(0, SqlType::TEXT)],
            set(),
            true,
            false,
        ));
        assert_eq!(f.run(&mut g).unwrap(), f.run(&mut h).unwrap());
    }

    #[test]
    fn empty_input_gives_no_rows_and_stays_fused() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (input, reads, _) = CountingExec::new(vec![]);
        let mut e = node(Box::new(input));
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn rewind_starts_over() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e = node(sorted_data());
        // 途中で rewind しても同じ。
        e.next(&mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        let all = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(all.len(), 3);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), all);
    }

    #[test]
    fn distinct_charge_is_returned_when_the_group_changes() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e = node(sorted_data());
        // 最初のグループ (a) を返した時点で、a の DISTINCT の課金は返っていて、次のグループ (b) の分だけ。
        e.next(&mut ctx).unwrap();
        let one_value = crate::executor::mem::estimate_row_bytes(&vec![Datum::Int4(5)])
            + crate::executor::mem::HASH_ENTRY_OVERHEAD;
        assert_eq!(ctx.mem.used(), one_value);
        drain(&mut e, &mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn interrupts_are_checked_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(100_000);
        let mut e: BoxedExecutor = Box::new(GroupAggregateExec::new(
            Box::new(input),
            vec![col(0, SqlType::INT4)],
            AggSet::new(&[]),
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(reads.get() <= 1);
    }
}
