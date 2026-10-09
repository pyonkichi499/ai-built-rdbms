//! ロールの DDL の構文解析（M5 AU-3。`m5/08` §6.5）。
//!
//! `CREATE / ALTER / DROP (ROLE | USER | GROUP)`。属性名（`SUPERUSER` など）は PostgreSQL では
//! 識別子として読まれる非予約語なので、引用符なしの語としてだけ一致させる。

use super::Parser;
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{
    AlterRoleAction, AlterRoleStmt, CreateRoleStmt, DropRoleStmt, RoleKeyword, RoleOption,
    RoleOptionItem, RoleSpec, Secret, Statement,
};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    /// カーソルが `ROLE` / `USER` / `GROUP` にあるか。
    pub(super) fn at_role_keyword(&self) -> bool {
        self.is_kw("role") || self.is_kw("user") || self.is_kw("group")
    }

    fn parse_role_keyword(&mut self) -> Result<RoleKeyword> {
        let kw = match self.peek().keyword() {
            Some("role") => RoleKeyword::Role,
            Some("user") => RoleKeyword::User,
            Some("group") => RoleKeyword::Group,
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(kw)
    }

    fn at_statement_end(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Semicolon | TokenKind::Eof | TokenKind::LexError
        )
    }

    fn skip_rest_of_statement(&mut self) -> Result<()> {
        while !self.at_statement_end() {
            self.advance();
        }
        if matches!(self.peek_kind(), TokenKind::LexError) {
            return Err(self.unexpected());
        }
        Ok(())
    }

    /// `CREATE (ROLE | USER | GROUP) name [WITH] option*`。カーソルは `ROLE` などにある。
    pub(super) fn parse_create_role(&mut self, start: u32) -> Result<Statement> {
        let keyword = self.parse_role_keyword()?;
        let name_start = self.start();
        let name = match self.parse_role_spec()? {
            RoleSpec::Name(id) => id.value,
            RoleSpec::Public => {
                return Err(reserved_name("public").with_span(self.span_from(name_start)));
            }
            other => {
                return Err(Error::new(
                    sqlstate::RESERVED_NAME,
                    format!(
                        "{} cannot be used as a role name here",
                        spec_keyword(&other)
                    ),
                )
                .with_span(self.span_from(name_start)));
            }
        };
        let name_span = self.span_from(name_start);
        self.eat_kw("with");
        let options = self.parse_role_options(true)?;
        Ok(Statement::CreateRole(CreateRoleStmt {
            keyword,
            name,
            name_span,
            options,
            span: self.span_from(start),
        }))
    }

    /// `ALTER (ROLE | USER | GROUP) ...`。カーソルは `ROLE` などにある。
    pub(super) fn parse_alter_role(&mut self, start: u32) -> Result<Statement> {
        let keyword = self.parse_role_keyword()?;
        let target_start = self.start();
        let is_all = self.is_kw("all");
        let target = if is_all {
            self.advance();
            RoleSpec::Public
        } else {
            self.parse_role_spec()?
        };
        let target_span = self.span_from(target_start);
        let action = if is_all {
            if !(self.is_kw("set") || self.is_kw("reset") || self.is_kw("in")) {
                return Err(self.unexpected());
            }
            self.skip_rest_of_statement()?;
            AlterRoleAction::Unsupported("ALTER ROLE ALL SET/RESET")
        } else if self.is_kw("rename") {
            self.advance();
            self.expect_kw("to")?;
            AlterRoleAction::Rename(self.parse_role_name()?)
        } else if self.is_kw("set")
            || self.is_kw("reset")
            || (self.is_kw("in") && self.nth_is_kw(1, "database"))
        {
            self.skip_rest_of_statement()?;
            AlterRoleAction::Unsupported("ALTER ROLE ... SET/RESET")
        } else if keyword == RoleKeyword::Group && (self.is_kw("add") || self.is_kw("drop")) {
            self.skip_rest_of_statement()?;
            AlterRoleAction::Unsupported("ALTER GROUP ... ADD/DROP USER")
        } else if keyword == RoleKeyword::Group {
            return Err(self.unexpected());
        } else {
            self.eat_kw("with");
            AlterRoleAction::Options(self.parse_role_options(false)?)
        };
        Ok(Statement::AlterRole(AlterRoleStmt {
            keyword,
            target,
            target_span,
            action,
            span: self.span_from(start),
        }))
    }

    /// `DROP (ROLE | USER | GROUP) [IF EXISTS] name [, ...]`。カーソルは `ROLE` などにある。
    pub(super) fn parse_drop_role(&mut self, start: u32) -> Result<Statement> {
        let keyword = self.parse_role_keyword()?;
        let if_exists = self.is_kw("if") && self.nth_is_kw(1, "exists");
        if if_exists {
            self.advance();
            self.advance();
        }
        let mut names = Vec::new();
        loop {
            let s = self.start();
            let spec = self.parse_role_spec()?;
            names.push((spec, self.span_from(s)));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::DropRole(DropRoleStmt {
            keyword,
            names,
            if_exists,
            span: self.span_from(start),
        }))
    }

    /// `RENAME TO` などの新しい名前。`public` / `none` / `CURRENT_USER` などは使えない。
    fn parse_role_name(&mut self) -> Result<String> {
        let s = self.start();
        match self.parse_role_spec()? {
            RoleSpec::Name(id) => Ok(id.value),
            RoleSpec::Public => Err(reserved_name("public").with_span(self.span_from(s))),
            other => Err(Error::new(
                sqlstate::RESERVED_NAME,
                format!(
                    "{} cannot be used as a role name here",
                    spec_keyword(&other)
                ),
            )
            .with_span(self.span_from(s))),
        }
    }

    fn parse_role_name_list(&mut self) -> Result<Vec<String>> {
        let mut out = vec![self.parse_role_name()?];
        while self.eat(&TokenKind::Comma) {
            out.push(self.parse_role_name()?);
        }
        Ok(out)
    }

    /// 引用符なしの語（PostgreSQL では識別子の扱いの属性名）。
    fn unquoted_word(&self) -> Option<&str> {
        self.peek().keyword()
    }

    fn parse_sconst(&mut self) -> Result<String> {
        match self.peek_kind().clone() {
            TokenKind::String(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn parse_signed_i32(&mut self) -> Result<i32> {
        let neg = if self.eat_op("-") {
            true
        } else {
            self.eat_op("+");
            false
        };
        let TokenKind::Integer(digits) = self.peek_kind().clone() else {
            return Err(self.unexpected());
        };
        let text = if neg { format!("-{digits}") } else { digits };
        let v = text.parse::<i32>().map_err(|_| self.unexpected())?;
        self.advance();
        Ok(v)
    }

    /// `option*`。`create` が偽なら ALTER ROLE のオプション（`IN ROLE` などは構文エラー）。
    fn parse_role_options(&mut self, create: bool) -> Result<Vec<RoleOptionItem>> {
        let mut out = Vec::new();
        while !self.at_statement_end() {
            let start = self.start();
            let option = self.parse_role_option(create)?;
            out.push(RoleOptionItem {
                option,
                span: self.span_from(start),
            });
        }
        if matches!(self.peek_kind(), TokenKind::LexError) {
            return Err(self.unexpected());
        }
        Ok(out)
    }

    fn parse_role_option(&mut self, create: bool) -> Result<RoleOption> {
        // 引用符つきの語は、キーワードでも属性名でもなく「未知のオプション」になる。
        if let TokenKind::Word {
            value,
            quoted: true,
        } = self.peek_kind().clone()
        {
            return Err(unrecognized(&value).with_span(self.peek().span));
        }
        let Some(word) = self.unquoted_word().map(str::to_owned) else {
            return Err(self.unexpected());
        };
        let flag = |this: &mut Self, opt: RoleOption| {
            this.advance();
            Ok(opt)
        };
        match word.as_str() {
            "superuser" => flag(self, RoleOption::Superuser(true)),
            "nosuperuser" => flag(self, RoleOption::Superuser(false)),
            "createdb" => flag(self, RoleOption::CreateDb(true)),
            "nocreatedb" => flag(self, RoleOption::CreateDb(false)),
            "createrole" => flag(self, RoleOption::CreateRole(true)),
            "nocreaterole" => flag(self, RoleOption::CreateRole(false)),
            "inherit" => flag(self, RoleOption::Inherit(true)),
            "noinherit" => flag(self, RoleOption::Inherit(false)),
            "login" => flag(self, RoleOption::Login(true)),
            "nologin" => flag(self, RoleOption::Login(false)),
            "replication" => flag(self, RoleOption::Replication(true)),
            "noreplication" => flag(self, RoleOption::Replication(false)),
            "bypassrls" => flag(self, RoleOption::BypassRls(true)),
            "nobypassrls" => flag(self, RoleOption::BypassRls(false)),
            "connection" => {
                self.advance();
                self.expect_kw("limit")?;
                Ok(RoleOption::ConnectionLimit(self.parse_signed_i32()?))
            }
            "encrypted" => {
                self.advance();
                self.expect_kw("password")?;
                self.parse_password_value()
            }
            "password" => {
                self.advance();
                self.parse_password_value()
            }
            "unencrypted" => {
                self.advance();
                self.expect_kw("password")?;
                self.parse_sconst()?;
                Ok(RoleOption::UnencryptedPassword)
            }
            "valid" => {
                self.advance();
                self.expect_kw("until")?;
                Ok(RoleOption::ValidUntil(self.parse_sconst()?))
            }
            "sysid" if create => {
                self.advance();
                let TokenKind::Integer(d) = self.peek_kind().clone() else {
                    return Err(self.unexpected());
                };
                let v = d.parse::<i64>().map_err(|_| self.unexpected())?;
                self.advance();
                Ok(RoleOption::Sysid(v))
            }
            "in" if create => {
                self.advance();
                if !(self.eat_kw("role") || self.eat_kw("group")) {
                    return Err(self.unexpected());
                }
                Ok(RoleOption::InRole(self.parse_role_name_list()?))
            }
            "role" if create => {
                self.advance();
                Ok(RoleOption::Role(self.parse_role_name_list()?))
            }
            "user" if create => {
                self.advance();
                Ok(RoleOption::Role(self.parse_role_name_list()?))
            }
            "admin" if create => {
                self.advance();
                Ok(RoleOption::Admin(self.parse_role_name_list()?))
            }
            // PostgreSQL の文法でキーワードの語は、ここでは構文エラー。
            "sysid" | "in" | "role" | "user" | "admin" => Err(self.unexpected()),
            _ => {
                // 予約語や記号は構文エラー、ふつうの識別子は未知のオプション。
                if self.at_col_id() {
                    Err(unrecognized(&word).with_span(self.peek().span))
                } else {
                    Err(self.unexpected())
                }
            }
        }
    }

    fn parse_password_value(&mut self) -> Result<RoleOption> {
        if self.eat_kw("null") {
            return Ok(RoleOption::Password(None));
        }
        Ok(RoleOption::Password(Some(Secret(self.parse_sconst()?))))
    }
}

fn reserved_name(name: &str) -> Error {
    Error::new(
        sqlstate::RESERVED_NAME,
        format!("role name \"{name}\" is reserved"),
    )
}

fn unrecognized(word: &str) -> Error {
    Error::new(
        sqlstate::SYNTAX_ERROR,
        format!("unrecognized role option \"{word}\""),
    )
}

fn spec_keyword(spec: &RoleSpec) -> &'static str {
    match spec {
        RoleSpec::CurrentUser => "CURRENT_USER",
        RoleSpec::CurrentRole => "CURRENT_ROLE",
        RoleSpec::SessionUser => "SESSION_USER",
        _ => "PUBLIC",
    }
}

#[cfg(test)]
mod tests {
    use crate::sql::ast::{AlterRoleAction, RoleKeyword, RoleOption, RoleSpec, Secret, Statement};
    use crate::sql::parser::parse;

    fn one(sql: &str) -> Statement {
        let mut v = parse(sql).expect(sql);
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    fn err(sql: &str) -> crate::error::Error {
        parse(sql).expect_err(sql)
    }

    fn create(sql: &str) -> crate::sql::ast::CreateRoleStmt {
        match one(sql) {
            Statement::CreateRole(c) => c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_role_options() {
        let c = create(
            "CREATE ROLE alice WITH SUPERUSER NOCREATEDB LOGIN CONNECTION LIMIT -1 \
             ENCRYPTED PASSWORD 'pw' VALID UNTIL '2030-01-01' INHERIT NOBYPASSRLS",
        );
        assert_eq!(c.keyword, RoleKeyword::Role);
        assert_eq!(c.name, "alice");
        let opts: Vec<_> = c.options.iter().map(|o| o.option.clone()).collect();
        assert_eq!(
            opts,
            vec![
                RoleOption::Superuser(true),
                RoleOption::CreateDb(false),
                RoleOption::Login(true),
                RoleOption::ConnectionLimit(-1),
                RoleOption::Password(Some(Secret("pw".into()))),
                RoleOption::ValidUntil("2030-01-01".into()),
                RoleOption::Inherit(true),
                RoleOption::BypassRls(false),
            ]
        );
    }

    #[test]
    fn create_user_and_group_and_password_null() {
        assert_eq!(create("create user u").keyword, RoleKeyword::User);
        assert_eq!(create("create group g").keyword, RoleKeyword::Group);
        let c = create("CREATE ROLE r PASSWORD NULL");
        assert_eq!(c.options[0].option, RoleOption::Password(None));
    }

    #[test]
    fn create_membership_options_are_parsed() {
        let c = create("CREATE ROLE r IN ROLE a, b ADMIN c SYSID 5 UNENCRYPTED PASSWORD 'x'");
        let opts: Vec<_> = c.options.iter().map(|o| o.option.clone()).collect();
        assert_eq!(
            opts,
            vec![
                RoleOption::InRole(vec!["a".into(), "b".into()]),
                RoleOption::Admin(vec!["c".into()]),
                RoleOption::Sysid(5),
                RoleOption::UnencryptedPassword,
            ]
        );
    }

    #[test]
    fn unknown_and_quoted_options() {
        let e = err("CREATE ROLE r FOO");
        assert_eq!(e.sqlstate.0, "42601");
        assert!(e.message.contains("unrecognized role option \"foo\""));
        let e = err("CREATE ROLE r \"SUPERUSER\"");
        assert!(e.message.contains("unrecognized role option \"SUPERUSER\""));
    }

    #[test]
    fn reserved_names() {
        assert_eq!(err("CREATE ROLE public").sqlstate.0, "42939");
        assert_eq!(err("CREATE ROLE none").sqlstate.0, "42939");
        let e = err("CREATE ROLE current_user");
        assert_eq!(e.sqlstate.0, "42939");
        assert!(
            e.message
                .contains("CURRENT_USER cannot be used as a role name here")
        );
    }

    #[test]
    fn alter_role_forms() {
        let Statement::AlterRole(a) = one("ALTER USER bob WITH NOLOGIN PASSWORD 'x'") else {
            panic!()
        };
        assert_eq!(a.keyword, RoleKeyword::User);
        assert!(matches!(a.target, RoleSpec::Name(ref i) if i.value == "bob"));
        let AlterRoleAction::Options(o) = a.action else {
            panic!()
        };
        assert_eq!(o.len(), 2);

        let Statement::AlterRole(a) = one("ALTER ROLE CURRENT_USER") else {
            panic!()
        };
        assert_eq!(a.target, RoleSpec::CurrentUser);
        assert_eq!(a.action, AlterRoleAction::Options(vec![]));

        let Statement::AlterRole(a) = one("ALTER ROLE a RENAME TO b") else {
            panic!()
        };
        assert_eq!(a.action, AlterRoleAction::Rename("b".into()));

        let Statement::AlterRole(a) = one("ALTER ROLE a SET search_path = x") else {
            panic!()
        };
        assert!(matches!(a.action, AlterRoleAction::Unsupported(_)));
        let Statement::AlterRole(a) = one("ALTER ROLE ALL SET x = 1") else {
            panic!()
        };
        assert!(matches!(a.action, AlterRoleAction::Unsupported(_)));
        let Statement::AlterRole(a) = one("ALTER GROUP g ADD USER u") else {
            panic!()
        };
        assert!(matches!(a.action, AlterRoleAction::Unsupported(_)));
    }

    #[test]
    fn alter_role_rejects_create_only_options() {
        for sql in [
            "ALTER ROLE a IN ROLE b",
            "ALTER ROLE a SYSID 1",
            "ALTER ROLE a ADMIN b",
        ] {
            let e = err(sql);
            assert_eq!(e.sqlstate.0, "42601", "{sql}");
            assert!(e.message.starts_with("syntax error at or near"), "{sql}");
        }
    }

    #[test]
    fn drop_role() {
        let Statement::DropRole(d) = one("DROP ROLE IF EXISTS a, b") else {
            panic!()
        };
        assert!(d.if_exists);
        assert_eq!(d.names.len(), 2);
        let Statement::DropRole(d) = one("DROP USER current_user") else {
            panic!()
        };
        assert_eq!(d.names[0].0, RoleSpec::CurrentUser);
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let c = create("CREATE ROLE r PASSWORD 'supersecret'");
        assert!(!format!("{c:?}").contains("supersecret"));
    }
}
