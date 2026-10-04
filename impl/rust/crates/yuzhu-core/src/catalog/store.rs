//! `CatalogStore`: reads and writes the system catalogs through the heap
//! API (`m2.md` §4.6, §6.8).
//!
//! 担当 F が実装する。

use std::sync::Arc;

use crate::catalog::{CheckDef, ColumnDef, TableDef};
use crate::datadir::OidAllocator;
use crate::error::{Error, Result};
use crate::storage::{TableStore, WriteCtx};
use crate::txn::Snapshot;
use crate::types::Oid;

fn pending() -> Error {
    Error::not_supported("catalog store is not implemented yet")
}

#[derive(Debug)]
pub struct CatalogStore {
    #[allow(dead_code)]
    db_oid: Oid,
    #[allow(dead_code)]
    storage: Arc<dyn TableStore>,
}

impl CatalogStore {
    pub fn new(db_oid: Oid, storage: Arc<dyn TableStore>) -> Self {
        CatalogStore { db_oid, storage }
    }

    pub fn lookup_relation(&self, _snap: &Snapshot, _nsp: Oid, _name: &str) -> Result<Option<Oid>> {
        Err(pending())
    }

    pub fn load_table_def(&self, _snap: &Snapshot, _oid: Oid) -> Result<Option<TableDef>> {
        Err(pending())
    }

    pub fn namespace_oid(&self, _snap: &Snapshot, _name: &str) -> Result<Option<Oid>> {
        Err(pending())
    }

    /// Takes an OID from the counter and retries while a row with that OID
    /// exists in `catalog_oid` under a "see every version" scan
    /// (`GetNewOidWithIndex`, `m2.md` §6.9.2).
    pub fn get_new_oid(&self, _oids: &OidAllocator, _catalog_oid: Oid) -> Result<Oid> {
        Err(pending())
    }

    /// `get_new_oid(pg_class)` plus a retry while the relation file exists.
    pub fn get_new_relation_oid(&self, _oids: &OidAllocator) -> Result<Oid> {
        Err(pending())
    }

    /// Inserts rows into `pg_class`, `pg_attribute`, `pg_attrdef` and
    /// `pg_constraint`. Does not create the file.
    pub fn create_table(&self, _w: &WriteCtx, _snap: &Snapshot, _spec: &NewTable) -> Result<()> {
        Err(pending())
    }

    /// Deletes the rows `create_table` inserted.
    pub fn drop_table(&self, _w: &WriteCtx, _snap: &Snapshot, _def: &TableDef) -> Result<()> {
        Err(pending())
    }
}

#[derive(Debug, Clone)]
pub struct NewTable {
    pub oid: Oid,
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
}

/// The only path to the shared catalogs (`pg_database`, `pg_authid`).
#[derive(Debug)]
pub struct SharedCatalogStore {
    #[allow(dead_code)]
    storage: Arc<dyn TableStore>,
}

impl SharedCatalogStore {
    pub fn new(storage: Arc<dyn TableStore>) -> Self {
        SharedCatalogStore { storage }
    }

    pub fn database_by_name(&self, _snap: &Snapshot, _name: &str) -> Result<Option<DatabaseRow>> {
        Err(pending())
    }

    pub fn role_by_name(&self, _snap: &Snapshot, _name: &str) -> Result<Option<RoleRow>> {
        Err(pending())
    }

    pub fn role_name(&self, _snap: &Snapshot, _oid: Oid) -> Result<Option<String>> {
        Err(pending())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseRow {
    pub oid: Oid,
    pub name: String,
    pub allow_conn: bool,
    pub is_template: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRow {
    pub oid: Oid,
    pub name: String,
    pub can_login: bool,
    pub superuser: bool,
}
