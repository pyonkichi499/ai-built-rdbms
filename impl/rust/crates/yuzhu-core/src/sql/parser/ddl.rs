//! DDL: CREATE TABLE, DROP TABLE, constraints, `WITH (...)` options.
//! Indexes, ALTER TABLE, TRUNCATE and VACUUM are in `ddl_index.rs`,
//! sequences in `seq.rs`.

use super::Parser;
use crate::error::{Error, Result};
use crate::sql::ast::{
    ColumnConstraint, ColumnConstraintKind, ColumnDefinition, CreateTable, DropBehavior, DropTable,
    GeneratedWhen, Ident, IndexParams, KeyConstraint, ObjectName, RelOption, SeqPersistence,
    SourceExpr, Statement, TableConstraint, TableConstraintKind, TableElement,
};
use crate::sql::token::TokenKind;

/// The argument of a `name [value]` option (`def_arg`, `utility_option_arg`).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum OptArg {
    /// Unquoted word.
    Word(String),
    /// Quoted word or string literal content.
    Str(String),
    /// Integer literal text with its sign (`-` only).
    Int(String),
    /// Decimal literal text with its sign.
    Dec(String),
}

impl OptArg {
    /// The text of the argument (`RelOption` / `VacuumOption` values).
    pub(super) fn into_text(self) -> String {
        match self {
            OptArg::Word(s) | OptArg::Str(s) | OptArg::Int(s) | OptArg::Dec(s) => s,
        }
    }
}

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
        if self.is_kw("unique") || self.is_kw("index") {
            return self.parse_create_index(start);
        }
        if self.is_kw("global") || self.is_kw("local") {
            self.advance();
        }
        let temp = self.is_kw("temp") || self.is_kw("temporary");
        let unlogged = self.is_kw("unlogged");
        if (temp || unlogged) && self.nth_is_kw(1, "sequence") {
            self.advance();
            let persistence = if temp {
                SeqPersistence::Temporary
            } else {
                SeqPersistence::Unlogged
            };
            return self.parse_create_sequence(start, persistence);
        }
        if temp {
            return Err(self.not_supported("temporary tables"));
        }
        if unlogged {
            return Err(self.not_supported("unlogged tables"));
        }
        if self.is_kw("sequence") {
            return self.parse_create_sequence(start, SeqPersistence::Permanent);
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
        let options = if self.is_kw("with") && self.peek_nth(1).kind == TokenKind::LParen {
            self.advance();
            self.parse_rel_options()?
        } else {
            Vec::new()
        };
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
            options,
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
        let mut no_inherit = false;
        if self.is_kw("no") && self.nth_is_kw(1, "inherit") {
            self.advance();
            self.advance();
            no_inherit = true;
        }
        Ok(SourceExpr {
            expr,
            text,
            no_inherit,
        })
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
                    ColumnConstraintKind::Default(SourceExpr {
                        expr,
                        text,
                        no_inherit: false,
                    })
                }
                "check" => {
                    self.advance();
                    ColumnConstraintKind::Check(self.parse_check_body()?)
                }
                "primary" => {
                    self.advance();
                    self.expect_kw("key")?;
                    ColumnConstraintKind::PrimaryKey(self.parse_index_params(false)?)
                }
                "unique" => {
                    self.advance();
                    let nulls_not_distinct = self.parse_nulls_not_distinct()?;
                    let mut params = self.parse_index_params(false)?;
                    params.nulls_not_distinct = nulls_not_distinct;
                    ColumnConstraintKind::Unique(params)
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
                "generated" => self.parse_generated()?,
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
                    if self.skip_constraint_attribute(&mut false)? {
                        continue;
                    }
                    // `NOT` followed by neither NULL nor DEFERRABLE.
                    self.advance();
                    return Err(self.unexpected());
                }
                _ if name.is_some() => return Err(self.unexpected()),
                _ => break,
            };
            let deferrable = self.parse_constraint_attributes()?
                && matches!(
                    kind,
                    ColumnConstraintKind::PrimaryKey(_)
                        | ColumnConstraintKind::Unique(_)
                        | ColumnConstraintKind::References { .. }
                );
            constraints.push(ColumnConstraint {
                name,
                kind,
                deferrable,
                span: self.span_from(start),
            });
        }
        Ok(constraints)
    }

    /// `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(sequence options)]`.
    /// Generated columns (`AS (expr) STORED`) are `0A000` at `GENERATED`.
    fn parse_generated(&mut self) -> Result<ColumnConstraintKind> {
        let generated = self.advance().span;
        let when = if self.eat_kw("always") {
            GeneratedWhen::Always
        } else {
            self.expect_kw("by")?;
            self.expect_kw("default")?;
            GeneratedWhen::ByDefault
        };
        self.expect_kw("as")?;
        if !self.eat_kw("identity") {
            return Err(
                Error::not_supported("generated columns is not supported yet").with_span(generated),
            );
        }
        let options = if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let options = self.parse_seq_options()?;
            if options.is_empty() {
                return Err(self.unexpected());
            }
            self.expect(&TokenKind::RParen)?;
            options
        } else {
            Vec::new()
        };
        Ok(ColumnConstraintKind::Identity { when, options })
    }

    /// Skips one of `DEFERRABLE`, `NOT DEFERRABLE`, `INITIALLY
    /// {DEFERRED|IMMEDIATE}`, updating `deferrable`; returns false if none
    /// is at the cursor.
    fn skip_constraint_attribute(&mut self, deferrable: &mut bool) -> Result<bool> {
        if self.eat_kw("deferrable") {
            *deferrable = true;
            return Ok(true);
        }
        if self.is_kw("not") && self.nth_is_kw(1, "deferrable") {
            self.advance();
            self.advance();
            *deferrable = false;
            return Ok(true);
        }
        if self.eat_kw("initially") {
            if self.eat_kw("deferred") {
                *deferrable = true;
            } else {
                self.expect_kw("immediate")?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Skips all constraint attributes; true if the constraint was made
    /// deferrable (`DEFERRABLE` or `INITIALLY DEFERRED`).
    fn parse_constraint_attributes(&mut self) -> Result<bool> {
        let mut deferrable = false;
        while self.skip_constraint_attribute(&mut deferrable)? {}
        Ok(deferrable)
    }

    /// `[NULLS [NOT] DISTINCT]`; true for `NULLS NOT DISTINCT`.
    fn parse_nulls_not_distinct(&mut self) -> Result<bool> {
        if self.is_kw("nulls") && (self.nth_is_kw(1, "distinct") || self.nth_is_kw(1, "not")) {
            self.advance();
            let not = self.eat_kw("not");
            self.expect_kw("distinct")?;
            return Ok(not);
        }
        Ok(false)
    }

    /// `[INCLUDE (cols)] [WITH (...)] [USING INDEX TABLESPACE name]`.
    /// `INCLUDE` is only valid for table constraints.
    fn parse_index_params(&mut self, table_level: bool) -> Result<IndexParams> {
        let mut params = IndexParams::default();
        if table_level && self.eat_kw("include") {
            params.include = self.parse_paren_name_list()?;
        }
        if self.is_kw("with") && self.peek_nth(1).kind == TokenKind::LParen {
            self.advance();
            params.options = self.parse_rel_options()?;
        }
        if self.is_kw("using") && self.nth_is_kw(1, "index") && self.nth_is_kw(2, "tablespace") {
            self.advance();
            self.advance();
            self.advance();
            params.tablespace = Some(self.parse_col_id()?);
        }
        Ok(params)
    }

    /// `'(' reloption [, ...] ')'`, with the cursor on the `(`:
    /// `name [. name] [= value]`.
    pub(super) fn parse_rel_options(&mut self) -> Result<Vec<RelOption>> {
        self.expect(&TokenKind::LParen)?;
        let mut options = Vec::new();
        loop {
            let start = self.start();
            let mut name = self.parse_col_label()?;
            let mut namespace = None;
            if self.eat(&TokenKind::Dot) {
                namespace = Some(name);
                name = self.parse_col_label()?;
            }
            let value = if self.eat_op("=") {
                match self.parse_opt_arg()? {
                    Some(v) => Some(v.into_text()),
                    None => return Err(self.unexpected()),
                }
            } else {
                None
            };
            options.push(RelOption {
                namespace,
                name,
                value,
                span: self.span_from(start),
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(options)
    }

    /// An option argument: a word, a string, or a number with an optional
    /// sign. `None` when the cursor is on `,` or `)` (no argument).
    pub(super) fn parse_opt_arg(&mut self) -> Result<Option<OptArg>> {
        let t = self.peek().clone();
        let arg = match &t.kind {
            TokenKind::Comma | TokenKind::RParen => return Ok(None),
            TokenKind::Word { value, quoted } => {
                if *quoted {
                    OptArg::Str(value.clone())
                } else {
                    OptArg::Word(value.clone())
                }
            }
            TokenKind::String(s) => OptArg::Str(s.clone()),
            TokenKind::Integer(_) | TokenKind::Decimal(_) => return self.parse_signed_number(),
            TokenKind::Op(o) if o == "-" || o == "+" => return self.parse_signed_number(),
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(Some(arg))
    }

    /// `NumericOnly`: `[+|-] number`. A `+` sign is dropped.
    pub(super) fn parse_signed_number(&mut self) -> Result<Option<OptArg>> {
        let mut negative = false;
        if self.is_op("-") {
            negative = true;
            self.advance();
        } else if self.is_op("+") {
            self.advance();
        }
        let (text, is_int) = match self.peek_kind() {
            TokenKind::Integer(s) => (s.clone(), true),
            TokenKind::Decimal(s) => (s.clone(), false),
            _ => return Err(self.unexpected()),
        };
        self.advance();
        let text = if negative { format!("-{text}") } else { text };
        Ok(Some(if is_int {
            OptArg::Int(text)
        } else {
            OptArg::Dec(text)
        }))
    }

    /// `relation_expr`: `[ONLY] name [*]` or `ONLY (name)`; true if `ONLY`.
    pub(super) fn parse_relation_name(&mut self) -> Result<(ObjectName, bool)> {
        if self.eat_kw("only") {
            if self.eat(&TokenKind::LParen) {
                let name = self.parse_object_name()?;
                self.expect(&TokenKind::RParen)?;
                return Ok((name, true));
            }
            return Ok((self.parse_object_name()?, true));
        }
        let name = self.parse_object_name()?;
        self.eat_op("*");
        Ok((name, false))
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

    pub(super) fn parse_table_constraint(&mut self) -> Result<TableConstraint> {
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
                let nulls_not_distinct = self.parse_nulls_not_distinct()?;
                let mut key = self.parse_key_constraint()?;
                key.params.nulls_not_distinct = nulls_not_distinct;
                TableConstraintKind::Unique(key)
            }
            "primary" => {
                self.advance();
                self.expect_kw("key")?;
                TableConstraintKind::PrimaryKey(self.parse_key_constraint()?)
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
        let deferrable = self.parse_constraint_attributes()?
            && matches!(
                kind,
                TableConstraintKind::PrimaryKey(_)
                    | TableConstraintKind::Unique(_)
                    | TableConstraintKind::ForeignKey { .. }
            );
        Ok(TableConstraint {
            name,
            kind,
            deferrable,
            span: self.span_from(start),
        })
    }

    /// After `PRIMARY KEY` / `UNIQUE [NULLS ...]` at table level:
    /// `(cols) [INCLUDE ...] [WITH ...] [USING INDEX TABLESPACE t]` or
    /// `USING INDEX name`.
    fn parse_key_constraint(&mut self) -> Result<KeyConstraint> {
        if self.is_kw("using") && self.nth_is_kw(1, "index") && !self.nth_is_kw(2, "tablespace") {
            self.advance();
            self.advance();
            let index = self.parse_col_id()?;
            return Ok(KeyConstraint {
                columns: Vec::new(),
                params: IndexParams {
                    using_index: Some(index),
                    ..IndexParams::default()
                },
            });
        }
        let columns = self.parse_paren_name_list()?;
        let params = self.parse_index_params(true)?;
        Ok(KeyConstraint { columns, params })
    }

    pub(super) fn parse_drop(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        if self.is_kw("index") {
            return self.parse_drop_index(start);
        }
        if self.is_kw("sequence") {
            return self.parse_drop_sequence(start);
        }
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
        let behavior = self.parse_drop_behavior();
        Ok(Statement::DropTable(DropTable {
            names,
            if_exists,
            behavior,
            span: self.span_from(start),
        }))
    }

    /// `[CASCADE | RESTRICT]`.
    pub(super) fn parse_drop_behavior(&mut self) -> Option<DropBehavior> {
        if self.eat_kw("cascade") {
            Some(DropBehavior::Cascade)
        } else if self.eat_kw("restrict") {
            Some(DropBehavior::Restrict)
        } else {
            None
        }
    }
}
