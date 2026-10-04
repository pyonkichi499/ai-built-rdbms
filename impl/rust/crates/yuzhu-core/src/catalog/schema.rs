//! Static column definitions of the 13 system catalogs and their OID
//! constants (genbki's `Schema_pg_*`) (`m2.md` §4.6, §6.8.1).
//!
//! Names, types and order are those of the PostgreSQL 17 headers
//! (`pg_*.h`); the column lists were taken from a PostgreSQL 17 catalog and
//! are checked against `tests/slt/m2/catalog/catalog_columns.slt`.

use std::sync::Arc;

use super::{CheckDef, ColumnDef, RelKind, TableDef};
use crate::storage::smgr::{
    DEFAULTTABLESPACE_OID, GLOBALTABLESPACE_OID, RelFileLocator, RelFileNumber,
};
use crate::types::{Oid, SqlType, oid};

/// OIDs of the catalogs and of the objects that the initial rows refer to.
pub mod oids {
    use crate::types::Oid;

    pub const PG_CLASS: Oid = 1259;
    pub const PG_ATTRIBUTE: Oid = 1249;
    pub const PG_TYPE: Oid = 1247;
    pub const PG_PROC: Oid = 1255;
    pub const PG_NAMESPACE: Oid = 2615;
    pub const PG_OPERATOR: Oid = 2617;
    pub const PG_CAST: Oid = 2605;
    pub const PG_AM: Oid = 2601;
    pub const PG_ATTRDEF: Oid = 2604;
    pub const PG_CONSTRAINT: Oid = 2606;
    pub const PG_DATABASE: Oid = 1262;
    pub const PG_AUTHID: Oid = 1260;
    pub const PG_TABLESPACE: Oid = 1213;

    /// `pg_catalog`.
    pub const NAMESPACE_PG_CATALOG: Oid = 11;
    /// `pg_toast`.
    pub const NAMESPACE_PG_TOAST: Oid = 99;
    /// `public`.
    pub const NAMESPACE_PUBLIC: Oid = 2200;
    /// The bootstrap superuser (`initdb -U`).
    pub const BOOTSTRAP_SUPERUSER: Oid = 10;
    /// `pg_database_owner`, the owner of `public`.
    pub const DATABASE_OWNER: Oid = 6171;
    /// Access methods.
    pub const HEAP_TABLE_AM: Oid = 2;
    pub const BTREE_AM: Oid = 403;
    /// `heap_tableam_handler` / `bthandler`.
    pub const HEAP_TABLEAM_HANDLER: Oid = 3;
    pub const BTHANDLER: Oid = 330;
    /// The language OID of `internal` functions. `pg_language` does not
    /// exist in M2, so this is the one reference that does not resolve.
    pub const INTERNAL_LANGUAGE: Oid = 12;
}

#[derive(Debug)]
pub struct CatalogColumn {
    pub name: &'static str,
    pub type_oid: Oid,
    pub not_null: bool,
}

const fn col(name: &'static str, type_oid: Oid, not_null: bool) -> CatalogColumn {
    CatalogColumn {
        name,
        type_oid,
        not_null,
    }
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

impl CatalogDef {
    pub fn natts(&self) -> usize {
        self.columns.len()
    }

    /// 0-based index of a column.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// `pg_class.reltablespace`: 1664 for shared catalogs, 0 otherwise.
    pub fn reltablespace(&self) -> Oid {
        if self.shared { GLOBALTABLESPACE_OID } else { 0 }
    }

    /// `pg_class.relfilenode`: 0 for mapped catalogs.
    pub fn relfilenode(&self) -> Oid {
        if self.mapped { 0 } else { self.oid }
    }

    /// Where the main fork lives. Shared catalogs ignore `db_oid`.
    pub fn locator(&self, db_oid: Oid) -> RelFileLocator {
        if self.shared {
            RelFileLocator {
                spc_oid: GLOBALTABLESPACE_OID,
                db_oid: 0,
                rel_number: RelFileNumber(relmap_lookup(self.oid)),
            }
        } else {
            RelFileLocator {
                spc_oid: DEFAULTTABLESPACE_OID,
                db_oid,
                rel_number: RelFileNumber(relmap_lookup(self.oid)),
            }
        }
    }
}

/// The file number of a catalog. M2 has no `pg_filenode.map`: a mapped
/// catalog's file number is its OID (`m2.md` §6.8.1). M5 replaces this with
/// a lookup in the map file.
pub fn relmap_lookup(catalog_oid: Oid) -> Oid {
    catalog_oid
}

#[rustfmt::skip]
static PG_CLASS_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("relname", 19, true),
    col("relnamespace", 26, true),
    col("reltype", 26, true),
    col("reloftype", 26, true),
    col("relowner", 26, true),
    col("relam", 26, true),
    col("relfilenode", 26, true),
    col("reltablespace", 26, true),
    col("relpages", 23, true),
    col("reltuples", 700, true),
    col("relallvisible", 23, true),
    col("reltoastrelid", 26, true),
    col("relhasindex", 16, true),
    col("relisshared", 16, true),
    col("relpersistence", 18, true),
    col("relkind", 18, true),
    col("relnatts", 21, true),
    col("relchecks", 21, true),
    col("relhasrules", 16, true),
    col("relhastriggers", 16, true),
    col("relhassubclass", 16, true),
    col("relrowsecurity", 16, true),
    col("relforcerowsecurity", 16, true),
    col("relispopulated", 16, true),
    col("relreplident", 18, true),
    col("relispartition", 16, true),
    col("relrewrite", 26, true),
    col("relfrozenxid", 28, true),
    col("relminmxid", 28, true),
    col("relacl", 1034, false),
    col("reloptions", 1009, false),
    col("relpartbound", 194, false),
];

