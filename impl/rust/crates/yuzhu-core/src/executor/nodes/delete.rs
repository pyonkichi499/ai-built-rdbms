//! Executor node: DELETE (`m2.md` §4.7, §5.5). Input rows are the target
//! table's user columns followed by the `ctid`.

use crate::error::{Error, Result};
use crate::executor::nodes::update::{already_modified, split_ctid};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::storage::{RelHandle, TmResult};
use crate::types::Row;

pub struct DeleteExec {
    rel: RelHandle,
    input: BoxedExecutor,
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
    pub fn new(rel: RelHandle, input: BoxedExecutor) -> Self {
        DeleteExec {
            rel,
            input,
            count: 0,
            done: false,
        }
    }

    fn delete_one(&mut self, input: Row, ctx: &mut ExecCtx<'_>) -> Result<()> {
        ctx.check_interrupts()?;
        let (_, tid) = split_ctid(input, self.rel.desc.attrs.len())?;
        let w = ctx.write_ctx()?;
        match ctx.storage.delete(&self.rel, &w, ctx.snapshot, tid)? {
            TmResult::Ok => self.count += 1,
            TmResult::SelfModified { cmax } => {
                if cmax != w.cid {
                    return Err(already_modified("deleted"));
                }
            }
            other => {
                return Err(Error::internal(format!(
                    "unexpected result of delete on relation {}: {other:?}",
                    self.rel.oid
                )));
            }
        }
        Ok(())
    }
}

impl Executor for DeleteExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        while let Some(input) = self.input.next(ctx)? {
            self.delete_one(input, ctx)?;
        }
        Ok(None)
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}
