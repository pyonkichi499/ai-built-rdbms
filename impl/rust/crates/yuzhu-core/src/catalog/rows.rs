//! Initial rows of the system catalogs, built from `builtin.rs` and
//! `schema.rs`, the row builders shared with `CatalogStore`, and
//! `builtin_hash` (`m2.md` §6.8.2).
//!
//! A row is a `Vec<Datum>` in the column order of `schema::CATALOGS`. Rows
//! are built by column name ([`RowBuilder`]) so that a typo is caught by the
//! unit tests (which build every row) and not by a misplaced value.

use std::collections::HashSet;

use super::builtin::{self, BuiltinType, CastContext};
use super::schema::{self, CatalogDef, SYSTEM_COLUMNS, oids};
use crate::types::{Datum, Oid, Row, SqlType, oid};

/// The OID of the first `pg_cast` row; the others follow in
/// `(castsource, casttarget)` order (`m2.md` §6.8.2).
const FIRST_CAST_OID: Oid = oid::FIRST_GENBKI_OBJECT_ID;

/// The `attcollation` of catalog columns whose type has a collation
/// (PostgreSQL forces `C` for them at bootstrap).
const C_COLLATION: Oid = 950;

/// `relfrozenxid` / `datfrozenxid` of rows created by initdb.
const FROZEN_XID: u32 = 3;

/// Builds a catalog row by column name. Columns that are not set are NULL.
#[derive(Debug)]
pub struct RowBuilder {
    def: &'static CatalogDef,
    values: Row,
}

impl RowBuilder {
    /// A row of the catalog with this OID (all NULL).
    ///
    /// # Panics
    /// If `catalog_oid` is not one of the 13 catalogs.
    pub fn new(catalog_oid: Oid) -> RowBuilder {
        let def = schema::catalog_def(catalog_oid).expect("RowBuilder: unknown catalog");
        RowBuilder {
            def,
            values: vec![Datum::Null; def.natts()],
        }
    }

    /// Sets a column.
    ///
    /// # Panics
    /// If the catalog has no column of that name.
    #[must_use]
    pub fn set(mut self, column: &str, value: Datum) -> RowBuilder {
        let i = self
            .def
            .column_index(column)
            .unwrap_or_else(|| panic!("{} has no column {column}", self.def.name));
        self.values[i] = value;
        self
    }

    pub fn build(self) -> Row {
        self.values
    }
}

fn text(s: &str) -> Datum {
    Datum::Text(s.to_owned())
}

fn ch(c: char) -> Datum {
    Datum::Char(u8::try_from(c).unwrap_or(0))
}

/// 0-based position of a column of a catalog.
///
/// # Panics
/// If the catalog or the column does not exist.
pub fn column_index(catalog_oid: Oid, column: &str) -> usize {
    schema::catalog_def(catalog_oid)
        .and_then(|d| d.column_index(column))
        .unwrap_or_else(|| panic!("no column {column} in catalog {catalog_oid}"))
}

// ----- row builders shared with CatalogStore -----------------------------------

/// What a `pg_class` row needs.
#[derive(Debug, Clone)]
pub struct ClassSpec<'a> {
    pub oid: Oid,
    pub name: &'a str,
    pub namespace: Oid,
    /// `reltype` (0 for tables created by users).
    pub reltype: Oid,
    pub owner: Oid,
    /// `relfilenode` (0 for mapped catalogs).
    pub relfilenode: Oid,
    pub reltablespace: Oid,
    pub is_shared: bool,
    /// Number of user columns.
    pub natts: i16,
    pub nchecks: i16,
    /// `relreplident`: `d` for user tables, `n` for catalogs.
    pub replident: char,
}

pub fn class_row(s: &ClassSpec<'_>) -> Row {
    RowBuilder::new(oids::PG_CLASS)
        .set("oid", Datum::Oid(s.oid))
        .set("relname", text(s.name))
        .set("relnamespace", Datum::Oid(s.namespace))
        .set("reltype", Datum::Oid(s.reltype))
        .set("reloftype", Datum::Oid(0))
        .set("relowner", Datum::Oid(s.owner))
        .set("relam", Datum::Oid(oids::HEAP_TABLE_AM))
        .set("relfilenode", Datum::Oid(s.relfilenode))
        .set("reltablespace", Datum::Oid(s.reltablespace))
        .set("relpages", Datum::Int4(0))
        .set("reltuples", Datum::Float4(-1.0))
        .set("relallvisible", Datum::Int4(0))
        .set("reltoastrelid", Datum::Oid(0))
        .set("relhasindex", Datum::Bool(false))
        .set("relisshared", Datum::Bool(s.is_shared))
        .set("relpersistence", ch('p'))
        .set("relkind", ch('r'))
        .set("relnatts", Datum::Int2(s.natts))
        .set("relchecks", Datum::Int2(s.nchecks))
        .set("relhasrules", Datum::Bool(false))
        .set("relhastriggers", Datum::Bool(false))
        .set("relhassubclass", Datum::Bool(false))
        .set("relrowsecurity", Datum::Bool(false))
        .set("relforcerowsecurity", Datum::Bool(false))
        .set("relispopulated", Datum::Bool(true))
        .set("relreplident", ch(s.replident))
        .set("relispartition", Datum::Bool(false))
        .set("relrewrite", Datum::Oid(0))
        .set("relfrozenxid", Datum::Xid(FROZEN_XID))
        .set("relminmxid", Datum::Xid(1))
        .build()
}

/// What a `pg_attribute` row needs.
#[derive(Debug, Clone)]
pub struct AttributeSpec<'a> {
    pub relid: Oid,
    pub name: &'a str,
    /// 1-based for user columns, negative for system columns.
    pub attnum: i16,
    pub ty: SqlType,
    pub not_null: bool,
    pub has_default: bool,
    /// A column of a system catalog: collatable types get `C` (950).
    pub catalog_column: bool,
}

