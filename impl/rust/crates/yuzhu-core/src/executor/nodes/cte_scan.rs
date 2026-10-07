//! CteScan ノード。`m4/05` §5.21。
//!
//! `ctx.ctes.slots[cte]` が共有ストア（最初の参照で CTE の plan を実行して行を溜める）。各 `CteScan` は自分の
//! 読み位置 `pos` を持ち、溜まった行は複製して返す。2 つの参照が交互に `next` を呼んでも、先に進んだ方が
//! ストアに溜め、遅れた方はストアから読む。課金は行をストアに入れるときの 1 回だけで、解放しない（文の終わりまで）。
//! `ctes[cte]` が `Param` に依存する（`free_params` が空でない）ときは `Error::internal`（D5-20。planner が
//! `0A000` で拒否する）。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use crate::error::{Error, Result};
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::mem::estimate_row_bytes;
use crate::executor::subplan::SlotExec;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::Row;

#[derive(Debug)]
pub struct CteScanExec {
    cte: usize,
    pos: usize,
}

impl CteScanExec {
    pub fn new(cte: usize) -> Self {
        CteScanExec { cte, pos: 0 }
    }
}

fn out_of_range(cte: usize) -> Error {
    Error::internal(format!("CTE {cte} out of range"))
}

/// CTE の Executor を借りる。未構築なら `ctes[cte]` から作る（`rewindable = true`）。
fn take_exec(ctx: &mut ExecCtx<'_>, cte: usize) -> Result<BoxedExecutor> {
    let query = ctx.query;
    let slot = ctx
        .ctes
        .slots
        .get_mut(cte)
        .ok_or_else(|| out_of_range(cte))?;
    match std::mem::replace(&mut slot.exec, SlotExec::Lent) {
        SlotExec::Idle(e) => Ok(e),
        SlotExec::Unbuilt => {
            let plan = query.ctes.get(cte).ok_or_else(|| out_of_range(cte))?;
            if !free_params(plan, Some(query)).is_empty() {
                slot.exec = SlotExec::Unbuilt;
                return Err(Error::internal(
                    "a CTE that depends on outer parameters is not supported",
                ));
            }
            let ib = ctx
                .instr
                .as_ref()
                .map(|i| crate::executor::instrument::InstrBuild::new(query, i));
            let env = BuildEnv {
                query: Some(query),
                instr: ib.as_ref(),
            };
            Ok(build_scoped(plan, &env, true))
        }
        SlotExec::Lent => Err(Error::internal("CTE re-entered while it was running")),
    }
}

