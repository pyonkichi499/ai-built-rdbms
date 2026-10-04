//! M1 transactions: an undo log (replaced by MVCC in M3).
//!
//! `Transaction` records what to undo. Applying the entries needs mutable
//! access to the catalog and storage, which the session / engine owns; it
//! can use [`apply_undo`] (newest entry first) and [`commit`].

use std::sync::Arc;

use crate::catalog::TableDef;
use crate::catalog::memory::MemoryCatalog;
use crate::error::Result;
use crate::storage::{RowId, TableStore};
use crate::types::{Oid, Row};

/// One reversible change.
#[derive(Clone, Debug)]
pub enum UndoEntry {
    /// Undo: `storage.delete_row(table_oid, row_id)`.
    Inserted { table_oid: Oid, row_id: RowId },
    /// Undo: drop the table from catalog and storage.
    CreatedTable { table_oid: Oid },
    /// Undo: re-create the table with `def` and re-insert `rows`.
    DroppedTable { def: Arc<TableDef>, rows: Vec<Row> },
}

#[derive(Debug, Default)]
pub struct Transaction {
    pub undo: Vec<UndoEntry>,
    /// Index into `undo` where the current statement started.
    pub statement_start: usize,
}

impl Transaction {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the start of a statement (its changes can be undone alone).
    pub fn begin_statement(&mut self) {
        self.statement_start = self.undo.len();
    }

    pub fn record(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
    }

    /// Removes and returns the current statement's entries, **newest first**
    /// (the order in which they must be undone).
    pub fn take_statement_undo(&mut self) -> Vec<UndoEntry> {
        let start = self.statement_start.min(self.undo.len());
        let mut v = self.undo.split_off(start);
        v.reverse();
        v
    }

    /// Removes and returns all entries, newest first (ROLLBACK).
    pub fn take_all_undo(&mut self) -> Vec<UndoEntry> {
        self.statement_start = 0;
        let mut v = std::mem::take(&mut self.undo);
        v.reverse();
        v
    }

    /// Forgets the undo log (COMMIT).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.statement_start = 0;
    }
}

/// Applies undo entries in the given order (as returned by
/// `take_statement_undo` / `take_all_undo`, i.e. newest first).
///
/// Every entry is attempted even if an earlier one fails; the first error
/// is returned.
pub fn apply_undo(
    entries: Vec<UndoEntry>,
    catalog: &mut MemoryCatalog,
    storage: &dyn TableStore,
) -> Result<()> {
    let mut first_err = None;
    for entry in entries {
        if let Err(e) = apply_one(entry, catalog, storage) {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

fn apply_one(
    entry: UndoEntry,
    catalog: &mut MemoryCatalog,
    storage: &dyn TableStore,
) -> Result<()> {
    match entry {
        UndoEntry::Inserted { table_oid, row_id } => storage.delete_row(table_oid, row_id),
        UndoEntry::CreatedTable { table_oid } => {
            catalog.remove_table(table_oid);
            storage.drop_table(table_oid)?;
            // The table never existed outside this transaction.
            storage.release_dropped(table_oid);
            Ok(())
        }
        UndoEntry::DroppedTable { def, rows } => {
            let oid = def.oid;
            catalog.put_table(def);
            if !storage.restore_dropped(oid)? {
                storage.create_table(oid)?;
                for row in rows {
                    storage.insert(oid, row)?;
                }
            }
            Ok(())
        }
    }
}

/// Commits: forgets the undo log and releases data kept for undoing DROPs.
pub fn commit(txn: &mut Transaction, storage: &dyn TableStore) {
    for entry in &txn.undo {
        if let UndoEntry::DroppedTable { def, .. } = entry {
            storage.release_dropped(def.oid);
        }
    }
    txn.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogReader;
    use crate::storage::memory::MemoryTableStore;
    use crate::types::Datum;

    fn table(oid: Oid) -> Arc<TableDef> {
        Arc::new(TableDef {
            oid,
            schema: "public".into(),
            name: format!("t{oid}"),
            columns: vec![],
            checks: vec![],
        })
    }

    #[test]
    fn rollback_create_insert_drop() {
        let mut cat = MemoryCatalog::new("postgres");
        let st = MemoryTableStore::new();
        // Pre-existing table with a dead slot from an earlier rollback.
        cat.put_table(table(1));
        st.create_table(1).unwrap();
        let dead = st.insert(1, vec![Datum::Int4(0)]).unwrap();
        st.delete_row(1, dead).unwrap();
        let keep = st.insert(1, vec![Datum::Int4(1)]).unwrap();

        let mut t = Transaction::new();
        let id = st.insert(1, vec![Datum::Int4(2)]).unwrap();
        t.record(UndoEntry::Inserted {
            table_oid: 1,
            row_id: id,
        });
        cat.put_table(table(2));
        st.create_table(2).unwrap();
        t.record(UndoEntry::CreatedTable { table_oid: 2 });
        let rows = st.scan(1).unwrap().into_iter().map(|(_, r)| r).collect();
        let def = cat.remove_table(1).unwrap();
        st.drop_table(1).unwrap();
        t.record(UndoEntry::DroppedTable { def, rows });

        apply_undo(t.take_all_undo(), &mut cat, &st).unwrap();
        assert!(cat.table_by_oid(2).is_none());
        assert!(st.scan(2).is_err());
        assert!(cat.table_by_oid(1).is_some());
        assert_eq!(st.scan(1).unwrap(), vec![(keep, vec![Datum::Int4(1)])]);
    }

    #[test]
    fn commit_releases_dropped_data() {
        let mut cat = MemoryCatalog::new("postgres");
        let st = MemoryTableStore::new();
        cat.put_table(table(1));
        st.create_table(1).unwrap();
        st.insert(1, vec![Datum::Int4(1)]).unwrap();
        let mut t = Transaction::new();
        let def = cat.remove_table(1).unwrap();
        st.drop_table(1).unwrap();
        t.record(UndoEntry::DroppedTable {
            def,
            rows: vec![vec![Datum::Int4(1)]],
        });
        commit(&mut t, &st);
        assert!(t.undo.is_empty());
        assert!(!st.restore_dropped(1).unwrap());
    }

    #[test]
    fn dropped_table_fallback_reinserts_rows() {
        // A store that cannot restore (default trait method) gets the rows
        // re-inserted.
        let mut cat = MemoryCatalog::new("postgres");
        let st = MemoryTableStore::new();
        st.create_table(1).unwrap();
        st.drop_table(1).unwrap();
        st.release_dropped(1);
        apply_undo(
            vec![UndoEntry::DroppedTable {
                def: table(1),
                rows: vec![vec![Datum::Int4(7)]],
            }],
            &mut cat,
            &st,
        )
        .unwrap();
        assert_eq!(st.scan(1).unwrap(), vec![(RowId(0), vec![Datum::Int4(7)])]);
    }

    fn ins(n: u64) -> UndoEntry {
        UndoEntry::Inserted {
            table_oid: 1,
            row_id: RowId(n),
        }
    }

    #[test]
    fn statement_undo_is_newest_first() {
        let mut t = Transaction::new();
        t.record(ins(0));
        t.begin_statement();
        t.record(ins(1));
        t.record(ins(2));
        let v = t.take_statement_undo();
        assert!(matches!(
            v[0],
            UndoEntry::Inserted {
                row_id: RowId(2),
                ..
            }
        ));
        assert_eq!(v.len(), 2);
        assert_eq!(t.undo.len(), 1);
        assert_eq!(t.take_all_undo().len(), 1);
        assert!(t.undo.is_empty());
    }
}
