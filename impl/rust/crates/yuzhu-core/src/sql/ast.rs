//! Abstract syntax tree. Pure data: no dependency on the catalog.
//!
//! Conventions for the parser:
//! - Every node carries a `Span` (byte offsets into the query text).
//!   For expressions, `span.start` is PostgreSQL's error "location" of the
//!   node and `span.end` the end of the whole expression: infix / postfix
//!   operators (`BinaryOp`, `And`, `Or`, `IsNull`, `IsBool`, `Between`,
//!   `InList`, `Like`, ...) start at the operator token, `x::t` at `::`;
//!   everything else at its first token (see `sql::parser` expr docs).
//! - Unquoted identifiers are folded to lower case; quoted ones are kept.
//! - Operators are kept as PostgreSQL operator strings (`"+"`, `"="`,
//!   `"<>"`, `"||"`, ...) and resolved through the catalog by the analyzer.
//!   `!=` is normalized to `"<>"` (as PostgreSQL's lexer does).
//! - Unary minus applied directly to a numeric literal is folded into the
//!   literal (`Literal::Integer("-2147483648")`), like gram.y's `doNegate`.
//! - Syntax that M1 does not execute (UPDATE, DELETE, JOIN, GROUP BY,
//!   subqueries, PRIMARY KEY, ...) is represented so that the analyzer can
//!   reject it with `0A000` after a successful parse.

pub use crate::error::Span;

/// An identifier (already case-folded unless quoted).
#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub value: String,
    pub quoted: bool,
    pub span: Span,
}

/// A possibly qualified name: `name`, `schema.name`, `db.schema.name`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectName {
    pub parts: Vec<Ident>,
    pub span: Span,
}

impl ObjectName {
    /// The last part (the object's own name).
    pub fn name(&self) -> &Ident {
        self.parts.last().expect("ObjectName has at least one part")
    }

    /// The schema part, if qualified.
    pub fn schema(&self) -> Option<&Ident> {
        let n = self.parts.len();
        (n >= 2).then(|| &self.parts[n - 2])
    }
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    /// SELECT, VALUES, and set operations.
    Query(Box<Query>),
    Transaction(TransactionStmt),
    Set(SetStmt),
    Reset(ResetStmt),
    Show(ShowStmt),
    Explain(Explain),
    /// `CHECKPOINT` (M2).
    Checkpoint(Checkpoint),
    /// `CREATE [UNIQUE] INDEX ...` (M4).
    CreateIndex(CreateIndex),
    /// `DROP INDEX ...` (M4).
    DropIndex(DropIndex),
    /// `ALTER TABLE ...` (M4).
    AlterTable(AlterTable),
    /// `TRUNCATE ...` (M4).
    Truncate(Truncate),
    /// `VACUUM` / `ANALYZE` (M4).
    Vacuum(Vacuum),
    /// `CREATE SEQUENCE ...` (M4).
    CreateSequence(CreateSequence),
    /// `ALTER SEQUENCE ...` (M4).
    AlterSequence(AlterSequence),
    /// `DROP SEQUENCE ...` (M4).
    DropSequence(DropSequence),
    /// `COPY ...` (M4).
    Copy(Copy),
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::CreateTable(s) => s.span,
            Statement::DropTable(s) => s.span,
            Statement::Insert(s) => s.span,
            Statement::Update(s) => s.span,
            Statement::Delete(s) => s.span,
            Statement::Query(s) => s.span,
            Statement::Transaction(s) => s.span,
            Statement::Set(s) => s.span,
            Statement::Reset(s) => s.span,
            Statement::Show(s) => s.span,
            Statement::Explain(s) => s.span,
            Statement::Checkpoint(s) => s.span,
            Statement::CreateIndex(s) => s.span,
            Statement::DropIndex(s) => s.span,
            Statement::AlterTable(s) => s.span,
            Statement::Truncate(s) => s.span,
            Statement::Vacuum(s) => s.span,
            Statement::CreateSequence(s) => s.span,
            Statement::AlterSequence(s) => s.span,
            Statement::DropSequence(s) => s.span,
            Statement::Copy(s) => s.span,
        }
    }
}

// ----- DDL -----------------------------------------------------------------