#[rustfmt::skip]
static PG_ATTRIBUTE_COLUMNS: &[CatalogColumn] = &[
    col("attrelid", 26, true),
    col("attname", 19, true),
    col("atttypid", 26, true),
    col("attlen", 21, true),
    col("attnum", 21, true),
    col("attcacheoff", 23, true),
    col("atttypmod", 23, true),
    col("attndims", 21, true),
    col("attbyval", 16, true),
    col("attalign", 18, true),
    col("attstorage", 18, true),
    col("attcompression", 18, true),
    col("attnotnull", 16, true),
    col("atthasdef", 16, true),
    col("atthasmissing", 16, true),
    col("attidentity", 18, true),
    col("attgenerated", 18, true),
    col("attisdropped", 16, true),
    col("attislocal", 16, true),
    col("attinhcount", 21, true),
    col("attcollation", 26, true),
    col("attstattarget", 21, false),
    col("attacl", 1034, false),
    col("attoptions", 1009, false),
    col("attfdwoptions", 1009, false),
    col("attmissingval", 2277, false),
];

#[rustfmt::skip]
static PG_TYPE_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("typname", 19, true),
    col("typnamespace", 26, true),
    col("typowner", 26, true),
    col("typlen", 21, true),
    col("typbyval", 16, true),
    col("typtype", 18, true),
    col("typcategory", 18, true),
    col("typispreferred", 16, true),
    col("typisdefined", 16, true),
    col("typdelim", 18, true),
    col("typrelid", 26, true),
    col("typsubscript", 24, true),
    col("typelem", 26, true),
    col("typarray", 26, true),
    col("typinput", 24, true),
    col("typoutput", 24, true),
    col("typreceive", 24, true),
    col("typsend", 24, true),
    col("typmodin", 24, true),
    col("typmodout", 24, true),
    col("typanalyze", 24, true),
    col("typalign", 18, true),
    col("typstorage", 18, true),
    col("typnotnull", 16, true),
    col("typbasetype", 26, true),
    col("typtypmod", 23, true),
    col("typndims", 23, true),
    col("typcollation", 26, true),
    col("typdefaultbin", 194, false),
    col("typdefault", 25, false),
    col("typacl", 1034, false),
];

#[rustfmt::skip]
static PG_PROC_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("proname", 19, true),
    col("pronamespace", 26, true),
    col("proowner", 26, true),
    col("prolang", 26, true),
    col("procost", 700, true),
    col("prorows", 700, true),
    col("provariadic", 26, true),
    col("prosupport", 24, true),
    col("prokind", 18, true),
    col("prosecdef", 16, true),
    col("proleakproof", 16, true),
    col("proisstrict", 16, true),
    col("proretset", 16, true),
    col("provolatile", 18, true),
    col("proparallel", 18, true),
    col("pronargs", 21, true),
    col("pronargdefaults", 21, true),
    col("prorettype", 26, true),
    col("proargtypes", 30, true),
    col("proallargtypes", 1028, false),
    col("proargmodes", 1002, false),
    col("proargnames", 1009, false),
    col("proargdefaults", 194, false),
    col("protrftypes", 1028, false),
    col("prosrc", 25, true),
    col("probin", 25, false),
    col("prosqlbody", 194, false),
    col("proconfig", 1009, false),
    col("proacl", 1034, false),
];

