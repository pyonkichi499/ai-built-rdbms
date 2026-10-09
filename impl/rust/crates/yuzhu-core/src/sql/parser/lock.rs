//! `LOCK [TABLE] [ONLY] name [*] [, ...] [IN lockmode MODE] [NOWAIT]` の構文解析（M5 LK-4b。01 §6.4）。
//!
//! PostgreSQL の `gram.y` の `LockStmt`。`ONLY` と `*` は受け付けて無視する（継承がない）。
//! モード省略は `ACCESS EXCLUSIVE`。不正なモードは `42601 syntax error at or near "..."`。

use super::Parser;
use crate::error::Result;
use crate::sql::ast::{LockTableName, LockTableStmt, TableLockMode};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    /// カーソルが `LOCK` にある状態で呼ぶ。
    pub(super) fn parse_lock(&mut self) -> Result<LockTableStmt> {
        let start = self.advance().span.start;
        self.eat_kw("table");
        let mut tables = Vec::new();
        loop {
            let (name, only) = self.parse_relation_name()?;
            tables.push(LockTableName { name, only });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let mut mode = TableLockMode::AccessExclusive;
        if self.eat_kw("in") {
            mode = self.parse_table_lock_mode()?;
            self.expect_kw("mode")?;
        }
        let nowait = self.eat_kw("nowait");
        Ok(LockTableStmt {
            tables,
            mode,
            nowait,
            span: self.span_from(start),
        })
    }

    fn parse_table_lock_mode(&mut self) -> Result<TableLockMode> {
        if self.eat_kw("access") {
            return if self.eat_kw("share") {
                Ok(TableLockMode::AccessShare)
            } else {
                self.expect_kw("exclusive")?;
                Ok(TableLockMode::AccessExclusive)
            };
        }
        if self.eat_kw("row") {
            return if self.eat_kw("share") {
                Ok(TableLockMode::RowShare)
            } else {
                self.expect_kw("exclusive")?;
                Ok(TableLockMode::RowExclusive)
            };
        }
        if self.eat_kw("share") {
            if self.eat_kw("update") {
                self.expect_kw("exclusive")?;
                return Ok(TableLockMode::ShareUpdateExclusive);
            }
            if self.eat_kw("row") {
                self.expect_kw("exclusive")?;
                return Ok(TableLockMode::ShareRowExclusive);
            }
            return Ok(TableLockMode::Share);
        }
        if self.eat_kw("exclusive") {
            return Ok(TableLockMode::Exclusive);
        }
        Err(self.unexpected())
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use crate::error::sqlstate;
    use crate::sql::ast::{LockTableStmt, Statement, TableLockMode};

    fn lock(sql: &str) -> LockTableStmt {
        match parse(sql).unwrap().remove(0) {
            Statement::LockTable(s) => s,
            other => panic!("not LOCK: {other:?}"),
        }
    }

    #[test]
    fn defaults_to_access_exclusive() {
        let s = lock("lock table t");
        assert_eq!(s.mode, TableLockMode::AccessExclusive);
        assert!(!s.nowait);
        assert_eq!(s.tables.len(), 1);
        assert_eq!(s.tables[0].name.name().value, "t");
    }

    #[test]
    fn table_keyword_is_optional() {
        assert_eq!(lock("LOCK t").tables.len(), 1);
    }

    #[test]
    fn all_eight_modes() {
        let cases = [
            ("access share", TableLockMode::AccessShare),
            ("row share", TableLockMode::RowShare),
            ("row exclusive", TableLockMode::RowExclusive),
            (
                "share update exclusive",
                TableLockMode::ShareUpdateExclusive,
            ),
            ("share", TableLockMode::Share),
            ("share row exclusive", TableLockMode::ShareRowExclusive),
            ("exclusive", TableLockMode::Exclusive),
            ("access exclusive", TableLockMode::AccessExclusive),
        ];
        for (words, mode) in cases {
            let s = lock(&format!("lock table t in {words} mode"));
            assert_eq!(s.mode, mode, "{words}");
            let s = lock(&format!("LOCK TABLE t IN {} MODE", words.to_uppercase()));
            assert_eq!(s.mode, mode, "{words}");
        }
    }

    #[test]
    fn only_star_list_and_nowait() {
        let s = lock("lock table only a, public.b *, c in share mode nowait");
        assert_eq!(s.tables.len(), 3);
        assert!(s.tables[0].only);
        assert!(!s.tables[1].only);
        assert_eq!(s.tables[1].name.parts.len(), 2);
        assert_eq!(s.mode, TableLockMode::Share);
        assert!(s.nowait);
        assert!(lock("lock t nowait").nowait);
    }

    #[test]
    fn syntax_errors() {
        for (sql, near) in [
            ("lock table t in bogus mode", "\"bogus\""),
            ("lock table t in share", "end of input"),
            ("lock table t in row mode", "\"mode\""),
            ("lock table", "end of input"),
            ("lock table t,", "end of input"),
            ("lock table t in share mode wait", "\"wait\""),
        ] {
            let e = parse(sql).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR, "{sql}");
            assert!(e.message.contains(near), "{sql}: {}", e.message);
        }
    }

    #[test]
    fn span_covers_the_statement() {
        let s = lock("lock table t nowait");
        assert_eq!((s.span.start, s.span.end), (0, 19));
    }
}
