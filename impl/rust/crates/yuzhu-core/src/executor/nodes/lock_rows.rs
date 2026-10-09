//! Executor node: `LockRows`（`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE`。`m5/02` §5.7、D8）。
//!
//! 入力の行ごとに、各 `RowMark` の `ctid` の行ロックを取る。ロックを取る間に最新版へ更新されていたら（RC）、
//! 対象表の部分を最新版に差し替えて `recheck`（元の WHERE と結合条件）を評価し直す（EvalPlanQual の簡易版）。
//! 他の表の列は入力のまま固定する。出力の形は入力と同じ。

use crate::error::{Error, Result, sqlstate};
use crate::executor::eval::eval_pred;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{PhysExpr, RowMark};
use crate::storage::{HeapTuple, RowWait, TmResult};
use crate::types::{Datum, Row, Tid};

/// RR で並行更新・削除に当たったときの 40001（`concurrent update` の文言は削除でも同じ。RW-D13）。
pub(crate) fn serialize_failure(what: &str) -> Error {
    Error::new(
        sqlstate::SERIALIZATION_FAILURE,
        format!("could not serialize access due to concurrent {what}"),
    )
}

/// `NOWAIT`（`RowWait::Error`）で行ロックが取れなかったときの 55P03。
pub(crate) fn lock_not_available(table_name: &str) -> Error {
    Error::new(
        sqlstate::LOCK_NOT_AVAILABLE,
        format!("could not obtain lock on row in relation \"{table_name}\""),
    )
}

pub struct LockRowsExec {
    input: BoxedExecutor,
    marks: Vec<RowMark>,
    recheck: Option<PhysExpr>,
}

impl std::fmt::Debug for LockRowsExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockRowsExec")
            .field("marks", &self.marks.len())
            .finish_non_exhaustive()
    }
}

impl LockRowsExec {
    pub fn new(input: BoxedExecutor, marks: Vec<RowMark>, recheck: Option<PhysExpr>) -> Self {
        LockRowsExec {
            input,
            marks,
            recheck,
        }
    }

    /// 1 行ぶんの行ロックを取る。行を捨てるなら `Ok(None)`、通すなら最新版に差し替えた行（差し替えが無ければ入力のまま）。
    fn lock_row(&self, mut row: Row, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let w = ctx.write_ctx()?;
        let snap = ctx.snapshot;
        let rr = ctx.xact_snapshot.is_some();
        let mut replaced: Vec<(usize, HeapTuple)> = Vec::new();
        for (i, m) in self.marks.iter().enumerate() {
            let tid = match row.get(m.tid_col) {
                Some(Datum::Tid(t)) => *t,
                // 外部結合の NULL 側など。
                Some(Datum::Null) => continue,
                other => {
                    return Err(Error::internal(format!(
                        "LockRows mark column {} must be a ctid, got {other:?}",
                        m.tid_col
                    )));
                }
            };
            let wait = ctx.wait.ok_or_else(|| {
                Error::internal(
                    "row locks need a wait context, but none is wired into this executor",
                )
            })?;
            let lk = ctx
                .storage
                .lock_tuple(&m.rel, &w, snap, tid, m.mode, m.wait, !rr, &wait)?;
            match lk.result {
                TmResult::Ok => {
                    if let Some(t) = lk.latest {
                        replaced.push((i, t));
                    }
                }
                TmResult::WouldBlock => {
                    if m.wait == RowWait::Error {
                        return Err(lock_not_available(&m.table_name));
                    }
                    // SKIP LOCKED: この行を捨てる（他の mark で取ったロックは残る）。
                    return Ok(None);
                }
                TmResult::Deleted { .. } => {
                    if rr {
                        return Err(serialize_failure("update"));
                    }
                    return Ok(None);
                }
                TmResult::Updated { .. } => return Err(serialize_failure("update")),
                // 同じコマンドまたは後のコマンドで更新済み（PostgreSQL の nodeLockRows と同じ）。
                TmResult::SelfModified { .. } => return Ok(None),
                TmResult::Invisible => {
                    return Err(Error::internal("attempted to lock invisible tuple"));
                }
                TmResult::BeingModified { .. } => {
                    return Err(Error::internal(
                        "lock_tuple returned BeingModified although it was told to wait",
                    ));
                }
            }
        }
        if replaced.is_empty() {
            return Ok(Some(row));
        }
        for (i, t) in replaced {
            let m = &self.marks[i];
            replace_target(&mut row, m.cols.clone(), m.tid_col, t);
        }
        if let Some(q) = &self.recheck
            && eval_pred(q, &row, ctx)? != Some(true)
        {
            // 条件を満たさない。飛ばす（ロックは残る）。
            return Ok(None);
        }
        Ok(Some(row))
    }
}

/// 行の対象表の部分（ユーザー列と ctid）を `t` に差し替える。
pub(crate) fn replace_target(
    row: &mut Row,
    cols: std::ops::Range<usize>,
    tid_col: usize,
    t: HeapTuple,
) {
    for (slot, v) in row[cols].iter_mut().zip(t.row) {
        *slot = v;
    }
    row[tid_col] = Datum::Tid(Tid {
        block: t.tid.block,
        offset: t.tid.offset,
    });
}

impl Executor for LockRowsExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            if let Some(out) = self.lock_row(row, ctx)? {
                return Ok(Some(out));
            }
        }
        Ok(None)
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        Err(Error::internal("rewind is not supported for LockRows"))
    }
}
