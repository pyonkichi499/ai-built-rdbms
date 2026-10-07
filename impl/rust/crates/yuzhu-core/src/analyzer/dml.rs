//! INSERT (PostgreSQL's `transformInsertStmt`) and the stored DEFAULT /
//! CHECK expressions of a table.

use super::Analyzer;
use super::bound::OutputColumn;
use super::bound::{
    BoundCheck, BoundDelete, BoundDistinct, BoundExpr, BoundExprKind, BoundInsert, BoundQuery,
    BoundReturning, BoundSelect, BoundSetExpr, BoundUpdate, FromItem, UpdateSource,
};
use super::coerce::{resolve_unknown, tname};
use super::cte::CteScope;
use super::ddl::{GivenValue, IdentityInsert};
use super::expr::{ExprCtx, figure_colname};
use super::from::table_rel;
use super::scope::{FromBuilder, ParseExprKind, QueryEnv, ScopeStack};
use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{RteId, Var};
use crate::sql::ast::{
    Delete, Expr, Ident, Insert, InsertSource, QueryBody, SelectItem, TableAlias, TableRef, Update,
    Values,
};
use crate::types::{Datum, SqlType, oid};

/// Errors from re-parsed stored text carry offsets into that text, not
/// into the query: drop them.
fn strip_position(mut e: Error) -> Error {
    e.cursor_byte = None;
    e
}

