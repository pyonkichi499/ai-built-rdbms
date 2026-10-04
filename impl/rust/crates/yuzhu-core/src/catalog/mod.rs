//! Catalog: table definitions, built-in objects and the read-only view the
//! analyzer and planner use.

pub mod builtin;
pub mod memory;

use std::sync::Arc;

pub use builtin::{
    BuiltinCast, BuiltinFunction, BuiltinOperator, BuiltinType, CastContext, CastMethod,
};

use crate::types::{Oid, SqlType};

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

/// An immutable table definition. Shared via `Arc`; DDL replaces the whole
/// `Arc<TableDef>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    pub oid: Oid,
    /// Always `"public"` in M1.
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
}

impl TableDef {
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
/// Only `table`, `table_by_oid` and `current_database` must be provided;
/// the built-in lookups default to the static tables in `builtin`.
pub trait CatalogReader {
    /// Looks up a table. `schema = None` means "search the `search_path`"
    /// (M1: only `public`; `pg_catalog` tables may be added for stubs).
    fn table(&self, schema: Option<&str>, name: &str) -> Option<Arc<TableDef>>;

    fn table_by_oid(&self, oid: Oid) -> Option<Arc<TableDef>>;

    fn current_database(&self) -> &str;

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
