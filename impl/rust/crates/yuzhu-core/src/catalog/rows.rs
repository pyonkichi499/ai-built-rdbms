//! Initial rows of the system catalogs, built from `builtin.rs` and
//! `schema.rs`, the row builders shared with `CatalogStore`, and
//! `builtin_hash` (`m2.md` §6.8.2).
//!
//! A row is a `Vec<Datum>` in the column order of `schema::CATALOGS`. Rows
//! are built by column name ([`RowBuilder`]) so that a typo is caught by the
//! unit tests (which build every row) and not by a misplaced value.

use std::collections::HashSet;

use super::builtin::{self, BuiltinType, CastContext};
use super::depend::NewDepend;
use super::opclass;
use super::schema::{self, CatalogDef, SYSTEM_COLUMNS, oids};
use super::{ConstraintKind, IdentityKind, RelKind, SequenceParams};
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
    /// `relkind` と、それで決まる既定値（`relam`、`relfrozenxid`、`relminmxid`）。
    pub kind: RelKind,
    /// `relhasindex`。
    pub has_index: bool,
    pub relpages: i32,
    pub reltuples: f32,
}

impl<'a> ClassSpec<'a> {
    /// 表の既定値（`relpages = 0`、`reltuples = -1`、索引なし）。
    pub fn table(oid: Oid, name: &'a str, namespace: Oid, owner: Oid, natts: i16) -> Self {
        ClassSpec {
            oid,
            name,
            namespace,
            reltype: 0,
            owner,
            relfilenode: oid,
            reltablespace: 0,
            is_shared: false,
            natts,
            nchecks: 0,
            replident: 'd',
            kind: RelKind::Table,
            has_index: false,
            relpages: 0,
            reltuples: -1.0,
        }
    }
}

/// `pg_class.relam`: 表は heap、索引は btree、シーケンスは 0。
fn relam_of(kind: RelKind) -> Oid {
    match kind {
        RelKind::Table => oids::HEAP_TABLE_AM,
        RelKind::Index => oids::BTREE_AM,
        RelKind::Sequence => 0,
    }
}

pub fn class_row(s: &ClassSpec<'_>) -> Row {
    // 表だけが凍結 XID を持つ（索引・シーケンスは 0）。
    let (frozen, minmxid) = match s.kind {
        RelKind::Table => (FROZEN_XID, 1),
        RelKind::Index | RelKind::Sequence => (0, 0),
    };
    RowBuilder::new(oids::PG_CLASS)
        .set("oid", Datum::Oid(s.oid))
        .set("relname", text(s.name))
        .set("relnamespace", Datum::Oid(s.namespace))
        .set("reltype", Datum::Oid(s.reltype))
        .set("reloftype", Datum::Oid(0))
        .set("relowner", Datum::Oid(s.owner))
        .set("relam", Datum::Oid(relam_of(s.kind)))
        .set("relfilenode", Datum::Oid(s.relfilenode))
        .set("reltablespace", Datum::Oid(s.reltablespace))
        .set("relpages", Datum::Int4(s.relpages))
        .set("reltuples", Datum::Float4(s.reltuples))
        .set("relallvisible", Datum::Int4(0))
        .set("reltoastrelid", Datum::Oid(0))
        .set("relhasindex", Datum::Bool(s.has_index))
        .set("relisshared", Datum::Bool(s.is_shared))
        .set("relpersistence", ch('p'))
        .set("relkind", ch(s.kind.code()))
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
        .set("relfrozenxid", Datum::Xid(frozen))
        .set("relminmxid", Datum::Xid(minmxid))
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
    /// `attidentity`（`None` = 通常の列）。
    pub identity: Option<IdentityKind>,
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
            .set(
                "attidentity",
                Datum::Char(
                    s.identity
                        .map_or(0, |i| u8::try_from(i.code()).unwrap_or(0)),
                ),
            )
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
                identity: None,
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

/// A CHECK constraint row (`contype = 'c'`)。`constraint_row` の薄い包み。
pub fn check_constraint_row(
    oid: Oid,
    name: &str,
    namespace: Oid,
    relid: Oid,
    expr_sql: &str,
    no_inherit: bool,
) -> Row {
    constraint_row(&ConstraintRowSpec {
        oid,
        name,
        namespace,
        kind: ConstraintKind::Check,
        relid,
        index_oid: 0,
        columns: &[],
        check_sql: Some(expr_sql),
        no_inherit,
    })
}

/// What a `pg_index` row needs (`m4/07-catalog-ddl.md` §3.4). 列ごとの配列は同じ長さ。
#[derive(Debug, Clone)]
pub struct IndexRowSpec<'a> {
    pub index_oid: Oid,
    pub table_oid: Oid,
    pub unique: bool,
    pub primary: bool,
    /// `indkey`: 表の attnum の並び。
    pub key: &'a [i16],
    /// `indcollation`: 列ごとの照合順序の OID。
    pub collations: &'a [Oid],
    /// `indclass`: 列ごとの演算子クラスの OID。
    pub classes: &'a [Oid],
    /// `indoption`: 列ごと。bit0 = DESC、bit1 = NULLS FIRST。
    pub options: &'a [i16],
}

