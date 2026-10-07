//! Executor node: result (FROM-less SELECT, exactly one row).
//!
//! 初回の `next` で `one_time_filter` を評価し、真なら `exprs` を評価して 1 行を返す
//! （偽 / NULL なら 0 行）。`rewind` で再び出す（式は再評価される）。

use crate::error::Result;
use crate::executor::eval::{eval, eval_pred};
use crate::executor::{ExecCtx, Executor};
use crate::planner::physical::PhysExpr;
use crate::types::Row;

#[derive(Debug)]
pub struct ResultExec {
    exprs: Vec<PhysExpr>,
    one_time_filter: Option<PhysExpr>,
    done: bool,
}

impl ResultExec {
    pub fn new(exprs: Vec<PhysExpr>) -> Self {
        ResultExec::with_filter(exprs, None)
    }

    pub fn with_filter(exprs: Vec<PhysExpr>, one_time_filter: Option<PhysExpr>) -> Self {
        ResultExec {
            exprs,
            one_time_filter,
            done: false,
        }
    }
}

impl Executor for ResultExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let empty = Row::new();
        if let Some(f) = &self.one_time_filter
            && eval_pred(f, &empty, ctx)? != Some(true)
        {
            return Ok(None);
        }
        let row = self
            .exprs
            .iter()
            .map(|e| eval(e, &empty, ctx))
            .collect::<Result<Row>>()?;
        Ok(Some(row))
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.done = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::BoxedExecutor;
    use crate::executor::eval::tests::{boolean, int, param};
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::types::{Datum, SqlType};

    #[test]
    fn emits_one_row() {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(ResultExec::new(vec![int(1), int(2)]));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(1), Datum::Int4(2)]]
        );
        let mut e: BoxedExecutor = Box::new(ResultExec::new(vec![]));
        assert_eq!(f.run(&mut e).unwrap(), vec![Vec::<Datum>::new()]);
    }

    #[test]
    fn one_time_filter_false_or_null_gives_no_rows() {
        let mut f = Fixture::new();
        for flt in [boolean(Some(false)), boolean(None)] {
            let mut e: BoxedExecutor = Box::new(ResultExec::with_filter(vec![int(1)], Some(flt)));
            assert!(f.run(&mut e).unwrap().is_empty());
        }
        let mut e: BoxedExecutor = Box::new(ResultExec::with_filter(
            vec![int(1)],
            Some(boolean(Some(true))),
        ));
        assert_eq!(f.run(&mut e).unwrap().len(), 1);
    }

    #[test]
    fn rewind_replays_and_reevaluates_params() {
        let mut f = Fixture::with_params(1);
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor = Box::new(ResultExec::new(vec![param(0, SqlType::INT4)]));
        // 未開始の rewind は何もしない。
        e.rewind(&mut ctx).unwrap();
        ctx.params[0] = Datum::Int4(1);
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), vec![vec![Datum::Int4(1)]]);
        assert!(e.next(&mut ctx).unwrap().is_none());
        ctx.params[0] = Datum::Int4(2);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), vec![vec![Datum::Int4(2)]]);
    }

    #[test]
    fn checks_interrupts_per_next() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let mut e: BoxedExecutor = Box::new(ResultExec::new(vec![int(1)]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
    }
}
