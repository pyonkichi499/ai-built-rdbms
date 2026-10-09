//! M5 の DDL・ユーティリティ文の `BoundDdl` の組み立て。
//!
//! F0b-2 が `Statement` / `BoundDdl` の変種と、この振り分けを作った。持ち主の章は自分の文の
//! `not_supported` の腕を、自分のモジュールの `analyze` の呼び出しに置き換える。

pub mod database;
pub mod foreign_key;
pub mod lock_table;
pub mod role;
pub mod vacuum;

use super::bound::BoundDdl;
use super::not_supported;
use crate::catalog::CatalogReader;
use crate::error::{Error, Result};
use crate::sql::ast::Statement;

/// M5 の文（LOCK TABLE・ANALYZE・ロール・データベースの DDL）を `BoundDdl` にする。
/// PREPARE / EXECUTE / DEALLOCATE / DISCARD は Session が直接処理するので、ここには来ない。
pub fn analyze(catalog: &dyn CatalogReader, stmt: &Statement) -> Result<BoundDdl> {
    let what = match stmt {
        Statement::LockTable(s) => {
            return lock_table::analyze(catalog, s).map(BoundDdl::LockTable);
        }
        Statement::Analyze(_) => "ANALYZE",
        Statement::CreateRole(s) => return role::analyze_create(s).map(BoundDdl::CreateRole),
        Statement::AlterRole(s) => return role::analyze_alter(s).map(BoundDdl::AlterRole),
        Statement::DropRole(s) => return role::analyze_drop(s).map(BoundDdl::DropRole),
        Statement::CreateDatabase(s) => {
            return database::analyze_create(s).map(BoundDdl::CreateDatabase);
        }
        Statement::DropDatabase(s) => {
            return database::analyze_drop(s).map(BoundDdl::DropDatabase);
        }
        Statement::AlterDatabase(s) => {
            return database::analyze_alter(s).map(BoundDdl::AlterDatabase);
        }
        _ => {
            return Err(Error::internal(
                "statement is not handled by analyzer::ddl_ext",
            ));
        }
    };
    Err(not_supported(what, stmt.span()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Span, sqlstate};

    #[test]
    fn stubs_are_not_supported() {
        use crate::sql::ast::AnalyzeStmt;
        let stmt = Statement::Analyze(AnalyzeStmt {
            span: Span::default(),
        });
        let cat = crate::catalog::fake::FakeCatalog::new("t");
        let e = analyze(&cat, &stmt).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn session_statements_are_internal_errors() {
        let stmt = Statement::Checkpoint(crate::sql::ast::Checkpoint {
            span: Span::default(),
        });
        let cat = crate::catalog::fake::FakeCatalog::new("t");
        assert!(analyze(&cat, &stmt).is_err());
    }
}