/// `CREATE TABLE [IF NOT EXISTS] name (elements)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: ObjectName,
    pub if_not_exists: bool,
    /// Columns and table constraints in source order.
    pub elements: Vec<TableElement>,
    /// `WITH (name = value, ...)` (M4). Validated by the DDL layer.
    pub options: Vec<RelOption>,
    pub span: Span,
}

/// One `name [= value]` of a `WITH (...)` list (storage parameters).
/// `value` is the literal text: a string literal without quotes, a
/// number with its sign, or a word as written (lower-cased if unquoted).
/// `None` means no value was given (treated as `true`).
#[derive(Debug, Clone, PartialEq)]
pub struct RelOption {
    /// `toast` in `toast.autovacuum_enabled`.
    pub namespace: Option<Ident>,
    pub name: Ident,
    pub value: Option<String>,
    /// From the first name part to the end of the value.
    pub span: Span,
}

/// Parameters shared by `PRIMARY KEY` / `UNIQUE` constraints (M4). The
/// analyzer rejects the unsupported ones with `0A000`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexParams {
    /// `INCLUDE (cols)`.
    pub include: Vec<Ident>,
    /// `NULLS NOT DISTINCT` (UNIQUE only).
    pub nulls_not_distinct: bool,
    /// `WITH (...)`.
    pub options: Vec<RelOption>,
    /// `USING INDEX TABLESPACE name`.
    pub tablespace: Option<Ident>,
    /// `PRIMARY KEY | UNIQUE USING INDEX name` (ALTER TABLE ADD).
    pub using_index: Option<Ident>,
}

/// The key of a table-level `PRIMARY KEY (cols)` / `UNIQUE (cols)`.
/// `columns` is empty for the `USING INDEX name` form.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyConstraint {
    pub columns: Vec<Ident>,
    pub params: IndexParams,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableElement {
    Column(ColumnDefinition),
    Constraint(TableConstraint),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDefinition {
    pub name: Ident,
    pub type_name: TypeName,
    pub constraints: Vec<ColumnConstraint>,
    pub span: Span,
}

/// An expression together with its exact source text. Used where the
/// expression is stored in the catalog as SQL text (DEFAULT, CHECK).
/// `text` must be the verbatim slice of the query covering `expr`
/// (for CHECK: the part inside the parentheses).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceExpr {
    pub expr: Expr,
    pub text: String,
    /// `CHECK (...) NO INHERIT`（CHECK 以外では常に false）。
    pub no_inherit: bool,
}

/// `[CONSTRAINT name] kind`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnConstraint {
    pub name: Option<Ident>,
    pub kind: ColumnConstraintKind,
    /// `DEFERRABLE` or `INITIALLY DEFERRED` was written (M4). Only kept
    /// for the kinds that may be deferrable (PRIMARY KEY, UNIQUE,
    /// REFERENCES); the analyzer rejects it with `0A000`.
    pub deferrable: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraintKind {
    NotNull,
    Null,
    Default(SourceExpr),
    Check(SourceExpr),
    /// `PRIMARY KEY [WITH (...)] [USING INDEX TABLESPACE t]` (M4).
    PrimaryKey(IndexParams),
    /// `UNIQUE [NULLS [NOT] DISTINCT] [WITH (...)] [USING INDEX TABLESPACE t]` (M4).
    Unique(IndexParams),
    /// `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(sequence options)]` (M4).
    /// `GENERATED ... AS (expr) STORED` stays `0A000` in the parser.
    Identity {
        when: GeneratedWhen,
        options: Vec<SeqOption>,
    },
    /// `REFERENCES table [(column)]`. Not supported in M1 (0A000).
    References {
        table: ObjectName,
        columns: Vec<Ident>,
    },
}

