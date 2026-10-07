//! Materialize ノード（`m4/05` §5.12）。入力の行を遅延して溜め、`rewind` で読み直せるようにする。
//!
//! NestedLoop の inner に置かれ、外側の行ごとに先頭から読み直される。溜めた行は `ctx.mem` に課金する。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use crate::error::Result;
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::mem::estimate_row_bytes;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::Row;

/// `Materialize` を実行する Executor を作る。`reuse`（C-6）は子の部分木の `free_params` が空かで決める。
pub(in crate::executor) fn build(
    plan: &PhysicalPlan,
    env: &BuildEnv<'_>,
    rewindable: bool,
) -> BoxedExecutor {
    let PhysicalPlan::Materialize { input } = plan else {
        unreachable!("materialize::build called with {plan:?}");
    };
    Box::new(MaterializeExec::new(
        build_scoped(input, env, rewindable),
        free_params(input, env.query).is_empty(),
    ))
}

pub struct MaterializeExec {
    input: BoxedExecutor,
    /// 子が外側の `Param` に依存しないなら、`rewind` で溜めた行を読み直す。
    reuse: bool,
    store: Vec<Row>,
    child_done: bool,
    pos: usize,
    /// `ctx.mem` に課金した合計。
    charged: usize,
}

impl std::fmt::Debug for MaterializeExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaterializeExec")
            .field("reuse", &self.reuse)
            .field("stored", &self.store.len())
            .finish_non_exhaustive()
    }
}

impl MaterializeExec {
    pub fn new(input: BoxedExecutor, reuse: bool) -> Self {
        MaterializeExec {
            input,
            reuse,
            store: Vec::new(),
            child_done: false,
            pos: 0,
            charged: 0,
        }
    }
}

impl Executor for MaterializeExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if let Some(r) = self.store.get(self.pos) {
            self.pos += 1;
            return Ok(Some(r.clone()));
        }
        if self.child_done {
            return Ok(None);
        }
        ctx.check_interrupts()?;
        let Some(row) = self.input.next(ctx)? else {
            self.child_done = true;
            return Ok(None);
        };
        let bytes = estimate_row_bytes(&row);
        ctx.mem.charge(bytes)?;
        self.charged += bytes;
        self.store.push(row.clone());
        self.pos += 1;
        Ok(Some(row))
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.reuse {
            // 子は `rewind` しない。まだ読み切っていなければ、続きは子の現在位置から読む。
            self.pos = 0;
            return Ok(());
        }
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.store.clear();
        self.child_done = false;
        self.pos = 0;
        self.input.rewind(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::Datum;

    fn ints(rows: &[Row]) -> Vec<i32> {
        rows.iter()
            .map(|r| match r[0] {
                Datum::Int4(v) => v,
                ref d => panic!("{d:?}"),
            })
            .collect()
    }

    #[test]
    fn reads_lazily_and_rereads_on_rewind() {
        let (child, reads, rewinds) = CountingExec::ints(4);
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), true));
        let mut ctx = f.ctx();
        // 遅延: 作っただけでは読まない。
        assert_eq!(reads.get(), 0);
        assert_eq!(ints(&drain(&mut e, &mut ctx).unwrap()), vec![0, 1, 2, 3]);
        assert_eq!(reads.get(), 4);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ints(&drain(&mut e, &mut ctx).unwrap()), vec![0, 1, 2, 3]);
        // reuse: 子は読み直さず、`rewind` もしない。
        assert_eq!((reads.get(), rewinds.get()), (4, 0));
        assert!(ctx.mem.used() > 0);
    }

    #[test]
    fn partial_read_then_rewind_continues_from_the_child() {
        let (child, reads, _) = CountingExec::ints(5);
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), true));
        let mut ctx = f.ctx();
        assert_eq!(e.next(&mut ctx).unwrap().unwrap(), vec![Datum::Int4(0)]);
        assert_eq!(e.next(&mut ctx).unwrap().unwrap(), vec![Datum::Int4(1)]);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ints(&drain(&mut e, &mut ctx).unwrap()), vec![0, 1, 2, 3, 4]);
        assert_eq!(reads.get(), 5);
        // 枯渇後は子を呼ばずに None を返し続ける。
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert_eq!(reads.get(), 5);
    }

    #[test]
    fn without_reuse_rewind_rebuilds_and_returns_the_charge() {
        let (child, reads, rewinds) = CountingExec::ints(3);
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), false));
        let mut ctx = f.ctx();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 3);
        let used = ctx.mem.used();
        assert!(used > 0);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        assert_eq!(rewinds.get(), 1);
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 3);
        assert_eq!(reads.get(), 6);
        assert_eq!(ctx.mem.used(), used);
    }

    #[test]
    fn exceeding_the_budget_is_53200() {
        let (child, _, _) = CountingExec::ints(100);
        let mut f = Fixture::new();
        f.mem_limit = 200;
        let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), true));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::OUT_OF_MEMORY);
        assert!(err.message.starts_with("out of memory"));
    }

    #[test]
    fn exact_limit_succeeds_and_one_byte_less_fails() {
        let rows: Vec<Row> = (0..3).map(|i| vec![Datum::Int4(i)]).collect();
        let total: usize = rows.iter().map(estimate_row_bytes).sum();
        for (limit, ok) in [(total, true), (total - 1, false)] {
            let (child, _, _) = CountingExec::new(rows.clone());
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), true));
            assert_eq!(f.run(&mut e).is_ok(), ok, "limit {limit}");
        }
    }

    #[test]
    fn interrupts_stop_the_read() {
        let (child, _, _) = CountingExec::ints(10);
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(MaterializeExec::new(Box::new(child), true));
        f.interrupts.request_terminate();
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::ADMIN_SHUTDOWN);
    }
}
