//! Analyzer: AST + catalog → typed `BoundStatement`.
//!
//! Follows PostgreSQL's parse analysis (`parse_*.c`):
//! - `resolve`: operator / function resolution (`oper`, `func_get_detail`,
//!   `func_select_candidate`).
//! - `coerce`: casts and coercion contexts (`find_coercion_pathway`,
//!   `coerce_to_target_type`, `select_common_type`).
//! - `expr`: expressions (`transformExpr`) and output column names
//!   (`FigureColname`).
//! - `select`: SELECT / VALUES, ORDER BY, DISTINCT, LIMIT.
//! - `dml`: INSERT, stored DEFAULT / CHECK expressions.
//! - `ddl`: CREATE TABLE / DROP TABLE, type names.

pub mod bound;
mod coerce;
mod ddl;
mod dml;
mod expr;
mod resolve;
mod scope;
mod select;

#[cfg(test)]
mod tests;

pub use bound::*;

use crate::catalog::CatalogReader;
use crate::error::{Error, Result};
use crate::sql::ast::Statement;

/// Analyzes SELECT / VALUES / INSERT / CREATE TABLE / DROP TABLE.
/// Other statements (UPDATE, DELETE, EXPLAIN) give 0A000; utility
/// statements handled by the session never reach here.
pub fn analyze(stmt: &Statement, catalog: &dyn CatalogReader) -> Result<BoundStatement> {
    let a = Analyzer { catalog };
    match stmt {
        Statement::Query(q) => Ok(BoundStatement::Select(Box::new(a.analyze_query(q, true)?))),
        Statement::Insert(ins) => a.analyze_insert(ins).map(BoundStatement::Insert),
        Statement::CreateTable(ct) => a.analyze_create_table(ct).map(BoundStatement::CreateTable),
        Statement::DropTable(dt) => a.analyze_drop_table(dt).map(BoundStatement::DropTable),
        Statement::Update(u) => {
            Err(Error::not_supported("UPDATE is not supported yet").with_span(u.span))
        }
        Statement::Delete(d) => {
            Err(Error::not_supported("DELETE is not supported yet").with_span(d.span))
        }
        Statement::Explain(e) => {
            Err(Error::not_supported("EXPLAIN is not supported yet").with_span(e.span))
        }
        Statement::Transaction(_)
        | Statement::Set(_)
        | Statement::Reset(_)
        | Statement::Show(_) => Err(Error::internal("utility statement passed to the analyzer")),
    }
}

/// Analyzes the stored text of a column DEFAULT expression (as kept in
/// `ColumnDef::default`) and coerces it to the column type with
/// assignment semantics. Used for INSERT; exposed for other callers.
pub fn analyze_column_default(
    catalog: &dyn CatalogReader,
    column: &crate::catalog::ColumnDef,
) -> Result<Option<BoundExpr>> {
    Analyzer { catalog }.column_default(column)
}

/// Analyzes the CHECK constraints of a table over its full row.
pub fn analyze_table_checks(
    catalog: &dyn CatalogReader,
    table: &crate::catalog::TableDef,
) -> Result<Vec<BoundCheck>> {
    Analyzer { catalog }.table_checks(table)
}

/// Shared analysis state: the catalog view.
pub(crate) struct Analyzer<'a> {
    pub(crate) catalog: &'a dyn CatalogReader,
}
