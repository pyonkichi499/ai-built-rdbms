//! Static column definitions of the 13 system catalogs and their OID
//! constants (genbki's `Schema_pg_*`) (`m2.md` §4.6, §6.8.1).
//!
//! 担当 F が実装する。

use std::sync::Arc;

use super::TableDef;
use crate::types::Oid;

#[derive(Debug)]
pub struct CatalogColumn {
    pub name: &'static str,
    pub type_oid: Oid,
    pub not_null: bool,
}

#[derive(Debug)]
pub struct CatalogDef {
    pub oid: Oid,
    pub name: &'static str,
    /// Lives in `global/`.
    pub shared: bool,
    /// `pg_class.relfilenode = 0`.
    pub mapped: bool,
    /// `pg_class.reltype` (0 is possible).
    pub rowtype_oid: Oid,
    pub columns: &'static [CatalogColumn],
}

/// The 13 catalogs (`m2.md` §6.8.1). 担当 F が埋める。
pub static CATALOGS: &[CatalogDef] = &[];

/// `(name, attnum, type OID)` of the system columns.
pub const SYSTEM_COLUMNS: [(&str, i16, Oid); 6] = [
    ("ctid", -1, crate::types::oid::TID),
    ("xmin", -2, crate::types::oid::XID),
    ("cmin", -3, crate::types::oid::CID),
    ("xmax", -4, crate::types::oid::XID),
    ("cmax", -5, crate::types::oid::CID),
    ("tableoid", -6, crate::types::oid::OID),
];

/// The pinned `TableDef` of a system catalog. Shared catalogs ignore
/// `db_oid` and use `locator = (GLOBALTABLESPACE_OID, 0, relfilenode)`.
pub fn catalog_table_def(oid: Oid, db_oid: Oid) -> Option<Arc<TableDef>> {
    let _ = (oid, db_oid);
    None
}