/// A `pg_index` row. INCLUDE・式・部分索引・DEFERRABLE はないので、対応する列は固定値（または NULL）。
pub fn index_row(s: &IndexRowSpec<'_>) -> Row {
    let natts = i16::try_from(s.key.len()).unwrap_or(i16::MAX);
    RowBuilder::new(oids::PG_INDEX)
        .set("indexrelid", Datum::Oid(s.index_oid))
        .set("indrelid", Datum::Oid(s.table_oid))
        .set("indnatts", Datum::Int2(natts))
        .set("indnkeyatts", Datum::Int2(natts))
        .set("indisunique", Datum::Bool(s.unique))
        .set("indnullsnotdistinct", Datum::Bool(false))
        .set("indisprimary", Datum::Bool(s.primary))
        .set("indisexclusion", Datum::Bool(false))
        .set("indimmediate", Datum::Bool(true))
        .set("indisclustered", Datum::Bool(false))
        .set("indisvalid", Datum::Bool(true))
        .set("indcheckxmin", Datum::Bool(false))
        .set("indisready", Datum::Bool(true))
        .set("indislive", Datum::Bool(true))
        .set("indisreplident", Datum::Bool(false))
        .set("indkey", Datum::Int2Vector(s.key.to_vec()))
        .set("indcollation", Datum::OidVector(s.collations.to_vec()))
        .set("indclass", Datum::OidVector(s.classes.to_vec()))
        .set("indoption", Datum::Int2Vector(s.options.to_vec()))
        .build()
}

/// What a `pg_constraint` row needs (c / p / u)。
#[derive(Debug, Clone)]
pub struct ConstraintRowSpec<'a> {
    pub oid: Oid,
    pub name: &'a str,
    pub namespace: Oid,
    pub kind: ConstraintKind,
    pub relid: Oid,
    /// `conindid`（Check は 0）。
    pub index_oid: Oid,
    /// `conkey`（p / u）。Check は無視する（NULL）。
    pub columns: &'a [i16],
    /// `conbin`（Check）。
    pub check_sql: Option<&'a str>,
    /// CHECK の `NO INHERIT`（p / u の `connoinherit` は常に真）。
    pub no_inherit: bool,
}

fn constraint_type_char(kind: ConstraintKind) -> char {
    match kind {
        ConstraintKind::Check => 'c',
        ConstraintKind::PrimaryKey => 'p',
        ConstraintKind::Unique => 'u',
    }
}

/// A `pg_constraint` row. `connoinherit` は p / u で真、CHECK で偽（実測）。
/// `conkey` は p / u だけ値を持つ（CHECK は式の走査が要るので NULL。M5）。
pub fn constraint_row(s: &ConstraintRowSpec<'_>) -> Row {
    let is_check = s.kind == ConstraintKind::Check;
    let mut b = RowBuilder::new(oids::PG_CONSTRAINT)
        .set("oid", Datum::Oid(s.oid))
        .set("conname", text(s.name))
        .set("connamespace", Datum::Oid(s.namespace))
        .set("contype", ch(constraint_type_char(s.kind)))
        .set("condeferrable", Datum::Bool(false))
        .set("condeferred", Datum::Bool(false))
        .set("convalidated", Datum::Bool(true))
        .set("conrelid", Datum::Oid(s.relid))
        .set("contypid", Datum::Oid(0))
        .set("conindid", Datum::Oid(s.index_oid))
        .set("conparentid", Datum::Oid(0))
        .set("confrelid", Datum::Oid(0))
        .set("confupdtype", ch(' '))
        .set("confdeltype", ch(' '))
        .set("confmatchtype", ch(' '))
        .set("conislocal", Datum::Bool(true))
        .set("coninhcount", Datum::Int2(0))
        .set("connoinherit", Datum::Bool(!is_check || s.no_inherit));
    if is_check {
        if let Some(sql) = s.check_sql {
            b = b.set("conbin", text(sql));
        }
    } else {
        b = b.set("conkey", Datum::Int2Vector(s.columns.to_vec()));
    }
    b.build()
}

