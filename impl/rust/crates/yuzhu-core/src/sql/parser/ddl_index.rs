//! M4 DDL: CREATE / DROP INDEX, ALTER TABLE, TRUNCATE, VACUUM / ANALYZE.

use super::Parser;
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{
    AlterTable, AlterTableAction, CreateIndex, DropBehavior, DropIndex, Expr, Ident, IndexElem,
    IndexElemKind, NullsOrder, RoleSpec, SortDirection, Statement, Truncate, Vacuum, VacuumOption,
    VacuumTarget,
};
use crate::sql::token::TokenKind;

/// First words of `ALTER TABLE` actions that are valid PostgreSQL syntax
/// but not supported (reported by the analyzer as `0A000`).
const ALTER_TABLE_OTHER: &[&str] = &[
    "drop", "alter", "rename", "set", "reset", "enable", "disable", "force", "no", "inherit", "of",
    "not", "cluster", "replica", "attach", "detach", "validate",
];

impl Parser<'_> {
    /// `[IF NOT EXISTS]` (consumed when present).
    fn eat_if_not_exists(&mut self) -> bool {
        let found = self.is_kw("if") && self.nth_is_kw(1, "not") && self.nth_is_kw(2, "exists");
        if found {
            for _ in 0..3 {
                self.advance();
            }
        }
        found
    }

    /// `[IF EXISTS]` (consumed when present).
    pub(super) fn eat_if_exists(&mut self) -> bool {
        let found = self.is_kw("if") && self.nth_is_kw(1, "exists");
        if found {
            self.advance();
            self.advance();
        }
        found
    }

    /// The syntax error for the token that starts at byte offset `start`.
    fn unexpected_at(&self, start: u32) -> Error {
        match self.tokens.iter().find(|t| t.span.start == start) {
            Some(t) => Error::syntax_at(
                t.span,
                format!("syntax error at or near \"{}\"", self.text(t.span)),
            ),
            None => self.unexpected(),
        }
    }

    // ----- CREATE INDEX ------------------------------------------------

    /// `CREATE [UNIQUE] INDEX ...`, with the cursor after `CREATE`.
    pub(super) fn parse_create_index(&mut self, start: u32) -> Result<Statement> {
        let unique = self.eat_kw("unique");
        self.expect_kw("index")?;
        let concurrently = self.eat_kw("concurrently");
        let if_not_exists = self.eat_if_not_exists();
        let name = if self.is_kw("on") && !if_not_exists {
            None
        } else {
            Some(self.parse_col_id()?)
        };
        self.expect_kw("on")?;
        let (table, _only) = self.parse_relation_name()?;
        let method = if self.eat_kw("using") {
            Some(self.parse_col_id()?)
        } else {
            None
        };
        self.expect(&TokenKind::LParen)?;
        let mut columns = vec![self.parse_index_elem()?];
        while self.eat(&TokenKind::Comma) {
            columns.push(self.parse_index_elem()?);
        }
        self.expect(&TokenKind::RParen)?;
        let include = if self.eat_kw("include") {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        let nulls_not_distinct = self.parse_nulls_not_distinct_clause()?;
        let options = if self.is_kw("with") && self.peek_nth(1).kind == TokenKind::LParen {
            self.advance();
            self.parse_rel_options()?
        } else {
            Vec::new()
        };
        let tablespace = if self.eat_kw("tablespace") {
            Some(self.parse_col_id()?)
        } else {
            None
        };
        let where_clause = if self.eat_kw("where") {
            Some(self.parse_a_expr()?)
        } else {
            None
        };
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            table,
            unique,
            if_not_exists,
            concurrently,
            method,
            columns,
            include,
            nulls_not_distinct,
            options,
            tablespace,
            where_clause,
            span: self.span_from(start),
        }))
    }

    /// `[NULLS [NOT] DISTINCT]`; true for `NULLS NOT DISTINCT`.
    fn parse_nulls_not_distinct_clause(&mut self) -> Result<bool> {
        if self.is_kw("nulls") && (self.nth_is_kw(1, "distinct") || self.nth_is_kw(1, "not")) {
            self.advance();
            let not = self.eat_kw("not");
            self.expect_kw("distinct")?;
            return Ok(not);
        }
        Ok(false)
    }

    /// `index_elem`: `(ColId | func_expr | '(' a_expr ')') [COLLATE name]
    /// [opclass] [ASC|DESC] [NULLS FIRST|LAST]`.
    fn parse_index_elem(&mut self) -> Result<IndexElem> {
        let start = self.start();
        let kind = if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let e = self.parse_a_expr()?;
            self.expect(&TokenKind::RParen)?;
            IndexElemKind::Expr(e)
        } else if self.at_col_id() && self.peek_nth(1).kind != TokenKind::LParen {
            IndexElemKind::Column(self.parse_col_id()?)
        } else {
            let e = self.parse_a_expr()?;
            if !matches!(
                e,
                Expr::Function { .. }
                    | Expr::Coalesce { .. }
                    | Expr::NullIf { .. }
                    | Expr::MinMax { .. }
                    | Expr::SessionValue { .. }
                    | Expr::Cast { .. }
            ) {
                return Err(self.unexpected_at(e.span().start));
            }
            IndexElemKind::Expr(e)
        };
        let collation = if self.eat_kw("collate") {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        let nulls_follows =
            self.is_kw("nulls") && (self.nth_is_kw(1, "first") || self.nth_is_kw(1, "last"));
        let opclass = if self.eat_kw("using") || (self.at_col_id() && !nulls_follows) {
            let name = self.parse_object_name()?;
            if self.peek_kind() == &TokenKind::LParen {
                return Err(self.not_supported("operator class parameters"));
            }
            Some(name)
        } else {
            None
        };
        let direction = if self.eat_kw("asc") {
            Some(SortDirection::Asc)
        } else if self.eat_kw("desc") {
            Some(SortDirection::Desc)
        } else {
            None
        };
        let nulls = if self.is_kw("nulls") {
            self.advance();
            if self.eat_kw("first") {
                Some(NullsOrder::First)
            } else {
                self.expect_kw("last")?;
                Some(NullsOrder::Last)
            }
        } else {
            None
        };
        Ok(IndexElem {
            kind,
            collation,
            opclass,
            direction,
            nulls,
            span: self.span_from(start),
        })
    }

    // ----- DROP INDEX --------------------------------------------------

    /// `DROP INDEX [CONCURRENTLY] [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`,
    /// with the cursor on `INDEX`.
    pub(super) fn parse_drop_index(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let concurrently = self.eat_kw("concurrently");
        let if_exists = self.eat_if_exists();
        let mut names = vec![self.parse_object_name()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.parse_object_name()?);
        }
        let cascade = self.parse_drop_behavior() == Some(DropBehavior::Cascade);
        Ok(Statement::DropIndex(DropIndex {
            names,
            if_exists,
            concurrently,
            cascade,
            span: self.span_from(start),
        }))
    }

    // ----- ALTER -------------------------------------------------------

    pub(super) fn parse_alter(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        if self.is_kw("sequence") {
            return self.parse_alter_sequence(start);
        }
        if !self.is_kw("table") {
            return Err(match self.peek().keyword() {
                Some(kw) => self.not_supported(&format!("ALTER {}", kw.to_ascii_uppercase())),
                None => self.unexpected(),
            });
        }
        self.advance();
        let if_exists = self.eat_if_exists();
        let (name, only) = self.parse_relation_name()?;
        let action_start = self.start();
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        let action = match kw.as_str() {
            "add" => {
                self.advance();
                let kw2 = self.peek().keyword().unwrap_or_default().to_string();
                let is_constraint = matches!(
                    kw2.as_str(),
                    "constraint" | "primary" | "unique" | "check" | "foreign"
                ) || (kw2 == "exclude"
                    && self.peek_nth(1).kind == TokenKind::LParen);
                if is_constraint {
                    AlterTableAction::AddConstraint(self.parse_table_constraint()?)
                } else {
                    self.skip_to_statement_end()?;
                    AlterTableAction::Other {
                        what: "ADD COLUMN".to_string(),
                        span: self.span_from(action_start),
                    }
                }
            }
            "owner" => {
                self.advance();
                self.expect_kw("to")?;
                AlterTableAction::OwnerTo(self.parse_role_spec()?)
            }
            k if ALTER_TABLE_OTHER.contains(&k) => {
                self.advance();
                let mut what = k.to_ascii_uppercase();
                if let Some(next) = self.peek().keyword()
                    && matches!(
                        k,
                        "drop" | "alter" | "set" | "enable" | "disable" | "validate"
                    )
                    && next != "not"
                {
                    what.push(' ');
                    what.push_str(&next.to_ascii_uppercase());
                }
                self.skip_to_statement_end()?;
                AlterTableAction::Other {
                    what,
                    span: self.span_from(action_start),
                }
            }
            _ => return Err(self.unexpected()),
        };
        if self.peek_kind() == &TokenKind::Comma {
            return Err(self.not_supported("multiple ALTER TABLE actions"));
        }
        Ok(Statement::AlterTable(AlterTable {
            name,
            if_exists,
            only,
            action,
            span: self.span_from(start),
        }))
    }

    /// Consumes tokens up to (not including) `;` or the end of input.
    fn skip_to_statement_end(&mut self) -> Result<()> {
        while !matches!(
            self.peek_kind(),
            TokenKind::Semicolon | TokenKind::Eof | TokenKind::LexError
        ) {
            self.advance();
        }
        if self.peek_kind() == &TokenKind::LexError {
            return Err(self.unexpected());
        }
        Ok(())
    }

    /// `RoleSpec`. `none` is reserved (`42939`); `PUBLIC` is its own variant.
    pub(super) fn parse_role_spec(&mut self) -> Result<RoleSpec> {
        let spec = match self.peek().keyword() {
            Some("current_user") => Some(RoleSpec::CurrentUser),
            Some("current_role") => Some(RoleSpec::CurrentRole),
            Some("session_user") => Some(RoleSpec::SessionUser),
            _ => None,
        };
        if let Some(spec) = spec {
            self.advance();
            return Ok(spec);
        }
        let ident: Ident = self.parse_non_reserved_word()?;
        match ident.value.as_str() {
            "none" => Err(
                Error::new(sqlstate::RESERVED_NAME, "role name \"none\" is reserved")
                    .with_span(ident.span),
            ),
            "public" => Ok(RoleSpec::Public),
            _ => Ok(RoleSpec::Name(ident)),
        }
    }

    // ----- TRUNCATE ----------------------------------------------------

    /// `TRUNCATE [TABLE] relation_expr_list [RESTART | CONTINUE IDENTITY] [CASCADE | RESTRICT]`.
    pub(super) fn parse_truncate(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        self.eat_kw("table");
        let mut tables = Vec::new();
        let mut only = false;
        loop {
            let (name, o) = self.parse_relation_name()?;
            only |= o;
            tables.push(name);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let mut restart_identity = false;
        if self.eat_kw("restart") {
            self.expect_kw("identity")?;
            restart_identity = true;
        } else if self.eat_kw("continue") {
            self.expect_kw("identity")?;
        }
        let cascade = self.parse_drop_behavior() == Some(DropBehavior::Cascade);
        Ok(Statement::Truncate(Truncate {
            tables,
            restart_identity,
            cascade,
            only,
            span: self.span_from(start),
        }))
    }

    // ----- VACUUM / ANALYZE --------------------------------------------

    /// `VACUUM` / `ANALYZE` (`ANALYSE`) with the cursor on the keyword.
    pub(super) fn parse_vacuum(&mut self) -> Result<Statement> {
        let tok = self.advance();
        let start = tok.span.start;
        let vacuum = tok.is_kw("vacuum");
        let mut options = Vec::new();
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            loop {
                let name = if self.is_kw("analyze") || self.is_kw("analyse") {
                    let t = self.advance();
                    Ident {
                        value: "analyze".to_string(),
                        quoted: false,
                        span: t.span,
                    }
                } else {
                    self.parse_non_reserved_word()?
                };
                let value = self.parse_opt_arg()?.map(super::ddl::OptArg::into_text);
                options.push(VacuumOption { name, value });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        } else {
            let legacy: &[&str] = if vacuum {
                &["full", "freeze", "verbose", "analyze", "analyse"]
            } else {
                &["verbose"]
            };
            while let Some(kw) = self.peek().keyword().filter(|k| legacy.contains(k)) {
                let value = if kw == "analyse" { "analyze" } else { kw }.to_string();
                let span = self.advance().span;
                options.push(VacuumOption {
                    name: Ident {
                        value,
                        quoted: false,
                        span,
                    },
                    value: None,
                });
            }
        }
        let mut targets = Vec::new();
        if self.at_col_id() {
            loop {
                let name = self.parse_object_name()?;
                let columns = if self.peek_kind() == &TokenKind::LParen {
                    self.parse_paren_name_list()?
                } else {
                    Vec::new()
                };
                targets.push(VacuumTarget { name, columns });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        Ok(Statement::Vacuum(Vacuum {
            vacuum,
            options,
            targets,
            span: self.span_from(start),
        }))
    }
}