/// `[CONSTRAINT name] kind` at table level.
#[derive(Debug, Clone, PartialEq)]
pub struct TableConstraint {
    pub name: Option<Ident>,
    pub kind: TableConstraintKind,
    /// `DEFERRABLE` or `INITIALLY DEFERRED` was written (M4). Only kept
    /// for PRIMARY KEY, UNIQUE and FOREIGN KEY.
    pub deferrable: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraintKind {
    Check(SourceExpr),
    /// `PRIMARY KEY (cols) [INCLUDE ...] [WITH ...]` (M4).
    PrimaryKey(KeyConstraint),
    /// `UNIQUE [NULLS [NOT] DISTINCT] (cols) [INCLUDE ...] [WITH ...]` (M4).
    Unique(KeyConstraint),
    /// Not supported in M1 (0A000).
    ForeignKey {
        columns: Vec<Ident>,
        ref_table: ObjectName,
        ref_columns: Vec<Ident>,
    },
}

/// A type name as written.
///
/// The parser normalizes SQL-standard keyword spellings to the internal
/// `pg_type.typname`, as gram.y does (single-part `names`):
/// `int`/`integer` → `int4`, `smallint` → `int2`, `bigint` → `int8`,
/// `real` → `float4`, `double precision` → `float8`, `float` → `float8`,
/// `float(p)` → `float4` (p ≤ 24) / `float8` (p ≤ 53; out of range is a
/// 22023 error in the parser, and the modifier is dropped),
/// `boolean` → `bool`, `varchar`/`character varying`/`char varying` →
/// `varchar`, `char`/`character` → `bpchar` (no length means length 1),
/// `decimal`/`dec`/`numeric` → `numeric`, `timestamp [without time zone]`
/// → `timestamp`, `timestamp with time zone` → `timestamptz`, `time ...`
/// → `time`/`timetz`, `interval` → `interval`.
/// Non-keyword names (`int4`, `text`, `"integer"`) are kept as written.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    /// Usually one part; `pg_catalog.int4` gives two.
    pub names: Vec<Ident>,
    /// Type modifiers, e.g. `varchar(10)` → `[Integer("10")]`.
    pub modifiers: Vec<Expr>,
    /// Array bounds: `int[]` → `[None]`, `int[3][]` → `[Some(3), None]`.
    pub array_bounds: Vec<Option<i64>>,
    pub span: Span,
}

/// `DROP TABLE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub behavior: Option<DropBehavior>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropBehavior {
    Cascade,
    Restrict,
}

// ----- DML -----------------------------------------------------------------

/// `INSERT INTO table [AS alias] [(columns)] source [RETURNING ...]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    pub columns: Vec<Ident>,
    /// `OVERRIDING {SYSTEM | USER} VALUE` (M4). Cannot be combined with
    /// `DEFAULT VALUES`.
    pub overriding: Option<OverridingKind>,
    pub source: InsertSource,
    /// Not supported in M1 (0A000) when non-empty.
    pub returning: Vec<SelectItem>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverridingKind {
    System,
    User,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES ...` (body is `QueryBody::Values`, may contain
    /// `Expr::Default`) or `SELECT ...`.
    Query(Box<Query>),
    /// `DEFAULT VALUES`.
    DefaultValues,
}

/// `UPDATE table [AS alias] SET ... [FROM ...] [WHERE ...]` (M2).
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    pub assignments: Vec<Assignment>,
    pub from: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
    pub span: Span,
}

/// `column = value` in UPDATE (value may be `Expr::Default`).
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: Ident,
    /// Field names after the column (`SET t.a = ...` parses with `t` as
    /// `column` and `a` here). Always an error in the analyzer: there are no
    /// composite types.
    pub fields: Vec<Ident>,
    pub value: Expr,
    pub span: Span,
}

/// `DELETE FROM table [AS alias] [USING ...] [WHERE ...]` (M2).
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: ObjectName,
    pub alias: Option<Ident>,
    pub using: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
    pub span: Span,
}

// ----- Queries -------------------------------------------------------------

/// A full query: body plus ORDER BY / LIMIT / OFFSET.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Leading `WITH` (M4). Parenthesized subqueries carry their own.
    pub with: Option<With>,
    pub body: QueryBody,
    pub order_by: Vec<OrderByItem>,
    /// `LIMIT n` / `FETCH FIRST n ROWS ONLY`. `LIMIT ALL` gives `None`.
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub span: Span,
}

/// `WITH [RECURSIVE] cte, ...` (M4).
#[derive(Debug, Clone, PartialEq)]
pub struct With {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
    pub span: Span,
}

/// `name [(columns)] AS [[NOT] MATERIALIZED] (query)` (M4).
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: Ident,
    /// Column aliases `x(a, b)`; empty = none.
    pub columns: Vec<Ident>,
    /// `MATERIALIZED` = `Some(true)`, `NOT MATERIALIZED` = `Some(false)`.
    pub materialized: Option<bool>,
    pub query: Box<Query>,
    /// From the name to the closing parenthesis.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryBody {
    Select(Box<Select>),
    Values(Values),
    /// `left UNION|INTERSECT|EXCEPT [ALL] right` (M4).
    SetOp {
        op: SetOperator,
        all: bool,
        left: Box<QueryBody>,
        right: Box<QueryBody>,
        span: Span,
    },
    /// A parenthesized query with its own ORDER BY / LIMIT:
    /// `(SELECT ... ORDER BY ...) UNION ...`.
    Nested(Box<Query>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

/// `VALUES (row), (row), ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Values {
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: Option<Distinct>,
    pub targets: Vec<SelectItem>,
    /// Comma-separated FROM items (empty = no FROM).
    pub from: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Distinct {
    /// `SELECT DISTINCT`.
    All,
    /// `SELECT DISTINCT ON (exprs)` (not supported in M1).
    On(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `expr [[AS] alias]`.
    Expr {
        expr: Expr,
        alias: Option<Ident>,
        span: Span,
    },
    /// `*`.
    Wildcard(Span),
    /// `t.*` (the qualifier may have several parts).
    QualifiedWildcard(ObjectName, Span),
}

/// A FROM-clause item.
#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: ObjectName,
        alias: Option<TableAlias>,
        span: Span,
    },
    /// `generate_series(1, 3) AS g(x)` (M4). `args` may be empty. With no
    /// alias the analyzer uses the function name.
    Function {
        name: ObjectName,
        args: Vec<Expr>,
        alias: Option<TableAlias>,
        span: Span,
    },
    /// `(SELECT ...) [AS] alias` (M4).
    Subquery {
        query: Box<Query>,
        alias: Option<TableAlias>,
        span: Span,
    },
    /// `left [kind] JOIN right [ON ... | USING (...)]` (M4).
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        constraint: JoinConstraint,
        span: Span,
    },
}

