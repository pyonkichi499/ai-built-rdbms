//! Expressions (Pratt parsing) and type names.
//!
//! Precedence and associativity follow `gram.y` (lowest first):
//!
//! | level | operators | assoc |
//! |---|---|---|
//! | 1 | `OR` | left |
//! | 2 | `AND` | left |
//! | 3 | `NOT` (prefix) | right |
//! | 4 | `IS`, `ISNULL`, `NOTNULL` | non |
//! | 5 | `< > = <= >= <>` | non |
//! | 6 | `BETWEEN IN LIKE ILIKE SIMILAR` (and `NOT` before them) | non |
//! | 9 | any other operator (`\|\|`, ...) | left |
//! | 10 | `+ -` | left |
//! | 11 | `* / %` | left |
//! | 12 | `^` | left |
//! | 13 | `AT` | left |
//! | 14 | `COLLATE` | left |
//! | 15 | unary `+ -` | right |
//! | 16 | `[ ]` | left |
//! | 18 | `::` | left |
//!
//! As in yacc, an operator of the same level as a non-associative operator
//! whose right operand is being parsed is a syntax error (`1 < 2 < 3`).
//!
//! Node spans: `span.end` is the end of the whole expression, `span.start`
//! is PostgreSQL's "location" of the node, which is where errors about the
//! node point. For most nodes that is the first token, but for infix and
//! postfix operators (`BinaryOp`, `And`, `Or`, `IsNull`, `IsBool`,
//! `IsDistinctFrom`, `Between`, `InList`, `InSubquery`, `Like`) it is the
//! operator token (for `NOT BETWEEN` etc. the `NOT`), and for `x::t` it is
//! the `::`. A folded negative literal starts at the minus sign.

use super::Parser;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    BoolTestValue, CastSyntax, Expr, Ident, Literal, ObjectName, Query, QueryBody,
    SessionValueKind, TypeName, WhenClause,
};
use crate::sql::token::{KeywordCategory, TokenKind, keyword_category};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Assoc {
    Left,
    Right,
    Non,
}

const P_OR: u8 = 1;
const P_AND: u8 = 2;
const P_NOT: u8 = 3;
const P_IS: u8 = 4;
const P_CMP: u8 = 5;
const P_LIKE: u8 = 6;
const P_OP: u8 = 9;
const P_ADD: u8 = 10;
const P_MUL: u8 = 11;
const P_EXP: u8 = 12;
const P_AT: u8 = 13;
const P_COLLATE: u8 = 14;
const P_UMINUS: u8 = 15;
const P_SUBSCRIPT: u8 = 16;
const P_TYPECAST: u8 = 18;

/// The operator whose right operand is being parsed, plus whether we are in
/// a `b_expr` (no boolean operators, `IS` tests, `BETWEEN`/`IN`/`LIKE`).
#[derive(Clone, Copy, Debug)]
pub(super) struct Ctx {
    prec: u8,
    assoc: Assoc,
    restricted: bool,
}

impl Ctx {
    const fn new(prec: u8, assoc: Assoc, restricted: bool) -> Self {
        Ctx {
            prec,
            assoc,
            restricted,
        }
    }
}

/// Precedence class of an operator token used as an infix operator.
fn op_prec(op: &str) -> Option<u8> {
    Some(match op {
        "+" | "-" => P_ADD,
        "*" | "/" | "%" => P_MUL,
        "^" => P_EXP,
        "<" | ">" | "=" | "<=" | ">=" | "<>" => P_CMP,
        "=>" => return None,
        _ => P_OP,
    })
}

/// Keywords that start a query (`select_with_parens` contents).
pub(super) fn is_query_start_kw(kw: Option<&str>) -> bool {
    matches!(kw, Some("select" | "values" | "with" | "table"))
}

fn negate_number(s: &str) -> String {
    match s.strip_prefix('-') {
        Some(rest) => rest.to_string(),
        None if s == "0" => s.to_string(),
        None => format!("-{s}"),
    }
}

fn ident(value: &str, span: Span) -> Ident {
    Ident {
        value: value.to_string(),
        quoted: false,
        span,
    }
}

