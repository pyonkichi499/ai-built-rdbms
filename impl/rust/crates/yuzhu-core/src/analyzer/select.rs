//! SELECT and VALUES (PostgreSQL's `transformSelectStmt`,
//! `transformValuesClause`, `transformSortClause`, `transformLimitClause`).
//!
//! `analyze_query` の骨格（WITH・集合演算・Values・Nested の振り分け）は P0-d が置く（11 §7.1 の C-20）。
//! N3 は `setop.rs` / `cte.rs` / `sublink.rs` の中だけを書き、`select.rs` は N2 だけが編集する。

use std::sync::Arc;

use super::Analyzer;
use super::agg::find_aggregate;
use super::bound::{
    BoundDistinct, BoundExpr, BoundExprKind, BoundQuery, BoundSelect, BoundSetExpr, BoundSortKey,
    OutputColumn, Rte, RteColumn, RteKind,
};
use super::coerce::resolve_unknown;
use super::cte::CteScope;
use super::expr::{ExprCtx, figure_colname, parse_int_literal};
use super::scope::{FromBuilder, ParseExprKind, QueryEnv, ScopeColumn, ScopeRel, ScopeStack};
use crate::catalog::TableDef;
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{RteId, Var};
use crate::sql::ast::{
    Distinct, Expr, Literal, NullsOrder, ObjectName, OrderByItem, Query, QueryBody, Select,
    SelectItem, SortDirection, Values,
};
use crate::types::{Oid, SqlType};