#[rustfmt::skip]
static PG_NAMESPACE_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("nspname", 19, true),
    col("nspowner", 26, true),
    col("nspacl", 1034, false),
];

#[rustfmt::skip]
static PG_OPERATOR_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("oprname", 19, true),
    col("oprnamespace", 26, true),
    col("oprowner", 26, true),
    col("oprkind", 18, true),
    col("oprcanmerge", 16, true),
    col("oprcanhash", 16, true),
    col("oprleft", 26, true),
    col("oprright", 26, true),
    col("oprresult", 26, true),
    col("oprcom", 26, true),
    col("oprnegate", 26, true),
    col("oprcode", 24, true),
    col("oprrest", 24, true),
    col("oprjoin", 24, true),
];

#[rustfmt::skip]
static PG_CAST_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("castsource", 26, true),
    col("casttarget", 26, true),
    col("castfunc", 26, true),
    col("castcontext", 18, true),
    col("castmethod", 18, true),
];

#[rustfmt::skip]
static PG_AM_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("amname", 19, true),
    col("amhandler", 24, true),
    col("amtype", 18, true),
];

#[rustfmt::skip]
static PG_ATTRDEF_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("adrelid", 26, true),
    col("adnum", 21, true),
    col("adbin", 194, true),
];

#[rustfmt::skip]
static PG_CONSTRAINT_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("conname", 19, true),
    col("connamespace", 26, true),
    col("contype", 18, true),
    col("condeferrable", 16, true),
    col("condeferred", 16, true),
    col("convalidated", 16, true),
    col("conrelid", 26, true),
    col("contypid", 26, true),
    col("conindid", 26, true),
    col("conparentid", 26, true),
    col("confrelid", 26, true),
    col("confupdtype", 18, true),
    col("confdeltype", 18, true),
    col("confmatchtype", 18, true),
    col("conislocal", 16, true),
    col("coninhcount", 21, true),
    col("connoinherit", 16, true),
    col("conkey", 1005, false),
    col("confkey", 1005, false),
    col("conpfeqop", 1028, false),
    col("conppeqop", 1028, false),
    col("conffeqop", 1028, false),
    col("confdelsetcols", 1005, false),
    col("conexclop", 1028, false),
    col("conbin", 194, false),
];

#[rustfmt::skip]
static PG_DATABASE_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("datname", 19, true),
    col("datdba", 26, true),
    col("encoding", 23, true),
    col("datlocprovider", 18, true),
    col("datistemplate", 16, true),
    col("datallowconn", 16, true),
    col("dathasloginevt", 16, true),
    col("datconnlimit", 23, true),
    col("datfrozenxid", 28, true),
    col("datminmxid", 28, true),
    col("dattablespace", 26, true),
    col("datcollate", 25, true),
    col("datctype", 25, true),
    col("datlocale", 25, false),
    col("daticurules", 25, false),
    col("datcollversion", 25, false),
    col("datacl", 1034, false),
];

#[rustfmt::skip]
static PG_AUTHID_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("rolname", 19, true),
    col("rolsuper", 16, true),
    col("rolinherit", 16, true),
    col("rolcreaterole", 16, true),
    col("rolcreatedb", 16, true),
    col("rolcanlogin", 16, true),
    col("rolreplication", 16, true),
    col("rolbypassrls", 16, true),
    col("rolconnlimit", 23, true),
    col("rolpassword", 25, false),
    col("rolvaliduntil", 1184, false),
];

#[rustfmt::skip]
static PG_TABLESPACE_COLUMNS: &[CatalogColumn] = &[
    col("oid", 26, true),
    col("spcname", 19, true),
    col("spcowner", 26, true),
    col("spcacl", 1034, false),
    col("spcoptions", 1009, false),
];

