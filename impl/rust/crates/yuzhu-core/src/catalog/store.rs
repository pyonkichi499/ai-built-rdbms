//! `CatalogStore`: reads and writes the system catalogs through the heap
//! API (`m2.md` §4.6, §6.8; `m4/07-catalog-ddl.md` §4.3、§4.5、§6.3).
//!
//! Every access goes through [`TableStore`] with a snapshot, exactly like a
//! user table (`m2.md` §6.8.7): there is no path that bypasses MVCC, so DDL
//! is transactional without extra code. The pinned `TableDef`s of the 22
//! catalogs come from `schema::catalog_table_def`.
//!
//! 1 コマンドで同じカタログ行を 2 度更新しない（D07-3）: 作る行は最初から最終形で書き、更新が要るのは
//! **前のコマンド**で作られた行だけ。`update_row_where` は見えない行（同じコマンドで挿入した行）を内部エラーにする。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::depend::{
    DependRow, DependType, DropBehavior, DropKind, DropPlan, NewDepend, ObjectAddress, classes,
    plan_drop,
};
use super::rows::{
    self, AttributeSpec, ClassSpec, ConstraintRowSpec, IndexRowSpec, InitParams, attrdef_row,
    attribute_row, class_row, column_index, constraint_row, depend_row, index_attribute_rows,
    index_row, sequence_attribute_rows, sequence_row, system_attribute_rows,
};
use super::schema::{self, oids};
use super::{
    BoundExprSource, CheckDef, ColumnDef, ConstraintDef, ConstraintKind, IdentityKind, IndexColumn,
    IndexConstraintRef, IndexDef, RelKind, SequenceParams, TableDef, builtin, opclass,
};
use crate::datadir::OidAllocator;
use crate::error::{Error, Result, sqlstate};
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::storage::{BuildStats, HeapTuple, RelHandle, TableStore, TmResult, WriteCtx};
use crate::txn::{CommandId, Snapshot, Xid};
use crate::types::{Datum, Oid, Row, SqlType, oid};

/// How many OIDs `get_new_oid` tries before it gives up (PostgreSQL warns
/// after a million; a counter that keeps colliding means a broken catalog).
const MAX_OID_ATTEMPTS: u32 = 1_000_000;

/// 制約が所有する空の索引の `relpages` / `reltuples`（メタページとルート。PostgreSQL は `1` / `0`）。
pub const EMPTY_INDEX_STATS: BuildStats = BuildStats {
    tuples: 0,
    pages: 2,
    levels: 0,
};

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