/// A `pg_depend` row.
pub fn depend_row(d: &NewDepend) -> Row {
    RowBuilder::new(oids::PG_DEPEND)
        .set("classid", Datum::Oid(d.dependent.class_id))
        .set("objid", Datum::Oid(d.dependent.obj_id))
        .set("objsubid", Datum::Int4(d.dependent.obj_sub))
        .set("refclassid", Datum::Oid(d.referenced.class_id))
        .set("refobjid", Datum::Oid(d.referenced.obj_id))
        .set("refobjsubid", Datum::Int4(d.referenced.obj_sub))
        .set("deptype", ch(d.deptype.code()))
        .build()
}

/// A `pg_sequence` row. `owned_by` は `pg_depend` の側にあるので書かない。
pub fn sequence_row(oid: Oid, p: &SequenceParams) -> Row {
    RowBuilder::new(oids::PG_SEQUENCE)
        .set("seqrelid", Datum::Oid(oid))
        .set("seqtypid", Datum::Oid(p.type_oid))
        .set("seqstart", Datum::Int8(p.start))
        .set("seqincrement", Datum::Int8(p.increment))
        .set("seqmax", Datum::Int8(p.max))
        .set("seqmin", Datum::Int8(p.min))
        .set("seqcache", Datum::Int8(p.cache))
        .set("seqcycle", Datum::Bool(p.cycle))
        .build()
}

/// The `pg_attribute` rows of an index: one per key column, no system columns
/// (`m4/07-catalog-ddl.md` §3.3)。型と typmod は表の列のもの。`attnotnull` は常に偽。
/// Returns `None`-free rows; a column whose type is not built in makes the whole call give an empty list
/// (the caller has validated the types).
pub fn index_attribute_rows(s: &super::store::NewIndex) -> Vec<Row> {
    s.columns
        .iter()
        .zip(1i16..)
        .filter_map(|(c, attnum)| {
            attribute_row(&AttributeSpec {
                relid: s.oid,
                name: &c.name,
                attnum,
                ty: c.ty,
                not_null: false,
                has_default: false,
                catalog_column: false,
                identity: None,
            })
        })
        .collect()
}

