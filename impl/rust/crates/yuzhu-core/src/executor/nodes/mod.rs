//! Executor nodes. (Owned by the executor implementer.)
//!
//! 各ノードは `m4/02` §3.7.1 の表に従って `next` / `rewind` を持つ。★ スタブの 12 ファイル
//! （`nested_loop` ほか）は `build` が [`UnsupportedExec`] を返し、X1〜X3 が本実装に置き換える。

pub mod aggregate;
pub mod append;
pub mod cte_scan;
pub mod delete;
pub mod distinct;
pub mod filter;
pub mod function_scan;
pub mod group_aggregate;
pub mod hash_aggregate;
pub mod hash_join;
pub mod hash_setop;
pub mod index_scan;
pub mod insert;
pub mod limit;
pub mod materialize;
pub mod nested_loop;
pub mod project;
pub mod result;
pub mod seq_scan;
pub mod sort;
pub mod unique;
pub mod update;
pub mod values;

#[cfg(test)]
pub(crate) mod test_util;

pub use delete::DeleteExec;
pub use distinct::DistinctExec;
pub use filter::FilterExec;
pub use insert::InsertExec;
pub use limit::{LimitExec, collect_constants};
pub use project::ProjectExec;
pub use result::ResultExec;
pub use seq_scan::SeqScanExec;
pub use sort::SortExec;
pub use update::UpdateExec;
pub use values::ValuesExec;

use super::{ExecCtx, Executor};
use crate::error::{Error, Result};
use crate::types::Row;

/// まだ実装されていないノード（`0A000 <ノード名> is not supported yet`）。ノード名は PostgreSQL の
/// EXPLAIN の表記。
#[derive(Debug)]
pub struct UnsupportedExec {
    node: &'static str,
}

impl UnsupportedExec {
    pub fn new(node: &'static str) -> Self {
        UnsupportedExec { node }
    }

    fn error(&self) -> Error {
        Error::not_supported(format!("{} is not supported yet", self.node))
    }
}

impl Executor for UnsupportedExec {
    fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        Err(self.error())
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        Err(self.error())
    }
}
