//! Executor node: UPDATE (`m2.md` §4.7, §5.5). Input rows are the target
//! table's user columns followed by the `ctid`.
//!
//! Per input row: evaluate the SET expressions over the old row (so
//! `SET a = b, b = a` swaps) → NOT NULL → CHECK (before touching the heap)
//! → `storage.update`. Emits no rows; the count is `rows_affected`.

use crate::analyzer::{BoundCheck, UpdateSource};
use crate::error::{Error, Result, sqlstate};
use crate::executor::eval::eval;
use crate::executor::nodes::insert::enforce_constraints;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::storage::{RelHandle, TmResult};
use crate::types::{Datum, Row, Tid};

pub struct UpdateExec {
    rel: RelHandle,
    table_name: String,
    input: BoxedExecutor,
    assignments: Vec<(usize, UpdateSource)>,
    checks: Vec<BoundCheck>,
    not_null: Vec<bool>,
    count: u64,
    done: bool,
}

impl std::fmt::Debug for UpdateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateExec")
            .field("table", &self.table_name)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

/// Splits an `[user columns..., ctid]` input row.
pub(crate) fn split_ctid(mut input: Row, natts: usize) -> Result<(Row, Tid)> {
    if input.len() != natts + 1 {
        return Err(Error::internal(format!(
            "expected {} input columns (user columns and ctid), got {}",
            natts + 1,
            input.len()
        )));
    }
    match input.pop() {
        Some(Datum::Tid(t)) => Ok((input, t)),
        other => Err(Error::internal(format!(
            "last input column must be a ctid, got {other:?}"
        ))),
    }
}

/// The error for a row changed twice by one command (`27000`).
pub(crate) fn already_modified(verb: &str) -> Error {
    Error::new(
        sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION,
        format!(
            "tuple to be {verb} was already modified by an operation triggered by the current command"
        ),
    )
    .with_hint(
        "Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.",
    )
}

impl UpdateExec {
    pub fn new(
        rel: RelHandle,
        table_name: String,
        input: BoxedExecutor,
        assignments: Vec<(usize, UpdateSource)>,
        checks: Vec<BoundCheck>,
        not_null: Vec<bool>,
    ) -> Self {
        UpdateExec {
            rel,
            table_name,
            input,
            assignments,
            checks,
            not_null,
            count: 0,
            done: false,
        }
    }

    fn new_row(&self, old: &Row, ctx: &ExecCtx<'_>) -> Result<Row> {
        let empty = Row::new();
        let mut new = old.clone();
        for (idx, src) in &self.assignments {
            let v = match src {
                UpdateSource::Expr(e) => eval(e, old, ctx)?,
                UpdateSource::Default(Some(e)) => eval(e, &empty, ctx)?,
                UpdateSource::Default(None) => Datum::Null,
            };
            *new.get_mut(*idx).ok_or_else(|| {
                Error::internal(format!("assignment to column {idx} out of range"))
            })? = v;
        }
        Ok(new)
    }

    fn update_one(&mut self, input: Row, ctx: &mut ExecCtx<'_>) -> Result<()> {
        ctx.check_interrupts()?;
        let (old, tid) = split_ctid(input, self.rel.desc.attrs.len())?;
        let new = self.new_row(&old, ctx)?;
        enforce_constraints(self.rel.oid, &new, &self.not_null, &self.checks, ctx)?;
        let w = ctx.write_ctx()?;
        let out = ctx.storage.update(&self.rel, &w, ctx.snapshot, tid, &new)?;
        match out.result {
            TmResult::Ok => self.count += 1,
            TmResult::SelfModified { cmax } => {
                if cmax != w.cid {
                    return Err(already_modified("updated"));
                }
            }
            other => {
                return Err(Error::internal(format!(
                    "unexpected result of update on table \"{}\": {other:?}",
                    self.table_name
                )));
            }
        }
        Ok(())
    }
}

impl Executor for UpdateExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        while let Some(input) = self.input.next(ctx)? {
            self.update_one(input, ctx)?;
        }
        Ok(None)
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}