/// `a.b.c` as written, for messages.
pub(super) fn display_name(name: &ObjectName) -> String {
    name.parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// ORDER BY / GROUP BY / DISTINCT ON が共有する出力列のリスト（`m4/03` §3.3）。
#[derive(Debug, Default)]
pub(super) struct TargetList {
    /// 先頭 `n_visible` 個が可視、以降は resjunk。
    pub(super) exprs: Vec<BoundExpr>,
    /// 出力名（resjunk は空）。
    pub(super) names: Vec<String>,
    /// 出力列の由来 `(table_oid, attnum)`（resjunk は `(0, 0)`）。
    pub(super) origins: Vec<(Oid, i16)>,
    pub(super) n_visible: usize,
}

impl TargetList {
    fn push(&mut self, e: BoundExpr, name: String, origin: (Oid, i16)) {
        self.exprs.push(e);
        self.names.push(name);
        self.origins.push(origin);
    }

    fn push_junk(&mut self, e: BoundExpr) -> usize {
        self.push(e, String::new(), (0, 0));
        self.exprs.len() - 1
    }

    /// 外に見える出力列。
    fn columns(&self) -> Vec<OutputColumn> {
        (0..self.n_visible)
            .map(|i| OutputColumn {
                name: self.names[i].clone(),
                ty: self.exprs[i].ty,
                table_oid: self.origins[i].0,
                attnum: self.origins[i].1,
            })
            .collect()
    }
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
            .table(schema, &name.name().value)?
            .ok_or_else(|| {
                Error::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", display_name(name)),
                )
                .with_span(name.span)
            })
    }

    /// WITH + 本体 + ORDER BY / LIMIT / OFFSET → `BoundQuery`（`m4/03` §4.2）。`env.resolve_unknowns`:
    /// unknown 型の出力列を text にする（`INSERT ... SELECT` は false で、対象列が決める）。
    pub(super) fn analyze_query(&self, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        crate::sql::stack::check_stack_depth()?;
        // 括弧つきの問い合わせ: 外枠と内側のどちらかが空なら、1 つにまとめて解析する。
        if let QueryBody::Nested(inner) = &q.body {
            return self.analyze_nested(q, inner, env);
        }
        let (ctes, scope) = match &q.with {
            Some(w) => {
                let r = self.analyze_with(w, env)?;
                (r.ctes, r.scope)
            }
            None => (Vec::new(), CteScope::level(env.ctes)),
        };
        let env2 = QueryEnv {
            outer: env.outer,
            ctes: &scope,
            resolve_unknowns: env.resolve_unknowns,
        };
        let mut bq = match &q.body {
            QueryBody::Select(s) => self.analyze_select(s, q, &env2)?,
            QueryBody::Values(v) => self.analyze_values_query(v, q, &env2)?,
            QueryBody::SetOp { .. } => self.analyze_set_operation(&q.body, q, &env2)?,
            QueryBody::Nested(_) => {
                return Err(Error::internal("nested query reached the dispatcher"));
            }
        };
        bq.ctes = ctes;
        Ok(bq)
    }

    fn analyze_nested(&self, q: &Query, inner: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        let outer_empty = q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none();
        let inner_empty =
            inner.order_by.is_empty() && inner.limit.is_none() && inner.offset.is_none();
        if outer_empty && q.with.is_none() {
            self.analyze_query(inner, env)
        } else if inner_empty {
            // 外枠の WITH / ORDER BY / LIMIT / OFFSET を内側の本体に付ける。
            if q.with.is_some() && inner.with.is_some() {
                return Err(super::not_supported(
                    "WITH on a parenthesized query that has its own WITH",
                    q.span,
                ));
            }
            let merged = Query {
                with: q.with.clone().or_else(|| inner.with.clone()),
                body: inner.body.clone(),
                order_by: q.order_by.clone(),
                limit: q.limit.clone(),
                offset: q.offset.clone(),
                span: q.span,
            };
            self.analyze_query(&merged, env)
        } else {
            Err(super::not_supported("nested ORDER BY / LIMIT", q.span))
        }
    }

    fn analyze_select(&self, s: &Select, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        // 手順は PostgreSQL の `transformSelectStmt` と同じ（`m4/03` §4.3）。エラーの出る順序を守る。
        // 1. FROM
        let mut fb = FromBuilder::default();
        let from = self.analyze_from_clause(&s.from, &mut fb, env)?;
        let scopes = match env.outer {
            Some(outer) => outer.with_frame(fb.ns.clone()),
            None => ScopeStack::single(fb.ns.clone()),
        };
        let cx = |kind| {
            let c = ExprCtx::new(&scopes, kind);
            c.with_ctes(env.ctes)
        };

        // 2. SELECT 句
        let mut tl =
            self.transform_target_list(&s.targets, &cx(ParseExprKind::SelectTarget), env)?;

        // 3. WHERE
        let filter = match &s.selection {
            Some(w) => {
                let b = self.transform_expr(w, &cx(ParseExprKind::Where))?;
                Some(self.coerce_to_boolean(b, "WHERE")?)
            }
            None => None,
        };

        // 4. HAVING（GROUP BY の別名・位置番号は見ない。検査は 9 でまとめて行う）
        let having = match &s.having {
            Some(h) => {
                let b = self.transform_expr(h, &cx(ParseExprKind::Having))?;
                Some(self.coerce_to_boolean(b, "HAVING")?)
            }
            None => None,
        };

        // 5. ORDER BY（出力名が先。resjunk の式を `tl` に足す）
        let (order_by, order_spans) =
            self.transform_sort_clause(&q.order_by, &mut tl, &cx(ParseExprKind::OrderBy))?;

        // 6. GROUP BY（入力列が先。D3-3）
        let group_by = self.resolve_group_by(&s.group_by, &mut tl, &cx(ParseExprKind::GroupBy))?;

        // 7. DISTINCT / DISTINCT ON
        let distinct = match &s.distinct {
            None => BoundDistinct::None,
            Some(Distinct::All) => {
                Self::check_distinct_order_by(&order_by, &order_spans, tl.n_visible)?;
                BoundDistinct::All
            }
            Some(Distinct::On(exprs)) => BoundDistinct::On(self.resolve_distinct_on(
                exprs,
                &order_by,
                &mut tl,
                &cx(ParseExprKind::DistinctOn),
            )?),
        };

        // 8. LIMIT / OFFSET
        let limit = self.transform_limit(q.limit.as_ref(), &cx(ParseExprKind::Limit))?;
        let offset = self.transform_limit(q.offset.as_ref(), &cx(ParseExprKind::Offset))?;

        // 9. 集約の有無（D3-4）と、グループ化の検査
        let has_agg = !group_by.is_empty()
            || having.is_some()
            || tl
                .exprs
                .iter()
                .any(super::bound::BoundExpr::contains_aggregate);
        let columns = tl.columns();
        let mut sel = BoundSelect {
            rtable: fb.rtable,
            from,
            filter,
            group_by,
            having,
            has_agg,
            targets: tl.exprs,
            n_visible: tl.n_visible,
            distinct,
        };
        if has_agg {
            Self::check_grouping(&mut sel)?;
        }
        Ok(BoundQuery {
            ctes: Vec::new(),
            body: BoundSetExpr::Select(Box::new(sel)),
            order_by,
            limit,
            offset,
            columns,
        })
    }

    /// SELECT 句（`*` の展開、別名、出力名、出力列の由来）。
    fn transform_target_list(
        &self,
        items: &[SelectItem],
        cx: &ExprCtx<'_>,
        env: &QueryEnv<'_>,
    ) -> Result<TargetList> {
        let mut tl = TargetList::default();
        for item in items {
            match item {
                SelectItem::Expr { expr, alias, .. } => {
                    let mut b = self.transform_expr(expr, cx)?;
                    if env.resolve_unknowns {
                        b = resolve_unknown(b);
                    }
                    let name = alias
                        .as_ref()
                        .map_or_else(|| figure_colname(expr), |a| a.value.clone());
                    let origin = Self::origin_of(&b, cx.scopes);
                    tl.push(b, name, origin);
                }
                SelectItem::Wildcard(span) => {
                    Self::push_star(cx.scopes, None, *span, &mut tl)?;
                }
                SelectItem::QualifiedWildcard(q, span) => {
                    Self::push_star(cx.scopes, Some(q), *span, &mut tl)?;
                }
            }
        }
        tl.n_visible = tl.exprs.len();
        Ok(tl)
    }

    fn push_star(
        scopes: &ScopeStack,
        qual: Option<&ObjectName>,
        span: Span,
        tl: &mut TargetList,
    ) -> Result<()> {
        for col in scopes.expand_star(qual.map(|q| q.parts.as_slice()), span)? {
            tl.push(col.expr, col.name, col.origin);
        }
        Ok(())
    }

    /// 出力式が `Var` そのものなら元の表の列（`markTargetListOrigin`）。
    fn origin_of(b: &BoundExpr, scopes: &ScopeStack) -> (Oid, i16) {
        match &b.kind {
            BoundExprKind::Column(v) => scopes.origin(*v),
            _ => (0, 0),
        }
    }

    /// Analyzes the rows of a VALUES list (no coercion yet). Errors on
    /// rows of different lengths.
    pub(super) fn transform_values_rows(&self, v: &Values) -> Result<Vec<Vec<BoundExpr>>> {
        // 各行は rtable が空の 1 スコープ（03 §3.2.1）。
        let scopes = ScopeStack::empty();
        let cx = ExprCtx::new(&scopes, ParseExprKind::Values);
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

    fn analyze_values_query(
        &self,
        v: &Values,
        q: &Query,
        _env: &QueryEnv<'_>,
    ) -> Result<BoundQuery> {
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
        self.values_query(rows, types, q)
    }

    /// VALUES の行（列型にそろえ済み）に、問い合わせの ORDER BY / LIMIT / OFFSET を付ける。
    /// 本体は `BoundSetExpr::Values`。ORDER BY が出力列でない式（resjunk が要る）のときだけ、
    /// `RteKind::Values` を FROM に持つ SELECT で表す（`m4/03` §3.2.7）。
    pub(super) fn values_query(
        &self,
        rows: Vec<Vec<BoundExpr>>,
        types: Vec<SqlType>,
        q: &Query,
    ) -> Result<BoundQuery> {
        let names: Vec<String> = (1..=types.len()).map(|i| format!("column{i}")).collect();
        let rte = RteId(0);
        let rel_columns: Vec<ScopeColumn> = names
            .iter()
            .zip(&types)
            .map(|(n, t)| ScopeColumn {
                name: n.clone(),
                ty: *t,
                attnum: 0,
            })
            .collect();
        let scopes = ScopeStack::single(vec![ScopeRel {
            rte,
            hidden_name: None,
            refname: "*VALUES*".to_owned(),
            schema: None,
            table_oid: 0,
            system_columns: false,
            columns: rel_columns,
        }]);
        let mut tl = TargetList::default();
        for (i, (name, t)) in names.iter().zip(&types).enumerate() {
            let var = Var::user(rte, u16::try_from(i).unwrap_or(u16::MAX));
            tl.push(
                BoundExpr::new(BoundExprKind::Column(var), *t, q.span),
                name.clone(),
                (0, 0),
            );
        }
        tl.n_visible = tl.exprs.len();
        let (order_by, _) = self.transform_sort_clause(
            &q.order_by,
            &mut tl,
            &ExprCtx::new(&scopes, ParseExprKind::OrderBy),
        )?;
        let limit = self.transform_limit(
            q.limit.as_ref(),
            &ExprCtx::new(&scopes, ParseExprKind::Limit),
        )?;
        let offset = self.transform_limit(
            q.offset.as_ref(),
            &ExprCtx::new(&scopes, ParseExprKind::Offset),
        )?;
        let columns = tl.columns();
        let body = if tl.exprs.len() == tl.n_visible {
            BoundSetExpr::Values { rows, types }
        } else {
            // 出力列でない式で整列する: FROM 句の VALUES を読む SELECT にする。
            BoundSetExpr::Select(Box::new(BoundSelect {
                rtable: vec![Rte {
                    kind: RteKind::Values { rows },
                    refname: Some("*VALUES*".to_owned()),
                    columns: names
                        .iter()
                        .zip(&types)
                        .map(|(n, t)| RteColumn {
                            name: n.clone(),
                            ty: *t,
                        })
                        .collect(),
                    span: q.span,
                }],
                from: vec![super::bound::FromItem::Scan(rte)],
                filter: None,
                group_by: Vec::new(),
                having: None,
                has_agg: false,
                targets: tl.exprs,
                n_visible: tl.n_visible,
                distinct: BoundDistinct::None,
            }))
        };
        let bq = BoundQuery {
            ctes: Vec::new(),
            body,
            order_by,
            limit,
            offset,
            columns,
        };
        Ok(bq)
    }

    /// ORDER BY（`transformSortClause`）。各項目を [`Self::find_target_entry`] で `targets` の位置にする。
    /// 同じ式・向きの重複は 1 つにする（`order by a, a`）。第 2 の値は各キーの式の位置（42P10 の報告用）。
    fn transform_sort_clause(
        &self,
        items: &[OrderByItem],
        tl: &mut TargetList,
        cx: &ExprCtx<'_>,
    ) -> Result<(Vec<BoundSortKey>, Vec<Span>)> {
        let mut keys: Vec<BoundSortKey> = Vec::with_capacity(items.len());
        let mut spans = Vec::with_capacity(items.len());
        for item in items {
            let Resolved::Target(target) =
                self.find_target_entry(&item.expr, ClauseKind::OrderBy, tl, cx)?
            else {
                return Err(Error::internal(
                    "ORDER BY resolved to a grouping expression",
                ));
            };
            let descending = item.direction == Some(SortDirection::Desc);
            let nulls_first = match item.nulls {
                Some(NullsOrder::First) => true,
                Some(NullsOrder::Last) => false,
                None => descending,
            };
            if keys
                .iter()
                .any(|k| k.target == target && k.descending == descending)
            {
                continue;
            }
            keys.push(BoundSortKey {
                target,
                descending,
                nulls_first,
            });
            spans.push(leading_span(&item.expr));
        }
        Ok((keys, spans))
    }

    /// `SELECT DISTINCT` の ORDER BY は出力列だけを指せる（42P10。位置は ORDER BY の式の先頭）。
    fn check_distinct_order_by(
        keys: &[BoundSortKey],
        spans: &[Span],
        n_visible: usize,
    ) -> Result<()> {
        match keys.iter().zip(spans).find(|(k, _)| k.target >= n_visible) {
            Some((_, span)) => Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
            )
            .with_span(*span)),
            None => Ok(()),
        }
    }

    /// `findTargetlistEntrySQL92` と `SQL99`（`m4/03` §5.6.1）。項目 `e` を `targets` の位置にする。
    ///
    /// 1. 単純な名前: GROUP BY は FROM の列が先（見つかれば手順 3 へ）。それ以外と GROUP BY で FROM の列に
    ///    ない名前は、出力列の名前で探す。
    /// 2. 整数リテラルは出力列の位置。ほかのリテラルは `42601`。
    /// 3. 式: 入力に対して解析し、`targets` に同じ式があればその位置。なければ ORDER BY / DISTINCT ON は
    ///    resjunk として足し、GROUP BY は式そのものを返す。
    fn find_target_entry(
        &self,
        e: &Expr,
        kind: ClauseKind,
        tl: &mut TargetList,
        cx: &ExprCtx<'_>,
    ) -> Result<Resolved> {
        let clause = kind.name();
        if let Expr::Column { parts, span } = e
            && let [name] = parts.as_slice()
            && !(kind == ClauseKind::GroupBy && Self::is_input_column(parts, *span, cx)?)
        {
            let mut found: Option<usize> = None;
            for i in 0..tl.n_visible {
                if tl.names[i] != name.value {
                    continue;
                }
                match found {
                    Some(j) if !tl.exprs[j].same_as(&tl.exprs[i]) => {
                        return Err(Error::new(
                            sqlstate::AMBIGUOUS_COLUMN,
                            format!("{clause} \"{}\" is ambiguous", name.value),
                        )
                        .with_span(*span));
                    }
                    Some(_) => {}
                    None => found = Some(i),
                }
            }
            if let Some(i) = found {
                return Ok(Resolved::Target(i));
            }
        }
        if let Expr::Literal { value, span } = e {
            let pos = match value {
                Literal::Integer(s) => parse_int_literal(s),
                _ => None,
            };
            let Some(pos) = pos else {
                return Err(Error::syntax_at(
                    *span,
                    format!("non-integer constant in {clause}"),
                ));
            };
            if pos < 1 || usize::try_from(pos).map_or(true, |p| p > tl.n_visible) {
                return Err(Error::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    format!("{clause} position {pos} is not in select list"),
                )
                .with_span(*span));
            }
            return Ok(Resolved::Target(usize::try_from(pos - 1).unwrap_or(0)));
        }
        let b = resolve_unknown(self.transform_expr(e, cx)?);
        if let Some(i) = tl.exprs.iter().position(|t| t.same_as(&b)) {
            return Ok(Resolved::Target(i));
        }
        Ok(match kind {
            ClauseKind::GroupBy => Resolved::Group(b),
            ClauseKind::OrderBy | ClauseKind::DistinctOn => Resolved::Target(tl.push_junk(b)),
        })
    }

    /// GROUP BY の単純な名前が FROM の列（現スコープ）として見つかるか。曖昧なら 42702。
    /// 外側のスコープの列と、見つからない名前は `false`（出力列の別名として探す）。
    fn is_input_column(
        parts: &[crate::sql::ast::Ident],
        span: Span,
        cx: &ExprCtx<'_>,
    ) -> Result<bool> {
        match cx.scopes.resolve_column(parts, span) {
            Ok(c) => Ok(matches!(c.kind, BoundExprKind::Column(v) if v.levels_up == 0)),
            Err(e) if e.sqlstate == sqlstate::UNDEFINED_COLUMN => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// LIMIT / OFFSET: coerced to int8, no variables of the current level（副問い合わせの中の参照も数える。
    /// 5.12.2）。
    fn transform_limit(&self, e: Option<&Expr>, cx: &ExprCtx<'_>) -> Result<Option<BoundExpr>> {
        let Some(e) = e else {
            return Ok(None);
        };
        let b = self.transform_expr(e, cx)?;
        let b = self.coerce_to_specific_type(b, SqlType::INT8, cx.kind.name())?;
        if let Some(span) = first_current_level_var(&b) {
            return Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!("argument of {} must not contain variables", cx.kind.name()),
            )
            .with_span(span));
        }
        Ok(Some(b))
    }

    /// `GROUP BY` の項目を解決する（5.6.3）。出力列の名前・位置番号は `targets` の式の複製にする。
    /// 集約を含む出力列を指したら `42803`。
    pub(super) fn resolve_group_by(
        &self,
        items: &[Expr],
        tl: &mut TargetList,
        cx: &ExprCtx<'_>,
    ) -> Result<Vec<BoundExpr>> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match self.find_target_entry(item, ClauseKind::GroupBy, tl, cx)? {
                Resolved::Target(pos) => {
                    let target = &tl.exprs[pos];
                    if let Some(agg) = find_aggregate(target) {
                        return Err(Error::new(
                            sqlstate::GROUPING_ERROR,
                            "aggregate functions are not allowed in GROUP BY",
                        )
                        .with_span(agg.span));
                    }
                    out.push(target.clone());
                }
                Resolved::Group(e) => out.push(e),
            }
        }
        Ok(out)
    }

    /// `DISTINCT ON` の式を解決し、`targets` の位置を返す（5.6.4。`transformDistinctOnClause`）。
    ///
    /// 戻り値は ORDER BY の先頭から DISTINCT ON の式に一致する項目を順に、続けて、まだ出ていない
    /// DISTINCT ON の式を書かれた順に並べたもの（`BoundDistinct::On`。C-23）。
    pub(super) fn resolve_distinct_on(
        &self,
        exprs: &[Expr],
        keys: &[BoundSortKey],
        tl: &mut TargetList,
        cx: &ExprCtx<'_>,
    ) -> Result<Vec<usize>> {
        let mut refs = Vec::with_capacity(exprs.len());
        for e in exprs {
            match self.find_target_entry(e, ClauseKind::DistinctOn, tl, cx)? {
                Resolved::Target(t) => refs.push(t),
                Resolved::Group(_) => {
                    return Err(Error::internal(
                        "DISTINCT ON resolved to a grouping expression",
                    ));
                }
            }
        }
        let mismatch = |span: Span| {
            Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
            )
            .with_span(span)
        };
        let mut skipped = false;
        let mut result: Vec<usize> = Vec::with_capacity(refs.len());
        let mut seen: Vec<usize> = Vec::with_capacity(keys.len());
        for key in keys {
            // 同じ式の重複した ORDER BY 項目は PG が取り除く（無いものとして扱う）。
            if seen.contains(&key.target) {
                continue;
            }
            seen.push(key.target);
            if let Some(i) = refs.iter().position(|r| *r == key.target) {
                if skipped {
                    return Err(mismatch(leading_span(&exprs[i])));
                }
                if !result.contains(&key.target) {
                    result.push(key.target);
                }
            } else {
                skipped = true;
            }
        }
        for (i, r) in refs.iter().enumerate() {
            if result.contains(r) {
                continue;
            }
            if skipped {
                return Err(mismatch(leading_span(&exprs[i])));
            }
            result.push(*r);
        }
        Ok(result)
    }
}