/// `[AS] name [(col, ...)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableAlias {
    pub name: Ident,
    pub columns: Vec<Ident>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<Ident>),
    Natural,
    /// CROSS JOIN.
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    /// `None` when neither ASC nor DESC was written (= ASC).
    pub direction: Option<SortDirection>,
    /// `None` when NULLS FIRST/LAST was not written (default depends on
    /// direction: ASC → NULLS LAST, DESC → NULLS FIRST).
    pub nulls: Option<NullsOrder>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

// ----- Transaction control -------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TransactionStmt {
    pub kind: TransactionKind,
    /// Modes given to BEGIN / START TRANSACTION (ignored or rejected in M1).
    pub modes: Vec<TransactionMode>,
    /// `COMMIT / ROLLBACK ... AND CHAIN` (`AND NO CHAIN` is `false`).
    pub chain: bool,
    pub span: Span,
}

/// Which keyword was used; this determines the command tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionKind {
    /// `BEGIN [WORK|TRANSACTION]` → tag `BEGIN`.
    Begin,
    /// `START TRANSACTION` → tag `START TRANSACTION`.
    StartTransaction,
    /// `COMMIT [WORK|TRANSACTION]` → tag `COMMIT`.
    Commit,
    /// `END [WORK|TRANSACTION]` → tag `COMMIT`.
    End,
    /// `ROLLBACK [WORK|TRANSACTION]` → tag `ROLLBACK`.
    Rollback,
    /// `ABORT [WORK|TRANSACTION]` → tag `ROLLBACK`.
    Abort,
    /// `SAVEPOINT name` (accepted as syntax; the session rejects it).
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`.
    Release(String),
    /// `ROLLBACK [WORK|TRANSACTION] TO [SAVEPOINT] name`.
    RollbackTo(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionMode {
    /// `ISOLATION LEVEL ...`, lower-cased words joined by one space
    /// (e.g. `"read committed"`).
    IsolationLevel(String),
    ReadOnly,
    ReadWrite,
    Deferrable,
    NotDeferrable,
}

// ----- Session -------------------------------------------------------------

/// `SET [SESSION | LOCAL] name {TO | =} value [, ...]`, `SET name TO DEFAULT`,
/// `SET TIME ZONE value`.
///
/// The parser maps special forms to plain parameters: `SET TIME ZONE x` →
/// name `timezone` (`LOCAL`/`DEFAULT` → `SetValue::Default`), `SET NAMES x`
/// → `client_encoding`, `SET SCHEMA 'x'` → `search_path`.
#[derive(Debug, Clone, PartialEq)]
pub struct SetStmt {
    /// `SET LOCAL`.
    pub local: bool,
    /// Lower-cased parameter name; may be dotted (`myapp.flag`).
    pub name: String,
    pub value: SetValue,
    /// `SET TRANSACTION ...` / `SET SESSION CHARACTERISTICS AS TRANSACTION
    /// ...`: all the modes. `name` and `value` hold the equivalent
    /// parameter assignment of the first mode.
    pub transaction: Option<SetTransaction>,
    /// `SET CONSTRAINTS ALL DEFERRED | IMMEDIATE` (a no-op for now).
    pub constraints: bool,
    pub span: Span,
}

