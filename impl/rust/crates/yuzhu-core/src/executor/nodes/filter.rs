//! Executor node: filter (keeps rows whose predicate is true).

use crate::analyzer::BoundExpr;
use crate::error::Result;
use crate::executor::eval::eval_bool;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::types::Row;

pub struct FilterExec {
    input: BoxedExecutor,
    predicate: BoundExpr,
}

impl std::fmt::Debug for FilterExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterExec")
            .field("predicate", &self.predicate)
            .finish_non_exhaustive()
    }
}

impl FilterExec {
    pub fn new(input: BoxedExecutor, predicate: BoundExpr) -> Self {
        FilterExec { input, predicate }
    }
}

impl Executor for FilterExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            if eval_bool(&self.predicate, &row, ctx.session)? == Some(true) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, null, op};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::{Datum, SqlType};

    #[test]
    fn keeps_only_true_rows() {
        let mut f = Fixture::new();
        let input = Box::new(ValuesExec::new(vec![
            vec![int(1)],
            vec![int(5)],
            vec![null(SqlType::INT4)],
            vec![int(3)],
        ]));
        let pred = op(&GT, col(0, SqlType::INT4), int(2));
        let mut e: BoxedExecutor = Box::new(FilterExec::new(input, pred));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(5)], vec![Datum::Int4(3)]]
        );
    }
}
