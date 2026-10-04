//! Table storage abstraction. M1 keeps rows in memory; M2 replaces the
//! implementation with heap files behind the same trait.

pub mod memory;

use crate::error::Result;
use crate::types::{Oid, Row};

/// Identifies a stored row. In M2 this becomes a TID (`PageId`, slot).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RowId(pub u64);

pub trait TableStore: Send + Sync {
    fn create_table(&self, table_oid: Oid) -> Result<()>;
    fn drop_table(&self, table_oid: Oid) -> Result<()>;
    fn insert(&self, table_oid: Oid, row: Row) -> Result<RowId>;
    /// Used by undo (and by DELETE from M2 on).
    fn delete_row(&self, table_oid: Oid, id: RowId) -> Result<()>;
    /// All live rows in insertion order. M1 returns a full copy; M2 turns
    /// this into a cursor.
    fn scan(&self, table_oid: Oid) -> Result<Vec<(RowId, Row)>>;

    /// Undo of `drop_table`: brings the dropped table back with its exact
    /// row ids, if the store still has its data. Returns `false` when it
    /// does not (the caller then re-creates the table and re-inserts rows).
    fn restore_dropped(&self, table_oid: Oid) -> Result<bool> {
        let _ = table_oid;
        Ok(false)
    }

    /// Forgets the data kept for `restore_dropped` (the DROP committed).
    fn release_dropped(&self, table_oid: Oid) {
        let _ = table_oid;
    }
}
