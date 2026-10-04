//! Executor nodes. (Owned by the executor implementer.)

pub mod distinct;
pub mod filter;
pub mod insert;
pub mod limit;
pub mod project;
pub mod result;
pub mod seq_scan;
pub mod sort;
pub mod values;

pub use distinct::DistinctExec;
pub use filter::FilterExec;
pub use insert::InsertExec;
pub use limit::LimitExec;
pub use project::ProjectExec;
pub use result::ResultExec;
pub use seq_scan::SeqScanExec;
pub use sort::SortExec;
pub use values::ValuesExec;

#[cfg(test)]
pub(crate) mod test_util {
    use crate::catalog::memory::MemoryCatalog;
    use crate::error::Result;
    use crate::executor::eval::tests::session;
    use crate::executor::{BoxedExecutor, ExecCtx};
    use crate::storage::memory::MemoryTableStore;
    use crate::txn::Transaction;
    use crate::types::Row;

    /// Test fixture owning everything an `ExecCtx` borrows.
    pub(crate) struct Fixture {
        pub(crate) catalog: MemoryCatalog,
        pub(crate) storage: MemoryTableStore,
        pub(crate) txn: Transaction,
        pub(crate) session: crate::executor::SessionInfo,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Fixture {
                catalog: MemoryCatalog::new("postgres"),
                storage: MemoryTableStore::new(),
                txn: Transaction::new(),
                session: session(),
            }
        }

        /// Runs an executor to completion, returning all rows.
        pub(crate) fn run(&mut self, exec: &mut BoxedExecutor) -> Result<Vec<Row>> {
            let mut ctx = ExecCtx {
                catalog: &self.catalog,
                storage: &self.storage,
                txn: &mut self.txn,
                session: &self.session,
            };
            let mut out = Vec::new();
            while let Some(r) = exec.next(&mut ctx)? {
                out.push(r);
            }
            Ok(out)
        }
    }
}
