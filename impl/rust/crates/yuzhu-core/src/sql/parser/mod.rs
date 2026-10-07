//! Hand-written recursive-descent parser (Pratt parsing for expressions),
//! following PostgreSQL's `gram.y`.
//!
//! Syntax errors use PostgreSQL's wording and position: the token where the
//! parse cannot continue is reported as `syntax error at or near "<raw
//! token text>"`, or `syntax error at end of input`. Lexical errors are
//! reported only when the parser reaches them.
//!
//! Some syntax that has no room in the AST (row constructors, array
//! subscripts, window functions, ...) is rejected here with `0A000`.

mod copy;
mod ddl;
mod ddl_index;
mod dml;
mod expr;
mod misc;
mod select;
mod seq;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_query;
#[cfg(test)]
mod tests_stmt;

use super::ast::{Checkpoint, Expr, Ident, ObjectName, Statement};
use super::lexer::tokenize;
use super::stack::{check_stack_depth, stack_depth_error};
use super::token::{KeywordCategory, Token, TokenKind, keyword_category};
use crate::error::{Error, Result, Span, sqlstate};

/// Parses a query string into statements (`;`-separated). An empty string
/// (only whitespace / comments) gives an empty vector.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut p = Parser::new(sql);
    let mut stmts = Vec::new();
    loop {
        while p.eat(&TokenKind::Semicolon) {}
        if p.at_eof() {
            break;
        }
        p.height = 0;
        stmts.push(p.parse_statement()?);
        if !p.eat(&TokenKind::Semicolon) && !p.at_eof() {
            return Err(p.unexpected());
        }
    }
    p.check_defaults()?;
    Ok(stmts)
}

/// Parses a single expression (used to re-parse DEFAULT / CHECK text stored
/// in the catalog). Spans are relative to `sql`.
pub fn parse_expr(sql: &str) -> Result<Expr> {
    let mut p = Parser::new(sql);
    let e = p.parse_a_expr()?;
    if !p.at_eof() {
        return Err(p.unexpected());
    }
    p.check_defaults()?;
    Ok(e)
}

/// Parser state over a fully tokenized query.
pub(crate) struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    lex_error: Option<Error>,
    /// End offset of the last consumed token.
    prev_end: u32,
    /// Spans of `DEFAULT` expressions not (yet) known to be in an allowed
    /// position (top level of an INSERT's VALUES row or an UPDATE SET).
    defaults: Vec<Span>,
    /// Number of currently open nesting levels (see [`Parser::nested`]).
    depth: usize,
    /// Upper bound of the height of the trees completed in the innermost
    /// open level so far.
    height: usize,
}

/// The maximum height of the syntax tree (and the parser's recursion depth),
/// counted in nesting levels: every expression / query / join level and
/// every link of a left-associative operator chain counts as one.
///
/// Deeper input fails with `54001 stack depth limit exceeded`, as does
/// recursion that uses up the thread's stack budget first
/// ([`check_stack_depth`]). Later stages (analyzer, executor, `Drop` of the
/// tree) recurse over the tree, so bounding its height here is what keeps a
/// single query from overflowing a connection thread's stack.
pub const MAX_NESTING_DEPTH: usize = 5000;

impl<'a> Parser<'a> {
    pub(crate) fn new(sql: &'a str) -> Self {
        let (tokens, lex_error) = tokenize(sql);
        Parser {
            sql,
            tokens,
            pos: 0,
            lex_error,
            prev_end: 0,
            defaults: Vec::new(),
            depth: 0,
            height: 0,
        }
    }

    /// Parser position for backtracking (see [`Parser::restore`]).
    pub(super) fn snapshot(&self) -> (usize, u32, usize, usize, usize) {
        (
            self.pos,
            self.prev_end,
            self.depth,
            self.height,
            self.defaults.len(),
        )
    }

    pub(super) fn restore(&mut self, s: (usize, u32, usize, usize, usize)) {
        self.pos = s.0;
        self.prev_end = s.1;
        self.depth = s.2;
        self.height = s.3;
        self.defaults.truncate(s.4);
    }

    // ----- nesting depth guard -----------------------------------------

