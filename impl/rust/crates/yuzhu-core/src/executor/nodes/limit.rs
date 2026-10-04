//! Executor node: LIMIT / OFFSET.
//!
//! Both counts are evaluated once, OFFSET first (as PostgreSQL's
//! `recompute_limits`). NULL means "no limit" / "no offset"; negative
//! values raise 2201X / 2201W.

use crate::analyzer::BoundExpr;
use crate::error::{Error, Result, sqlstate};
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::types::{Datum, Row};

pub struct LimitExec {
    input: BoxedExecutor,
    limit: Option<BoundExpr>,
    offset: Option<BoundExpr>,
    /// `(remaining to skip, remaining to emit)` once evaluated.
    state: Option<(u64, Option<u64>)>,
}

impl std::fmt::Debug for LimitExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimitExec")
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl LimitExec {
    pub fn new(input: BoxedExecutor, limit: Option<BoundExpr>, offset: Option<BoundExpr>) -> Self {
        LimitExec {
            input,
            limit,
            offset,
            state: None,
        }
    }
}

fn eval_count(expr: Option<&BoundExpr>, ctx: &ExecCtx<'_>) -> Result<Option<i64>> {
    let Some(e) = expr else {
        return Ok(None);
    };
    match eval(e, &Row::new(), ctx)? {
        Datum::Null => Ok(None),
        d => d
            .as_i64()
            .map(Some)
            .ok_or_else(|| Error::internal(format!("LIMIT/OFFSET value is not an integer: {d:?}"))),
    }
}

impl Executor for LimitExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.state.is_none() {
            let offset = eval_count(self.offset.as_ref(), ctx)?;
            if offset.is_some_and(|v| v < 0) {
                return Err(Error::new(
                    sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                    "OFFSET must not be negative",
                ));
            }
            let limit = eval_count(self.limit.as_ref(), ctx)?;
            if limit.is_some_and(|v| v < 0) {
                return Err(Error::new(
                    sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                    "LIMIT must not be negative",
                ));
            }
            let to_u64 = |v: i64| u64::try_from(v).unwrap_or(0);
            self.state = Some((offset.map_or(0, to_u64), limit.map(to_u64)));
        }
        let Some((skip, remaining)) = self.state.as_mut() else {
            return Ok(None);
        };
        if *remaining == Some(0) {
            return Ok(None);
        }
        while *skip > 0 {
            if self.input.next(ctx)?.is_none() {
                return Ok(None);
            }
            *skip -= 1;
        }
        let row = self.input.next(ctx)?;
        if row.is_some()
            && let Some(r) = remaining.as_mut()
        {
            *r -= 1;
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, lit, null};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::SqlType;

    fn i8(v: i64) -> BoundExpr {
        lit(Datum::Int8(v), SqlType::INT8)
    }

    fn run(limit: Option<BoundExpr>, offset: Option<BoundExpr>) -> Result<Vec<i64>> {
        let mut f = Fixture::new();
        let input = Box::new(ValuesExec::new((1..=5).map(|v| vec![int(v)]).collect()));
        let mut e: BoxedExecutor = Box::new(LimitExec::new(input, limit, offset));
        Ok(f.run(&mut e)?
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect())
    }

    #[test]
    fn limit_offset() {
        assert_eq!(run(Some(i8(2)), None).unwrap(), vec![1, 2]);
        assert_eq!(run(Some(i8(2)), Some(i8(3))).unwrap(), vec![4, 5]);
        assert_eq!(run(None, Some(i8(4))).unwrap(), vec![5]);
        assert_eq!(run(None, Some(i8(10))).unwrap(), Vec::<i64>::new());
        assert_eq!(run(Some(i8(0)), None).unwrap(), Vec::<i64>::new());
        assert_eq!(run(Some(i8(100)), None).unwrap().len(), 5);
        // NULL = no limit / no offset.
        assert_eq!(
            run(Some(null(SqlType::INT8)), Some(null(SqlType::INT8)))
                .unwrap()
                .len(),
            5
        );
    }

    #[test]
    fn negative_counts() {
        let e = run(Some(i8(-1)), None).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE);
        assert_eq!(e.message, "LIMIT must not be negative");
        let e = run(None, Some(i8(-1))).unwrap_err();
        assert_eq!(
            e.sqlstate,
            sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE
        );
        assert_eq!(e.message, "OFFSET must not be negative");
        // OFFSET is checked first.
        let e = run(Some(i8(-1)), Some(i8(-1))).unwrap_err();
        assert_eq!(
            e.sqlstate,
            sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE
        );
    }
}