/// A `pg_attribute` row. The length, alignment and storage come from the
/// type table. Returns `None` for a type that is not in `builtin::TYPES`.
pub fn attribute_row(s: &AttributeSpec<'_>) -> Option<Row> {
    let t = builtin::type_by_oid(s.ty.oid)?;
    let collation = match (s.catalog_column, t.collation) {
        (true, c) if c != 0 => C_COLLATION,
        (_, c) => c,
    };
    Some(
        RowBuilder::new(oids::PG_ATTRIBUTE)
            .set("attrelid", Datum::Oid(s.relid))
            .set("attname", text(s.name))
            .set("atttypid", Datum::Oid(s.ty.oid))
            .set("attlen", Datum::Int2(t.typlen))
            .set("attnum", Datum::Int2(s.attnum))
            .set("attcacheoff", Datum::Int4(-1))
            .set("atttypmod", Datum::Int4(s.ty.typmod))
            .set("attndims", Datum::Int2(i16::from(t.category == 'A')))
            .set("attbyval", Datum::Bool(t.typbyval))
            .set("attalign", ch(t.align))
            .set("attstorage", ch(t.storage))
            .set("attcompression", Datum::Char(0))
            .set("attnotnull", Datum::Bool(s.not_null))
            .set("atthasdef", Datum::Bool(s.has_default))
            .set("atthasmissing", Datum::Bool(false))
            .set("attidentity", Datum::Char(0))
            .set("attgenerated", Datum::Char(0))
            .set("attisdropped", Datum::Bool(false))
            .set("attislocal", Datum::Bool(true))
            .set("attinhcount", Datum::Int2(0))
            .set("attcollation", Datum::Oid(collation))
            .build(),
    )
}

/// The six `pg_attribute` rows of the system columns of a relation.
pub fn system_attribute_rows(relid: Oid, catalog_column: bool) -> Vec<Row> {
    SYSTEM_COLUMNS
        .iter()
        .filter_map(|(name, attnum, type_oid)| {
            attribute_row(&AttributeSpec {
                relid,
                name,
                attnum: *attnum,
                ty: SqlType::of(*type_oid),
                not_null: true,
                has_default: false,
                catalog_column,
            })
        })
        .collect()
}

pub fn attrdef_row(oid: Oid, relid: Oid, adnum: i16, expr_sql: &str) -> Row {
    RowBuilder::new(oids::PG_ATTRDEF)
        .set("oid", Datum::Oid(oid))
        .set("adrelid", Datum::Oid(relid))
        .set("adnum", Datum::Int2(adnum))
        .set("adbin", text(expr_sql))
        .build()
}

/// A CHECK constraint row (`contype = 'c'`). `conkey` stays NULL because
/// `int2[]` is a NULL-only type in M2.
pub fn check_constraint_row(
    oid: Oid,
    name: &str,
    namespace: Oid,
    relid: Oid,
    expr_sql: &str,
) -> Row {
    RowBuilder::new(oids::PG_CONSTRAINT)
        .set("oid", Datum::Oid(oid))
        .set("conname", text(name))
        .set("connamespace", Datum::Oid(namespace))
        .set("contype", ch('c'))
        .set("condeferrable", Datum::Bool(false))
        .set("condeferred", Datum::Bool(false))
        .set("convalidated", Datum::Bool(true))
        .set("conrelid", Datum::Oid(relid))
        .set("contypid", Datum::Oid(0))
        .set("conindid", Datum::Oid(0))
        .set("conparentid", Datum::Oid(0))
        .set("confrelid", Datum::Oid(0))
        .set("confupdtype", ch(' '))
        .set("confdeltype", ch(' '))
        .set("confmatchtype", ch(' '))
        .set("conislocal", Datum::Bool(true))
        .set("coninhcount", Datum::Int2(0))
        .set("connoinherit", Datum::Bool(false))
        .set("conbin", text(expr_sql))
        .build()
}

// ----- initial rows -------------------------------------------------------------

/// Parameters of the initial rows that are not constants.
#[derive(Debug, Clone)]
pub struct InitParams {
    /// `initdb -U`: the name of role 10.
    pub superuser: String,
}

/// The initial rows of one catalog, in the order they are inserted. The
/// catalogs with no initial rows (`pg_attrdef`, `pg_constraint`) give an
/// empty list. Per-database catalogs are the same in every database.
pub fn initial_rows(catalog_oid: Oid, params: &InitParams) -> Vec<Row> {
    match catalog_oid {
        oids::PG_NAMESPACE => namespace_rows(),
        oids::PG_AUTHID => authid_rows(&params.superuser),
        oids::PG_TABLESPACE => tablespace_rows(),
        oids::PG_AM => am_rows(),
        oids::PG_DATABASE => database_rows(),
        oids::PG_TYPE => type_rows(),
        oids::PG_PROC => proc_rows(),
        oids::PG_OPERATOR => operator_rows(),
        oids::PG_CAST => cast_rows(),
        oids::PG_CLASS => schema::CATALOGS.iter().map(catalog_class_row).collect(),
        oids::PG_ATTRIBUTE => catalog_attribute_rows(),
        _ => Vec::new(),
    }
}

fn namespace_rows() -> Vec<Row> {
    [
        (
            oids::NAMESPACE_PG_CATALOG,
            "pg_catalog",
            oids::BOOTSTRAP_SUPERUSER,
        ),
        (
            oids::NAMESPACE_PG_TOAST,
            "pg_toast",
            oids::BOOTSTRAP_SUPERUSER,
        ),
        (oids::NAMESPACE_PUBLIC, "public", oids::DATABASE_OWNER),
    ]
    .into_iter()
    .map(|(o, name, owner)| {
        RowBuilder::new(oids::PG_NAMESPACE)
            .set("oid", Datum::Oid(o))
            .set("nspname", text(name))
            .set("nspowner", Datum::Oid(owner))
            .build()
    })
    .collect()
}

fn authid_rows(superuser: &str) -> Vec<Row> {
    let role = |oid: Oid, name: &str, privileged: bool, inherit: bool, login: bool| {
        RowBuilder::new(oids::PG_AUTHID)
            .set("oid", Datum::Oid(oid))
            .set("rolname", text(name))
            .set("rolsuper", Datum::Bool(privileged))
            .set("rolinherit", Datum::Bool(inherit))
            .set("rolcreaterole", Datum::Bool(privileged))
            .set("rolcreatedb", Datum::Bool(privileged))
            .set("rolcanlogin", Datum::Bool(login))
            .set("rolreplication", Datum::Bool(privileged))
            .set("rolbypassrls", Datum::Bool(privileged))
            .set("rolconnlimit", Datum::Int4(-1))
            .build()
    };
    vec![
        role(oids::BOOTSTRAP_SUPERUSER, superuser, true, true, true),
        role(
            oids::DATABASE_OWNER,
            "pg_database_owner",
            false,
            true,
            false,
        ),
    ]
}

