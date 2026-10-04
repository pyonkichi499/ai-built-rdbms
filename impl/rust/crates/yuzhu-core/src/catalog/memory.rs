//! In-memory catalog for M1.
//!
//! A minimal, unsynchronized implementation; the owner (`engine::Database`)
//! wraps it in a lock. The session implementer may extend it (e.g. with
//! `pg_catalog` stub tables).

use std::collections::HashMap;
use std::sync::Arc;

use super::{CatalogReader, TableDef};
use crate::types::{Oid, oid};

#[derive(Debug)]
pub struct MemoryCatalog {
    database: String,
    tables: HashMap<Oid, Arc<TableDef>>,
    next_oid: Oid,
}

impl MemoryCatalog {
    pub fn new(database: impl Into<String>) -> Self {
        MemoryCatalog {
            database: database.into(),
            tables: HashMap::new(),
            next_oid: oid::FIRST_NORMAL_OBJECT_ID,
        }
    }

    /// Allocates a new object OID (16384 and up).
    pub fn allocate_oid(&mut self) -> Oid {
        let o = self.next_oid;
        self.next_oid += 1;
        o
    }

    /// Inserts or replaces a table definition.
    pub fn put_table(&mut self, def: Arc<TableDef>) {
        self.tables.insert(def.oid, def);
    }

    pub fn remove_table(&mut self, oid: Oid) -> Option<Arc<TableDef>> {
        self.tables.remove(&oid)
    }

    /// All tables, in OID order.
    pub fn tables(&self) -> Vec<Arc<TableDef>> {
        let mut v: Vec<_> = self.tables.values().cloned().collect();
        v.sort_by_key(|t| t.oid);
        v
    }
}

impl CatalogReader for MemoryCatalog {
    fn table(&self, schema: Option<&str>, name: &str) -> Option<Arc<TableDef>> {
        let schema = schema.unwrap_or("public");
        self.tables
            .values()
            .find(|t| t.schema == schema && t.name == name)
            .cloned()
    }

    fn table_by_oid(&self, oid: Oid) -> Option<Arc<TableDef>> {
        self.tables.get(&oid).cloned()
    }

    fn current_database(&self) -> &str {
        &self.database
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::types::SqlType;

    #[test]
    fn put_lookup_remove() {
        let mut c = MemoryCatalog::new("postgres");
        let oid = c.allocate_oid();
        assert_eq!(oid, 16384);
        c.put_table(Arc::new(TableDef {
            oid,
            schema: "public".into(),
            name: "t".into(),
            columns: vec![ColumnDef {
                name: "a".into(),
                attnum: 1,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
            }],
            checks: vec![],
        }));
        assert_eq!(c.table(None, "t").unwrap().oid, oid);
        assert!(c.table(Some("other"), "t").is_none());
        assert_eq!(c.table_by_oid(oid).unwrap().column_index("a"), Some(0));
        assert_eq!(c.type_by_name("text").unwrap().oid, 25);
        assert_eq!(c.current_database(), "postgres");
        assert!(c.remove_table(oid).is_some());
        assert!(c.table(None, "t").is_none());
    }
}
