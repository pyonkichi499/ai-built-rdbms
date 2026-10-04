//! Executor node: DELETE (`m2.md` §4.7, §6.10). Input rows are the target
//! table's user columns followed by the `ctid`.
//!
//! 担当 H2 が実装する。

use crate::error::{Error, Result};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::storage::RelHandle;
use crate::types::Row;

pub struct DeleteExec {
    #[allow(dead_code)]
    rel: RelHandle,
    #[allow(dead_code)]
    input: BoxedExecutor,
    count: u64,
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
        }
    }
}

impl Executor for DeleteExec {
    fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        Err(Error::not_supported("DELETE is not implemented yet"))
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}
