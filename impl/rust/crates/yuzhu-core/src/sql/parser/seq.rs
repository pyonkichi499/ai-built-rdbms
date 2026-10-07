//! M4: CREATE / ALTER / DROP SEQUENCE and sequence options.

use super::Parser;
use super::ddl::OptArg;
use crate::error::Result;
use crate::sql::ast::{
    AlterSequence, AlterSequenceAction, CreateSequence, DropBehavior, DropSequence, SeqNumber,
    SeqOption, SeqOptionKind, SeqPersistence, Statement,
};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    /// `CREATE [TEMP | UNLOGGED] SEQUENCE ...`, with the cursor on `SEQUENCE`.
    pub(super) fn parse_create_sequence(
        &mut self,
        start: u32,
        persistence: SeqPersistence,
    ) -> Result<Statement> {
        self.expect_kw("sequence")?;
        let if_not_exists =
            self.is_kw("if") && self.nth_is_kw(1, "not") && self.nth_is_kw(2, "exists");
        if if_not_exists {
            for _ in 0..3 {
                self.advance();
            }
        }
        let name = self.parse_object_name()?;
        let options = self.parse_seq_options()?;
        Ok(Statement::CreateSequence(CreateSequence {
            name,
            if_not_exists,
            persistence,
            options,
            span: self.span_from(start),
        }))
    }

    /// `DROP SEQUENCE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`, with
    /// the cursor on `SEQUENCE`.
    pub(super) fn parse_drop_sequence(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let if_exists = self.eat_if_exists();
        let mut names = vec![self.parse_object_name()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.parse_object_name()?);
        }
        let cascade = self.parse_drop_behavior() == Some(DropBehavior::Cascade);
        Ok(Statement::DropSequence(DropSequence {
            names,
            if_exists,
            cascade,
            span: self.span_from(start),
        }))
    }

    /// `ALTER SEQUENCE [IF EXISTS] name action`, with the cursor on `SEQUENCE`.
    pub(super) fn parse_alter_sequence(&mut self, start: u32) -> Result<Statement> {
        self.advance();
        let if_exists = self.eat_if_exists();
        let name = self.parse_object_name()?;
        let action = if self.is_kw("owner") {
            self.advance();
            self.expect_kw("to")?;
            AlterSequenceAction::OwnerTo(self.parse_role_spec()?)
        } else if self.is_kw("rename") {
            self.advance();
            self.expect_kw("to")?;
            AlterSequenceAction::RenameTo(self.parse_col_id()?)
        } else if self.is_kw("set") && self.nth_is_kw(1, "schema") {
            self.advance();
            self.advance();
            AlterSequenceAction::SetSchema(self.parse_col_id()?)
        } else if self.is_kw("set")
            && (self.nth_is_kw(1, "logged") || self.nth_is_kw(1, "unlogged"))
        {
            self.advance();
            let w = self
                .peek()
                .keyword()
                .unwrap_or_default()
                .to_ascii_uppercase();
            return Err(self.not_supported(&format!("ALTER SEQUENCE ... SET {w}")));
        } else {
            let options = self.parse_seq_options()?;
            if options.is_empty() {
                return Err(self.unexpected());
            }
            AlterSequenceAction::Options(options)
        };
        Ok(Statement::AlterSequence(AlterSequence {
            name,
            if_exists,
            action,
            span: self.span_from(start),
        }))
    }

    /// `NumericOnly` as a `SeqNumber`.
    fn parse_seq_number(&mut self) -> Result<SeqNumber> {
        let start = self.start();
        let Some(OptArg::Int(text) | OptArg::Dec(text)) = self.parse_signed_number()? else {
            return Err(self.unexpected());
        };
        Ok(SeqNumber {
            text,
            span: self.span_from(start),
        })
    }

    fn at_number(&self) -> bool {
        matches!(
            self.peek_kind(),
            TokenKind::Integer(_) | TokenKind::Decimal(_)
        ) || self.is_op("-")
            || self.is_op("+")
    }

    /// Zero or more sequence options (stops at the first other token).
    pub(super) fn parse_seq_options(&mut self) -> Result<Vec<SeqOption>> {
        let mut options = Vec::new();
        loop {
            let start = self.start();
            let kw = self.peek().keyword().unwrap_or_default().to_string();
            let kind = match kw.as_str() {
                "as" => {
                    self.advance();
                    SeqOptionKind::As(self.parse_type_name(false)?)
                }
                "increment" => {
                    self.advance();
                    self.eat_kw("by");
                    SeqOptionKind::Increment(self.parse_seq_number()?)
                }
                "minvalue" => {
                    self.advance();
                    SeqOptionKind::MinValue(Some(self.parse_seq_number()?))
                }
                "maxvalue" => {
                    self.advance();
                    SeqOptionKind::MaxValue(Some(self.parse_seq_number()?))
                }
                "start" => {
                    self.advance();
                    self.eat_kw("with");
                    SeqOptionKind::Start(self.parse_seq_number()?)
                }
                "restart" => {
                    self.advance();
                    let with = self.eat_kw("with");
                    if with || self.at_number() {
                        SeqOptionKind::Restart(Some(self.parse_seq_number()?))
                    } else {
                        SeqOptionKind::Restart(None)
                    }
                }
                "cache" => {
                    self.advance();
                    SeqOptionKind::Cache(self.parse_seq_number()?)
                }
                "cycle" => {
                    self.advance();
                    SeqOptionKind::Cycle(true)
                }
                "no" => {
                    self.advance();
                    if self.eat_kw("minvalue") {
                        SeqOptionKind::MinValue(None)
                    } else if self.eat_kw("maxvalue") {
                        SeqOptionKind::MaxValue(None)
                    } else {
                        self.expect_kw("cycle")?;
                        SeqOptionKind::Cycle(false)
                    }
                }
                "owned" => {
                    self.advance();
                    self.expect_kw("by")?;
                    SeqOptionKind::OwnedBy(self.parse_object_name()?)
                }
                "sequence" => {
                    self.advance();
                    self.expect_kw("name")?;
                    SeqOptionKind::SequenceName(self.parse_object_name()?)
                }
                _ => break,
            };
            options.push(SeqOption {
                kind,
                span: self.span_from(start),
            });
        }
        Ok(options)
    }
}
