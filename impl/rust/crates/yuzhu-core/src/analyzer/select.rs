//! SELECT and VALUES (PostgreSQL's `transformSelectStmt`,
//! `transformValuesClause`, `transformSortClause`, `transformLimitClause`).

use std::sync::Arc;

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind, BoundFrom, BoundSelect, BoundSortKey, OutputColumn};
use super::coerce::resolve_unknown;
use super::expr::{ExprCtx, contains_column_ref, figure_colname, parse_int_literal, same_expr};
use super::scope::{ExprKind, Scope, ScopeColumn, ScopeRel};
use crate::catalog::TableDef;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    Distinct, Expr, Literal, NullsOrder, ObjectName, OrderByItem, Query, QueryBody, Select,
    SelectItem, SortDirection, TableAlias, TableRef, Values,
};
use crate::types::SqlType;

/// `a.b.c` as written, for messages.
pub(super) fn display_name(name: &ObjectName) -> String {
    name.parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Builds the scope of a base table (with optional alias / column aliases).
pub(super) fn table_scope(table: &TableDef, alias: Option<&TableAlias>) -> Result<Scope> {
    let mut columns: Vec<ScopeColumn> = table
        .columns
        .iter()
        .map(|c| ScopeColumn {
            name: c.name.clone(),
            ty: c.ty,
            attnum: c.attnum,
        })
        .collect();
    if let Some(a) = alias {
        if a.columns.len() > columns.len() {
            return Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!(
                    "table \"{}\" has {} columns available but {} columns specified",
                    a.name.value,
                    columns.len(),
                    a.columns.len()
                ),
            )
            .with_span(a.span));
        }
        for (c, n) in columns.iter_mut().zip(&a.columns) {
            c.name.clone_from(&n.value);
        }
    }
    Ok(Scope::with_rel(ScopeRel {
        refname: alias.map_or_else(|| table.name.clone(), |a| a.name.value.clone()),
        schema: if alias.is_none() {
            Some(table.schema.clone())
        } else {
            None
        },
        table_oid: table.oid,
        columns,
    }))
}

fn not_supported(what: &str, span: Span) -> Error {
    Error::not_supported(format!("{what} is not supported yet")).with_span(span)
}

