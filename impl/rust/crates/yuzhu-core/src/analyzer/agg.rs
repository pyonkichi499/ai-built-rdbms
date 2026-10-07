//! 集約の解決とグループ化の検査（`m4/03-parser-analyzer.md` §5.5。N2）。
//!
//! - [`Analyzer::analyze_agg_call`]: `Expr::Function` が集約なら解析する（`parse_agg.c` の
//!   `transformAggregateCall` と `func_get_detail` の集約側）。
//! - [`Analyzer::check_grouping`]: `parseCheckAggregates` / `check_ungrouped_columns`。主キーへの関数従属で
//!   許された列は `group_by` の末尾に足す（D3-10）。
//!
//! 同じスコープの集約の入れ子（`sum(count(*))`）は、引数・FILTER を解析した後に「引数の中に同じスコープの
//! 集約があるか」で検出する（`BoundExpr` の走査は `SubLink` の `query` に降りないので、副問い合わせの中の
//! 集約は数えない）。`has_agg` も解析後の式から求める（`analyzer/select.rs`）。

use super::Analyzer;
use super::bound::{BoundAggCall, BoundExpr, BoundExprKind, BoundSelect, Rte, RteKind};
use super::coerce::{CoercionContext, resolve_unknown, tname};
use super::expr::ExprCtx;
use super::scope::ParseExprKind;
use crate::catalog::BuiltinAggregate;
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{AggOrderKey, SYSTEM_COL_BASE, Var, system_col_index};
use crate::sql::ast::Expr;
use crate::types::{Oid, SqlType};

/// `oid::ANY`（`count("any")` の引数）。
const ANY_OID: Oid = 2276;

/// `aggregates_named` に行がない既知の集約名（D3-18）。`0A000 aggregate function X is not supported yet`。
const UNSUPPORTED_AGGREGATES: &[&str] = &[
    "array_agg",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "bit_and",
    "bit_or",
    "bit_xor",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "xmlagg",
    "corr",
    "covar_pop",
    "covar_samp",
    "mode",
    "percentile_cont",
    "percentile_disc",
    "range_agg",
];

/// 文脈ごとの集約の禁止（5.12.1）。`Some(name)` なら `aggregate functions are not allowed in {name}`。
/// N1 が `ParseExprKind` に足す文脈（JOIN conditions、functions in FROM、RETURNING）は、足した担当がここに足す。
pub(super) fn agg_forbidden_in(kind: ParseExprKind) -> Option<&'static str> {
    match kind {
        ParseExprKind::Where => Some("WHERE"),
        ParseExprKind::GroupBy => Some("GROUP BY"),
        ParseExprKind::Limit => Some("LIMIT"),
        ParseExprKind::Offset => Some("OFFSET"),
        ParseExprKind::Filter => Some("FILTER"),
        ParseExprKind::Values => Some("VALUES"),
        ParseExprKind::UpdateSet => Some("UPDATE"),
        ParseExprKind::Check => Some("check constraints"),
        ParseExprKind::JoinOn => Some("JOIN conditions"),
        ParseExprKind::FromFunction => Some("functions in FROM"),
        ParseExprKind::Returning => Some("RETURNING"),
        ParseExprKind::ColumnDefault => Some("DEFAULT expressions"),
        _ => None,
    }
}

/// 式の中（`SubLink` の `query` の外）の、最初の集約（先行順）。
pub(super) fn find_aggregate(e: &BoundExpr) -> Option<&BoundExpr> {
    if matches!(e.kind, BoundExprKind::Aggregate(_)) {
        return Some(e);
    }
    e.children().into_iter().find_map(find_aggregate)
}

