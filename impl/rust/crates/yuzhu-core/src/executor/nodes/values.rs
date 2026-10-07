//! Executor node: values (each row of expressions over an empty row).

use crate::error::Result;
use crate::executor::{ExecCtx, Executor, eval};
use crate::planner::physical::PhysExpr;
use crate::types::Row;

#[derive(Debug)]
pub struct ValuesExec {
    rows: Vec<Vec<PhysExpr>>,
    pos: usize,
}

impl ValuesExec {
    pub fn new(rows: Vec<Vec<PhysExpr>>) -> Self {
        ValuesExec { rows, pos: 0 }
    }
}

impl Executor for ValuesExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        let Some(exprs) = self.rows.get(self.pos) else {
            return Ok(None);
        };
        self.pos += 1;
        let empty = Row::new();
        let row = exprs
            .iter()
            .map(|e| eval(e, &empty, ctx))
            .collect::<Result<Row>>()?;
        Ok(Some(row))
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.pos = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::BoxedExecutor;
    use crate::executor::eval::tests::{int, text};
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::types::Datum;

    fn rows() -> Vec<Vec<Datum>> {
        vec![
            vec![Datum::Int4(1), Datum::Text("a".into())],
            vec![Datum::Int4(2), Datum::Text("b".into())],
        ]
    }

    fn values() -> BoxedExecutor {
        Box::new(ValuesExec::new(vec![
            vec![int(1), text("a")],
            vec![int(2), text("b")],
        ]))
    }

    #[test]
    fn emits_each_row() {
        let mut f = Fixture::new();
        assert_eq!(f.run(&mut values()).unwrap(), rows());
    }

    #[test]
    fn rewind_replays_same_rows() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e = values();
        // 未開始の rewind は何もしない。
        e.rewind(&mut ctx).unwrap();
        assert!(e.next(&mut ctx).unwrap().is_some());
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), rows());
        assert!(e.next(&mut ctx).unwrap().is_none());
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), rows());
    }

    #[test]
    fn checks_interrupts_per_next() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let err = f.run(&mut values()).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
    }
}