/// The modes of `SET TRANSACTION` / `SET SESSION CHARACTERISTICS`.
#[derive(Debug, Clone, PartialEq)]
pub struct SetTransaction {
    /// `SET SESSION CHARACTERISTICS AS TRANSACTION` (sets the defaults).
    pub session_characteristics: bool,
    pub modes: Vec<TransactionMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    /// `TO DEFAULT` / `= DEFAULT`.
    Default,
    /// One or more comma-separated values.
    Values(Vec<SetArg>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetArg {
    /// Identifier or keyword (`on`, `iso`, `"$user"` quoted keeps case).
    Word(String),
    /// String literal content.
    String(String),
    /// Numeric literal text including sign (`-1`, `2.5`).
    Number(String),
}

/// `RESET name` / `RESET ALL`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResetStmt {
    pub target: ParamTarget,
    pub span: Span,
}

/// `SHOW name` / `SHOW ALL`. `SHOW TIME ZONE` → `timezone`,
/// `SHOW TRANSACTION ISOLATION LEVEL` → `transaction_isolation`.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowStmt {
    pub target: ParamTarget,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamTarget {
    All,
    /// Lower-cased name.
    Name(String),
}

/// `CHECKPOINT`.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub span: Span,
}

/// `EXPLAIN [ANALYZE] [VERBOSE] statement` or
/// `EXPLAIN (option [value], ...) statement` (M4).
///
/// The legacy form becomes the options `analyze` / `verbose` without a
/// value. Values are checked by the analyzer, not the parser.
#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    pub options: Vec<ExplainOption>,
    pub statement: Box<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainOption {
    /// The name as lexed (unquoted names are lower case).
    pub name: String,
    pub value: Option<ExplainValue>,
    /// Position of the name (used for error positions).
    pub name_span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExplainValue {
    /// A word (`true`, `off`, `json`, ...) or a quoted string.
    Word(String),
    Integer(i64),
    /// A decimal or an integer that does not fit `i64`: never a valid Boolean.
    Other(String),
}

// ----- M4 DDL: indexes, ALTER TABLE, TRUNCATE, VACUUM ----------------------

/// `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] [name] ON [ONLY] table
/// [USING method] (elems) [INCLUDE (cols)] [NULLS [NOT] DISTINCT]
/// [WITH (...)] [TABLESPACE t] [WHERE expr]`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub name: Option<Ident>,
    pub table: ObjectName,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    pub method: Option<Ident>,
    pub columns: Vec<IndexElem>,
    pub include: Vec<Ident>,
    pub nulls_not_distinct: bool,
    pub options: Vec<RelOption>,
    pub tablespace: Option<Ident>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexElem {
    pub kind: IndexElemKind,
    pub collation: Option<ObjectName>,
    pub opclass: Option<ObjectName>,
    pub direction: Option<SortDirection>,
    pub nulls: Option<NullsOrder>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexElemKind {
    Column(Ident),
    /// A function call or a parenthesized expression (rejected by the analyzer).
    Expr(Expr),
}

/// `DROP INDEX [CONCURRENTLY] [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndex {
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub concurrently: bool,
    pub cascade: bool,
    pub span: Span,
}

/// `ALTER TABLE [IF EXISTS] [ONLY] name action`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub name: ObjectName,
    pub if_exists: bool,
    pub only: bool,
    pub action: AlterTableAction,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    /// `ADD [CONSTRAINT n] PRIMARY KEY | UNIQUE | CHECK | FOREIGN KEY ...`.
    AddConstraint(TableConstraint),
    /// `OWNER TO role`.
    OwnerTo(RoleSpec),
    /// Valid PostgreSQL syntax that is not supported (the analyzer gives
    /// `0A000 ALTER TABLE ... {what} is not supported yet`). The rest of the
    /// statement is skipped unparsed.
    Other { what: String, span: Span },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RoleSpec {
    Name(Ident),
    CurrentUser,
    CurrentRole,
    SessionUser,
    /// `PUBLIC`.
    Public,
}

/// `TRUNCATE [TABLE] [ONLY] name [*] [, ...] [RESTART | CONTINUE IDENTITY] [CASCADE | RESTRICT]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Truncate {
    pub tables: Vec<ObjectName>,
    pub restart_identity: bool,
    pub cascade: bool,
    /// `ONLY` was written for some table.
    pub only: bool,
    pub span: Span,
}

