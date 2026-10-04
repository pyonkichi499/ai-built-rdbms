//! DDL: CREATE TABLE, DROP TABLE.

use super::Parser;
use crate::error::Result;
use crate::sql::ast::{
    ColumnConstraint, ColumnConstraintKind, ColumnDefinition, CreateTable, DropBehavior, DropTable,
    Ident, SourceExpr, Statement, TableConstraint, TableConstraintKind, TableElement,
};
use crate::sql::token::TokenKind;

/// Object kinds after CREATE / DROP that are valid PostgreSQL syntax but
/// not supported yet (`0A000` instead of a syntax error).
const CREATE_OBJECTS: &[&str] = &[
    "access",
    "aggregate",
    "cast",
    "collation",
    "conversion",
    "database",
    "default",
    "domain",
    "event",
    "extension",
    "foreign",
    "function",
    "group",
    "index",
    "language",
    "materialized",
    "operator",
    "or",
    "policy",
    "procedure",
    "publication",
    "recursive",
    "role",
    "rule",
    "schema",
    "sequence",
    "server",
    "statistics",
    "subscription",
    "tablespace",
    "text",
    "transform",
    "trigger",
    "type",
    "unique",
    "user",
    "view",
];

impl Parser<'_> {
    pub(super) fn parse_create(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        if self.is_kw("global") || self.is_kw("local") {
            self.advance();
        }
        if self.is_kw("temp") || self.is_kw("temporary") {
            return Err(self.not_supported("temporary tables"));
        }
        if self.is_kw("unlogged") {
            return Err(self.not_supported("unlogged tables"));
        }
        if !self.is_kw("table") {
            return Err(match self.peek().keyword() {
                Some(kw) if CREATE_OBJECTS.contains(&kw) => {
                    self.not_supported(&format!("CREATE {}", kw.to_ascii_uppercase()))
                }
                _ => self.unexpected(),
            });
        }
        self.advance();
        let if_not_exists =
            self.is_kw("if") && self.nth_is_kw(1, "not") && self.nth_is_kw(2, "exists");
        if if_not_exists {
            self.advance();
            self.advance();
            self.advance();
        }
        let name = self.parse_object_name()?;
        for kw in ["as", "of", "partition"] {
            if self.is_kw(kw) {
                return Err(
                    self.not_supported(&format!("CREATE TABLE ... {}", kw.to_ascii_uppercase()))
                );
            }
        }
        self.expect(&TokenKind::LParen)?;
        let mut elements = Vec::new();
        if !self.eat(&TokenKind::RParen) {
            loop {
                elements.push(self.parse_table_element()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        }
        for kw in [
            "inherits",
            "partition",
            "using",
            "with",
            "without",
            "on",
            "tablespace",
        ] {
            if self.is_kw(kw) {
                return Err(
                    self.not_supported(&format!("CREATE TABLE ... {}", kw.to_ascii_uppercase()))
                );
            }
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            elements,
            span: self.span_from(start),
        }))
    }

    fn parse_table_element(&mut self) -> Result<TableElement> {
        if ["constraint", "check", "unique", "primary", "foreign"]
            .iter()
            .any(|kw| self.is_kw(kw))
        {
            return Ok(TableElement::Constraint(self.parse_table_constraint()?));
        }
        if self.is_kw("exclude")
            && (self.peek_nth(1).kind == TokenKind::LParen || self.nth_is_kw(1, "using"))
        {
            return Err(self.not_supported("EXCLUDE constraints"));
        }
        if self.is_kw("like") {
            return Err(self.not_supported("CREATE TABLE ... LIKE"));
        }
        let start = self.start();
        let name = self.parse_col_id()?;
        let type_name = self.parse_type_name(false)?;
        let constraints = self.parse_column_constraints()?;
        Ok(TableElement::Column(ColumnDefinition {
            name,
            type_name,
            constraints,
            span: self.span_from(start),
        }))
    }

    /// The verbatim source text of the tokens consumed since `start`.
    fn source_expr_text(&self, start: u32) -> String {
        self.text(self.span_from(start)).to_string()
    }

    /// `CHECK '(' a_expr ')' [NO INHERIT]` after the CHECK keyword.
    fn parse_check_body(&mut self) -> Result<SourceExpr> {
        self.expect(&TokenKind::LParen)?;
        let expr_start = self.start();
        let expr = self.parse_a_expr()?;
        let text = self.source_expr_text(expr_start);
        self.expect(&TokenKind::RParen)?;
        if self.is_kw("no") && self.nth_is_kw(1, "inherit") {
            self.advance();
            self.advance();
        }
        Ok(SourceExpr { expr, text })
    }

    fn parse_column_constraints(&mut self) -> Result<Vec<ColumnConstraint>> {
        let mut constraints = Vec::new();
        loop {
            let start = self.start();
            let name = if self.eat_kw("constraint") {
                Some(self.parse_col_id()?)
            } else {
                None
            };
            let kw = self.peek().keyword().unwrap_or_default().to_string();
            let kind = match kw.as_str() {
                "not" if self.nth_is_kw(1, "null") => {
                    self.advance();
                    self.advance();
                    ColumnConstraintKind::NotNull
                }
                "null" => {
                    self.advance();
                    ColumnConstraintKind::Null
                }
                "default" => {
                    self.advance();
                    let expr_start = self.start();
                    let expr = self.parse_b_expr()?;
                    let text = self.source_expr_text(expr_start);
                    ColumnConstraintKind::Default(SourceExpr { expr, text })
                }
                "check" => {
                    self.advance();
                    ColumnConstraintKind::Check(self.parse_check_body()?)
                }
                "primary" => {
                    self.advance();
                    self.expect_kw("key")?;
                    self.reject_index_parameters()?;
                    ColumnConstraintKind::PrimaryKey
                }
                "unique" => {
                    self.advance();
                    self.skip_nulls_distinct()?;
                    self.reject_index_parameters()?;
                    ColumnConstraintKind::Unique
                }
                "references" => {
                    self.advance();
                    let table = self.parse_object_name()?;
                    let columns = if self.peek_kind() == &TokenKind::LParen {
                        self.parse_paren_name_list()?
                    } else {
                        Vec::new()
                    };
                    self.skip_fk_actions()?;
                    ColumnConstraintKind::References { table, columns }
                }
                "generated" => return Err(self.not_supported("generated columns")),
                "collate" if name.is_none() => {
                    self.advance();
                    let collation = self.parse_object_name()?;
                    let c = collation.name();
                    if collation.parts.len() > 1
                        || !matches!(c.value.as_str(), "C" | "POSIX" | "default")
                    {
                        return Err(self.not_supported("COLLATE"));
                    }
                    continue;
                }
                "deferrable" | "initially" | "not" if name.is_none() => {
                    if self.skip_constraint_attribute()? {
                        continue;
                    }
                    // `NOT` followed by neither NULL nor DEFERRABLE.
                    self.advance();
                    return Err(self.unexpected());
                }
                _ if name.is_some() => return Err(self.unexpected()),
                _ => break,
            };
            self.skip_constraint_attributes()?;
            constraints.push(ColumnConstraint {
                name,
                kind,
                span: self.span_from(start),
            });
        }
        Ok(constraints)
    }

    /// Skips one of `DEFERRABLE`, `NOT DEFERRABLE`, `INITIALLY
    /// {DEFERRED|IMMEDIATE}`; returns false if none is at the cursor.
    fn skip_constraint_attribute(&mut self) -> Result<bool> {
        if self.eat_kw("deferrable") {
            return Ok(true);
        }
        if self.is_kw("not") && self.nth_is_kw(1, "deferrable") {
            self.advance();
            self.advance();
            return Ok(true);
        }
        if self.eat_kw("initially") {
            if !self.eat_kw("deferred") {
                self.expect_kw("immediate")?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn skip_constraint_attributes(&mut self) -> Result<()> {
        while self.skip_constraint_attribute()? {}
        Ok(())
    }

    /// `[NULLS [NOT] DISTINCT]`.
    fn skip_nulls_distinct(&mut self) -> Result<()> {
        if self.is_kw("nulls") && (self.nth_is_kw(1, "distinct") || self.nth_is_kw(1, "not")) {
            self.advance();
            self.eat_kw("not");
            self.expect_kw("distinct")?;
        }
        Ok(())
    }

    fn reject_index_parameters(&self) -> Result<()> {
        for kw in ["include", "with", "using"] {
            if self.is_kw(kw) {
                return Err(self.not_supported("index parameters"));
            }
        }
        Ok(())
    }

    /// `[MATCH FULL|PARTIAL|SIMPLE] [ON DELETE action] [ON UPDATE action]`.
    fn skip_fk_actions(&mut self) -> Result<()> {
        if self.eat_kw("match") && !(self.eat_kw("full") || self.eat_kw("partial")) {
            self.expect_kw("simple")?;
        }
        while self.is_kw("on") {
            self.advance();
            if !self.eat_kw("delete") {
                self.expect_kw("update")?;
            }
            if self.eat_kw("no") {
                self.expect_kw("action")?;
            } else if self.eat_kw("restrict") || self.eat_kw("cascade") {
            } else {
                self.expect_kw("set")?;
                if !self.eat_kw("null") {
                    self.expect_kw("default")?;
                }
                if self.peek_kind() == &TokenKind::LParen {
                    self.parse_paren_name_list()?;
                }
            }
        }
        Ok(())
    }

    fn parse_table_constraint(&mut self) -> Result<TableConstraint> {
        let start = self.start();
        let name: Option<Ident> = if self.eat_kw("constraint") {
            Some(self.parse_col_id()?)
        } else {
            None
        };
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        let kind = match kw.as_str() {
            "check" => {
                self.advance();
                TableConstraintKind::Check(self.parse_check_body()?)
            }
            "unique" => {
                self.advance();
                self.skip_nulls_distinct()?;
                let cols = self.parse_paren_name_list()?;
                self.reject_index_parameters()?;
                TableConstraintKind::Unique(cols)
            }
            "primary" => {
                self.advance();
                self.expect_kw("key")?;
                let cols = self.parse_paren_name_list()?;
                self.reject_index_parameters()?;
                TableConstraintKind::PrimaryKey(cols)
            }
            "foreign" => {
                self.advance();
                self.expect_kw("key")?;
                let columns = self.parse_paren_name_list()?;
                self.expect_kw("references")?;
                let ref_table = self.parse_object_name()?;
                let ref_columns = if self.peek_kind() == &TokenKind::LParen {
                    self.parse_paren_name_list()?
                } else {
                    Vec::new()
                };
                self.skip_fk_actions()?;
                TableConstraintKind::ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                }
            }
            "exclude" => return Err(self.not_supported("EXCLUDE constraints")),
            _ => return Err(self.unexpected()),
        };
        self.skip_constraint_attributes()?;
        Ok(TableConstraint {
            name,
            kind,
            span: self.span_from(start),
        })
    }

    pub(super) fn parse_drop(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        if !self.is_kw("table") {
            return Err(match self.peek().keyword() {
                Some(kw) if CREATE_OBJECTS.contains(&kw) || kw == "owned" => {
                    self.not_supported(&format!("DROP {}", kw.to_ascii_uppercase()))
                }
                _ => self.unexpected(),
            });
        }
        self.advance();
        let if_exists = self.is_kw("if") && self.nth_is_kw(1, "exists");
        if if_exists {
            self.advance();
            self.advance();
        }
        let mut names = vec![self.parse_object_name()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.parse_object_name()?);
        }
        let behavior = if self.eat_kw("cascade") {
            Some(DropBehavior::Cascade)
        } else if self.eat_kw("restrict") {
            Some(DropBehavior::Restrict)
        } else {
            None
        };
        Ok(Statement::DropTable(DropTable {
            names,
            if_exists,
            behavior,
            span: self.span_from(start),
        }))
    }
}
