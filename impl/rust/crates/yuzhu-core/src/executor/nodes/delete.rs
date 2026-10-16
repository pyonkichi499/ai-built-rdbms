//! Executor node: DELETE (`m4/05` §7.7). 入力の行は対象表のユーザー列（`n_user_cols` 個）++ ctid。

use crate::error::{Error, Result};
use crate::executor::dml::{already_modified, delete_row, tid_at};
use crate::executor::eval::eval;
use crate::executor::eval::eval_pred;
use crate::executor::nodes::lock_rows::{replace_target, serialize_failure};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{PhysExpr, RecheckSpec};
use crate::storage::{RelHandle, RowWait, TmResult};
use crate::txn::LockTupleMode;
use crate::types::Row;

pub struct DeleteExec {
    rel: RelHandle,
    input: BoxedExecutor,
    n_user_cols: usize,
    returning: Option<Vec<PhysExpr>>,
    recheck: Option<RecheckSpec>,
    count: u64,
    done: bool,
}

impl std::fmt::Debug for DeleteExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeleteExec")
            .field("rel", &self.rel.oid)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl DeleteExec {
    /// 並行更新に当たったときに最新版で条件を評価し直す情報（RC の `EvalPlanQual`）。
    #[must_use]
    pub fn with_recheck(mut self, recheck: Option<RecheckSpec>) -> Self {
        self.recheck = recheck;
        self
    }

    pub fn new(rel: RelHandle, input: BoxedExecutor, n_user_cols: usize) -> Self {
        DeleteExec::with_returning(rel, input, n_user_cols, None)
    }

    pub fn with_returning(
        rel: RelHandle,
        input: BoxedExecutor,
        n_user_cols: usize,
        returning: Option<Vec<PhysExpr>>,
    ) -> Self {
        DeleteExec {
            rel,
            input,
            n_user_cols,
            returning,
            recheck: None,
            count: 0,
            done: false,
        }
    }

    /// 1 行を削除する。RETURNING があり、削除した行なら古い行に対する値を返す。
    ///
    /// 並行更新に当たったとき（RC）は `EvalPlanQual` の簡易版: 最新版を `Exclusive` でロックし、`recheck.qual` を
    /// 最新版で評価し直して、通れば最新版を削除する。RR は 40001（`m5/02` §5.6）。
    fn delete_one(&mut self, input: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if input.len() < self.n_user_cols + 1 {
            return Err(Error::internal(format!(
                "expected at least {} input columns (user columns and ctid), got {}",
                self.n_user_cols + 1,
                input.len()
            )));
        }
        let mut cur_tid = tid_at(input, self.n_user_cols)?;
        let mut old: Row = input[..self.n_user_cols].to_vec();
        let w = ctx.write_ctx()?;
        let snap = ctx.snapshot;
        let rr = ctx.xact_snapshot.is_some();
        loop {
            ctx.check_interrupts()?;
            match delete_row(ctx, &self.rel, &w, snap, cur_tid, &old, None)? {
                TmResult::Ok => {
                    self.count += 1;
                    break;
                }
                TmResult::SelfModified { cmax } => {
                    if cmax != w.cid {
                        return Err(already_modified("deleted"));
                    }
                    return Ok(None);
                }
                TmResult::Deleted { .. } => {
                    if rr {
                        return Err(serialize_failure("delete"));
                    }
                    return Ok(None);
                }
                TmResult::Updated { .. } => {
                    if rr {
                        return Err(serialize_failure("update"));
                    }
                    let wait = ctx.wait.ok_or_else(|| {
                        Error::internal("EvalPlanQual needs a wait context, but none is wired")
                    })?;
                    let lk = ctx.storage.lock_tuple(
                        &self.rel,
                        &w,
                        snap,
                        cur_tid,
                        LockTupleMode::Exclusive,
                        RowWait::Block,
                        true,
                        &wait,
                    )?;
                    match lk.result {
                        TmResult::Ok => {
                            let latest = lk.latest.ok_or_else(|| {
                                Error::internal("lock_tuple followed updates but returned no tuple")
                            })?;
                            let mut r2 = input.clone();
                            let tid_col = self.n_user_cols;
                            old.clone_from(&latest.row);
                            cur_tid = latest.tid;
                            replace_target(&mut r2, 0..self.n_user_cols, tid_col, latest);
                            if let Some(q) = self.recheck.as_ref().and_then(|s| s.qual.as_ref())
                                && eval_pred(q, &r2, ctx)? != Some(true)
                            {
                                return Ok(None);
                            }
                        }
                        TmResult::Deleted { .. } => return Ok(None),
                        TmResult::SelfModified { cmax } => {
                            if cmax != w.cid {
                                return Err(already_modified("deleted"));
                            }
                            return Ok(None);
                        }
                        other => {
                            return Err(Error::internal(format!(
                                "unexpected result of locking the latest version in relation {}: {other:?}",
                                self.rel.oid
                            )));
                        }
                    }
                }
                other => {
                    return Err(Error::internal(format!(
                        "unexpected result of delete on relation {}: {other:?}",
                        self.rel.oid
                    )));
                }
            }
        }
        let Some(returning) = &self.returning else {
            return Ok(None);
        };
        returning
            .iter()
            .map(|e| eval(e, &old, ctx))
            .collect::<Result<Row>>()
            .map(Some)
    }
}

