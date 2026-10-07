//! 集合演算（`UNION` / `INTERSECT` / `EXCEPT`）。PostgreSQL の `transformSetOperationStmt` /
//! `transformSetOperationTree`（`m4/03-parser-analyzer.md` §3.2.6、§5.8、N3）。
//!
//! 腕は `BoundQuery`（ORDER BY / LIMIT / WITH を持てる）。腕どうしは兄弟で、同じスコープ連鎖
//! （`env.outer`）を持つ。CTE の連鎖は、腕ごとに 1 段（`CteScope::level`）作る（`CteRef.levels_up` は
//! `BoundQuery` の入れ子を数える）。

use super::Analyzer;
use super::bound::{
    BoundExpr, BoundExprKind, BoundQuery, BoundSetExpr, BoundSortKey, OutputColumn, SetOpKind,
};
use super::coerce::{CoercionContext, resolve_unknown, select_common_typmod};
use super::cte::CteScope;
use super::expr::{ExprCtx, contains_column_ref, parse_int_literal};
use super::scope::{ParseExprKind, QueryEnv, ScopeColumn, ScopeRel, ScopeStack};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{RteId, Var};
use crate::sql::ast::{
    CastSyntax, Expr, Literal, NullsOrder, OrderByItem, Query, QueryBody, SetOperator,
    SortDirection,
};
use crate::types::{SqlType, oid};

/// 集合演算の名前（エラーメッセージの `UNION` など）。
fn op_name(op: SetOperator) -> &'static str {
    match op {
        SetOperator::Union => "UNION",
        SetOperator::Intersect => "INTERSECT",
        SetOperator::Except => "EXCEPT",
    }
}

fn op_kind(op: SetOperator) -> SetOpKind {
    match op {
        SetOperator::Union => SetOpKind::Union,
        SetOperator::Intersect => SetOpKind::Intersect,
        SetOperator::Except => SetOpKind::Except,
    }
}

/// 問い合わせの `i` 番目の出力列の式の位置（エラーの位置に使う）。
fn output_span(q: &BoundQuery, i: usize) -> Span {
    match &q.body {
        BoundSetExpr::Select(s) => s.targets.get(i).map(|t| t.span),
        BoundSetExpr::Values { rows, .. } => rows.first().and_then(|r| r.get(i)).map(|e| e.span),
        BoundSetExpr::SetOp { left, .. } => Some(output_span(left, i)),
    }
    .unwrap_or_default()
}

