//! Append ノード。`m4/05` §5.19。
//!
//! `inputs` を順に最後まで読む。出力の列は全入力で同じ（型変換は planner が Project で済ませる）。
//! `rewind`: `idx = 0`、読み始めた入力だけ `rewind` する。課金なし。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use crate::error::Result;
use crate::executor::build::{BuildEnv, build_scoped};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::Row;

pub struct AppendExec {
    inputs: Vec<BoxedExecutor>,
    idx: usize,
    /// 1 行でも読もうとした入力の数（先頭から）。`rewind` はここまでだけ戻す。
    touched: usize,
}

impl std::fmt::Debug for AppendExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppendExec")
            .field("inputs", &self.inputs.len())
            .field("idx", &self.idx)
            .finish_non_exhaustive()
    }
}

impl AppendExec {
    pub fn new(inputs: Vec<BoxedExecutor>) -> Self {
        AppendExec {
            inputs,
            idx: 0,
            touched: 0,
        }
    }
}

impl Executor for AppendExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while self.idx < self.inputs.len() {
            ctx.check_interrupts()?;
            self.touched = self.touched.max(self.idx + 1);
            if let Some(row) = self.inputs[self.idx].next(ctx)? {
                return Ok(Some(row));
            }
            self.idx += 1;
        }
        Ok(None)
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        for input in self.inputs.iter_mut().take(self.touched) {
            input.rewind(ctx)?;
        }
        self.idx = 0;
        self.touched = 0;
        Ok(())
    }
}

pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::Append { inputs } = plan else {
        return Box::new(super::UnsupportedExec::new("Append"));
    };
    Box::new(AppendExec::new(
        inputs
            .iter()
            .map(|p| build_scoped(p, env, rewindable))
            .collect(),
    ))
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::Datum;

    fn rows(v: &[i32]) -> Vec<Row> {
        v.iter().map(|i| vec![Datum::Int4(*i)]).collect()
    }

    #[test]
    fn concatenates_inputs_in_order() {
        let mut f = Fixture::new();
        let (a, _, _) = CountingExec::new(rows(&[1, 2]));
        let (b, _, _) = CountingExec::new(vec![]);
        let (c, _, _) = CountingExec::new(rows(&[3]));
        let mut e: BoxedExecutor =
            Box::new(AppendExec::new(vec![Box::new(a), Box::new(b), Box::new(c)]));
        assert_eq!(f.run(&mut e).unwrap(), rows(&[1, 2, 3]));
    }

    #[test]
    fn no_inputs_is_empty_and_fused() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor = Box::new(AppendExec::new(vec![]));
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert!(e.next(&mut ctx).unwrap().is_none());
    }

    #[test]
    fn exhausted_inputs_are_not_read_again() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (a, ra, _) = CountingExec::new(rows(&[1]));
        let mut e: BoxedExecutor = Box::new(AppendExec::new(vec![Box::new(a)]));
        drain(&mut e, &mut ctx).unwrap();
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert_eq!(ra.get(), 1);
    }

    #[test]
    fn rewind_only_rewinds_started_inputs() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let (a, _, rwa) = CountingExec::new(rows(&[1, 2]));
        let (b, _, rwb) = CountingExec::new(rows(&[3]));
        let mut e: BoxedExecutor = Box::new(AppendExec::new(vec![Box::new(a), Box::new(b)]));
        e.next(&mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!((rwa.get(), rwb.get()), (1, 0));
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), rows(&[1, 2, 3]));
        e.rewind(&mut ctx).unwrap();
        assert_eq!((rwa.get(), rwb.get()), (2, 1));
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), rows(&[1, 2, 3]));
        // 未開始なら何もしない。
        let (c, _, rwc) = CountingExec::new(rows(&[9]));
        let mut e: BoxedExecutor = Box::new(AppendExec::new(vec![Box::new(c)]));
        e.rewind(&mut ctx).unwrap();
        assert_eq!(rwc.get(), 0);
    }

    #[test]
    fn interrupts_are_checked_between_reads() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (a, reads, _) = CountingExec::new(rows(&[1, 2, 3]));
        let mut e: BoxedExecutor = Box::new(AppendExec::new(vec![Box::new(a)]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert_eq!(reads.get(), 0);
    }
}