/// 式が参照する `Var` が属するスコープの、現スコープからの距離の最小値（`SubLink` の中は深さを引く）。
/// `Var` がなければ `None`（現スコープに属する集約として扱う）。
fn min_var_level<'a>(exprs: impl IntoIterator<Item = &'a BoundExpr>) -> Option<u16> {
    let mut min: Option<u16> = None;
    let mut note = |v: Var, depth: u16| {
        if let Some(level) = v.levels_up.checked_sub(depth) {
            min = Some(min.map_or(level, |m| m.min(level)));
        }
    };
    for e in exprs {
        e.walk(&mut |n| {
            match &n.kind {
                BoundExprKind::Column(v) => note(*v, 0),
                BoundExprKind::SubLink { query, .. } => {
                    query.walk_exprs(1, &mut |x, depth| {
                        if let BoundExprKind::Column(v) = &x.kind {
                            note(*v, depth);
                        }
                    });
                }
                _ => {}
            }
            true
        });
    }
    min
}

fn function_missing(name: &str, inputs: &[Oid], span: Span) -> Error {
    let shown = inputs
        .iter()
        .map(|t| tname(*t))
        .collect::<Vec<_>>()
        .join(", ");
    Error::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("function {name}({shown}) does not exist"),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
    .with_span(span)
}

