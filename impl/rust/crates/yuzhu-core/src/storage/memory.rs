//! In-memory `TableStore` for M1: one insertion-ordered `Vec<Option<Row>>`
//! per table (deleted slots become `None`, so `RowId` = slot index stays
//! stable). A dropped table's slots are kept aside until the DROP commits
//! (`release_dropped`) so that undo can restore the same row ids.

use std::collections::HashMap;
use std::sync::Mutex;

use super::{RowId, TableStore};
use crate::error::{Error, Result};
use crate::types::{Oid, Row};

type Slots = Vec<Option<Row>>;

#[derive(Debug, Default)]
struct Inner {
    tables: HashMap<Oid, Slots>,
    dropped: HashMap<Oid, Slots>,
}

#[derive(Debug, Default)]
pub struct MemoryTableStore {
    inner: Mutex<Inner>,
}

impl MemoryTableStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another thread panicked mid-statement;
        // the map itself is still structurally valid.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Number of live rows (for tests and diagnostics).
    pub fn live_rows(&self, table_oid: Oid) -> Result<usize> {
        let g = self.lock();
        let rows = g.tables.get(&table_oid).ok_or_else(|| missing(table_oid))?;
        Ok(rows.iter().filter(|r| r.is_some()).count())
    }
}

fn missing(table_oid: Oid) -> Error {
    Error::internal(format!("storage for relation {table_oid} does not exist"))
}

impl TableStore for MemoryTableStore {
    fn create_table(&self, table_oid: Oid) -> Result<()> {
        let mut g = self.lock();
        if g.tables.contains_key(&table_oid) {
            return Err(Error::internal(format!(
                "storage for relation {table_oid} already exists"
            )));
        }
        g.dropped.remove(&table_oid);
        g.tables.insert(table_oid, Vec::new());
        Ok(())
    }

    fn drop_table(&self, table_oid: Oid) -> Result<()> {
        let mut g = self.lock();
        let slots = g
            .tables
            .remove(&table_oid)
            .ok_or_else(|| missing(table_oid))?;
        g.dropped.insert(table_oid, slots);
        Ok(())
    }

    fn insert(&self, table_oid: Oid, row: Row) -> Result<RowId> {
        let mut g = self.lock();
        let rows = g
            .tables
            .get_mut(&table_oid)
            .ok_or_else(|| missing(table_oid))?;
        rows.push(Some(row));
        Ok(RowId(rows.len() as u64 - 1))
    }

    fn delete_row(&self, table_oid: Oid, id: RowId) -> Result<()> {
        let mut g = self.lock();
        let rows = g
            .tables
            .get_mut(&table_oid)
            .ok_or_else(|| missing(table_oid))?;
        let slot = usize::try_from(id.0)
            .ok()
            .and_then(|i| rows.get_mut(i))
            .ok_or_else(|| Error::internal(format!("invalid row id {}", id.0)))?;
        *slot = None;
        Ok(())
    }

    fn scan(&self, table_oid: Oid) -> Result<Vec<(RowId, Row)>> {
        let g = self.lock();
        let rows = g.tables.get(&table_oid).ok_or_else(|| missing(table_oid))?;
        Ok(rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.as_ref().map(|r| (RowId(i as u64), r.clone())))
            .collect())
    }

    fn restore_dropped(&self, table_oid: Oid) -> Result<bool> {
        let mut g = self.lock();
        if g.tables.contains_key(&table_oid) {
            return Err(Error::internal(format!(
                "storage for relation {table_oid} already exists"
            )));
        }
        match g.dropped.remove(&table_oid) {
            Some(slots) => {
                g.tables.insert(table_oid, slots);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn release_dropped(&self, table_oid: Oid) {
        self.lock().dropped.remove(&table_oid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Datum;

    #[test]
    fn insert_scan_delete() {
        let s = MemoryTableStore::new();
        s.create_table(1).unwrap();
        assert!(s.create_table(1).is_err());
        let a = s.insert(1, vec![Datum::Int4(1)]).unwrap();
        let b = s.insert(1, vec![Datum::Int4(2)]).unwrap();
        assert_eq!((a, b), (RowId(0), RowId(1)));
        s.delete_row(1, a).unwrap();
        assert!(s.delete_row(1, RowId(99)).is_err());
        assert_eq!(s.scan(1).unwrap(), vec![(RowId(1), vec![Datum::Int4(2)])]);
        assert_eq!(s.live_rows(1).unwrap(), 1);
        s.drop_table(1).unwrap();
        assert!(s.scan(1).is_err());
        assert!(s.drop_table(1).is_err());
        assert!(s.insert(1, vec![]).is_err());
    }

    #[test]
    fn restore_and_release_dropped() {
        let s = MemoryTableStore::new();
        s.create_table(1).unwrap();
        let a = s.insert(1, vec![Datum::Int4(1)]).unwrap();
        let b = s.insert(1, vec![Datum::Int4(2)]).unwrap();
        s.delete_row(1, a).unwrap();
        s.drop_table(1).unwrap();
        assert!(s.restore_dropped(1).unwrap());
        // Row ids survive the round trip.
        assert_eq!(s.scan(1).unwrap(), vec![(b, vec![Datum::Int4(2)])]);
        assert!(s.restore_dropped(1).is_err());
        s.drop_table(1).unwrap();
        s.release_dropped(1);
        assert!(!s.restore_dropped(1).unwrap());
    }
}