/// The `pg_attribute` rows of a sequence: `last_value` / `log_cnt` / `is_called`, then the system columns.
pub fn sequence_attribute_rows(relid: Oid) -> Vec<Row> {
    let mut rows: Vec<Row> = [
        ("last_value", SqlType::INT8),
        ("log_cnt", SqlType::INT8),
        ("is_called", SqlType::BOOL),
    ]
    .into_iter()
    .zip(1i16..)
    .filter_map(|((name, ty), attnum)| {
        attribute_row(&AttributeSpec {
            relid,
            name,
            attnum,
            ty,
            not_null: true,
            has_default: false,
            catalog_column: false,
            identity: None,
        })
    })
    .collect();
    rows.extend(system_attribute_rows(relid, false));
    rows
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
        oids::PG_LANGUAGE => language_rows(),
        oids::PG_OPFAMILY => opfamily_rows(),
        oids::PG_OPCLASS => opclass_rows(),
        oids::PG_AMOP => amop_rows(),
        oids::PG_AMPROC => amproc_rows(),
        // pg_index / pg_depend / pg_sequence / pg_description は空（行は DDL が書く）。
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
        (
            oids::NAMESPACE_INFORMATION_SCHEMA,
            "information_schema",
            oids::BOOTSTRAP_SUPERUSER,
        ),
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
                .set("prolang", Datum::Oid(oids::LANGUAGE_INTERNAL))
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

/// `pg_language` の 3 行（実測の PostgreSQL 17。`lanvalidator` は対応する `pg_proc` の行がないので 0）。
fn language_rows() -> Vec<Row> {
    // (oid, name, trusted)
    [
        (oids::LANGUAGE_INTERNAL, "internal", false),
        (oids::LANGUAGE_C, "c", false),
        (oids::LANGUAGE_SQL, "sql", true),
    ]
    .into_iter()
    .map(|(o, name, trusted)| {
        RowBuilder::new(oids::PG_LANGUAGE)
            .set("oid", Datum::Oid(o))
            .set("lanname", text(name))
            .set("lanowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
            .set("lanispl", Datum::Bool(false))
            .set("lanpltrusted", Datum::Bool(trusted))
            .set("lanplcallfoid", Datum::Oid(0))
            .set("laninline", Datum::Oid(0))
            .set("lanvalidator", Datum::Oid(0))
            .build()
    })
    .collect()
}

/// 静的な表（`opclass.rs`）から作る。oid 順に並べるので、表の書き順に左右されない。
fn opfamily_rows() -> Vec<Row> {
    let mut v: Vec<_> = opclass::OPFAMILIES.iter().collect();
    v.sort_by_key(|f| f.oid);
    v.into_iter()
        .map(|f| {
            RowBuilder::new(oids::PG_OPFAMILY)
                .set("oid", Datum::Oid(f.oid))
                .set("opfmethod", Datum::Oid(oids::BTREE_AM))
                .set("opfname", text(f.name))
                .set("opfnamespace", Datum::Oid(oids::NAMESPACE_PG_CATALOG))
                .set("opfowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .build()
        })
        .collect()
}

fn opclass_rows() -> Vec<Row> {
    let mut v: Vec<_> = opclass::OPCLASSES.iter().collect();
    v.sort_by_key(|c| c.oid);
    v.into_iter()
        .map(|c| {
            RowBuilder::new(oids::PG_OPCLASS)
                .set("oid", Datum::Oid(c.oid))
                .set("opcmethod", Datum::Oid(oids::BTREE_AM))
                .set("opcname", text(c.name))
                .set("opcnamespace", Datum::Oid(oids::NAMESPACE_PG_CATALOG))
                .set("opcowner", Datum::Oid(oids::BOOTSTRAP_SUPERUSER))
                .set("opcfamily", Datum::Oid(c.family))
                .set("opcintype", Datum::Oid(c.input_type))
                .set("opcdefault", Datum::Bool(c.is_default))
                // `name_ops` は PostgreSQL では cstring だが、yuzhu は name のまま扱う（D07-17）。
                .set("opckeytype", Datum::Oid(0))
                .build()
        })
        .collect()
}

/// `(family, left, right, strategy)` 順。`oid` は 10000 から振る（`pg_cast` と同じ）。
fn amop_rows() -> Vec<Row> {
    let mut v: Vec<_> = opclass::AMOPS.iter().collect();
    v.sort_by_key(|a| (a.family, a.left, a.right, a.strategy));
    v.into_iter()
        .zip(oid::FIRST_GENBKI_OBJECT_ID..)
        .map(|(a, o)| {
            RowBuilder::new(oids::PG_AMOP)
                .set("oid", Datum::Oid(o))
                .set("amopfamily", Datum::Oid(a.family))
                .set("amoplefttype", Datum::Oid(a.left))
                .set("amoprighttype", Datum::Oid(a.right))
                .set("amopstrategy", Datum::Int2(i16::from(a.strategy)))
                .set("amoppurpose", ch('s'))
                .set("amopopr", Datum::Oid(a.operator))
                .set("amopmethod", Datum::Oid(oids::BTREE_AM))
                .set("amopsortfamily", Datum::Oid(0))
                .build()
        })
        .collect()
}

fn amproc_rows() -> Vec<Row> {
    let mut v: Vec<_> = opclass::AMPROCS.iter().collect();
    v.sort_by_key(|a| (a.family, a.left, a.right, a.support));
    v.into_iter()
        .zip(oid::FIRST_GENBKI_OBJECT_ID..)
        .map(|(a, o)| {
            RowBuilder::new(oids::PG_AMPROC)
                .set("oid", Datum::Oid(o))
                .set("amprocfamily", Datum::Oid(a.family))
                .set("amproclefttype", Datum::Oid(a.left))
                .set("amprocrighttype", Datum::Oid(a.right))
                .set("amprocnum", Datum::Int2(i16::from(a.support)))
                .set("amproc", Datum::Oid(a.proc_oid))
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
        kind: RelKind::Table,
        has_index: false,
        relpages: 0,
        reltuples: -1.0,
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
                identity: None,
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
        Datum::BpChar(s) => {
            out.push(17);
            put_bytes(out, s.as_bytes());
        }
        Datum::Date(v) => {
            out.push(18);
            out.extend(v.0.to_le_bytes());
        }
        Datum::Timestamp(v) => {
            out.push(19);
            out.extend(v.0.to_le_bytes());
        }
        Datum::TimestampTz(v) => {
            out.push(20);
            out.extend(v.0.to_le_bytes());
        }
        Datum::Int2Vector(v) => {
            out.push(21);
            out.extend(u32::try_from(v.len()).unwrap_or(u32::MAX).to_le_bytes());
            for e in v {
                out.extend(e.to_le_bytes());
            }
        }
    }
}

/// The canonical byte string of the initial rows of `pg_type`, `pg_proc`,
/// `pg_operator`, `pg_cast`, `pg_language` and the rows generated from the
/// static operator-class tables (`pg_opfamily` / `pg_opclass` / `pg_amop` /
/// `pg_amproc`). The rows are sorted by OID (`pg_cast` by
/// `(castsource, casttarget)`, `pg_amop` / `pg_amproc` by their keys), so the
/// order of the tables in `builtin.rs` and `opclass.rs` does not matter
/// (`m4/07-catalog-ddl.md` §3.8).
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
        oids::PG_LANGUAGE,
        oids::PG_OPFAMILY,
        oids::PG_OPCLASS,
        oids::PG_AMOP,
        oids::PG_AMPROC,
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
        Datum::BpChar(_) => type_oid == oid::BPCHAR,
        Datum::Date(_) => type_oid == oid::DATE,
        Datum::Timestamp(_) => type_oid == oid::TIMESTAMP,
        Datum::TimestampTz(_) => type_oid == oid::TIMESTAMPTZ,
        Datum::Int2Vector(_) => matches!(type_oid, oid::INT2VECTOR | oid::INT2_ARRAY),
    }
}

/// Every OID column of the built-in catalog rows and the catalog that the
/// reference must resolve in; used to check that the rows are closed.
/// `pg_language` が実在するので、`prolang` も閉包に含まれる（`m4/07-catalog-ddl.md` §3.7）。
fn reference_columns() -> Vec<(Oid, &'static str, Oid)> {
    use oids::{
        PG_AM, PG_AMOP, PG_AMPROC, PG_AUTHID, PG_CAST, PG_CLASS, PG_LANGUAGE, PG_NAMESPACE,
        PG_OPCLASS, PG_OPERATOR, PG_OPFAMILY, PG_PROC, PG_TABLESPACE, PG_TYPE,
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
        (PG_PROC, "prolang", PG_LANGUAGE),
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
        (PG_OPFAMILY, "opfmethod", PG_AM),
        (PG_OPFAMILY, "opfnamespace", PG_NAMESPACE),
        (PG_OPFAMILY, "opfowner", PG_AUTHID),
        (PG_OPCLASS, "opcmethod", PG_AM),
        (PG_OPCLASS, "opcnamespace", PG_NAMESPACE),
        (PG_OPCLASS, "opcowner", PG_AUTHID),
        (PG_OPCLASS, "opcfamily", PG_OPFAMILY),
        (PG_OPCLASS, "opcintype", PG_TYPE),
        (PG_AMOP, "amopfamily", PG_OPFAMILY),
        (PG_AMOP, "amoplefttype", PG_TYPE),
        (PG_AMOP, "amoprighttype", PG_TYPE),
        (PG_AMOP, "amopopr", PG_OPERATOR),
        (PG_AMOP, "amopmethod", PG_AM),
        (PG_AMPROC, "amprocfamily", PG_OPFAMILY),
        (PG_AMPROC, "amproclefttype", PG_TYPE),
        (PG_AMPROC, "amprocrighttype", PG_TYPE),
        (PG_AMPROC, "amproc", PG_PROC),
        (PG_LANGUAGE, "lanowner", PG_AUTHID),
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
        assert_eq!(n(oids::PG_NAMESPACE), 4);
        assert_eq!(n(oids::PG_AUTHID), 2);
        assert_eq!(n(oids::PG_TABLESPACE), 2);
        assert_eq!(n(oids::PG_AM), 2);
        assert_eq!(n(oids::PG_DATABASE), 3);
        assert_eq!(n(oids::PG_CLASS), 22);
        assert_eq!(n(oids::PG_ATTRDEF), 0);
        assert_eq!(n(oids::PG_CONSTRAINT), 0);
        assert_eq!(n(oids::PG_TYPE), builtin::TYPES.len());
        assert_eq!(n(oids::PG_PROC), builtin::PROCS.len());
        assert_eq!(n(oids::PG_OPERATOR), builtin::OPERATORS.len());
        assert_eq!(n(oids::PG_CAST), builtin::CASTS.len());
        assert_eq!(n(oids::PG_LANGUAGE), 3);
        assert_eq!(n(oids::PG_OPFAMILY), opclass::OPFAMILIES.len());
        assert_eq!(n(oids::PG_OPCLASS), opclass::OPCLASSES.len());
        assert_eq!(n(oids::PG_AMOP), opclass::AMOPS.len());
        assert_eq!(n(oids::PG_AMPROC), opclass::AMPROCS.len());
        for empty in [
            oids::PG_INDEX,
            oids::PG_DEPEND,
            oids::PG_SEQUENCE,
            oids::PG_DESCRIPTION,
        ] {
            assert_eq!(n(empty), 0);
        }
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
            oids::PG_LANGUAGE,
            oids::PG_OPFAMILY,
            oids::PG_OPCLASS,
            oids::PG_AMOP,
            oids::PG_AMPROC,
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
            nchecks: 1,
            ..ClassSpec::table(16384, "t", 2200, 10, 2)
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
            identity: None,
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
            identity: None,
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
                identity: None,
            })
            .is_none()
        );
        assert_eq!(system_attribute_rows(16384, false).len(), 6);
        let d = attrdef_row(20000, 16384, 1, "1");
        assert_eq!(get(&d, oids::PG_ATTRDEF, "adbin"), &text("1"));
        let k = check_constraint_row(20001, "t_a_check", 2200, 16384, "a > 0", false);
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

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests_m4 {
    use super::*;
    use crate::catalog::depend::{DependType, ObjectAddress};

    fn get<'a>(row: &'a Row, catalog: Oid, column: &str) -> &'a Datum {
        &row[column_index(catalog, column)]
    }

    #[test]
    fn index_row_encodes_indoption_for_the_four_cases() {
        for (desc, nulls_first, want) in [
            (false, false, 0i16),
            (true, true, 3),
            (true, false, 1),
            (false, true, 2),
        ] {
            let r = index_row(&IndexRowSpec {
                index_oid: 20_000,
                table_oid: 16_384,
                unique: false,
                primary: false,
                key: &[3],
                collations: &[0],
                classes: &[1978],
                options: &[i16::from(desc) | (i16::from(nulls_first) << 1)],
            });
            let p = oids::PG_INDEX;
            assert_eq!(get(&r, p, "indoption"), &Datum::Int2Vector(vec![want]));
            assert_eq!(get(&r, p, "indkey"), &Datum::Int2Vector(vec![3]));
            assert_eq!(get(&r, p, "indclass"), &Datum::OidVector(vec![1978]));
            assert_eq!(get(&r, p, "indimmediate"), &Datum::Bool(true));
            assert_eq!(get(&r, p, "indnatts"), &Datum::Int2(1));
            assert!(get(&r, p, "indexprs").is_null() && get(&r, p, "indpred").is_null());
        }
    }

    #[test]
    fn constraint_rows_follow_the_measured_values() {
        let spec = |kind, cols: &'static [i16]| ConstraintRowSpec {
            oid: 1,
            name: "c",
            namespace: 2200,
            kind,
            relid: 2,
            index_oid: 3,
            columns: cols,
            check_sql: None,
            no_inherit: false,
        };
        let c = oids::PG_CONSTRAINT;
        let p = constraint_row(&spec(ConstraintKind::PrimaryKey, &[1, 2]));
        assert_eq!(get(&p, c, "contype"), &Datum::Char(b'p'));
        assert_eq!(get(&p, c, "connoinherit"), &Datum::Bool(true));
        assert_eq!(get(&p, c, "conkey"), &Datum::Int2Vector(vec![1, 2]));
        assert_eq!(get(&p, c, "conindid"), &Datum::Oid(3));
        let u = constraint_row(&spec(ConstraintKind::Unique, &[2]));
        assert_eq!(get(&u, c, "contype"), &Datum::Char(b'u'));
        let k = check_constraint_row(1, "k", 2200, 2, "a > 0", false);
        assert_eq!(get(&k, c, "connoinherit"), &Datum::Bool(false));
        assert!(get(&k, c, "conkey").is_null());
        assert_eq!(get(&k, c, "conindid"), &Datum::Oid(0));
    }

    #[test]
    fn depend_sequence_and_class_rows() {
        let d = depend_row(&NewDepend {
            dependent: ObjectAddress::attrdef(5),
            referenced: ObjectAddress::column(6, 2),
            deptype: DependType::Auto,
        });
        let p = oids::PG_DEPEND;
        assert_eq!(get(&d, p, "classid"), &Datum::Oid(2604));
        assert_eq!(get(&d, p, "refobjsubid"), &Datum::Int4(2));
        assert_eq!(get(&d, p, "deptype"), &Datum::Char(b'a'));
        let params = SequenceParams {
            type_oid: oid::INT4,
            start: 1,
            increment: 1,
            min: 1,
            max: 2_147_483_647,
            cache: 1,
            cycle: false,
            owned_by: None,
        };
        let s = sequence_row(9, &params);
        assert_eq!(
            get(&s, oids::PG_SEQUENCE, "seqmax"),
            &Datum::Int8(2_147_483_647)
        );
        assert_eq!(sequence_attribute_rows(9).len(), 9);
        // relkind drives relam and the frozen XIDs
        let c = oids::PG_CLASS;
        let mk = |kind| {
            class_row(&ClassSpec {
                kind,
                ..ClassSpec::table(1, "x", 2200, 10, 1)
            })
        };
        let (t, i, q) = (
            mk(RelKind::Table),
            mk(RelKind::Index),
            mk(RelKind::Sequence),
        );
        assert_eq!(get(&t, c, "relam"), &Datum::Oid(2));
        assert_eq!(get(&i, c, "relam"), &Datum::Oid(403));
        assert_eq!(get(&q, c, "relam"), &Datum::Oid(0));
        assert_eq!(get(&i, c, "relkind"), &Datum::Char(b'i'));
        assert_eq!(get(&q, c, "relfrozenxid"), &Datum::Xid(0));
        assert_eq!(get(&t, c, "relfrozenxid"), &Datum::Xid(3));
    }

    #[test]
    fn identity_is_written_to_attidentity() {
        let r = attribute_row(&AttributeSpec {
            relid: 1,
            name: "id",
            attnum: 1,
            ty: SqlType::INT4,
            not_null: true,
            has_default: false,
            catalog_column: false,
            identity: Some(IdentityKind::Always),
        })
        .unwrap();
        assert_eq!(
            get(&r, oids::PG_ATTRIBUTE, "attidentity"),
            &Datum::Char(b'a')
        );
    }

    #[test]
    fn language_and_generated_rows() {
        let p = InitParams {
            superuser: "postgres".into(),
        };
        let langs = initial_rows(oids::PG_LANGUAGE, &p);
        let names: Vec<_> = langs
            .iter()
            .map(|r| get(r, oids::PG_LANGUAGE, "lanname").clone())
            .collect();
        assert_eq!(names, [text("internal"), text("c"), text("sql")]);
        assert_eq!(
            get(&langs[2], oids::PG_LANGUAGE, "lanpltrusted"),
            &Datum::Bool(true)
        );
        // oid order, and every class belongs to an existing family
        let fam: Vec<Oid> = initial_rows(oids::PG_OPFAMILY, &p)
            .iter()
            .map(|r| oid_of(&r[0]))
            .collect();
        let classes = initial_rows(oids::PG_OPCLASS, &p);
        let mut sorted: Vec<Oid> = classes.iter().map(|r| oid_of(&r[0])).collect();
        let before = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(before, sorted);
        for r in &classes {
            assert!(fam.contains(&oid_of(get(r, oids::PG_OPCLASS, "opcfamily"))));
        }
        let amop = initial_rows(oids::PG_AMOP, &p);
        assert_eq!(oid_of(&amop[0][0]), 10_000);
        assert_eq!(
            get(&amop[0], oids::PG_AMOP, "amoppurpose"),
            &Datum::Char(b's')
        );
        let dangling = dangling_references(&p);
        assert!(dangling.is_empty(), "{dangling:?}");
    }

    fn oid_of(d: &Datum) -> Oid {
        match d {
            Datum::Oid(o) => *o,
            _ => 0,
        }
    }
}
