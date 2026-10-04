//! INSERT (PostgreSQL's `transformInsertStmt`) and the stored DEFAULT /
//! CHECK expressions of a table.

use super::Analyzer;
use super::bound::{
    BoundCheck, BoundDelete, BoundExpr, BoundExprKind, BoundFrom, BoundInsert, BoundSelect,
    BoundUpdate, UpdateSource,
};
use super::coerce::{resolve_unknown, tname};
use super::expr::ExprCtx;
use super::scope::{ExprKind, Scope};
use super::select::table_scope;
use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    Delete, Expr, Ident, Insert, InsertSource, QueryBody, SelectItem, TableAlias, Update, Values,
};
use crate::types::{Datum, SqlType, oid};

/// Errors from re-parsed stored text carry offsets into that text, not
/// into the query: drop them.
fn strip_position(mut e: Error) -> Error {
    e.cursor_byte = None;
    e
}

impl Analyzer<'_> {
    /// The column's DEFAULT expression (from its stored text), coerced to
    /// the column type with assignment semantics. `None` = no default.
    pub(super) fn column_default(&self, col: &ColumnDef) -> Result<Option<BoundExpr>> {
        let Some(src) = &col.default else {
            return Ok(None);
        };
        let expr = crate::sql::parse_expr(&src.expr_sql).map_err(strip_position)?;
        self.default_expr(&expr, col)
            .map(Some)
            .map_err(strip_position)
    }

    /// Analyzes a DEFAULT expression for a column (no column references).
    pub(super) fn default_expr(&self, expr: &Expr, col: &ColumnDef) -> Result<BoundExpr> {
        let scope = Scope::empty();
        let b = self.transform_expr(expr, &ExprCtx::new(&scope, ExprKind::ColumnDefault))?;
        self.coerce_assignment(b, &col.name, col.ty, "default expression")
    }

    /// The table's CHECK constraints, analyzed over the full table row.
    pub(super) fn table_checks(&self, table: &TableDef) -> Result<Vec<BoundCheck>> {
        let scope = table_scope(table, None)?;
        table
            .checks
            .iter()
            .map(|c| {
                let expr = crate::sql::parse_expr(&c.expr_sql).map_err(strip_position)?;
                let b = self
                    .transform_expr(&expr, &ExprCtx::new(&scope, ExprKind::Check))
                    .and_then(|b| self.coerce_to_boolean(b, "CHECK"))
                    .map_err(strip_position)?;
                Ok(BoundCheck {
                    name: c.name.clone(),
                    expr: b,
                })
            })
            .collect()
    }

    pub(super) fn analyze_insert(&self, ins: &Insert) -> Result<BoundInsert> {
        if let Some(r) = ins.returning.first() {
            let span = match r {
                crate::sql::ast::SelectItem::Expr { span, .. }
                | crate::sql::ast::SelectItem::Wildcard(span)
                | crate::sql::ast::SelectItem::QualifiedWildcard(_, span) => *span,
            };
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        let table = self.resolve_table(&ins.table)?;
        Self::check_writable(&table)?;

        // Target columns (indices into table.columns).
        let explicit = !ins.columns.is_empty();
        let mut targets: Vec<usize> = Vec::new();
        if explicit {
            for c in &ins.columns {
                let Some(idx) = table.column_index(&c.value) else {
                    return Err(Error::new(
                        sqlstate::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            c.value, table.name
                        ),
                    )
                    .with_span(c.span));
                };
                if targets.contains(&idx) {
                    return Err(Error::new(
                        sqlstate::DUPLICATE_COLUMN,
                        format!("column \"{}\" specified more than once", c.value),
                    )
                    .with_span(c.span));
                }
                targets.push(idx);
            }
        } else {
            targets = (0..table.columns.len()).collect();
        }

        let defaults = table
            .columns
            .iter()
            .map(|c| self.column_default(c))
            .collect::<Result<Vec<_>>>()?;
        let checks = self.table_checks(&table)?;

        let mut coercions = None;
        let source = match &ins.source {
            InsertSource::DefaultValues => {
                targets.clear();
                BoundSelect {
                    from: BoundFrom::None,
                    filter: None,
                    targets: vec![],
                    columns: vec![],
                    distinct: false,
                    order_by: vec![],
                    limit: None,
                    offset: None,
                }
            }
            InsertSource::Query(q) => {
                let plain_values = q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none();
                match &q.body {
                    QueryBody::Values(v) if plain_values => {
                        self.insert_values(v, q, &table, &mut targets, &ins.columns, &defaults)?
                    }
                    _ => {
                        let (sel, c) = self.insert_select(q, &table, &mut targets, &ins.columns)?;
                        coercions = c;
                        sel
                    }
                }
            }
        };

        let column_map = (0..table.columns.len())
            .map(|c| targets.iter().position(|t| *t == c))
            .collect();
        Ok(BoundInsert {
            table,
            source: Box::new(source),
            coercions,
            column_map,
            defaults,
            checks,
        })
    }

    /// Checks the number of source expressions against the target columns
    /// and truncates the implicit target list to it.
    fn check_insert_arity(
        n: usize,
        targets: &mut Vec<usize>,
        cols: &[Ident],
        first_extra_expr: Option<Span>,
    ) -> Result<()> {
        if n > targets.len() {
            let mut e = Error::syntax_at(
                first_extra_expr.unwrap_or_default(),
                "INSERT has more expressions than target columns",
            );
            if first_extra_expr.is_none() {
                e.cursor_byte = None;
            }
            return Err(e);
        }
        if !cols.is_empty() && n < targets.len() {
            return Err(Error::syntax_at(
                cols[n].span,
                "INSERT has more target columns than expressions",
            ));
        }
        targets.truncate(n);
        Ok(())
    }

    /// `INSERT ... VALUES`: each value is coerced to its target column on
    /// its own (no common type across rows); `DEFAULT` becomes the column
    /// default (or a typed NULL).
    fn insert_values(
        &self,
        v: &Values,
        q: &crate::sql::ast::Query,
        table: &TableDef,
        targets: &mut Vec<usize>,
        cols: &[Ident],
        defaults: &[Option<BoundExpr>],
    ) -> Result<BoundSelect> {
        let width = v.rows.first().map_or(0, Vec::len);
        for row in &v.rows {
            if row.len() != width {
                let span = row.first().map_or(v.span, Expr::span);
                return Err(Error::syntax_at(
                    span,
                    "VALUES lists must all be the same length",
                ));
            }
        }
        let extra = v
            .rows
            .first()
            .and_then(|r| r.get(targets.len()))
            .map(Expr::span);
        Self::check_insert_arity(width, targets, cols, extra)?;

        let scope = Scope::empty();
        let cx = ExprCtx::new(&scope, ExprKind::Values);
        let mut rows = Vec::with_capacity(v.rows.len());
        for row in &v.rows {
            let mut out = Vec::with_capacity(width);
            for (e, &ci) in row.iter().zip(targets.iter()) {
                let col = &table.columns[ci];
                let b = if let Expr::Default { span } = e {
                    match &defaults[ci] {
                        Some(d) => d.clone(),
                        None => BoundExpr::new(BoundExprKind::Literal(Datum::Null), col.ty, *span),
                    }
                } else {
                    let b = self.transform_expr(e, &cx)?;
                    self.coerce_assignment(b, &col.name, col.ty, "expression")?
                };
                out.push(b);
            }
            rows.push(out);
        }
        let types: Vec<SqlType> = targets.iter().map(|&ci| table.columns[ci].ty).collect();
        self.values_select(rows, types, q)
    }

    /// `INSERT ... SELECT`: the query's visible columns are coerced to the
    /// target columns. A plain query is coerced in its own target list
    /// (PostgreSQL pulls such a subquery up). A query with ORDER BY /
    /// DISTINCT / LIMIT / OFFSET is left uncoerced and the coercions are
    /// returned separately, to be applied above the query on the rows it
    /// returns.
    fn insert_select(
        &self,
        q: &crate::sql::ast::Query,
        table: &TableDef,
        targets: &mut Vec<usize>,
        cols: &[Ident],
    ) -> Result<(BoundSelect, Option<Vec<BoundExpr>>)> {
        let mut sel = self.analyze_query(q, false)?;
        let n = sel.columns.len();
        let extra = sel.targets.get(targets.len()).map(|t| t.span);
        Self::check_insert_arity(n, targets, cols, extra)?;
        let simple =
            !sel.distinct && sel.order_by.is_empty() && sel.limit.is_none() && sel.offset.is_none();
        if simple {
            for (i, &ci) in targets.iter().enumerate() {
                let col = &table.columns[ci];
                let coerced = self.coerce_assignment(
                    sel.targets[i].clone(),
                    &col.name,
                    col.ty,
                    "expression",
                )?;
                sel.columns[i].ty = coerced.ty;
                sel.targets[i] = coerced;
            }
            return Ok((sel, None));
        }
        let mut coercions = Vec::with_capacity(targets.len());
        for (i, &ci) in targets.iter().enumerate() {
            let col = &table.columns[ci];
            if sel.targets[i].ty.oid == oid::UNKNOWN {
                if sel.order_by.iter().any(|k| k.target == i) {
                    // A sort key resolves an unknown output column to text
                    // (`addTargetToSortList`).
                    sel.targets[i] = resolve_unknown(sel.targets[i].clone());
                } else {
                    // An unknown literal (or NULL) is a constant: convert it
                    // in place, so invalid input is reported at analysis
                    // time as in PostgreSQL. Constant in every row, so
                    // converting it before DISTINCT changes nothing.
                    sel.targets[i] = self.coerce_assignment(
                        sel.targets[i].clone(),
                        &col.name,
                        col.ty,
                        "expression",
                    )?;
                }
                sel.columns[i].ty = sel.targets[i].ty;
            }
            let src = &sel.targets[i];
            let input = BoundExpr::new(BoundExprKind::ColumnRef { index: i }, src.ty, src.span);
            coercions.push(self.coerce_assignment(input, &col.name, col.ty, "expression")?);
        }
        Ok((sel, Some(coercions)))
    }
}

