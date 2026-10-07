//! FunctionScan ノード（`generate_series`）。`m4/05` §5.5、D5-21。
//!
//! 最初の `next` で `args` を空の行に対して評価し、`FnKind::Set` の `begin` で行の生成器を作る
//! （`generate_series(int4|int8, ..)` の 2 引数・3 引数。NULL 引数は 0 行、`step = 0` は 22023、
//! 向きが逆なら 0 行、桁あふれは「そこで終わり」。これらは `types/funcs.rs` の `Series` が決める）。
//! 集合返却関数でない関数は最初の `next` で `Error::internal`（planner が来させない）。
//! `rewind`: 未初期化に戻す（引数を再評価する）。メモリ課金なし。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use crate::catalog::builtin::BuiltinFunction;
use crate::catalog::{FnKind, SetIter};
use crate::error::{Error, Result};
use crate::executor::build::BuildEnv;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::physical::{PhysExpr, PhysicalPlan};
use crate::types::{Datum, Row};

pub struct FunctionScanExec {
    func: &'static BuiltinFunction,
    args: Vec<PhysExpr>,
    iter: Option<Box<dyn SetIter>>,
}

impl std::fmt::Debug for FunctionScanExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionScanExec")
            .field("func", &self.func.name)
            .field("started", &self.iter.is_some())
            .finish_non_exhaustive()
    }
}

impl FunctionScanExec {
    pub fn new(func: &'static BuiltinFunction, args: Vec<PhysExpr>) -> Self {
        FunctionScanExec {
            func,
            args,
            iter: None,
        }
    }

    fn start(&self, ctx: &mut ExecCtx<'_>) -> Result<Box<dyn SetIter>> {
        let FnKind::Set(set) = self.func.kind else {
            return Err(Error::internal(format!(
                "{} is not a set-returning function",
                self.func.name
            )));
        };
        let empty: Row = Vec::new();
        let args = self
            .args
            .iter()
            .map(|e| eval(e, &empty, ctx))
            .collect::<Result<Vec<Datum>>>()?;
        (set.begin)(&args)
    }
}

impl Executor for FunctionScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if self.iter.is_none() {
            self.iter = Some(self.start(ctx)?);
        }
        let Some(it) = self.iter.as_mut() else {
            return Ok(None);
        };
        Ok(it.next()?.map(|d| vec![d]))
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.iter = None;
        Ok(())
    }
}

#[cfg(test)]
pub(in crate::executor) fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    build_in(plan, &BuildEnv::default(), true)
}