fn tablespace_rows() -> Vec<Row> {
    [(1663, "pg_default"), (1664, "pg_global")]
        .into_iter()
        .map(|(o, name)| {
            RowBuilder::new(oids::PG_TABLESPACE)
                .set("oid", Datum::Oid(o))
                .set("spcname", text(name))
                .set("spcowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .build()
        })
        .collect()
}

fn am_rows() -> Vec<Row> {
    [
        (oids::HEAP_TABLE_AM, "heap", oids::HEAP_TABLEAM_HANDLER, 't'),
        (oids::BTREE_AM, "btree", oids::BTHANDLER, 'i'),
    ]
    .into_iter()
    .map(|(o, name, handler, kind)| {
        RowBuilder::new(oids::PG_AM)
            .set("oid", Datum::Oid(o))
            .set("amname", text(name))
            .set("amhandler", Datum::Oid(handler))
            .set("amtype", ch(kind))
            .build()
    })
    .collect()
}

fn database_rows() -> Vec<Row> {
    // (oid, name, istemplate, allowconn)
    [
        (1, "template1", true, true),
        (4, "template0", true, false),
        (5, "postgres", false, true),
    ]
    .into_iter()
    .map(|(o, name, is_template, allow_conn)| {
        RowBuilder::new(oids::PG_DATABASE)
            .set("oid", Datum::Oid(o))
            .set("datname", text(name))
            .set("datdba", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
            .set("encoding", Datum::Int4(6))
            .set("datlocprovider", ch('c'))
            .set("datistemplate", Datum::Bool(is_template))
            .set("datallowconn", Datum::Bool(allow_conn))
            .set("dathasloginevt", Datum::Bool(false))
            .set("datconnlimit", Datum::Int4(-1))
            .set("datfrozenxid", Datum::Xid(FROZEN_XID))
            .set("datminmxid", Datum::Xid(1))
            .set("dattablespace", Datum::Oid(1663))
            .set("datcollate", text("C"))
            .set("datctype", text("C"))
            .build()
    })
    .collect()
}

/// `typarray` as the row shows it: the array type only if it has a row.
fn typarray(t: &BuiltinType) -> Oid {
    if t.array_oid != 0 && builtin::TYPES.iter().any(|a| a.oid == t.array_oid) {
        t.array_oid
    } else {
        0
    }
}

fn type_rows() -> Vec<Row> {
    let mut types: Vec<&BuiltinType> = builtin::TYPES.iter().collect();
    types.sort_by_key(|t| t.oid);
    types
        .into_iter()
        .map(|t| {
            RowBuilder::new(oids::PG_TYPE)
                .set("oid", Datum::Oid(t.oid))
                .set("typname", text(t.name))
                .set("typnamespace", Datum::Oid(oids::NAMESPACE_PG_CATALOG))
                .set("typowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .set("typlen", Datum::Int2(t.typlen))
                .set("typbyval", Datum::Bool(t.typbyval))
                .set("typtype", ch(t.typtype))
                .set("typcategory", ch(t.category))
                .set("typispreferred", Datum::Bool(t.preferred))
                .set("typisdefined", Datum::Bool(true))
                .set("typdelim", ch(t.delim))
                .set("typrelid", Datum::Oid(t.relid))
                .set("typsubscript", Datum::Oid(0))
                .set("typelem", Datum::Oid(t.elem))
                .set("typarray", Datum::Oid(typarray(t)))
                .set("typinput", Datum::Oid(t.input_oid))
                .set("typoutput", Datum::Oid(t.output_oid))
                .set("typreceive", Datum::Oid(0))
                .set("typsend", Datum::Oid(0))
                .set("typmodin", Datum::Oid(0))
                .set("typmodout", Datum::Oid(0))
                .set("typanalyze", Datum::Oid(0))
                .set("typalign", ch(t.align))
                .set("typstorage", ch(t.storage))
                .set("typnotnull", Datum::Bool(false))
                .set("typbasetype", Datum::Oid(0))
                .set("typtypmod", Datum::Int4(-1))
                .set("typndims", Datum::Int4(0))
                .set("typcollation", Datum::Oid(t.collation))
                .build()
        })
        .collect()
}

fn proc_rows() -> Vec<Row> {
    let mut procs: Vec<_> = builtin::PROCS.iter().collect();
    procs.sort_by_key(|p| p.oid);
    procs
        .into_iter()
        .map(|p| {
            RowBuilder::new(oids::PG_PROC)
                .set("oid", Datum::Oid(p.oid))
                .set("proname", text(p.name))
                .set("pronamespace", Datum::Oid(oids::NAMESPACE_PG_CATALOG))
                .set("proowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .set("prolang", Datum::Oid(oids::INTERNAL_LANGUAGE))
                .set("procost", Datum::Float4(p.cost))
                .set("prorows", Datum::Float4(0.0))
                .set("provariadic", Datum::Oid(0))
                .set("prosupport", Datum::Oid(0))
                .set("prokind", ch('f'))
                .set("prosecdef", Datum::Bool(false))
                .set("proleakproof", Datum::Bool(p.leakproof))
                .set("proisstrict", Datum::Bool(p.strict))
                .set("proretset", Datum::Bool(false))
                .set("provolatile", ch(p.volatility))
                .set("proparallel", ch(p.parallel))
                .set(
                    "pronargs",
                    Datum::Int2(i16::try_from(p.args.len()).unwrap_or(i16::MAX)),
                )
                .set("pronargdefaults", Datum::Int2(0))
                .set("prorettype", Datum::Oid(p.result))
                .set("proargtypes", Datum::OidVector(p.args.to_vec()))
                .set("prosrc", text(p.prosrc))
                .build()
        })
        .collect()
}

fn operator_rows() -> Vec<Row> {
    let exists = |o: Oid| o != 0 && builtin::OPERATORS.iter().any(|x| x.oid == o);
    let resolved = |o: Oid| if exists(o) { o } else { 0 };
    let mut ops: Vec<_> = builtin::OPERATORS.iter().collect();
    ops.sort_by_key(|o| o.oid);
    ops.into_iter()
        .map(|o| {
            let meta = builtin::operator_meta(o.oid).expect("every operator has OPERATOR_META");
            let (can_merge, can_hash) = builtin::operator_merge_hash(o.oid);
            RowBuilder::new(oids::PG_OPERATOR)
                .set("oid", Datum::Oid(o.oid))
                .set("oprname", text(o.name))
                .set("oprnamespace", Datum::Oid(oids::NAMESPACE_PG_CATALOG))
                .set("oprowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .set("oprkind", ch(o.kind()))
                .set("oprcanmerge", Datum::Bool(can_merge))
                .set("oprcanhash", Datum::Bool(can_hash))
                .set("oprleft", Datum::Oid(o.left.unwrap_or(0)))
                .set("oprright", Datum::Oid(o.right))
                .set("oprresult", Datum::Oid(o.result))
                .set("oprcom", Datum::Oid(resolved(meta.com)))
                .set("oprnegate", Datum::Oid(resolved(meta.negate)))
                .set("oprcode", Datum::Oid(meta.proc_oid))
                .set("oprrest", Datum::Oid(0))
                .set("oprjoin", Datum::Oid(0))
                .build()
        })
        .collect()
}

fn cast_rows() -> Vec<Row> {
    let mut casts: Vec<_> = builtin::CASTS.iter().collect();
    casts.sort_by_key(|c| (c.source, c.target));
    casts
        .into_iter()
        .zip(FIRST_CAST_OID..)
        .map(|(c, o)| {
            RowBuilder::new(oids::PG_CAST)
                .set("oid", Datum::Oid(o))
                .set("castsource", Datum::Oid(c.source))
                .set("casttarget", Datum::Oid(c.target))
                .set("castfunc", Datum::Oid(c.func_oid))
                .set("castcontext", ch(c.context.code()))
                .set("castmethod", ch(c.pg_method))
                .build()
        })
        .collect()
}

fn catalog_class_row(def: &'static CatalogDef) -> Row {
    class_row(&ClassSpec {
        oid: def.oid,
        name: def.name,
        namespace: oids::NAMESPACE_PG_CATALOG,
        reltype: def.rowtype_oid,
        owner: oids::BOOTSTRAP_SUPERUSER,
        relfilenode: def.relfilenode(),
        reltablespace: def.reltablespace(),
        is_shared: def.shared,
        natts: i16::try_from(def.natts()).unwrap_or(i16::MAX),
        nchecks: 0,
        replident: 'n',
    })
}

fn catalog_attribute_rows() -> Vec<Row> {
    let mut rows = Vec::new();
    for def in schema::CATALOGS {
        for (c, attnum) in def.columns.iter().zip(1i16..) {
            let spec = AttributeSpec {
                relid: def.oid,
                name: c.name,
                attnum,
                ty: SqlType::of(c.type_oid),
                not_null: c.not_null,
                has_default: false,
                catalog_column: true,
            };
            rows.extend(attribute_row(&spec));
        }
        rows.extend(system_attribute_rows(def.oid, true));
    }
    rows
}

// ----- builtin_hash ---------------------------------------------------------------

/// FNV-1a, 64 bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Appends a length-prefixed byte string.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The canonical bytes of one value: a type tag, then the value in
/// little-endian form.
fn put_datum(out: &mut Vec<u8>, d: &Datum) {
    match d {
        Datum::Null => out.push(0),
        Datum::Bool(v) => out.extend([1, u8::from(*v)]),
        Datum::Int2(v) => {
            out.push(2);
            out.extend(v.to_le_bytes());
        }
        Datum::Int4(v) => {
            out.push(3);
            out.extend(v.to_le_bytes());
        }
        Datum::Int8(v) => {
            out.push(4);
            out.extend(v.to_le_bytes());
        }
        Datum::Float4(v) => {
            out.push(5);
            out.extend(v.to_bits().to_le_bytes());
        }
        Datum::Float8(v) => {
            out.push(6);
            out.extend(v.to_bits().to_le_bytes());
        }
        Datum::Text(s) => {
            out.push(7);
            put_bytes(out, s.as_bytes());
        }
        Datum::Oid(v) => {
            out.push(8);
            out.extend(v.to_le_bytes());
        }
        Datum::Char(v) => out.extend([9, *v]),
        Datum::Xid(v) => {
            out.push(10);
            out.extend(v.to_le_bytes());
        }
        Datum::Cid(v) => {
            out.push(11);
            out.extend(v.to_le_bytes());
        }
        Datum::Tid(t) => {
            out.push(12);
            out.extend(t.block.to_le_bytes());
            out.extend(t.offset.to_le_bytes());
        }
        Datum::OidVector(v) => {
            out.push(13);
            out.extend(u32::try_from(v.len()).unwrap_or(u32::MAX).to_le_bytes());
            for o in v {
                out.extend(o.to_le_bytes());
            }
        }
        Datum::Int4Array(v) => {
            out.push(14);
            out.extend(u32::try_from(v.len()).unwrap_or(u32::MAX).to_le_bytes());
            for e in v {
                match e {
                    Some(x) => out.extend([1u8].into_iter().chain(x.to_le_bytes())),
                    None => out.push(0),
                }
            }
        }
        Datum::Void => out.push(15),
        Datum::Numeric(n) => {
            out.push(16);
            put_bytes(out, &n.to_binary());
        }
    }
}

/// The canonical byte string of the initial rows of `pg_type`, `pg_proc`,
/// `pg_operator` and `pg_cast`. The rows are sorted by OID (`pg_cast` by
/// `(castsource, casttarget)`), so the order of the tables in `builtin.rs`
/// does not matter.
pub fn builtin_canonical_bytes() -> Vec<u8> {
    let params = InitParams {
        superuser: String::new(),
    };
    let mut out = Vec::new();
    for catalog in [
        oids::PG_TYPE,
        oids::PG_PROC,
        oids::PG_OPERATOR,
        oids::PG_CAST,
    ] {
        let rows = initial_rows(catalog, &params);
        out.extend(catalog.to_le_bytes());
        out.extend(u32::try_from(rows.len()).unwrap_or(u32::MAX).to_le_bytes());
        for row in &rows {
            for d in row {
                put_datum(&mut out, d);
            }
        }
    }
    out
}

/// The hash of the built-in rows (FNV-1a 64 over
/// [`builtin_canonical_bytes`]). initdb stores it in the control file and
/// startup compares it, so a binary whose built-in tables differ from the
/// ones that created the data directory refuses to start (`m2.md` §6.8.5).
pub fn builtin_hash() -> u64 {
    fnv1a(&builtin_canonical_bytes())
}

// ----- checks used by the tests and by initdb --------------------------------------

/// Whether a value of `d` is valid for a column of type `type_oid`
/// (NULL always is).
pub fn datum_matches_type(d: &Datum, type_oid: Oid) -> bool {
    match d {
        Datum::Null => true,
        Datum::Bool(_) => type_oid == oid::BOOL,
        Datum::Int2(_) => type_oid == oid::INT2,
        Datum::Int4(_) => type_oid == oid::INT4,
        Datum::Int8(_) => type_oid == oid::INT8,
        Datum::Float4(_) => type_oid == oid::FLOAT4,
        Datum::Float8(_) => type_oid == oid::FLOAT8,
        Datum::Numeric(_) => type_oid == oid::NUMERIC,
        Datum::Text(_) => matches!(
            type_oid,
            oid::TEXT | oid::VARCHAR | oid::NAME | oid::PG_NODE_TREE
        ),
        Datum::Oid(_) => matches!(type_oid, oid::OID | oid::REGPROC),
        Datum::Char(_) => type_oid == oid::CHAR,
        Datum::Xid(_) => type_oid == oid::XID,
        Datum::Cid(_) => type_oid == oid::CID,
        Datum::Tid(_) => type_oid == oid::TID,
        Datum::OidVector(_) => type_oid == oid::OIDVECTOR,
        Datum::Int4Array(_) => type_oid == oid::INT4_ARRAY,
        Datum::Void => type_oid == oid::VOID,
    }
}

/// Every OID column of the built-in catalog rows and the catalog that the
/// reference must resolve in; used to check that the rows are closed.
/// `prolang` is the only exception (`m2.md` §6.8.2) and is not listed.
fn reference_columns() -> Vec<(Oid, &'static str, Oid)> {
    use oids::{
        PG_AM, PG_AUTHID, PG_CAST, PG_CLASS, PG_NAMESPACE, PG_OPERATOR, PG_PROC, PG_TABLESPACE,
        PG_TYPE,
    };
    vec![
        (PG_CLASS, "relnamespace", PG_NAMESPACE),
        (PG_CLASS, "reltype", PG_TYPE),
        (PG_CLASS, "relowner", PG_AUTHID),
        (PG_CLASS, "relam", PG_AM),
        (PG_CLASS, "reltablespace", PG_TABLESPACE),
        (oids::PG_ATTRIBUTE, "attrelid", PG_CLASS),
        (oids::PG_ATTRIBUTE, "atttypid", PG_TYPE),
        (PG_TYPE, "typnamespace", PG_NAMESPACE),
        (PG_TYPE, "typowner", PG_AUTHID),
        (PG_TYPE, "typrelid", PG_CLASS),
        (PG_TYPE, "typelem", PG_TYPE),
        (PG_TYPE, "typarray", PG_TYPE),
        (PG_TYPE, "typinput", PG_PROC),
        (PG_TYPE, "typoutput", PG_PROC),
        (PG_PROC, "pronamespace", PG_NAMESPACE),
        (PG_PROC, "proowner", PG_AUTHID),
        (PG_PROC, "prorettype", PG_TYPE),
        (PG_OPERATOR, "oprnamespace", PG_NAMESPACE),
        (PG_OPERATOR, "oprowner", PG_AUTHID),
        (PG_OPERATOR, "oprleft", PG_TYPE),
        (PG_OPERATOR, "oprright", PG_TYPE),
        (PG_OPERATOR, "oprresult", PG_TYPE),
        (PG_OPERATOR, "oprcom", PG_OPERATOR),
        (PG_OPERATOR, "oprnegate", PG_OPERATOR),
        (PG_OPERATOR, "oprcode", PG_PROC),
        (PG_CAST, "castsource", PG_TYPE),
        (PG_CAST, "casttarget", PG_TYPE),
        (PG_CAST, "castfunc", PG_PROC),
        (PG_AM, "amhandler", PG_PROC),
        (PG_NAMESPACE, "nspowner", PG_AUTHID),
        (PG_TABLESPACE, "spcowner", PG_AUTHID),
        (oids::PG_DATABASE, "datdba", PG_AUTHID),
        (oids::PG_DATABASE, "dattablespace", PG_TABLESPACE),
    ]
}

/// Returns the references among the initial rows that point nowhere, as
/// `"catalog.column = oid"` strings (empty when the rows are closed). Also
/// checks the elements of `proargtypes`.
pub fn dangling_references(params: &InitParams) -> Vec<String> {
    let mut oids_of: std::collections::HashMap<Oid, HashSet<Oid>> =
        std::collections::HashMap::new();
    for def in schema::CATALOGS {
        let set = initial_rows(def.oid, params)
            .iter()
            .filter_map(|r| match r.first() {
                Some(Datum::Oid(o)) if def.columns[0].name == "oid" => Some(*o),
                _ => None,
            })
            .collect();
        oids_of.insert(def.oid, set);
    }
    let mut missing = Vec::new();
    for (catalog, column, target) in reference_columns() {
        let def = schema::catalog_def(catalog).expect("catalog");
        let idx = def.column_index(column).expect("column");
        for row in initial_rows(catalog, params) {
            if let Datum::Oid(v) = row[idx]
                && v != 0
                && !oids_of[&target].contains(&v)
            {
                missing.push(format!("{}.{column} = {v}", def.name));
            }
        }
    }
    let proc_def = schema::catalog_def(oids::PG_PROC).expect("pg_proc");
    let argtypes = proc_def.column_index("proargtypes").expect("proargtypes");
    for row in initial_rows(oids::PG_PROC, params) {
        if let Datum::OidVector(v) = &row[argtypes] {
            for t in v {
                if !oids_of[&oids::PG_TYPE].contains(t) {
                    missing.push(format!("pg_proc.proargtypes = {t}"));
                }
            }
        }
    }
    missing
}

/// The context of a cast, as a character (re-exported for `store.rs`).
pub fn cast_context_char(c: CastContext) -> char {
    c.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> InitParams {
        InitParams {
            superuser: "postgres".into(),
        }
    }

    fn get<'a>(row: &'a Row, catalog: Oid, column: &str) -> &'a Datum {
        &row[column_index(catalog, column)]
    }

    #[test]
    fn every_initial_row_has_the_shape_of_its_catalog() {
        for def in schema::CATALOGS {
            for row in initial_rows(def.oid, &params()) {
                assert_eq!(row.len(), def.natts(), "{}", def.name);
                for (d, c) in row.iter().zip(def.columns) {
                    assert!(
                        datum_matches_type(d, c.type_oid),
                        "{}.{}: {d:?} is not a {}",
                        def.name,
                        c.name,
                        c.type_oid
                    );
                    if c.not_null {
                        assert!(!d.is_null(), "{}.{} is NOT NULL", def.name, c.name);
                    }
                }
            }
        }
    }

    #[test]
    fn null_only_columns_are_null() {
        for def in schema::CATALOGS {
            for row in initial_rows(def.oid, &params()) {
                for (d, c) in row.iter().zip(def.columns) {
                    if builtin::is_null_only_type(c.type_oid) {
                        assert!(d.is_null(), "{}.{}", def.name, c.name);
                    }
                }
            }
        }
    }

    #[test]
    fn row_counts() {
        let n = |oid| initial_rows(oid, &params()).len();
        assert_eq!(n(oids::PG_NAMESPACE), 3);
        assert_eq!(n(oids::PG_AUTHID), 2);
        assert_eq!(n(oids::PG_TABLESPACE), 2);
        assert_eq!(n(oids::PG_AM), 2);
        assert_eq!(n(oids::PG_DATABASE), 3);
        assert_eq!(n(oids::PG_CLASS), 13);
        assert_eq!(n(oids::PG_ATTRDEF), 0);
        assert_eq!(n(oids::PG_CONSTRAINT), 0);
        assert_eq!(n(oids::PG_TYPE), builtin::TYPES.len());
        assert_eq!(n(oids::PG_PROC), builtin::PROCS.len());
        assert_eq!(n(oids::PG_OPERATOR), builtin::OPERATORS.len());
        assert_eq!(n(oids::PG_CAST), builtin::CASTS.len());
        let natts: usize = schema::CATALOGS.iter().map(|c| c.natts() + 6).sum();
        assert_eq!(n(oids::PG_ATTRIBUTE), natts);
    }

    #[test]
    fn oids_are_unique_per_catalog() {
        for catalog in [
            oids::PG_TYPE,
            oids::PG_PROC,
            oids::PG_OPERATOR,
            oids::PG_CAST,
            oids::PG_CLASS,
            oids::PG_NAMESPACE,
            oids::PG_AUTHID,
            oids::PG_DATABASE,
            oids::PG_TABLESPACE,
            oids::PG_AM,
        ] {
            let mut seen = HashSet::new();
            for row in initial_rows(catalog, &params()) {
                let Datum::Oid(o) = row[0] else { panic!() };
                assert!(seen.insert(o), "duplicate OID {o} in catalog {catalog}");
            }
        }
    }

    #[test]
    fn references_are_closed() {
        let missing = dangling_references(&params());
        assert!(missing.is_empty(), "dangling: {missing:?}");
    }

    #[test]
    fn pg_class_rows_follow_the_spec() {
        let rows = initial_rows(oids::PG_CLASS, &params());
        let by_oid = |o| {
            rows.iter()
                .find(|r| r[0] == Datum::Oid(o))
                .unwrap_or_else(|| panic!("no pg_class row {o}"))
        };
        let c = oids::PG_CLASS;
        let r = by_oid(1259);
        assert_eq!(get(r, c, "relname"), &text("pg_class"));
        assert_eq!(get(r, c, "reltype"), &Datum::Oid(83));
        assert_eq!(get(r, c, "relfilenode"), &Datum::Oid(0));
        assert_eq!(get(r, c, "relnatts"), &Datum::Int2(33));
        assert_eq!(get(r, c, "relisshared"), &Datum::Bool(false));
        assert_eq!(get(r, c, "reltablespace"), &Datum::Oid(0));
        assert_eq!(get(r, c, "relreplident"), &Datum::Char(b'n'));
        assert_eq!(get(r, c, "relfrozenxid"), &Datum::Xid(3));
        let r = by_oid(1213);
        assert_eq!(get(r, c, "relisshared"), &Datum::Bool(true));
        assert_eq!(get(r, c, "reltablespace"), &Datum::Oid(1664));
        assert_eq!(get(r, c, "relfilenode"), &Datum::Oid(0));
        let r = by_oid(2601);
        assert_eq!(get(r, c, "relfilenode"), &Datum::Oid(2601));
        assert_eq!(get(r, c, "reltype"), &Datum::Oid(0));
        let r = by_oid(1260);
        assert_eq!(get(r, c, "reltype"), &Datum::Oid(2842));
    }

    #[test]
    fn pg_attribute_rows_of_a_catalog() {
        let rows = initial_rows(oids::PG_ATTRIBUTE, &params());
        let a = oids::PG_ATTRIBUTE;
        let find = |rel: Oid, name: &str| {
            rows.iter()
                .find(|r| r[0] == Datum::Oid(rel) && r[1] == text(name))
                .unwrap_or_else(|| panic!("no attribute {rel}.{name}"))
        };
        let r = find(1259, "relname");
        assert_eq!(get(r, a, "attnum"), &Datum::Int2(2));
        assert_eq!(get(r, a, "attlen"), &Datum::Int2(64));
        assert_eq!(get(r, a, "attcollation"), &Datum::Oid(950));
        assert_eq!(get(r, a, "attnotnull"), &Datum::Bool(true));
        assert_eq!(get(r, a, "attalign"), &Datum::Char(b'c'));
        assert_eq!(get(r, a, "attstorage"), &Datum::Char(b'p'));
        let r = find(1255, "prosrc");
        assert_eq!(get(r, a, "attcollation"), &Datum::Oid(950));
        assert_eq!(get(r, a, "attstorage"), &Datum::Char(b'x'));
        assert_eq!(get(r, a, "attlen"), &Datum::Int2(-1));
        let r = find(1259, "relacl");
        assert_eq!(get(r, a, "atttypid"), &Datum::Oid(oid::ACLITEM_ARRAY));
        assert_eq!(get(r, a, "attnotnull"), &Datum::Bool(false));
        let r = find(1259, "tableoid");
        assert_eq!(get(r, a, "attnum"), &Datum::Int2(-6));
        assert_eq!(get(r, a, "attnotnull"), &Datum::Bool(true));
        assert_eq!(get(r, a, "atttypid"), &Datum::Oid(oid::OID));
        let r = find(1259, "ctid");
        assert_eq!(get(r, a, "attlen"), &Datum::Int2(6));
        assert_eq!(get(r, a, "attbyval"), &Datum::Bool(false));
        assert_eq!(get(r, a, "attalign"), &Datum::Char(b's'));
        assert_eq!(get(r, a, "attislocal"), &Datum::Bool(true));
        assert_eq!(get(r, a, "attcacheoff"), &Datum::Int4(-1));
        let r = find(1255, "proargtypes");
        assert_eq!(get(r, a, "attndims"), &Datum::Int2(1));
        assert_eq!(get(find(1255, "proname"), a, "attndims"), &Datum::Int2(0));
    }

    #[test]
    fn fixed_rows() {
        let ns = initial_rows(oids::PG_NAMESPACE, &params());
        assert_eq!(ns[2][0], Datum::Oid(2200));
        assert_eq!(ns[2][2], Datum::Oid(6171));
        let auth = initial_rows(oids::PG_AUTHID, &params());
        assert_eq!(get(&auth[0], oids::PG_AUTHID, "rolname"), &text("postgres"));
        assert_eq!(
            get(&auth[0], oids::PG_AUTHID, "rolsuper"),
            &Datum::Bool(true)
        );
        assert_eq!(get(&auth[1], oids::PG_AUTHID, "oid"), &Datum::Oid(6171));
        assert_eq!(
            get(&auth[1], oids::PG_AUTHID, "rolsuper"),
            &Datum::Bool(false)
        );
        assert_eq!(
            get(&auth[1], oids::PG_AUTHID, "rolinherit"),
            &Datum::Bool(true)
        );
        assert_eq!(
            get(&auth[1], oids::PG_AUTHID, "rolcanlogin"),
            &Datum::Bool(false)
        );
        let other = initial_rows(
            oids::PG_AUTHID,
            &InitParams {
                superuser: "alice".into(),
            },
        );
        assert_eq!(get(&other[0], oids::PG_AUTHID, "rolname"), &text("alice"));
        let db = initial_rows(oids::PG_DATABASE, &params());
        let d = oids::PG_DATABASE;
        let names: Vec<_> = db.iter().map(|r| get(r, d, "datname").clone()).collect();
        assert_eq!(
            names,
            [text("template1"), text("template0"), text("postgres")]
        );
        assert_eq!(get(&db[1], d, "datallowconn"), &Datum::Bool(false));
        assert_eq!(get(&db[1], d, "datistemplate"), &Datum::Bool(true));
        assert_eq!(get(&db[2], d, "oid"), &Datum::Oid(5));
        assert_eq!(get(&db[2], d, "encoding"), &Datum::Int4(6));
        assert_eq!(get(&db[2], d, "datcollate"), &text("C"));
        let am = initial_rows(oids::PG_AM, &params());
        assert_eq!(am[0][1], text("heap"));
        assert_eq!(am[1][2], Datum::Oid(330));
    }

    #[test]
    fn type_rows_follow_the_spec() {
        let rows = initial_rows(oids::PG_TYPE, &params());
        let t = oids::PG_TYPE;
        let find = |o: Oid| {
            rows.iter()
                .find(|r| r[0] == Datum::Oid(o))
                .expect("type row")
        };
        // A type whose array type has a row keeps typarray, others get 0.
        assert_eq!(get(find(oid::OID), t, "typarray"), &Datum::Oid(1028));
        assert_eq!(get(find(oid::BOOL), t, "typarray"), &Datum::Oid(0));
        assert_eq!(get(find(83), t, "typarray"), &Datum::Oid(0));
        assert_eq!(
            get(find(oid::OIDVECTOR), t, "typelem"),
            &Datum::Oid(oid::OID)
        );
        assert_eq!(get(find(oid::NAME), t, "typcollation"), &Datum::Oid(950));
        assert_eq!(get(find(oid::TEXT), t, "typcollation"), &Datum::Oid(100));
        assert_eq!(get(find(oid::UNKNOWN), t, "typtype"), &Datum::Char(b'p'));
        assert_eq!(get(find(83), t, "typrelid"), &Datum::Oid(1259));
        assert_eq!(get(find(83), t, "typinput"), &Datum::Oid(2290));
        assert_eq!(get(find(oid::INT4), t, "typinput"), &Datum::Oid(42));
        assert_eq!(get(find(oid::INT4), t, "typreceive"), &Datum::Oid(0));
        assert_eq!(get(find(oid::INT4), t, "typtypmod"), &Datum::Int4(-1));
        assert_eq!(get(find(oid::INT4), t, "typdelim"), &Datum::Char(b','));
        // Sorted by OID.
        let oids_in_order: Vec<_> = rows.iter().map(|r| r[0].clone()).collect();
        let mut sorted = oids_in_order.clone();
        sorted.sort_by(crate::types::cmp_datum);
        assert_eq!(oids_in_order, sorted);
    }

    #[test]
    fn proc_operator_and_cast_rows() {
        let procs = initial_rows(oids::PG_PROC, &params());
        let p = oids::PG_PROC;
        let int4pl = procs
            .iter()
            .find(|r| r[0] == Datum::Oid(177))
            .expect("int4pl");
        assert_eq!(get(int4pl, p, "proname"), &text("int4pl"));
        assert_eq!(get(int4pl, p, "pronargs"), &Datum::Int2(2));
        assert_eq!(
            get(int4pl, p, "proargtypes"),
            &Datum::OidVector(vec![23, 23])
        );
        assert_eq!(get(int4pl, p, "prorettype"), &Datum::Oid(23));
        assert_eq!(get(int4pl, p, "prolang"), &Datum::Oid(12));
        assert_eq!(get(int4pl, p, "provolatile"), &Datum::Char(b'i'));
        let ops = initial_rows(oids::PG_OPERATOR, &params());
        let o = oids::PG_OPERATOR;
        let plus = ops
            .iter()
            .find(|r| r[0] == Datum::Oid(551))
            .expect("int4 +");
        assert_eq!(get(plus, o, "oprname"), &text("+"));
        assert_eq!(get(plus, o, "oprcode"), &Datum::Oid(177));
        assert_eq!(get(plus, o, "oprkind"), &Datum::Char(b'b'));
        assert_eq!(get(plus, o, "oprcom"), &Datum::Oid(551));
        assert_eq!(get(plus, o, "oprrest"), &Datum::Oid(0));
        let neg = ops
            .iter()
            .find(|r| r[0] == Datum::Oid(558))
            .expect("int4 unary -");
        assert_eq!(get(neg, o, "oprkind"), &Datum::Char(b'l'));
        assert_eq!(get(neg, o, "oprleft"), &Datum::Oid(0));
        let casts = initial_rows(oids::PG_CAST, &params());
        let c = oids::PG_CAST;
        assert_eq!(casts[0][0], Datum::Oid(10000));
        assert_eq!(casts[1][0], Datum::Oid(10001));
        let pairs: Vec<_> = casts
            .iter()
            .map(|r| {
                (
                    get(r, c, "castsource").clone(),
                    get(r, c, "casttarget").clone(),
                )
            })
            .collect();
        let mut sorted = pairs.clone();
        sorted.sort_by(|a, b| {
            crate::types::cmp_datum(&a.0, &b.0).then(crate::types::cmp_datum(&a.1, &b.1))
        });
        assert_eq!(pairs, sorted);
        let int4_oid = casts
            .iter()
            .find(|r| {
                get(r, c, "castsource") == &Datum::Oid(23)
                    && get(r, c, "casttarget") == &Datum::Oid(26)
            })
            .expect("int4 to oid");
        assert_eq!(get(int4_oid, c, "castmethod"), &Datum::Char(b'b'));
        assert_eq!(get(int4_oid, c, "castfunc"), &Datum::Oid(0));
        assert_eq!(get(int4_oid, c, "castcontext"), &Datum::Char(b'i'));
        let int4_int8 = casts
            .iter()
            .find(|r| {
                get(r, c, "castsource") == &Datum::Oid(23)
                    && get(r, c, "casttarget") == &Datum::Oid(20)
            })
            .expect("int4 to int8");
        assert_eq!(get(int4_int8, c, "castfunc"), &Datum::Oid(481));
        assert_eq!(get(int4_int8, c, "castmethod"), &Datum::Char(b'f'));
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        assert_eq!(builtin_hash(), builtin_hash());
        assert_eq!(builtin_canonical_bytes(), builtin_canonical_bytes());
        // FNV-1a test vectors.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
        // The initdb -U name is not part of the hash.
        assert_ne!(builtin_hash(), 0);
        // Changing a value changes the hash.
        let mut bytes = builtin_canonical_bytes();
        let before = fnv1a(&bytes);
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_ne!(before, fnv1a(&bytes));
    }

    #[allow(clippy::many_single_char_names)]
    #[test]
    fn row_builders_for_user_tables() {
        let c = class_row(&ClassSpec {
            oid: 16384,
            name: "t",
            namespace: 2200,
            reltype: 0,
            owner: 10,
            relfilenode: 16384,
            reltablespace: 0,
            is_shared: false,
            natts: 2,
            nchecks: 1,
            replident: 'd',
        });
        assert_eq!(c.len(), 33);
        assert_eq!(get(&c, oids::PG_CLASS, "relreplident"), &Datum::Char(b'd'));
        assert_eq!(get(&c, oids::PG_CLASS, "relchecks"), &Datum::Int2(1));
        let a = attribute_row(&AttributeSpec {
            relid: 16384,
            name: "b",
            attnum: 2,
            ty: SqlType::TEXT,
            not_null: false,
            has_default: false,
            catalog_column: false,
        })
        .expect("text is a built-in type");
        assert_eq!(
            get(&a, oids::PG_ATTRIBUTE, "attcollation"),
            &Datum::Oid(100)
        );
        let v = attribute_row(&AttributeSpec {
            relid: 16384,
            name: "v",
            attnum: 3,
            ty: SqlType::varchar(10),
            not_null: true,
            has_default: true,
            catalog_column: false,
        })
        .expect("varchar");
        assert_eq!(get(&v, oids::PG_ATTRIBUTE, "atttypmod"), &Datum::Int4(14));
        assert_eq!(get(&v, oids::PG_ATTRIBUTE, "atthasdef"), &Datum::Bool(true));
        assert!(
            attribute_row(&AttributeSpec {
                relid: 1,
                name: "x",
                attnum: 1,
                ty: SqlType::of(99_999),
                not_null: false,
                has_default: false,
                catalog_column: false,
            })
            .is_none()
        );
        assert_eq!(system_attribute_rows(16384, false).len(), 6);
        let d = attrdef_row(20000, 16384, 1, "1");
        assert_eq!(get(&d, oids::PG_ATTRDEF, "adbin"), &text("1"));
        let k = check_constraint_row(20001, "t_a_check", 2200, 16384, "a > 0");
        assert_eq!(get(&k, oids::PG_CONSTRAINT, "contype"), &Datum::Char(b'c'));
        assert_eq!(
            get(&k, oids::PG_CONSTRAINT, "confupdtype"),
            &Datum::Char(b' ')
        );
        assert!(get(&k, oids::PG_CONSTRAINT, "conkey").is_null());
        assert_eq!(get(&k, oids::PG_CONSTRAINT, "conbin"), &text("a > 0"));
        assert_eq!(cast_context_char(CastContext::Assignment), 'a');
    }
}
