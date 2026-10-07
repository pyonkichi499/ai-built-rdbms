//! 副問い合わせ式（`Expr::{Subquery, Exists, InSubquery, QuantifiedSubquery}`）。
//! PostgreSQL の `transformSubLink`（`m4/03-parser-analyzer.md` §3.2.8、§5.7、N3）。
//!
//! 副問い合わせの本体は `cx.scopes` を外側のスコープ（`levels_up = 1`）にして `analyze_query` で
//! 解析する。`SubLink` は Bound の式のまま持ち、プランナが書き換える（§5.7.3）。

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind, BoundQuery};
use super::coerce::tname;
use super::cte::CteScope;
use super::expr::{ExprCtx, figure_colname};
use super::scope::{ParseExprKind, QueryEnv};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::SubLinkKind;
use crate::sql::ast::{Expr, Quantifier, Query, QueryBody, SelectItem};
use crate::types::{SqlType, oid};

/// `cannot use subquery in {name}` の name（副問い合わせを書けない文脈）。
fn sublink_forbidden_in(kind: ParseExprKind) -> Option<&'static str> {
    match kind {
        ParseExprKind::Check => Some("check constraint"),
        ParseExprKind::ColumnDefault => Some("DEFAULT expression"),
        _ => None,
    }
}

/// スカラー副問い合わせの出力列名（`FigureColname`。`m4/03` §5.11）。副問い合わせの最初の出力列の
/// 別名か、その式の名前。`*` は式だけでは分からないので `?column?`。
pub(super) fn subquery_colname(q: &Query) -> String {
    body_colname(&q.body)
}

fn body_colname(body: &QueryBody) -> String {
    match body {
        QueryBody::Select(s) => match s.targets.first() {
            Some(SelectItem::Expr { expr, alias, .. }) => alias
                .as_ref()
                .map_or_else(|| figure_colname(expr), |a| a.value.clone()),
            _ => "?column?".to_owned(),
        },
        QueryBody::Values(_) => "column1".to_owned(),
        QueryBody::SetOp { left, .. } => body_colname(left),
        QueryBody::Nested(inner) => subquery_colname(inner),
    }
}

fn bool_expr(kind: BoundExprKind, span: Span) -> BoundExpr {
    BoundExpr::new(kind, SqlType::BOOL, span)
}

/// `x op ANY|ALL (subquery)` の解析に必要な部品。
struct Quantified<'e> {
    lhs: &'e Expr,
    op: &'e str,
    kind: SubLinkKind,
    /// `NOT IN`。
    negated: bool,
    span: Span,
}

