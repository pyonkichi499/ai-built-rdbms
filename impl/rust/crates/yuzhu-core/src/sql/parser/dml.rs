//! DML: INSERT, UPDATE, DELETE.

use super::Parser;
use super::expr::is_query_start_kw;
use crate::error::Result;
use crate::sql::ast::{
    Assignment, Delete, Ident, Insert, InsertSource, QueryBody, SelectItem, Update,
};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    fn parse_returning(&mut self) -> Result<Vec<SelectItem>> {
        if self.eat_kw("returning") {
            self.parse_target_list()
        } else {
            Ok(Vec::new())
        }
    }

    /// A column name in an INSERT column list or UPDATE SET target.
    fn parse_target_column(&mut self) -> Result<Ident> {
        let col = self.parse_col_id()?;
        if matches!(self.peek_kind(), TokenKind::Dot | TokenKind::LBracket) {
            return Err(self.not_supported("assignment to a field or subscript"));
        }
        Ok(col)
    }

    pub(super) fn parse_insert(&mut self) -> Result<Insert> {
        let start = self.advance().span.start;
        self.expect_kw("into")?;
        let table = self.parse_object_name()?;
        let alias = if self.eat_kw("as") {
            Some(self.parse_col_id()?)
        } else {
            None
        };
        let mut columns = Vec::new();
        if self.peek_kind() == &TokenKind::LParen
            && !is_query_start_kw(self.peek_nth(1).keyword())
            && self.peek_nth(1).kind != TokenKind::LParen
        {
            self.advance();
            loop {
                columns.push(self.parse_target_column()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        }
        if self.is_kw("overriding") {
            return Err(self.not_supported("OVERRIDING"));
        }
        let source = if self.is_kw("default") && self.nth_is_kw(1, "values") {
            self.advance();
            self.advance();
            InsertSource::DefaultValues
        } else {
            let query = self.parse_query()?;
            // DEFAULT is allowed at the top level of the rows of a plain
            // `VALUES` list (PostgreSQL: no ORDER BY / LIMIT / OFFSET).
            if let QueryBody::Values(values) = &query.body
                && query.order_by.is_empty()
                && query.limit.is_none()
                && query.offset.is_none()
            {
                for e in values.rows.iter().flatten() {
                    self.allow_default(e);
                }
            }
            InsertSource::Query(Box::new(query))
        };
        if self.is_kw("on") && self.nth_is_kw(1, "conflict") {
            return Err(self.not_supported("ON CONFLICT"));
        }
        let returning = self.parse_returning()?;
        Ok(Insert {
            table,
            alias,
            columns,
            source,
            returning,
            span: self.span_from(start),
        })
    }

    /// `relation_expr`: `[ONLY] name [*]` or `ONLY (name)`. There is no table
    /// inheritance, so `ONLY` and `*` change nothing.
    fn parse_relation_expr(&mut self) -> Result<crate::sql::ast::ObjectName> {
        if self.eat_kw("only") {
            if self.eat(&TokenKind::LParen) {
                let name = self.parse_object_name()?;
                self.expect(&TokenKind::RParen)?;
                return Ok(name);
            }
            return self.parse_object_name();
        }
        let name = self.parse_object_name()?;
        self.eat_op("*");
        Ok(name)
    }

    /// `relation_expr_opt_alias`: `name [[AS] ColId]`. A bare alias cannot
    /// be the keyword `SET` (for UPDATE).
    fn parse_relation_alias(&mut self) -> Result<Option<Ident>> {
        if self.eat_kw("as") {
            return Ok(Some(self.parse_col_id()?));
        }
        if self.at_col_id() && !self.is_kw("set") {
            return Ok(Some(self.parse_col_id()?));
        }
        Ok(None)
    }

    pub(super) fn parse_update(&mut self) -> Result<Update> {
        let start = self.advance().span.start;
        let table = self.parse_relation_expr()?;
        let alias = self.parse_relation_alias()?;
        self.expect_kw("set")?;
        let mut assignments = Vec::new();
        loop {
            if self.peek_kind() == &TokenKind::LParen {
                return Err(self.not_supported("multiple-column UPDATE"));
            }
            let astart = self.start();
            let column = self.parse_col_id()?;
            let mut fields = Vec::new();
            while self.eat(&TokenKind::Dot) {
                fields.push(self.parse_col_label()?);
            }
            if self.peek_kind() == &TokenKind::LBracket {
                return Err(self.not_supported("assignment to a subscript"));
            }
            if !self.eat_op("=") {
                return Err(self.unexpected());
            }
            let value = self.parse_a_expr()?;
            self.allow_default(&value);
            assignments.push(Assignment {
                column,
                fields,
                value,
                span: self.span_from(astart),
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let from = if self.eat_kw("from") {
            self.parse_from_list()?
        } else {
            Vec::new()
        };
        let selection = self.parse_dml_where()?;
        let returning = self.parse_returning()?;
        Ok(Update {
            table,
            alias,
            assignments,
            from,
            selection,
            returning,
            span: self.span_from(start),
        })
    }

    fn parse_dml_where(&mut self) -> Result<Option<crate::sql::ast::Expr>> {
        if !self.eat_kw("where") {
            return Ok(None);
        }
        if self.is_kw("current") && self.nth_is_kw(1, "of") {
            return Err(self.not_supported("WHERE CURRENT OF"));
        }
        Ok(Some(self.parse_a_expr()?))
    }

    pub(super) fn parse_delete(&mut self) -> Result<Delete> {
        let start = self.advance().span.start;
        self.expect_kw("from")?;
        let table = self.parse_relation_expr()?;
        let alias = self.parse_relation_alias()?;
        let using = if self.eat_kw("using") {
            self.parse_from_list()?
        } else {
            Vec::new()
        };
        let selection = self.parse_dml_where()?;
        let returning = self.parse_returning()?;
        Ok(Delete {
            table,
            alias,
            using,
            selection,
            returning,
            span: self.span_from(start),
        })
    }
}
