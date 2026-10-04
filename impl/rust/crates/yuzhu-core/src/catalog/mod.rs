//! Catalog: table definitions, built-in objects and the read-only view the
//! analyzer and planner use.

pub mod builtin;
pub mod cache;
#[cfg(test)]
pub(crate) mod fake;
pub mod reader;
pub mod rows;
pub mod schema;
pub mod store;

use std::sync::Arc;

pub use builtin::{
    BuiltinCast, BuiltinFunction, BuiltinOperator, BuiltinType, CastContext, CastMethod,
};

use crate::error::Result;
use crate::executor::SessionInfo;
use crate::storage::smgr::RelFileLocator;
use crate::types::ops::BuiltinFn;
use crate::types::{Datum, Oid, SqlType, oid};

/// Source text of a stored expression (DEFAULT). It is parsed and analyzed
/// again every time it is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundExprSource {
    pub expr_sql: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// 1-based.
    pub attnum: i16,
    pub ty: SqlType,
    pub not_null: bool,
    pub default: Option<BoundExprSource>,
}

/// A CHECK constraint, stored as SQL text of its expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckDef {
    pub name: String,
    pub expr_sql: String,
}

/// `pg_class.relkind`. Only ordinary tables in M2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelKind {
    /// `r`. M4 adds indexes and others.
    Table,
}

/// An immutable table definition. Shared via `Arc`; DDL replaces the whole
/// `Arc<TableDef>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    pub oid: Oid,
    /// `pg_class.relnamespace`: 11 = `pg_catalog`, 2200 = `public`.
    pub namespace: Oid,
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    /// Where the table's main fork lives.
    pub locator: RelFileLocator,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
}

impl TableDef {
    /// Tables created by initdb (OID below 16384).
    pub fn is_system_catalog(&self) -> bool {
        self.oid < oid::FIRST_NORMAL_OBJECT_ID
    }

    /// Finds a column by (already case-folded) name.
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// 0-based index of a column by name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// Read-only view of the catalog used by the analyzer and planner.
///
/// The table lookups and the session-related methods must be provided; the
/// built-in lookups default to the static tables in `builtin` (`m2.md` §6.8.5).
pub trait CatalogReader: std::fmt::Debug {
    /// Looks up a table. `schema = None` means "search the `search_path`"
    /// (`pg_catalog` is implicitly first). Errors are I/O or checksum
    /// failures while reading the catalog tables.
    fn table(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>>;

    fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>>;

    fn current_database(&self) -> &str;

    /// The effective `search_path` entries (`"$user"`, `public`, ...).
    fn search_path(&self) -> &[String];

    /// `pg_get_userbyid`: the role name for a role OID.
    fn role_name(&self, oid: Oid) -> Result<Option<String>>;

    /// `pg_table_is_visible`: OIDs of the namespaces on the resolved
    /// `search_path` (`pg_catalog` first).
    fn visible_namespaces(&self) -> Result<Vec<Oid>>;

    fn type_by_oid(&self, oid: Oid) -> Option<&'static BuiltinType> {
        builtin::type_by_oid(oid)
    }

    /// Looks up a type by its `pg_type.typname` (e.g. `int4`, `varchar`).
    /// SQL-standard spellings (`integer`, `character varying`) are
    /// normalized by the parser, as in PostgreSQL's grammar.
    fn type_by_name(&self, name: &str) -> Option<&'static BuiltinType> {
        builtin::type_by_name(name)
    }

    /// The cast from `source` to `target`, if one exists in `pg_cast`.
    /// (Contract 3.3 allows this form instead of `casts_from`.)
    fn find_cast(&self, source: Oid, target: Oid) -> Option<&'static BuiltinCast> {
        builtin::find_cast(source, target)
    }

    fn operators_named(&self, name: &str) -> Vec<&'static BuiltinOperator> {
        builtin::operators_named(name)
    }

    fn functions_named(&self, name: &str) -> Vec<&'static BuiltinFunction> {
        builtin::functions_named(name)
    }
}

/// System columns a SELECT / UPDATE / DELETE may reference (`m2.md` §4.6).
/// Shared by the analyzer and the planner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SystemColumn {
    Ctid,
    Xmin,
    Cmin,
    Xmax,
    Cmax,
    TableOid,
}

/// How a built-in function is implemented (`m2.md` §4.6).
#[derive(Clone, Copy)]
pub enum FnKind {
    /// Depends on the arguments only (the M1 form).
    Pure(BuiltinFn),
    /// Uses the catalog or session (`pg_get_userbyid`, `pg_table_is_visible`,
    /// `format_type`, ...).
    Context(fn(&[Datum], &dyn CatalogReader, &SessionInfo) -> Result<Datum>),
}

impl std::fmt::Debug for FnKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FnKind::Pure(_) => f.write_str("FnKind::Pure(..)"),
            FnKind::Context(_) => f.write_str("FnKind::Context(..)"),
        }
    }
}