    /// Runs `f` as one nesting level of the tree being built: fails with
    /// `54001` when the recursion depth or the height of the produced tree
    /// would exceed [`MAX_NESTING_DEPTH`], or when the thread's stack
    /// budget is used up ([`check_stack_depth`]). The check happens before the
    /// tree gets any deeper, so neither parsing nor dropping a partially
    /// built tree on an error path can overflow the stack.
    pub(super) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(stack_depth_error());
        }
        check_stack_depth()?;
        self.depth += 1;
        let outer = std::mem::replace(&mut self.height, 0);
        let result = f(self);
        self.depth -= 1;
        self.height = outer.max(self.height + 1);
        if result.is_ok() && self.height + self.depth > MAX_NESTING_DEPTH {
            return Err(stack_depth_error());
        }
        result
    }

    /// Accounts for one more link of a left-associative chain (`a + b + c`,
    /// `x UNION y UNION z`, `a JOIN b JOIN c`) built in the current level:
    /// each link makes the tree one level higher without any recursion.
    pub(super) fn chain_link(&mut self) -> Result<()> {
        self.height += 1;
        if self.height + self.depth > MAX_NESTING_DEPTH {
            return Err(stack_depth_error());
        }
        Ok(())
    }

    // ----- token access ------------------------------------------------

    pub(super) fn peek(&self) -> &Token {
        self.peek_nth(0)
    }

    /// The token `n` positions ahead (the last token, `Eof` or `LexError`,
    /// repeats forever).
    pub(super) fn peek_nth(&self, n: usize) -> &Token {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i]
    }

    pub(super) fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    pub(super) fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    pub(super) fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if !matches!(t.kind, TokenKind::Eof | TokenKind::LexError) {
            self.pos += 1;
            self.prev_end = t.span.end;
        }
        t
    }

    pub(super) fn start(&self) -> u32 {
        self.peek().span.start
    }

    /// Span from `start` to the end of the last consumed token.
    pub(super) fn span_from(&self, start: u32) -> Span {
        Span::new(start, self.prev_end.max(start))
    }

    pub(super) fn text(&self, span: Span) -> &'a str {
        &self.sql[span.start as usize..span.end as usize]
    }

    pub(super) fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek_kind() == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    pub(super) fn expect(&mut self, kind: &TokenKind) -> Result<Token> {
        if self.peek_kind() == kind {
            Ok(self.advance())
        } else {
            Err(self.unexpected())
        }
    }

    pub(super) fn is_kw(&self, kw: &str) -> bool {
        self.peek().is_kw(kw)
    }

    pub(super) fn nth_is_kw(&self, n: usize, kw: &str) -> bool {
        self.peek_nth(n).is_kw(kw)
    }

    pub(super) fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    pub(super) fn expect_kw(&mut self, kw: &str) -> Result<Token> {
        if self.is_kw(kw) {
            Ok(self.advance())
        } else {
            Err(self.unexpected())
        }
    }

    pub(super) fn is_op(&self, op: &str) -> bool {
        self.peek().is_op(op)
    }

    pub(super) fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.advance();
            true
        } else {
            false
        }
    }

    // ----- errors ------------------------------------------------------

    /// The syntax error for the current token (or the pending lexer error).
    pub(super) fn unexpected(&self) -> Error {
        let t = self.peek();
        match &t.kind {
            TokenKind::LexError => self
                .lex_error
                .clone()
                .unwrap_or_else(|| Error::internal("missing lexer error")),
            TokenKind::Eof => Error::syntax_at(t.span, "syntax error at end of input"),
            _ => Error::syntax_at(
                t.span,
                format!("syntax error at or near \"{}\"", self.text(t.span)),
            ),
        }
    }

    /// `0A000` with the current token's position.
    pub(super) fn not_supported(&self, what: &str) -> Error {
        if matches!(self.peek_kind(), TokenKind::LexError) {
            return self.unexpected();
        }
        Error::not_supported(format!("{what} is not supported yet")).with_span(self.peek().span)
    }

    fn check_defaults(&self) -> Result<()> {
        match self.defaults.iter().min_by_key(|s| s.start) {
            Some(span) => Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                "DEFAULT is not allowed in this context",
            )
            .with_span(*span)),
            None => Ok(()),
        }
    }

    /// Marks a top-level `DEFAULT` expression as allowed.
    pub(super) fn allow_default(&mut self, e: &Expr) {
        if let Expr::Default { span } = e {
            self.defaults.retain(|s| s != span);
        }
    }

    // ----- names -------------------------------------------------------

    /// The current word if it is usable in a position accepting the given
    /// keyword categories (identifiers and quoted words always are).
    fn word_in(&self, ok: &[KeywordCategory]) -> Option<(String, bool)> {
        match self.peek_kind() {
            TokenKind::Word { value, quoted } => {
                if *quoted || ok.contains(&keyword_category(value)) {
                    Some((value.clone(), *quoted))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn take_ident(&mut self, ok: &[KeywordCategory]) -> Result<Ident> {
        match self.word_in(ok) {
            Some((value, quoted)) => {
                let span = self.advance().span;
                Ok(Ident {
                    value,
                    quoted,
                    span,
                })
            }
            None => Err(self.unexpected()),
        }
    }

    /// True if the current token is a `ColId` (identifier, unreserved or
    /// column-name keyword).
    pub(super) fn at_col_id(&self) -> bool {
        self.word_in(COL_ID).is_some()
    }

    /// `ColId`: identifier, unreserved keyword or column-name keyword.
    pub(super) fn parse_col_id(&mut self) -> Result<Ident> {
        self.take_ident(COL_ID)
    }

    /// `ColLabel`: any identifier or keyword (used after `AS` and `.`).
    pub(super) fn parse_col_label(&mut self) -> Result<Ident> {
        self.take_ident(ALL_WORDS)
    }

    /// `NonReservedWord`: anything but a fully reserved keyword.
    pub(super) fn parse_non_reserved_word(&mut self) -> Result<Ident> {
        self.take_ident(NON_RESERVED)
    }

    /// `qualified_name` / `any_name`: `ColId ('.' ColLabel)*`.
    pub(super) fn parse_object_name(&mut self) -> Result<ObjectName> {
        let first = self.parse_col_id()?;
        let start = first.span.start;
        let mut parts = vec![first];
        while self.peek_kind() == &TokenKind::Dot {
            self.advance();
            parts.push(self.parse_col_label()?);
        }
        Ok(ObjectName {
            parts,
            span: self.span_from(start),
        })
    }

    /// `'(' ColId [, ...] ')'`.
    pub(super) fn parse_paren_name_list(&mut self) -> Result<Vec<Ident>> {
        self.expect(&TokenKind::LParen)?;
        let mut names = vec![self.parse_col_id()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.parse_col_id()?);
        }
        self.expect(&TokenKind::RParen)?;
        Ok(names)
    }

    /// `var_name`: `ColId ('.' ColId)*`, joined with dots.
    pub(super) fn parse_var_name(&mut self) -> Result<String> {
        let mut name = self.parse_col_id()?.value;
        while self.eat(&TokenKind::Dot) {
            name.push('.');
            name.push_str(&self.parse_col_id()?.value);
        }
        Ok(name)
    }

    // ----- statements --------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement> {
        let t = self.peek().clone();
        match &t.kind {
            TokenKind::LParen => Ok(Statement::Query(Box::new(self.parse_query()?))),
            TokenKind::Word {
                value,
                quoted: false,
            } => match value.as_str() {
                "select" | "values" | "table" | "with" => {
                    Ok(Statement::Query(Box::new(self.parse_query()?)))
                }
                "insert" => self.parse_insert().map(Statement::Insert),
                "update" => self.parse_update().map(Statement::Update),
                "delete" => self.parse_delete().map(Statement::Delete),
                "create" => self.parse_create(),
                "drop" => self.parse_drop(),
                "alter" => self.parse_alter(),
                "truncate" => self.parse_truncate(),
                "vacuum" | "analyze" | "analyse" => self.parse_vacuum(),
                "copy" => self.parse_copy(),
                "begin" | "start" | "commit" | "end" | "rollback" | "abort" | "savepoint"
                | "release" => self.parse_transaction().map(Statement::Transaction),
                "set" => self.parse_set().map(Statement::Set),
                "show" => self.parse_show().map(Statement::Show),
                "reset" => self.parse_reset().map(Statement::Reset),
                "explain" => self.parse_explain().map(Statement::Explain),
                "checkpoint" => {
                    let span = self.advance().span;
                    Ok(Statement::Checkpoint(Checkpoint { span }))
                }
                "prepare" | "grant" | "revoke" | "execute" | "deallocate" | "discard"
                | "listen" | "notify" | "unlisten" | "lock" | "declare" | "fetch" | "move"
                | "close" | "comment" | "merge" | "call" | "do" | "reindex" | "cluster"
                | "security" | "refresh" | "import" | "load" | "reassign" => {
                    Err(self.not_supported(&value.to_ascii_uppercase()))
                }
                _ => Err(self.unexpected()),
            },
            _ => Err(self.unexpected()),
        }
    }
}

const COL_ID: &[KeywordCategory] = &[KeywordCategory::Unreserved, KeywordCategory::ColName];
const NON_RESERVED: &[KeywordCategory] = &[
    KeywordCategory::Unreserved,
    KeywordCategory::ColName,
    KeywordCategory::TypeFuncName,
];
const ALL_WORDS: &[KeywordCategory] = &[
    KeywordCategory::Unreserved,
    KeywordCategory::ColName,
    KeywordCategory::TypeFuncName,
    KeywordCategory::Reserved,
];
