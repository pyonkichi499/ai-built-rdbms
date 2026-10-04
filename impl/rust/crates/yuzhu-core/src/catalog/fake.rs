//! Test double for [`CatalogReader`]: an in-memory table list. It replaces
//! M1's `MemoryCatalog` in unit tests of the analyzer, planner and executor
//! until the real `StatementCatalog` exists. Not part of the product.

use std::collections::HashMap;
use std::sync::Arc;

use super::{CatalogReader, RelKind, TableDef};
use crate::error::Result;
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::types::{Oid, oid};

#[derive(Debug)]
pub(crate) struct FakeCatalog {
    database: String,
    search_path: Vec<String>,
    tables: HashMap<Oid, Arc<TableDef>>,
    next_oid: Oid,
}

impl FakeCatalog {
    pub(crate) fn new(database: impl Into<String>) -> Self {
        FakeCatalog {
            database: database.into(),
            search_path: vec!["public".into()],
            tables: HashMap::new(),
            next_oid: oid::FIRST_NORMAL_OBJECT_ID,
        }
    }

    pub(crate) fn allocate_oid(&mut self) -> Oid {
        let o = self.next_oid;
        self.next_oid += 1;
        o
    }

    pub(crate) fn put_table(&mut self, def: Arc<TableDef>) {
        self.tables.insert(def.oid, def);
    }

    pub(crate) fn remove_table(&mut self, oid: Oid) -> Option<Arc<TableDef>> {
        self.tables.remove(&oid)
    }
}

/// A `TableDef` in `public` of database 5 with the file named after the OID.
pub(crate) fn table_def(
    oid: Oid,
    name: &str,
    columns: Vec<super::ColumnDef>,
    checks: Vec<super::CheckDef>,
) -> TableDef {
    TableDef {
        oid,
        namespace: 2200,
        schema: "public".into(),
        name: name.into(),
        kind: RelKind::Table,
        locator: RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: 5,
            rel_number: RelFileNumber(oid),
        },
        columns,
        checks,
    }
}

impl CatalogReader for FakeCatalog {
    fn table(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>> {
        let schema = schema.unwrap_or("public");
        Ok(self
            .tables
            .values()
            .find(|t| t.schema == schema && t.name == name)
            .cloned())
    }

    fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
        Ok(self.tables.get(&oid).cloned())
    }

    fn current_database(&self) -> &str {
        &self.database
    }

    fn search_path(&self) -> &[String] {
        &self.search_path
    }

    fn role_name(&self, _oid: Oid) -> Result<Option<String>> {
        Ok(None)
    }

    fn visible_namespaces(&self) -> Result<Vec<Oid>> {
        Ok(vec![11, 2200])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::types::SqlType;

    #[test]
    fn put_lookup_remove() {
        let mut c = FakeCatalog::new("postgres");
        let oid = c.allocate_oid();
        assert_eq!(oid, 16384);
        let col = ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
        };
        c.put_table(Arc::new(table_def(oid, "t", vec![col], vec![])));
        assert_eq!(c.table(None, "t").unwrap().unwrap().oid, oid);
        assert!(c.table(Some("other"), "t").unwrap().is_none());
        assert_eq!(
            c.table_by_oid(oid).unwrap().unwrap().column_index("a"),
            Some(0)
        );
        assert_eq!(c.type_by_name("text").unwrap().oid, 25);
        assert_eq!(c.current_database(), "postgres");
        assert!(c.remove_table(oid).is_some());
        assert!(c.table(None, "t").unwrap().is_none());
    }
}
