//! Analyzer: AST + catalog → typed `BoundStatement`.
//!
//! Follows PostgreSQL's parse analysis (`parse_*.c`):
//! - `resolve`: operator / function resolution (`oper`, `func_get_detail`,
//!   `func_select_candidate`).
//! - `coerce`: casts and coercion contexts (`find_coercion_pathway`,
//!   `coerce_to_target_type`, `select_common_type`).
//! - `expr`: expressions (`transformExpr`) and output column names
//!   (`FigureColname`).
//! - `select`: SELECT / VALUES, ORDER BY, DISTINCT, LIMIT; `analyze_query`
//!   dispatches WITH / set operations / VALUES / parenthesized queries.
//! - `dml`: INSERT / UPDATE / DELETE, stored DEFAULT / CHECK expressions.
//! - `ddl`: CREATE TABLE / DROP TABLE, type names.
//! - `scope`: `ScopeStack` (name resolution; `Var { rte, col, levels_up }`).
//! - stubs for the M4 features (`from`, `agg`, `sublink`, `setop`, `cte`,
//!   `ddl_constraint`, `ddl_index`): `m4/02` §5.4.
//!
//! The output is [`bound::BoundStatement`] (`m4/00-contracts.md` §7).

mod agg;
pub mod bound;
mod coerce;
mod cte;
mod ddl;
mod ddl_constraint;
mod ddl_index;
mod dml;
mod expr;
mod from;
/// 旧名（P0-d まで）。他担当の `analyzer::query::` の参照が残る間だけの別名。統合で `bound` に置き換えて消す。
pub use bound as query;
mod resolve;
mod scope;
mod select;
mod setop;
mod sublink;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_agg;
#[cfg(test)]
mod tests_from;
#[cfg(test)]
mod tests_sub;
#[cfg(test)]
mod tests_types;

pub use bound::*;

use crate::catalog::CatalogReader;
use crate::error::{Error, Result, Span};
use crate::sql::ast::Statement;

/// `0A000` `<what> is not supported yet`（位置つき）。M3 までの文言と同じ書式。
pub(crate) fn not_supported(what: &str, span: Span) -> Error {
    Error::not_supported(format!("{what} is not supported yet")).with_span(span)
}

/// Analyzes SELECT / VALUES / INSERT / UPDATE / DELETE / CHECKPOINT / CREATE
/// TABLE / DROP TABLE.
/// EXPLAIN gives 0A000; utility
/// statements handled by the session never reach here.
///
/// デバッグビルドでは出力に `BoundStatement::validate`（B1〜B11）を通す。
pub fn analyze(stmt: &Statement, catalog: &dyn CatalogReader) -> Result<bound::BoundStatement> {
    let a = Analyzer { catalog };
    let bound = a.analyze_statement(stmt)?;
    if cfg!(debug_assertions) {
        bound.validate()?;
    }
    Ok(bound)
}

impl Analyzer<'_> {
    fn analyze_statement(&self, stmt: &Statement) -> Result<bound::BoundStatement> {
        use bound::{BoundDdl, BoundStatement};
        match stmt {
            Statement::Query(q) => {
                let root = cte::CteScope::root();
                let env = scope::QueryEnv::root(true, &root);
                Ok(BoundStatement::Select(Box::new(
                    self.analyze_query(q, &env)?,
                )))
            }
            Statement::Insert(ins) => self.analyze_insert(ins).map(BoundStatement::Insert),
            Statement::CreateTable(ct) => self
                .analyze_create_table(ct)
                .map(|b| BoundStatement::Ddl(BoundDdl::CreateTable(b))),
            Statement::DropTable(dt) => self
                .analyze_drop_table(dt)
                .map(|b| BoundStatement::Ddl(BoundDdl::DropTable(b))),
            Statement::Update(u) => self.analyze_update(u).map(BoundStatement::Update),
            Statement::Delete(d) => self.analyze_delete(d).map(BoundStatement::Delete),
            Statement::Checkpoint(_) => Ok(BoundStatement::Checkpoint),
            Statement::Explain(e) => {
                let options = crate::explain::resolve_options(&e.options)?;
                let inner = self.analyze_statement(&e.statement)?;
                Ok(BoundStatement::Explain(Box::new(bound::BoundExplain {
                    options,
                    inner,
                })))
            }
            Statement::CreateSequence(s) => self
                .analyze_create_sequence(s)
                .map(|b| BoundStatement::Ddl(BoundDdl::CreateSequence(b))),
            Statement::AlterSequence(s) => self
                .analyze_alter_sequence(s)
                .map(|b| BoundStatement::Ddl(BoundDdl::AlterSequence(b))),
            Statement::DropSequence(s) => self
                .analyze_drop_sequence(s)
                .map(|b| BoundStatement::Ddl(BoundDdl::DropSequence(b))),
            Statement::CreateIndex(c) => self
                .analyze_create_index(c)
                .map(|b| BoundStatement::Ddl(BoundDdl::CreateIndex(b))),
            Statement::DropIndex(c) => self
                .analyze_drop_index(c)
                .map(|b| BoundStatement::Ddl(BoundDdl::DropIndex(b))),
            Statement::AlterTable(c) => self.analyze_alter_table(c).map(|b| match b {
                ddl_index::BoundAlterTable::AddConstraint(x) => {
                    BoundStatement::Ddl(BoundDdl::AlterTableAddConstraint(x))
                }
                ddl_index::BoundAlterTable::AddCheck(x) => {
                    BoundStatement::Ddl(BoundDdl::AlterTableAddCheck(x))
                }
                ddl_index::BoundAlterTable::Owner(x) => {
                    BoundStatement::Ddl(BoundDdl::AlterTableOwner(x))
                }
            }),
            Statement::Truncate(c) => self
                .analyze_truncate(c)
                .map(|b| BoundStatement::Ddl(BoundDdl::Truncate(b))),
            Statement::Vacuum(c) => self
                .analyze_vacuum(c)
                .map(|b| BoundStatement::Ddl(BoundDdl::Vacuum(b))),
            Statement::Copy(_) => Err(not_supported("this statement", stmt.span())),
            Statement::Transaction(_)
            | Statement::Set(_)
            | Statement::Reset(_)
            | Statement::Show(_) => {
                Err(Error::internal("utility statement passed to the analyzer"))
            }
        }
    }
}

/// Analyzes the stored text of a column DEFAULT expression (as kept in
/// `ColumnDef::default`) and coerces it to the column type with
/// assignment semantics (`Var { rte: 0 }` for the table's columns).
pub fn analyze_column_default_bound(
    catalog: &dyn CatalogReader,
    column: &crate::catalog::ColumnDef,
) -> Result<Option<bound::BoundExpr>> {
    Analyzer { catalog }.column_default(column)
}

/// [`analyze_column_default_bound`] に、IDENTITY 列の暗黙の `nextval(...)` を加えたもの（COPY 用）。
pub fn analyze_column_default_in_table(
    catalog: &dyn CatalogReader,
    table: &crate::catalog::TableDef,
    column: &crate::catalog::ColumnDef,
) -> Result<Option<bound::BoundExpr>> {
    Analyzer { catalog }.column_default_in(table, column)
}

/// Analyzes the CHECK constraints of a table over its full row.
pub fn analyze_table_checks_bound(
    catalog: &dyn CatalogReader,
    table: &crate::catalog::TableDef,
) -> Result<Vec<bound::BoundCheck>> {
    Analyzer { catalog }.table_checks(table)
}

/// Shared analysis state: the catalog view.
pub(crate) struct Analyzer<'a> {
    pub(crate) catalog: &'a dyn CatalogReader,
}