/// ORDER BY / GROUP BY / DISTINCT ON のどれか（文言の `{clause}` と式の文脈）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClauseKind {
    OrderBy,
    GroupBy,
    DistinctOn,
}

impl ClauseKind {
    fn name(self) -> &'static str {
        match self {
            ClauseKind::OrderBy => "ORDER BY",
            ClauseKind::GroupBy => "GROUP BY",
            ClauseKind::DistinctOn => "DISTINCT ON",
        }
    }
}

/// [`Analyzer::find_target_entry`] の結果。
enum Resolved {
    /// `targets` の位置。
    Target(usize),
    /// GROUP BY の式（`targets` にない）。
    Group(BoundExpr),
}

/// 式の中の現スコープの `Var`（`SubLink` の中はその深さを引く）の最初の位置。LIMIT / OFFSET の検査。
fn first_current_level_var(e: &BoundExpr) -> Option<Span> {
    let mut found: Option<Span> = None;
    e.walk(&mut |n| {
        match &n.kind {
            BoundExprKind::Column(v) if v.levels_up == 0 => {
                found.get_or_insert(n.span);
            }
            BoundExprKind::SubLink { query, .. } => {
                query.walk_exprs(1, &mut |x, depth| {
                    if let BoundExprKind::Column(v) = &x.kind
                        && v.levels_up == depth
                    {
                        found.get_or_insert(x.span);
                    }
                });
            }
            _ => {}
        }
        true
    });
    found
}

/// 式の先頭の位置（二項演算子などは `span` が演算子から始まるので、最も左のオペランドまでさかのぼる。
/// PostgreSQL の `exprLocation`）。
fn leading_span(e: &Expr) -> Span {
    let first: Option<&Expr> = match e {
        Expr::BinaryOp { left, .. }
        | Expr::And { left, .. }
        | Expr::Or { left, .. }
        | Expr::IsDistinctFrom { left, .. } => Some(left),
        Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Between { expr, .. }
        | Expr::InList { expr, .. }
        | Expr::InSubquery { expr, .. }
        | Expr::QuantifiedSubquery { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Like { expr, .. } => Some(expr),
        _ => None,
    };
    let span = e.span();
    first.map_or(span, |f| {
        Span::new(leading_span(f).start.min(span.start), span.end)
    })
}