impl Analyzer<'_> {
    /// `Expr::Function` が集約なら解析して返す。集約でなければ `Ok(None)`（呼び出し側が通常の関数として
    /// 解決する）。5.5.1 の手順。
    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_agg_call(&self, e: &Expr, cx: &ExprCtx<'_>) -> Result<Option<BoundExpr>> {
        let Expr::Function {
            name,
            args,
            distinct,
            star,
            filter,
            order_by,
            span,
        } = e
        else {
            return Ok(None);
        };
        let fname = name.name().value.as_str();
        // `public.count(*)` などは通常の関数解決に任せる（`42883` / `3F000`）。
        if name.schema().is_some_and(|s| s.value != "pg_catalog") {
            return Ok(None);
        }
        let candidates = self.catalog.aggregates_named(fname);
        if candidates.is_empty() {
            if !order_by.is_empty() && !self.catalog.functions_named(fname).is_empty() {
                return Err(Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("ORDER BY specified, but {fname} is not an aggregate function"),
                )
                .with_span(*span));
            }
            return self.not_an_aggregate(fname, *distinct, *star, filter.is_some(), *span);
        }

        // 引数と FILTER（引数の中の集約は同じ文脈の検査を受ける）。
        let mut bargs = Vec::with_capacity(args.len());
        for a in args {
            bargs.push(self.transform_expr(a, cx)?);
        }
        let bfilter = match filter {
            Some(f) => Some(self.transform_expr(
                f,
                &ExprCtx {
                    kind: ParseExprKind::Filter,
                    ..*cx
                },
            )?),
            None => None,
        };
        let mut border = Vec::with_capacity(order_by.len());
        for item in order_by {
            border.push((
                self.transform_expr(&item.expr, cx)?,
                matches!(item.direction, Some(crate::sql::ast::SortDirection::Desc)),
                item.nulls,
            ));
        }

        // 集約の属するスコープ（D3-4）。外側のスコープに属する集約は未対応。
        if min_var_level(
            bargs
                .iter()
                .chain(bfilter.iter())
                .chain(border.iter().map(|k| &k.0)),
        )
        .is_some_and(|r| r > 0)
        {
            return Err(Error::not_supported(
                "aggregate functions of an outer query level are not supported yet",
            )
            .with_span(*span));
        }
        if let Some(clause) = agg_forbidden_in(cx.kind) {
            return Err(Error::new(
                sqlstate::GROUPING_ERROR,
                format!("aggregate functions are not allowed in {clause}"),
            )
            .with_span(*span));
        }
        if let Some(inner) = bargs
            .iter()
            .chain(bfilter.iter())
            .chain(border.iter().map(|k| &k.0))
            .find_map(find_aggregate)
        {
            return Err(Error::new(
                sqlstate::GROUPING_ERROR,
                "aggregate function calls cannot be nested",
            )
            .with_span(inner.span));
        }

        let inputs: Vec<Oid> = bargs.iter().map(|a| a.ty.oid).collect();
        if inputs.is_empty() && !*star && candidates.iter().any(|c| c.args.is_empty()) {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("{fname}(*) must be used to call a parameterless aggregate function"),
            )
            .with_span(*span));
        }
        let func = self.resolve_aggregate(fname, &candidates, &inputs, *span)?;
        let mut coerced = Vec::with_capacity(bargs.len());
        for (a, declared) in bargs.into_iter().zip(func.args) {
            coerced.push(self.coerce_agg_arg(a, *declared)?);
        }
        let bfilter = bfilter
            .map(|f| self.coerce_to_boolean(f, "FILTER"))
            .transpose()?;
        Ok(Some(BoundExpr::new(
            BoundExprKind::Aggregate(Box::new(BoundAggCall {
                func,
                args: coerced,
                distinct: *distinct,
                filter: bfilter,
                order_by: border
                    .into_iter()
                    .map(|(expr, descending, nulls)| AggOrderKey {
                        expr,
                        descending,
                        nulls_first: nulls
                            .map_or(descending, |n| n == crate::sql::ast::NullsOrder::First),
                    })
                    .collect(),
            })),
            SqlType::of(func.result),
            *span,
        )))
    }

    /// `aggregates_named` が空のとき（5.5.1 の 2）。
    fn not_an_aggregate(
        &self,
        fname: &str,
        distinct: bool,
        star: bool,
        filter: bool,
        span: Span,
    ) -> Result<Option<BoundExpr>> {
        if UNSUPPORTED_AGGREGATES.contains(&fname) {
            return Err(super::not_supported(
                &format!("aggregate function {fname}"),
                span,
            ));
        }
        if !(distinct || star || filter) || self.catalog.functions_named(fname).is_empty() {
            return Ok(None);
        }
        if distinct {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("DISTINCT specified, but {fname} is not an aggregate function"),
            )
            .with_span(span));
        }
        if filter {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("FILTER specified, but {fname} is not an aggregate function"),
            )
            .with_span(span));
        }
        Err(function_missing(fname, &[], span))
    }

    /// 引数の型から集約の行を選ぶ（`func_get_detail`。5.5.1 の 7）。
    /// 完全一致 → 暗黙キャストで届く候補（`ANY` は何でも受ける）→ 候補が複数なら絞り込み。
    fn resolve_aggregate(
        &self,
        name: &str,
        candidates: &[&'static BuiltinAggregate],
        inputs: &[Oid],
        span: Span,
    ) -> Result<&'static BuiltinAggregate> {
        let same_arity: Vec<&'static BuiltinAggregate> = candidates
            .iter()
            .copied()
            .filter(|c| c.args.len() == inputs.len())
            .collect();
        if let Some(exact) = same_arity.iter().find(|c| c.args == inputs) {
            return Ok(exact);
        }
        let matched: Vec<&'static BuiltinAggregate> = same_arity
            .into_iter()
            .filter(|c| {
                inputs.iter().zip(c.args).all(|(i, a)| {
                    *a == ANY_OID || self.can_coerce(*i, *a, CoercionContext::Implicit)
                })
            })
            .collect();
        match matched.len() {
            0 => Err(function_missing(name, inputs, span)),
            1 => Ok(matched[0]),
            _ => self.select_aggregate(&matched, inputs).ok_or_else(|| {
                let shown = inputs.iter().map(|t| tname(*t)).collect::<Vec<_>>().join(", ");
                Error::new(
                    sqlstate::AMBIGUOUS_FUNCTION,
                    format!("function {name}({shown}) is not unique"),
                )
                .with_hint(
                    "Could not choose a best candidate function. You might need to add explicit type casts.",
                )
                .with_span(span)
            }),
        }
    }

    /// `func_select_candidate` の集約向けの簡約版。複数の候補から 1 つに絞れなければ `None`。
    ///
    /// 1. 既知の型の入力と完全に一致する引数が最も多い候補。
    /// 2. 入力の型カテゴリの推奨型（`text`、`float8` など）を引数に持つ候補。
    /// 3. unknown の入力: 文字列カテゴリの候補があれば、その中の推奨型。なければ決められない
    ///    （PostgreSQL の `sum` / `avg` は `interval` / `money` の行があるため `sum('1')` が曖昧になる）。
    fn select_aggregate(
        &self,
        matched: &[&'static BuiltinAggregate],
        inputs: &[Oid],
    ) -> Option<&'static BuiltinAggregate> {
        let unknown = |t: Oid| t == crate::types::oid::UNKNOWN;
        let mut cands: Vec<&'static BuiltinAggregate> = matched.to_vec();
        let keep_best = |cands: Vec<&'static BuiltinAggregate>,
                         score: &dyn Fn(&BuiltinAggregate) -> usize| {
            let best = cands.iter().map(|c| score(c)).max().unwrap_or(0);
            cands
                .into_iter()
                .filter(|c| score(c) == best)
                .collect::<Vec<_>>()
        };
        cands = keep_best(cands, &|c| {
            inputs
                .iter()
                .zip(c.args)
                .filter(|(i, a)| !unknown(**i) && *i == *a)
                .count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        cands = keep_best(cands, &|c| {
            inputs
                .iter()
                .zip(c.args)
                .filter(|(i, a)| {
                    !unknown(**i)
                        && (*i == *a
                            || (self.category(**a) == self.category(**i) && self.is_preferred(**a)))
                })
                .count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        for (pos, input) in inputs.iter().enumerate() {
            if !unknown(*input) {
                continue;
            }
            let strings: Vec<&'static BuiltinAggregate> = cands
                .iter()
                .copied()
                .filter(|c| self.category(c.args[pos]) == 'S')
                .collect();
            if strings.is_empty() {
                return None;
            }
            let preferred: Vec<&'static BuiltinAggregate> = strings
                .iter()
                .copied()
                .filter(|c| self.is_preferred(c.args[pos]))
                .collect();
            cands = if preferred.is_empty() {
                strings
            } else {
                preferred
            };
        }
        (cands.len() == 1).then(|| cands[0])
    }

    /// 引数を宣言型へ暗黙キャストする。`ANY` は変換しない（unknown の定数は text にする）。
    fn coerce_agg_arg(&self, arg: BoundExpr, declared: Oid) -> Result<BoundExpr> {
        if declared == ANY_OID {
            return Ok(resolve_unknown(arg));
        }
        let (src, span) = (arg.ty.oid, arg.span);
        self.coerce_type(arg, declared, CoercionContext::Implicit)?
            .ok_or_else(|| {
                Error::internal(format!(
                    "no implicit coercion from {} to {} for a resolved aggregate argument",
                    tname(src),
                    tname(declared)
                ))
                .with_span(span)
            })
    }

    /// 集約・GROUP BY の検査（`analyze_select` の最後。5.5.3）。`has_agg` のときだけ呼ぶ。
    /// `targets`（resjunk を含む）と `having` の、グループ化されていない列の参照を `42803` にする。
    /// 関数従属で許された列は `group_by` の末尾に足す。
    pub(super) fn check_grouping(sel: &mut BoundSelect) -> Result<()> {
        let mut checker = Grouping {
            rtable: &sel.rtable,
            group_by: &sel.group_by,
            deps: Vec::new(),
        };
        for e in sel.targets.iter().chain(sel.having.iter()) {
            checker.walk(e)?;
        }
        let deps = checker.deps;
        sel.group_by.extend(deps);
        Ok(())
    }
}

/// `check_ungrouped_columns` の走査の状態。
struct Grouping<'a> {
    rtable: &'a [Rte],
    group_by: &'a [BoundExpr],
    /// 関数従属で許された列（`group_by` の末尾に足す。重複なし）。
    deps: Vec<BoundExpr>,
}

impl Grouping<'_> {
    /// 現スコープの式の検査（`sublevels == 0`）。
    fn walk(&mut self, e: &BoundExpr) -> Result<()> {
        match &e.kind {
            // 現スコープの集約の中は、グループ化されていない列を含んでよい。
            BoundExprKind::Aggregate(_) => return Ok(()),
            BoundExprKind::Column(v) => {
                if v.levels_up == 0 && !self.var_is_grouped(*v, e) {
                    return Err(self.ungrouped(*v, 0, e.span));
                }
                return Ok(());
            }
            _ => {}
        }
        if self.group_by.iter().any(|g| g.same_as(e)) {
            return Ok(());
        }
        if let BoundExprKind::SubLink { query, .. } = &e.kind {
            let mut failure: Option<Error> = None;
            query.walk_exprs(1, &mut |x, depth| {
                if failure.is_some() {
                    return;
                }
                if let BoundExprKind::Column(v) = &x.kind
                    && v.levels_up == depth
                    && !self.var_is_grouped(v.with_levels_up(0), x)
                {
                    failure = Some(self.ungrouped(*v, depth, x.span));
                }
            });
            if let Some(err) = failure {
                return Err(err);
            }
        }
        for child in e.children() {
            self.walk(child)?;
        }
        Ok(())
    }

    /// `group_by` にあるか、主キーへの関数従属で許されるか。`v` は `levels_up = 0` に直した `Var`。
    fn var_is_grouped(&mut self, v: Var, node: &BoundExpr) -> bool {
        let in_group = |vars: &[BoundExpr], target: Var| {
            vars.iter()
                .any(|g| matches!(&g.kind, BoundExprKind::Column(gv) if *gv == target))
        };
        if in_group(self.group_by, v) || in_group(&self.deps, v) {
            return true;
        }
        let Some(rte) = self.rtable.get(usize::from(v.rte.0)) else {
            return false;
        };
        let RteKind::Table { table } = &rte.kind else {
            return false;
        };
        let Some(pk) = table.primary_key() else {
            return false;
        };
        let all_keys_grouped = !pk.columns.is_empty()
            && pk.columns.iter().all(|c| {
                u16::try_from(c.attnum - 1)
                    .is_ok_and(|col| in_group(self.group_by, Var::user(v.rte, col)))
            });
        if !all_keys_grouped {
            return false;
        }
        let mut dep = node.clone();
        dep.kind = BoundExprKind::Column(v);
        self.deps.push(dep);
        true
    }

    /// `column "t.b" must appear in the GROUP BY clause or be used in an aggregate function`。
    fn ungrouped(&self, v: Var, sublevels: u16, span: Span) -> Error {
        let name = self.column_label(v);
        let message = if sublevels == 0 {
            format!(
                "column \"{name}\" must appear in the GROUP BY clause or be used in an aggregate function"
            )
        } else {
            format!("subquery uses ungrouped column \"{name}\" from outer query")
        };
        Error::new(sqlstate::GROUPING_ERROR, message).with_span(span)
    }

    /// `{表の別名}.{列名}`（別名のない派生表・VALUES は `unnamed_subquery`）。
    fn column_label(&self, v: Var) -> String {
        let Some(rte) = self.rtable.get(usize::from(v.rte.0)) else {
            return "?".to_owned();
        };
        let refname = rte.refname.as_deref().unwrap_or("unnamed_subquery");
        let column = if v.is_system() {
            let i = usize::from(system_col_index_of(v));
            crate::catalog::schema::SYSTEM_COLUMNS
                .get(i)
                .map_or("?", |(n, _, _)| *n)
                .to_owned()
        } else {
            rte.columns
                .get(usize::from(v.col))
                .map_or_else(|| "?".to_owned(), |c| c.name.clone())
        };
        format!("{refname}.{column}")
    }
}

/// システム列の `Var` の、`SYSTEM_COLUMNS` の添字。
fn system_col_index_of(v: Var) -> u16 {
    v.system_column()
        .map_or(v.col.saturating_sub(SYSTEM_COL_BASE), system_col_index)
}