/// 対象表（`rtable[0]`）だけを見せるスコープ（CHECK。システム列は参照できない）。
fn check_scope(table: &TableDef) -> Result<ScopeStack> {
    Ok(ScopeStack::single(vec![table_rel(
        table,
        None,
        RteId(0),
        false,
    )?]))
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

    /// [`Self::column_default`] に、IDENTITY 列の暗黙の `nextval(...)` を加えたもの。
    pub(super) fn column_default_in(
        &self,
        table: &TableDef,
        col: &ColumnDef,
    ) -> Result<Option<BoundExpr>> {
        if col.identity.is_some() {
            return super::ddl::identity_default_expr(table, col, self.catalog);
        }
        self.column_default(col)
    }

    /// Analyzes a DEFAULT expression for a column (no column references).
    pub(super) fn default_expr(&self, expr: &Expr, col: &ColumnDef) -> Result<BoundExpr> {
        let scopes = ScopeStack::empty();
        let b = self.transform_expr(expr, &ExprCtx::new(&scopes, ParseExprKind::ColumnDefault))?;
        self.coerce_assignment(b, &col.name, col.ty, "default expression")
    }

    /// The table's CHECK constraints, analyzed over the full table row
    /// (`Var { rte: 0 }`).
    pub(super) fn table_checks(&self, table: &TableDef) -> Result<Vec<BoundCheck>> {
        let scopes = check_scope(table)?;
        table
            .checks
            .iter()
            .map(|c| {
                let expr = crate::sql::parse_expr(&c.expr_sql).map_err(strip_position)?;
                let b = self
                    .transform_expr(&expr, &ExprCtx::new(&scopes, ParseExprKind::Check))
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
        self.analyze_insert_with(ins, RETURNING_ENABLED)
    }

    /// [`Self::analyze_insert`]。`returning_enabled` は RETURNING を受け付けるか（テストで切り替える）。
    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_insert_with(
        &self,
        ins: &Insert,
        returning_enabled: bool,
    ) -> Result<BoundInsert> {
        if !returning_enabled && let Some(span) = returning_span(&ins.returning) {
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        let table = self.resolve_relation(&ins.table, None)?;
        Self::check_writable(&table, ins.table.span)?;

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
            .map(|c| self.column_default_in(&table, c))
            .collect::<Result<Vec<_>>>()?;
        let checks = self.table_checks(&table)?;

        let mut coercions = None;
        let source = match &ins.source {
            InsertSource::DefaultValues => {
                targets.clear();
                BoundQuery {
                    ctes: vec![],
                    body: BoundSetExpr::Select(Box::new(BoundSelect {
                        rtable: vec![],
                        from: vec![],
                        filter: None,
                        group_by: vec![],
                        having: None,
                        has_agg: false,
                        targets: vec![],
                        n_visible: 0,
                        distinct: BoundDistinct::None,
                    })),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    columns: vec![],
                }
            }
            InsertSource::Query(q) => {
                let plain_values = q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none();
                match &q.body {
                    QueryBody::Values(v) if plain_values => self.insert_values(
                        v,
                        q,
                        &table,
                        &mut targets,
                        &ins.columns,
                        &defaults,
                        ins.overriding,
                    )?,
                    _ => {
                        let (sel, c) = self.insert_select(q, &table, &mut targets, &ins.columns)?;
                        coercions = c;
                        sel
                    }
                }
            }
        };

        let mut column_map: Vec<Option<usize>> = (0..table.columns.len())
            .map(|c| targets.iter().position(|t| *t == c))
            .collect();
        // INSERT ... SELECT: IDENTITY 列に値が来るときの規則（08 §5.8）。値を捨てるなら既定値（nextval）を使う。
        // VALUES は行ごとに `insert_values` が決める。
        let from_select = matches!(&ins.source, InsertSource::Query(q)
            if !(matches!(&q.body, QueryBody::Values(_))
                && q.order_by.is_empty()
                && q.limit.is_none()
                && q.offset.is_none()));
        if from_select {
            for &ci in &targets {
                let col = &table.columns[ci];
                if col.identity.is_some()
                    && super::ddl::identity_insert_rule(col, GivenValue::Value, ins.overriding)?
                        == IdentityInsert::Generate
                {
                    column_map[ci] = None;
                }
            }
        }
        let returning = if returning_enabled && !ins.returning.is_empty() {
            let rel = table_rel(
                &table,
                alias_of(ins.alias.as_ref()).as_ref(),
                RteId(0),
                true,
            )?;
            let scopes = ScopeStack::single(vec![rel]);
            let root = CteScope::root();
            Some(self.analyze_returning(&ins.returning, &scopes, &root)?)
        } else {
            None
        };
        Ok(BoundInsert {
            table,
            source: Box::new(source),
            coercions,
            column_map,
            defaults,
            checks,
            overriding: ins.overriding.map(|o| match o {
                crate::sql::ast::OverridingKind::System => super::bound::OverridingKind::System,
                crate::sql::ast::OverridingKind::User => super::bound::OverridingKind::User,
            }),
            returning,
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
    #[allow(clippy::too_many_arguments)]
    fn insert_values(
        &self,
        v: &Values,
        q: &crate::sql::ast::Query,
        table: &TableDef,
        targets: &mut Vec<usize>,
        cols: &[Ident],
        defaults: &[Option<BoundExpr>],
        overriding: Option<crate::sql::ast::OverridingKind>,
    ) -> Result<BoundQuery> {
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

        // 各行は rtable が空の 1 スコープ。
        let scopes = ScopeStack::empty();
        let cx = ExprCtx::new(&scopes, ParseExprKind::Values);
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
                    let v = self.coerce_assignment(b, &col.name, col.ty, "expression")?;
                    if col.identity.is_some()
                        && super::ddl::identity_insert_rule(col, GivenValue::Value, overriding)?
                            == IdentityInsert::Generate
                    {
                        defaults[ci]
                            .clone()
                            .ok_or_else(|| Error::internal("identity column without a default"))?
                    } else {
                        v
                    }
                };
                out.push(b);
            }
            rows.push(out);
        }
        let types: Vec<SqlType> = targets.iter().map(|&ci| table.columns[ci].ty).collect();
        self.values_query(rows, types, q)
    }

    /// `INSERT ... SELECT`: the query's visible columns are coerced to the
    /// target columns. A plain query is coerced in its own target list
    /// (PostgreSQL pulls such a subquery up). A query with ORDER BY /
    /// DISTINCT / LIMIT / OFFSET (or a VALUES body) is left uncoerced and
    /// the coercions are returned separately (`Var { rte: 0, col: i }` over
    /// the query's output), to be applied above the query on the rows it
    /// returns.
    fn insert_select(
        &self,
        q: &crate::sql::ast::Query,
        table: &TableDef,
        targets: &mut Vec<usize>,
        cols: &[Ident],
    ) -> Result<(BoundQuery, Option<Vec<BoundExpr>>)> {
        let root = CteScope::root();
        let mut bq = self.analyze_query(q, &QueryEnv::root(false, &root))?;
        let n = bq.columns.len();
        let extra = match &bq.body {
            BoundSetExpr::Select(s) => s.targets.get(targets.len()).map(|t| t.span),
            // 以前は VALUES の列が問い合わせの位置を持っていた。
            BoundSetExpr::Values { types, .. } => (types.len() > targets.len()).then_some(q.span),
            BoundSetExpr::SetOp { .. } => None,
        };
        Self::check_insert_arity(n, targets, cols, extra)?;
        let plain = bq.order_by.is_empty() && bq.limit.is_none() && bq.offset.is_none();
        if let BoundSetExpr::Select(sel) = &mut bq.body
            && plain
            && sel.distinct == BoundDistinct::None
        {
            for (i, &ci) in targets.iter().enumerate() {
                let col = &table.columns[ci];
                let coerced = self.coerce_assignment(
                    sel.targets[i].clone(),
                    &col.name,
                    col.ty,
                    "expression",
                )?;
                bq.columns[i].ty = coerced.ty;
                sel.targets[i] = coerced;
            }
            return Ok((bq, None));
        }
        let mut coercions = Vec::with_capacity(targets.len());
        for (i, &ci) in targets.iter().enumerate() {
            let col = &table.columns[ci];
            if let BoundSetExpr::Select(sel) = &mut bq.body
                && sel.targets[i].ty.oid == oid::UNKNOWN
            {
                if bq.order_by.iter().any(|k| k.target == i) {
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
                bq.columns[i].ty = sel.targets[i].ty;
            }
            let (ty, span) = match &bq.body {
                BoundSetExpr::Select(sel) => (sel.targets[i].ty, sel.targets[i].span),
                _ => (bq.columns[i].ty, q.span),
            };
            let var = Var::user(RteId(0), u16::try_from(i).unwrap_or(u16::MAX));
            let input = BoundExpr::new(BoundExprKind::Column(var), ty, span);
            coercions.push(self.coerce_assignment(input, &col.name, col.ty, "expression")?);
        }
        Ok((bq, Some(coercions)))
    }
}

/// RETURNING を受け付けるか（D3-17）。X3 と 04 が RETURNING に対応したら true にする。`false` の間は、
/// `returning` が空でなければ最初に `0A000 RETURNING is not supported yet`（位置は最初の項目）。
const RETURNING_ENABLED: bool = false;

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

/// 対象表（`rtable[0]`）と、FROM / USING 句を解析した結果。
struct DmlInput {
    rtable: Vec<super::bound::Rte>,
    from: Vec<FromItem>,
    /// WHERE・SET・RETURNING から見える名前空間（対象表が先頭）。
    scopes: ScopeStack,
}

impl Analyzer<'_> {
    /// DML on a system catalog is refused (`m2.md` §6.8.7, M2-Q12). シーケンスは書き換えられない
    /// （`42809`。08 §4.6）。
    fn check_writable(table: &TableDef, span: Span) -> Result<()> {
        if table.is_system_catalog() {
            return Err(Error::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {}", table.name),
            ));
        }
        if table.kind == crate::catalog::RelKind::Sequence {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot change sequence \"{}\"", table.name),
            )
            .with_span(span));
        }
        Ok(())
    }

    /// WHERE of UPDATE / DELETE (type bool).
    fn dml_filter(
        &self,
        selection: Option<&Expr>,
        scopes: &ScopeStack,
        ctes: &CteScope<'_>,
    ) -> Result<Option<BoundExpr>> {
        selection
            .map(|w| {
                let cx = ExprCtx::new(scopes, ParseExprKind::Where).with_ctes(ctes);
                let b = self.transform_expr(w, &cx)?;
                self.coerce_to_boolean(b, "WHERE")
            })
            .transpose()
    }

    /// 対象表を `rtable[0]` に積み（FROM / USING 句からは見えない）、FROM / USING 句を解析する。
    /// 解析が済んだら対象表を WHERE 以降に見せる（`m4/03` §4.4・§4.7）。
    fn dml_input(
        &self,
        table: &std::sync::Arc<TableDef>,
        alias: Option<&Ident>,
        span: Span,
        from: &[TableRef],
        ctes: &CteScope<'_>,
    ) -> Result<DmlInput> {
        let mut fb = FromBuilder::default();
        fb.push_table(table, alias_of(alias).as_ref(), true, span)?;
        fb.ns[0].lateral_only = true;
        fb.ns[0].lateral_ok = false;
        let env = QueryEnv::root(true, ctes);
        let from = self.analyze_from_clause(from, &mut fb, &env)?;
        fb.ns[0].lateral_only = false;
        let scopes = ScopeStack::single(fb.ns);
        Ok(DmlInput {
            rtable: fb.rtable,
            from,
            scopes,
        })
    }

    /// `RETURNING` の項目（`m4/03` §5.10.2）。対象表（`rte = 0`）の列だけを参照できる。
    pub(super) fn analyze_returning(
        &self,
        items: &[SelectItem],
        scopes: &ScopeStack,
        ctes: &CteScope<'_>,
    ) -> Result<BoundReturning> {
        let cx = ExprCtx::new(scopes, ParseExprKind::Returning).with_ctes(ctes);
        let mut targets = Vec::new();
        let mut columns = Vec::new();
        for item in items {
            let mut push = |e: BoundExpr, name: String, origin: (crate::types::Oid, i16)| {
                columns.push(OutputColumn {
                    name,
                    ty: e.ty,
                    table_oid: origin.0,
                    attnum: origin.1,
                });
                targets.push(e);
            };
            match item {
                SelectItem::Expr { expr, alias, .. } => {
                    let b = resolve_unknown(self.transform_expr(expr, &cx)?);
                    Self::check_returning_expr(&b)?;
                    let name = alias
                        .as_ref()
                        .map_or_else(|| figure_colname(expr), |a| a.value.clone());
                    let origin = scopes.origin_of_expr(&b);
                    push(b, name, origin);
                }
                SelectItem::Wildcard(span) => {
                    for c in scopes.expand_star(None, *span)? {
                        Self::check_returning_expr(&c.expr)?;
                        push(c.expr, c.name, c.origin);
                    }
                }
                SelectItem::QualifiedWildcard(q, span) => {
                    for c in scopes.expand_star(Some(q.parts.as_slice()), *span)? {
                        Self::check_returning_expr(&c.expr)?;
                        push(c.expr, c.name, c.origin);
                    }
                }
            }
        }
        Ok(BoundReturning { targets, columns })
    }

    /// RETURNING の式の制限: 集約は不可（42803）、副問い合わせと他の表の列は 0A000。
    fn check_returning_expr(e: &BoundExpr) -> Result<()> {
        if e.contains_aggregate() {
            return Err(Error::new(
                sqlstate::GROUPING_ERROR,
                "aggregate functions are not allowed in RETURNING",
            )
            .with_span(e.span));
        }
        if e.contains_sublink() {
            return Err(
                Error::not_supported("subquery in RETURNING is not supported yet")
                    .with_span(e.span),
            );
        }
        if e.columns().iter().any(|v| v.rte.0 != 0 || v.levels_up != 0) {
            return Err(Error::not_supported(
                "RETURNING referencing other tables is not supported yet",
            )
            .with_span(e.span));
        }
        Ok(())
    }

    pub(super) fn analyze_delete(&self, del: &Delete) -> Result<BoundDelete> {
        self.analyze_delete_with(del, RETURNING_ENABLED)
    }

    /// DELETE（対象表 → USING → WHERE → RETURNING の順。`m4/03` §4.7）。
    pub(super) fn analyze_delete_with(
        &self,
        del: &Delete,
        returning_enabled: bool,
    ) -> Result<BoundDelete> {
        if !returning_enabled && let Some(span) = returning_span(&del.returning) {
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        let table = self.resolve_relation(&del.table, None)?;
        Self::check_writable(&table, del.table.span)?;
        let root = CteScope::root();
        let input = self.dml_input(
            &table,
            del.alias.as_ref(),
            del.table.span,
            &del.using,
            &root,
        )?;
        let filter = self.dml_filter(del.selection.as_ref(), &input.scopes, &root)?;
        let returning = if del.returning.is_empty() {
            None
        } else {
            Some(self.analyze_returning(&del.returning, &input.scopes, &root)?)
        };
        Ok(BoundDelete {
            rtable: input.rtable,
            from: input.from,
            filter,
            returning,
        })
    }

    pub(super) fn analyze_update(&self, upd: &Update) -> Result<BoundUpdate> {
        self.analyze_update_with(upd, RETURNING_ENABLED)
    }

    /// UPDATE（対象表 → FROM → WHERE → RETURNING → SET の順。`m4/03` §4.7、R12）。
    pub(super) fn analyze_update_with(
        &self,
        upd: &Update,
        returning_enabled: bool,
    ) -> Result<BoundUpdate> {
        if !returning_enabled && let Some(span) = returning_span(&upd.returning) {
            return Err(Error::not_supported("RETURNING is not supported yet").with_span(span));
        }
        let table = self.resolve_relation(&upd.table, None)?;
        Self::check_writable(&table, upd.table.span)?;
        let refname = upd
            .alias
            .as_ref()
            .map_or(table.name.as_str(), |a| a.value.as_str());
        let root = CteScope::root();
        let input = self.dml_input(&table, upd.alias.as_ref(), upd.table.span, &upd.from, &root)?;
        let scopes = &input.scopes;
        let filter = self.dml_filter(upd.selection.as_ref(), scopes, &root)?;
        let returning = if upd.returning.is_empty() {
            None
        } else {
            Some(self.analyze_returning(&upd.returning, scopes, &root)?)
        };

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
                return Err(Error::new(
                    sqlstate::SYNTAX_ERROR,
                    format!("multiple assignments to same column \"{name}\""),
                ));
            }
            let col = &table.columns[idx];
            if col.identity.is_some() {
                super::ddl::identity_update_rule(col, matches!(a.value, Expr::Default { .. }))?;
            }
            let src = if let Expr::Default { .. } = a.value {
                UpdateSource::Default(self.column_default_in(&table, col)?)
            } else {
                let cx = ExprCtx::new(scopes, ParseExprKind::UpdateSet).with_ctes(&root);
                let b = self.transform_expr(&a.value, &cx)?;
                UpdateSource::Expr(self.coerce_assignment(b, &col.name, col.ty, "expression")?)
            };
            assignments.push((idx, src));
        }
        let checks = self.table_checks(&table)?;
        let not_null = table.columns.iter().map(|c| c.not_null).collect();
        Ok(BoundUpdate {
            rtable: input.rtable,
            from: input.from,
            filter,
            assignments,
            checks,
            not_null,
            returning,
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