impl Executor for CteScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        {
            let slot = ctx
                .ctes
                .slots
                .get(self.cte)
                .ok_or_else(|| out_of_range(self.cte))?;
            if let Some(row) = slot.rows.get(self.pos) {
                self.pos += 1;
                return Ok(Some(row.clone()));
            }
            if slot.done {
                return Ok(None);
            }
        }
        ctx.check_interrupts()?;
        let mut exec = take_exec(ctx, self.cte)?;
        let r = exec.next(ctx);
        let slot = ctx
            .ctes
            .slots
            .get_mut(self.cte)
            .ok_or_else(|| out_of_range(self.cte))?;
        slot.exec = SlotExec::Idle(exec);
        match r? {
            None => {
                slot.done = true;
                Ok(None)
            }
            Some(row) => {
                // 課金に失敗したら行は入れない。
                ctx.mem.charge(estimate_row_bytes(&row))?;
                slot.rows.push(row.clone());
                self.pos += 1;
                Ok(Some(row))
            }
        }
    }

    /// 読み位置だけ戻す（共有ストアは触らない。ほかの `CteScan` の進み方に影響しない）。
    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.pos = 0;
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
    let PhysicalPlan::CteScan { cte } = plan else {
        return Box::new(super::UnsupportedExec::new("CTE Scan"));
    };
    Box::new(CteScanExec::new(*cte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, op, param};
    use crate::executor::nodes::test_util::Fixture;
    use crate::planner::physical::{PhysicalPlan as P, PhysicalQuery};
    use crate::types::{Datum, SqlType};

    fn values(n: i32) -> P {
        P::Values {
            rows: (0..n).map(|i| vec![int(i)]).collect(),
        }
    }

    fn fixture(cte: P) -> Fixture {
        let mut q = PhysicalQuery::single(P::CteScan { cte: 0 }, vec![]);
        q.ctes.push(cte);
        Fixture::with_query(q)
    }

    fn ints(rows: &[Row]) -> Vec<i64> {
        rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
    }

    #[test]
    fn reads_all_rows_and_runs_the_cte_once() {
        let mut f = fixture(values(3));
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        let mut b = CteScanExec::new(0);
        let mut got_a = vec![];
        while let Some(r) = a.next(&mut ctx).unwrap() {
            got_a.push(r);
        }
        let mut got_b = vec![];
        while let Some(r) = b.next(&mut ctx).unwrap() {
            got_b.push(r);
        }
        assert_eq!(ints(&got_a), [0, 1, 2]);
        assert_eq!(got_a, got_b);
        // 1 回しか実行されない: ストアは 3 行だけ。
        assert_eq!(ctx.ctes.slots[0].rows.len(), 3);
        assert!(ctx.ctes.slots[0].done);
        assert!(a.next(&mut ctx).unwrap().is_none());
    }

    #[test]
    fn two_scans_interleave() {
        let mut f = fixture(values(3));
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        let mut b = CteScanExec::new(0);
        let mut seq = vec![];
        for turn in 0..8 {
            let s = if turn % 3 == 2 { &mut b } else { &mut a };
            seq.push(s.next(&mut ctx).unwrap().map(|r| r[0].as_i64().unwrap()));
        }
        // a: 0, 1; b: 0; a: 2, None; b: 1 ...
        assert_eq!(seq[..3], [Some(0), Some(1), Some(0)]);
        assert_eq!(ctx.ctes.slots[0].rows.len(), 3);
        let mut rest_b = vec![];
        while let Some(r) = b.next(&mut ctx).unwrap() {
            rest_b.push(r[0].as_i64().unwrap());
        }
        // b は最後まで読める（読んだ分と合わせて 0, 1, 2）。
        let read_b: Vec<i64> = seq
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 3 == 2)
            .filter_map(|(_, v)| *v)
            .collect();
        let all_b: Vec<i64> = read_b.into_iter().chain(rest_b).collect();
        assert_eq!(all_b, [0, 1, 2]);
    }

    #[test]
    fn rewind_replays_without_touching_the_store() {
        let mut f = fixture(values(2));
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        a.next(&mut ctx).unwrap();
        a.rewind(&mut ctx).unwrap();
        let mut got = vec![];
        while let Some(r) = a.next(&mut ctx).unwrap() {
            got.push(r);
        }
        assert_eq!(ints(&got), [0, 1]);
        let n = ctx.ctes.slots[0].rows.len();
        a.rewind(&mut ctx).unwrap();
        while a.next(&mut ctx).unwrap().is_some() {}
        assert_eq!(ctx.ctes.slots[0].rows.len(), n);
    }

    #[test]
    fn empty_cte() {
        let mut f = fixture(values(0));
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        assert!(a.next(&mut ctx).unwrap().is_none());
        assert!(a.next(&mut ctx).unwrap().is_none());
    }

    #[test]
    fn stored_rows_are_charged_once_and_never_released() {
        let mut f = fixture(values(3));
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        let mut b = CteScanExec::new(0);
        while a.next(&mut ctx).unwrap().is_some() {}
        let used = ctx.mem.used();
        assert_eq!(used, 3 * estimate_row_bytes(&vec![Datum::Int4(0)]));
        while b.next(&mut ctx).unwrap().is_some() {}
        b.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), used);
    }

    #[test]
    fn exceeding_the_budget_is_53200() {
        let row = estimate_row_bytes(&vec![Datum::Int4(0)]);
        for (limit, ok) in [(3 * row, true), (3 * row - 1, false)] {
            let mut f = fixture(values(3));
            f.mem_limit = limit;
            let mut ctx = f.ctx();
            let mut a = CteScanExec::new(0);
            let mut res = Ok(());
            loop {
                match a.next(&mut ctx) {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        res = Err(e);
                        break;
                    }
                }
            }
            assert_eq!(res.is_ok(), ok, "limit {limit}");
            if let Err(e) = res {
                assert_eq!(e.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
                // 失敗した行はストアに入らない。
                assert_eq!(ctx.ctes.slots[0].rows.len(), 2);
            }
        }
    }

    #[test]
    fn param_dependent_cte_is_an_internal_error() {
        let plan = P::Filter {
            input: Box::new(values(2)),
            predicate: op(&GT, col(0, SqlType::INT4), param(0, SqlType::INT4)),
        };
        let mut f = fixture(plan);
        f.query.n_params = 1;
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        let e = a.next(&mut ctx).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn interrupts_are_checked_when_filling() {
        let mut f = fixture(values(3));
        f.interrupts.request_cancel();
        let mut ctx = f.ctx();
        let mut a = CteScanExec::new(0);
        let e = a.next(&mut ctx).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(ctx.ctes.slots[0].rows.is_empty());
    }

    #[test]
    fn executor_is_returned_after_an_error() {
        // 失敗後に Executor がスロットへ戻る（Lent のまま残らない）。
        let mut f = fixture(values(2));
        let mut ctx = f.ctx();
        ctx.mem = crate::executor::mem::MemBudget::new(0);
        let mut a = CteScanExec::new(0);
        assert!(a.next(&mut ctx).is_err());
        assert!(matches!(ctx.ctes.slots[0].exec, SlotExec::Idle(_)));
    }

    #[test]
    fn build_from_plan() {
        let mut f = fixture(values(2));
        let mut e = build(&P::CteScan { cte: 0 });
        assert_eq!(ints(&f.run(&mut e).unwrap()), [0, 1]);
    }
}
