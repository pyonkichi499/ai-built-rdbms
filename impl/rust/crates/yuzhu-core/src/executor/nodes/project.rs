//! Executor node: projection (computes expressions over each input row).

use crate::error::Result;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::physical::PhysExpr;
use crate::types::Row;

pub struct ProjectExec {
    input: BoxedExecutor,
    exprs: Vec<PhysExpr>,
}

impl std::fmt::Debug for ProjectExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectExec")
            .field("exprs", &self.exprs)
            .finish_non_exhaustive()
    }
}

impl ProjectExec {
    pub fn new(input: BoxedExecutor, exprs: Vec<PhysExpr>) -> Self {
        ProjectExec { input, exprs }
    }
}

impl Executor for ProjectExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let Some(row) = self.input.next(ctx)? else {
            return Ok(None);
        };
        let out = self
            .exprs
            .iter()
            .map(|e| eval(e, &row, ctx))
            .collect::<Result<Row>>()?;
        Ok(Some(out))
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.input.rewind(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, op, param, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::types::{Datum, SqlType};

    fn project() -> BoxedExecutor {
        let input = Box::new(ValuesExec::new(vec![
            vec![int(1), text("a")],
            vec![int(5), text("b")],
        ]));
        let exprs = vec![
            col(1, SqlType::TEXT),
            op(&GT, col(0, SqlType::INT4), int(2)),
        ];
        Box::new(ProjectExec::new(input, exprs))
    }

    #[test]
    fn computes_exprs() {
        let mut f = Fixture::new();
        assert_eq!(
            f.run(&mut project()).unwrap(),
            vec![
                vec![Datum::Text("a".into()), Datum::Bool(false)],
                vec![Datum::Text("b".into()), Datum::Bool(true)],
            ]
        );
    }

    #[test]
    fn rewind_replays_same_rows_and_reads_params() {
        let mut f = Fixture::with_params(1);
        let mut ctx = f.ctx();
        let mut e = project();
        e.rewind(&mut ctx).unwrap();
        let first = drain(&mut e, &mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), first);
        // Param を読む式は rewind のたびに最新の値になる。
        let mut p: BoxedExecutor = Box::new(ProjectExec::new(
            Box::new(ValuesExec::new(vec![vec![int(0)]])),
            vec![param(0, SqlType::INT4)],
        ));
        ctx.params[0] = Datum::Int4(7);
        assert_eq!(drain(&mut p, &mut ctx).unwrap(), vec![vec![Datum::Int4(7)]]);
        ctx.params[0] = Datum::Int4(8);
        p.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut p, &mut ctx).unwrap(), vec![vec![Datum::Int4(8)]]);
    }
}
