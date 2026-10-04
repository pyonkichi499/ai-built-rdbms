//! Executor node: values (each row of expressions over an empty row).

use crate::analyzer::BoundExpr;
use crate::error::Result;
use crate::executor::{ExecCtx, Executor, eval};
use crate::types::Row;

#[derive(Debug)]
pub struct ValuesExec {
    rows: Vec<Vec<BoundExpr>>,
    pos: usize,
}

impl ValuesExec {
    pub fn new(rows: Vec<Vec<BoundExpr>>) -> Self {
        ValuesExec { rows, pos: 0 }
    }
}

impl Executor for ValuesExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, text};
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::Datum;

    #[test]
    fn emits_each_row() {
        let mut f = Fixture::new();
        let mut e: crate::executor::BoxedExecutor = Box::new(ValuesExec::new(vec![
            vec![int(1), text("a")],
            vec![int(2), text("b")],
        ]));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Int4(1), Datum::Text("a".into())],
                vec![Datum::Int4(2), Datum::Text("b".into())],
            ]
        );
    }
}
