//! Queries: SELECT, VALUES, TABLE, set operations, ORDER BY / LIMIT /
//! OFFSET / FETCH, FROM items and joins.

use super::Parser;
use super::expr::is_query_start_kw;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    Cte, Distinct, Expr, Ident, JoinConstraint, JoinKind, Literal, NullsOrder, ObjectName,
    OrderByItem, Query, QueryBody, Select, SelectItem, SetOperator, SortDirection, TableAlias,
    TableRef, Values, With,
};
use crate::sql::token::{TokenKind, requires_as_label};

fn body_span(body: &QueryBody) -> Span {
    match body {
        QueryBody::Select(s) => s.span,
        QueryBody::Values(v) => v.span,
        QueryBody::SetOp { span, .. } => *span,
        QueryBody::Nested(q) => q.span,
    }
}

impl Parser<'_> {
    /// `SelectStmt`: a full query (without surrounding parentheses).
    pub(super) fn parse_query(&mut self) -> Result<Query> {
        self.nested(Self::parse_query_level)
    }

    fn parse_query_level(&mut self) -> Result<Query> {
        let start = self.start();
        let with = if self.is_kw("with") {
            Some(self.parse_with()?)
        } else {
            None
        };
        let first = self.parse_set_primary()?;
        let mut query = self.continue_query(first, start)?;
        if let Some(with) = with {
            if query.with.is_some() {
                // `WITH a AS (..) (WITH b AS (..) SELECT ..)`: keep both.
                let span = query.span;
                query = Query {
                    with: None,
                    body: QueryBody::Nested(Box::new(query)),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    span,
                };
            }
            query.with = Some(with);
        }
        Ok(query)
    }

    /// `with_clause`: `WITH [RECURSIVE] cte [, ...]`.
    fn parse_with(&mut self) -> Result<With> {
        let start = self.advance().span.start;
        // `recursive` is also a valid CTE name (`WITH recursive AS (..)`).
        let recursive = self.is_kw("recursive")
            && !self.nth_is_kw(1, "as")
            && self.peek_nth(1).kind != TokenKind::LParen
            && self.eat_kw("recursive");
        let mut ctes = vec![self.parse_cte()?];
        while self.eat(&TokenKind::Comma) {
            ctes.push(self.parse_cte()?);
        }
        if is_dml_kw(self.peek().keyword()) {
            return Err(self.not_supported("WITH clause on INSERT, UPDATE or DELETE"));
        }
        Ok(With {
            recursive,
            ctes,
            span: self.span_from(start),
        })
    }

    /// `common_table_expr`.
    fn parse_cte(&mut self) -> Result<Cte> {
        let start = self.start();
        let name = self.parse_col_id()?;
        let columns = if self.peek_kind() == &TokenKind::LParen {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        self.expect_kw("as")?;
        let materialized = if self.eat_kw("materialized") {
            Some(true)
        } else if self.is_kw("not") && self.nth_is_kw(1, "materialized") {
            self.advance();
            self.advance();
            Some(false)
        } else {
            None
        };
        self.expect(&TokenKind::LParen)?;
        if is_dml_kw(self.peek().keyword()) {
            return Err(self.not_supported("data-modifying statements in WITH"));
        }
        let query = Box::new(self.parse_query()?);
        self.expect(&TokenKind::RParen)?;
        let span = self.span_from(start);
        if self.is_kw("search") || self.is_kw("cycle") {
            return Err(self.not_supported("SEARCH and CYCLE clauses"));
        }
        Ok(Cte {
            name,
            columns,
            materialized,
            query,
            span,
        })
    }

    /// Parses the rest of a query whose first set-operation operand is
    /// `first`: set operations, then ORDER BY / LIMIT / OFFSET / FETCH.
    pub(super) fn continue_query(&mut self, first: QueryBody, start: u32) -> Result<Query> {
        let body = self.parse_set_ops(first, 1)?;
        self.finish_query(body, start)
    }

    fn set_op_prec(&self) -> u8 {
        if self.is_kw("union") || self.is_kw("except") {
            1
        } else if self.is_kw("intersect") {
            2
        } else {
            0
        }
    }

    /// Precedence climbing over UNION / EXCEPT (1) and INTERSECT (2), all
    /// left-associative.
    fn parse_set_ops(&mut self, mut left: QueryBody, min_prec: u8) -> Result<QueryBody> {
        loop {
            let prec = self.set_op_prec();
            if prec == 0 || prec < min_prec {
                return Ok(left);
            }
            let op = match self.advance().keyword() {
                Some("union") => SetOperator::Union,
                Some("intersect") => SetOperator::Intersect,
                _ => SetOperator::Except,
            };
            let all = if self.eat_kw("all") {
                true
            } else {
                self.eat_kw("distinct");
                false
            };
            self.chain_link()?;
            let primary = self.parse_set_primary()?;
            let right = self.parse_set_ops(primary, prec + 1)?;
            let start = body_span(&left).start;
            left = QueryBody::SetOp {
                op,
                all,
                left: Box::new(left),
                right: Box::new(right),
                span: self.span_from(start),
            };
        }
    }

    /// `simple_select` or `select_with_parens`.
    fn parse_set_primary(&mut self) -> Result<QueryBody> {
        match self.peek().keyword() {
            Some("select") => Ok(QueryBody::Select(Box::new(self.parse_select()?))),
            Some("values") => Ok(QueryBody::Values(self.parse_values()?)),
            Some("table") => {
                let start = self.advance().span.start;
                let name = self.parse_object_name()?;
                let span = self.span_from(start);
                Ok(QueryBody::Select(Box::new(Select {
                    distinct: None,
                    targets: vec![SelectItem::Wildcard(span)],
                    from: vec![TableRef::Table {
                        name,
                        alias: None,
                        span,
                    }],
                    selection: None,
                    group_by: Vec::new(),
                    having: None,
                    span,
                })))
            }
            _ if self.peek_kind() == &TokenKind::LParen => {
                self.advance();
                let q = self.parse_query()?;
                self.expect(&TokenKind::RParen)?;
                Ok(Self::unwrap_query(q))
            }
            _ => Err(self.unexpected()),
        }
    }

    /// ORDER BY, LIMIT / OFFSET / FETCH, locking clauses.
    fn finish_query(&mut self, body: QueryBody, start: u32) -> Result<Query> {
        let mut order_by = Vec::new();
        let mut order_start = None;
        if self.is_kw("order") {
            self.advance();
            self.expect_kw("by")?;
            order_start = Some(self.start());
            order_by.push(self.parse_order_by_item()?);
            while self.eat(&TokenKind::Comma) {
                order_by.push(self.parse_order_by_item()?);
            }
        }
        let mut limit = None;
        let mut offset = None;
        let mut limit_seen = false;
        let mut offset_seen = false;
        loop {
            if !limit_seen && self.is_kw("limit") {
                limit_seen = true;
                limit = self.parse_limit()?;
            } else if !limit_seen && self.is_kw("fetch") {
                limit_seen = true;
                limit = Some(self.parse_fetch()?);
            } else if !offset_seen && self.is_kw("offset") {
                offset_seen = true;
                self.advance();
                offset = Some(self.parse_a_expr()?);
                if !self.eat_kw("row") {
                    self.eat_kw("rows");
                }
            } else {
                break;
            }
        }
        if self.is_kw("for") {
            return Err(self.not_supported("FOR UPDATE/SHARE"));
        }
        // `(SELECT ... ORDER BY ...) LIMIT 1`: merge into the inner query.
        if let QueryBody::Nested(mut inner) = body {
            if !order_by.is_empty() {
                if !inner.order_by.is_empty() {
                    return Err(multiple("ORDER BY", order_start.unwrap_or(start)));
                }
                inner.order_by = order_by;
            }
            if let Some(l) = limit {
                if inner.limit.is_some() {
                    return Err(multiple("LIMIT", l.span().start));
                }
                inner.limit = Some(l);
            }
            if let Some(o) = offset {
                if inner.offset.is_some() {
                    return Err(multiple("OFFSET", o.span().start));
                }
                inner.offset = Some(o);
            }
            inner.span = self.span_from(start);
            return Ok(*inner);
        }
        Ok(Query {
            with: None,
            body,
            order_by,
            limit,
            offset,
            span: self.span_from(start),
        })
    }

    /// `LIMIT expr | LIMIT ALL`; returns `None` for ALL.
    fn parse_limit(&mut self) -> Result<Option<Expr>> {
        let limit_tok = self.advance();
        if self.eat_kw("all") {
            return Ok(None);
        }
        let e = self.parse_a_expr()?;
        if self.peek_kind() == &TokenKind::Comma {
            return Err(
                Error::new(sqlstate::SYNTAX_ERROR, "LIMIT #,# syntax is not supported")
                    .with_hint("Use separate LIMIT and OFFSET clauses.")
                    .with_span(limit_tok.span),
            );
        }
        Ok(Some(e))
    }

    /// `FETCH {FIRST|NEXT} [n] {ROW|ROWS} ONLY`.
    fn parse_fetch(&mut self) -> Result<Expr> {
        self.advance();
        if !self.eat_kw("first") {
            self.expect_kw("next")?;
        }
        let count = if self.is_kw("row") || self.is_kw("rows") {
            Expr::Literal {
                value: Literal::Integer("1".into()),
                span: self.peek().span,
            }
        } else {
            self.parse_a_expr()?
        };
        if !self.eat_kw("row") {
            self.expect_kw("rows")?;
        }
        if self.is_kw("with") {
            return Err(self.not_supported("FETCH ... WITH TIES"));
        }
        self.expect_kw("only")?;
        Ok(count)
    }

    pub(super) fn parse_order_by_item(&mut self) -> Result<OrderByItem> {
        let start = self.start();
        let expr = self.parse_a_expr()?;
        let direction = if self.eat_kw("asc") {
            Some(SortDirection::Asc)
        } else if self.eat_kw("desc") {
            Some(SortDirection::Desc)
        } else if self.is_kw("using") {
            return Err(self.not_supported("ORDER BY ... USING"));
        } else {
            None
        };
        let mut nulls = None;
        if self.is_kw("nulls") && (self.nth_is_kw(1, "first") || self.nth_is_kw(1, "last")) {
            self.advance();
            nulls = Some(if self.advance().is_kw("first") {
                NullsOrder::First
            } else {
                NullsOrder::Last
            });
        }
        Ok(OrderByItem {
            expr,
            direction,
            nulls,
            span: self.span_from(start),
        })
    }

    /// `VALUES (row), (row), ...`.
    pub(super) fn parse_values(&mut self) -> Result<Values> {
        let start = self.advance().span.start;
        let mut rows = Vec::new();
        loop {
            self.expect(&TokenKind::LParen)?;
            let row = self.parse_expr_list()?;
            if self.peek_kind() != &TokenKind::RParen {
                return Err(self.unexpected());
            }
            self.advance();
            rows.push(row);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Values {
            rows,
            span: self.span_from(start),
        })
    }

    fn at_empty_target_list(&self) -> bool {
        match self.peek_kind() {
            TokenKind::RParen | TokenKind::Semicolon | TokenKind::Eof => true,
            _ => matches!(
                self.peek().keyword(),
                Some(
                    "from"
                        | "where"
                        | "group"
                        | "having"
                        | "window"
                        | "order"
                        | "limit"
                        | "offset"
                        | "fetch"
                        | "for"
                        | "union"
                        | "intersect"
                        | "except"
                        | "into"
                )
            ),
        }
    }

    fn parse_select(&mut self) -> Result<Select> {
        let start = self.advance().span.start;
        let mut distinct = None;
        if self.eat_kw("distinct") {
            if self.eat_kw("on") {
                self.expect(&TokenKind::LParen)?;
                let exprs = self.parse_expr_list()?;
                self.expect(&TokenKind::RParen)?;
                distinct = Some(Distinct::On(exprs));
            } else {
                distinct = Some(Distinct::All);
            }
        } else {
            self.eat_kw("all");
        }
        let targets = if distinct.is_none() && self.at_empty_target_list() {
            Vec::new()
        } else {
            self.parse_target_list()?
        };
        if self.is_kw("into") {
            return Err(self.not_supported("SELECT INTO"));
        }
        let from = if self.eat_kw("from") {
            self.parse_from_list()?
        } else {
            Vec::new()
        };
        let selection = if self.eat_kw("where") {
            Some(self.parse_a_expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.is_kw("group") {
            self.advance();
            self.expect_kw("by")?;
            if !self.eat_kw("all") {
                self.eat_kw("distinct");
            }
            group_by = self.parse_group_by_list()?;
        }
        let having = if self.eat_kw("having") {
            Some(self.parse_a_expr()?)
        } else {
            None
        };
        if self.is_kw("window") {
            return Err(self.not_supported("WINDOW"));
        }
        Ok(Select {
            distinct,
            targets,
            from,
            selection,
            group_by,
            having,
            span: self.span_from(start),
        })
    }

    /// `group_clause` items. `()`, `ROLLUP`, `CUBE` and `GROUPING SETS` have
    /// no room in the AST.
    fn parse_group_by_list(&mut self) -> Result<Vec<Expr>> {
        let mut items = Vec::new();
        loop {
            let special = match self.peek().keyword() {
                Some("rollup" | "cube") => self.peek_nth(1).kind == TokenKind::LParen,
                Some("grouping") => self.nth_is_kw(1, "sets"),
                _ => {
                    self.peek_kind() == &TokenKind::LParen
                        && self.peek_nth(1).kind == TokenKind::RParen
                }
            };
            if special {
                return Err(Error::not_supported(
                    "GROUPING SETS / ROLLUP / CUBE / empty grouping sets are not supported yet",
                )
                .with_span(self.peek().span));
            }
            items.push(self.parse_a_expr()?);
            if !self.eat(&TokenKind::Comma) {
                return Ok(items);
            }
        }
    }

    /// `target_list`.
    pub(super) fn parse_target_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = vec![self.parse_select_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_select_item()?);
        }
        Ok(items)
    }

    /// True if the tokens at the cursor are `name(.name)*.*`.
    fn at_qualified_wildcard(&self) -> bool {
        let mut i = 0;
        loop {
            if !matches!(self.peek_nth(i).kind, TokenKind::Word { .. }) {
                return false;
            }
            if self.peek_nth(i + 1).kind != TokenKind::Dot {
                return false;
            }
            if self.peek_nth(i + 2).is_op("*") {
                return true;
            }
            i += 2;
        }
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        let start = self.start();
        if self.is_op("*") {
            let span = self.advance().span;
            return Ok(SelectItem::Wildcard(span));
        }
        if self.at_qualified_wildcard() {
            let first = self.parse_col_id()?;
            let mut parts = vec![first];
            loop {
                self.expect(&TokenKind::Dot)?;
                if self.eat_op("*") {
                    break;
                }
                parts.push(self.parse_col_label()?);
            }
            let name_end = parts.last().map_or(start, |p| p.span.end);
            let name = ObjectName {
                parts,
                span: Span::new(start, name_end),
            };
            return Ok(SelectItem::QualifiedWildcard(name, self.span_from(start)));
        }
        let expr = self.parse_a_expr()?;
        let alias = if self.eat_kw("as") {
            Some(self.parse_col_label()?)
        } else {
            self.parse_bare_label()
        };
        Ok(SelectItem::Expr {
            expr,
            alias,
            span: self.span_from(start),
        })
    }

    /// `BareColLabel`: an identifier or a keyword allowed as a label
    /// without `AS`.
    fn parse_bare_label(&mut self) -> Option<Ident> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Word { value, quoted } if quoted || !requires_as_label(&value) => {
                self.advance();
                Some(Ident {
                    value,
                    quoted,
                    span: t.span,
                })
            }
            _ => None,
        }
    }

    /// `from_list`.
    pub(super) fn parse_from_list(&mut self) -> Result<Vec<TableRef>> {
        let mut items = vec![self.parse_table_ref()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_table_ref()?);
        }
        Ok(items)
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let left = self.parse_table_primary()?;
        self.parse_joins(left)
    }

    /// The join type at the cursor, consuming `... JOIN`; `None` if no join
    /// starts here. Returns (kind, natural).
    fn parse_join_keyword(&mut self) -> Result<Option<(JoinKind, bool)>> {
        let kw = self.peek().keyword().unwrap_or_default().to_string();
        let result = match kw.as_str() {
            "cross" if self.nth_is_kw(1, "join") => {
                self.advance();
                (JoinKind::Cross, false)
            }
            "join" => (JoinKind::Inner, false),
            "inner" | "left" | "right" | "full" | "natural" => {
                self.advance();
                let natural = kw == "natural";
                let kind_kw = if natural {
                    let k = self.peek().keyword().unwrap_or_default().to_string();
                    if matches!(k.as_str(), "inner" | "left" | "right" | "full") {
                        self.advance();
                    }
                    k
                } else {
                    kw.clone()
                };
                let kind = match kind_kw.as_str() {
                    "left" => JoinKind::Left,
                    "right" => JoinKind::Right,
                    "full" => JoinKind::Full,
                    _ => JoinKind::Inner,
                };
                if kind != JoinKind::Inner {
                    self.eat_kw("outer");
                }
                (kind, natural)
            }
            _ => return Ok(None),
        };
        self.expect_kw("join")?;
        Ok(Some(result))
    }

    fn parse_joins(&mut self, left: TableRef) -> Result<TableRef> {
        self.nested(|p| p.parse_joins_level(left))
    }

    fn parse_joins_level(&mut self, mut left: TableRef) -> Result<TableRef> {
        while let Some((kind, natural)) = self.parse_join_keyword()? {
            self.chain_link()?;
            let start = table_ref_span(&left).start;
            let mut right = self.parse_table_primary()?;
            let constraint = if natural {
                JoinConstraint::Natural
            } else if kind == JoinKind::Cross {
                JoinConstraint::None
            } else {
                // `a JOIN b JOIN c ON x ON y` nests to the right.
                right = self.parse_joins(right)?;
                if self.eat_kw("on") {
                    JoinConstraint::On(self.parse_a_expr()?)
                } else if self.is_kw("using") {
                    self.advance();
                    let cols = self.parse_paren_name_list()?;
                    if self.is_kw("as") {
                        return Err(self.not_supported("JOIN USING aliases"));
                    }
                    JoinConstraint::Using(cols)
                } else {
                    return Err(self.unexpected());
                }
            };
            left = TableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                constraint,
                span: self.span_from(start),
            };
        }
        Ok(left)
    }

    /// True if the tokens from `n` on are `'('* <query start keyword>`.
    pub(super) fn parens_then_query(&self, mut n: usize) -> bool {
        while self.peek_nth(n).kind == TokenKind::LParen {
            n += 1;
        }
        is_query_start_kw(self.peek_nth(n).keyword())
    }

    fn parse_table_primary(&mut self) -> Result<TableRef> {
        self.nested(Self::parse_table_primary_level)
    }

    fn parse_table_primary_level(&mut self) -> Result<TableRef> {
        let start = self.start();
        if self.peek_kind() == &TokenKind::LParen {
            if self.parens_then_query(1) {
                // `((select 1) x JOIN ...)` starts like `((select 1) UNION ...)`:
                // when the nested form is ambiguous, try the subquery first and
                // fall back to a parenthesized join.
                if self.peek_nth(1).kind != TokenKind::LParen {
                    return self.parse_paren_subquery(start);
                }
                let saved = self.snapshot();
                let first = match self.parse_paren_subquery(start) {
                    Ok(t) => return Ok(t),
                    Err(e) => e,
                };
                let first_pos = self.pos;
                self.restore(saved);
                return match self.parse_paren_join() {
                    Ok(t) => Ok(t),
                    Err(e) if self.pos >= first_pos => Err(e),
                    Err(_) => Err(first),
                };
            }
            return self.parse_paren_join();
        }
        if self.is_kw("lateral") {
            return Err(self.not_supported("LATERAL"));
        }
        if self.is_kw("rows") && self.nth_is_kw(1, "from") {
            return Err(self.not_supported("ROWS FROM"));
        }
        let name = if self.eat_kw("only") {
            // No inheritance: `ONLY t` and `ONLY (t)` mean `t`.
            let paren = self.eat(&TokenKind::LParen);
            let name = self.parse_object_name()?;
            if paren {
                self.expect(&TokenKind::RParen)?;
            }
            name
        } else {
            let name = self.parse_object_name()?;
            if self.peek_kind() == &TokenKind::LParen {
                return self.parse_function_table(name, start);
            }
            self.eat_op("*");
            name
        };
        let alias = self.parse_opt_alias()?;
        if self.is_kw("tablesample") {
            return Err(self.not_supported("TABLESAMPLE"));
        }
        Ok(TableRef::Table {
            name,
            alias,
            span: self.span_from(start),
        })
    }

    /// `'(' query ')' [alias]`, at the `(`.
    fn parse_paren_subquery(&mut self, start: u32) -> Result<TableRef> {
        self.advance();
        let query = Box::new(self.parse_query()?);
        self.expect(&TokenKind::RParen)?;
        let alias = self.parse_opt_alias()?;
        Ok(TableRef::Subquery {
            query,
            alias,
            span: self.span_from(start),
        })
    }

    /// `'(' joined_table ')'`, at the `(`.
    fn parse_paren_join(&mut self) -> Result<TableRef> {
        self.advance();
        let inner = self.parse_table_ref()?;
        if !matches!(inner, TableRef::Join { .. }) {
            return Err(self.unexpected());
        }
        self.expect(&TokenKind::RParen)?;
        if self.is_kw("as") || self.at_col_id() {
            return Err(self.not_supported("aliases for parenthesized joins"));
        }
        Ok(inner)
    }

    /// `func_table`: the arguments and alias after `name`.
    fn parse_function_table(&mut self, name: ObjectName, start: u32) -> Result<TableRef> {
        self.expect(&TokenKind::LParen)?;
        let args = if self.peek_kind() == &TokenKind::RParen {
            Vec::new()
        } else {
            self.parse_expr_list()?
        };
        self.expect(&TokenKind::RParen)?;
        if self.is_kw("with") && self.nth_is_kw(1, "ordinality") {
            return Err(self.not_supported("WITH ORDINALITY"));
        }
        let alias = self.parse_opt_alias()?;
        Ok(TableRef::Function {
            name,
            args,
            alias,
            span: self.span_from(start),
        })
    }

    /// `[AS] ColId ['(' name_list ')']`.
    pub(super) fn parse_opt_alias(&mut self) -> Result<Option<TableAlias>> {
        let start = self.start();
        let name = if self.eat_kw("as") || self.at_col_id() {
            self.parse_col_id()?
        } else {
            return Ok(None);
        };
        let columns = if self.peek_kind() == &TokenKind::LParen {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        Ok(Some(TableAlias {
            name,
            columns,
            span: self.span_from(start),
        }))
    }
}

fn table_ref_span(t: &TableRef) -> Span {
    match t {
        TableRef::Table { span, .. }
        | TableRef::Subquery { span, .. }
        | TableRef::Function { span, .. }
        | TableRef::Join { span, .. } => *span,
    }
}

fn multiple(what: &str, pos: u32) -> Error {
    Error::new(
        sqlstate::SYNTAX_ERROR,
        format!("multiple {what} clauses not allowed"),
    )
    .with_span(Span::new(pos, pos))
}

/// Keywords that start a data-modifying statement.
fn is_dml_kw(kw: Option<&str>) -> bool {
    matches!(kw, Some("insert" | "update" | "delete" | "merge"))
}