fn as_i64(d: &Datum, catalog: &str, column: &str) -> Result<i64> {
    match d {
        Datum::Int8(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_char(d: &Datum, catalog: &str, column: &str) -> Result<u8> {
    match d {
        Datum::Char(v) => Ok(*v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_int2vector<'a>(d: &'a Datum, catalog: &str, column: &str) -> Result<&'a [i16]> {
    match d {
        Datum::Int2Vector(v) => Ok(v),
        other => Err(bad_value(catalog, column, other)),
    }
}

fn as_oidvector<'a>(d: &'a Datum, catalog: &str, column: &str) -> Result<&'a [Oid]> {
    match d {
        Datum::OidVector(v) => Ok(v),
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

/// `pg_depend` の 1 行（列の名前で引くためのインデックスをまとめて持つ）。
struct DependCols {
    classid: usize,
    objid: usize,
    objsubid: usize,
    refclassid: usize,
    refobjid: usize,
    refobjsubid: usize,
    deptype: usize,
}

impl DependCols {
    fn new() -> Self {
        let c = |n: &str| column_index(oids::PG_DEPEND, n);
        DependCols {
            classid: c("classid"),
            objid: c("objid"),
            objsubid: c("objsubid"),
            refclassid: c("refclassid"),
            refobjid: c("refobjid"),
            refobjsubid: c("refobjsubid"),
            deptype: c("deptype"),
        }
    }

    fn dependent(&self, r: &Row) -> Result<ObjectAddress> {
        Ok(ObjectAddress {
            class_id: as_oid(&r[self.classid], "pg_depend", "classid")?,
            obj_id: as_oid(&r[self.objid], "pg_depend", "objid")?,
            obj_sub: as_i32(&r[self.objsubid], "pg_depend", "objsubid")?,
        })
    }

    fn referenced(&self, r: &Row) -> Result<ObjectAddress> {
        Ok(ObjectAddress {
            class_id: as_oid(&r[self.refclassid], "pg_depend", "refclassid")?,
            obj_id: as_oid(&r[self.refobjid], "pg_depend", "refobjid")?,
            obj_sub: as_i32(&r[self.refobjsubid], "pg_depend", "refobjsubid")?,
        })
    }

    fn deptype(&self, r: &Row) -> Result<DependType> {
        let c = as_char(&r[self.deptype], "pg_depend", "deptype")?;
        DependType::from_code(char::from(c)).ok_or_else(|| {
            Error::corrupted(format!(
                "pg_depend has an unknown deptype {}",
                char::from(c)
            ))
        })
    }

    fn row(&self, r: &Row) -> Result<DependRow> {
        Ok(DependRow {
            dependent: self.dependent(r)?,
            referenced: self.referenced(r)?,
            deptype: self.deptype(r)?,
        })
    }
}

// ----- types ----------------------------------------------------------------------

/// `pg_class` の 1 行の主な列（`kind` は `relkind` から）。
#[derive(Clone, Debug)]
pub struct RelationRow {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub kind: RelKind,
    pub owner: Oid,
    pub locator: RelFileLocator,
    pub natts: i16,
    pub has_index: bool,
}

/// 表の部品の OID（DROP の安全網と整合検査が使う）。
#[derive(Clone, Debug, Default)]
pub struct TableParts {
    /// `pg_constraint.conrelid` がこの表の制約。
    pub constraints: Vec<Oid>,
    /// `pg_attrdef.adrelid` がこの表の既定値。
    pub attrdefs: Vec<Oid>,
    /// `pg_index.indrelid` がこの表の索引（`pg_class` の OID）。
    pub indexes: Vec<Oid>,
}

/// `pg_class` の更新（`None` の項目は触らない）。
#[derive(Default, Clone, Debug)]
pub struct ClassPatch {
    pub relfilenode: Option<Oid>,
    pub relhasindex: Option<bool>,
    pub relpages: Option<i32>,
    pub reltuples: Option<f32>,
    pub relowner: Option<Oid>,
    pub relchecks: Option<i16>,
}

/// `pg_attribute` の更新。
#[derive(Default, Clone, Debug)]
pub struct AttributePatch {
    pub not_null: Option<bool>,
    pub has_default: Option<bool>,
}

/// What `CatalogStore::create_table` writes.
#[derive(Debug, Clone)]
pub struct NewTable {
    pub oid: Oid,
    pub namespace: Oid,
    pub name: String,
    pub owner: Oid,
    /// `identity`（`attidentity`）を含む。
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
    /// `pg_attrdef` OIDs, one per column that has a DEFAULT, in column
    /// order (`CatalogStore::allocate_table_oids`).
    pub attrdef_oids: Vec<Oid>,
    /// `pg_constraint` OIDs, one per CHECK, in `checks` order.
    pub constraint_oids: Vec<Oid>,
    /// PRIMARY KEY / UNIQUE 制約が所有する索引（PRIMARY KEY が先）。空の表なので `init_index` 済みで
    /// `build` は不要。
    pub indexes: Vec<NewIndex>,
    /// 呼び出し側が組み立てる追加の依存（既定値 → シーケンスの `n`、08 のシーケンス → 列の `a` / `i`）。
    pub extra_depends: Vec<NewDepend>,
}

impl NewTable {
    /// 索引・追加の依存のない表。
    pub fn plain(
        oid: Oid,
        namespace: Oid,
        name: impl Into<String>,
        owner: Oid,
        columns: Vec<ColumnDef>,
    ) -> NewTable {
        NewTable {
            oid,
            namespace,
            name: name.into(),
            owner,
            columns,
            checks: Vec::new(),
            attrdef_oids: Vec::new(),
            constraint_oids: Vec::new(),
            indexes: Vec::new(),
            extra_depends: Vec::new(),
        }
    }
}

/// 索引の列 1 つ。
#[derive(Clone, Debug)]
pub struct NewIndexColumn {
    /// 索引の `attname`（`ChooseIndexColumnNames` の結果）。
    pub name: String,
    pub column: IndexColumn,
    /// 表の列の型（`atttypid` / `atttypmod` / `attcollation` の元）。
    pub ty: SqlType,
}

/// 索引が所有される制約（`pg_constraint` の p / u の行の元）。
#[derive(Clone, Debug)]
pub struct NewConstraint {
    pub oid: Oid,
    pub name: String,
}

/// 索引 1 つぶんのカタログの行の元。
#[derive(Clone, Debug)]
pub struct NewIndex {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub table_oid: Oid,
    /// `relfilenode`（M4 は常に `oid` と同じ）。
    pub relfilenode: Oid,
    pub columns: Vec<NewIndexColumn>,
    pub unique: bool,
    pub primary: bool,
    /// `Some` なら `pg_constraint` の行（`contype` p / u）と、制約 → 列の `a`、索引 → 制約の `i` を書く。
    /// `None` なら索引 → 列の `a`。
    pub constraint: Option<NewConstraint>,
    /// `relpages` / `reltuples`（`EMPTY_INDEX_STATS` または `build` の結果）。
    pub stats: BuildStats,
}

/// シーケンス 1 つぶんのカタログの行の元（08 の `ddl/sequence.rs` が使う）。
#[derive(Clone, Debug)]
pub struct NewSequence {
    pub oid: Oid,
    pub name: String,
    pub namespace: Oid,
    pub owner: Oid,
    pub params: SequenceParams,
    /// SERIAL → `a`、IDENTITY → `i`。`params.owned_by` が `Some` のときだけ依存を書く。
    pub owned_by_deptype: Option<DependType>,
}

/// `CREATE TABLE` が先に決める OID の要求（`m4/07-catalog-ddl.md` §5.1 の手順 4）。
#[derive(Clone, Copy, Debug, Default)]
pub struct TableOidRequest {
    pub n_attrdefs: usize,
    pub n_checks: usize,
    pub n_index_constraints: usize,
}

#[derive(Clone, Debug)]
pub struct TableOids {
    pub table: Oid,
    pub attrdefs: Vec<Oid>,
    pub checks: Vec<Oid>,
    /// 索引制約ごと（索引、制約）の順に取った OID。
    pub index_constraints: Vec<(Oid, Oid)>,
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
    pub(crate) fn scan(
        &self,
        snap: &Snapshot,
        catalog_oid: Oid,
        mut keep: impl FnMut(&Row) -> Result<bool>,
    ) -> Result<Vec<HeapTuple>> {
        scan_rel(&*self.storage, self.rel(catalog_oid)?, snap, &mut keep)
    }

    fn catalog_name(catalog_oid: Oid) -> &'static str {
        schema::catalog_def(catalog_oid).map_or("catalog", |d| d.name)
    }

    // ----- reading: pg_class ------------------------------------------------------

    fn relation_row_of(&self, row: &Row) -> Result<RelationRow> {
        let col = |name: &str| column_index(oids::PG_CLASS, name);
        let oid = as_oid(&row[0], "pg_class", "oid")?;
        let relkind = as_char(&row[col("relkind")], "pg_class", "relkind")?;
        let kind = RelKind::from_code(char::from(relkind)).ok_or_else(|| {
            Error::corrupted(format!(
                "relation {oid} has the unsupported relkind '{}'",
                char::from(relkind)
            ))
        })?;
        let tablespace = as_oid(&row[col("reltablespace")], "pg_class", "reltablespace")?;
        let relfilenode = as_oid(&row[col("relfilenode")], "pg_class", "relfilenode")?;
        // Mapped catalogs have relfilenode 0: their file is the pinned one.
        let locator = match schema::catalog_def(oid) {
            Some(def) => def.locator(self.db_oid),
            None => RelFileLocator {
                spc_oid: if tablespace == 0 {
                    DEFAULTTABLESPACE_OID
                } else {
                    tablespace
                },
                db_oid: self.db_oid,
                rel_number: RelFileNumber(relfilenode),
            },
        };
        Ok(RelationRow {
            oid,
            name: as_text(&row[col("relname")], "pg_class", "relname")?.to_owned(),
            namespace: as_oid(&row[col("relnamespace")], "pg_class", "relnamespace")?,
            kind,
            owner: as_oid(&row[col("relowner")], "pg_class", "relowner")?,
            locator,
            natts: as_i16(&row[col("relnatts")], "pg_class", "relnatts")?,
            has_index: as_bool(&row[col("relhasindex")], "pg_class", "relhasindex")?,
        })
    }

    /// The relation `name` in namespace `nsp` (table, index or sequence).
    /// A `relkind` other than `r` / `i` / `S` is `Error::corrupted`.
    pub fn lookup_relation_row(
        &self,
        snap: &Snapshot,
        nsp: Oid,
        name: &str,
    ) -> Result<Option<RelationRow>> {
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
            .map(|t| self.relation_row_of(&t.row))
            .transpose()
    }

    /// The `pg_class` row with this OID.
    pub fn relation_row(&self, snap: &Snapshot, oid: Oid) -> Result<Option<RelationRow>> {
        let found = self.scan(snap, oids::PG_CLASS, |r| {
            Ok(as_oid(&r[0], "pg_class", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| self.relation_row_of(&t.row))
            .transpose()
    }

    /// `pg_class.oid` of the relation `name` in namespace `nsp`.
    pub fn lookup_relation(&self, snap: &Snapshot, nsp: Oid, name: &str) -> Result<Option<Oid>> {
        Ok(self.lookup_relation_row(snap, nsp, name)?.map(|r| r.oid))
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

    // ----- reading: pg_index, pg_constraint, pg_attrdef, pg_attribute --------------

    /// `pg_index.indrelid`。索引でなければ `None`（`pg_class` の `relkind` は見ない）。
    pub fn index_owner(&self, snap: &Snapshot, index_oid: Oid) -> Result<Option<Oid>> {
        let (id, rel) = (
            column_index(oids::PG_INDEX, "indexrelid"),
            column_index(oids::PG_INDEX, "indrelid"),
        );
        let found = self.scan(snap, oids::PG_INDEX, |r| {
            Ok(as_oid(&r[id], "pg_index", "indexrelid")? == index_oid)
        })?;
        found
            .first()
            .map(|t| as_oid(&t.row[rel], "pg_index", "indrelid"))
            .transpose()
    }

    fn constraint_def_of(row: &Row) -> Result<ConstraintDef> {
        let col = |name: &str| column_index(oids::PG_CONSTRAINT, name);
        let oid = as_oid(&row[0], "pg_constraint", "oid")?;
        let contype = as_char(&row[col("contype")], "pg_constraint", "contype")?;
        let kind = match contype {
            b'c' => ConstraintKind::Check,
            b'p' => ConstraintKind::PrimaryKey,
            b'u' => ConstraintKind::Unique,
            other => {
                return Err(Error::corrupted(format!(
                    "constraint {oid} has the unsupported contype '{}'",
                    char::from(other)
                )));
            }
        };
        let indid = as_oid(&row[col("conindid")], "pg_constraint", "conindid")?;
        let columns = match &row[col("conkey")] {
            Datum::Null => Vec::new(),
            d => as_int2vector(d, "pg_constraint", "conkey")?.to_vec(),
        };
        let check_sql = match &row[col("conbin")] {
            Datum::Null => None,
            d => Some(as_text(d, "pg_constraint", "conbin")?.to_owned()),
        };
        Ok(ConstraintDef {
            oid,
            name: as_text(&row[col("conname")], "pg_constraint", "conname")?.to_owned(),
            namespace: as_oid(&row[col("connamespace")], "pg_constraint", "connamespace")?,
            kind,
            table_oid: as_oid(&row[col("conrelid")], "pg_constraint", "conrelid")?,
            index_oid: (indid != 0).then_some(indid),
            columns,
            check_sql,
            no_inherit: matches!(&row[col("connoinherit")], Datum::Bool(true)),
        })
    }

    /// One `pg_constraint` row (c / p / u).
    pub fn constraint_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<ConstraintDef>> {
        let found = self.scan(snap, oids::PG_CONSTRAINT, |r| {
            Ok(as_oid(&r[0], "pg_constraint", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| Self::constraint_def_of(&t.row))
            .transpose()
    }

    /// 名前空間内に同名の制約があるか（`ConstraintNameExists`）。
    pub fn constraint_name_exists(&self, snap: &Snapshot, nsp: Oid, name: &str) -> Result<bool> {
        let (n, s) = (
            column_index(oids::PG_CONSTRAINT, "connamespace"),
            column_index(oids::PG_CONSTRAINT, "conname"),
        );
        Ok(!self
            .scan(snap, oids::PG_CONSTRAINT, |r| {
                Ok(as_oid(&r[n], "pg_constraint", "connamespace")? == nsp
                    && as_text(&r[s], "pg_constraint", "conname")? == name)
            })?
            .is_empty())
    }

    /// この表の制約（c / p / u）の名前。`ADD CONSTRAINT` の重複検査（`42710`）に使う。
    pub fn constraint_names_of(&self, snap: &Snapshot, relid: Oid) -> Result<Vec<String>> {
        let (r, n) = (
            column_index(oids::PG_CONSTRAINT, "conrelid"),
            column_index(oids::PG_CONSTRAINT, "conname"),
        );
        self.scan(snap, oids::PG_CONSTRAINT, |row| {
            Ok(as_oid(&row[r], "pg_constraint", "conrelid")? == relid)
        })?
        .iter()
        .map(|t| as_text(&t.row[n], "pg_constraint", "conname").map(str::to_owned))
        .collect()
    }

    /// `pg_attrdef` の行: `(adrelid, adnum)`。
    pub fn attrdef_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<(Oid, i16)>> {
        let (rel, num) = (
            column_index(oids::PG_ATTRDEF, "adrelid"),
            column_index(oids::PG_ATTRDEF, "adnum"),
        );
        let found = self.scan(snap, oids::PG_ATTRDEF, |r| {
            Ok(as_oid(&r[0], "pg_attrdef", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| {
                Ok((
                    as_oid(&t.row[rel], "pg_attrdef", "adrelid")?,
                    as_i16(&t.row[num], "pg_attrdef", "adnum")?,
                ))
            })
            .transpose()
    }

    /// 列の名前（`pg_attribute`）。
    pub fn attribute_name(
        &self,
        snap: &Snapshot,
        relid: Oid,
        attnum: i16,
    ) -> Result<Option<String>> {
        let col = |name: &str| column_index(oids::PG_ATTRIBUTE, name);
        let (rel, num, name) = (col("attrelid"), col("attnum"), col("attname"));
        let found = self.scan(snap, oids::PG_ATTRIBUTE, |r| {
            Ok(as_oid(&r[rel], "pg_attribute", "attrelid")? == relid
                && as_i16(&r[num], "pg_attribute", "attnum")? == attnum)
        })?;
        found
            .first()
            .map(|t| as_text(&t.row[name], "pg_attribute", "attname").map(str::to_owned))
            .transpose()
    }

    /// 表の部品（制約・既定値・索引）の OID。
    pub fn table_parts(&self, snap: &Snapshot, relid: Oid) -> Result<TableParts> {
        let by = |catalog: Oid, column: &str, result: usize| -> Result<Vec<Oid>> {
            let idx = column_index(catalog, column);
            let name = Self::catalog_name(catalog);
            self.scan(snap, catalog, |r| {
                Ok(as_oid(&r[idx], name, column)? == relid)
            })?
            .iter()
            .map(|t| as_oid(&t.row[result], name, "oid"))
            .collect()
        };
        Ok(TableParts {
            constraints: by(oids::PG_CONSTRAINT, "conrelid", 0)?,
            attrdefs: by(oids::PG_ATTRDEF, "adrelid", 0)?,
            indexes: by(
                oids::PG_INDEX,
                "indrelid",
                column_index(oids::PG_INDEX, "indexrelid"),
            )?,
        })
    }

    // ----- reading: pg_depend --------------------------------------------------------

    /// `pg_depend`: 依存先が `referenced` の行（物理順）。`referenced.obj_sub == 0` は
    /// 「そのオブジェクトの全列」を含む。
    pub fn dependents_of(
        &self,
        snap: &Snapshot,
        referenced: ObjectAddress,
    ) -> Result<Vec<DependRow>> {
        let c = DependCols::new();
        self.scan(snap, oids::PG_DEPEND, |r| {
            let x = c.referenced(r)?;
            Ok(x.class_id == referenced.class_id
                && x.obj_id == referenced.obj_id
                && (referenced.obj_sub == 0 || x.obj_sub == referenced.obj_sub))
        })?
        .iter()
        .map(|t| c.row(&t.row))
        .collect()
    }

    /// `pg_depend`: 依存元が `dependent` の行（`objsubid` は問わない）。
    pub fn references_of(
        &self,
        snap: &Snapshot,
        dependent: ObjectAddress,
    ) -> Result<Vec<DependRow>> {
        let c = DependCols::new();
        self.scan(snap, oids::PG_DEPEND, |r| {
            let d = c.dependent(r)?;
            Ok(d.class_id == dependent.class_id && d.obj_id == dependent.obj_id)
        })?
        .iter()
        .map(|t| c.row(&t.row))
        .collect()
    }

    /// この表の列が所有するシーケンス（`pg_depend`: 依存元が `relkind = S` の `pg_class`、
    /// 依存先が表、`deptype` a / i）の OID（`pg_depend` の物理順）。
    pub fn owned_sequences(&self, snap: &Snapshot, table_oid: Oid) -> Result<Vec<Oid>> {
        let rows = self
            .dependents_of(snap, ObjectAddress::relation(table_oid))?
            .into_iter()
            .filter(|d| {
                d.dependent.class_id == classes::RELATION
                    && matches!(d.deptype, DependType::Auto | DependType::Internal)
            })
            .map(|d| d.dependent.obj_id)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Ok(rows);
        }
        let want: HashSet<Oid> = rows.iter().copied().collect();
        let kind = column_index(oids::PG_CLASS, "relkind");
        let sequences: HashSet<Oid> = self
            .scan(snap, oids::PG_CLASS, |r| {
                Ok(want.contains(&as_oid(&r[0], "pg_class", "oid")?)
                    && as_char(&r[kind], "pg_class", "relkind")? == b'S')
            })?
            .iter()
            .map(|t| as_oid(&t.row[0], "pg_class", "oid"))
            .collect::<Result<_>>()?;
        Ok(rows.into_iter().filter(|o| sequences.contains(o)).collect())
    }

    // ----- load_table_def ---------------------------------------------------------------

    /// Builds the definition of a relation from `pg_class`, `pg_attribute`,
    /// `pg_attrdef`, `pg_constraint`, `pg_index`, `pg_depend` and
    /// `pg_sequence` as the snapshot sees them (`m4/07-catalog-ddl.md` §4.5).
    /// Tables (`r`) and sequences (`S`) give a `TableDef`; an index (`i`) is
    /// `Ok(None)`. The system catalogs give their pinned definition.
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
        let rel = self.relation_row_of(&class.row)?;
        if rel.kind == RelKind::Index {
            return Ok(None);
        }
        let nchecks = as_i16(
            &class.row[column_index(oids::PG_CLASS, "relchecks")],
            "pg_class",
            "relchecks",
        )?;
        let schema_name = self.namespace_name(snap, rel.namespace)?.ok_or_else(|| {
            Error::internal(format!("relation {oid} has no namespace {}", rel.namespace))
        })?;
        let defaults = self.load_defaults(snap, oid)?;
        let columns = self.load_columns(snap, oid, rel.natts, &defaults)?;
        let mut def = TableDef {
            oid,
            namespace: rel.namespace,
            schema: schema_name,
            name: rel.name.clone(),
            kind: rel.kind,
            locator: rel.locator,
            columns,
            checks: Vec::new(),
            indexes: Vec::new(),
            sequence: None,
            identity_seqs: Vec::new(),
        };
        match rel.kind {
            RelKind::Table => {
                def.checks = self.load_checks(snap, oid, nchecks)?;
                def.indexes = self.load_indexes(snap, oid, &def.columns)?;
                def.identity_seqs = self.load_identity_seqs(snap, oid, &def.columns)?;
            }
            RelKind::Sequence => def.sequence = Some(self.load_sequence_params(snap, oid)?),
            RelKind::Index => return Ok(None),
        }
        Ok(Some(def))
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
            let identity = match as_char(&r[col("attidentity")], "pg_attribute", "attidentity")? {
                0 => None,
                c => Some(IdentityKind::from_code(char::from(c)).ok_or_else(|| {
                    Error::corrupted(format!(
                        "relation {relid} column {attnum} has the unknown attidentity '{}'",
                        char::from(c)
                    ))
                })?),
            };
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
                identity,
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
                    no_inherit: matches!(&t.row[col("connoinherit")], Datum::Bool(true)),
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

    /// `pg_index` + `pg_class` + `pg_constraint` から、表の索引を OID の昇順で組み立てる（§4.5 の手順 5b）。
    fn load_indexes(
        &self,
        snap: &Snapshot,
        table_oid: Oid,
        columns: &[ColumnDef],
    ) -> Result<Vec<Arc<IndexDef>>> {
        let ic = |name: &str| column_index(oids::PG_INDEX, name);
        let indrelid = ic("indrelid");
        let index_rows = self.scan(snap, oids::PG_INDEX, |r| {
            Ok(as_oid(&r[indrelid], "pg_index", "indrelid")? == table_oid)
        })?;
        if index_rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: HashSet<Oid> = index_rows
            .iter()
            .map(|t| as_oid(&t.row[ic("indexrelid")], "pg_index", "indexrelid"))
            .collect::<Result<_>>()?;
        // pg_class: 索引の名前・名前空間・ファイル。
        let classes = self
            .scan(snap, oids::PG_CLASS, |r| {
                Ok(ids.contains(&as_oid(&r[0], "pg_class", "oid")?))
            })?
            .iter()
            .map(|t| {
                let row = self.relation_row_of(&t.row)?;
                Ok((row.oid, row))
            })
            .collect::<Result<HashMap<Oid, RelationRow>>>()?;
        // pg_constraint: conindid → (oid, name)（p / u だけ）。
        let cc = |name: &str| column_index(oids::PG_CONSTRAINT, name);
        let (conrelid, contype) = (cc("conrelid"), cc("contype"));
        let owners: HashMap<Oid, IndexConstraintRef> = self
            .scan(snap, oids::PG_CONSTRAINT, |r| {
                Ok(
                    as_oid(&r[conrelid], "pg_constraint", "conrelid")? == table_oid
                        && matches!(
                            as_char(&r[contype], "pg_constraint", "contype")?,
                            b'p' | b'u'
                        ),
                )
            })?
            .iter()
            .map(|t| {
                Ok((
                    as_oid(&t.row[cc("conindid")], "pg_constraint", "conindid")?,
                    IndexConstraintRef {
                        oid: as_oid(&t.row[0], "pg_constraint", "oid")?,
                        name: as_text(&t.row[cc("conname")], "pg_constraint", "conname")?
                            .to_owned(),
                    },
                ))
            })
            .collect::<Result<_>>()?;

        let mut out = Vec::with_capacity(index_rows.len());
        for t in &index_rows {
            let r = &t.row;
            let index_oid = as_oid(&r[ic("indexrelid")], "pg_index", "indexrelid")?;
            let corrupt = |what: &str| {
                Error::corrupted(format!("index {index_oid} of relation {table_oid}: {what}"))
            };
            if !r[ic("indexprs")].is_null() || !r[ic("indpred")].is_null() {
                return Err(corrupt("expression and partial indexes are not supported"));
            }
            let class = classes
                .get(&index_oid)
                .ok_or_else(|| corrupt("it has no pg_class row"))?;
            let natts = usize::try_from(as_i16(&r[ic("indnatts")], "pg_index", "indnatts")?)
                .unwrap_or(usize::MAX);
            let key = as_int2vector(&r[ic("indkey")], "pg_index", "indkey")?;
            let classes_v = as_oidvector(&r[ic("indclass")], "pg_index", "indclass")?;
            let options = as_int2vector(&r[ic("indoption")], "pg_index", "indoption")?;
            if key.len() != natts || classes_v.len() != natts || options.len() != natts {
                return Err(corrupt("indnatts does not match the key columns"));
            }
            let mut cols = Vec::with_capacity(natts);
            for ((attnum, class_oid), option) in key.iter().zip(classes_v).zip(options) {
                if !columns.iter().any(|c| c.attnum == *attnum) {
                    return Err(corrupt("indkey refers to a column the table does not have"));
                }
                let opfamily = opclass::opclass_by_oid(*class_oid)
                    .map(|c| c.family)
                    .ok_or_else(|| {
                        Error::internal(format!("unknown operator class {class_oid}"))
                    })?;
                let (descending, nulls_first) = IndexDef::flags_from_indoption(*option);
                cols.push(IndexColumn {
                    attnum: *attnum,
                    opclass: *class_oid,
                    opfamily,
                    descending,
                    nulls_first,
                });
            }
            out.push(Arc::new(IndexDef {
                oid: index_oid,
                name: class.name.clone(),
                namespace: class.namespace,
                table_oid,
                locator: class.locator,
                columns: cols,
                unique: as_bool(&r[ic("indisunique")], "pg_index", "indisunique")?,
                primary: as_bool(&r[ic("indisprimary")], "pg_index", "indisprimary")?,
                constraint: owners.get(&index_oid).cloned(),
            }));
        }
        out.sort_by_key(|i| i.oid);
        Ok(out)
    }

    /// IDENTITY 列と暗黙のシーケンスの対応（`attnum` 順）。`attidentity` のある列と 1 対 1 でなければ
    /// `Error::corrupted`。
    fn load_identity_seqs(
        &self,
        snap: &Snapshot,
        table_oid: Oid,
        columns: &[ColumnDef],
    ) -> Result<Vec<(i16, Oid)>> {
        let mut seqs: Vec<(i16, Oid)> = self
            .dependents_of(snap, ObjectAddress::relation(table_oid))?
            .into_iter()
            .filter(|d| {
                d.deptype == DependType::Internal
                    && d.dependent.class_id == classes::RELATION
                    && d.referenced.obj_sub > 0
            })
            .filter_map(|d| {
                i16::try_from(d.referenced.obj_sub)
                    .ok()
                    .map(|a| (a, d.dependent.obj_id))
            })
            .collect();
        seqs.sort_unstable();
        let identity_cols: Vec<i16> = columns
            .iter()
            .filter(|c| c.identity.is_some())
            .map(|c| c.attnum)
            .collect();
        let have: Vec<i16> = seqs.iter().map(|(a, _)| *a).collect();
        if have != identity_cols {
            return Err(Error::corrupted(format!(
                "relation {table_oid}: the identity columns {identity_cols:?} do not match the identity sequences {have:?}"
            )));
        }
        Ok(seqs)
    }

    fn load_sequence_params(&self, snap: &Snapshot, oid: Oid) -> Result<SequenceParams> {
        let c = |name: &str| column_index(oids::PG_SEQUENCE, name);
        let found = self.scan(snap, oids::PG_SEQUENCE, |r| {
            Ok(as_oid(&r[0], "pg_sequence", "seqrelid")? == oid)
        })?;
        let Some(t) = found.first() else {
            return Err(Error::corrupted(format!(
                "sequence {oid} has no pg_sequence row"
            )));
        };
        let r = &t.row;
        // owned_by: 依存元がこのシーケンス、依存先が表の列（a / i）。
        let owned_by = self
            .references_of(snap, ObjectAddress::relation(oid))?
            .into_iter()
            .find(|d| {
                d.referenced.class_id == classes::RELATION
                    && matches!(d.deptype, DependType::Auto | DependType::Internal)
            })
            .and_then(|d| {
                i16::try_from(d.referenced.obj_sub)
                    .ok()
                    .map(|a| (d.referenced.obj_id, a))
            });
        Ok(SequenceParams {
            type_oid: as_oid(&r[c("seqtypid")], "pg_sequence", "seqtypid")?,
            start: as_i64(&r[c("seqstart")], "pg_sequence", "seqstart")?,
            increment: as_i64(&r[c("seqincrement")], "pg_sequence", "seqincrement")?,
            min: as_i64(&r[c("seqmin")], "pg_sequence", "seqmin")?,
            max: as_i64(&r[c("seqmax")], "pg_sequence", "seqmax")?,
            cache: as_i64(&r[c("seqcache")], "pg_sequence", "seqcache")?,
            cycle: as_bool(&r[c("seqcycle")], "pg_sequence", "seqcycle")?,
            owned_by,
        })
    }

    // ----- OIDs ---------------------------------------------------------------------------

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

    /// 新しい relfilenode（TRUNCATE）。`get_new_relation_oid` と同じ（OID カウンタ + カタログと
    /// `storage_exists` の衝突確認）。
    pub fn get_new_relfilenumber(&self, alloc: &OidAllocator) -> Result<Oid> {
        self.get_new_relation_oid(alloc)
    }

    /// The OIDs `create_table` needs, in the order of `m4/07-catalog-ddl.md` §5.1 の手順 4:
    /// 表 → `pg_attrdef`（列の順）→ CHECK の制約 → 索引制約ごとに（索引、その制約）。
    pub fn allocate_table_oids(
        &self,
        alloc: &OidAllocator,
        req: &TableOidRequest,
    ) -> Result<TableOids> {
        // The OIDs of one statement must differ from each other: the
        // counter never repeats a value, so distinct calls do not collide.
        let table = self.get_new_relation_oid(alloc)?;
        let attrdefs = (0..req.n_attrdefs)
            .map(|_| self.get_new_oid(alloc, oids::PG_ATTRDEF))
            .collect::<Result<Vec<_>>>()?;
        let checks = (0..req.n_checks)
            .map(|_| self.get_new_oid(alloc, oids::PG_CONSTRAINT))
            .collect::<Result<Vec<_>>>()?;
        let index_constraints = (0..req.n_index_constraints)
            .map(|_| {
                let index = self.get_new_relation_oid(alloc)?;
                let constraint = self.get_new_oid(alloc, oids::PG_CONSTRAINT)?;
                Ok((index, constraint))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TableOids {
            table,
            attrdefs,
            checks,
            index_constraints,
        })
    }

    /// M2 の形（`allocate_table_oids` に移行するまでの互換）: one OID per column with a DEFAULT
    /// (`pg_attrdef`) and one per CHECK (`pg_constraint`).
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
        let constraints = checks
            .iter()
            .map(|_| self.get_new_oid(alloc, oids::PG_CONSTRAINT))
            .collect::<Result<Vec<_>>>()?;
        Ok((attrdefs, constraints))
    }

    // ----- writing --------------------------------------------------------------------------

    fn insert_row(&self, w: &WriteCtx, catalog_oid: Oid, row: &Row) -> Result<()> {
        self.storage.insert(self.rel(catalog_oid)?, w, row)?;
        Ok(())
    }

    /// カタログ `catalog_oid` の、`snap` から見える行のうち `pred` が真の最初の 1 行を `new_row(old)` で
    /// 置き換える。見つからない・見えない（同じコマンドで挿入した行）・同じコマンドで更新済みは
    /// `Error::internal`（D07-3: 単一ライター、1 コマンド 1 回）。
    fn update_row_where(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        catalog_oid: Oid,
        pred: impl FnMut(&Row) -> Result<bool>,
        new_row: impl FnOnce(&Row) -> Row,
    ) -> Result<()> {
        let name = Self::catalog_name(catalog_oid);
        let found = self.scan(snap, catalog_oid, pred)?;
        let Some(old) = found.first() else {
            return Err(Error::internal(format!(
                "no visible {name} row to update (a row cannot be updated in the command that created it)"
            )));
        };
        let new = new_row(&old.row);
        let out = self
            .storage
            .update(self.rel(catalog_oid)?, w, snap, old.tid, &new)?;
        match out.result {
            TmResult::Ok => Ok(()),
            other => Err(Error::internal(format!(
                "could not update a {name} row: {other:?}"
            ))),
        }
    }

    /// 走査で TID を集めてから消す（走査と更新を混ぜない）。消した件数を返す。
    fn delete_where(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        catalog_oid: Oid,
        pred: impl FnMut(&Row) -> Result<bool>,
    ) -> Result<usize> {
        let name = Self::catalog_name(catalog_oid);
        let victims = self.scan(snap, catalog_oid, pred)?;
        for t in &victims {
            let result = self
                .storage
                .delete(self.rel(catalog_oid)?, w, snap, t.tid)?;
            expect_deleted(result, name)?;
        }
        Ok(victims.len())
    }

    /// `pg_class`・`pg_attribute`（システム列を含む）・`pg_attrdef`・`pg_constraint`（CHECK）に加えて、
    /// `spec.indexes` の索引ごとの行（[`Self::write_index_rows`]）、既定値・CHECK の依存、
    /// `spec.extra_depends` を書く。ファイルは作らない。
    ///
    /// 表の行は最初から最終形（`relhasindex = !indexes.is_empty()`）で書く（D07-3）。
    #[allow(clippy::too_many_lines)]
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
        if spec.indexes.iter().any(|i| i.table_oid != spec.oid) {
            return Err(Error::internal(
                "create_table: an index belongs to a different table",
            ));
        }
        let nchecks = i16::try_from(spec.checks.len())
            .map_err(|_| Error::new(sqlstate::TOO_MANY_COLUMNS, "too many CHECK constraints"))?;

        self.insert_row(
            w,
            oids::PG_CLASS,
            &class_row(&ClassSpec {
                nchecks,
                has_index: !spec.indexes.is_empty(),
                ..ClassSpec::table(spec.oid, &spec.name, spec.namespace, spec.owner, natts)
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
                identity: c.identity,
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
        let defaults: Vec<(i16, &str)> = spec
            .columns
            .iter()
            .filter_map(|c| c.default.as_ref().map(|d| (c.attnum, d.expr_sql.as_str())))
            .collect();
        for ((attnum, sql), attrdef_oid) in defaults.iter().zip(&spec.attrdef_oids) {
            self.insert_row(
                w,
                oids::PG_ATTRDEF,
                &attrdef_row(*attrdef_oid, spec.oid, *attnum, sql),
            )?;
        }
        for (check, constraint_oid) in spec.checks.iter().zip(&spec.constraint_oids) {
            self.insert_row(
                w,
                oids::PG_CONSTRAINT,
                &rows::check_constraint_row(
                    *constraint_oid,
                    &check.name,
                    spec.namespace,
                    spec.oid,
                    &check.expr_sql,
                    check.no_inherit,
                ),
            )?;
        }
        for index in &spec.indexes {
            self.write_index_rows(w, index)?;
        }
        // 表の依存: 既定値 → 列（a）、CHECK → 表（a）。
        let mut depends = Vec::new();
        for ((attnum, _), attrdef_oid) in defaults.iter().zip(&spec.attrdef_oids) {
            depends.push(NewDepend {
                dependent: ObjectAddress::attrdef(*attrdef_oid),
                referenced: ObjectAddress::column(spec.oid, *attnum),
                deptype: DependType::Auto,
            });
        }
        for constraint_oid in &spec.constraint_oids {
            depends.push(NewDepend {
                dependent: ObjectAddress::constraint(*constraint_oid),
                referenced: ObjectAddress::relation(spec.oid),
                deptype: DependType::Auto,
            });
        }
        depends.extend(spec.extra_depends.iter().cloned());
        self.record_dependencies(w, &depends)
    }

    /// 索引 1 つぶんの行: `pg_class(i)` → `pg_attribute` → `pg_index` → 制約の `pg_constraint` → `pg_depend`
    /// （`create_table` と `create_index` が共有する。`m4/07-catalog-ddl.md` §3.6）。
    #[allow(clippy::too_many_lines)]
    fn write_index_rows(&self, w: &WriteCtx, spec: &NewIndex) -> Result<()> {
        if spec.columns.is_empty() {
            return Err(Error::internal("an index needs at least one column"));
        }
        let natts = i16::try_from(spec.columns.len()).map_err(|_| {
            Error::new(
                sqlstate::TOO_MANY_COLUMNS,
                "cannot use more than 32 columns in an index",
            )
        })?;
        let attrs = index_attribute_rows(spec);
        if attrs.len() != spec.columns.len() {
            return Err(Error::internal(format!(
                "index \"{}\" has a column whose type is not a built-in type",
                spec.name
            )));
        }
        self.insert_row(
            w,
            oids::PG_CLASS,
            &class_row(&ClassSpec {
                oid: spec.oid,
                name: &spec.name,
                namespace: spec.namespace,
                reltype: 0,
                owner: spec.owner,
                relfilenode: spec.relfilenode,
                reltablespace: 0,
                is_shared: false,
                natts,
                nchecks: 0,
                replident: 'n',
                kind: RelKind::Index,
                has_index: false,
                relpages: i32::try_from(spec.stats.pages).unwrap_or(i32::MAX),
                #[allow(clippy::cast_precision_loss)]
                reltuples: spec.stats.tuples as f32,
            }),
        )?;
        for row in &attrs {
            self.insert_row(w, oids::PG_ATTRIBUTE, row)?;
        }
        let key: Vec<i16> = spec.columns.iter().map(|c| c.column.attnum).collect();
        let collations: Vec<Oid> = spec
            .columns
            .iter()
            .map(|c| builtin::type_by_oid(c.ty.oid).map_or(0, |t| t.collation))
            .collect();
        let classes_v: Vec<Oid> = spec.columns.iter().map(|c| c.column.opclass).collect();
        let options: Vec<i16> = spec
            .columns
            .iter()
            .map(|c| i16::from(c.column.descending) | (i16::from(c.column.nulls_first) << 1))
            .collect();
        self.insert_row(
            w,
            oids::PG_INDEX,
            &index_row(&IndexRowSpec {
                index_oid: spec.oid,
                table_oid: spec.table_oid,
                unique: spec.unique,
                primary: spec.primary,
                key: &key,
                collations: &collations,
                classes: &classes_v,
                options: &options,
            }),
        )?;
        let mut depends = Vec::new();
        if let Some(con) = &spec.constraint {
            let kind = if spec.primary {
                ConstraintKind::PrimaryKey
            } else {
                ConstraintKind::Unique
            };
            self.insert_row(
                w,
                oids::PG_CONSTRAINT,
                &constraint_row(&ConstraintRowSpec {
                    oid: con.oid,
                    name: &con.name,
                    namespace: spec.namespace,
                    kind,
                    relid: spec.table_oid,
                    index_oid: spec.oid,
                    columns: &key,
                    check_sql: None,
                    no_inherit: false,
                }),
            )?;
            for attnum in &key {
                depends.push(NewDepend {
                    dependent: ObjectAddress::constraint(con.oid),
                    referenced: ObjectAddress::column(spec.table_oid, *attnum),
                    deptype: DependType::Auto,
                });
            }
            depends.push(NewDepend {
                dependent: ObjectAddress::relation(spec.oid),
                referenced: ObjectAddress::constraint(con.oid),
                deptype: DependType::Internal,
            });
        } else {
            for attnum in &key {
                depends.push(NewDepend {
                    dependent: ObjectAddress::relation(spec.oid),
                    referenced: ObjectAddress::column(spec.table_oid, *attnum),
                    deptype: DependType::Auto,
                });
            }
        }
        // 同じ列の重複は 1 行にまとめる。
        self.record_dependencies(w, &depends)
    }

    /// 既存の表への索引: `pg_class(i)`・`pg_attribute`・`pg_index`・（`constraint` があれば）`pg_constraint` と
    /// `pg_depend`。`mark_table_indexed` が真なら表の `pg_class.relhasindex` を `true` にする
    /// （まだ `false` のときだけ `update_class_row`）。
    pub fn create_index(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        spec: &NewIndex,
        mark_table_indexed: bool,
    ) -> Result<()> {
        self.write_index_rows(w, spec)?;
        if mark_table_indexed {
            let table = self.relation_row(snap, spec.table_oid)?.ok_or_else(|| {
                Error::internal(format!("table {} of the new index is gone", spec.table_oid))
            })?;
            if !table.has_index {
                self.update_class_row(
                    w,
                    snap,
                    spec.table_oid,
                    &ClassPatch {
                        relhasindex: Some(true),
                        ..ClassPatch::default()
                    },
                )?;
            }
        }
        Ok(())
    }

    /// `ALTER TABLE ADD PRIMARY KEY / UNIQUE`: `create_index`（`spec.constraint = Some`、
    /// `mark_table_indexed = true`）に加えて、`spec.primary` なら key 列の `attnotnull` を `true` にする
    /// （すでに `true` の列は更新しない）。
    pub fn add_constraint(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        spec: &NewIndex,
        table: &TableDef,
    ) -> Result<()> {
        if spec.constraint.is_none() {
            return Err(Error::internal("add_constraint needs a constraint"));
        }
        self.create_index(w, snap, spec, true)?;
        if spec.primary {
            for c in &spec.columns {
                let attnum = c.column.attnum;
                let already = table
                    .columns
                    .iter()
                    .any(|d| d.attnum == attnum && d.not_null);
                if !already {
                    self.update_attribute(
                        w,
                        snap,
                        table.oid,
                        attnum,
                        &AttributePatch {
                            not_null: Some(true),
                            ..AttributePatch::default()
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    /// `ALTER TABLE ADD CHECK`: `pg_constraint(c)` の行、`relchecks` の加算、表への依存（a）。
    pub fn add_check(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        table: &TableDef,
        constraint_oid: Oid,
        check: &CheckDef,
    ) -> Result<()> {
        self.insert_row(
            w,
            oids::PG_CONSTRAINT,
            &rows::check_constraint_row(
                constraint_oid,
                &check.name,
                table.namespace,
                table.oid,
                &check.expr_sql,
                check.no_inherit,
            ),
        )?;
        let nchecks = i16::try_from(table.checks.len() + 1)
            .map_err(|_| Error::internal("too many CHECK constraints"))?;
        self.update_class_row(
            w,
            snap,
            table.oid,
            &ClassPatch {
                relchecks: Some(nchecks),
                ..ClassPatch::default()
            },
        )?;
        self.record_dependencies(
            w,
            &[NewDepend {
                dependent: ObjectAddress::constraint(constraint_oid),
                referenced: ObjectAddress::relation(table.oid),
                deptype: DependType::Auto,
            }],
        )
    }

    /// シーケンス: `pg_class(S)`・`pg_attribute`（3 列 + システム列）・`pg_sequence`・`pg_depend`
    /// （`params.owned_by` と `owned_by_deptype` が `Some` のとき）。08 の `ddl/sequence.rs` が呼ぶ。
    pub fn create_sequence(
        &self,
        w: &WriteCtx,
        _snap: &Snapshot,
        spec: &NewSequence,
    ) -> Result<()> {
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
                natts: 3,
                nchecks: 0,
                replident: 'n',
                kind: RelKind::Sequence,
                has_index: false,
                relpages: 1,
                reltuples: 1.0,
            }),
        )?;
        let attrs = sequence_attribute_rows(spec.oid);
        if attrs.len() != 3 + 6 {
            return Err(Error::internal(
                "the sequence columns are not built-in types",
            ));
        }
        for row in &attrs {
            self.insert_row(w, oids::PG_ATTRIBUTE, row)?;
        }
        self.insert_row(w, oids::PG_SEQUENCE, &sequence_row(spec.oid, &spec.params))?;
        if let (Some((table, attnum)), Some(deptype)) =
            (spec.params.owned_by, spec.owned_by_deptype)
        {
            self.record_dependency(
                w,
                &NewDepend {
                    dependent: ObjectAddress::relation(spec.oid),
                    referenced: ObjectAddress::column(table, attnum),
                    deptype,
                },
            )?;
        }
        Ok(())
    }

    /// `ALTER SEQUENCE`: `pg_sequence` の行を更新する（`seqtypid`・`seqstart`・`seqincrement`・`seqmax`・
    /// `seqmin`・`seqcache`・`seqcycle`）。`owned_by` は `pg_depend` なので触らない。
    pub fn update_sequence_params(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        oid: Oid,
        p: &SequenceParams,
    ) -> Result<()> {
        self.update_row_where(
            w,
            snap,
            oids::PG_SEQUENCE,
            |r| Ok(as_oid(&r[0], "pg_sequence", "seqrelid")? == oid),
            |_| sequence_row(oid, p),
        )
    }

    /// `plan.items` のオブジェクトの行をすべて消す（`m4/07-catalog-ddl.md` §6.3）。消したリレーションの
    /// ファイルの場所を返す（呼び出し側が `pending_unlinks` に積む）。`DROP INDEX` も `plan_drop` の
    /// items（`DropKind::Index`）を渡す。
    pub fn drop_objects(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        plan: &DropPlan,
    ) -> Result<Vec<RelFileLocator>> {
        let keys: HashSet<(Oid, Oid)> = plan
            .items
            .iter()
            .map(|i| (i.addr.class_id, i.addr.obj_id))
            .collect();
        let mut locators = Vec::new();
        let (mut rels, mut indexes, mut sequences) =
            (HashSet::new(), HashSet::new(), HashSet::new());
        let mut constraints = HashSet::new();
        let mut attrdefs: Vec<(Oid, Oid, i16)> = Vec::new();
        for item in &plan.items {
            let oid = item.addr.obj_id;
            match item.kind {
                DropKind::Table => {
                    rels.insert(oid);
                }
                DropKind::Index => {
                    rels.insert(oid);
                    indexes.insert(oid);
                }
                DropKind::Sequence => {
                    rels.insert(oid);
                    sequences.insert(oid);
                }
                DropKind::Constraint => {
                    constraints.insert(oid);
                }
                DropKind::AttrDefault => {
                    if let (Some(table), Some(attnum)) = (item.owner_table, item.attnum) {
                        attrdefs.push((oid, table, attnum));
                    }
                }
            }
            if let Some(l) = item.locator {
                locators.push(l);
            }
        }

        let a_rel = column_index(oids::PG_ATTRIBUTE, "attrelid");
        self.delete_where(w, snap, oids::PG_ATTRIBUTE, |r| {
            Ok(rels.contains(&as_oid(&r[a_rel], "pg_attribute", "attrelid")?))
        })?;
        if !indexes.is_empty() {
            let (id, rel) = (
                column_index(oids::PG_INDEX, "indexrelid"),
                column_index(oids::PG_INDEX, "indrelid"),
            );
            self.delete_where(w, snap, oids::PG_INDEX, |r| {
                Ok(indexes.contains(&as_oid(&r[id], "pg_index", "indexrelid")?)
                    || rels.contains(&as_oid(&r[rel], "pg_index", "indrelid")?))
            })?;
        }
        if !sequences.is_empty() {
            self.delete_where(w, snap, oids::PG_SEQUENCE, |r| {
                Ok(sequences.contains(&as_oid(&r[0], "pg_sequence", "seqrelid")?))
            })?;
        }
        if !constraints.is_empty() {
            self.delete_where(w, snap, oids::PG_CONSTRAINT, |r| {
                Ok(constraints.contains(&as_oid(&r[0], "pg_constraint", "oid")?))
            })?;
        }
        if !attrdefs.is_empty() {
            let defs: HashSet<Oid> = attrdefs.iter().map(|(o, _, _)| *o).collect();
            self.delete_where(w, snap, oids::PG_ATTRDEF, |r| {
                Ok(defs.contains(&as_oid(&r[0], "pg_attrdef", "oid")?))
            })?;
            // 列が生き残る（表が計画にない）既定値は、atthasdef を戻す。
            for (_, table, attnum) in &attrdefs {
                if !keys.contains(&(classes::RELATION, *table)) {
                    self.update_attribute(
                        w,
                        snap,
                        *table,
                        *attnum,
                        &AttributePatch {
                            has_default: Some(false),
                            ..AttributePatch::default()
                        },
                    )?;
                }
            }
        }
        self.delete_where(w, snap, oids::PG_CLASS, |r| {
            Ok(rels.contains(&as_oid(&r[0], "pg_class", "oid")?))
        })?;
        let c = DependCols::new();
        self.delete_where(w, snap, oids::PG_DEPEND, |r| {
            Ok(keys.contains(&c.dependent(r)?.key_pair())
                || keys.contains(&c.referenced(r)?.key_pair()))
        })?;
        Ok(locators)
    }

    /// Legacy of M2: 表 1 つを CASCADE で消す（`drop_objects` に移行するまでの互換）。ファイルの場所は
    /// 返さない（呼び出し側が `def.locator` を unlink する。索引・シーケンスのファイルは呼び出し側が
    /// `drop_objects` を使うまで残る）。
    pub fn drop_table(&self, w: &WriteCtx, snap: &Snapshot, def: &TableDef) -> Result<()> {
        if def.is_system_catalog() {
            return Err(Error::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{}\" is a system catalog", def.name),
            ));
        }
        let plan = plan_drop(
            self,
            snap,
            &[ObjectAddress::relation(def.oid)],
            DropBehavior::Cascade,
            &[],
        )?;
        self.drop_objects(w, snap, &plan)?;
        Ok(())
    }

    /// `pg_class` の 1 行を更新する。見つからない・見えない・同じコマンドで 2 度目は `Error::internal`（D07-3）。
    pub fn update_class_row(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        oid: Oid,
        patch: &ClassPatch,
    ) -> Result<()> {
        let col = |name: &str| column_index(oids::PG_CLASS, name);
        self.update_row_where(
            w,
            snap,
            oids::PG_CLASS,
            |r| Ok(as_oid(&r[0], "pg_class", "oid")? == oid),
            |old| {
                let mut row = old.clone();
                if let Some(v) = patch.relfilenode {
                    row[col("relfilenode")] = Datum::Oid(v);
                }
                if let Some(v) = patch.relhasindex {
                    row[col("relhasindex")] = Datum::Bool(v);
                }
                if let Some(v) = patch.relpages {
                    row[col("relpages")] = Datum::Int4(v);
                }
                if let Some(v) = patch.reltuples {
                    row[col("reltuples")] = Datum::Float4(v);
                }
                if let Some(v) = patch.relowner {
                    row[col("relowner")] = Datum::Oid(v);
                }
                if let Some(v) = patch.relchecks {
                    row[col("relchecks")] = Datum::Int2(v);
                }
                row
            },
        )
    }

    pub fn update_attribute(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        relid: Oid,
        attnum: i16,
        patch: &AttributePatch,
    ) -> Result<()> {
        let col = |name: &str| column_index(oids::PG_ATTRIBUTE, name);
        let (rel, num) = (col("attrelid"), col("attnum"));
        self.update_row_where(
            w,
            snap,
            oids::PG_ATTRIBUTE,
            |r| {
                Ok(as_oid(&r[rel], "pg_attribute", "attrelid")? == relid
                    && as_i16(&r[num], "pg_attribute", "attnum")? == attnum)
            },
            |old| {
                let mut row = old.clone();
                if let Some(v) = patch.not_null {
                    row[col("attnotnull")] = Datum::Bool(v);
                }
                if let Some(v) = patch.has_default {
                    row[col("atthasdef")] = Datum::Bool(v);
                }
                row
            },
        )
    }

    /// TRUNCATE: `update_class_row(oid, relfilenode = new, relpages = 0, reltuples = -1)`。
    pub fn truncate_relation(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        oid: Oid,
        new_relfilenode: Oid,
    ) -> Result<()> {
        self.update_class_row(
            w,
            snap,
            oid,
            &ClassPatch {
                relfilenode: Some(new_relfilenode),
                relpages: Some(0),
                reltuples: Some(-1.0),
                ..ClassPatch::default()
            },
        )
    }

    pub fn record_dependency(&self, w: &WriteCtx, d: &NewDepend) -> Result<()> {
        self.insert_row(w, oids::PG_DEPEND, &depend_row(d))
    }

    /// 同じ（依存元、依存先、`deptype`）の重複を除いて挿入する。
    pub fn record_dependencies(&self, w: &WriteCtx, ds: &[NewDepend]) -> Result<()> {
        let mut seen: Vec<&NewDepend> = Vec::with_capacity(ds.len());
        for d in ds {
            if seen.contains(&d) {
                continue;
            }
            seen.push(d);
            self.record_dependency(w, d)?;
        }
        Ok(())
    }

    /// `pg_depend` の行を消す。`filter` に当たる行をすべて（DROP の後始末と、08 の OWNED BY の付け替えが使う）。
    pub fn delete_dependencies(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        filter: DependFilter,
    ) -> Result<usize> {
        let c = DependCols::new();
        self.delete_where(w, snap, oids::PG_DEPEND, |r| match filter {
            DependFilter::Dependent { class_id, obj_id } => {
                let d = c.dependent(r)?;
                Ok(d.class_id == class_id && d.obj_id == obj_id)
            }
            DependFilter::Referenced { class_id, obj_id } => {
                let x = c.referenced(r)?;
                Ok(x.class_id == class_id && x.obj_id == obj_id)
            }
            DependFilter::Exact {
                dependent,
                referenced,
            } => Ok(c.dependent(r)? == dependent && c.referenced(r)? == referenced),
        })
    }

    /// initdb: creates the files of the per-database catalogs and inserts
    /// their initial rows (`m2.md` §6.8.2). The rows of the shared catalogs
    /// are written by [`SharedCatalogStore::bootstrap`].
    pub fn bootstrap(&self, w: &WriteCtx, params: &InitParams) -> Result<()> {
        for def in schema::CATALOGS.iter().filter(|d| !d.shared) {
            let rel = self.rel(def.oid)?;
            self.storage.create_storage(w, rel.locator)?;
            for row in rows::initial_rows(def.oid, params) {
                self.storage.insert(rel, w, &row)?;
            }
        }
        Ok(())
    }
}

/// `pg_depend` の削除条件（`CatalogStore::delete_dependencies`）。
#[derive(Clone, Copy, Debug)]
pub enum DependFilter {
    /// 依存元が `(class_id, obj_id)`（`obj_sub` は問わない）。
    Dependent { class_id: Oid, obj_id: Oid },
    /// 依存先が `(class_id, obj_id)`（`obj_sub` は問わない）。
    Referenced { class_id: Oid, obj_id: Oid },
    /// 依存元・依存先の両方を指定（行を 1 つ消す）。
    Exact {
        dependent: ObjectAddress,
        referenced: ObjectAddress,
    },
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
            self.storage.create_storage(w, rel.locator)?;
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
    //! An in-memory `TableStore` for the catalog unit tests. It models the visibility rules of the
    //! heap that the catalog code depends on: a row inserted by the own transaction is invisible to a
    //! snapshot whose `curcid` is not above the row's command ID (so a row cannot be updated in the
    //! command that created it, D07-3), a deleted row stays in the file for the "see everything"
    //! snapshot, and every other transaction's changes count as committed.

    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::*;
    use crate::catalog::rows::datum_matches_type;
    use crate::storage::{HeapScan, UpdateOutcome};
    use crate::types::Tid;

    #[derive(Debug, Clone)]
    struct FakeTuple {
        tid: Tid,
        row: Row,
        xmin: Xid,
        cmin: CommandId,
        xmax: Option<Xid>,
        cmax: CommandId,
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

    fn visible(t: &FakeTuple, snap: &Snapshot) -> bool {
        if is_any(snap) {
            return true;
        }
        if snap.own_xid == Some(t.xmin) && t.cmin >= snap.curcid {
            return false;
        }
        match t.xmax {
            None => true,
            Some(x) if snap.own_xid == Some(x) => t.cmax >= snap.curcid,
            Some(_) => false,
        }
    }

    impl TableStore for FakeStore {
        fn create_storage(&self, _w: &WriteCtx, rel: RelFileLocator) -> Result<()> {
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
                // `int2[]` holds `Datum::Int2Vector` since M4 (conkey).
                if !d.is_null()
                    && builtin::is_null_only_type(a.type_oid)
                    && !matches!(d, Datum::Int2Vector(_))
                {
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
                cmin: w.cid,
                xmax: None,
                cmax: 0,
            });
            Ok(tid)
        }

        fn delete(
            &self,
            rel: &RelHandle,
            w: &WriteCtx,
            snap: &Snapshot,
            tid: Tid,
        ) -> Result<TmResult> {
            let mut g = self.inner.lock().unwrap();
            let t = g
                .tuples
                .get_mut(&rel.locator)
                .and_then(|v| v.iter_mut().find(|t| t.tid == tid));
            match t {
                Some(t) if !visible(t, snap) => Ok(TmResult::Invisible),
                Some(t) if t.xmax == Some(w.xid) => Ok(TmResult::SelfModified { cmax: t.cmax }),
                Some(t) => {
                    t.xmax = Some(w.xid);
                    t.cmax = w.cid;
                    Ok(TmResult::Ok)
                }
                None => Ok(TmResult::Invisible),
            }
        }

        fn update(
            &self,
            rel: &RelHandle,
            w: &WriteCtx,
            snap: &Snapshot,
            tid: Tid,
            new_row: &[Datum],
        ) -> Result<UpdateOutcome> {
            let result = self.delete(rel, w, snap, tid)?;
            if result != TmResult::Ok {
                return Ok(UpdateOutcome {
                    result,
                    new_tid: None,
                });
            }
            let new_tid = self.insert(rel, w, new_row)?;
            Ok(UpdateOutcome {
                result: TmResult::Ok,
                new_tid: Some(new_tid),
            })
        }

        fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
            let g = self.inner.lock().unwrap();
            let tuples = g
                .tuples
                .get(&rel.locator)
                .map(|v| {
                    v.iter()
                        .filter(|t| visible(t, snap))
                        .map(|t| HeapTuple {
                            tid: t.tid,
                            xmin: t.xmin,
                            xmax: t.xmax.unwrap_or(Xid::INVALID),
                            cmin: t.cmin,
                            cmax: t.cmax,
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
pub(crate) mod test_env {
    //! 共有のテスト環境（`FakeStore` の上の `CatalogStore`）。`depend.rs` / `check.rs` のテストも使う。

    use super::fake_store::FakeStore;
    use super::*;
    use crate::control::{ControlData, ControlFileHandle};
    use crate::storage::vfs::{SimVfs, Vfs};

    pub(crate) const DB: Oid = 5;
    pub(crate) const OWN: Xid = Xid(100);

    /// 他のトランザクションの変更はコミット済みとして見える、`own_xid` なしのスナップショット。
    pub(crate) fn snap() -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(1000),
            xip: vec![],
            curcid: 100,
            own_xid: None,
        }
    }

    pub(crate) fn wctx() -> WriteCtx {
        WriteCtx {
            xid: Xid::BOOTSTRAP,
            cid: 0,
        }
    }

    pub(crate) fn params() -> InitParams {
        InitParams {
            superuser: "postgres".into(),
        }
    }

    pub(crate) struct Env {
        pub fake: Arc<FakeStore>,
        pub store: CatalogStore,
        pub shared: SharedCatalogStore,
        pub oids: OidAllocator,
    }

    pub(crate) fn env() -> Env {
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

    pub(crate) fn col(
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
            identity: None,
        }
    }

    pub(crate) fn int4_col(attnum: i16, desc: bool, nulls_first: bool) -> NewIndexColumn {
        NewIndexColumn {
            name: format!("c{attnum}"),
            column: IndexColumn {
                attnum,
                opclass: 1978,
                opfamily: 1976,
                descending: desc,
                nulls_first,
            },
            ty: SqlType::INT4,
        }
    }

    pub(crate) fn new_index(
        e: &Env,
        table: Oid,
        name: &str,
        cols: Vec<NewIndexColumn>,
        constraint: Option<(Oid, bool)>,
    ) -> NewIndex {
        let oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        NewIndex {
            oid,
            name: name.into(),
            namespace: oids::NAMESPACE_PUBLIC,
            owner: oids::BOOTSTRAP_SUPERUSER,
            table_oid: table,
            relfilenode: oid,
            columns: cols,
            unique: constraint.is_some(),
            primary: constraint.is_some_and(|c| c.1),
            constraint: constraint.map(|(o, _)| NewConstraint {
                oid: o,
                name: name.into(),
            }),
            stats: EMPTY_INDEX_STATS,
        }
    }

    /// `t(a int default 1, b varchar(10) default 'x', c text)` + 2 CHECKs（M2 のテストの表）。
    pub(crate) fn plain_spec(e: &Env, name: &str) -> NewTable {
        let table_oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        let columns = vec![
            col("a", 1, SqlType::INT4, true, Some("1")),
            col("b", 2, SqlType::varchar(10), false, Some("'x'")),
            col("c", 3, SqlType::TEXT, false, None),
        ];
        let checks = vec![
            CheckDef {
                name: format!("{name}_c_check"),
                expr_sql: "c <> ''".into(),
                no_inherit: false,
            },
            CheckDef {
                name: format!("{name}_a_check"),
                expr_sql: "a > 0".into(),
                no_inherit: false,
            },
        ];
        let (attrdef_oids, constraint_oids) = e
            .store
            .allocate_child_oids(&e.oids, &columns, &checks)
            .unwrap();
        NewTable {
            checks,
            attrdef_oids,
            constraint_oids,
            ..NewTable::plain(
                table_oid,
                oids::NAMESPACE_PUBLIC,
                name,
                oids::BOOTSTRAP_SUPERUSER,
                columns,
            )
        }
    }

    /// 表のファイルを作り、カタログの行を書く。
    pub(crate) fn write_table(e: &Env, spec: &NewTable) {
        let locator = RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: DB,
            rel_number: RelFileNumber(spec.oid),
        };
        e.fake.create_storage(&wctx(), locator).unwrap();
        for i in &spec.indexes {
            let l = RelFileLocator {
                spc_oid: DEFAULTTABLESPACE_OID,
                db_oid: DB,
                rel_number: RelFileNumber(i.relfilenode),
            };
            e.fake.create_storage(&wctx(), l).unwrap();
        }
        e.store.create_table(&wctx(), &snap(), spec).unwrap();
    }

    /// 表 `name`（`PRIMARY KEY (id)` + `UNIQUE (name)`）。列は `id int`・`name text`・`n int`。
    pub(crate) fn pk_spec(e: &Env, name: &str) -> NewTable {
        let oids_ = e
            .store
            .allocate_table_oids(
                &e.oids,
                &TableOidRequest {
                    n_attrdefs: 0,
                    n_checks: 0,
                    n_index_constraints: 2,
                },
            )
            .unwrap();
        let cols = |a: i16, ty: SqlType| NewIndexColumn {
            name: format!("c{a}"),
            column: IndexColumn {
                attnum: a,
                opclass: if ty == SqlType::TEXT { 3126 } else { 1978 },
                opfamily: if ty == SqlType::TEXT { 1994 } else { 1976 },
                descending: false,
                nulls_first: false,
            },
            ty,
        };
        let mk = |i: usize, pname: String, primary: bool, c: NewIndexColumn| {
            let (index, con) = oids_.index_constraints[i];
            NewIndex {
                oid: index,
                name: pname.clone(),
                namespace: oids::NAMESPACE_PUBLIC,
                owner: oids::BOOTSTRAP_SUPERUSER,
                table_oid: oids_.table,
                relfilenode: index,
                columns: vec![c],
                unique: true,
                primary,
                constraint: Some(NewConstraint {
                    oid: con,
                    name: pname,
                }),
                stats: EMPTY_INDEX_STATS,
            }
        };
        NewTable {
            indexes: vec![
                mk(0, format!("{name}_pkey"), true, cols(1, SqlType::INT4)),
                mk(1, format!("{name}_name_key"), false, cols(2, SqlType::TEXT)),
            ],
            ..NewTable::plain(
                oids_.table,
                oids::NAMESPACE_PUBLIC,
                name,
                oids::BOOTSTRAP_SUPERUSER,
                vec![
                    col("id", 1, SqlType::INT4, true, None),
                    col("name", 2, SqlType::TEXT, false, None),
                    col("n", 3, SqlType::INT4, false, None),
                ],
            )
        }
    }

    /// 全カタログの、この OID を指す行の数（DROP の取りこぼしの検査）。
    pub(crate) fn rows_mentioning(e: &Env, oid: Oid) -> usize {
        let s = snap();
        let n = |cat: Oid, cols: &[&str]| {
            let idx: Vec<usize> = cols.iter().map(|c| column_index(cat, c)).collect();
            e.store
                .scan(&s, cat, |r| {
                    Ok(idx.iter().any(|i| r[*i] == Datum::Oid(oid)))
                })
                .unwrap()
                .len()
        };
        n(oids::PG_CLASS, &["oid"])
            + n(oids::PG_ATTRIBUTE, &["attrelid"])
            + n(oids::PG_ATTRDEF, &["adrelid", "oid"])
            + n(oids::PG_CONSTRAINT, &["conrelid", "conindid", "oid"])
            + n(oids::PG_INDEX, &["indrelid", "indexrelid"])
            + n(oids::PG_SEQUENCE, &["seqrelid"])
            + n(oids::PG_DEPEND, &["objid", "refobjid"])
    }
}

#[cfg(test)]
mod tests {
    use super::fake_store::FakeStore;
    use super::test_env::*;
    use super::*;
    use crate::control::{ControlData, ControlFileHandle};
    use crate::storage::vfs::{SimVfs, Vfs};

    fn count(e: &Env, catalog: Oid) -> usize {
        e.fake.physical_rows(e.store.rel(catalog).unwrap().locator)
    }

    fn load(e: &Env, oid: Oid) -> TableDef {
        e.store.load_table_def(&snap(), oid).unwrap().unwrap()
    }

    #[test]
    fn bootstrap_fills_the_catalogs() {
        let e = env();
        let any = snapshot_any();
        assert_eq!(count(&e, oids::PG_CLASS), 22);
        assert_eq!(count(&e, oids::PG_NAMESPACE), 4);
        assert_eq!(
            count(&e, oids::PG_TYPE),
            crate::catalog::builtin::TYPES.len()
        );
        assert_eq!(count(&e, oids::PG_LANGUAGE), 3);
        assert_eq!(count(&e, oids::PG_OPCLASS), opclass::OPCLASSES.len());
        for empty in [
            oids::PG_ATTRDEF,
            oids::PG_INDEX,
            oids::PG_DEPEND,
            oids::PG_SEQUENCE,
        ] {
            assert_eq!(count(&e, empty), 0);
        }
        assert_eq!(e.store.namespace_oid(&any, "public").unwrap(), Some(2200));
        assert_eq!(e.store.namespace_oid(&snap(), "nope").unwrap(), None);
        assert_eq!(
            e.store.namespace_name(&snap(), 2200).unwrap().as_deref(),
            Some("public")
        );
        assert_eq!(
            e.store.lookup_relation(&snap(), 11, "pg_index").unwrap(),
            Some(oids::PG_INDEX)
        );
        assert_eq!(
            e.store.lookup_relation(&snap(), 2200, "pg_class").unwrap(),
            None
        );
        // mapped catalogs keep their pinned file in the relation row
        let row = e
            .store
            .relation_row(&snap(), oids::PG_CLASS)
            .unwrap()
            .unwrap();
        assert_eq!(
            row.locator,
            schema::catalog_def(oids::PG_CLASS).unwrap().locator(DB)
        );
        assert_eq!(row.kind, RelKind::Table);
    }

    #[test]
    fn shared_catalogs_are_read_through_the_shared_store() {
        let e = env();
        let s = snap();
        let db = e.shared.database_by_name(&s, "postgres").unwrap().unwrap();
        assert_eq!(db.oid, 5);
        assert!(db.allow_conn && !db.is_template);
        let t0 = e.shared.database_by_name(&s, "template0").unwrap().unwrap();
        assert!(!t0.allow_conn && t0.is_template);
        assert!(e.shared.database_by_name(&s, "nope").unwrap().is_none());
        let role = e.shared.role_by_name(&s, "postgres").unwrap().unwrap();
        assert!(role.superuser && role.can_login);
        assert_eq!(
            e.shared.role_name(&s, 6171).unwrap().as_deref(),
            Some("pg_database_owner")
        );
        assert_eq!(e.shared.role_name(&s, 99_999).unwrap(), None);
        assert_eq!(e.shared.defs().len(), 3);
        let other = CatalogStore::new(1, e.fake.clone());
        assert_eq!(
            other.nailed_def(oids::PG_DATABASE).unwrap().locator,
            e.shared.defs()[0].locator
        );
    }

    #[test]
    fn create_then_load_round_trips() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        let def = load(&e, spec.oid);
        assert_eq!(
            (def.name.as_str(), def.schema.as_str(), def.namespace),
            ("t", "public", 2200)
        );
        assert_eq!(def.kind, RelKind::Table);
        assert_eq!(def.locator.rel_number, RelFileNumber(spec.oid));
        assert_eq!(def.columns, spec.columns);
        let names: Vec<_> = def.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["t_a_check", "t_c_check"]);
        assert!(def.indexes.is_empty() && def.sequence.is_none() && def.identity_seqs.is_empty());
        assert!(e.store.load_table_def(&snap(), 99_999).unwrap().is_none());
        // CHECK -> table and default -> column dependencies
        let deps = e
            .store
            .dependents_of(&snap(), ObjectAddress::relation(spec.oid))
            .unwrap();
        assert_eq!(deps.len(), 4);
        assert!(deps.iter().all(|d| d.deptype == DependType::Auto));
        let col_deps = e
            .store
            .dependents_of(&snap(), ObjectAddress::column(spec.oid, 2))
            .unwrap();
        assert_eq!(col_deps.len(), 1);
        assert_eq!(col_deps[0].dependent.class_id, classes::ATTRDEF);
    }

    #[test]
    fn pk_unique_check_and_default_round_trip() {
        let e = env();
        let spec = pk_spec(&e, "p");
        write_table(&e, &spec);
        let def = load(&e, spec.oid);
        assert_eq!(def.indexes.len(), 2);
        let (pk, uq) = (&def.indexes[0], &def.indexes[1]);
        assert!(pk.oid < uq.oid, "indexes come in OID order");
        assert_eq!(
            (pk.name.as_str(), pk.unique, pk.primary),
            ("p_pkey", true, true)
        );
        assert_eq!(
            (uq.name.as_str(), uq.unique, uq.primary),
            ("p_name_key", true, false)
        );
        assert_eq!(pk.columns[0].opclass, 1978);
        assert_eq!(pk.columns[0].opfamily, 1976);
        assert_eq!(uq.columns[0].opclass, 3126);
        assert_eq!(uq.columns[0].opfamily, 1994);
        let c = pk.constraint.as_ref().unwrap();
        assert_eq!(c.name, "p_pkey");
        assert!(pk.is_constraint_index());
        assert_eq!(def.primary_key().unwrap().oid, pk.oid);
        assert_eq!(pk.locator.rel_number, RelFileNumber(pk.oid));
        // the table row says relhasindex from the start
        let rel = e.store.relation_row(&snap(), spec.oid).unwrap().unwrap();
        assert!(rel.has_index);
        // pg_index: collation, class, option
        let ix = e
            .store
            .scan(&snap(), oids::PG_INDEX, |r| Ok(r[0] == Datum::Oid(uq.oid)))
            .unwrap();
        let ic = |n: &str| column_index(oids::PG_INDEX, n);
        assert_eq!(ix[0].row[ic("indcollation")], Datum::OidVector(vec![100]));
        assert_eq!(ix[0].row[ic("indkey")], Datum::Int2Vector(vec![2]));
        assert_eq!(ix[0].row[ic("indoption")], Datum::Int2Vector(vec![0]));
        // constraint row
        let con = e.store.constraint_by_oid(&snap(), c.oid).unwrap().unwrap();
        assert_eq!(con.kind, ConstraintKind::PrimaryKey);
        assert_eq!(
            (con.table_oid, con.index_oid, con.columns.clone()),
            (spec.oid, Some(pk.oid), vec![1])
        );
        // the index relation row and attribute
        let irow = e.store.relation_row(&snap(), pk.oid).unwrap().unwrap();
        assert_eq!(irow.kind, RelKind::Index);
        assert_eq!(irow.namespace, 2200);
        assert!(e.store.load_table_def(&snap(), pk.oid).unwrap().is_none());
        assert_eq!(
            e.store.index_owner(&snap(), pk.oid).unwrap(),
            Some(spec.oid)
        );
        assert_eq!(e.store.index_owner(&snap(), spec.oid).unwrap(), None);
        // dependencies: constraint -> column (a), index -> constraint (i)
        let refs = e
            .store
            .references_of(&snap(), ObjectAddress::relation(pk.oid))
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].deptype, DependType::Internal);
        assert_eq!(refs[0].referenced, ObjectAddress::constraint(c.oid));
        let refs = e
            .store
            .references_of(&snap(), ObjectAddress::constraint(c.oid))
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].referenced, ObjectAddress::column(spec.oid, 1));
        assert_eq!(
            e.store.constraint_names_of(&snap(), spec.oid).unwrap(),
            ["p_pkey", "p_name_key"]
        );
        assert!(
            e.store
                .constraint_name_exists(&snap(), 2200, "p_pkey")
                .unwrap()
        );
        assert!(
            !e.store
                .constraint_name_exists(&snap(), 11, "p_pkey")
                .unwrap()
        );
    }

    #[test]
    fn index_columns_keep_flags_and_names() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        let mut idx = new_index(
            &e,
            spec.oid,
            "t_ac_idx",
            vec![int4_col(1, true, true), int4_col(1, false, false)],
            None,
        );
        idx.columns[1].name = "c1_1".into();
        e.fake
            .create_storage(
                &wctx(),
                RelFileLocator {
                    spc_oid: DEFAULTTABLESPACE_OID,
                    db_oid: DB,
                    rel_number: RelFileNumber(idx.oid),
                },
            )
            .unwrap();
        // a later command: the table row exists for the snapshot
        e.store.create_index(&wctx(), &snap(), &idx, true).unwrap();
        let def = load(&e, spec.oid);
        let i = &def.indexes[0];
        assert!(!i.unique && !i.primary && i.constraint.is_none());
        assert_eq!(
            (i.columns[0].descending, i.columns[0].nulls_first),
            (true, true)
        );
        assert_eq!(
            (i.columns[1].descending, i.columns[1].nulls_first),
            (false, false)
        );
        assert_eq!(i.indoption(0), 3);
        assert!(
            e.store
                .relation_row(&snap(), spec.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        // plain index: one dependency per distinct column, none on the table itself
        let refs = e
            .store
            .references_of(&snap(), ObjectAddress::relation(idx.oid))
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].referenced, ObjectAddress::column(spec.oid, 1));
        assert_eq!(refs[0].deptype, DependType::Auto);
        // attribute rows: no system columns, attnotnull false
        let attrs = e
            .store
            .scan(&snap(), oids::PG_ATTRIBUTE, |r| {
                Ok(r[0] == Datum::Oid(idx.oid))
            })
            .unwrap();
        assert_eq!(attrs.len(), 2);
        let nn = column_index(oids::PG_ATTRIBUTE, "attnotnull");
        assert!(attrs.iter().all(|t| t.row[nn] == Datum::Bool(false)));
    }

    #[test]
    fn add_constraint_sets_not_null_and_relhasindex() {
        let e = env();
        let mut spec = plain_spec(&e, "t");
        spec.columns[0].not_null = false;
        spec.columns[0].default = None;
        spec.attrdef_oids.remove(0);
        spec.columns[1].default = None;
        spec.attrdef_oids.clear();
        write_table(&e, &spec);
        assert!(
            !e.store
                .relation_row(&snap(), spec.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        let con_oid = e.store.get_new_oid(&e.oids, oids::PG_CONSTRAINT).unwrap();
        let idx = new_index(
            &e,
            spec.oid,
            "t_pkey",
            vec![int4_col(1, false, false)],
            Some((con_oid, true)),
        );
        e.fake
            .create_storage(
                &wctx(),
                RelFileLocator {
                    spc_oid: DEFAULTTABLESPACE_OID,
                    db_oid: DB,
                    rel_number: RelFileNumber(idx.oid),
                },
            )
            .unwrap();
        let table = load(&e, spec.oid);
        assert!(!table.columns[0].not_null);
        e.store
            .add_constraint(&wctx(), &snap(), &idx, &table)
            .unwrap();
        let after = load(&e, spec.oid);
        assert!(after.columns[0].not_null);
        assert!(after.primary_key().is_some());
        assert!(
            e.store
                .relation_row(&snap(), spec.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        // without a constraint add_constraint refuses
        let plain = new_index(&e, spec.oid, "x", vec![int4_col(1, false, false)], None);
        assert!(
            e.store
                .add_constraint(&wctx(), &snap(), &plain, &table)
                .is_err()
        );
    }

    #[test]
    fn create_rejects_inconsistent_specs() {
        let e = env();
        let mut spec = plain_spec(&e, "t1");
        spec.attrdef_oids.pop();
        assert_eq!(
            e.store
                .create_table(&wctx(), &snap(), &spec)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        let mut spec = plain_spec(&e, "t2");
        spec.columns[1].attnum = 5;
        assert!(e.store.create_table(&wctx(), &snap(), &spec).is_err());
        let mut spec = plain_spec(&e, "t3");
        spec.columns[0].ty = SqlType::of(99_999);
        assert!(e.store.create_table(&wctx(), &snap(), &spec).is_err());
        let mut spec = pk_spec(&e, "t4");
        spec.indexes[0].table_oid += 1;
        assert!(e.store.create_table(&wctx(), &snap(), &spec).is_err());
    }

    #[test]
    fn allocate_table_oids_follows_the_documented_order() {
        let e = env();
        let o = e
            .store
            .allocate_table_oids(
                &e.oids,
                &TableOidRequest {
                    n_attrdefs: 1,
                    n_checks: 1,
                    n_index_constraints: 2,
                },
            )
            .unwrap();
        let mut all = vec![o.table, o.attrdefs[0], o.checks[0]];
        for (i, c) in &o.index_constraints {
            all.push(*i);
            all.push(*c);
        }
        let sorted = {
            let mut s = all.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(all, sorted, "table, attrdef, check, (index, constraint)...");
        let mut dedup = all.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), 7);
    }

    #[test]
    fn drop_objects_removes_every_row_of_a_table_with_indexes() {
        let e = env();
        let spec = pk_spec(&e, "p");
        let keep = pk_spec(&e, "keep");
        write_table(&e, &spec);
        write_table(&e, &keep);
        let ids: Vec<Oid> = std::iter::once(spec.oid)
            .chain(spec.indexes.iter().map(|i| i.oid))
            .chain(
                spec.indexes
                    .iter()
                    .filter_map(|i| i.constraint.as_ref().map(|c| c.oid)),
            )
            .collect();
        for id in &ids {
            assert!(rows_mentioning(&e, *id) > 0);
        }
        let before_keep = rows_mentioning(&e, keep.oid);
        let plan = plan_drop(
            &e.store,
            &snap(),
            &[ObjectAddress::relation(spec.oid)],
            DropBehavior::Restrict,
            &[2200, 11],
        )
        .unwrap();
        assert_eq!(plan.items.len(), 5, "table, 2 indexes, 2 constraints");
        assert!(plan.cascaded.is_empty());
        let locators = e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        assert_eq!(locators.len(), 3);
        for id in &ids {
            assert_eq!(rows_mentioning(&e, *id), 0, "rows left for {id}");
        }
        assert_eq!(rows_mentioning(&e, keep.oid), before_keep);
        assert_eq!(load(&e, keep.oid).indexes.len(), 2);
        assert!(e.store.load_table_def(&snap(), spec.oid).unwrap().is_none());
        // the deleted rows are still in the file for a "see everything" scan
        let any = e
            .store
            .scan(&snapshot_any(), oids::PG_CLASS, |r| {
                Ok(r[0] == Datum::Oid(spec.oid))
            })
            .unwrap();
        assert_eq!(any.len(), 1);
    }

    #[test]
    fn legacy_drop_table_cascades_to_the_parts() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        let def = load(&e, spec.oid);
        e.store.drop_table(&wctx(), &snap(), &def).unwrap();
        assert_eq!(rows_mentioning(&e, spec.oid), 0);
        assert!(e.store.load_table_def(&snap(), spec.oid).unwrap().is_none());
    }

    #[test]
    fn dropping_a_system_catalog_is_refused() {
        let e = env();
        let def = load(&e, oids::PG_CLASS);
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
        let def = load(&e, oids::PG_TYPE);
        assert_eq!((def.name.as_str(), def.columns.len()), ("pg_type", 32));
        assert!(def.is_system_catalog());
        let idx = load(&e, oids::PG_INDEX);
        assert_eq!(idx.columns.len(), 21);
        assert!(idx.indexes.is_empty() && idx.sequence.is_none());
    }

    #[test]
    fn sequences_and_identity_round_trip() {
        let e = env();
        let table_oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        let seq_oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        let mut id = col("id", 1, SqlType::INT4, true, None);
        id.identity = Some(IdentityKind::ByDefault);
        let serial = col("s", 2, SqlType::INT4, true, None);
        let spec = NewTable::plain(table_oid, 2200, "ids", 10, vec![id, serial]);
        let params = SequenceParams {
            type_oid: oid::INT4,
            start: 1,
            increment: 1,
            min: 1,
            max: 2_147_483_647,
            cache: 1,
            cycle: false,
            owned_by: Some((table_oid, 1)),
        };
        let seq = NewSequence {
            oid: seq_oid,
            name: "ids_id_seq".into(),
            namespace: 2200,
            owner: 10,
            params,
            owned_by_deptype: Some(DependType::Internal),
        };
        write_table(&e, &spec);
        e.store.create_sequence(&wctx(), &snap(), &seq).unwrap();
        // the identity column must have its sequence
        let def = load(&e, table_oid);
        assert_eq!(def.identity_seqs, vec![(1, seq_oid)]);
        assert_eq!(def.columns[0].identity, Some(IdentityKind::ByDefault));
        assert_eq!(
            e.store.owned_sequences(&snap(), table_oid).unwrap(),
            vec![seq_oid]
        );
        let s = load(&e, seq_oid);
        assert_eq!(s.kind, RelKind::Sequence);
        assert_eq!(s.sequence, Some(params));
        let names: Vec<_> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["last_value", "log_cnt", "is_called"]);
        let rel = e.store.relation_row(&snap(), seq_oid).unwrap().unwrap();
        assert_eq!(rel.kind, RelKind::Sequence);
        // ALTER SEQUENCE
        let mut changed = params;
        changed.increment = 5;
        changed.cycle = true;
        let s2 = Snapshot { ..snap() };
        e.store
            .update_sequence_params(&wctx(), &s2, seq_oid, &changed)
            .unwrap();
        assert_eq!(load(&e, seq_oid).sequence, Some(changed));
        // dropping the table takes the owned sequence with it
        let plan = plan_drop(
            &e.store,
            &snap(),
            &[ObjectAddress::relation(table_oid)],
            DropBehavior::Restrict,
            &[2200],
        )
        .unwrap();
        assert!(plan.items.iter().any(|i| i.kind == DropKind::Sequence));
        e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        assert_eq!(rows_mentioning(&e, seq_oid), 0);
        assert_eq!(rows_mentioning(&e, table_oid), 0);
    }

    #[test]
    fn identity_without_a_sequence_is_corrupt() {
        let e = env();
        let table_oid = e.store.get_new_relation_oid(&e.oids).unwrap();
        let mut id = col("id", 1, SqlType::INT4, true, None);
        id.identity = Some(IdentityKind::Always);
        write_table(&e, &NewTable::plain(table_oid, 2200, "bad", 10, vec![id]));
        let err = e.store.load_table_def(&snap(), table_oid).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DATA_CORRUPTED);
    }

    #[test]
    fn dependency_records_and_filters() {
        let e = env();
        let a = ObjectAddress::relation(20_001);
        let b = ObjectAddress::column(20_002, 3);
        let d = NewDepend {
            dependent: a,
            referenced: b,
            deptype: DependType::Normal,
        };
        e.store
            .record_dependencies(&wctx(), &[d.clone(), d.clone()])
            .unwrap();
        assert_eq!(count(&e, oids::PG_DEPEND), 1, "duplicates are dropped");
        assert_eq!(
            e.store
                .dependents_of(&snap(), ObjectAddress::relation(20_002))
                .unwrap()
                .len(),
            1
        );
        assert!(
            e.store
                .dependents_of(&snap(), ObjectAddress::column(20_002, 4))
                .unwrap()
                .is_empty()
        );
        let other = NewDepend {
            dependent: ObjectAddress::attrdef(20_003),
            referenced: a,
            deptype: DependType::Auto,
        };
        e.store.record_dependency(&wctx(), &other).unwrap();
        let gone = e
            .store
            .delete_dependencies(
                &wctx(),
                &snap(),
                DependFilter::Exact {
                    dependent: a,
                    referenced: b,
                },
            )
            .unwrap();
        assert_eq!(gone, 1);
        let gone = e
            .store
            .delete_dependencies(
                &wctx(),
                &snap(),
                DependFilter::Referenced {
                    class_id: classes::RELATION,
                    obj_id: 20_001,
                },
            )
            .unwrap();
        assert_eq!(gone, 1);
        assert_eq!(
            e.store
                .delete_dependencies(
                    &wctx(),
                    &snap(),
                    DependFilter::Dependent {
                        class_id: 1,
                        obj_id: 1
                    }
                )
                .unwrap(),
            0
        );
    }

    /// D07-3: a row cannot be updated in the command that created it, and not twice in one command.
    #[test]
    fn a_catalog_row_is_not_updated_twice_in_one_command() {
        let e = env();
        let spec = plain_spec(&e, "t");
        let w0 = WriteCtx { xid: OWN, cid: 0 };
        let snap0 = Snapshot {
            own_xid: Some(OWN),
            curcid: 0,
            ..snap()
        };
        e.fake
            .create_storage(
                &w0,
                RelFileLocator {
                    spc_oid: DEFAULTTABLESPACE_OID,
                    db_oid: DB,
                    rel_number: RelFileNumber(spec.oid),
                },
            )
            .unwrap();
        e.store.create_table(&w0, &snap0, &spec).unwrap();
        let patch = ClassPatch {
            relhasindex: Some(true),
            ..ClassPatch::default()
        };
        // same command: invisible
        let err = e
            .store
            .update_class_row(&w0, &snap0, spec.oid, &patch)
            .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        // next command: works once
        let w1 = WriteCtx { xid: OWN, cid: 1 };
        let snap1 = Snapshot {
            own_xid: Some(OWN),
            curcid: 1,
            ..snap()
        };
        e.store
            .update_class_row(&w1, &snap1, spec.oid, &patch)
            .unwrap();
        let err = e
            .store
            .update_class_row(&w1, &snap1, spec.oid, &patch)
            .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        let rel = e
            .store
            .relation_row(
                &Snapshot {
                    curcid: 2,
                    ..snap1.clone()
                },
                spec.oid,
            )
            .unwrap()
            .unwrap();
        assert!(rel.has_index);
        // missing rows are internal errors too
        assert!(
            e.store
                .update_class_row(&w1, &snap1, 99_999, &patch)
                .is_err()
        );
        assert!(
            e.store
                .update_attribute(&w1, &snap1, 99_999, 1, &AttributePatch::default())
                .is_err()
        );
    }

    #[test]
    fn truncate_changes_relfilenode_and_stats() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        let new = e.store.get_new_relfilenumber(&e.oids).unwrap();
        e.store
            .truncate_relation(&wctx(), &snap(), spec.oid, new)
            .unwrap();
        let def = load(&e, spec.oid);
        assert_eq!(def.locator.rel_number, RelFileNumber(new));
        assert_ne!(new, spec.oid);
    }

    #[test]
    fn new_oids_avoid_deleted_and_existing_rows() {
        let e = env();
        let a = plain_spec(&e, "a");
        write_table(&e, &a);
        let def = load(&e, a.oid);
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
        assert!(e.store.get_new_relation_oid(&alloc).unwrap() > fresh);
        let mut data = ControlData::initial(7, 9, 131_072);
        data.next_oid = a.attrdef_oids[0];
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(3));
        let alloc2 = OidAllocator::new(Arc::new(ControlFileHandle::create(&vfs, &data).unwrap()));
        let got = e.store.get_new_oid(&alloc2, oids::PG_ATTRDEF).unwrap();
        assert_ne!(got, a.attrdef_oids[0]);
    }

    #[test]
    fn relation_oids_skip_existing_files() {
        let e = env();
        let next = e.oids.next_raw().unwrap() + 1;
        e.fake
            .create_storage(
                &wctx(),
                RelFileLocator {
                    spc_oid: DEFAULTTABLESPACE_OID,
                    db_oid: DB,
                    rel_number: RelFileNumber(next),
                },
            )
            .unwrap();
        let got = e.store.get_new_relation_oid(&e.oids).unwrap();
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
        let _ = FakeStore::new();
    }

    #[test]
    fn relkind_other_than_r_i_s_is_corrupt() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        // rewrite the relkind to 'v'
        let k = column_index(oids::PG_CLASS, "relkind");
        e.store
            .update_row_where(
                &wctx(),
                &snap(),
                oids::PG_CLASS,
                |r| Ok(r[0] == Datum::Oid(spec.oid)),
                |old| {
                    let mut r = old.clone();
                    r[k] = Datum::Char(b'v');
                    r
                },
            )
            .unwrap();
        let err = e.store.relation_row(&snap(), spec.oid).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DATA_CORRUPTED);
    }
}
