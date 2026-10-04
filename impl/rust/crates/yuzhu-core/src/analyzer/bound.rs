//! The analyzer's output: statements with names resolved and every
//! expression typed.

use std::sync::Arc;

use crate::catalog::{
    BuiltinFunction, BuiltinOperator, CastMethod, CheckDef, ColumnDef, SystemColumn, TableDef,
};
use crate::error::Span;
pub use crate::sql::ast::SessionValueKind;
use crate::types::{Datum, Oid, SqlType};

/// Statements that go through analyze → plan → execute (SELECT / VALUES /
/// INSERT), plus DDL whose analysis (type resolution, constraint naming)
/// is done here and whose execution is done by the session.
/// Transaction control and SET / SHOW / RESET are handled by the session
/// straight from the AST.
#[derive(Debug, Clone)]
pub enum BoundStatement {
    Select(Box<BoundSelect>),
    Insert(BoundInsert),
    CreateTable(BoundCreateTable),
    DropTable(BoundDropTable),
    Update(BoundUpdate),
    Delete(BoundDelete),
    /// `CHECKPOINT`: executed by the session, never planned.
    Checkpoint,
}

/// A typed expression.
#[derive(Debug, Clone)]
pub struct BoundExpr {
    pub kind: BoundExprKind,
    /// Result type. For `Literal(Datum::Null)` of undetermined type this is
    /// `unknown` until resolved (resolved to `text` at the latest).
    pub ty: SqlType,
    pub span: Span,
}

impl BoundExpr {
    pub fn new(kind: BoundExprKind, ty: SqlType, span: Span) -> Self {
        BoundExpr { kind, ty, span }
    }
}

/// `IS [NOT] TRUE | FALSE | UNKNOWN` (never returns NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolTestKind {
    IsTrue,
    IsNotTrue,
    IsFalse,
    IsNotFalse,
    IsUnknown,
    IsNotUnknown,
}

#[derive(Debug, Clone)]
pub enum BoundExprKind {
    /// A constant (already converted to the node's type).
    Literal(Datum),
    /// Position of the column in the input row.
    ColumnRef {
        index: usize,
    },
    /// Operator call; operands already coerced to the operator's argument
    /// types. Prefix operators have one argument.
    Operator {
        op: &'static BuiltinOperator,
        args: Vec<BoundExpr>,
    },
    /// Function call; arguments already coerced. Whether NULL arguments
    /// short-circuit is given by `func.strict`.
    Function {
        func: &'static BuiltinFunction,
        args: Vec<BoundExpr>,
    },
    /// Type conversion to the node's `ty.oid` using `method`
    /// (`Binary` = reuse the datum, `InOut` = `output_text` + `input_text`).
    /// Strict: NULL in, NULL out. Any typmod is applied by a separate
    /// `CoerceTypmod` node on top.
    Cast {
        expr: Box<BoundExpr>,
        method: CastMethod,
    },
    /// Length coercion to `ty.typmod` (`varchar(n)`); see
    /// `types::ops::varchar_coerce`. `explicit` = silently truncate.
    CoerceTypmod {
        expr: Box<BoundExpr>,
        explicit: bool,
    },
    /// Three-valued AND / OR over two or more operands.
    And(Vec<BoundExpr>),
    Or(Vec<BoundExpr>),
    Not(Box<BoundExpr>),
    IsNull(Box<BoundExpr>),
    IsNotNull(Box<BoundExpr>),
    BoolTest {
        expr: Box<BoundExpr>,
        test: BoolTestKind,
    },
    /// Searched CASE. A simple CASE (`CASE x WHEN v ...`) is lowered by the
    /// analyzer into conditions `x = v` (the operand is side-effect free in
    /// M1, so evaluating it per arm is fine). Missing ELSE = NULL.
    Case {
        arms: Vec<(BoundExpr, BoundExpr)>,
        else_result: Option<Box<BoundExpr>>,
    },
    /// First non-NULL argument (non-strict, lazily evaluated).
    Coalesce(Vec<BoundExpr>),
    /// NULL if `left = right` (using `eq_op`), else `left`.
    NullIf {
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
        eq_op: &'static BuiltinOperator,
    },
    /// `expr [NOT] LIKE pattern` (C collation; ILIKE folds case).
    /// `escape` defaults to backslash.
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        escape: Option<Box<BoundExpr>>,
        negated: bool,
        case_insensitive: bool,
    },
    /// `expr [NOT] IN (list)`, all coerced to a common type and compared
    /// with `eq_op` (SQL semantics: NULL if no match and any NULL).
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        eq_op: &'static BuiltinOperator,
        negated: bool,
    },
    /// A per-session value, read from `ExecCtx::session`. The analyzer also
    /// lowers `current_database()` to `CurrentCatalog` and
    /// `current_schema()` to `CurrentSchema` (built-in functions cannot see
    /// the session).
    SessionValue(SessionValueKind),
}

/// Output column metadata (feeds `ColumnDesc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: SqlType,
    /// Source table / attnum when the target is a plain column reference,
    /// else 0.
    pub table_oid: Oid,
    pub attnum: i16,
}

