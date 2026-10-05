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
pub use limit::{LimitExec, collect_constants};
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
    use crate::error::Result;
    use crate::executor::eval::tests::session;
    use crate::executor::{BoxedExecutor, ExecCtx};
    use crate::interrupt::InterruptFlag;
    use crate::storage::smgr::RelFileLocator;
    use crate::storage::{
        HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
    };
    use crate::txn::{Snapshot, Transaction, Xid};
    use crate::types::{Datum, Oid, Row, Tid};

    /// A `TableStore` that keeps rows in memory (no MVCC). Deleted rows
    /// leave a hole so TIDs stay stable. `force_result` makes the next
    /// `delete` / `update` return a given `TmResult` without changing
    /// anything. Stands in for `HeapStore` in executor and planner tests.
    #[derive(Debug, Default)]
    pub(crate) struct FakeStore {
        tables: Mutex<HashMap<Oid, Vec<Option<Row>>>>,
        forced: Mutex<std::collections::VecDeque<TmResult>>,
        /// `(xid, cid)` of every write, in order.
        writes: Mutex<Vec<(Xid, u32)>>,
    }

    impl FakeStore {
        /// Appends a row without going through `TableStore::insert`.
        pub(crate) fn add_row(&self, oid: Oid, row: Row) {
            self.tables
                .lock()
                .unwrap()
                .entry(oid)
                .or_default()
                .push(Some(row));
        }

        /// Live rows in TID order.
        pub(crate) fn rows(&self, oid: Oid) -> Vec<Row> {
            self.tables
                .lock()
                .unwrap()
                .get(&oid)
                .map(|v| v.iter().flatten().cloned().collect())
                .unwrap_or_default()
        }

        /// The next `delete` / `update` returns `r` and changes nothing.
        pub(crate) fn force_result(&self, r: TmResult) {
            self.forced.lock().unwrap().push_back(r);
        }

        pub(crate) fn writes(&self) -> Vec<(Xid, u32)> {
            self.writes.lock().unwrap().clone()
        }

        fn slot(tid: Tid) -> usize {
            usize::from(tid.offset).saturating_sub(1)
        }
    }

    fn tid_of(n: usize) -> Tid {
        Tid {
            block: 0,
            offset: u16::try_from(n + 1).unwrap(),
        }
    }

    impl TableStore for FakeStore {
        fn create_storage(&self, _w: &WriteCtx, _rel: RelFileLocator) -> Result<()> {
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
            self.writes.lock().unwrap().push((w.xid, w.cid));
            let mut t = self.tables.lock().unwrap();
            let rows = t.entry(rel.oid).or_default();
            rows.push(Some(row.to_vec()));
            Ok(tid_of(rows.len() - 1))
        }
        fn delete(
            &self,
            rel: &RelHandle,
            w: &WriteCtx,
            _: &Snapshot,
            tid: Tid,
        ) -> Result<TmResult> {
            self.writes.lock().unwrap().push((w.xid, w.cid));
            if let Some(r) = self.forced.lock().unwrap().pop_front() {
                return Ok(r);
            }
            let mut t = self.tables.lock().unwrap();
            match t.get_mut(&rel.oid).and_then(|v| v.get_mut(Self::slot(tid))) {
                Some(s) if s.is_some() => {
                    *s = None;
                    Ok(TmResult::Ok)
                }
                _ => Ok(TmResult::Invisible),
            }
        }
        fn update(
            &self,
            rel: &RelHandle,
            w: &WriteCtx,
            _: &Snapshot,
            tid: Tid,
            new_row: &[Datum],
        ) -> Result<UpdateOutcome> {
            self.writes.lock().unwrap().push((w.xid, w.cid));
            if let Some(r) = self.forced.lock().unwrap().pop_front() {
                return Ok(UpdateOutcome {
                    result: r,
                    new_tid: None,
                });
            }
            let mut t = self.tables.lock().unwrap();
            let rows = t.entry(rel.oid).or_default();
            match rows.get_mut(Self::slot(tid)) {
                Some(s) if s.is_some() => {
                    *s = None;
                    rows.push(Some(new_row.to_vec()));
                    Ok(UpdateOutcome {
                        result: TmResult::Ok,
                        new_tid: Some(tid_of(rows.len() - 1)),
                    })
                }
                _ => Ok(UpdateOutcome {
                    result: TmResult::Invisible,
                    new_tid: None,
                }),
            }
        }
        fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
            // The live rows at this moment: rows added later (the new
            // versions written by an UPDATE) are not seen, like the
            // command-ID rule of the real heap.
            let tuples = self
                .tables
                .lock()
                .unwrap()
                .get(&rel.oid)
                .map(|v| {
                    v.iter()
                        .enumerate()
                        .filter_map(|(i, r)| {
                            r.as_ref().map(|row| HeapTuple {
                                tid: tid_of(i),
                                xmin: Xid::BOOTSTRAP,
                                xmax: Xid::INVALID,
                                cmin: 0,
                                cmax: 0,
                                row: row.clone(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
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
                runtime: &crate::executor::NullRuntime,
            };
            let mut out = Vec::new();
            while let Some(r) = exec.next(&mut ctx)? {
                out.push(r);
            }
            Ok(out)
        }
    }
}
