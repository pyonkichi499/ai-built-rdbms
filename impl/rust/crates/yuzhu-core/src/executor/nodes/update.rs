//! Executor node: UPDATE (`m2.md` §4.7, §6.10). Input rows are the target
//! table's user columns followed by the `ctid`.
//!
//! 担当 H2 が実装する。

use crate::analyzer::{BoundCheck, UpdateSource};
use crate::error::{Error, Result};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::storage::RelHandle;
use crate::types::Row;

pub struct UpdateExec {
    #[allow(dead_code)]
    rel: RelHandle,
    table_name: String,
    #[allow(dead_code)]
    input: BoxedExecutor,
    #[allow(dead_code)]
    assignments: Vec<(usize, UpdateSource)>,
    #[allow(dead_code)]
    checks: Vec<BoundCheck>,
    #[allow(dead_code)]
    not_null: Vec<bool>,
    count: u64,
}

impl std::fmt::Debug for UpdateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateExec")
            .field("table", &self.table_name)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
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
        }
    }
}

impl Executor for UpdateExec {
    fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        Err(Error::not_supported("UPDATE is not implemented yet"))
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}
