//! Physical plan (immutable tree).

use crate::analyzer::{BoundCheck, BoundExpr};
use crate::types::{Oid, SqlType};

/// A sort key over the input row of a `Sort` node.
#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: BoundExpr,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    /// FROM-less SELECT: emits exactly one row of `exprs` (evaluated over
    /// an empty row). With no exprs, emits one empty row.
    Result { exprs: Vec<BoundExpr> },
    /// Emits each row of constant-ish expressions (evaluated over an empty
    /// row).
    Values { rows: Vec<Vec<BoundExpr>> },
    /// Full scan of a table in insertion order. `columns` are the table's
    /// column types in attnum order (the row layout).
    SeqScan {
        table_oid: Oid,
        columns: Vec<SqlType>,
    },
    /// Passes rows for which `predicate` is true (NULL/false drop).
    Filter {
        input: Box<PhysicalPlan>,
        predicate: BoundExpr,
    },
    /// Emits `exprs` evaluated over each input row.
    Project {
        input: Box<PhysicalPlan>,
        exprs: Vec<BoundExpr>,
    },
    /// Sorts all input rows (stable) by `keys` using `cmp_datum`.
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<SortKey>,
    },
    /// Removes duplicate rows (NULLs compare equal to each other).
    Distinct { input: Box<PhysicalPlan> },
    /// `limit`/`offset` are int8 expressions evaluated once (NULL = none;
    /// negative → 2201W / 2201X).
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<BoundExpr>,
        offset: Option<BoundExpr>,
    },
    /// Inserts input rows into `table_oid`; emits nothing and counts rows.
    /// Per row: build the table row via `column_map` / `defaults` → NOT
    /// NULL check (`not_null[i]`) → `checks` → `storage.insert` → undo log.
    /// Input rows are already coerced to the column types.
    Insert {
        table_oid: Oid,
        input: Box<PhysicalPlan>,
        /// Per table column: index into the input row, or `None`.
        column_map: Vec<Option<usize>>,
        /// Per table column: default expression (`None` = NULL).
        defaults: Vec<Option<BoundExpr>>,
        checks: Vec<BoundCheck>,
        /// Per table column.
        not_null: Vec<bool>,
    },
}