impl Executor for DeleteExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        while let Some(input) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            if let Some(out) = self.delete_one(&input, ctx)? {
                return Ok(Some(out));
            }
        }
        self.done = true;
        Ok(None)
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        Err(Error::internal("rewind is not supported for DML nodes"))
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::catalog::fake::table_def;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{col, int, lit};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::{Datum, SqlType, Tid};

    fn rel() -> RelHandle {
        let col = ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
            identity: None,
        };
        RelHandle::from_table(&table_def(7, "t", vec![col], vec![]))
    }

    fn ctid(offset: u16) -> PhysExpr {
        lit(
            Datum::Tid(Tid { block: 0, offset }),
            SqlType::of(crate::types::oid::TID),
        )
    }

    fn delete(rows: Vec<Vec<PhysExpr>>, returning: Option<Vec<PhysExpr>>) -> BoxedExecutor {
        Box::new(DeleteExec::with_returning(
            rel(),
            Box::new(ValuesExec::new(rows)),
            1,
            returning,
        ))
    }

    #[test]
    fn deletes_and_counts_rows() {
        let mut f = Fixture::new();
        f.storage.add_row(7, vec![Datum::Int4(1)]);
        f.storage.add_row(7, vec![Datum::Int4(2)]);
        let mut e = delete(vec![vec![int(1), ctid(1)]], None);
        assert!(f.run(&mut e).unwrap().is_empty());
        assert_eq!(e.rows_affected(), 1);
        assert_eq!(f.storage.rows(7), vec![vec![Datum::Int4(2)]]);
    }

    #[test]
    fn returning_uses_the_old_row() {
        let mut f = Fixture::new();
        f.storage.add_row(7, vec![Datum::Int4(5)]);
        let mut e = delete(
            vec![vec![int(5), ctid(1)]],
            Some(vec![col(0, SqlType::INT4)]),
        );
        assert_eq!(f.run(&mut e).unwrap(), vec![vec![Datum::Int4(5)]]);
    }

    #[test]
    fn malformed_rows_rewind_and_interrupts() {
        let mut f = Fixture::new();
        // 幅が合わない / 最後の列が ctid でない。
        let e = f.run(&mut delete(vec![vec![int(1)]], None)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        let e = f
            .run(&mut delete(vec![vec![int(1), int(2)]], None))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        // rewind は XX000。
        let mut ctx = f.ctx();
        let mut e = delete(vec![], None);
        assert_eq!(
            e.rewind(&mut ctx).unwrap_err().sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        drop(ctx);
        // 入力 1 行ごとに割り込みを検査する。
        f.interrupts.request_terminate();
        let e = f
            .run(&mut delete(vec![vec![int(1), ctid(1)]], None))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ADMIN_SHUTDOWN);
    }
}