fn returning_span(items: &[SelectItem]) -> Option<Span> {
    items.first().map(|r| match r {
        SelectItem::Expr { span, .. }
        | SelectItem::Wildcard(span)
        | SelectItem::QualifiedWildcard(_, span) => *span,
    })
}

fn alias_of(alias: Option<&Ident>) -> Option<TableAlias> {
    alias.map(|a| TableAlias {
        name: a.clone(),
        columns: Vec::new(),
        span: a.span,
    })
}

impl Analyzer<'_> {
    /// DML on a system catalog is refused (`m2.md` §6.8.7, M2-Q12).
    fn check_writable(table: &TableDef) -> Result<()> {
        if table.is_system_catalog() {
            return Err(Error::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {}", table.name),
            ));
        }
        Ok(())
    }

    /// WHERE of UPDATE / DELETE (type bool).
    fn dml_filter(&self, selection: Option<&Expr>, scope: &Scope) -> Result<Option<BoundExpr>> {
        selection
            .map(|w| {
                let b = self.transform_expr(w, &ExprCtx::new(scope, ExprKind::Where))?;
                self.coerce_to_boolean(b, "WHERE")
            })
            .transpose()
    }

    pub(super) fn analyze_delete(&self, del: &Delete) -> Result<BoundDelete> {
        if let Some(span) = returning_span(&del.returning) {
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        if let Some(u) = del.using.first() {
            return Err(
                Error::not_supported("DELETE ... USING is not supported yet")
                    .with_span(table_ref_span(u)),
            );
        }
        let table = self.resolve_table(&del.table)?;
        Self::check_writable(&table)?;
        let mut scope = table_scope(&table, alias_of(del.alias.as_ref()).as_ref())?;
        scope.enable_system_columns();
        let filter = self.dml_filter(del.selection.as_ref(), &scope)?;
        Ok(BoundDelete {
            table,
            filter,
            system_columns: scope.used_system_columns(),
        })
    }

    pub(super) fn analyze_update(&self, upd: &Update) -> Result<BoundUpdate> {
        if let Some(span) = returning_span(&upd.returning) {
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        if let Some(f) = upd.from.first() {
            return Err(Error::not_supported("UPDATE ... FROM is not supported yet")
                .with_span(table_ref_span(f)));
        }
        let table = self.resolve_table(&upd.table)?;
        Self::check_writable(&table)?;
        let refname = upd
            .alias
            .as_ref()
            .map_or(table.name.as_str(), |a| a.value.as_str());
        let mut scope = table_scope(&table, alias_of(upd.alias.as_ref()).as_ref())?;
        scope.enable_system_columns();
        let filter = self.dml_filter(upd.selection.as_ref(), &scope)?;

        let mut assignments: Vec<(usize, UpdateSource)> = Vec::new();
        for a in &upd.assignments {
            let name = &a.column.value;
            let Some(idx) = table.column_index(name) else {
                return Err(Self::bad_set_target(&table, refname, a));
            };
            if let Some(f) = a.fields.first() {
                return Err(Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "cannot assign to field \"{}\" of column \"{name}\" because its type {} is not a composite type",
                        f.value,
                        tname(table.columns[idx].ty.oid)
                    ),
                )
                .with_span(a.column.span));
            }
            if assignments.iter().any(|(i, _)| *i == idx) {
                return Err(Error::syntax_at(
                    a.column.span,
                    format!("multiple assignments to same column \"{name}\""),
                ));
            }
            let col = &table.columns[idx];
            let src = if let Expr::Default { .. } = a.value {
                UpdateSource::Default(self.column_default(col)?)
            } else {
                let b =
                    self.transform_expr(&a.value, &ExprCtx::new(&scope, ExprKind::UpdateSet))?;
                UpdateSource::Expr(self.coerce_assignment(b, &col.name, col.ty, "expression")?)
            };
            assignments.push((idx, src));
        }
        let checks = self.table_checks(&table)?;
        let not_null = table.columns.iter().map(|c| c.not_null).collect();
        Ok(BoundUpdate {
            table,
            assignments,
            filter,
            system_columns: scope.used_system_columns(),
            checks,
            not_null,
        })
    }

    /// Error for a SET target that is not a user column.
    fn bad_set_target(table: &TableDef, refname: &str, a: &crate::sql::ast::Assignment) -> Error {
        let name = &a.column.value;
        if a.fields.is_empty()
            && crate::catalog::schema::SYSTEM_COLUMNS
                .iter()
                .any(|(n, _, _)| n == name)
        {
            return Error::not_supported(format!("cannot assign to system column \"{name}\""))
                .with_span(a.column.span);
        }
        let e = Error::new(
            sqlstate::UNDEFINED_COLUMN,
            format!(
                "column \"{name}\" of relation \"{}\" does not exist",
                table.name
            ),
        )
        .with_span(a.column.span);
        if !a.fields.is_empty() && name == refname {
            e.with_hint("SET target columns cannot be qualified with the relation name.")
        } else {
            e
        }
    }
}

fn table_ref_span(t: &crate::sql::ast::TableRef) -> Span {
    use crate::sql::ast::TableRef;
    match t {
        TableRef::Table { span, .. }
        | TableRef::Subquery { span, .. }
        | TableRef::Join { span, .. } => *span,
    }
}
