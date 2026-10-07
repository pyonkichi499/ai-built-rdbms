//! Executor node: LIMIT / OFFSET.
//!
//! Both counts are evaluated once, OFFSET first (as PostgreSQL's
//! `recompute_limits`). NULL means "no limit" / "no offset"; negative
//! values raise 2201X / 2201W. `rewind` discards the state so the counts are
//! evaluated again (they may read `Param`s) and rewinds the input.

use crate::catalog::{CastMethod, FnKind};
use crate::error::{Error, Result, sqlstate};
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::expr::ExprKind;
use crate::planner::physical::PhysExpr;
use crate::types::{Datum, Row};

pub struct LimitExec {
    input: BoxedExecutor,
    limit: Option<PhysExpr>,
    offset: Option<PhysExpr>,
    /// `(remaining to skip, remaining to emit)` once evaluated.
    state: Option<(u64, Option<u64>)>,
    /// 下位ノードの定数部分式。PostgreSQL はプランナの定数畳み込みでエラーを出すので、
    /// LIMIT / OFFSET の検査より先に評価する。
    folds: Vec<PhysExpr>,
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
    pub fn new(input: BoxedExecutor, limit: Option<PhysExpr>, offset: Option<PhysExpr>) -> Self {
        LimitExec {
            input,
            limit,
            offset,
            state: None,
            folds: Vec::new(),
        }
    }

    /// 定数畳み込みで評価する式を渡す（`collect_constants` の結果）。
    #[must_use]
    pub fn with_folds(mut self, folds: Vec<PhysExpr>) -> Self {
        self.folds = folds;
        self
    }

    /// OFFSET → LIMIT の順に評価して状態を作る。
    fn start(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        for e in &self.folds {
            eval(e, &Row::new(), ctx)?;
        }
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
        Ok(())
    }
}

fn eval_count(expr: Option<&PhysExpr>, ctx: &mut ExecCtx<'_>) -> Result<Option<i64>> {
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
            self.start(ctx)?;
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
            ctx.check_interrupts()?;
            *skip -= 1;
        }
        let row = self.input.next(ctx)?;
        if row.is_some() {
            ctx.check_interrupts()?;
            if let Some(r) = remaining.as_mut() {
                *r -= 1;
            }
        }
        Ok(row)
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.state = None;
        self.input.rewind(ctx)
    }
}

fn is_constant(e: &PhysExpr) -> bool {
    match &e.kind {
        ExprKind::Literal(_) => true,
        // `TypeEnv` を使う変換は stable なので定数畳み込みしない（11 §7.1 の C-14）。
        ExprKind::Column(_)
        | ExprKind::SessionValue(_)
        | ExprKind::SubLink { .. }
        | ExprKind::SubLinkOutput(_)
        | ExprKind::Aggregate(_)
        | ExprKind::Cast {
            method: CastMethod::Env(_),
            ..
        } => false,
        ExprKind::Function { func, args } => {
            matches!(func.kind, FnKind::Pure(_)) && args.iter().all(is_constant)
        }
        _ => e.children().into_iter().all(is_constant),
    }
}

/// 定数だけでできた最大の部分式（リテラルそのものは除く）を集める。
pub fn collect_constants(e: &PhysExpr, out: &mut Vec<PhysExpr>) {
    if is_constant(e) {
        if !matches!(e.kind, ExprKind::Literal(_)) {
            out.push(e.clone());
        }
    } else {
        for x in e.children() {
            collect_constants(x, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, lit, null, param};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::SqlType;

    fn i8(v: i64) -> PhysExpr {
        lit(Datum::Int8(v), SqlType::INT8)
    }

    fn run(limit: Option<PhysExpr>, offset: Option<PhysExpr>) -> Result<Vec<i64>> {
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

    #[test]
    fn rewind_replays_and_reevaluates_counts() {
        let mut f = Fixture::with_params(1);
        let mut ctx = f.ctx();
        let input = Box::new(ValuesExec::new((1..=5).map(|v| vec![int(v)]).collect()));
        let mut e: BoxedExecutor = Box::new(LimitExec::new(
            input,
            Some(param(0, SqlType::INT8)),
            Some(i8(1)),
        ));
        e.rewind(&mut ctx).unwrap();
        ctx.params[0] = Datum::Int8(2);
        let rows = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(rows, vec![vec![Datum::Int4(2)], vec![Datum::Int4(3)]]);
        ctx.params[0] = Datum::Int8(1);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), vec![vec![Datum::Int4(2)]]);
    }

    #[test]
    fn skip_loop_checks_interrupts_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(1_000);
        let mut e: BoxedExecutor = Box::new(LimitExec::new(Box::new(input), None, Some(i8(500))));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::QUERY_CANCELED);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn constant_subexpressions_are_collected() {
        let mut out = Vec::new();
        collect_constants(&int(1), &mut out);
        assert!(out.is_empty());
        collect_constants(
            &crate::executor::eval::tests::op(
                &crate::executor::eval::tests::GT,
                crate::executor::eval::tests::col(0, SqlType::INT4),
                int(1),
            ),
            &mut out,
        );
        assert!(out.is_empty());
    }
}
