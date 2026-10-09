//! 配列の構文（`m5/05-types-core.md` §5.4、TY-5b）: `ARRAY[...]`、`ARRAY(SELECT ...)`、
//! `expr[i]`（スライス `expr[l:u]` は構文だけ受けて解析が `0A000`）、`x op ANY|ALL (array_expr)`。

use super::Parser;
use crate::error::Result;
use crate::sql::ast::{Expr, Ident, Quantifier};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    /// `ARRAY [ ... ]` / `ARRAY ( query )`。カーソルは `ARRAY`。
    pub(super) fn parse_array_expr(&mut self) -> Result<Expr> {
        let start = self.start();
        self.advance();
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            let query = Box::new(self.parse_query()?);
            self.expect(&TokenKind::RParen)?;
            return Ok(Expr::ArraySubquery {
                query,
                span: self.span_from(start),
            });
        }
        self.parse_array_bracket(start)
    }

    /// `[ expr, ... ]`（空も可）。内側の `[..]` は入れ子の配列（解析が `0A000`）。
    fn parse_array_bracket(&mut self, start: u32) -> Result<Expr> {
        self.expect(&TokenKind::LBracket)?;
        let mut elems = Vec::new();
        if self.peek_kind() != &TokenKind::RBracket {
            loop {
                let e = if self.peek_kind() == &TokenKind::LBracket {
                    let s = self.start();
                    self.nested(|p| p.parse_array_bracket(s))?
                } else {
                    self.parse_a_expr()?
                };
                elems.push(e);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBracket)?;
        Ok(Expr::ArrayConstructor {
            elems,
            span: self.span_from(start),
        })
    }

    /// 後置の `[i]` / `[l:u]`。カーソルは `[`。`ARRAY[..][1]` とリテラルへの添字は構文エラー（PG と同じ）。
    pub(super) fn parse_subscript(&mut self, base: Expr) -> Result<Expr> {
        if matches!(
            base,
            Expr::ArrayConstructor { .. } | Expr::Literal { .. } | Expr::ArraySubquery { .. }
        ) {
            return Err(self.unexpected());
        }
        let start = base.span().start;
        self.expect(&TokenKind::LBracket)?;
        let mut slice = false;
        let mut lower = None;
        let mut upper = None;
        if self.peek_kind() != &TokenKind::Colon {
            lower = Some(Box::new(self.parse_a_expr()?));
        }
        if self.eat(&TokenKind::Colon) {
            slice = true;
            if self.peek_kind() != &TokenKind::RBracket {
                upper = Some(Box::new(self.parse_a_expr()?));
            }
        }
        self.expect(&TokenKind::RBracket)?;
        Ok(Expr::Subscript {
            expr: Box::new(base),
            lower,
            upper,
            slice,
            span: self.span_from(start),
        })
    }

    /// `x op ANY|SOME|ALL ( array_expr )`。カーソルは量化子。
    pub(super) fn parse_quantified_array(
        &mut self,
        left: Box<Expr>,
        op: String,
        op_schema: Option<Ident>,
        op_start: u32,
    ) -> Result<Expr> {
        let quantifier = if self.advance().is_kw("all") {
            Quantifier::All
        } else {
            Quantifier::Any
        };
        self.expect(&TokenKind::LParen)?;
        let array = Box::new(self.parse_a_expr()?);
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::QuantifiedArray {
            expr: left,
            op,
            op_schema,
            quantifier,
            array,
            span: self.span_from(op_start),
        })
    }
}