impl Analyzer<'_> {
    /// Looks up a table by possibly qualified name (42P01 if missing).
    pub(super) fn resolve_table(&self, name: &ObjectName) -> Result<Arc<TableDef>> {
        if name.parts.len() > 3 {
            return Err(Error::syntax_at(
                name.span,
                format!(
                    "improper relation name (too many dotted names): {}",
                    display_name(name)
                ),
            ));
        }
        if name.parts.len() == 3 && name.parts[0].value != self.catalog.current_database() {
            return Err(Error::not_supported(format!(
                "cross-database references are not implemented: {}",
                display_name(name)
            ))
            .with_span(name.span));
        }
        let schema = name.schema().map(|s| s.value.as_str());
        self.catalog
            .table(schema, &name.name().value)
            .ok_or_else(|| {
                Error::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", display_name(name)),
                )
                .with_span(name.span)
            })
    }

    /// Analyzes a query. `resolve_unknowns`: turn unknown-typed output
    /// columns into text (false for `INSERT ... SELECT`, where the target
    /// columns decide).
    pub(super) fn analyze_query(&self, q: &Query, resolve_unknowns: bool) -> Result<BoundSelect> {
        match &q.body {
            QueryBody::Select(s) => self.analyze_select(s, q, resolve_unknowns),
            QueryBody::Values(v) => self.analyze_values(v, q),
            QueryBody::SetOp { span, .. } => Err(not_supported("UNION/INTERSECT/EXCEPT", *span)),
            QueryBody::Nested(inner) => {
                let outer_empty = q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none();
                let inner_empty =
                    inner.order_by.is_empty() && inner.limit.is_none() && inner.offset.is_none();
                if outer_empty {
                    self.analyze_query(inner, resolve_unknowns)
                } else if inner_empty {
                    let merged = Query {
                        body: inner.body.clone(),
                        order_by: q.order_by.clone(),
                        limit: q.limit.clone(),
                        offset: q.offset.clone(),
                        span: q.span,
                    };
                    self.analyze_query(&merged, resolve_unknowns)
                } else {
                    Err(not_supported("nested ORDER BY / LIMIT", q.span))
                }
            }
        }
    }

    fn analyze_select(&self, s: &Select, q: &Query, resolve_unknowns: bool) -> Result<BoundSelect> {
        if let Some(Distinct::On(_)) = &s.distinct {
            return Err(not_supported("SELECT DISTINCT ON", s.span));
        }
        if let Some(g) = s.group_by.first() {
            return Err(not_supported("GROUP BY", g.span()));
        }
        if let Some(h) = &s.having {
            return Err(not_supported("HAVING", h.span()));
        }
        // FROM
        let (from, scope) = match s.from.as_slice() {
            [] => (BoundFrom::None, Scope::empty()),
            [TableRef::Table { name, alias, .. }] => {
                let table = self.resolve_table(name)?;
                let scope = table_scope(&table, alias.as_ref())?;
                (
                    BoundFrom::Table {
                        table,
                        alias: alias.as_ref().map(|a| a.name.value.clone()),
                    },
                    scope,
                )
            }
            [TableRef::Join { span, .. }, ..] => return Err(not_supported("JOIN", *span)),
            [TableRef::Subquery { span, .. }, ..] => {
                return Err(not_supported("subquery in FROM", *span));
            }
            [_, second, ..] => {
                let span = match second {
                    TableRef::Table { span, .. }
                    | TableRef::Subquery { span, .. }
                    | TableRef::Join { span, .. } => *span,
                };
                return Err(not_supported("JOIN (more than one table in FROM)", span));
            }
        };

        // Target list.
        let tcx = ExprCtx::new(&scope, ExprKind::SelectTarget);
        let mut targets = Vec::new();
        let mut columns = Vec::new();
        for item in &s.targets {
            match item {
                SelectItem::Expr { expr, alias, .. } => {
                    let mut b = self.transform_expr(expr, &tcx)?;
                    if resolve_unknowns {
                        b = resolve_unknown(b);
                    }
                    let name = alias
                        .as_ref()
                        .map_or_else(|| figure_colname(expr), |a| a.value.clone());
                    columns.push(Self::output_column(name, &b, &scope));
                    targets.push(b);
                }
                SelectItem::Wildcard(span) => {
                    Self::push_star(&scope, None, *span, &mut targets, &mut columns)?;
                }
                SelectItem::QualifiedWildcard(q, span) => {
                    Self::push_star(&scope, Some(q), *span, &mut targets, &mut columns)?;
                }
            }
        }

        // WHERE
        let filter = match &s.selection {
            Some(w) => {
                let b = self.transform_expr(w, &ExprCtx::new(&scope, ExprKind::Where))?;
                Some(self.coerce_to_boolean(b, "WHERE")?)
            }
            None => None,
        };

        let distinct = matches!(s.distinct, Some(Distinct::All));
        let order_by =
            self.transform_sort_clause(&q.order_by, &mut targets, &columns, &scope, distinct)?;
        let limit = self.transform_limit(q.limit.as_ref(), &scope, ExprKind::Limit)?;
        let offset = self.transform_limit(q.offset.as_ref(), &scope, ExprKind::Offset)?;
        Ok(BoundSelect {
            from,
            filter,
            targets,
            columns,
            distinct,
            order_by,
            limit,
            offset,
        })
    }

    fn push_star(
        scope: &Scope,
        qual: Option<&ObjectName>,
        span: Span,
        targets: &mut Vec<BoundExpr>,
        columns: &mut Vec<OutputColumn>,
    ) -> Result<()> {
        for (index, col) in scope.expand_star(qual.map(|q| q.parts.as_slice()), span)? {
            let b = BoundExpr::new(BoundExprKind::ColumnRef { index }, col.ty, span);
            columns.push(Self::output_column(col.name.clone(), &b, scope));
            targets.push(b);
        }
        Ok(())
    }

    fn output_column(name: String, b: &BoundExpr, scope: &Scope) -> OutputColumn {
        let (table_oid, attnum) = match (&b.kind, &scope.rel) {
            (BoundExprKind::ColumnRef { index }, Some(rel)) if rel.table_oid != 0 => (
                rel.table_oid,
                rel.columns.get(*index).map_or(0, |c| c.attnum),
            ),
            _ => (0, 0),
        };
        OutputColumn {
            name,
            ty: b.ty,
            table_oid,
            attnum,
        }
    }

    /// Analyzes the rows of a VALUES list (no coercion yet). Errors on
    /// rows of different lengths.
    pub(super) fn transform_values_rows(&self, v: &Values) -> Result<Vec<Vec<BoundExpr>>> {
        let scope = Scope::empty();
        let cx = ExprCtx::new(&scope, ExprKind::Values);
        let width = v.rows.first().map_or(0, Vec::len);
        let mut rows = Vec::with_capacity(v.rows.len());
        for row in &v.rows {
            if row.len() != width {
                let span = row.first().map_or(v.span, Expr::span);
                return Err(Error::syntax_at(
                    span,
                    "VALUES lists must all be the same length",
                ));
            }
            rows.push(
                row.iter()
                    .map(|e| self.transform_expr(e, &cx))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(rows)
    }

    fn analyze_values(&self, v: &Values, q: &Query) -> Result<BoundSelect> {
        let rows = self.transform_values_rows(v)?;
        let width = rows.first().map_or(0, Vec::len);
        // Resolve each column to its common type.
        let mut cols: Vec<Vec<BoundExpr>> =
            (0..width).map(|_| Vec::with_capacity(rows.len())).collect();
        for row in rows {
            for (i, e) in row.into_iter().enumerate() {
                cols[i].push(e);
            }
        }
        let mut types = Vec::with_capacity(width);
        let mut coerced_cols = Vec::with_capacity(width);
        for col in cols {
            let (c, ty) = self.coerce_all_to_common(col, "VALUES")?;
            types.push(ty);
            coerced_cols.push(c);
        }
        let nrows = v.rows.len();
        let mut rows: Vec<Vec<BoundExpr>> = (0..nrows).map(|_| Vec::with_capacity(width)).collect();
        for col in coerced_cols {
            for (r, e) in col.into_iter().enumerate() {
                rows[r].push(e);
            }
        }
        self.values_select(rows, types, q)
    }

    /// A SELECT over VALUES rows already coerced to `types`, with the
    /// query's ORDER BY / LIMIT / OFFSET.
    pub(super) fn values_select(
        &self,
        rows: Vec<Vec<BoundExpr>>,
        types: Vec<SqlType>,
        q: &Query,
    ) -> Result<BoundSelect> {
        let names: Vec<String> = (1..=types.len()).map(|i| format!("column{i}")).collect();
        let scope = Scope::with_rel(ScopeRel {
            refname: "*VALUES*".to_owned(),
            schema: None,
            table_oid: 0,
            columns: names
                .iter()
                .zip(&types)
                .map(|(n, t)| ScopeColumn {
                    name: n.clone(),
                    ty: *t,
                    attnum: 0,
                })
                .collect(),
        });
        let mut targets: Vec<BoundExpr> = types
            .iter()
            .enumerate()
            .map(|(index, t)| BoundExpr::new(BoundExprKind::ColumnRef { index }, *t, q.span))
            .collect();
        let columns: Vec<OutputColumn> = names
            .into_iter()
            .zip(&types)
            .map(|(name, t)| OutputColumn {
                name,
                ty: *t,
                table_oid: 0,
                attnum: 0,
            })
            .collect();
        let order_by =
            self.transform_sort_clause(&q.order_by, &mut targets, &columns, &scope, false)?;
        let limit = self.transform_limit(q.limit.as_ref(), &scope, ExprKind::Limit)?;
        let offset = self.transform_limit(q.offset.as_ref(), &scope, ExprKind::Offset)?;
        Ok(BoundSelect {
            from: BoundFrom::Values { rows, types },
            filter: None,
            targets,
            columns,
            distinct: false,
            order_by,
            limit,
            offset,
        })
    }

    /// ORDER BY (`transformSortClause` with PostgreSQL's SQL92-then-SQL99
    /// target lookup).
    fn transform_sort_clause(
        &self,
        items: &[OrderByItem],
        targets: &mut Vec<BoundExpr>,
        columns: &[OutputColumn],
        scope: &Scope,
        distinct: bool,
    ) -> Result<Vec<BoundSortKey>> {
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let target = self.find_target_entry(&item.expr, targets, columns, scope)?;
            if distinct && target >= columns.len() {
                return Err(Error::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                )
                .with_span(item.expr.span()));
            }
            let descending = item.direction == Some(SortDirection::Desc);
            let nulls_first = match item.nulls {
                Some(NullsOrder::First) => true,
                Some(NullsOrder::Last) => false,
                None => descending,
            };
            keys.push(BoundSortKey {
                target,
                descending,
                nulls_first,
            });
        }
        Ok(keys)
    }

    /// `findTargetlistEntrySQL92`: a bare name matches an output column
    /// name first; an integer constant is an output column position;
    /// anything else is an expression over the input (SQL99), matched
    /// against existing targets or added as a resjunk target.
    fn find_target_entry(
        &self,
        e: &Expr,
        targets: &mut Vec<BoundExpr>,
        columns: &[OutputColumn],
        scope: &Scope,
    ) -> Result<usize> {
        if let Expr::Column { parts, span } = e
            && let [name] = parts.as_slice()
        {
            let mut found: Option<usize> = None;
            for (i, c) in columns.iter().enumerate() {
                if c.name != name.value {
                    continue;
                }
                match found {
                    Some(j) if !same_expr(&targets[j], &targets[i]) => {
                        return Err(Error::new(
                            sqlstate::AMBIGUOUS_COLUMN,
                            format!("ORDER BY \"{}\" is ambiguous", name.value),
                        )
                        .with_span(*span));
                    }
                    Some(_) => {}
                    None => found = Some(i),
                }
            }
            if let Some(i) = found {
                return Ok(i);
            }
        }
        if let Expr::Literal { value, span } = e
            && !matches!(value, Literal::Bool(_))
        {
            let pos = match value {
                Literal::Integer(s) => parse_int_literal(s),
                _ => None,
            };
            let Some(pos) = pos else {
                return Err(Error::syntax_at(*span, "non-integer constant in ORDER BY"));
            };
            if pos < 1 || usize::try_from(pos).map_or(true, |p| p > columns.len()) {
                return Err(Error::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    format!("ORDER BY position {pos} is not in select list"),
                )
                .with_span(*span));
            }
            return Ok(usize::try_from(pos - 1).unwrap_or(0));
        }
        let b = resolve_unknown(self.transform_expr(e, &ExprCtx::new(scope, ExprKind::OrderBy))?);
        if let Some(i) = targets.iter().position(|t| same_expr(t, &b)) {
            return Ok(i);
        }
        targets.push(b);
        Ok(targets.len() - 1)
    }

    /// LIMIT / OFFSET: coerced to int8, no column references.
    fn transform_limit(
        &self,
        e: Option<&Expr>,
        scope: &Scope,
        kind: ExprKind,
    ) -> Result<Option<BoundExpr>> {
        let Some(e) = e else {
            return Ok(None);
        };
        let b = self.transform_expr(e, &ExprCtx::new(scope, kind))?;
        if contains_column_ref(&b) {
            return Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!("argument of {} must not contain variables", kind.name()),
            )
            .with_span(b.span));
        }
        self.coerce_to_specific_type(b, SqlType::INT8, kind.name())
            .map(Some)
    }
}
