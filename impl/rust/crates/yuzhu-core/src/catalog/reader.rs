//! `StatementCatalog`: the per-statement `CatalogReader` (`m2.md` §4.6).
//!
//! 担当 F が実装する。

use std::sync::Arc;

use super::{CatalogReader, TableDef};
use crate::engine::DatabaseHandle;
use crate::error::{Error, Result};
use crate::txn::Snapshot;
use crate::types::Oid;

pub struct StatementCatalog<'a> {
    pub db: &'a DatabaseHandle,
    pub snapshot: &'a Snapshot,
    pub gen_at_snapshot: u64,
    /// The transaction changed the catalog: skip the shared cache.
    pub bypass_cache: bool,
    pub search_path: &'a [String],
}

impl std::fmt::Debug for StatementCatalog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatementCatalog")
            .field("db", &self.db.name)
            .field("gen_at_snapshot", &self.gen_at_snapshot)
            .field("bypass_cache", &self.bypass_cache)
            .field("search_path", &self.search_path)
            .finish_non_exhaustive()
    }
}

fn pending() -> Error {
    Error::not_supported("statement catalog is not implemented yet")
}

impl CatalogReader for StatementCatalog<'_> {
    fn table(&self, _schema: Option<&str>, _name: &str) -> Result<Option<Arc<TableDef>>> {
        Err(pending())
    }

    fn table_by_oid(&self, _oid: Oid) -> Result<Option<Arc<TableDef>>> {
        Err(pending())
    }

    fn current_database(&self) -> &str {
        &self.db.name
    }

    fn search_path(&self) -> &[String] {
        self.search_path
    }

    fn role_name(&self, _oid: Oid) -> Result<Option<String>> {
        Err(pending())
    }

    fn visible_namespaces(&self) -> Result<Vec<Oid>> {
        Err(pending())
    }
}