/// The 13 catalogs (`m2.md` §6.8.1), shared ones last.
#[rustfmt::skip]
pub static CATALOGS: &[CatalogDef] = &[
    CatalogDef { oid: oids::PG_CLASS, name: "pg_class", shared: false, mapped: true, rowtype_oid: 83, columns: PG_CLASS_COLUMNS },
    CatalogDef { oid: oids::PG_ATTRIBUTE, name: "pg_attribute", shared: false, mapped: true, rowtype_oid: 75, columns: PG_ATTRIBUTE_COLUMNS },
    CatalogDef { oid: oids::PG_TYPE, name: "pg_type", shared: false, mapped: true, rowtype_oid: 71, columns: PG_TYPE_COLUMNS },
    CatalogDef { oid: oids::PG_PROC, name: "pg_proc", shared: false, mapped: true, rowtype_oid: 81, columns: PG_PROC_COLUMNS },
    CatalogDef { oid: oids::PG_NAMESPACE, name: "pg_namespace", shared: false, mapped: false, rowtype_oid: 0, columns: PG_NAMESPACE_COLUMNS },
    CatalogDef { oid: oids::PG_OPERATOR, name: "pg_operator", shared: false, mapped: false, rowtype_oid: 0, columns: PG_OPERATOR_COLUMNS },
    CatalogDef { oid: oids::PG_CAST, name: "pg_cast", shared: false, mapped: false, rowtype_oid: 0, columns: PG_CAST_COLUMNS },
    CatalogDef { oid: oids::PG_AM, name: "pg_am", shared: false, mapped: false, rowtype_oid: 0, columns: PG_AM_COLUMNS },
    CatalogDef { oid: oids::PG_ATTRDEF, name: "pg_attrdef", shared: false, mapped: false, rowtype_oid: 0, columns: PG_ATTRDEF_COLUMNS },
    CatalogDef { oid: oids::PG_CONSTRAINT, name: "pg_constraint", shared: false, mapped: false, rowtype_oid: 0, columns: PG_CONSTRAINT_COLUMNS },
    CatalogDef { oid: oids::PG_DATABASE, name: "pg_database", shared: true, mapped: true, rowtype_oid: 1248, columns: PG_DATABASE_COLUMNS },
    CatalogDef { oid: oids::PG_AUTHID, name: "pg_authid", shared: true, mapped: true, rowtype_oid: 2842, columns: PG_AUTHID_COLUMNS },
    CatalogDef { oid: oids::PG_TABLESPACE, name: "pg_tablespace", shared: true, mapped: true, rowtype_oid: 0, columns: PG_TABLESPACE_COLUMNS },
];

/// The catalog with this OID.
pub fn catalog_def(oid: Oid) -> Option<&'static CatalogDef> {
    CATALOGS.iter().find(|c| c.oid == oid)
}

/// The catalog with this name (`pg_catalog.<name>`).
pub fn catalog_by_name(name: &str) -> Option<&'static CatalogDef> {
    CATALOGS.iter().find(|c| c.name == name)
}

/// `(name, attnum, type OID)` of the system columns.
pub const SYSTEM_COLUMNS: [(&str, i16, Oid); 6] = [
    ("ctid", -1, oid::TID),
    ("xmin", -2, oid::XID),
    ("cmin", -3, oid::CID),
    ("xmax", -4, oid::XID),
    ("cmax", -5, oid::CID),
    ("tableoid", -6, oid::OID),
];