impl Analyzer<'_> {
    /// 副問い合わせ式 1 つを解析する。
    pub(super) fn analyze_sublink(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr> {
        let span = e.span();
        if cx.kind == ParseExprKind::Returning {
            return Err(super::not_supported("subquery in RETURNING", span));
        }
        if let Some(what) = sublink_forbidden_in(cx.kind) {
            return Err(
                Error::not_supported(format!("cannot use subquery in {what}")).with_span(span),
            );
        }
        if let Expr::QuantifiedSubquery {
            op_schema: Some(s), ..
        } = e
        {
            return Err(super::not_supported(
                "OPERATOR() in a subquery comparison",
                s.span,
            ));
        }
        let (Expr::Subquery { query, .. }
        | Expr::Exists { query, .. }
        | Expr::InSubquery { query, .. }
        | Expr::QuantifiedSubquery { query, .. }) = e
        else {
            return Err(Error::internal("analyze_sublink without a subquery"));
        };
        // 本体を先に解析する（本体のエラーが左辺のエラーより先に出る。PostgreSQL と同じ）。
        let root = CteScope::root();
        let ctes: &CteScope<'_> = cx.ctes.unwrap_or(&root);
        let env = QueryEnv {
            outer: Some(cx.scopes),
            ctes,
            resolve_unknowns: true,
        };
        let sub = self.analyze_query(query, &env)?;

        match e {
            Expr::Subquery { .. } => {
                if sub.columns.len() != 1 {
                    return Err(Error::new(
                        sqlstate::SYNTAX_ERROR,
                        "subquery must return only one column",
                    )
                    .with_span(span));
                }
                let ty = sub.columns[0].ty;
                Ok(BoundExpr::new(
                    BoundExprKind::SubLink {
                        kind: SubLinkKind::Scalar,
                        test: None,
                        query: Box::new(sub),
                    },
                    ty,
                    span,
                ))
            }
            Expr::Exists { .. } => Ok(bool_expr(
                BoundExprKind::SubLink {
                    kind: SubLinkKind::Exists,
                    test: None,
                    query: Box::new(sub),
                },
                span,
            )),
            Expr::InSubquery {
                expr,
                negated,
                span,
                ..
            } => self.quantified_sublink(
                &Quantified {
                    lhs: expr,
                    op: "=",
                    kind: SubLinkKind::Any,
                    negated: *negated,
                    span: *span,
                },
                sub,
                cx,
            ),
            Expr::QuantifiedSubquery {
                expr,
                op,
                quantifier,
                span,
                ..
            } => self.quantified_sublink(
                &Quantified {
                    lhs: expr,
                    op,
                    kind: match quantifier {
                        Quantifier::Any => SubLinkKind::Any,
                        Quantifier::All => SubLinkKind::All,
                    },
                    negated: false,
                    span: *span,
                },
                sub,
                cx,
            ),
            _ => Err(Error::internal("analyze_sublink without a subquery")),
        }
    }

    /// `IN` / `ANY` / `ALL` の左辺・列数・演算子（`m4/03` §5.7.1 の 4）。
    fn quantified_sublink(
        &self,
        q: &Quantified<'_>,
        sub: BoundQuery,
        cx: &ExprCtx<'_>,
    ) -> Result<BoundExpr> {
        let span = q.span;
        // a. 左辺（行値なら要素の並び）。
        let lhs: Vec<BoundExpr> = match q.lhs {
            Expr::Row { items, .. } => items
                .iter()
                .map(|i| self.transform_expr(i, cx))
                .collect::<Result<_>>()?,
            other => vec![self.transform_expr(other, cx)?],
        };
        // b. 列数。
        let n = sub.columns.len();
        if lhs.len() < n {
            return Err(
                Error::new(sqlstate::SYNTAX_ERROR, "subquery has too many columns").with_span(span),
            );
        }
        if lhs.len() > n {
            return Err(
                Error::new(sqlstate::SYNTAX_ERROR, "subquery has too few columns").with_span(span),
            );
        }
        // 行値の `<> ALL` は `NOT (= ANY)` に書き換える（§3.2.8）。比較は `=` で解決する。
        let row_not_all = n > 1 && matches!(q.op, "<>" | "!=") && q.kind == SubLinkKind::All;
        let (op, kind, negated) = if row_not_all {
            ("=", SubLinkKind::Any, !q.negated)
        } else {
            (q.op, q.kind, q.negated)
        };
        // c. 各列の比較。
        let mut cmps = Vec::with_capacity(n);
        for (i, l) in lhs.into_iter().enumerate() {
            let col = u16::try_from(i).map_err(|_| {
                Error::new(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many columns").with_span(span)
            })?;
            let out = BoundExpr::new(BoundExprKind::SubLinkOutput(col), sub.columns[i].ty, span);
            let cmp = self.make_op(op, Some(l), out, span)?;
            // d. 結果は bool。
            if cmp.ty.oid != oid::BOOL {
                return Err(Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "row comparison operator must yield type boolean, not type {}",
                        tname(cmp.ty.oid)
                    ),
                )
                .with_span(span));
            }
            cmps.push(cmp);
        }
        // e. 行値は `=` ANY（IN）と `<>` ALL だけ。
        if n > 1 && !row_not_all && !(op == "=" && kind == SubLinkKind::Any) {
            return Err(super::not_supported(
                "row comparison with this operator in a subquery",
                span,
            ));
        }
        let mut test = BoundExpr::and_all(cmps);
        test.span = span;
        let sl = bool_expr(
            BoundExprKind::SubLink {
                kind,
                test: Some(Box::new(test)),
                query: Box::new(sub),
            },
            span,
        );
        Ok(if negated {
            bool_expr(BoundExprKind::Not(Box::new(sl)), span)
        } else {
            sl
        })
    }
}