/// Where a SELECT's input rows come from.
#[derive(Debug, Clone)]
pub enum BoundFrom {
    /// No FROM: a single empty row.
    None,
    /// A base table; input rows are the table's columns in attnum order.
    Table {
        table: Arc<TableDef>,
        /// Alias if given (for display / EXPLAIN).
        alias: Option<String>,
        /// System columns the query references (appearance order, no
        /// duplicates). A reference is `ColumnRef { index: natts + i }` for
        /// `system_columns[i]`; the scan emits them after the user columns.
        system_columns: Vec<SystemColumn>,
    },
    /// `VALUES` rows (each coerced to the common column types).
    Values {
        rows: Vec<Vec<BoundExpr>>,
        types: Vec<SqlType>,
    },
}

/// Sort key referring to a target entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundSortKey {
    /// Index into `BoundSelect::targets` (may point at a resjunk entry).
    pub target: usize,
    pub descending: bool,
    pub nulls_first: bool,
}

/// SELECT (or a bare VALUES, which is `from = Values` with targets
/// `ColumnRef(0..n)` named `column1..columnN`).
///
/// Evaluation order: rows from `from` → `filter` → compute `targets` →
/// sort by `order_by` → `distinct` → `offset`/`limit` → drop resjunk.
#[derive(Debug, Clone)]
pub struct BoundSelect {
    pub from: BoundFrom,
    /// WHERE (type bool), evaluated over the input row.
    pub filter: Option<BoundExpr>,
    /// Target expressions over the input row. The first `columns.len()` are
    /// visible; any further entries are resjunk (ORDER BY expressions that
    /// are not in the select list).
    pub targets: Vec<BoundExpr>,
    pub columns: Vec<OutputColumn>,
    /// `SELECT DISTINCT` over the visible columns.
    pub distinct: bool,
    pub order_by: Vec<BoundSortKey>,
    /// int8 expressions without column references; NULL = no limit.
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
}

/// A CHECK constraint ready to evaluate over a full table row.
#[derive(Debug, Clone)]
pub struct BoundCheck {
    pub name: String,
    /// Type bool; NULL passes.
    pub expr: BoundExpr,
}

/// INSERT.
#[derive(Debug, Clone)]
pub struct BoundInsert {
    pub table: Arc<TableDef>,
    /// Produces rows whose columns are already coerced (assignment cast +
    /// typmod) to the target columns' types. A VALUES `DEFAULT` is replaced
    /// by the column's default expression (or NULL) here. For `DEFAULT
    /// VALUES` this is a FROM-less select with no targets.
    pub source: Box<BoundSelect>,
    /// `INSERT ... SELECT` whose query has ORDER BY / DISTINCT / LIMIT /
    /// OFFSET: the coercions to the target columns run *above* the whole
    /// source query (a `Project` on top of its `Limit`), on the rows it
    /// actually returns, as in PostgreSQL (such a subquery is not pulled
    /// up). One expression per visible source column, over the source's
    /// output row (`ColumnRef(i)`), giving the value for source position
    /// `i`. When set, `source` produces the *uncoerced* values (its
    /// `columns` keep the query's own types). `None` = `source` already
    /// produces coerced values.
    pub coercions: Option<Vec<BoundExpr>>,
    /// For each table column (attnum order): index into the source row, or
    /// `None` = not listed, use `defaults`.
    pub column_map: Vec<Option<usize>>,
    /// For each table column: default expression (coerced to the column
    /// type, no column refs), `None` = NULL.
    pub defaults: Vec<Option<BoundExpr>>,
    pub checks: Vec<BoundCheck>,
}

/// CREATE TABLE after analysis: types resolved, DEFAULT/CHECK validated,
/// constraint names generated. The session assigns the OID and applies it.
#[derive(Debug, Clone)]
pub struct BoundCreateTable {
    pub schema: String,
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
}

/// DROP TABLE after name resolution.
#[derive(Debug, Clone)]
pub struct BoundDropTable {
    /// Tables to drop. With IF EXISTS, missing names are listed in
    /// `missing` (the session emits `NOTICE: table "x" does not exist,
    /// skipping`); without it, the analyzer raises 42P01.
    pub tables: Vec<Arc<TableDef>>,
    pub missing: Vec<String>,
}

/// Source of an UPDATE assignment.
#[derive(Debug, Clone)]
pub enum UpdateSource {
    /// Evaluated over the old row (the user columns of the input row).
    /// Assignment cast and typmod are already applied.
    Expr(BoundExpr),
    /// `DEFAULT` (`None` = NULL).
    Default(Option<BoundExpr>),
}

/// UPDATE after analysis (`m2.md` §4.7).
#[derive(Debug, Clone)]
pub struct BoundUpdate {
    pub table: Arc<TableDef>,
    /// `(attnum - 1, source)` per assigned column.
    pub assignments: Vec<(usize, UpdateSource)>,
    pub filter: Option<BoundExpr>,
    /// System columns referenced by WHERE and SET (appearance order, no
    /// duplicates, `Ctid` excluded).
    pub system_columns: Vec<SystemColumn>,
    pub checks: Vec<BoundCheck>,
    /// Per table column.
    pub not_null: Vec<bool>,
}

/// DELETE after analysis.
#[derive(Debug, Clone)]
pub struct BoundDelete {
    pub table: Arc<TableDef>,
    pub filter: Option<BoundExpr>,
    pub system_columns: Vec<SystemColumn>,
}
