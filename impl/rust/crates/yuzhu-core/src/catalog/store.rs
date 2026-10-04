//! `CatalogStore`: reads and writes the system catalogs through the heap
//! API (`m2.md` §4.6, §6.8).
//!
//! Every access goes through [`TableStore`] with a snapshot, exactly like a
//! user table (`m2.md` §6.8.7): there is no path that bypasses MVCC, so DDL
//! is transactional without extra code. The pinned `TableDef`s of the 13
//! catalogs come from `schema::catalog_table_def`.

use std::collections::HashMap;
use std::sync::Arc;

use super::rows::{
    self, AttributeSpec, ClassSpec, InitParams, attrdef_row, attribute_row, check_constraint_row,
    class_row, column_index, system_attribute_rows,
};
use super::schema::{self, oids};
use super::{BoundExprSource, CheckDef, ColumnDef, RelKind, TableDef};
use crate::datadir::OidAllocator;
use crate::error::{Error, Result, sqlstate};
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::storage::{HeapTuple, RelHandle, TableStore, TmResult, WriteCtx};
use crate::txn::{CommandId, Snapshot, Xid};
use crate::types::{Datum, Oid, Row, SqlType, oid};

/// How many OIDs `get_new_oid` tries before it gives up (PostgreSQL warns
/// after a million; a counter that keeps colliding means a broken catalog).
const MAX_OID_ATTEMPTS: u32 = 1_000_000;

/// The snapshot that sees every version of every row: deleted rows that
/// still sit in the file, and the uncommitted rows of any transaction
/// (PostgreSQL's `SnapshotAny`). `get_new_oid` needs it (`m2.md` §6.9.2).
///
/// The convention is `xmin == xmax == Xid::INVALID`, which no real snapshot
/// has (a real snapshot's `xmax` is the next XID, at least 3). The heap's
/// visibility function must treat it as "visible" for every tuple.
pub fn snapshot_any() -> Snapshot {
    Snapshot {
        xmin: Xid::INVALID,
        xmax: Xid::INVALID,
        xip: Vec::new(),
        curcid: CommandId::MAX,
        own_xid: None,
    }
}

// ----- reading values out of rows ---------------------------------------------

fn bad_value(catalog: &str, column: &str, d: &Datum) -> Error {
    Error::internal(format!("{catalog}.{column} has an unexpected value {d:?}"))
}

