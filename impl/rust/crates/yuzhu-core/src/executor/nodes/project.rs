//! Executor node: projection (computes expressions over each input row).

use crate::analyzer::BoundExpr;
use crate::error::Result;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::types::Row;

pub struct ProjectExec {
    input: BoxedExecutor,
    exprs: Vec<BoundExpr>,
}

impl std::fmt::Debug for ProjectExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectExec")
            .field("exprs", &self.exprs)
            .finish_non_exhaustive()
    }
}

impl ProjectExec {
    pub fn new(input: BoxedExecutor, exprs: Vec<BoundExpr>) -> Self {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, op, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::{Datum, SqlType};

    #[test]
    fn computes_exprs() {
        let mut f = Fixture::new();
        let input = Box::new(ValuesExec::new(vec![
            vec![int(1), text("a")],
            vec![int(5), text("b")],
        ]));
        let exprs = vec![
            col(1, SqlType::TEXT),
            op(&GT, col(0, SqlType::INT4), int(2)),
        ];
        let mut e: BoxedExecutor = Box::new(ProjectExec::new(input, exprs));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Text("a".into()), Datum::Bool(false)],
                vec![Datum::Text("b".into()), Datum::Bool(true)],
            ]
        );
    }
}