pub(crate) fn build_in(
    plan: &PhysicalPlan,
    _env: &BuildEnv<'_>,
    _rewindable: bool,
) -> BoxedExecutor {
    let PhysicalPlan::FunctionScan { func, args } = plan else {
        return Box::new(super::UnsupportedExec::new("Function Scan"));
    };
    Box::new(FunctionScanExec::new(func, args.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::builtin::functions_named;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{int, lit, null, param};
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::types::SqlType;

    fn gs(nargs: usize, int8: bool) -> &'static BuiltinFunction {
        functions_named("generate_series")
            .into_iter()
            .find(|f| {
                f.args.len() == nargs
                    && f.result
                        == if int8 {
                            crate::types::oid::INT8
                        } else {
                            crate::types::oid::INT4
                        }
            })
            .expect("generate_series is built in")
    }

    fn i4(args: &[Option<i32>]) -> Result<Vec<Option<i64>>> {
        let exprs = args
            .iter()
            .map(|a| a.map_or_else(|| null(SqlType::INT4), int))
            .collect();
        run(gs(args.len(), false), exprs)
    }

    fn i8s(args: &[i64]) -> Result<Vec<Option<i64>>> {
        let exprs = args
            .iter()
            .map(|a| lit(Datum::Int8(*a), SqlType::INT8))
            .collect();
        run(gs(args.len(), true), exprs)
    }

    fn run(func: &'static BuiltinFunction, args: Vec<PhysExpr>) -> Result<Vec<Option<i64>>> {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(FunctionScanExec::new(func, args));
        Ok(f.run(&mut e)?.into_iter().map(|r| r[0].as_i64()).collect())
    }

    fn some(v: &[i64]) -> Vec<Option<i64>> {
        v.iter().map(|x| Some(*x)).collect()
    }

    /// 05 §5.5 の実機の照合値。
    #[test]
    fn boundaries_match_postgres() {
        assert_eq!(i4(&[Some(1), Some(3)]).unwrap(), some(&[1, 2, 3]));
        assert_eq!(
            i4(&[Some(2_147_483_646), Some(2_147_483_647)]).unwrap(),
            some(&[2_147_483_646, 2_147_483_647])
        );
        assert_eq!(
            i8s(&[9_223_372_036_854_775_806, 9_223_372_036_854_775_807, 2]).unwrap(),
            some(&[9_223_372_036_854_775_806])
        );
        assert!(i4(&[Some(1), Some(3), Some(-1)]).unwrap().is_empty());
        assert_eq!(i4(&[Some(3), Some(1), Some(-1)]).unwrap(), some(&[3, 2, 1]));
        assert!(i4(&[None, Some(3)]).unwrap().is_empty());
        assert!(i4(&[Some(1), Some(3), None]).unwrap().is_empty());
        assert_eq!(i4(&[Some(1), Some(10), Some(4)]).unwrap(), some(&[1, 5, 9]));
        assert_eq!(i4(&[Some(5), Some(5)]).unwrap(), some(&[5]));
        assert!(i4(&[Some(2), Some(1)]).unwrap().is_empty());
    }

    #[test]
    fn step_zero_is_22023_for_int4_and_int8() {
        for r in [i4(&[Some(1), Some(3), Some(0)]), i8s(&[1, 3, 0])] {
            let e = r.unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
            assert_eq!(e.message, "step size cannot equal zero");
        }
    }

    #[test]
    fn int8_range_and_negative_overflow() {
        assert_eq!(i8s(&[i64::MAX - 1, i64::MAX]).unwrap().len(), 2);
        assert_eq!(
            i8s(&[i64::MIN + 1, i64::MIN, -1]).unwrap(),
            some(&[i64::MIN + 1, i64::MIN])
        );
    }

    #[test]
    fn output_is_a_single_column_of_the_right_type() {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor =
            Box::new(FunctionScanExec::new(gs(2, false), vec![int(1), int(2)]));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]]
        );
        let mut e: BoxedExecutor = Box::new(FunctionScanExec::new(
            gs(2, true),
            vec![
                lit(Datum::Int8(1), SqlType::INT8),
                lit(Datum::Int8(1), SqlType::INT8),
            ],
        ));
        assert_eq!(f.run(&mut e).unwrap(), vec![vec![Datum::Int8(1)]]);
    }

    #[test]
    fn rewind_reevaluates_the_arguments() {
        let mut f = Fixture::with_params(1);
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor = Box::new(FunctionScanExec::new(
            gs(2, false),
            vec![int(1), param(0, SqlType::INT4)],
        ));
        ctx.params[0] = Datum::Int4(2);
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 2);
        assert!(e.next(&mut ctx).unwrap().is_none());
        ctx.params[0] = Datum::Int4(4);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 4);
        // 途中で rewind しても先頭から。
        e.rewind(&mut ctx).unwrap();
        e.next(&mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 4);
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn interrupts_are_checked_per_row() {
        let mut f = Fixture::new();
        f.interrupts.request_terminate();
        let mut e: BoxedExecutor = Box::new(FunctionScanExec::new(
            gs(2, false),
            vec![int(1), int(1_000_000)],
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::ADMIN_SHUTDOWN);
    }

    #[test]
    fn non_set_function_is_an_internal_error() {
        let func = functions_named("length").first().copied().unwrap();
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(FunctionScanExec::new(func, vec![]));
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            sqlstate::INTERNAL_ERROR
        );
    }

    #[test]
    fn build_from_plan() {
        let plan = PhysicalPlan::FunctionScan {
            func: gs(2, false),
            args: vec![int(1), int(3)],
        };
        let mut f = Fixture::new();
        let mut e = build(&plan);
        assert_eq!(f.run(&mut e).unwrap().len(), 3);
    }
}