/// 式の先頭の位置（PostgreSQL の `exprLocation` は演算子と左端の引数の小さい方）。
fn leftmost_span(e: &Expr) -> Span {
    match e {
        Expr::BinaryOp { left, .. }
        | Expr::And { left, .. }
        | Expr::Or { left, .. }
        | Expr::IsDistinctFrom { left, .. }
        | Expr::NullIf { left, .. } => leftmost_span(left),
        Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Between { expr, .. }
        | Expr::InList { expr, .. }
        | Expr::InSubquery { expr, .. }
        | Expr::QuantifiedSubquery { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Like { expr, .. } => leftmost_span(expr),
        Expr::Cast { expr, syntax, .. } if *syntax == CastSyntax::DoubleColon => {
            leftmost_span(expr)
        }
        other => other.span(),
    }
}

/// 腕の出力の `i` 番目（`Var { rte: 0, col: i }`。`left_coerce` / `right_coerce` の入力）。
fn arm_column(i: usize, ty: SqlType, span: Span) -> BoundExpr {
    let col = u16::try_from(i).unwrap_or(u16::MAX);
    BoundExpr::new(BoundExprKind::Column(Var::user(RteId(0), col)), ty, span)
}

impl Analyzer<'_> {
    /// `body`（`QueryBody::SetOp`）を解析し、`q` の ORDER BY / LIMIT / OFFSET を外枠に付ける。
    /// `env.ctes` はこの `BoundQuery` 自身の CTE スコープ（`analyze_query` が作る）。
    pub(super) fn analyze_set_operation(
        &self,
        body: &QueryBody,
        q: &Query,
        env: &QueryEnv<'_>,
    ) -> Result<BoundQuery> {
        let mut bq = self.set_tree(body, env)?;
        self.set_outer_clauses(&mut bq, q, env)?;
        Ok(bq)
    }

    /// `SetOp` の木 1 段（腕を解析して列をそろえる）。
    fn set_tree(&self, body: &QueryBody, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        crate::sql::stack::check_stack_depth()?;
        let QueryBody::SetOp {
            op,
            all,
            left,
            right,
            ..
        } = body
        else {
            return Err(Error::internal(
                "analyze_set_operation without a set operation",
            ));
        };
        let left_q = self.set_arm(left, env)?;
        let right_q = self.set_arm(right, env)?;
        self.combine_arms(*op, *all, left_q, right_q)
    }

    /// 腕 1 つ。unknown の出力列は残す（`resolve_unknowns = false`。D3-15）。
    fn set_arm(&self, arm: &QueryBody, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        let arm_env = QueryEnv {
            resolve_unknowns: false,
            ..*env
        };
        let (span, kind) = match arm {
            QueryBody::SetOp { .. } => {
                // 入れ子の集合演算も 1 つの `BoundQuery`（CTE の連鎖を 1 段足す）。
                let scope = CteScope::level(env.ctes);
                let inner_env = QueryEnv {
                    ctes: &scope,
                    ..arm_env
                };
                return self.set_tree(arm, &inner_env);
            }
            QueryBody::Nested(inner) => return self.analyze_query(inner, &arm_env),
            QueryBody::Select(s) => (s.span, arm.clone()),
            QueryBody::Values(v) => (v.span, arm.clone()),
        };
        let tmp = Query {
            with: None,
            body: kind,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            span,
        };
        self.analyze_query(&tmp, &arm_env)
    }

    /// 左右の腕を 1 つの `SetOp` にする（列数・型の決定・変換）。
    fn combine_arms(
        &self,
        op: SetOperator,
        all: bool,
        mut left: BoundQuery,
        mut right: BoundQuery,
    ) -> Result<BoundQuery> {
        let ctx = op_name(op);
        let n = left.columns.len();
        if right.columns.len() != n {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                format!("each {ctx} query must have the same number of columns"),
            )
            .with_span(output_span(&right, 0)));
        }
        let mut types = Vec::with_capacity(n);
        let mut left_exprs = Vec::with_capacity(n);
        let mut right_exprs = Vec::with_capacity(n);
        let (mut need_left, mut need_right) = (false, false);
        for i in 0..n {
            let (lt, rt) = (left.columns[i].ty, right.columns[i].ty);
            let lph = arm_column(i, lt, output_span(&left, i));
            let rph = arm_column(i, rt, output_span(&right, i));
            let common = if lt.oid == oid::UNKNOWN && rt.oid == oid::UNKNOWN {
                oid::TEXT
            } else {
                self.select_common_type(&[&lph, &rph], Some(ctx))?
                    .unwrap_or(oid::TEXT)
            };
            types.push(SqlType::new(
                common,
                select_common_typmod(&[&lph, &rph], common),
            ));
            left_exprs.push(self.coerce_arm_column(&mut left, i, common, ctx, &mut need_left)?);
            right_exprs.push(self.coerce_arm_column(
                &mut right,
                i,
                common,
                ctx,
                &mut need_right,
            )?);
        }
        let columns = left
            .columns
            .iter()
            .zip(&types)
            .map(|(c, t)| OutputColumn {
                name: c.name.clone(),
                ty: *t,
                table_oid: 0,
                attnum: 0,
            })
            .collect();
        Ok(BoundQuery {
            ctes: Vec::new(),
            body: BoundSetExpr::SetOp {
                op: op_kind(op),
                all,
                left: Box::new(left),
                right: Box::new(right),
                left_coerce: need_left.then_some(left_exprs),
                right_coerce: need_right.then_some(right_exprs),
                types,
            },
            order_by: Vec::new(),
            limit: None,
            offset: None,
            columns,
        })
    }

    /// 腕の `i` 番目の出力列を共通型 `common` にそろえる。unknown のリテラルは腕の式を直接書き換え
    /// （入力関数で評価。22P02 の位置はリテラル）、それ以外の型違いは変換式（暗黙キャスト）で表す。
    /// 戻り値は `left_coerce` / `right_coerce` の `i` 番目の要素。
    fn coerce_arm_column(
        &self,
        arm: &mut BoundQuery,
        i: usize,
        common: crate::types::Oid,
        ctx: &str,
        need: &mut bool,
    ) -> Result<BoundExpr> {
        let ty = arm.columns[i].ty;
        let span = output_span(arm, i);
        if ty.oid == common {
            return Ok(arm_column(i, ty, span));
        }
        if ty.oid == oid::UNKNOWN
            && let BoundSetExpr::Select(sel) = &mut arm.body
            && let Some(t) = sel.targets.get_mut(i)
            && t.ty.oid == oid::UNKNOWN
            && matches!(t.kind, BoundExprKind::Literal(_))
        {
            let lit = t.clone();
            let c = self
                .coerce_type(lit, common, CoercionContext::Implicit)?
                .ok_or_else(|| Error::internal("unknown literal cannot be coerced"))?;
            arm.columns[i].ty = c.ty;
            let new_ty = c.ty;
            *t = c;
            return Ok(arm_column(i, new_ty, span));
        }
        *need = true;
        self.coerce_to_common_type(arm_column(i, ty, span), common, ctx)
    }

    /// 外枠の ORDER BY / LIMIT / OFFSET（`m4/03` §5.8.1 の 6・7）。
    fn set_outer_clauses(&self, bq: &mut BoundQuery, q: &Query, env: &QueryEnv<'_>) -> Result<()> {
        if !q.order_by.is_empty() {
            let rel = ScopeRel {
                rte: RteId(0),
                refname: "*SELECT*".to_owned(),
                hidden_name: None,
                schema: None,
                table_oid: 0,
                system_columns: false,
                columns: bq
                    .columns
                    .iter()
                    .map(|c| ScopeColumn {
                        name: c.name.clone(),
                        ty: c.ty,
                        attnum: 0,
                    })
                    .collect(),
            };
            let scopes = match env.outer {
                Some(o) => o.with_frame(vec![rel]),
                None => ScopeStack::single(vec![rel]),
            };
            let cx = ExprCtx::new(&scopes, ParseExprKind::OrderBy).with_ctes(env.ctes);
            bq.order_by = q
                .order_by
                .iter()
                .map(|item| self.set_sort_key(item, &bq.columns, &cx))
                .collect::<Result<_>>()?;
        }
        bq.limit = self.set_limit(q.limit.as_ref(), ParseExprKind::Limit, env)?;
        bq.offset = self.set_limit(q.offset.as_ref(), ParseExprKind::Offset, env)?;
        Ok(())
    }

    /// 外枠の ORDER BY の項目 1 つ。名前は結果の列名、整数は列の位置、それ以外の式は
    /// 出力列そのものに限る（resjunk を足せないので 0A000）。
    fn set_sort_key(
        &self,
        item: &OrderByItem,
        cols: &[OutputColumn],
        cx: &ExprCtx<'_>,
    ) -> Result<BoundSortKey> {
        let target = self.set_sort_target(&item.expr, cols, cx)?;
        let descending = item.direction == Some(SortDirection::Desc);
        let nulls_first = match item.nulls {
            Some(NullsOrder::First) => true,
            Some(NullsOrder::Last) => false,
            None => descending,
        };
        Ok(BoundSortKey {
            target,
            descending,
            nulls_first,
        })
    }

    fn set_sort_target(&self, e: &Expr, cols: &[OutputColumn], cx: &ExprCtx<'_>) -> Result<usize> {
        if let Expr::Column { parts, span } = e
            && let [name] = parts.as_slice()
        {
            let mut hits = cols
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name == name.value)
                .map(|(i, _)| i);
            if let Some(first) = hits.next() {
                if hits.next().is_some() {
                    return Err(Error::new(
                        sqlstate::AMBIGUOUS_COLUMN,
                        format!("ORDER BY \"{}\" is ambiguous", name.value),
                    )
                    .with_span(*span));
                }
                return Ok(first);
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
            if pos < 1 || usize::try_from(pos).map_or(true, |p| p > cols.len()) {
                return Err(Error::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    format!("ORDER BY position {pos} is not in select list"),
                )
                .with_span(*span));
            }
            return Ok(usize::try_from(pos - 1).unwrap_or(0));
        }
        // 式: 結果の列だけの名前空間で解析する（未知の名前・修飾名はここでエラー）。
        let b = resolve_unknown(self.transform_expr(e, cx)?);
        if let BoundExprKind::Column(v) = &b.kind
            && v.levels_up == 0
            && v.rte == RteId(0)
            && !v.is_system()
        {
            return Ok(usize::from(v.col));
        }
        Err(Error::not_supported("invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
            .with_detail("Only result column names can be used, not expressions or functions.")
            .with_hint(
                "Add the expression/function to every SELECT, or move the UNION into a FROM clause.",
            )
            .with_span(leftmost_span(e)))
    }

    /// 外枠の LIMIT / OFFSET。int8 にそろえ、同じ問い合わせの列も外側の列も含められない。
    fn set_limit(
        &self,
        e: Option<&Expr>,
        kind: ParseExprKind,
        env: &QueryEnv<'_>,
    ) -> Result<Option<BoundExpr>> {
        let Some(e) = e else {
            return Ok(None);
        };
        let scopes = match env.outer {
            Some(o) => o.with_frame(Vec::<ScopeRel>::new()),
            None => ScopeStack::empty(),
        };
        let cx = ExprCtx::new(&scopes, kind).with_ctes(env.ctes);
        let b = self.transform_expr(e, &cx)?;
        let b = self.coerce_to_specific_type(b, SqlType::INT8, kind.name())?;
        if contains_column_ref(&b) {
            return Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!("argument of {} must not contain variables", kind.name()),
            )
            .with_span(b.span));
        }
        let mut outer_ref: Option<Span> = None;
        b.any(&mut |x| match &x.kind {
            BoundExprKind::Column(v) if v.levels_up >= 1 => {
                outer_ref = Some(x.span);
                true
            }
            _ => false,
        });
        if let Some(span) = outer_ref {
            return Err(super::not_supported(
                "correlated LIMIT or OFFSET on a set operation",
                span,
            ));
        }
        Ok(Some(b))
    }
}
