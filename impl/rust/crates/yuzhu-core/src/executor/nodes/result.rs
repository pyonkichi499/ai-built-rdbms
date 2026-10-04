//! Executor node: result (FROM-less SELECT, exactly one row).

use crate::analyzer::BoundExpr;
use crate::error::Result;
use crate::executor::{ExecCtx, Executor, eval};
use crate::types::Row;

#[derive(Debug)]
pub struct ResultExec {
    exprs: Vec<BoundExpr>,
    done: bool,
}

impl ResultExec {
    pub fn new(exprs: Vec<BoundExpr>) -> Self {
        ResultExec { exprs, done: false }
    }
}

impl Executor for ResultExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let empty = Row::new();
        let row = self
            .exprs
            .iter()
            .map(|e| eval(e, &empty, ctx))
            .collect::<Result<Row>>()?;
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::int;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::Datum;

    #[test]
    fn emits_one_row() {
        let mut f = Fixture::new();
        let mut e: crate::executor::BoxedExecutor = Box::new(ResultExec::new(vec![int(1), int(2)]));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(1), Datum::Int4(2)]]
        );
        let mut e: crate::executor::BoxedExecutor = Box::new(ResultExec::new(vec![]));
        assert_eq!(f.run(&mut e).unwrap(), vec![Vec::<Datum>::new()]);
    }
}
