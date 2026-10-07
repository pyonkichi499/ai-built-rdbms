//! M4: COPY.

use super::Parser;
use crate::error::{Error, Result};
use crate::sql::ast::{Copy, CopyDirection, CopyOption, CopyOptionValue, CopySource, Statement};
use crate::sql::parser::ddl::OptArg;
use crate::sql::token::TokenKind;

impl Parser<'_> {
    pub(super) fn parse_copy(&mut self) -> Result<Statement> {
        let start = self.advance().span.start;
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            self.parse_query()?;
            self.expect(&TokenKind::RParen)?;
            if !self.is_kw("to") {
                return Err(self.unexpected());
            }
            return Err(
                Error::not_supported("COPY TO is not supported yet").with_span(self.peek().span)
            );
        }
        let mut options = Vec::new();
        let binary_span = self.peek().span;
        if self.eat_kw("binary") {
            options.push(CopyOption {
                name: "format".to_string(),
                value: Some(CopyOptionValue::Word("binary".to_string())),
                name_span: binary_span,
            });
        }
        let table = self.parse_object_name()?;
        let columns = if self.peek_kind() == &TokenKind::LParen {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        let direction = if self.eat_kw("from") {
            CopyDirection::From
        } else if self.eat_kw("to") {
            CopyDirection::To
        } else {
            return Err(self.unexpected());
        };
        let source = if self.eat_kw("program") {
            match self.peek_kind().clone() {
                TokenKind::String(s) => {
                    self.advance();
                    CopySource::Program(s)
                }
                _ => return Err(self.unexpected()),
            }
        } else if self.is_kw("stdin") || self.is_kw("stdout") {
            self.advance();
            CopySource::Stdin
        } else {
            match self.peek_kind().clone() {
                TokenKind::String(s) => {
                    self.advance();
                    CopySource::File(s)
                }
                _ => return Err(self.unexpected()),
            }
        };
        self.eat_kw("with");
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            loop {
                options.push(self.parse_copy_generic_option()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        } else {
            while let Some(opt) = self.parse_copy_legacy_option()? {
                options.extend(opt);
            }
        }
        let where_clause = if direction == CopyDirection::From && self.eat_kw("where") {
            Some(self.parse_a_expr()?)
        } else {
            None
        };
        Ok(Statement::Copy(Copy {
            table,
            columns,
            direction,
            source,
            options,
            where_clause,
            span: self.span_from(start),
        }))
    }

    fn parse_copy_generic_option(&mut self) -> Result<CopyOption> {
        let name = self.parse_col_label()?;
        let value = if self.is_op("*") {
            self.advance();
            Some(CopyOptionValue::Star)
        } else if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let mut names = Vec::new();
            loop {
                match self.peek_kind().clone() {
                    TokenKind::String(s) => {
                        self.advance();
                        names.push(s);
                    }
                    _ => names.push(self.parse_col_id()?.value),
                }
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
            Some(CopyOptionValue::List(names))
        } else {
            self.parse_opt_arg()?.map(|a| match a {
                OptArg::Word(w) => CopyOptionValue::Word(w),
                OptArg::Str(s) => CopyOptionValue::String(s),
                OptArg::Int(t) => t
                    .parse::<i64>()
                    .map_or(CopyOptionValue::Word(t), CopyOptionValue::Integer),
                OptArg::Dec(t) => CopyOptionValue::Word(t),
            })
        };
        Ok(CopyOption {
            name: name.value,
            value,
            name_span: name.span,
        })
    }

    /// One legacy option (`FORCE NOT NULL` etc. give one `CopyOption`).
    fn parse_copy_legacy_option(&mut self) -> Result<Option<Vec<CopyOption>>> {
        let span = self.peek().span;
        let Some(kw) = self.peek().keyword().map(str::to_string) else {
            return Ok(None);
        };
        let opt = |name: &str, value: Option<CopyOptionValue>| CopyOption {
            name: name.to_string(),
            value,
            name_span: span,
        };
        let one = match kw.as_str() {
            "binary" => {
                self.advance();
                opt("format", Some(CopyOptionValue::Word("binary".into())))
            }
            "csv" => {
                self.advance();
                opt("format", Some(CopyOptionValue::Word("csv".into())))
            }
            "freeze" => {
                self.advance();
                opt("freeze", None)
            }
            "header" => {
                self.advance();
                opt("header", None)
            }
            "delimiter" | "null" | "quote" | "escape" | "encoding" => {
                self.advance();
                if kw != "encoding" {
                    self.eat_kw("as");
                }
                let TokenKind::String(s) = self.peek_kind().clone() else {
                    return Err(self.unexpected());
                };
                self.advance();
                opt(&kw, Some(CopyOptionValue::String(s)))
            }
            "force" => {
                self.advance();
                let name = if self.eat_kw("quote") {
                    "force_quote"
                } else if self.eat_kw("not") {
                    self.expect_kw("null")?;
                    "force_not_null"
                } else {
                    self.expect_kw("null")?;
                    "force_null"
                };
                let value = if self.is_op("*") {
                    self.advance();
                    CopyOptionValue::Star
                } else {
                    let mut cols = vec![self.parse_col_id()?.value];
                    while self.eat(&TokenKind::Comma) {
                        cols.push(self.parse_col_id()?.value);
                    }
                    CopyOptionValue::List(cols)
                };
                opt(name, Some(value))
            }
            _ => return Ok(None),
        };
        Ok(Some(vec![one]))
    }
}