/// The pinned `TableDef` of a system catalog. Shared catalogs ignore
/// `db_oid` and use `locator = (GLOBALTABLESPACE_OID, 0, relfilenode)`.
pub fn catalog_table_def(oid: Oid, db_oid: Oid) -> Option<Arc<TableDef>> {
    let def = catalog_def(oid)?;
    let columns = def
        .columns
        .iter()
        .zip(1i16..)
        .map(|(c, attnum)| ColumnDef {
            name: c.name.to_owned(),
            attnum,
            ty: SqlType::of(c.type_oid),
            not_null: c.not_null,
            default: None,
        })
        .collect();
    Some(Arc::new(TableDef {
        oid: def.oid,
        namespace: oids::NAMESPACE_PG_CATALOG,
        schema: "pg_catalog".to_owned(),
        name: def.name.to_owned(),
        kind: RelKind::Table,
        locator: def.locator(db_oid),
        columns,
        checks: Vec::<CheckDef>::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn natts(oid: Oid) -> usize {
        catalog_def(oid).unwrap().natts()
    }

    #[test]
    fn column_counts_match_postgresql_17() {
        assert_eq!(CATALOGS.len(), 13);
        assert_eq!(natts(oids::PG_CLASS), 33);
        assert_eq!(natts(oids::PG_ATTRIBUTE), 26);
        assert_eq!(natts(oids::PG_TYPE), 32);
        assert_eq!(natts(oids::PG_PROC), 30);
        assert_eq!(natts(oids::PG_NAMESPACE), 4);
        assert_eq!(natts(oids::PG_OPERATOR), 15);
        assert_eq!(natts(oids::PG_CAST), 6);
        assert_eq!(natts(oids::PG_AM), 4);
        assert_eq!(natts(oids::PG_ATTRDEF), 4);
        assert_eq!(natts(oids::PG_CONSTRAINT), 26);
        assert_eq!(natts(oids::PG_DATABASE), 18);
        assert_eq!(natts(oids::PG_AUTHID), 12);
        assert_eq!(natts(oids::PG_TABLESPACE), 5);
    }

    #[test]
    fn spot_check_columns() {
        let c = catalog_def(oids::PG_CLASS).unwrap();
        assert_eq!(c.columns[0].name, "oid");
        assert_eq!(
            c.columns[c.column_index("relkind").unwrap()].type_oid,
            oid::CHAR
        );
        assert_eq!(
            c.columns[c.column_index("relpages").unwrap()].type_oid,
            oid::INT4
        );
        assert_eq!(
            c.columns[c.column_index("reltuples").unwrap()].type_oid,
            oid::FLOAT4
        );
        assert_eq!(
            c.columns[c.column_index("relacl").unwrap()].type_oid,
            oid::ACLITEM_ARRAY
        );
        assert!(!c.columns[c.column_index("relacl").unwrap()].not_null);
        let a = catalog_def(oids::PG_ATTRIBUTE).unwrap();
        assert_eq!(
            a.columns[a.column_index("attstattarget").unwrap()].type_oid,
            oid::INT2
        );
        assert!(!a.columns[a.column_index("attstattarget").unwrap()].not_null);
        let p = catalog_def(oids::PG_PROC).unwrap();
        assert_eq!(
            p.columns[p.column_index("proargtypes").unwrap()].type_oid,
            oid::OIDVECTOR
        );
        let k = catalog_def(oids::PG_CONSTRAINT).unwrap();
        assert_eq!(
            k.columns[k.column_index("conbin").unwrap()].type_oid,
            oid::PG_NODE_TREE
        );
        assert_eq!(k.columns.last().unwrap().name, "conbin");
    }

    #[test]
    fn identities_and_locations() {
        let mut seen = std::collections::HashSet::new();
        for c in CATALOGS {
            assert!(seen.insert(c.oid));
            assert!(c.oid < oid::FIRST_GENBKI_OBJECT_ID);
        }
        let shared: Vec<_> = CATALOGS
            .iter()
            .filter(|c| c.shared)
            .map(|c| c.name)
            .collect();
        assert_eq!(shared, ["pg_database", "pg_authid", "pg_tablespace"]);
        let mapped: Vec<_> = CATALOGS
            .iter()
            .filter(|c| c.mapped)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            mapped,
            [
                "pg_class",
                "pg_attribute",
                "pg_type",
                "pg_proc",
                "pg_database",
                "pg_authid",
                "pg_tablespace"
            ]
        );
        assert_eq!(catalog_def(oids::PG_CLASS).unwrap().rowtype_oid, 83);
        assert_eq!(catalog_def(oids::PG_AM).unwrap().rowtype_oid, 0);
        assert_eq!(catalog_def(oids::PG_AM).unwrap().relfilenode(), oids::PG_AM);
        assert_eq!(catalog_def(oids::PG_CLASS).unwrap().relfilenode(), 0);
        assert_eq!(catalog_def(oids::PG_AUTHID).unwrap().reltablespace(), 1664);
        assert_eq!(catalog_def(oids::PG_CLASS).unwrap().reltablespace(), 0);
        assert_eq!(catalog_by_name("pg_type").unwrap().oid, oids::PG_TYPE);
        assert!(catalog_by_name("pg_index").is_none());
        assert!(catalog_def(1).is_none());
    }

    #[test]
    fn pinned_table_defs() {
        let t = catalog_table_def(oids::PG_CLASS, 5).unwrap();
        assert_eq!(t.name, "pg_class");
        assert_eq!(t.schema, "pg_catalog");
        assert_eq!(t.namespace, 11);
        assert!(t.is_system_catalog());
        assert_eq!(t.columns.len(), 33);
        assert_eq!(t.columns[0].attnum, 1);
        assert_eq!(t.columns[32].attnum, 33);
        assert_eq!(t.locator.spc_oid, 1663);
        assert_eq!(t.locator.db_oid, 5);
        assert_eq!(t.locator.rel_number, RelFileNumber(1259));
        // A shared catalog is the same file from every database.
        let a = catalog_table_def(oids::PG_DATABASE, 1).unwrap();
        let b = catalog_table_def(oids::PG_DATABASE, 5).unwrap();
        assert_eq!(a.locator, b.locator);
        assert_eq!(a.locator.spc_oid, 1664);
        assert_eq!(a.locator.db_oid, 0);
        assert!(catalog_table_def(42, 5).is_none());
    }

    #[test]
    fn system_columns() {
        assert_eq!(SYSTEM_COLUMNS[0], ("ctid", -1, oid::TID));
        assert_eq!(SYSTEM_COLUMNS[5], ("tableoid", -6, oid::OID));
    }
}
