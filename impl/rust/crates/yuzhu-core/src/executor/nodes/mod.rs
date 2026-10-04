//! Executor nodes. (Owned by the executor implementer.)

pub mod delete;
pub mod distinct;
pub mod filter;
pub mod insert;
pub mod limit;
pub mod project;
pub mod result;
pub mod seq_scan;
pub mod sort;
pub mod update;
pub mod values;

pub use delete::DeleteExec;
pub use distinct::DistinctExec;
pub use filter::FilterExec;
pub use insert::InsertExec;
pub use limit::LimitExec;
pub use project::ProjectExec;
pub use result::ResultExec;
pub use seq_scan::SeqScanExec;
pub use sort::SortExec;
pub use update::UpdateExec;
pub use values::ValuesExec;

#[cfg(test)]
pub(crate) mod test_util {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::catalog::fake::FakeCatalog;
    use crate::error::{Error, Result};
    use crate::executor::eval::tests::session;
    use crate::executor::{BoxedExecutor, ExecCtx};
    use crate::interrupt::InterruptFlag;
    use crate::storage::smgr::RelFileLocator;
    use crate::storage::{
        HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
    };
    use crate::txn::{Snapshot, Transaction, Xid};
    use crate::types::{Datum, Oid, Row, Tid};

    /// A `TableStore` that keeps rows in memory (no MVCC, no UPDATE /
    /// DELETE). Stands in for `HeapStore` in executor and planner unit tests.
    #[derive(Debug, Default)]
    pub(crate) struct FakeStore {
        tables: Mutex<HashMap<Oid, Vec<Row>>>,
    }

    impl FakeStore {
        /// Appends a row without going through `TableStore::insert`.
        pub(crate) fn add_row(&self, oid: Oid, row: Row) {
            self.tables
                .lock()
                .unwrap()
                .entry(oid)
                .or_default()
                .push(row);
        }

        pub(crate) fn rows(&self, oid: Oid) -> Vec<Row> {
            self.tables
                .lock()
                .unwrap()
                .get(&oid)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn tid_of(n: usize) -> Tid {
        Tid {
            block: 0,
            offset: u16::try_from(n + 1).unwrap(),
        }
    }

    fn unsupported() -> Error {
        Error::not_supported("not supported by FakeStore")
    }

    impl TableStore for FakeStore {
        fn create_storage(&self, _rel: RelFileLocator) -> Result<()> {
            Ok(())
        }
        fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
            Ok(false)
        }
        fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
            Ok(())
        }
        fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
            assert_ne!(w.xid, Xid::INVALID);
            let mut t = self.tables.lock().unwrap();
            let rows = t.entry(rel.oid).or_default();
            rows.push(row.to_vec());
            Ok(tid_of(rows.len() - 1))
        }
        fn delete(&self, _: &RelHandle, _: &WriteCtx, _: &Snapshot, _: Tid) -> Result<TmResult> {
            Err(unsupported())
        }
        fn update(
            &self,
            _: &RelHandle,
            _: &WriteCtx,
            _: &Snapshot,
            _: Tid,
            _: &[Datum],
        ) -> Result<UpdateOutcome> {
            Err(unsupported())
        }
        fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
            let tuples = self
                .rows(rel.oid)
                .into_iter()
                .enumerate()
                .map(|(i, row)| HeapTuple {
                    tid: tid_of(i),
                    xmin: Xid::BOOTSTRAP,
                    xmax: Xid::INVALID,
                    cmin: 0,
                    cmax: 0,
                    row,
                })
                .collect();
            Ok(HeapScan::from_tuples(rel.clone(), snap.clone(), tuples))
        }
        fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
            Ok(scan.pop_buffered())
        }
        fn fetch(&self, _: &RelHandle, _: &Snapshot, _: Tid) -> Result<Option<HeapTuple>> {
            Ok(None)
        }
    }

    /// Test fixture owning everything an `ExecCtx` borrows.
    pub(crate) struct Fixture {
        pub(crate) catalog: FakeCatalog,
        pub(crate) storage: FakeStore,
        pub(crate) txn: Transaction,
        pub(crate) snapshot: Snapshot,
        pub(crate) session: crate::executor::SessionInfo,
        pub(crate) interrupts: InterruptFlag,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            let mut txn = Transaction::new();
            txn.xid = Some(Xid(3));
            Fixture {
                catalog: FakeCatalog::new("postgres"),
                storage: FakeStore::default(),
                txn,
                snapshot: Snapshot {
                    xmin: Xid(3),
                    xmax: Xid(4),
                    xip: vec![],
                    curcid: 0,
                    own_xid: Some(Xid(3)),
                },
                session: session(),
                interrupts: InterruptFlag::default(),
            }
        }

        /// Runs an executor to completion, returning all rows.
        pub(crate) fn run(&mut self, exec: &mut BoxedExecutor) -> Result<Vec<Row>> {
            let mut ctx = ExecCtx {
                catalog: &self.catalog,
                storage: &self.storage,
                txn: &mut self.txn,
                snapshot: &self.snapshot,
                session: &self.session,
                interrupts: &self.interrupts,
            };
            let mut out = Vec::new();
            while let Some(r) = exec.next(&mut ctx)? {
                out.push(r);
            }
            Ok(out)
        }
    }
}
