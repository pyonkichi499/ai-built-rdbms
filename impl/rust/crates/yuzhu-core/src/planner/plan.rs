//! Physical plan (immutable tree).

pub use crate::analyzer::UpdateSource;
use crate::analyzer::{BoundCheck, BoundExpr};
use crate::catalog::SystemColumn;
use crate::storage::RelHandle;
use crate::types::SqlType;

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
    /// Full scan of a table. Output row = the user columns (attnum order,
    /// types in `columns`) followed by `system_columns` in order.
    SeqScan {
        rel: RelHandle,
        columns: Vec<SqlType>,
        system_columns: Vec<SystemColumn>,
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
    /// Inserts input rows into `rel`; emits nothing and counts rows.
    /// Per row: build the table row via `column_map` / `defaults` → NOT
    /// NULL check (`not_null[i]`) → `checks` → `storage.insert`.
    /// Input rows are already coerced to the column types.
    Insert {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
        /// Per table column: index into the input row, or `None`.
        column_map: Vec<Option<usize>>,
        /// Per table column: default expression (`None` = NULL).
        defaults: Vec<Option<BoundExpr>>,
        checks: Vec<BoundCheck>,
        /// Per table column.
        not_null: Vec<bool>,
        /// For error messages.
        table_name: String,
    },
    /// Updates the rows of `input`: the target table's user columns followed
    /// by `ctid` (`Datum::Tid`). Per row: evaluate `assignments` over the old
    /// row, NOT NULL, `checks`, then `storage.update`.
    Update {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
        /// `(attnum - 1, source)`.
        assignments: Vec<(usize, UpdateSource)>,
        checks: Vec<BoundCheck>,
        /// Per table column.
        not_null: Vec<bool>,
        table_name: String,
    },
    /// Deletes the rows of `input` (user columns followed by `ctid`).
    Delete {
        rel: RelHandle,
        input: Box<PhysicalPlan>,
    },
}