fn as_oid(d: &Datum, catalog: &str, column: &str) -> Result<Oid> {
    match d {
        Datum::Oid(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_text<'a>(d: &'a Datum, catalog: &str, column: &str) -> Result<&'a str> {
    match d {
        Datum::Text(s) => Ok(s),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_bool(d: &Datum, catalog: &str, column: &str) -> Result<bool> {
    match d {
        Datum::Bool(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_i16(d: &Datum, catalog: &str, column: &str) -> Result<i16> {
    match d {
        Datum::Int2(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_i32(d: &Datum, catalog: &str, column: &str) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_char(d: &Datum, catalog: &str, column: &str) -> Result<u8> {
    match d {
        Datum::Char(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

/// Turns the result of a catalog `delete` into an error unless it worked.
/// With the single-writer lock a catalog row cannot be changed by anyone
/// else, so every other outcome is a bug or a damaged catalog.
fn expect_deleted(result: TmResult, what: &str) -> Result<()> {
    match result {
        TmResult::Ok => Ok(()),
        other => Err(Error::internal(format!(
            "could not delete a {what} row: {other:?}"
        ))),
    }
}

// ----- CatalogStore -------------------------------------------------------------

/// Access to the catalogs of one database.
#[derive(Debug)]
pub struct CatalogStore {
    db_oid: Oid,
    storage: Arc<dyn TableStore>,
    /// The pinned definitions of the per-database catalogs and their
    /// `RelHandle`s. The shared catalogs are read through
    /// [`SharedCatalogStore`] only.
    nailed: HashMap<Oid, (Arc<TableDef>, RelHandle)>,
}

impl CatalogStore {
    pub fn new(db_oid: Oid, storage: Arc<dyn TableStore>) -> Self {
        let nailed = schema::CATALOGS
            .iter()
            .filter(|c| !c.shared)
            .filter_map(|c| {
                let def = schema::catalog_table_def(c.oid, db_oid)?;
                let rel = RelHandle::from_table(&def);
                Some((c.oid, (def, rel)))
            })
            .collect();
        CatalogStore {
            db_oid,
            storage,
            nailed,
        }
    }

    pub fn db_oid(&self) -> Oid {
        self.db_oid
    }

    /// The pinned `TableDef` of a system catalog of this database; the
    /// shared catalogs too (their location does not depend on the database).
    pub fn nailed_def(&self, catalog_oid: Oid) -> Option<Arc<TableDef>> {
        match self.nailed.get(&catalog_oid) {
            Some((def, _)) => Some(Arc::clone(def)),
            None => schema::catalog_table_def(catalog_oid, self.db_oid),
        }
    }

    fn rel(&self, catalog_oid: Oid) -> Result<&RelHandle> {
        self.nailed
            .get(&catalog_oid)
            .map(|(_, rel)| rel)
            .ok_or_else(|| Error::internal(format!("catalog {catalog_oid} is not per-database")))
    }

    /// Visible rows of a catalog for which `keep` returns true.
    fn scan(
        &self,
        snap: &Snapshot,
        catalog_oid: Oid,
        mut keep: impl FnMut(&Row) -> Result<bool>,
    ) -> Result<Vec<HeapTuple>> {
        scan_rel(&*self.storage, self.rel(catalog_oid)?, snap, &mut keep)
    }

    /// `pg_class.oid` of the relation `name` in namespace `nsp`.
    pub fn lookup_relation(&self, snap: &Snapshot, nsp: Oid, name: &str) -> Result<Option<Oid>> {
        let (c, n) = (
            column_index(oids::PG_CLASS, "relnamespace"),
            column_index(oids::PG_CLASS, "relname"),
        );
        let found = self.scan(snap, oids::PG_CLASS, |r| {
            Ok(as_oid(&r[c], "pg_class", "relnamespace")? == nsp
                && as_text(&r[n], "pg_class", "relname")? == name)
        })?;
        found
            .first()
            .map(|t| as_oid(&t.row[0], "pg_class", "oid"))
            .transpose()
    }

    /// `pg_namespace.oid` of the namespace `name`.
    pub fn namespace_oid(&self, snap: &Snapshot, name: &str) -> Result<Option<Oid>> {
        let n = column_index(oids::PG_NAMESPACE, "nspname");
        let found = self.scan(snap, oids::PG_NAMESPACE, |r| {
            Ok(as_text(&r[n], "pg_namespace", "nspname")? == name)
        })?;
        found
            .first()
            .map(|t| as_oid(&t.row[0], "pg_namespace", "oid"))
            .transpose()
    }

    /// The name of the namespace with this OID.
    pub fn namespace_name(&self, snap: &Snapshot, nsp: Oid) -> Result<Option<String>> {
        let n = column_index(oids::PG_NAMESPACE, "nspname");
        let found = self.scan(snap, oids::PG_NAMESPACE, |r| {
            Ok(as_oid(&r[0], "pg_namespace", "oid")? == nsp)
        })?;
        found
            .first()
            .map(|t| as_text(&t.row[n], "pg_namespace", "nspname").map(str::to_owned))
            .transpose()
    }

    /// Builds the definition of a relation from `pg_class`, `pg_attribute`,
    /// `pg_attrdef` and `pg_constraint` as the snapshot sees them. The
    /// system catalogs give their pinned definition.
    pub fn load_table_def(&self, snap: &Snapshot, oid: Oid) -> Result<Option<TableDef>> {
        if let Some(def) = schema::catalog_table_def(oid, self.db_oid) {
            return Ok(Some((*def).clone()));
        }
        let class = self.scan(snap, oids::PG_CLASS, |r| {
            Ok(as_oid(&r[0], "pg_class", "oid")? == oid)
        })?;
        let Some(class) = class.first() else {
            return Ok(None);
        };
        let col = |name: &str| column_index(oids::PG_CLASS, name);
        let row = &class.row;
        let relkind = as_char(&row[col("relkind")], "pg_class", "relkind")?;
        if relkind != b'r' {
            return Err(Error::not_supported(format!(
                "relation kind '{}' is not supported yet",
                char::from(relkind)
            )));
        }
        let name = as_text(&row[col("relname")], "pg_class", "relname")?.to_owned();
        let namespace = as_oid(&row[col("relnamespace")], "pg_class", "relnamespace")?;
        let relfilenode = as_oid(&row[col("relfilenode")], "pg_class", "relfilenode")?;
        let tablespace = as_oid(&row[col("reltablespace")], "pg_class", "reltablespace")?;
        let natts = as_i16(&row[col("relnatts")], "pg_class", "relnatts")?;
        let nchecks = as_i16(&row[col("relchecks")], "pg_class", "relchecks")?;
        let schema_name = self.namespace_name(snap, namespace)?.ok_or_else(|| {
            Error::internal(format!("relation {oid} has no namespace {namespace}"))
        })?;

        let defaults = self.load_defaults(snap, oid)?;
        let columns = self.load_columns(snap, oid, natts, &defaults)?;
        let checks = self.load_checks(snap, oid, nchecks)?;
        Ok(Some(TableDef {
            oid,
            namespace,
            schema: schema_name,
            name,
            kind: RelKind::Table,
            locator: RelFileLocator {
                spc_oid: if tablespace == 0 {
                    DEFAULTTABLESPACE_OID
                } else {
                    tablespace
                },
                db_oid: self.db_oid,
                rel_number: RelFileNumber(relfilenode),
            },
            columns,
            checks,
        }))
    }

    /// `adnum` to expression text.
    fn load_defaults(&self, snap: &Snapshot, relid: Oid) -> Result<HashMap<i16, String>> {
        let (rel, num, bin) = (
            column_index(oids::PG_ATTRDEF, "adrelid"),
            column_index(oids::PG_ATTRDEF, "adnum"),
            column_index(oids::PG_ATTRDEF, "adbin"),
        );
        let found = self.scan(snap, oids::PG_ATTRDEF, |r| {
            Ok(as_oid(&r[rel], "pg_attrdef", "adrelid")? == relid)
        })?;
        found
            .iter()
            .map(|t| {
                Ok((
                    as_i16(&t.row[num], "pg_attrdef", "adnum")?,
                    as_text(&t.row[bin], "pg_attrdef", "adbin")?.to_owned(),
                ))
            })
            .collect()
    }

    fn load_columns(
        &self,
        snap: &Snapshot,
        relid: Oid,
        natts: i16,
        defaults: &HashMap<i16, String>,
    ) -> Result<Vec<ColumnDef>> {
        let col = |name: &str| column_index(oids::PG_ATTRIBUTE, name);
        let (rel, num) = (col("attrelid"), col("attnum"));
        let found = self.scan(snap, oids::PG_ATTRIBUTE, |r| {
            Ok(as_oid(&r[rel], "pg_attribute", "attrelid")? == relid
                && as_i16(&r[num], "pg_attribute", "attnum")? > 0)
        })?;
        let mut columns = Vec::with_capacity(found.len());
        for t in &found {
            let r = &t.row;
            if as_bool(&r[col("attisdropped")], "pg_attribute", "attisdropped")? {
                continue;
            }
            let attnum = as_i16(&r[num], "pg_attribute", "attnum")?;
            columns.push(ColumnDef {
                name: as_text(&r[col("attname")], "pg_attribute", "attname")?.to_owned(),
                attnum,
                ty: SqlType::new(
                    as_oid(&r[col("atttypid")], "pg_attribute", "atttypid")?,
                    as_i32(&r[col("atttypmod")], "pg_attribute", "atttypmod")?,
                ),
                not_null: as_bool(&r[col("attnotnull")], "pg_attribute", "attnotnull")?,
                default: defaults.get(&attnum).map(|sql| BoundExprSource {
                    expr_sql: sql.clone(),
                }),
            });
        }
        columns.sort_by_key(|c| c.attnum);
        if columns.len() != usize::try_from(natts).unwrap_or(usize::MAX) {
            return Err(Error::corrupted(format!(
                "relation {relid} has {} columns in pg_attribute but relnatts = {natts}",
                columns.len()
            )));
        }
        Ok(columns)
    }

    /// CHECK constraints in name order (the order PostgreSQL evaluates them).
    fn load_checks(&self, snap: &Snapshot, relid: Oid, nchecks: i16) -> Result<Vec<CheckDef>> {
        let col = |name: &str| column_index(oids::PG_CONSTRAINT, name);
        let (rel, kind) = (col("conrelid"), col("contype"));
        let found = self.scan(snap, oids::PG_CONSTRAINT, |r| {
            Ok(as_oid(&r[rel], "pg_constraint", "conrelid")? == relid
                && as_char(&r[kind], "pg_constraint", "contype")? == b'c')
        })?;
        let mut checks = found
            .iter()
            .map(|t| {
                Ok(CheckDef {
                    name: as_text(&t.row[col("conname")], "pg_constraint", "conname")?.to_owned(),
                    expr_sql: as_text(&t.row[col("conbin")], "pg_constraint", "conbin")?.to_owned(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        checks.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        if checks.len() != usize::try_from(nchecks).unwrap_or(usize::MAX) {
            return Err(Error::corrupted(format!(
                "relation {relid} has {} CHECK constraints in pg_constraint but relchecks = {nchecks}",
                checks.len()
            )));
        }
        Ok(checks)
    }

    /// Takes an OID from the counter and retries while a row with that OID
    /// exists in `catalog_oid` under a "see every version" scan
    /// (`GetNewOidWithIndex`, `m2.md` §6.9.2). The catalog must have `oid`
    /// as its first column.
    pub fn get_new_oid(&self, alloc: &OidAllocator, catalog_oid: Oid) -> Result<Oid> {
        let def = schema::catalog_def(catalog_oid)
            .filter(|d| !d.shared && d.columns[0].name == "oid")
            .ok_or_else(|| {
                Error::internal(format!(
                    "catalog {catalog_oid} has no per-database oid column"
                ))
            })?;
        let any = snapshot_any();
        for _ in 0..MAX_OID_ATTEMPTS {
            let candidate = alloc.next_raw()?;
            if candidate < oid::FIRST_NORMAL_OBJECT_ID {
                continue;
            }
            let taken = self.scan(&any, def.oid, |r| {
                Ok(as_oid(&r[0], def.name, "oid")? == candidate)
            })?;
            if taken.is_empty() {
                return Ok(candidate);
            }
        }
        Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!("could not find a free OID in {}", def.name),
        ))
    }

    /// `get_new_oid(pg_class)` plus a retry while the relation file exists.
    pub fn get_new_relation_oid(&self, alloc: &OidAllocator) -> Result<Oid> {
        for _ in 0..MAX_OID_ATTEMPTS {
            let candidate = self.get_new_oid(alloc, oids::PG_CLASS)?;
            let locator = RelFileLocator {
                spc_oid: DEFAULTTABLESPACE_OID,
                db_oid: self.db_oid,
                rel_number: RelFileNumber(candidate),
            };
            if !self.storage.storage_exists(locator)? {
                return Ok(candidate);
            }
        }
        Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "could not find a free relation OID",
        ))
    }

    /// The OIDs `create_table` needs besides the table's own: one per column
    /// with a DEFAULT (`pg_attrdef`) and one per CHECK (`pg_constraint`).
    pub fn allocate_child_oids(
        &self,
        alloc: &OidAllocator,
        columns: &[ColumnDef],
        checks: &[CheckDef],
    ) -> Result<(Vec<Oid>, Vec<Oid>)> {
        let attrdefs = columns
            .iter()
            .filter(|c| c.default.is_some())
            .map(|_| self.get_new_oid(alloc, oids::PG_ATTRDEF))
            .collect::<Result<Vec<_>>>()?;
        // The OIDs of one statement must differ from each other: the
        // counter never repeats a value, so distinct calls do not collide.
        let constraints = checks
            .iter()
            .map(|_| self.get_new_oid(alloc, oids::PG_CONSTRAINT))
            .collect::<Result<Vec<_>>>()?;
        Ok((attrdefs, constraints))
    }

    fn insert_row(&self, w: &WriteCtx, catalog_oid: Oid, row: &Row) -> Result<()> {
        self.storage.insert(self.rel(catalog_oid)?, w, row)?;
        Ok(())
    }

    /// Inserts rows into `pg_class`, `pg_attribute`, `pg_attrdef` and
    /// `pg_constraint`. Does not create the file.
    pub fn create_table(&self, w: &WriteCtx, _snap: &Snapshot, spec: &NewTable) -> Result<()> {
        let with_default = spec.columns.iter().filter(|c| c.default.is_some()).count();
        if spec.attrdef_oids.len() != with_default
            || spec.constraint_oids.len() != spec.checks.len()
        {
            return Err(Error::internal(
                "create_table: the OIDs of pg_attrdef / pg_constraint rows do not match the table",
            ));
        }
        let natts = i16::try_from(spec.columns.len()).map_err(|_| {
            Error::new(
                sqlstate::TOO_MANY_COLUMNS,
                "tables can have at most 1600 columns",
            )
        })?;
        for (i, c) in spec.columns.iter().enumerate() {
            if usize::try_from(c.attnum).ok() != Some(i + 1) {
                return Err(Error::internal(format!(
                    "create_table: column \"{}\" has attnum {} at position {}",
                    c.name,
                    c.attnum,
                    i + 1
                )));
            }
        }
        let nchecks = i16::try_from(spec.checks.len())
            .map_err(|_| Error::new(sqlstate::TOO_MANY_COLUMNS, "too many CHECK constraints"))?;

        self.insert_row(
            w,
            oids::PG_CLASS,
            &class_row(&ClassSpec {
                oid: spec.oid,
                name: &spec.name,
                namespace: spec.namespace,
                reltype: 0,
                owner: spec.owner,
                relfilenode: spec.oid,
                reltablespace: 0,
                is_shared: false,
                natts,
                nchecks,
                replident: 'd',
            }),
        )?;
        for c in &spec.columns {
            let row = attribute_row(&AttributeSpec {
                relid: spec.oid,
                name: &c.name,
                attnum: c.attnum,
                ty: c.ty,
                not_null: c.not_null,
                has_default: c.default.is_some(),
                catalog_column: false,
            })
            .ok_or_else(|| {
                Error::internal(format!(
                    "column \"{}\" has type {} which is not a built-in type",
                    c.name, c.ty.oid
                ))
            })?;
            self.insert_row(w, oids::PG_ATTRIBUTE, &row)?;
        }
        for row in system_attribute_rows(spec.oid, false) {
            self.insert_row(w, oids::PG_ATTRIBUTE, &row)?;
        }
        let defaults = spec
            .columns
            .iter()
            .filter_map(|c| c.default.as_ref().map(|d| (c.attnum, d.expr_sql.as_str())));
        for ((attnum, sql), attrdef_oid) in defaults.zip(&spec.attrdef_oids) {
            self.insert_row(
                w,
                oids::PG_ATTRDEF,
                &attrdef_row(*attrdef_oid, spec.oid, attnum, sql),
            )?;
        }
        for (check, constraint_oid) in spec.checks.iter().zip(&spec.constraint_oids) {
            self.insert_row(
                w,
                oids::PG_CONSTRAINT,
                &check_constraint_row(
                    *constraint_oid,
                    &check.name,
                    spec.namespace,
                    spec.oid,
                    &check.expr_sql,
                ),
            )?;
        }
        Ok(())
    }

    /// Deletes the rows `create_table` inserted: `pg_constraint`,
    /// `pg_attrdef`, `pg_attribute`, then `pg_class` (there is no
    /// `pg_depend` in M2, so the dependent rows are found by relation OID).
    pub fn drop_table(&self, w: &WriteCtx, snap: &Snapshot, def: &TableDef) -> Result<()> {
        if def.is_system_catalog() {
            return Err(Error::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{}\" is a system catalog", def.name),
            ));
        }
        let relid = def.oid;
        for (catalog, column) in [
            (oids::PG_CONSTRAINT, "conrelid"),
            (oids::PG_ATTRDEF, "adrelid"),
            (oids::PG_ATTRIBUTE, "attrelid"),
            (oids::PG_CLASS, "oid"),
        ] {
            let idx = column_index(catalog, column);
            let name = schema::catalog_def(catalog).map_or("catalog", |d| d.name);
            // Collect first: the deletions must not disturb the scan.
            let victims = self.scan(snap, catalog, |r| {
                Ok(as_oid(&r[idx], name, column)? == relid)
            })?;
            for t in victims {
                let result = self.storage.delete(self.rel(catalog)?, w, snap, t.tid)?;
                expect_deleted(result, name)?;
            }
        }
        Ok(())
    }

    /// initdb: creates the files of the per-database catalogs and inserts
    /// their initial rows (`m2.md` §6.8.2). The rows of the shared catalogs
    /// are written by [`SharedCatalogStore::bootstrap`].
    pub fn bootstrap(&self, w: &WriteCtx, params: &InitParams) -> Result<()> {
        for def in schema::CATALOGS.iter().filter(|d| !d.shared) {
            let rel = self.rel(def.oid)?;
            self.storage.create_storage(rel.locator)?;
            for row in rows::initial_rows(def.oid, params) {
                self.storage.insert(rel, w, &row)?;
            }
        }
        Ok(())
    }
}

/// Scans a relation and keeps the rows that `keep` accepts.
fn scan_rel(
    storage: &dyn TableStore,
    rel: &RelHandle,
    snap: &Snapshot,
    keep: &mut dyn FnMut(&Row) -> Result<bool>,
) -> Result<Vec<HeapTuple>> {
    let mut scan = storage.begin_scan(rel, snap)?;
    let mut out = Vec::new();
    while let Some(t) = storage.scan_next(&mut scan)? {
        if keep(&t.row)? {
            out.push(t);
        }
    }
    Ok(out)
}

/// What `CatalogStore::create_table` writes.
#[derive(Debug, Clone)]
pub struct NewTable {
    pub oid: Oid,
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
    /// `pg_attrdef` OIDs, one per column that has a DEFAULT, in column
    /// order (`CatalogStore::allocate_child_oids`).
    pub attrdef_oids: Vec<Oid>,
    /// `pg_constraint` OIDs, one per CHECK, in `checks` order.
    pub constraint_oids: Vec<Oid>,
}

/// The only path to the shared catalogs (`pg_database`, `pg_authid`).
#[derive(Debug)]
pub struct SharedCatalogStore {
    storage: Arc<dyn TableStore>,
    database: (Arc<TableDef>, RelHandle),
    authid: (Arc<TableDef>, RelHandle),
    tablespace: (Arc<TableDef>, RelHandle),
}

fn shared_rel(catalog_oid: Oid) -> (Arc<TableDef>, RelHandle) {
    // The three shared catalogs are defined in `schema.rs`; the database
    // OID is ignored for them.
    let def = schema::catalog_table_def(catalog_oid, 0)
        .unwrap_or_else(|| unreachable!("shared catalog {catalog_oid} is defined"));
    let rel = RelHandle::from_table(&def);
    (def, rel)
}

impl SharedCatalogStore {
    pub fn new(storage: Arc<dyn TableStore>) -> Self {
        SharedCatalogStore {
            storage,
            database: shared_rel(oids::PG_DATABASE),
            authid: shared_rel(oids::PG_AUTHID),
            tablespace: shared_rel(oids::PG_TABLESPACE),
        }
    }

    pub fn database_by_name(&self, snap: &Snapshot, name: &str) -> Result<Option<DatabaseRow>> {
        let col = |n: &str| column_index(oids::PG_DATABASE, n);
        let name_idx = col("datname");
        let found = scan_rel(&*self.storage, &self.database.1, snap, &mut |r| {
            Ok(as_text(&r[name_idx], "pg_database", "datname")? == name)
        })?;
        found
            .first()
            .map(|t| {
                let r = &t.row;
                Ok(DatabaseRow {
                    oid: as_oid(&r[0], "pg_database", "oid")?,
                    name: as_text(&r[name_idx], "pg_database", "datname")?.to_owned(),
                    allow_conn: as_bool(&r[col("datallowconn")], "pg_database", "datallowconn")?,
                    is_template: as_bool(&r[col("datistemplate")], "pg_database", "datistemplate")?,
                })
            })
            .transpose()
    }

    pub fn role_by_name(&self, snap: &Snapshot, name: &str) -> Result<Option<RoleRow>> {
        let name_idx = column_index(oids::PG_AUTHID, "rolname");
        let found = scan_rel(&*self.storage, &self.authid.1, snap, &mut |r| {
            Ok(as_text(&r[name_idx], "pg_authid", "rolname")? == name)
        })?;
        found.first().map(|t| role_row(&t.row)).transpose()
    }

    pub fn role_name(&self, snap: &Snapshot, oid: Oid) -> Result<Option<String>> {
        let name_idx = column_index(oids::PG_AUTHID, "rolname");
        let found = scan_rel(&*self.storage, &self.authid.1, snap, &mut |r| {
            Ok(as_oid(&r[0], "pg_authid", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| as_text(&t.row[name_idx], "pg_authid", "rolname").map(str::to_owned))
            .transpose()
    }

    /// The pinned definitions of the shared catalogs.
    pub fn defs(&self) -> [Arc<TableDef>; 3] {
        [
            Arc::clone(&self.database.0),
            Arc::clone(&self.authid.0),
            Arc::clone(&self.tablespace.0),
        ]
    }

    /// initdb: creates the files of the shared catalogs and inserts their
    /// initial rows.
    pub fn bootstrap(&self, w: &WriteCtx, params: &InitParams) -> Result<()> {
        for (oid, rel) in [
            (oids::PG_DATABASE, &self.database.1),
            (oids::PG_AUTHID, &self.authid.1),
            (oids::PG_TABLESPACE, &self.tablespace.1),
        ] {
            self.storage.create_storage(rel.locator)?;
            for row in rows::initial_rows(oid, params) {
                self.storage.insert(rel, w, &row)?;
            }
        }
        Ok(())
    }
}

fn role_row(r: &Row) -> Result<RoleRow> {
    let col = |n: &str| column_index(oids::PG_AUTHID, n);
    Ok(RoleRow {
        oid: as_oid(&r[0], "pg_authid", "oid")?,
        name: as_text(&r[col("rolname")], "pg_authid", "rolname")?.to_owned(),
        can_login: as_bool(&r[col("rolcanlogin")], "pg_authid", "rolcanlogin")?,
        superuser: as_bool(&r[col("rolsuper")], "pg_authid", "rolsuper")?,
    })
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

#[cfg(test)]
pub(crate) mod fake_store {
    //! An in-memory `TableStore` for the catalog unit tests: committed rows
    //! only, with deletes and the "see everything" snapshot.

    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::*;
    use crate::catalog::builtin;
    use crate::catalog::rows::datum_matches_type;
    use crate::storage::{HeapScan, UpdateOutcome};
    use crate::types::Tid;

    #[derive(Debug, Clone)]
    struct FakeTuple {
        tid: Tid,
        row: Row,
        xmin: Xid,
        xmax: Option<Xid>,
    }

    #[derive(Debug, Default)]
    struct Inner {
        files: HashSet<RelFileLocator>,
        tuples: HashMap<RelFileLocator, Vec<FakeTuple>>,
    }

    #[derive(Debug, Default)]
    pub(crate) struct FakeStore {
        inner: Mutex<Inner>,
    }

    impl FakeStore {
        pub(crate) fn new() -> Arc<FakeStore> {
            Arc::new(FakeStore::default())
        }

        /// Number of rows (including deleted ones) in a relation.
        pub(crate) fn physical_rows(&self, rel: RelFileLocator) -> usize {
            self.inner
                .lock()
                .unwrap()
                .tuples
                .get(&rel)
                .map_or(0, Vec::len)
        }
    }

    fn is_any(snap: &Snapshot) -> bool {
        snap.xmin == Xid::INVALID && snap.xmax == Xid::INVALID
    }

    impl TableStore for FakeStore {
        fn create_storage(&self, rel: RelFileLocator) -> Result<()> {
            let mut g = self.inner.lock().unwrap();
            if !g.files.insert(rel) {
                return Err(Error::internal(format!("{rel:?} already exists")));
            }
            Ok(())
        }

        fn storage_exists(&self, rel: RelFileLocator) -> Result<bool> {
            Ok(self.inner.lock().unwrap().files.contains(&rel))
        }

        fn unlink_storage(&self, rel: RelFileLocator) -> Result<()> {
            let mut g = self.inner.lock().unwrap();
            g.files.remove(&rel);
            g.tuples.remove(&rel);
            Ok(())
        }

        fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
            // The row must fit the descriptor the way HeapStore requires it.
            if row.len() != rel.desc.attrs.len() {
                return Err(Error::internal(format!(
                    "row has {} values for {} columns",
                    row.len(),
                    rel.desc.attrs.len()
                )));
            }
            for (d, a) in row.iter().zip(&rel.desc.attrs) {
                if !datum_matches_type(d, a.type_oid) {
                    return Err(Error::internal(format!(
                        "value {d:?} does not fit type {}",
                        a.type_oid
                    )));
                }
                if !d.is_null() && builtin::is_null_only_type(a.type_oid) {
                    return Err(Error::not_supported("NULL-only type holds a value"));
                }
            }
            let mut g = self.inner.lock().unwrap();
            if !g.files.contains(&rel.locator) {
                return Err(Error::internal(format!("no file for {:?}", rel.locator)));
            }
            let v = g.tuples.entry(rel.locator).or_default();
            let tid = Tid {
                block: 0,
                offset: u16::try_from(v.len() + 1).unwrap(),
            };
            v.push(FakeTuple {
                tid,
                row: row.to_vec(),
                xmin: w.xid,
                xmax: None,
            });
            Ok(tid)
        }

        fn delete(
            &self,
            rel: &RelHandle,
            w: &WriteCtx,
            _snap: &Snapshot,
            tid: Tid,
        ) -> Result<TmResult> {
            let mut g = self.inner.lock().unwrap();
            let t = g
                .tuples
                .get_mut(&rel.locator)
                .and_then(|v| v.iter_mut().find(|t| t.tid == tid));
            match t {
                Some(t) if t.xmax.is_none() => {
                    t.xmax = Some(w.xid);
                    Ok(TmResult::Ok)
                }
                Some(t) => Ok(TmResult::Deleted {
                    xmax: t.xmax.unwrap_or(Xid::INVALID),
                }),
                None => Ok(TmResult::Invisible),
            }
        }

        fn update(
            &self,
            _rel: &RelHandle,
            _w: &WriteCtx,
            _snap: &Snapshot,
            _tid: Tid,
            _new_row: &[Datum],
        ) -> Result<UpdateOutcome> {
            Err(Error::not_supported("fake store: update"))
        }

        fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
            let g = self.inner.lock().unwrap();
            let tuples = g
                .tuples
                .get(&rel.locator)
                .map(|v| {
                    v.iter()
                        .filter(|t| is_any(snap) || t.xmax.is_none())
                        .map(|t| HeapTuple {
                            tid: t.tid,
                            xmin: t.xmin,
                            xmax: t.xmax.unwrap_or(Xid::INVALID),
                            cmin: 0,
                            cmax: 0,
                            row: t.row.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(HeapScan::from_tuples(rel.clone(), snap.clone(), tuples))
        }

        fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
            Ok(scan.pop_buffered())
        }

        fn fetch(
            &self,
            _rel: &RelHandle,
            _snap: &Snapshot,
            _tid: Tid,
        ) -> Result<Option<HeapTuple>> {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake_store::FakeStore;
    use super::*;
    use crate::control::{ControlData, ControlFileHandle};
    use crate::storage::vfs::{SimVfs, Vfs};

    const DB: Oid = 5;

    fn snap() -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(1000),
            xip: vec![],
            curcid: 100,
            own_xid: None,
        }
    }

    fn wctx() -> WriteCtx {
        WriteCtx {
            xid: Xid::BOOTSTRAP,
            cid: 0,
        }
    }

    fn params() -> InitParams {
        InitParams {
            superuser: "postgres".into(),
        }
    }

    struct Env {
        fake: Arc<FakeStore>,
        store: CatalogStore,
        shared: SharedCatalogStore,
        oids: OidAllocator,
    }

    fn env() -> Env {
        let fake = FakeStore::new();
        let store = CatalogStore::new(DB, fake.clone());
        let shared = SharedCatalogStore::new(fake.clone());
        shared.bootstrap(&wctx(), &params()).unwrap();
        store.bootstrap(&wctx(), &params()).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let control = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(7, 9, 131_072)).unwrap(),
        );
        Env {
            fake,
            store,
            shared,
            oids: OidAllocator::new(control),
        }
    }

    fn col(
        name: &str,
        attnum: i16,
        ty: SqlType,
        not_null: bool,
        default: Option<&str>,
    ) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: default.map(|s| BoundExprSource { expr_sql: s.into() }),
        }
    }

    fn sample_columns() -> Vec<ColumnDef> {
        vec![
            col("a", 1, SqlType::INT4, true, Some("1")),
            col("b", 2, SqlType::varchar(10), false, Some("'x'")),
            col("c", 3, SqlType::TEXT, false, None),
        ]
    }

    fn sample_checks() -> Vec<CheckDef> {
        vec![
            CheckDef {
                name: "t_c_check".into(),
                expr_sql: "c <> ''".into(),
            },
            CheckDef {
                name: "t_a_check".into(),
                expr_sql: "a > 0".into(),
            },
        ]
    }

    /// Allocates the OIDs and writes `t` (with the file), as `Session` will.
    fn create_t(e: &Env, name: &str) -> NewTable {
        let table_oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        let columns = sample_columns();
        let checks = sample_checks();
        let (attrdef_oids, constraint_oids) = e
            .store
            .allocate_child_oids(&e.oids, &columns, &checks)
            .unwrap();
        let spec = NewTable {
            oid: table_oid,
            namespace: oids::NAMESPACE_PUBLIC,
            name: name.into(),
            owner: oids::BOOTSTRAP_SUPERUSER,
            columns,
            checks,
            attrdef_oids,
            constraint_oids,
        };
        let def = TableDef {
            oid: spec.oid,
            namespace: 2200,
            schema: "public".into(),
            name: name.into(),
            kind: RelKind::Table,
            locator: RelFileLocator {
                spc_oid: DEFAULTTABLESPACE_OID,
                db_oid: DB,
                rel_number: RelFileNumber(spec.oid),
            },
            columns: vec![],
            checks: vec![],
        };
        e.fake.create_storage(def.locator).unwrap();
        e.store.create_table(&wctx(), &snap(), &spec).unwrap();
        spec
    }

    #[test]
    fn bootstrap_fills_the_catalogs() {
        let e = env();
        let any = snapshot_any();
        let n = |catalog: Oid| {
            let rel = e.store.rel(catalog).unwrap();
            e.fake.physical_rows(rel.locator)
        };
        assert_eq!(n(oids::PG_CLASS), 13);
        assert_eq!(n(oids::PG_NAMESPACE), 3);
        assert_eq!(n(oids::PG_TYPE), crate::catalog::builtin::TYPES.len());
        assert_eq!(n(oids::PG_ATTRDEF), 0);
        assert_eq!(e.store.namespace_oid(&any, "public").unwrap(), Some(2200));
        assert_eq!(
            e.store.namespace_oid(&snap(), "pg_catalog").unwrap(),
            Some(11)
        );
        assert_eq!(e.store.namespace_oid(&snap(), "nope").unwrap(), None);
        assert_eq!(
            e.store.namespace_name(&snap(), 2200).unwrap().as_deref(),
            Some("public")
        );
        assert_eq!(
            e.store.lookup_relation(&snap(), 11, "pg_class").unwrap(),
            Some(1259)
        );
        assert_eq!(
            e.store.lookup_relation(&snap(), 2200, "pg_class").unwrap(),
            None
        );
    }

    #[test]
    fn shared_catalogs_are_read_through_the_shared_store() {
        let e = env();
        let s = snap();
        let db = e.shared.database_by_name(&s, "postgres").unwrap().unwrap();
        assert_eq!(
            db,
            DatabaseRow {
                oid: 5,
                name: "postgres".into(),
                allow_conn: true,
                is_template: false
            }
        );
        let t0 = e.shared.database_by_name(&s, "template0").unwrap().unwrap();
        assert!(!t0.allow_conn && t0.is_template);
        assert!(e.shared.database_by_name(&s, "nope").unwrap().is_none());
        let role = e.shared.role_by_name(&s, "postgres").unwrap().unwrap();
        assert_eq!(
            role,
            RoleRow {
                oid: 10,
                name: "postgres".into(),
                can_login: true,
                superuser: true
            }
        );
        let owner = e
            .shared
            .role_by_name(&s, "pg_database_owner")
            .unwrap()
            .unwrap();
        assert!(!owner.can_login && !owner.superuser);
        assert_eq!(
            e.shared.role_name(&s, 6171).unwrap().as_deref(),
            Some("pg_database_owner")
        );
        assert_eq!(e.shared.role_name(&s, 99_999).unwrap(), None);
        assert_eq!(e.shared.defs().len(), 3);
        // The shared catalog files do not depend on the database.
        let other = CatalogStore::new(1, e.fake.clone());
        assert_eq!(
            other.nailed_def(oids::PG_DATABASE).unwrap().locator,
            e.shared.defs()[0].locator
        );
    }

    #[test]
    fn create_then_load_round_trips() {
        let e = env();
        let spec = create_t(&e, "t");
        let def = e.store.load_table_def(&snap(), spec.oid).unwrap().unwrap();
        assert_eq!(def.name, "t");
        assert_eq!(def.schema, "public");
        assert_eq!(def.namespace, 2200);
        assert_eq!(def.kind, RelKind::Table);
        assert_eq!(def.locator.db_oid, DB);
        assert_eq!(def.locator.spc_oid, 1663);
        assert_eq!(def.locator.rel_number, RelFileNumber(spec.oid));
        assert_eq!(def.columns, spec.columns);
        // CHECKs come back in name order.
        let names: Vec<_> = def.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["t_a_check", "t_c_check"]);
        assert_eq!(def.checks[0].expr_sql, "a > 0");
        assert!(!def.is_system_catalog());
        assert_eq!(
            e.store.lookup_relation(&snap(), 2200, "t").unwrap(),
            Some(spec.oid)
        );
        assert!(e.store.load_table_def(&snap(), 99_999).unwrap().is_none());
    }

    #[test]
    fn create_writes_the_rows_of_the_spec() {
        let e = env();
        let spec = create_t(&e, "t");
        let s = snap();
        let class = e
            .store
            .scan(&s, oids::PG_CLASS, |r| Ok(r[0] == Datum::Oid(spec.oid)))
            .unwrap();
        assert_eq!(class.len(), 1);
        let c = |n: &str| column_index(oids::PG_CLASS, n);
        assert_eq!(class[0].row[c("relnatts")], Datum::Int2(3));
        assert_eq!(class[0].row[c("relchecks")], Datum::Int2(2));
        assert_eq!(class[0].row[c("relfilenode")], Datum::Oid(spec.oid));
        assert_eq!(class[0].row[c("relowner")], Datum::Oid(10));
        assert_eq!(class[0].row[c("relreplident")], Datum::Char(b'd'));
        let attrs = e
            .store
            .scan(&s, oids::PG_ATTRIBUTE, |r| Ok(r[0] == Datum::Oid(spec.oid)))
            .unwrap();
        assert_eq!(attrs.len(), 3 + 6);
        let attnum = column_index(oids::PG_ATTRIBUTE, "attnum");
        let mut nums: Vec<_> = attrs
            .iter()
            .map(|t| as_i16(&t.row[attnum], "", "").unwrap())
            .collect();
        nums.sort_unstable();
        assert_eq!(nums, [-6, -5, -4, -3, -2, -1, 1, 2, 3]);
        let defs = e
            .store
            .scan(&s, oids::PG_ATTRDEF, |r| Ok(r[1] == Datum::Oid(spec.oid)))
            .unwrap();
        assert_eq!(defs.len(), 2);
        let cons = e
            .store
            .scan(&s, oids::PG_CONSTRAINT, |r| {
                Ok(r[column_index(oids::PG_CONSTRAINT, "conrelid")] == Datum::Oid(spec.oid))
            })
            .unwrap();
        assert_eq!(cons.len(), 2);
    }

    #[test]
    fn create_rejects_inconsistent_specs() {
        let e = env();
        let mut spec = create_t(&e, "t1");
        spec.oid += 100_000;
        spec.attrdef_oids.pop();
        assert_eq!(
            e.store
                .create_table(&wctx(), &snap(), &spec)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        let mut spec = create_t(&e, "t2");
        spec.oid += 200_000;
        spec.columns[1].attnum = 5;
        assert_eq!(
            e.store
                .create_table(&wctx(), &snap(), &spec)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        let mut spec = create_t(&e, "t3");
        spec.oid += 300_000;
        spec.columns[0].ty = SqlType::of(99_999);
        assert!(e.store.create_table(&wctx(), &snap(), &spec).is_err());
    }

    #[test]
    fn drop_removes_every_row() {
        let e = env();
        let spec = create_t(&e, "t");
        let keep = create_t(&e, "keep");
        let def = e.store.load_table_def(&snap(), spec.oid).unwrap().unwrap();
        e.store.drop_table(&wctx(), &snap(), &def).unwrap();
        let s = snap();
        assert!(e.store.load_table_def(&s, spec.oid).unwrap().is_none());
        assert_eq!(e.store.lookup_relation(&s, 2200, "t").unwrap(), None);
        for (catalog, column) in [
            (oids::PG_CLASS, "oid"),
            (oids::PG_ATTRIBUTE, "attrelid"),
            (oids::PG_ATTRDEF, "adrelid"),
            (oids::PG_CONSTRAINT, "conrelid"),
        ] {
            let idx = column_index(catalog, column);
            let left = e
                .store
                .scan(&s, catalog, |r| Ok(r[idx] == Datum::Oid(spec.oid)))
                .unwrap();
            assert!(left.is_empty(), "rows left in catalog {catalog}");
        }
        // The other table is untouched.
        let other = e.store.load_table_def(&s, keep.oid).unwrap().unwrap();
        assert_eq!(other.columns.len(), 3);
        assert_eq!(other.checks.len(), 2);
        // The deleted rows are still in the file for a "see everything" scan.
        let any = e
            .store
            .scan(&snapshot_any(), oids::PG_CLASS, |r| {
                Ok(r[0] == Datum::Oid(spec.oid))
            })
            .unwrap();
        assert_eq!(any.len(), 1);
    }

    #[test]
    fn dropping_a_system_catalog_is_refused() {
        let e = env();
        let def = e
            .store
            .load_table_def(&snap(), oids::PG_CLASS)
            .unwrap()
            .unwrap();
        let err = e.store.drop_table(&wctx(), &snap(), &def).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(
            err.message,
            "permission denied: \"pg_class\" is a system catalog"
        );
    }

    #[test]
    fn system_catalogs_load_as_pinned_definitions() {
        let e = env();
        let def = e
            .store
            .load_table_def(&snap(), oids::PG_TYPE)
            .unwrap()
            .unwrap();
        assert_eq!(def.name, "pg_type");
        assert_eq!(def.columns.len(), 32);
        assert_eq!(def.locator.db_oid, DB);
        assert!(def.is_system_catalog());
    }

    #[test]
    fn new_oids_avoid_deleted_and_existing_rows() {
        let e = env();
        let a = create_t(&e, "a");
        let def = e.store.load_table_def(&snap(), a.oid).unwrap().unwrap();
        e.store.drop_table(&wctx(), &snap(), &def).unwrap();
        // Reset the counter to the dropped table's OID: a normal snapshot
        // would call the OID free, the see-everything scan does not.
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(2));
        let mut data = ControlData::initial(7, 9, 131_072);
        data.next_oid = a.oid;
        let control = Arc::new(ControlFileHandle::create(&vfs, &data).unwrap());
        let alloc = OidAllocator::new(control);
        let fresh = e.store.get_new_oid(&alloc, oids::PG_CLASS).unwrap();
        assert_eq!(fresh, a.oid + 1);
        assert_ne!(fresh, a.oid);
        let relation = e.store.get_new_relation_oid(&alloc).unwrap();
        assert!(relation > fresh);
        // The OID of a pg_attrdef row is checked against pg_attrdef only.
        let mut data = ControlData::initial(7, 9, 131_072);
        data.next_oid = a.attrdef_oids[0];
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(3));
        let alloc2 = OidAllocator::new(Arc::new(ControlFileHandle::create(&vfs, &data).unwrap()));
        let got = e.store.get_new_oid(&alloc2, oids::PG_ATTRDEF).unwrap();
        assert_ne!(got, a.attrdef_oids[0]);
        assert_ne!(got, a.attrdef_oids[1]);
    }

    #[test]
    fn relation_oids_skip_existing_files() {
        let e = env();
        // Occupy the file for the next OID without a pg_class row.
        let next = e.oids.next_raw().unwrap() + 1;
        e.fake
            .create_storage(RelFileLocator {
                spc_oid: DEFAULTTABLESPACE_OID,
                db_oid: DB,
                rel_number: RelFileNumber(next),
            })
            .unwrap();
        let got = e.store.get_new_relation_oid(&e.oids).unwrap();
        assert_ne!(got, next);
        assert!(got > next);
    }

    #[test]
    fn get_new_oid_needs_an_oid_column() {
        let e = env();
        assert!(e.store.get_new_oid(&e.oids, oids::PG_ATTRIBUTE).is_err());
        assert!(e.store.get_new_oid(&e.oids, oids::PG_DATABASE).is_err());
        assert!(e.store.get_new_oid(&e.oids, 42).is_err());
    }

    #[test]
    fn snapshot_any_is_distinguishable() {
        let any = snapshot_any();
        assert_eq!((any.xmin, any.xmax), (Xid::INVALID, Xid::INVALID));
        assert_ne!(snap().xmax, any.xmax);
    }
}