/// `VACUUM [(options) | legacy options] [tables]` (`vacuum = true`) or
/// `ANALYZE [(options) | VERBOSE] [tables]` (`vacuum = false`).
/// Legacy keywords become options without a value (`full`, `freeze`,
/// `verbose`, `analyze`).
#[derive(Debug, Clone, PartialEq)]
pub struct Vacuum {
    pub vacuum: bool,
    pub options: Vec<VacuumOption>,
    pub targets: Vec<VacuumTarget>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VacuumOption {
    pub name: Ident,
    /// Word, string content or signed number text.
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VacuumTarget {
    pub name: ObjectName,
    pub columns: Vec<Ident>,
}

// ----- M4 sequences -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CreateSequence {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub persistence: SeqPersistence,
    pub options: Vec<SeqOption>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqPersistence {
    Permanent,
    Temporary,
    Unlogged,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterSequence {
    pub name: ObjectName,
    pub if_exists: bool,
    pub action: AlterSequenceAction,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterSequenceAction {
    Options(Vec<SeqOption>),
    OwnerTo(RoleSpec),
    /// The analyzer rejects the next two with `0A000`.
    RenameTo(Ident),
    SetSchema(Ident),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropSequence {
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub cascade: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeqOption {
    pub kind: SeqOptionKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SeqOptionKind {
    As(TypeName),
    Increment(SeqNumber),
    /// `None` = `NO MINVALUE`.
    MinValue(Option<SeqNumber>),
    MaxValue(Option<SeqNumber>),
    Start(SeqNumber),
    /// `RESTART [[WITH] n]`.
    Restart(Option<SeqNumber>),
    Cache(SeqNumber),
    /// `CYCLE` = true, `NO CYCLE` = false.
    Cycle(bool),
    /// `OWNED BY NONE` is the one-part name `none`.
    OwnedBy(ObjectName),
    SequenceName(ObjectName),
}

/// A sign (only `-` is kept) and digits (possibly with a decimal point) as
/// text, like PostgreSQL's `NumericOnly`.
#[derive(Debug, Clone, PartialEq)]
pub struct SeqNumber {
    pub text: String,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratedWhen {
    Always,
    ByDefault,
}

// ----- M4 COPY ----------------------------------------------------------------

/// `COPY table [(cols)] FROM|TO source [WITH] options [WHERE expr]`.
/// Legacy options are normalized to `CopyOption`s (`BINARY` -> `format
/// binary`, `CSV` -> `format csv`, ...). `COPY (query) TO` is rejected by
/// the parser with `0A000`.
#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    pub table: ObjectName,
    pub columns: Vec<Ident>,
    pub direction: CopyDirection,
    pub source: CopySource,
    pub options: Vec<CopyOption>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyDirection {
    From,
    To,
}

/// For `TO`, `Stdin` means STDOUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopySource {
    Stdin,
    File(String),
    Program(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopyOption {
    pub name: String,
    pub value: Option<CopyOptionValue>,
    pub name_span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CopyOptionValue {
    /// Unquoted word (`true`, `csv`, ...).
    Word(String),
    /// Quoted string.
    String(String),
    Integer(i64),
    List(Vec<String>),
    Star,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// Integer literal text, digits only (optionally a leading `-` after
    /// folding). The analyzer picks int4 / int8 / numeric by value.
    Integer(String),
    /// Decimal / exponent literal text (`1.5`, `.5`, `1e10`) → numeric.
    Decimal(String),
    /// String constant content after escape processing (type unknown).
    String(String),
    Bool(bool),
    Null,
}

/// SQL-standard niladic "functions" written without parentheses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionValueKind {
    CurrentUser,
    SessionUser,
    CurrentRole,
    /// `USER` (same as `CURRENT_USER`).
    User,
    CurrentCatalog,
    CurrentSchema,
    /// `CURRENT_DATE` (M4).
    CurrentDate,
    /// `CURRENT_TIMESTAMP [(p)]` (M4). `precision` is -1 when not given;
    /// values above 6 are clamped to 6.
    CurrentTimestamp {
        precision: i32,
    },
    /// `LOCALTIMESTAMP [(p)]` (M4). `precision` as for `CurrentTimestamp`.
    LocalTimestamp {
        precision: i32,
    },
    /// 関数 `now()`（`CURRENT_TIMESTAMP` と同じ値。逆変換では `now()` と書く）。
    Now,
    /// 関数 `transaction_timestamp()`。
    TransactionTimestamp,
}

impl SessionValueKind {
    /// Output column name (`FigureColname`).
    pub fn column_name(self) -> &'static str {
        match self {
            SessionValueKind::CurrentUser | SessionValueKind::User => "current_user",
            SessionValueKind::SessionUser => "session_user",
            SessionValueKind::CurrentRole => "current_role",
            SessionValueKind::CurrentCatalog => "current_catalog",
            SessionValueKind::CurrentSchema => "current_schema",
            SessionValueKind::CurrentDate => "current_date",
            SessionValueKind::CurrentTimestamp { .. } => "current_timestamp",
            SessionValueKind::LocalTimestamp { .. } => "localtimestamp",
            SessionValueKind::Now => "now",
            SessionValueKind::TransactionTimestamp => "transaction_timestamp",
        }
    }
}

/// How a cast was written (affects only error messages / column names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastSyntax {
    /// `CAST(x AS t)`.
    Cast,
    /// `x::t`.
    DoubleColon,
    /// `typename 'literal'` (e.g. `int4 '1'`).
    TypedLiteral,
}

/// `IS [NOT] TRUE | FALSE | UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolTestValue {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhenClause {
    pub condition: Expr,
    pub result: Expr,
    pub span: Span,
}

/// `ANY` (and `SOME`) or `ALL` in `x op ANY|ALL (subquery)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    Any,
    All,
}

/// Expressions. `COALESCE` and `NULLIF` are dedicated nodes because they
/// are keywords in PostgreSQL's grammar (not ordinary function calls);
/// `GREATEST`/`LEAST` are `MinMax` nodes for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal {
        value: Literal,
        span: Span,
    },
    /// `col`, `t.col`, `schema.t.col` (1 to 3 parts).
    Column {
        parts: Vec<Ident>,
        span: Span,
    },
    /// `$n` (Extended Query; rejected in Simple Query: 42P02).
    Parameter {
        index: u32,
        span: Span,
    },
    /// Infix operator, e.g. `"+"`, `"="`, `"<>"`, `"||"`.
    BinaryOp {
        op: String,
        /// `OPERATOR(schema.op)`; `None` for an ordinary operator.
        op_schema: Option<Ident>,
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// Prefix operator, e.g. `"-"`, `"+"`.
    UnaryOp {
        op: String,
        op_schema: Option<Ident>,
        expr: Box<Expr>,
        span: Span,
    },
    And {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    Or {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    Not {
        expr: Box<Expr>,
        span: Span,
    },
    /// `x IS [NOT] NULL` (also `ISNULL` / `NOTNULL`).
    IsNull {
        expr: Box<Expr>,
        negated: bool,
        span: Span,
    },
    /// `x IS [NOT] TRUE | FALSE | UNKNOWN`.
    IsBool {
        expr: Box<Expr>,
        value: BoolTestValue,
        negated: bool,
        span: Span,
    },
    /// `x IS [NOT] DISTINCT FROM y` (not in M1).
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
        span: Span,
    },
    Cast {
        expr: Box<Expr>,
        type_name: TypeName,
        syntax: CastSyntax,
        span: Span,
    },
    /// Function call `name(args)`.
    Function {
        name: ObjectName,
        args: Vec<Expr>,
        /// `f(DISTINCT x)`.
        distinct: bool,
        /// `f(*)`, e.g. `count(*)` (`args` is empty).
        star: bool,
        /// `FILTER (WHERE cond)` (M4).
        filter: Option<Box<Expr>>,
        /// `agg(args ORDER BY ...)`。
        order_by: Vec<OrderByItem>,
        span: Span,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<WhenClause>,
        else_result: Option<Box<Expr>>,
        span: Span,
    },
    /// `x [NOT] BETWEEN [SYMMETRIC] low AND high`.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
        symmetric: bool,
        span: Span,
    },
    /// `x [NOT] IN (a, b, ...)`.
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
        span: Span,
    },
    /// `x [NOT] IN (SELECT ...)` (M4).
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
        span: Span,
    },
    /// `x op ANY|SOME|ALL (subquery)` (M4). `op` is the operator spelling
    /// (`=`, `<>`, `~~` for LIKE, `!~~` for NOT LIKE, `~~*`, `!~~*`);
    /// `span.start` is the operator token (`NOT` for NOT LIKE).
    QuantifiedSubquery {
        expr: Box<Expr>,
        op: String,
        op_schema: Option<Ident>,
        quantifier: Quantifier,
        query: Box<Query>,
        span: Span,
    },
    /// `expr COLLATE name` (M4). `span.start` is the `COLLATE` token.
    Collate {
        expr: Box<Expr>,
        collation: ObjectName,
        span: Span,
    },
    /// Row constructor `(a, b)` (2+ items) or `ROW(...)` (`explicit`, any
    /// number of items). `span.start` is `(` or `ROW`. The analyzer accepts
    /// it only as the left side of IN / ANY / ALL subqueries.
    Row {
        items: Vec<Expr>,
        explicit: bool,
        span: Span,
    },
    /// `[NOT] EXISTS (SELECT ...)` (M4; NOT is a separate `Not` node).
    Exists {
        query: Box<Query>,
        span: Span,
    },
    /// Scalar subquery `(SELECT ...)` (M4).
    Subquery {
        query: Box<Query>,
        span: Span,
    },
    /// `x [NOT] LIKE|ILIKE pattern [ESCAPE e]`.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
        case_insensitive: bool,
        span: Span,
    },
    /// `GREATEST(a, ...)` / `LEAST(a, ...)`.
    MinMax {
        greatest: bool,
        args: Vec<Expr>,
        span: Span,
    },
    /// `COALESCE(a, b, ...)`.
    Coalesce {
        args: Vec<Expr>,
        span: Span,
    },
    /// `NULLIF(a, b)`.
    NullIf {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// `CURRENT_USER`, `SESSION_USER`, `CURRENT_CATALOG`, ...
    SessionValue {
        kind: SessionValueKind,
        span: Span,
    },
    /// The `DEFAULT` keyword inside VALUES or UPDATE SET.
    Default {
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal { span, .. }
            | Expr::Column { span, .. }
            | Expr::Parameter { span, .. }
            | Expr::BinaryOp { span, .. }
            | Expr::UnaryOp { span, .. }
            | Expr::And { span, .. }
            | Expr::Or { span, .. }
            | Expr::Not { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::IsBool { span, .. }
            | Expr::IsDistinctFrom { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Function { span, .. }
            | Expr::Case { span, .. }
            | Expr::Between { span, .. }
            | Expr::InList { span, .. }
            | Expr::InSubquery { span, .. }
            | Expr::QuantifiedSubquery { span, .. }
            | Expr::Collate { span, .. }
            | Expr::Row { span, .. }
            | Expr::Exists { span, .. }
            | Expr::Subquery { span, .. }
            | Expr::Like { span, .. }
            | Expr::Coalesce { span, .. }
            | Expr::MinMax { span, .. }
            | Expr::NullIf { span, .. }
            | Expr::SessionValue { span, .. }
            | Expr::Default { span } => *span,
        }
    }
}
