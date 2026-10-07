//! Executor node: filter (keeps rows whose predicate is true).

use std::rc::Rc;

use crate::error::Result;
use crate::executor::eval::eval_pred;
use crate::executor::instrument::Instrumentation;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{FilterCounter, PhysExpr};
use crate::types::Row;

pub struct FilterExec {
    input: BoxedExecutor,
    predicate: PhysExpr,
    /// EXPLAIN ANALYZE の計測（`set_counters` が渡す）。
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl std::fmt::Debug for FilterExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterExec")
            .field("predicate", &self.predicate)
            .finish_non_exhaustive()
    }
}

impl FilterExec {
    pub fn new(input: BoxedExecutor, predicate: PhysExpr) -> Self {
        FilterExec {
            input,
            predicate,
            counters: None,
        }
    }
}

impl Executor for FilterExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            if eval_pred(&self.predicate, &row, ctx)? == Some(true) {
                return Ok(Some(row));
            }
            if let Some((id, instr)) = &self.counters {
                instr.add_removed(*id, FilterCounter::Filter, 1);
            }
        }
        Ok(None)
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.input.rewind(ctx)
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.counters = Some((id, Rc::clone(instr)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, null, op};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::{Datum, SqlType};

    fn values() -> BoxedExecutor {
        Box::new(ValuesExec::new(vec![
            vec![int(1)],
            vec![int(5)],
            vec![null(SqlType::INT4)],
            vec![int(3)],
        ]))
    }

    fn pred() -> PhysExpr {
        op(&GT, col(0, SqlType::INT4), int(2))
    }

    #[test]
    fn keeps_only_true_rows() {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(FilterExec::new(values(), pred()));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(5)], vec![Datum::Int4(3)]]
        );
    }

    #[test]
    fn rewind_replays_same_rows() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor = Box::new(FilterExec::new(values(), pred()));
        e.rewind(&mut ctx).unwrap();
        assert_eq!(e.next(&mut ctx).unwrap(), Some(vec![Datum::Int4(5)]));
        e.rewind(&mut ctx).unwrap();
        let all = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(all, vec![vec![Datum::Int4(5)], vec![Datum::Int4(3)]]);
    }

    #[test]
    fn counts_removed_rows() {
        let mut f = Fixture::new();
        let instr = Rc::new(Instrumentation::new(3));
        let mut e = FilterExec::new(values(), pred());
        e.set_counters(2, &instr);
        let mut e: BoxedExecutor = Box::new(e);
        assert_eq!(f.run(&mut e).unwrap().len(), 2);
        // 1 と NULL が落ちる。
        assert_eq!(instr.node(2).removed_filter.get(), 2);
        assert_eq!(instr.node(0).removed_filter.get(), 0);
    }

    #[test]
    fn filter_loop_checks_interrupts_per_input_row() {
        // 述語が偽の行を読み捨てるループの中でキャンセルが効く（子は検査しない）。
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(1_000);
        let mut e: BoxedExecutor = Box::new(FilterExec::new(
            Box::new(input),
            op(&GT, col(0, SqlType::INT4), int(10_000)),
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert_eq!(reads.get(), 1);
    }
}
