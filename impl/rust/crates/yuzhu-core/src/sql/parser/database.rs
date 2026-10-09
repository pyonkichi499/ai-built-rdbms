//! `CREATE / DROP / ALTER DATABASE` の構文解析（M5 DB-2。`m5/09` §6.1）。
//!
//! オプション名は識別子として読み、未知の名前や値の検証はアナライザが行う。

use super::Parser;
use crate::error::Result;
use crate::sql::ast::{
    AlterDatabaseAction, AlterDatabaseStmt, CreateDatabaseStmt, DatabaseOption,
    DatabaseOptionValue, DropDatabaseStmt, Statement,
};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    fn db_at_end(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Semicolon | TokenKind::Eof | TokenKind::LexError
        )
    }

    fn db_skip_rest(&mut self) -> Result<()> {
        while !self.db_at_end() {
            self.advance();
        }
        if matches!(self.peek_kind(), TokenKind::LexError) {
            return Err(self.unexpected());
        }
        Ok(())
    }

    /// `CREATE DATABASE name [WITH] option*`。カーソルは `DATABASE` にある。
    pub(super) fn parse_create_database(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let name = self.parse_col_id()?.value;
        self.eat_kw("with");
        let options = self.parse_database_options()?;
        Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            name,
            options,
            span: self.span_from(start),
        }))
    }

    /// `DROP DATABASE [IF EXISTS] name [[WITH] (FORCE [, FORCE ...])]`。カーソルは `DATABASE` にある。
    pub(super) fn parse_drop_database(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let if_exists = self.is_kw("if") && self.nth_is_kw(1, "exists");
        if if_exists {
            self.advance();
            self.advance();
        }
        let name = self.parse_col_id()?.value;
        let mut force = false;
        let with = self.eat_kw("with");
        if with || self.peek_kind() == &TokenKind::LParen {
            if !self.eat(&TokenKind::LParen) {
                return Err(self.unexpected());
            }
            loop {
                self.expect_kw("force")?;
                force = true;
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            if !self.eat(&TokenKind::RParen) {
                return Err(self.unexpected());
            }
        }
        Ok(Statement::DropDatabase(DropDatabaseStmt {
            name,
            if_exists,
            force,
            span: self.span_from(start),
        }))
    }

    /// `ALTER DATABASE name ...`。カーソルは `DATABASE` にある。
    pub(super) fn parse_alter_database(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let name = self.parse_col_id()?.value;
        let unsupported = match self.peek().keyword() {
            Some("rename") => Some("RENAME TO"),
            Some("owner") => Some("OWNER TO"),
            Some("set") if self.nth_is_kw(1, "tablespace") => Some("SET TABLESPACE"),
            Some("set") => Some("SET"),
            Some("reset") => Some("RESET"),
            Some("refresh") => Some("REFRESH COLLATION VERSION"),
            _ => None,
        };
        let action = if let Some(what) = unsupported {
            self.db_skip_rest()?;
            AlterDatabaseAction::Unsupported(what)
        } else {
            self.eat_kw("with");
            let options = self.parse_database_options()?;
            if options.is_empty() {
                return Err(self.unexpected());
            }
            AlterDatabaseAction::Options(options)
        };
        Ok(Statement::AlterDatabase(AlterDatabaseStmt {
            name,
            action,
            span: self.span_from(start),
        }))
    }

    fn parse_database_options(&mut self) -> Result<Vec<DatabaseOption>> {
        let mut out = Vec::new();
        while !self.db_at_end() {
            out.push(self.parse_database_option()?);
        }
        if matches!(self.peek_kind(), TokenKind::LexError) {
            return Err(self.unexpected());
        }
        Ok(out)
    }

    fn parse_database_option(&mut self) -> Result<DatabaseOption> {
        let start = self.start();
        let TokenKind::Word { value, .. } = self.peek_kind().clone() else {
            return Err(self.unexpected());
        };
        let unquoted_connection = self.is_kw("connection");
        self.advance();
        let name = if unquoted_connection {
            self.expect_kw("limit")?;
            "connection_limit".to_owned()
        } else {
            value
        };
        self.eat_op("=");
        let value = self.parse_database_option_value()?;
        Ok(DatabaseOption {
            name,
            value,
            span: self.span_from(start),
        })
    }

    fn parse_database_option_value(&mut self) -> Result<DatabaseOptionValue> {
        match self.peek_kind().clone() {
            TokenKind::String(s) => {
                self.advance();
                Ok(DatabaseOptionValue::Str(s))
            }
            TokenKind::Integer(_) | TokenKind::Decimal(_) => self.parse_database_number(false),
            TokenKind::Op(op) if op == "-" || op == "+" => {
                self.advance();
                self.parse_database_number(op == "-")
            }
            TokenKind::Word { quoted: true, .. } => {
                let id = self.parse_col_label()?;
                Ok(DatabaseOptionValue::Str(id.value))
            }
            TokenKind::Word { .. } if self.is_kw("default") => {
                self.advance();
                Ok(DatabaseOptionValue::Default)
            }
            TokenKind::Word { .. } if self.is_kw("true") => {
                self.advance();
                Ok(DatabaseOptionValue::Bool(true))
            }
            TokenKind::Word { .. } if self.is_kw("false") => {
                self.advance();
                Ok(DatabaseOptionValue::Bool(false))
            }
            TokenKind::Word { .. } if self.is_kw("on") => {
                self.advance();
                Ok(DatabaseOptionValue::Ident("on".to_owned()))
            }
            TokenKind::Word { .. } => {
                let id = self.parse_non_reserved_word()?;
                Ok(DatabaseOptionValue::Ident(id.value))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn parse_database_number(&mut self, negative: bool) -> Result<DatabaseOptionValue> {
        let sign = if negative { "-" } else { "" };
        match self.peek_kind().clone() {
            TokenKind::Integer(d) => {
                self.advance();
                Ok(match format!("{sign}{d}").parse::<i64>() {
                    Ok(n) => DatabaseOptionValue::Int(n),
                    Err(_) => DatabaseOptionValue::Float(format!("{sign}{d}")),
                })
            }
            TokenKind::Decimal(d) => {
                self.advance();
                Ok(DatabaseOptionValue::Float(format!("{sign}{d}")))
            }
            _ => Err(self.unexpected()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::sqlstate;
    use crate::sql::ast::{AlterDatabaseAction, DatabaseOptionValue as V, Statement};
    use crate::sql::parser::parse;

    fn one(sql: &str) -> Statement {
        let mut v = parse(sql).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    #[test]
    fn create_with_options() {
        let Statement::CreateDatabase(s) = one(
            "CREATE DATABASE Foo WITH TEMPLATE = template0 ENCODING 'UTF8' CONNECTION LIMIT -1 \
             IS_TEMPLATE true OWNER \"Bob\" LC_COLLATE DEFAULT",
        ) else {
            panic!()
        };
        assert_eq!(s.name, "foo");
        let got: Vec<_> = s
            .options
            .iter()
            .map(|o| (o.name.as_str(), &o.value))
            .collect();
        assert_eq!(
            got,
            vec![
                ("template", &V::Ident("template0".into())),
                ("encoding", &V::Str("UTF8".into())),
                ("connection_limit", &V::Int(-1)),
                ("is_template", &V::Bool(true)),
                ("owner", &V::Str("Bob".into())),
                ("lc_collate", &V::Default),
            ]
        );
    }

    #[test]
    fn create_without_options_and_float() {
        let Statement::CreateDatabase(s) = one("create database x") else {
            panic!()
        };
        assert!(s.options.is_empty());
        let Statement::CreateDatabase(s) = one("create database x connection limit 1.5") else {
            panic!()
        };
        assert_eq!(s.options[0].value, V::Float("1.5".into()));
    }

    #[test]
    fn create_syntax_errors() {
        for sql in [
            "create database x is_template",
            "create database x connection = 3",
            "create database \"\"",
            "create database x owner current_user",
            "create database",
        ] {
            let e = parse(sql).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR, "{sql}");
        }
    }

    #[test]
    fn drop_forms() {
        let Statement::DropDatabase(s) = one("DROP DATABASE IF EXISTS a") else {
            panic!()
        };
        assert_eq!((s.name.as_str(), s.if_exists, s.force), ("a", true, false));
        let Statement::DropDatabase(s) = one("drop database a with (force)") else {
            panic!()
        };
        assert!(s.force);
        let Statement::DropDatabase(s) = one("drop database a (force, force)") else {
            panic!()
        };
        assert!(s.force);
        for sql in [
            "drop database a, b",
            "drop database if exists a, b",
            "drop database a with",
        ] {
            assert_eq!(
                parse(sql).unwrap_err().sqlstate,
                sqlstate::SYNTAX_ERROR,
                "{sql}"
            );
        }
    }

    #[test]
    fn alter_forms() {
        let Statement::AlterDatabase(s) =
            one("alter database a with allow_connections false connection limit 3")
        else {
            panic!()
        };
        let AlterDatabaseAction::Options(o) = s.action else {
            panic!()
        };
        assert_eq!(o.len(), 2);
        for (sql, what) in [
            ("alter database a rename to b", "RENAME TO"),
            ("alter database a owner to b", "OWNER TO"),
            ("alter database a set tablespace x", "SET TABLESPACE"),
            ("alter database a set foo = 1", "SET"),
            ("alter database a reset all", "RESET"),
            (
                "alter database a refresh collation version",
                "REFRESH COLLATION VERSION",
            ),
        ] {
            let Statement::AlterDatabase(s) = one(sql) else {
                panic!()
            };
            assert_eq!(s.action, AlterDatabaseAction::Unsupported(what), "{sql}");
        }
        assert!(parse("alter database a").is_err());
    }
}