impl Parser<'_> {
    /// `a_expr`: a full expression.
    pub(super) fn parse_a_expr(&mut self) -> Result<Expr> {
        self.expr_bp(Ctx::new(0, Assoc::Left, false))
    }

    /// `b_expr`: an expression without boolean operators, `IS` tests,
    /// `BETWEEN`, `IN`, `LIKE` (used for column `DEFAULT` and the lower
    /// bound of `BETWEEN`).
    pub(super) fn parse_b_expr(&mut self) -> Result<Expr> {
        self.expr_bp(Ctx::new(0, Assoc::Left, true))
    }

    /// `a_expr [, a_expr ...]`.
    pub(super) fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut list = vec![self.parse_a_expr()?];
        while self.eat(&TokenKind::Comma) {
            list.push(self.parse_a_expr()?);
        }
        Ok(list)
    }

    /// One expression level (guarded against unbounded nesting).
    fn expr_bp(&mut self, ctx: Ctx) -> Result<Expr> {
        self.nested(|p| p.expr_bp_level(ctx))
    }

    fn expr_bp_level(&mut self, ctx: Ctx) -> Result<Expr> {
        let mut left = self.parse_prefix(ctx)?;
        while let Some(p) = self.infix_prec(ctx.restricted) {
            if p < ctx.prec || (p == ctx.prec && ctx.assoc == Assoc::Left) {
                break;
            }
            if p == ctx.prec && ctx.assoc == Assoc::Non {
                return Err(self.unexpected());
            }
            // Check before building, so the error path never holds a tree
            // deeper than the limit either.
            self.chain_link()?;
            left = self.parse_infix(left, p, ctx.restricted)?;
        }
        Ok(left)
    }

    /// The precedence of the current token as an infix / postfix operator.
    fn infix_prec(&self, restricted: bool) -> Option<u8> {
        let t = self.peek();
        match &t.kind {
            TokenKind::Op(op) => op_prec(op),
            TokenKind::Typecast => Some(P_TYPECAST),
            TokenKind::LBracket => Some(P_SUBSCRIPT),
            TokenKind::Word {
                value,
                quoted: false,
            } => match value.as_str() {
                "or" if !restricted => Some(P_OR),
                "and" if !restricted => Some(P_AND),
                "is" => {
                    let distinct = self.nth_is_kw(1, "distinct")
                        || (self.nth_is_kw(1, "not") && self.nth_is_kw(2, "distinct"));
                    (!restricted || distinct).then_some(P_IS)
                }
                "isnull" | "notnull" if !restricted => Some(P_IS),
                "not" if !restricted => {
                    let next = self.peek_nth(1).keyword();
                    matches!(next, Some("between" | "in" | "like" | "ilike" | "similar"))
                        .then_some(P_LIKE)
                }
                "between" | "in" | "like" | "ilike" | "similar" if !restricted => Some(P_LIKE),
                "collate" => Some(P_COLLATE),
                "at" if self.nth_is_kw(1, "time") || self.nth_is_kw(1, "local") => Some(P_AT),
                _ => None,
            },
            _ => None,
        }
    }

    fn parse_infix(&mut self, left: Expr, p: u8, restricted: bool) -> Result<Expr> {
        let t = self.peek().clone();
        let op_start = t.span.start;
        let left = Box::new(left);
        match &t.kind {
            TokenKind::Op(op) => {
                self.advance();
                let assoc = if p == P_CMP { Assoc::Non } else { Assoc::Left };
                let right = Box::new(self.expr_bp(Ctx::new(p, assoc, restricted))?);
                Ok(Expr::BinaryOp {
                    op: op.clone(),
                    left,
                    right,
                    span: self.span_from(op_start),
                })
            }
            TokenKind::Typecast => {
                self.advance();
                let type_name = self.parse_type_name(false)?;
                Ok(Expr::Cast {
                    expr: left,
                    type_name,
                    syntax: CastSyntax::DoubleColon,
                    span: self.span_from(op_start),
                })
            }
            TokenKind::LBracket => Err(self.not_supported("array subscripting")),
            _ => self.parse_keyword_infix(left, restricted),
        }
    }

    fn parse_keyword_infix(&mut self, left: Box<Expr>, restricted: bool) -> Result<Expr> {
        let op_start = self.start();
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        match kw.as_str() {
            "or" | "and" => {
                self.advance();
                let p = if kw == "or" { P_OR } else { P_AND };
                let right = Box::new(self.expr_bp(Ctx::new(p, Assoc::Left, restricted))?);
                let span = self.span_from(op_start);
                Ok(if kw == "or" {
                    Expr::Or { left, right, span }
                } else {
                    Expr::And { left, right, span }
                })
            }
            "is" => self.parse_is(left, restricted),
            "isnull" | "notnull" => {
                self.advance();
                Ok(Expr::IsNull {
                    expr: left,
                    negated: kw == "notnull",
                    span: self.span_from(op_start),
                })
            }
            "collate" => Err(self.not_supported("COLLATE")),
            "at" => Err(self.not_supported("AT TIME ZONE")),
            _ => {
                let negated = self.eat_kw("not");
                self.parse_predicate(left, negated, op_start, restricted)
            }
        }
    }

    fn parse_is(&mut self, left: Box<Expr>, restricted: bool) -> Result<Expr> {
        let op_start = self.advance().span.start;
        let negated = self.eat_kw("not");
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        let value = match kw.as_str() {
            "null" => {
                self.advance();
                return Ok(Expr::IsNull {
                    expr: left,
                    negated,
                    span: self.span_from(op_start),
                });
            }
            "distinct" => {
                self.advance();
                self.expect_kw("from")?;
                let right = Box::new(self.expr_bp(Ctx::new(P_IS, Assoc::Non, restricted))?);
                return Ok(Expr::IsDistinctFrom {
                    left,
                    right,
                    negated,
                    span: self.span_from(op_start),
                });
            }
            "true" => BoolTestValue::True,
            "false" => BoolTestValue::False,
            "unknown" => BoolTestValue::Unknown,
            "document" | "normalized" | "json" | "of" | "nfc" | "nfd" | "nfkc" | "nfkd" => {
                return Err(self.not_supported(&format!("IS {}", kw.to_ascii_uppercase())));
            }
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(Expr::IsBool {
            expr: left,
            value,
            negated,
            span: self.span_from(op_start),
        })
    }

    /// `[NOT] BETWEEN | IN | LIKE | ILIKE | SIMILAR` after the optional NOT.
    fn parse_predicate(
        &mut self,
        left: Box<Expr>,
        negated: bool,
        op_start: u32,
        restricted: bool,
    ) -> Result<Expr> {
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        let operand = Ctx::new(P_LIKE, Assoc::Non, restricted);
        match kw.as_str() {
            "between" => {
                self.advance();
                let symmetric = if self.eat_kw("symmetric") {
                    true
                } else {
                    self.eat_kw("asymmetric");
                    false
                };
                let low = Box::new(self.parse_b_expr()?);
                self.expect_kw("and")?;
                let high = Box::new(self.expr_bp(operand)?);
                Ok(Expr::Between {
                    expr: left,
                    low,
                    high,
                    negated,
                    symmetric,
                    span: self.span_from(op_start),
                })
            }
            "in" => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                if is_query_start_kw(self.peek().keyword()) {
                    let query = Box::new(self.parse_query()?);
                    self.expect(&TokenKind::RParen)?;
                    return Ok(Expr::InSubquery {
                        expr: left,
                        query,
                        negated,
                        span: self.span_from(op_start),
                    });
                }
                let list = self.parse_expr_list()?;
                self.expect(&TokenKind::RParen)?;
                Ok(Expr::InList {
                    expr: left,
                    list,
                    negated,
                    span: self.span_from(op_start),
                })
            }
            "like" | "ilike" => {
                self.advance();
                let pattern = Box::new(self.expr_bp(operand)?);
                let escape = if self.eat_kw("escape") {
                    Some(Box::new(self.expr_bp(operand)?))
                } else {
                    None
                };
                Ok(Expr::Like {
                    expr: left,
                    pattern,
                    escape,
                    negated,
                    case_insensitive: kw == "ilike",
                    span: self.span_from(op_start),
                })
            }
            "similar" => Err(self.not_supported("SIMILAR TO")),
            _ => Err(self.unexpected()),
        }
    }

    fn parse_prefix(&mut self, ctx: Ctx) -> Result<Expr> {
        let t = self.peek().clone();
        let start = t.span.start;
        match &t.kind {
            TokenKind::Op(op) if op == "-" || op == "+" => {
                self.advance();
                let operand = self.expr_bp(Ctx::new(P_UMINUS, Assoc::Right, ctx.restricted))?;
                let span = self.span_from(start);
                if op == "-" {
                    match operand {
                        Expr::Literal {
                            value: Literal::Integer(s),
                            ..
                        } => {
                            return Ok(Expr::Literal {
                                value: Literal::Integer(negate_number(&s)),
                                span,
                            });
                        }
                        Expr::Literal {
                            value: Literal::Decimal(s),
                            ..
                        } => {
                            return Ok(Expr::Literal {
                                value: Literal::Decimal(negate_number(&s)),
                                span,
                            });
                        }
                        _ => {}
                    }
                    return Ok(Expr::UnaryOp {
                        op: op.clone(),
                        expr: Box::new(operand),
                        span,
                    });
                }
                Ok(Expr::UnaryOp {
                    op: op.clone(),
                    expr: Box::new(operand),
                    span,
                })
            }
            TokenKind::Op(op) if op_prec(op) == Some(P_OP) => {
                self.advance();
                let operand = self.expr_bp(Ctx::new(P_OP, Assoc::Left, ctx.restricted))?;
                Ok(Expr::UnaryOp {
                    op: op.clone(),
                    expr: Box::new(operand),
                    span: self.span_from(start),
                })
            }
            TokenKind::Word {
                value,
                quoted: false,
            } if value == "not" && !ctx.restricted => {
                self.advance();
                let operand = self.expr_bp(Ctx::new(P_NOT, Assoc::Right, false))?;
                Ok(Expr::Not {
                    expr: Box::new(operand),
                    span: self.span_from(start),
                })
            }
            _ => self.parse_primary(ctx.restricted),
        }
    }

    fn literal(&mut self, value: Literal) -> Expr {
        let span = self.advance().span;
        Expr::Literal { value, span }
    }

    /// `c_expr`.
    fn parse_primary(&mut self, restricted: bool) -> Result<Expr> {
        let t = self.peek().clone();
        match &t.kind {
            TokenKind::Integer(s) => Ok(self.literal(Literal::Integer(s.clone()))),
            TokenKind::Decimal(s) => Ok(self.literal(Literal::Decimal(s.clone()))),
            TokenKind::String(s) => Ok(self.literal(Literal::String(s.clone()))),
            TokenKind::Param(n) => {
                self.advance();
                self.reject_indirection()?;
                Ok(Expr::Parameter {
                    index: *n,
                    span: t.span,
                })
            }
            TokenKind::LParen => self.parse_paren_expr(),
            TokenKind::Word { quoted: true, .. } => self.parse_name_expr(),
            TokenKind::Word {
                value,
                quoted: false,
            } => self.parse_keyword_primary(&value.clone(), restricted),
            _ => Err(self.unexpected()),
        }
    }

    fn reject_indirection(&self) -> Result<()> {
        match self.peek_kind() {
            TokenKind::LBracket => Err(self.not_supported("array subscripting")),
            TokenKind::Dot => Err(self.not_supported("field selection")),
            _ => Ok(()),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_keyword_primary(&mut self, kw: &str, restricted: bool) -> Result<Expr> {
        let start = self.start();
        let next_is_paren = self.peek_nth(1).kind == TokenKind::LParen;
        let session_kind = match kw {
            "current_user" => Some(SessionValueKind::CurrentUser),
            "session_user" => Some(SessionValueKind::SessionUser),
            "current_role" => Some(SessionValueKind::CurrentRole),
            "user" => Some(SessionValueKind::User),
            "current_catalog" => Some(SessionValueKind::CurrentCatalog),
            "current_schema" if !next_is_paren => Some(SessionValueKind::CurrentSchema),
            _ => None,
        };
        if let Some(kind) = session_kind {
            let span = self.advance().span;
            return Ok(Expr::SessionValue { kind, span });
        }
        match kw {
            "true" => return Ok(self.literal(Literal::Bool(true))),
            "false" => return Ok(self.literal(Literal::Bool(false))),
            "null" => return Ok(self.literal(Literal::Null)),
            "case" => return self.parse_case(),
            "cast" => return self.parse_cast(),
            "default" if !restricted => {
                let span = self.advance().span;
                self.defaults.push(span);
                return Ok(Expr::Default { span });
            }
            "system_user" | "current_date" | "current_time" | "current_timestamp" | "localtime"
            | "localtimestamp" => {
                return Err(self.not_supported(&kw.to_ascii_uppercase()));
            }
            "array"
                if matches!(
                    self.peek_nth(1).kind,
                    TokenKind::LBracket | TokenKind::LParen
                ) =>
            {
                return Err(self.not_supported("ARRAY constructors"));
            }
            _ => {}
        }
        if next_is_paren {
            match kw {
                "exists" => {
                    self.advance();
                    self.expect(&TokenKind::LParen)?;
                    let query = Box::new(self.parse_query()?);
                    self.expect(&TokenKind::RParen)?;
                    return Ok(Expr::Exists {
                        query,
                        span: self.span_from(start),
                    });
                }
                "coalesce" => {
                    self.advance();
                    self.expect(&TokenKind::LParen)?;
                    let args = self.parse_expr_list()?;
                    self.expect(&TokenKind::RParen)?;
                    return Ok(Expr::Coalesce {
                        args,
                        span: self.span_from(start),
                    });
                }
                "nullif" => {
                    self.advance();
                    self.expect(&TokenKind::LParen)?;
                    let left = Box::new(self.parse_a_expr()?);
                    self.expect(&TokenKind::Comma)?;
                    let right = Box::new(self.parse_a_expr()?);
                    self.expect(&TokenKind::RParen)?;
                    return Ok(Expr::NullIf {
                        left,
                        right,
                        span: self.span_from(start),
                    });
                }
                "substring" => return self.parse_substring(),
                "position" => return self.parse_position(),
                "trim" => return self.parse_trim(),
                "extract" => return self.parse_extract(),
                "greatest" | "least" | "row" | "overlay" | "treat" | "normalize" | "grouping"
                | "merge_action" => {
                    return Err(self.not_supported(&kw.to_ascii_uppercase()));
                }
                _ if (kw.starts_with("xml") || kw.starts_with("json"))
                    && keyword_category(kw) == KeywordCategory::ColName =>
                {
                    return Err(self.not_supported(&kw.to_ascii_uppercase()));
                }
                _ => {}
            }
        }
        if self.at_typed_literal_keyword() {
            return self.parse_keyword_typed_literal();
        }
        match keyword_category(kw) {
            KeywordCategory::Reserved => Err(self.unexpected()),
            KeywordCategory::TypeFuncName
                if !next_is_paren && !matches!(self.peek_nth(1).kind, TokenKind::String(_)) =>
            {
                Err(self.unexpected())
            }
            _ => self.parse_name_expr(),
        }
    }

    /// Column reference, function call, or `name 'literal'` typed literal.
    fn parse_name_expr(&mut self) -> Result<Expr> {
        let start = self.start();
        let first_tok = self.advance();
        let TokenKind::Word { value, quoted } = first_tok.kind else {
            return Err(Error::internal("parse_name_expr: not a word"));
        };
        let category = if quoted {
            KeywordCategory::Unreserved
        } else {
            keyword_category(&value)
        };
        let mut parts = vec![Ident {
            value,
            quoted,
            span: first_tok.span,
        }];
        while self.peek_kind() == &TokenKind::Dot {
            if self.peek_nth(1).is_op("*") {
                self.advance();
                return Err(self.not_supported("qualified \"*\" in an expression"));
            }
            self.advance();
            parts.push(self.parse_col_label()?);
        }
        let single_col_name = parts.len() == 1 && category == KeywordCategory::ColName;
        if self.peek_kind() == &TokenKind::LParen {
            if single_col_name {
                return Err(self.unexpected());
            }
            let name = ObjectName {
                parts,
                span: self.span_from(start),
            };
            return self.parse_function_call(name, start);
        }
        if let TokenKind::String(s) = self.peek_kind().clone()
            && !single_col_name
        {
            let type_name = TypeName {
                names: parts,
                modifiers: Vec::new(),
                array_bounds: Vec::new(),
                span: self.span_from(start),
            };
            return Ok(self.typed_literal(type_name, s, start));
        }
        self.reject_indirection_brackets()?;
        Ok(Expr::Column {
            parts,
            span: self.span_from(start),
        })
    }

    fn reject_indirection_brackets(&self) -> Result<()> {
        if self.peek_kind() == &TokenKind::LBracket {
            return Err(self.not_supported("array subscripting"));
        }
        Ok(())
    }

    /// Consumes the string token and builds `type 'literal'`.
    fn typed_literal(&mut self, type_name: TypeName, s: String, start: u32) -> Expr {
        let lit_span = self.advance().span;
        Expr::Cast {
            expr: Box::new(Expr::Literal {
                value: Literal::String(s),
                span: lit_span,
            }),
            type_name,
            syntax: CastSyntax::TypedLiteral,
            span: self.span_from(start),
        }
    }

    fn parse_function_call(&mut self, name: ObjectName, start: u32) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        let mut args = Vec::new();
        let mut distinct = false;
        let mut star = false;
        if self.eat(&TokenKind::RParen) {
            // no arguments
        } else if self.is_op("*") && self.peek_nth(1).kind == TokenKind::RParen {
            self.advance();
            self.advance();
            star = true;
        } else {
            if self.eat_kw("distinct") {
                distinct = true;
            } else {
                self.eat_kw("all");
            }
            loop {
                if self.is_kw("variadic") {
                    return Err(self.not_supported("VARIADIC"));
                }
                if matches!(self.peek_kind(), TokenKind::Word { .. })
                    && (self.peek_nth(1).is_op("=>")
                        || self.peek_nth(1).kind == TokenKind::ColonEquals)
                {
                    return Err(self.not_supported("named arguments"));
                }
                args.push(self.parse_a_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            if self.is_kw("order") {
                return Err(self.not_supported("ORDER BY in aggregate calls"));
            }
            self.expect(&TokenKind::RParen)?;
        }
        if self.is_kw("within") && self.nth_is_kw(1, "group") {
            return Err(self.not_supported("WITHIN GROUP"));
        }
        if self.is_kw("filter") && self.peek_nth(1).kind == TokenKind::LParen {
            return Err(self.not_supported("FILTER"));
        }
        if self.is_kw("over") {
            return Err(self.not_supported("window functions"));
        }
        if let TokenKind::String(s) = self.peek_kind().clone()
            && !distinct
            && !star
            && !args.is_empty()
        {
            // `func_name '(' args ')' Sconst`: a typed literal with modifiers.
            let type_name = TypeName {
                names: name.parts,
                modifiers: args,
                array_bounds: Vec::new(),
                span: self.span_from(start),
            };
            return Ok(self.typed_literal(type_name, s, start));
        }
        Ok(Expr::Function {
            name,
            args,
            distinct,
            star,
            span: self.span_from(start),
        })
    }

    /// `(expr)`, `(subquery)`.
    fn parse_paren_expr(&mut self) -> Result<Expr> {
        let start = self.start();
        if is_query_start_kw(self.peek_nth(1).keyword()) {
            self.advance();
            let query = Box::new(self.parse_query()?);
            self.expect(&TokenKind::RParen)?;
            self.reject_indirection()?;
            return Ok(Expr::Subquery {
                query,
                span: self.span_from(start),
            });
        }
        self.advance();
        let inner = self.parse_a_expr()?;
        if self.peek_kind() == &TokenKind::Comma {
            return Err(self.not_supported("row constructors"));
        }
        // `((SELECT ...) UNION ...)`: the parenthesized subquery was the
        // first operand of a larger query.
        let continues_query = [
            "union",
            "intersect",
            "except",
            "order",
            "limit",
            "offset",
            "fetch",
        ]
        .iter()
        .any(|kw| self.is_kw(kw));
        if continues_query && let Expr::Subquery { query, .. } = inner {
            let body = Self::unwrap_query(*query);
            let query = Box::new(self.continue_query(body, start + 1)?);
            self.expect(&TokenKind::RParen)?;
            self.reject_indirection()?;
            return Ok(Expr::Subquery {
                query,
                span: self.span_from(start),
            });
        }
        self.expect(&TokenKind::RParen)?;
        self.reject_indirection()?;
        Ok(inner)
    }

    /// A parenthesized query as a set-operation operand: its body if it has
    /// no ORDER BY / LIMIT / OFFSET, otherwise `QueryBody::Nested`.
    pub(super) fn unwrap_query(q: Query) -> QueryBody {
        if q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none() {
            q.body
        } else {
            QueryBody::Nested(Box::new(q))
        }
    }

    fn parse_case(&mut self) -> Result<Expr> {
        let start = self.advance().span.start;
        let operand = if self.is_kw("when") {
            None
        } else {
            Some(Box::new(self.parse_a_expr()?))
        };
        let mut whens = Vec::new();
        while self.is_kw("when") {
            let wstart = self.advance().span.start;
            let condition = self.parse_a_expr()?;
            self.expect_kw("then")?;
            let result = self.parse_a_expr()?;
            whens.push(WhenClause {
                condition,
                result,
                span: self.span_from(wstart),
            });
        }
        if whens.is_empty() {
            return Err(self.unexpected());
        }
        let else_result = if self.eat_kw("else") {
            Some(Box::new(self.parse_a_expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            whens,
            else_result,
            span: self.span_from(start),
        })
    }

    fn parse_cast(&mut self) -> Result<Expr> {
        let start = self.advance().span.start;
        self.expect(&TokenKind::LParen)?;
        let expr = Box::new(self.parse_a_expr()?);
        self.expect_kw("as")?;
        let type_name = self.parse_type_name(false)?;
        self.expect(&TokenKind::RParen)?;
        Ok(Expr::Cast {
            expr,
            type_name,
            syntax: CastSyntax::Cast,
            span: self.span_from(start),
        })
    }

    /// Consumes a special-syntax function keyword and its `(`; returns the
    /// keyword's span.
    fn special_function_start(&mut self) -> Result<Span> {
        let span = self.advance().span;
        self.expect(&TokenKind::LParen)?;
        Ok(span)
    }

    fn special_function(&mut self, name: &str, name_span: Span, args: Vec<Expr>) -> Expr {
        Expr::Function {
            name: ObjectName {
                parts: vec![ident(name, name_span)],
                span: name_span,
            },
            args,
            distinct: false,
            star: false,
            span: self.span_from(name_span.start),
        }
    }

    /// `SUBSTRING(x FROM a FOR b)` and the plain call form.
    fn parse_substring(&mut self) -> Result<Expr> {
        let name_span = self.special_function_start()?;
        let mut args = Vec::new();
        if self.peek_kind() != &TokenKind::RParen {
            let first = self.parse_a_expr()?;
            if self.eat_kw("from") {
                let from = self.parse_a_expr()?;
                args = vec![first, from];
                if self.eat_kw("for") {
                    args.push(self.parse_a_expr()?);
                }
            } else if self.eat_kw("for") {
                let count = self.parse_a_expr()?;
                if self.eat_kw("from") {
                    let from = self.parse_a_expr()?;
                    args = vec![first, from, count];
                } else {
                    let one = Expr::Literal {
                        value: Literal::Integer("1".into()),
                        span: count.span(),
                    };
                    args = vec![first, one, count];
                }
            } else if self.is_kw("similar") {
                return Err(self.not_supported("SUBSTRING ... SIMILAR"));
            } else {
                args.push(first);
                while self.eat(&TokenKind::Comma) {
                    args.push(self.parse_a_expr()?);
                }
            }
        }
        self.expect(&TokenKind::RParen)?;
        Ok(self.special_function("substring", name_span, args))
    }

    /// `POSITION(a IN b)` → `position(b, a)`.
    fn parse_position(&mut self) -> Result<Expr> {
        let name_span = self.special_function_start()?;
        let needle = self.parse_b_expr()?;
        self.expect_kw("in")?;
        let haystack = self.parse_b_expr()?;
        self.expect(&TokenKind::RParen)?;
        Ok(self.special_function("position", name_span, vec![haystack, needle]))
    }

    /// `TRIM([BOTH|LEADING|TRAILING] [chars] FROM x)` → `btrim/ltrim/rtrim`.
    fn parse_trim(&mut self) -> Result<Expr> {
        let name_span = self.special_function_start()?;
        let name = if self.eat_kw("leading") {
            "ltrim"
        } else if self.eat_kw("trailing") {
            "rtrim"
        } else {
            self.eat_kw("both");
            "btrim"
        };
        let args = if self.eat_kw("from") {
            self.parse_expr_list()?
        } else {
            let first = self.parse_a_expr()?;
            if self.eat_kw("from") {
                let mut list = self.parse_expr_list()?;
                list.push(first);
                list
            } else {
                let mut list = vec![first];
                while self.eat(&TokenKind::Comma) {
                    list.push(self.parse_a_expr()?);
                }
                list
            }
        };
        self.expect(&TokenKind::RParen)?;
        Ok(self.special_function(name, name_span, args))
    }

    /// `EXTRACT(field FROM x)` → `extract('field', x)`.
    fn parse_extract(&mut self) -> Result<Expr> {
        let name_span = self.special_function_start()?;
        let t = self.peek().clone();
        let field = match &t.kind {
            TokenKind::Word { value, .. } => value.clone(),
            TokenKind::String(s) => s.clone(),
            _ => return Err(self.unexpected()),
        };
        self.advance();
        self.expect_kw("from")?;
        let source = self.parse_a_expr()?;
        self.expect(&TokenKind::RParen)?;
        let field = Expr::Literal {
            value: Literal::String(field),
            span: t.span,
        };
        Ok(self.special_function("extract", name_span, vec![field, source]))
    }

    // ----- type names ---------------------------------------------------

    /// True if the current keyword starts a `ConstTypename Sconst` typed
    /// literal (`integer '1'`, `varchar(3) 'x'`, `double precision '1'`).
    fn at_typed_literal_keyword(&self) -> bool {
        let next = self.peek_nth(1);
        let string = matches!(next.kind, TokenKind::String(_));
        let paren = next.kind == TokenKind::LParen;
        match self.peek().keyword() {
            Some("int" | "integer" | "smallint" | "bigint" | "real" | "boolean" | "json") => string,
            Some("float" | "decimal" | "dec" | "numeric" | "varchar" | "interval") => {
                string || paren
            }
            Some("double") => next.is_kw("precision"),
            Some("char" | "character" | "nchar" | "bit") => {
                string || paren || next.is_kw("varying")
            }
            Some("national") => next.is_kw("char") || next.is_kw("character"),
            Some("timestamp" | "time") => {
                string || paren || next.is_kw("with") || next.is_kw("without")
            }
            _ => false,
        }
    }

    fn parse_keyword_typed_literal(&mut self) -> Result<Expr> {
        let start = self.start();
        let type_name = self.parse_type_name(true)?;
        match self.peek_kind().clone() {
            TokenKind::String(s) => Ok(self.typed_literal(type_name, s, start)),
            _ => Err(self.unexpected()),
        }
    }

    /// An integer constant that fits in int4 (`Iconst`).
    fn parse_iconst(&mut self) -> Result<(i64, Span)> {
        if let TokenKind::Integer(s) = self.peek_kind()
            && let Ok(v) = s.parse::<i32>()
        {
            let span = self.advance().span;
            return Ok((i64::from(v), span));
        }
        Err(self.unexpected())
    }

    /// `'(' Iconst ')'`, if present, as a modifier list.
    fn opt_iconst_modifier(&mut self) -> Result<Vec<Expr>> {
        if !self.eat(&TokenKind::LParen) {
            return Ok(Vec::new());
        }
        let (v, span) = self.parse_iconst()?;
        self.expect(&TokenKind::RParen)?;
        Ok(vec![Expr::Literal {
            value: Literal::Integer(v.to_string()),
            span,
        }])
    }

    /// `'(' expr_list ')'`, if present.
    fn opt_type_modifiers(&mut self) -> Result<Vec<Expr>> {
        if !self.eat(&TokenKind::LParen) {
            return Ok(Vec::new());
        }
        let list = self.parse_expr_list()?;
        self.expect(&TokenKind::RParen)?;
        Ok(list)
    }

    /// `Typename`. In a constant context (`ConstTypename` of a typed literal)
    /// `char` / `bit` without a length get no modifier instead of length 1.
    #[allow(clippy::too_many_lines)]
    pub(super) fn parse_type_name(&mut self, const_ctx: bool) -> Result<TypeName> {
        let start = self.start();
        let kw = self.peek().keyword().map(str::to_string);
        let simple = |name: &'static str| -> Option<&'static str> { Some(name) };
        let fixed = match kw.as_deref() {
            Some("int" | "integer") => simple("int4"),
            Some("smallint") => simple("int2"),
            Some("bigint") => simple("int8"),
            Some("real") => simple("float4"),
            Some("boolean") => simple("bool"),
            Some("json") => simple("json"),
            _ => None,
        };
        let (name, modifiers): (&str, Vec<Expr>) = if let Some(name) = fixed {
            self.advance();
            (name, Vec::new())
        } else {
            match kw.as_deref() {
                Some("float") => {
                    self.advance();
                    let mut name = "float8";
                    if self.eat(&TokenKind::LParen) {
                        let (p, span) = self.parse_iconst()?;
                        if p < 1 {
                            return Err(Error::new(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "precision for type float must be at least 1 bit",
                            )
                            .with_span(span));
                        }
                        if p > 53 {
                            return Err(Error::new(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "precision for type float must be less than 54 bits",
                            )
                            .with_span(span));
                        }
                        if p <= 24 {
                            name = "float4";
                        }
                        self.expect(&TokenKind::RParen)?;
                    }
                    (name, Vec::new())
                }
                Some("double") => {
                    self.advance();
                    self.expect_kw("precision")?;
                    ("float8", Vec::new())
                }
                Some("decimal" | "dec" | "numeric") => {
                    self.advance();
                    ("numeric", self.opt_type_modifiers()?)
                }
                Some("bit") => {
                    self.advance();
                    let varying = self.eat_kw("varying");
                    let mods = self.opt_type_modifiers()?;
                    if varying {
                        ("varbit", mods)
                    } else {
                        ("bit", self.default_length_one(mods, const_ctx, start))
                    }
                }
                Some("character" | "char" | "nchar" | "national") => {
                    if self.eat_kw("national") {
                        if !self.eat_kw("character") {
                            self.expect_kw("char")?;
                        }
                    } else {
                        self.advance();
                    }
                    let varying = self.eat_kw("varying");
                    let mods = self.opt_iconst_modifier()?;
                    if varying {
                        ("varchar", mods)
                    } else {
                        ("bpchar", self.default_length_one(mods, const_ctx, start))
                    }
                }
                Some("varchar") => {
                    self.advance();
                    ("varchar", self.opt_iconst_modifier()?)
                }
                Some(k @ ("timestamp" | "time")) => {
                    let base = if k == "timestamp" {
                        "timestamp"
                    } else {
                        "time"
                    };
                    self.advance();
                    let mods = self.opt_iconst_modifier()?;
                    let tz = if self.eat_kw("with") {
                        self.expect_kw("time")?;
                        self.expect_kw("zone")?;
                        true
                    } else {
                        if self.eat_kw("without") {
                            self.expect_kw("time")?;
                            self.expect_kw("zone")?;
                        }
                        false
                    };
                    let name = match (base, tz) {
                        ("timestamp", true) => "timestamptz",
                        ("timestamp", false) => "timestamp",
                        (_, true) => "timetz",
                        _ => "time",
                    };
                    (name, mods)
                }
                Some("interval") => {
                    self.advance();
                    let mods = self.opt_iconst_modifier()?;
                    if ["year", "month", "day", "hour", "minute", "second"]
                        .iter()
                        .any(|f| self.is_kw(f))
                    {
                        return Err(self.not_supported("INTERVAL fields"));
                    }
                    ("interval", mods)
                }
                Some("setof") => return Err(self.not_supported("SETOF")),
                _ => return self.parse_generic_type(start),
            }
        };
        let name_span = self.span_from(start);
        let array_bounds = self.parse_array_bounds()?;
        Ok(TypeName {
            names: vec![ident(name, name_span)],
            modifiers,
            array_bounds,
            span: self.span_from(start),
        })
    }

    /// `char` / `bit` without a length mean length 1 outside constants.
    fn default_length_one(&self, mods: Vec<Expr>, const_ctx: bool, start: u32) -> Vec<Expr> {
        if mods.is_empty() && !const_ctx {
            vec![Expr::Literal {
                value: Literal::Integer("1".into()),
                span: self.span_from(start),
            }]
        } else {
            mods
        }
    }

    /// `GenericType`: `type_function_name ('.' ColLabel)* [modifiers]`.
    fn parse_generic_type(&mut self, start: u32) -> Result<TypeName> {
        let first =
            self.take_ident(&[KeywordCategory::Unreserved, KeywordCategory::TypeFuncName])?;
        let mut names = vec![first];
        while self.eat(&TokenKind::Dot) {
            names.push(self.parse_col_label()?);
        }
        let modifiers = self.opt_type_modifiers()?;
        let array_bounds = self.parse_array_bounds()?;
        Ok(TypeName {
            names,
            modifiers,
            array_bounds,
            span: self.span_from(start),
        })
    }

    /// `[]`, `[n]`, ... or `ARRAY [ '[' n ']' ]`.
    fn parse_array_bounds(&mut self) -> Result<Vec<Option<i64>>> {
        let mut bounds = Vec::new();
        loop {
            if self.eat(&TokenKind::LBracket) {
                if self.eat(&TokenKind::RBracket) {
                    bounds.push(None);
                } else {
                    let (n, _) = self.parse_iconst()?;
                    self.expect(&TokenKind::RBracket)?;
                    bounds.push(Some(n));
                }
            } else if self.eat_kw("array") {
                if self.eat(&TokenKind::LBracket) {
                    let (n, _) = self.parse_iconst()?;
                    self.expect(&TokenKind::RBracket)?;
                    bounds.push(Some(n));
                } else {
                    bounds.push(None);
                }
                break;
            } else {
                break;
            }
        }
        Ok(bounds)
    }
}
