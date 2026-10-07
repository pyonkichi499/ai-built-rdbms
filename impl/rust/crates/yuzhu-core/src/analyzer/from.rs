//! FROM 句（PostgreSQL の `transformFromClause`）。`m4/03-parser-analyzer.md` §4.4・§5.3・§5.4。
//!
//! 表（CTE を含む）・派生表・VALUES・FROM の関数（`generate_series`）・JOIN を解析し、RTE を
//! `FromBuilder.rtable` に（子を先に、JOIN の RTE を後に）、名前空間を `FromBuilder.ns` に積む。
//!
//! 結合の列（USING / NATURAL の併合列を含む）は解析時に子の列の式へ展開する（02 の D2）。結合 RTE を
//! 指す `Var` は作らない。

use std::sync::Arc;

use super::Analyzer;
use super::bound::{
    BoundExpr, BoundExprKind, FromItem, JoinColSource, JoinType, Rte, RteColumn, RteKind,
};
use super::cte::{CteLookup, CteScope};
use super::expr::ExprCtx;
use super::scope::{
    FromBuilder, NsItem, ParseExprKind, QueryEnv, ScopeColumn, ScopeRel, ScopeStack,
};
use crate::catalog::{FnKind, RelKind, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::RteId;
use crate::sql::ast::{
    Expr, Ident, JoinConstraint, JoinKind, ObjectName, Query, QueryBody, TableAlias, TableRef,
    Values,
};
use crate::types::{SqlType, oid};

/// 実表の名前空間の項目（別名・列別名つき）。`system_columns` はシステム列を参照できるか。
pub(super) fn table_rel(
    table: &TableDef,
    alias: Option<&TableAlias>,
    rte: RteId,
    system_columns: bool,
) -> Result<ScopeRel> {
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
        let mut names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        rename_columns(&mut names, Some(a))?;
        for (c, n) in columns.iter_mut().zip(names) {
            c.name = n;
        }
    }
    Ok(ScopeRel {
        rte,
        hidden_name: alias.map(|_| table.name.clone()),
        refname: alias.map_or_else(|| table.name.clone(), |a| a.name.value.clone()),
        schema: if alias.is_none() {
            Some(table.schema.clone())
        } else {
            None
        },
        table_oid: table.oid,
        columns,
        system_columns,
    })
}

/// 別名の列リストで列名を先頭から改名する。列数より多ければ `42P10`（位置なし）。
fn rename_columns(names: &mut [String], alias: Option<&TableAlias>) -> Result<()> {
    let Some(a) = alias else {
        return Ok(());
    };
    if a.columns.len() > names.len() {
        return Err(Error::new(
            sqlstate::INVALID_COLUMN_REFERENCE,
            format!(
                "table \"{}\" has {} columns available but {} columns specified",
                a.name.value,
                names.len(),
                a.columns.len()
            ),
        ));
    }
    for (n, new) in names.iter_mut().zip(&a.columns) {
        n.clone_from(&new.value);
    }
    Ok(())
}

fn no_position(mut e: Error) -> Error {
    e.cursor_byte = None;
    e.position = None;
    e
}

fn scope_columns(columns: &[RteColumn]) -> Vec<ScopeColumn> {
    columns
        .iter()
        .map(|c| ScopeColumn {
            name: c.name.clone(),
            ty: c.ty,
            attnum: 0,
        })
        .collect()
}

/// 左の項目を LATERAL なしでは参照できない形にした写し（`lateral_only`）。すでに `lateral_only` の
/// 項目（UPDATE の対象表）は旗をそのままにする。`ok` は LATERAL を付ければ参照できる位置か。
fn lateral_view(items: &[NsItem], ok: bool) -> Vec<NsItem> {
    items
        .iter()
        .cloned()
        .map(|mut i| {
            if !i.lateral_only {
                i.lateral_only = true;
                i.lateral_ok = ok;
            }
            i
        })
        .collect()
}

/// 参照できない（診断だけに使う）項目として足す写し。HINT は付けない。
fn hidden_view(items: &[NsItem]) -> Vec<NsItem> {
    items
        .iter()
        .cloned()
        .map(|mut i| {
            i.lateral_only = true;
            i.lateral_ok = false;
            i
        })
        .collect()
}

/// `checkNameSpaceConflicts`: 同じ名前（別名または表名）の項目が 2 つあれば `42712`。
fn check_ns_conflicts(existing: &[NsItem], added: &[NsItem]) -> Result<()> {
    for y in added.iter().filter(|i| i.rel_visible) {
        if existing
            .iter()
            .any(|x| x.rel_visible && x.rel.refname == y.rel.refname)
        {
            return Err(Error::new(
                sqlstate::DUPLICATE_ALIAS,
                format!("table name \"{}\" specified more than once", y.rel.refname),
            ));
        }
    }
    Ok(())
}

/// `FromItem` が持つ最上位の RTE。
fn item_rte(item: &FromItem) -> RteId {
    match item {
        FromItem::Scan(r) | FromItem::Join { rte: r, .. } => *r,
    }
}

/// 本体が ORDER BY / LIMIT / OFFSET / WITH を持たない `VALUES` ならその `Values`。
fn plain_values(q: &Query) -> Option<&Values> {
    if q.with.is_some() || !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
        return None;
    }
    match &q.body {
        QueryBody::Values(v) => Some(v),
        QueryBody::Nested(inner) => plain_values(inner),
        _ => None,
    }
}

impl FromBuilder {
    /// 次に積む RTE の `RteId`（65535 を超えたら `54000`）。
    pub(super) fn next_rte_id(&self) -> Result<RteId> {
        u16::try_from(self.rtable.len()).map(RteId).map_err(|_| {
            Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many range table entries",
            )
        })
    }

    /// 実表の RTE を積み、名前空間の項目を返す（`ns` には足さない）。
    fn table_item(
        &mut self,
        table: &Arc<TableDef>,
        alias: Option<&TableAlias>,
        system_columns: bool,
        span: Span,
    ) -> Result<(RteId, NsItem)> {
        let rte = self.next_rte_id()?;
        let rel = table_rel(table, alias, rte, system_columns)?;
        self.rtable.push(Rte {
            kind: RteKind::Table {
                table: Arc::clone(table),
            },
            refname: Some(rel.refname.clone()),
            columns: rel
                .columns
                .iter()
                .map(|c| RteColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                })
                .collect(),
            span,
        });
        Ok((rte, NsItem::new(rel)))
    }

    /// 実表の RTE と名前空間の項目を積む（DML の対象表）。
    pub(super) fn push_table(
        &mut self,
        table: &Arc<TableDef>,
        alias: Option<&TableAlias>,
        system_columns: bool,
        span: Span,
    ) -> Result<RteId> {
        let (rte, item) = self.table_item(table, alias, system_columns, span)?;
        self.ns.push(item);
        Ok(rte)
    }

    /// 列を持つ RTE（実表以外）を積む。`ScopeRel` は `refname` と列から作る。
    fn push_plain(
        &mut self,
        kind: RteKind,
        refname: &str,
        named: bool,
        columns: Vec<RteColumn>,
        hidden_name: Option<String>,
        span: Span,
    ) -> Result<(RteId, NsItem)> {
        let rte = self.next_rte_id()?;
        let rel = ScopeRel {
            rte,
            refname: refname.to_owned(),
            hidden_name,
            schema: None,
            table_oid: 0,
            columns: scope_columns(&columns),
            system_columns: false,
        };
        self.rtable.push(Rte {
            kind,
            refname: named.then(|| refname.to_owned()),
            columns,
            span,
        });
        let mut item = NsItem::new(rel);
        item.rel_visible = named;
        Ok((rte, item))
    }
}

impl Analyzer<'_> {
    /// カンマ区切りの FROM 項目を左から順に解析し、RTE を `fb.rtable` に、名前空間を `fb.ns` に積む。
    /// 戻り値は `FromItem` の並び（カンマ区切りごと）。
    pub(super) fn analyze_from_clause(
        &self,
        from: &[TableRef],
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
    ) -> Result<Vec<FromItem>> {
        let mut out = Vec::with_capacity(from.len());
        for t in from {
            let lateral = lateral_view(&fb.ns, true);
            let (item, ns) = self.analyze_from_item(t, fb, env, &lateral)?;
            check_ns_conflicts(&fb.ns, &ns)?;
            fb.ns.extend(ns);
            out.push(item);
        }
        Ok(out)
    }

    /// 1 つの項目（表・派生表・関数・JOIN）。`lateral` は左にあって参照できない項目（`lateral_only` 済み）。
    /// 戻り値の名前空間はまだ `fb.ns` に足さない。
    fn analyze_from_item(
        &self,
        t: &TableRef,
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
        lateral: &[NsItem],
    ) -> Result<(FromItem, Vec<NsItem>)> {
        crate::sql::stack::check_stack_depth()?;
        match t {
            TableRef::Table { name, alias, span } => {
                self.analyze_table_ref(name, alias.as_ref(), *span, fb, env)
            }
            TableRef::Subquery {
                query, alias, span, ..
            } => self.analyze_derived(query, alias.as_ref(), *span, fb, env, lateral),
            TableRef::Function {
                name,
                args,
                alias,
                span,
            } => self.analyze_from_function(name, args, alias.as_ref(), *span, fb, env, lateral),
            TableRef::Join {
                left,
                right,
                kind,
                constraint,
                span,
            } => self.analyze_join(left, right, *kind, constraint, *span, fb, env, lateral),
        }
    }

    /// 実表の名前を引く（`resolve_table`）。なければ診断を足す: 索引なら `42809`、同じ WITH の
    /// まだ参照できない項目なら DETAIL / HINT（`ctes` を渡したときだけ。DML の対象表は渡さない）。
    pub(super) fn resolve_relation(
        &self,
        name: &ObjectName,
        ctes: Option<&CteScope<'_>>,
    ) -> Result<Arc<TableDef>> {
        let e = match self.resolve_table(name) {
            Ok(t) => return Ok(t),
            Err(e) if e.sqlstate == sqlstate::UNDEFINED_TABLE => e,
            Err(e) => return Err(e),
        };
        let schema = name.schema().map(|s| s.value.as_str());
        let last = name.name().value.as_str();
        if let Ok(Some((_, RelKind::Index))) = self.catalog.relation_kind(schema, last) {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{last}\""),
            )
            .with_detail("This operation is not supported for indexes.")
            .with_span(name.span));
        }
        if name.parts.len() == 1
            && let Some(ctes) = ctes
            && self.is_future_cte(last, ctes)
        {
            return Err(e
                .with_detail(format!(
                    "There is a WITH item named \"{last}\", but it cannot be referenced from this part of the WITH query."
                ))
                .with_hint("Use WITH RECURSIVE, or re-order the WITH items to remove forward references."));
        }
        Err(e)
    }

    /// 表（名前に CTE があればそれ）の参照 1 つ。
    fn analyze_table_ref(
        &self,
        name: &ObjectName,
        alias: Option<&TableAlias>,
        span: Span,
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
    ) -> Result<(FromItem, Vec<NsItem>)> {
        if name.parts.len() == 1 {
            match self.find_cte(&name.name().value, env.ctes) {
                CteLookup::NotFound => {}
                CteLookup::Recursive => {
                    return Err(super::not_supported("WITH RECURSIVE", name.span));
                }
                CteLookup::Found { levels_up, entry } => {
                    let mut names: Vec<String> =
                        entry.columns.iter().map(|c| c.name.clone()).collect();
                    rename_columns(&mut names, alias)?;
                    let columns: Vec<RteColumn> = entry
                        .columns
                        .iter()
                        .zip(names)
                        .map(|(c, name)| RteColumn { name, ty: c.ty })
                        .collect();
                    let refname = alias.map_or(entry.name.as_str(), |a| a.name.value.as_str());
                    let (rte, mut item) = fb.push_plain(
                        RteKind::CteRef {
                            levels_up,
                            cte: entry.id,
                        },
                        refname,
                        true,
                        columns,
                        alias.map(|_| entry.name.clone()),
                        span,
                    )?;
                    item.origins.clone_from(&entry.origins);
                    return Ok((FromItem::Scan(rte), vec![item]));
                }
            }
        }
        let table = self.resolve_relation(name, Some(env.ctes))?;
        let (rte, item) = fb.table_item(&table, alias, true, span)?;
        Ok((FromItem::Scan(rte), vec![item]))
    }

    /// 派生表 `(SELECT ...) [AS] alias`。本体が単純な `VALUES` なら `RteKind::Values`。
    fn analyze_derived(
        &self,
        query: &Query,
        alias: Option<&TableAlias>,
        span: Span,
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
        lateral: &[NsItem],
    ) -> Result<(FromItem, Vec<NsItem>)> {
        let child = scope_over(env, lateral.to_vec());
        let (kind, mut columns, origins) = if let Some(v) = plain_values(query) {
            let (rows, types) = self.analyze_values_rows(v, &child, env)?;
            let columns: Vec<RteColumn> = types
                .iter()
                .enumerate()
                .map(|(i, ty)| RteColumn {
                    name: format!("column{}", i + 1),
                    ty: *ty,
                })
                .collect();
            (RteKind::Values { rows }, columns, Vec::new())
        } else {
            let qenv = QueryEnv {
                outer: Some(&child),
                ctes: env.ctes,
                resolve_unknowns: true,
            };
            let bq = self.analyze_query(query, &qenv)?;
            let columns = bq
                .columns
                .iter()
                .map(|c| RteColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                })
                .collect();
            let origins = bq.columns.iter().map(|c| (c.table_oid, c.attnum)).collect();
            (
                RteKind::Subquery {
                    query: Box::new(bq),
                },
                columns,
                origins,
            )
        };
        let mut names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        rename_columns(&mut names, alias)?;
        for (c, n) in columns.iter_mut().zip(names) {
            c.name = n;
        }
        let refname = alias.map_or("unnamed_subquery", |a| a.name.value.as_str());
        let (rte, mut item) = fb.push_plain(kind, refname, alias.is_some(), columns, None, span)?;
        item.origins = origins;
        Ok((FromItem::Scan(rte), vec![item]))
    }

    /// FROM 句の `VALUES` の行（各列を共通型にそろえる）。`scope` は式を解析するスコープ。
    fn analyze_values_rows(
        &self,
        v: &Values,
        scope: &ScopeStack,
        env: &QueryEnv<'_>,
    ) -> Result<(Vec<Vec<BoundExpr>>, Vec<SqlType>)> {
        let cx = ExprCtx::new(scope, ParseExprKind::Values).with_ctes(env.ctes);
        let width = v.rows.first().map_or(0, Vec::len);
        let mut cols: Vec<Vec<BoundExpr>> = (0..width)
            .map(|_| Vec::with_capacity(v.rows.len()))
            .collect();
        for row in &v.rows {
            if row.len() != width {
                let span = row.first().map_or(v.span, Expr::span);
                return Err(Error::syntax_at(
                    span,
                    "VALUES lists must all be the same length",
                ));
            }
            for (col, e) in cols.iter_mut().zip(row) {
                col.push(self.transform_expr(e, &cx)?);
            }
        }
        let mut types = Vec::with_capacity(width);
        let mut coerced = Vec::with_capacity(width);
        for col in cols {
            let (c, ty) = self.coerce_all_to_common(col, "VALUES")?;
            types.push(ty);
            coerced.push(c);
        }
        let mut rows: Vec<Vec<BoundExpr>> = (0..v.rows.len())
            .map(|_| Vec::with_capacity(width))
            .collect();
        for col in coerced {
            for (r, e) in col.into_iter().enumerate() {
                rows[r].push(e);
            }
        }
        Ok((rows, types))
    }

    /// FROM 句の関数（`generate_series`）。引数は左の項目を参照できない（暗黙の LATERAL は 0A000）。
    #[allow(clippy::too_many_arguments)]
    fn analyze_from_function(
        &self,
        name: &ObjectName,
        args: &[Expr],
        alias: Option<&TableAlias>,
        span: Span,
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
        lateral: &[NsItem],
    ) -> Result<(FromItem, Vec<NsItem>)> {
        let scope = scope_over(env, lateral.to_vec()).with_lateral_active();
        let cx = ExprCtx::new(&scope, ParseExprKind::FromFunction).with_ctes(env.ctes);
        let bargs = args
            .iter()
            .map(|a| self.transform_expr(a, &cx))
            .collect::<Result<Vec<_>>>()?;
        let fname = name.name().value.as_str();
        if let Some(schema) = name.schema()
            && schema.value != "pg_catalog"
        {
            if !super::resolve::schema_exists(&schema.value) {
                return Err(Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{}\" does not exist", schema.value),
                )
                .with_span(schema.span));
            }
            let shown: Vec<&str> = name.parts.iter().map(|p| p.value.as_str()).collect();
            let types: Vec<String> = bargs
                .iter()
                .map(|b| super::coerce::tname(b.ty.oid))
                .collect();
            return Err(Error::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function {}({}) does not exist",
                    shown.join("."),
                    types.join(", ")
                ),
            )
            .with_span(name.span));
        }
        let call = self.make_func_call_ext(fname, bargs, name.span, true)?;
        let (func_name, column_name) = match &call.kind {
            BoundExprKind::Function { func, .. } => match &func.kind {
                FnKind::Set(set_fn) => (func.name, set_fn.column_name),
                _ => return Err(not_set_function(fname, name.span)),
            },
            _ => return Err(not_set_function(fname, name.span)),
        };
        let refname = alias.map_or(column_name, |a| a.name.value.as_str());
        let mut names = vec![func_name.to_owned()];
        if let Some(a) = alias {
            names[0].clone_from(&a.name.value);
        }
        rename_columns(&mut names, alias)?;
        let columns = vec![RteColumn {
            name: names.remove(0),
            ty: call.ty,
        }];
        let (rte, item) = fb.push_plain(
            RteKind::Function { call },
            refname,
            true,
            columns,
            None,
            span,
        )?;
        Ok((FromItem::Scan(rte), vec![item]))
    }

    /// `left [kind] JOIN right [ON ... | USING (...) | NATURAL]`（`m4/03` §5.3.2）。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn analyze_join(
        &self,
        left: &TableRef,
        right: &TableRef,
        kind: JoinKind,
        constraint: &JoinConstraint,
        span: Span,
        fb: &mut FromBuilder,
        env: &QueryEnv<'_>,
        lateral: &[NsItem],
    ) -> Result<(FromItem, Vec<NsItem>)> {
        let jt = match kind {
            JoinKind::Inner => JoinType::Inner,
            JoinKind::Left => JoinType::Left,
            JoinKind::Right => JoinType::Right,
            JoinKind::Full => JoinType::Full,
            JoinKind::Cross => JoinType::Cross,
        };
        // 1. 左。
        let (l_item, l_ns) = self.analyze_from_item(left, fb, env, lateral)?;
        // 2. 右。右辺の派生表・関数から、左は LATERAL なしでは見えない。
        let left_ok = matches!(jt, JoinType::Inner | JoinType::Left | JoinType::Cross);
        let mut right_lateral = lateral.to_vec();
        right_lateral.extend(lateral_view(&l_ns, left_ok));
        let (r_item, r_ns) = self.analyze_from_item(right, fb, env, &right_lateral)?;
        // 3. 左右の名前の衝突。
        check_ns_conflicts(&l_ns, &r_ns)?;

        let l_rte = item_rte(&l_item);
        let r_rte = item_rte(&r_item);
        let l_cols = fb.rtable[usize::from(l_rte.0)].columns.clone();
        let r_cols = fb.rtable[usize::from(r_rte.0)].columns.clone();
        let l_expr = |i: usize| side_expr(&l_ns, l_rte, i, &l_cols);
        let r_expr = |i: usize| side_expr(&r_ns, r_rte, i, &r_cols);

        // 4. 結合列の決定。
        let using: Vec<Ident> = match constraint {
            JoinConstraint::Using(names) => names.clone(),
            JoinConstraint::Natural => {
                let mut names: Vec<Ident> = Vec::new();
                for lc in &l_cols {
                    if r_cols.iter().any(|rc| rc.name == lc.name)
                        && !names.iter().any(|n| n.value == lc.name)
                    {
                        names.push(Ident {
                            value: lc.name.clone(),
                            quoted: true,
                            span,
                        });
                    }
                }
                names
            }
            JoinConstraint::On(_) | JoinConstraint::None => Vec::new(),
        };

        // 5. USING の各名前: 併合列。
        let mut merged: Vec<(RteColumn, JoinColSource, BoundExpr)> = Vec::new();
        let mut conds: Vec<BoundExpr> = Vec::new();
        let mut l_used: Vec<usize> = Vec::new();
        let mut r_used: Vec<usize> = Vec::new();
        for (k, n) in using.iter().enumerate() {
            if using[..k].iter().any(|p| p.value == n.value) {
                return Err(Error::new(
                    sqlstate::DUPLICATE_COLUMN,
                    format!(
                        "column name \"{}\" appears more than once in USING clause",
                        n.value
                    ),
                ));
            }
            let li = find_join_column(&l_cols, &n.value, "left")?;
            let ri = find_join_column(&r_cols, &n.value, "right")?;
            let (le, re) = (l_expr(li), r_expr(ri));
            let cond = self
                .make_op("=", Some(le.clone()), re.clone(), Span::default())
                .map_err(no_position)?;
            conds.push(cond);
            let (expr, source) = self.merged_join_var(jt, li, ri, le, re)?;
            let col = RteColumn {
                name: n.value.clone(),
                ty: expr.ty,
            };
            merged.push((col, source, expr));
            l_used.push(li);
            r_used.push(ri);
        }

        // 6. ON。
        let on = match constraint {
            JoinConstraint::On(e) => {
                let mut items: Vec<NsItem> = hidden_view(lateral);
                items.extend(l_ns.iter().cloned());
                items.extend(r_ns.iter().cloned());
                let scope = scope_over(env, items);
                let cx = ExprCtx::new(&scope, ParseExprKind::JoinOn).with_ctes(env.ctes);
                let b = self.transform_expr(e, &cx)?;
                Some(self.coerce_to_boolean(b, "JOIN/ON")?)
            }
            _ if conds.is_empty() => None,
            _ => Some(BoundExpr::and_all(conds)),
        };

        // 7. 結合の RTE。列は併合列 → 左の残り → 右の残り。
        let mut columns: Vec<RteColumn> = Vec::new();
        let mut sources: Vec<JoinColSource> = Vec::new();
        let mut exprs: Vec<BoundExpr> = Vec::new();
        for (c, s, e) in merged {
            columns.push(c);
            sources.push(s);
            exprs.push(e);
        }
        for (i, c) in l_cols.iter().enumerate() {
            if !l_used.contains(&i) {
                columns.push(c.clone());
                sources.push(JoinColSource::Left(u16::try_from(i).unwrap_or(u16::MAX)));
                exprs.push(l_expr(i));
            }
        }
        for (j, c) in r_cols.iter().enumerate() {
            if !r_used.contains(&j) {
                columns.push(c.clone());
                sources.push(JoinColSource::Right(u16::try_from(j).unwrap_or(u16::MAX)));
                exprs.push(r_expr(j));
            }
        }
        let rte = fb.next_rte_id()?;
        let rel = ScopeRel {
            rte,
            refname: String::new(),
            hidden_name: None,
            schema: None,
            table_oid: 0,
            columns: scope_columns(&columns),
            system_columns: false,
        };
        fb.rtable.push(Rte {
            kind: RteKind::Join {
                kind: jt,
                left: l_rte,
                right: r_rte,
                sources,
            },
            refname: None,
            columns,
            span,
        });
        let mut join_item = NsItem::new(rel);
        join_item.rel_visible = false;
        join_item.join_exprs = Some(exprs);

        // 8. 外へ出す名前空間: 子の項目は列が見えない（修飾名では見える）。結合自身が列を持つ。
        let mut ns: Vec<NsItem> = l_ns.into_iter().chain(r_ns).collect();
        for i in &mut ns {
            i.cols_visible = false;
        }
        ns.push(join_item);
        Ok((
            FromItem::Join {
                rte,
                kind: jt,
                left: Box::new(l_item),
                right: Box::new(r_item),
                on,
            },
            ns,
        ))
    }

    /// 併合列の式と出どころ（`buildMergedJoinVar`。`m4/03` §3.2.2、11 §7.1 の C-25）。
    fn merged_join_var(
        &self,
        kind: JoinType,
        li: usize,
        ri: usize,
        l: BoundExpr,
        r: BoundExpr,
    ) -> Result<(BoundExpr, JoinColSource)> {
        let (li, ri) = (
            u16::try_from(li).unwrap_or(u16::MAX),
            u16::try_from(ri).unwrap_or(u16::MAX),
        );
        let (lt, rt) = (l.ty, r.ty);
        let out = if lt.oid == rt.oid {
            SqlType::new(
                lt.oid,
                if lt.typmod == rt.typmod {
                    lt.typmod
                } else {
                    -1
                },
            )
        } else {
            let common = self
                .select_common_type(&[&l, &r], Some("JOIN/USING"))?
                .unwrap_or(oid::TEXT);
            SqlType::of(common)
        };
        let span = l.span;
        let l_node = self.coerce_to_common_type(l, out.oid, "JOIN/USING")?;
        let r_node = self.coerce_to_common_type(r, out.oid, "JOIN/USING")?;
        Ok(match kind {
            JoinType::Left => (l_node, JoinColSource::Left(li)),
            JoinType::Right => (r_node, JoinColSource::Right(ri)),
            JoinType::Full => (
                BoundExpr::new(BoundExprKind::Coalesce(vec![l_node, r_node]), out, span),
                JoinColSource::Coalesce(li, ri),
            ),
            JoinType::Inner | JoinType::Cross => {
                if lt == out {
                    (l_node, JoinColSource::Left(li))
                } else if rt == out {
                    (r_node, JoinColSource::Right(ri))
                } else {
                    (l_node, JoinColSource::Left(li))
                }
            }
        })
    }
}

fn not_set_function(name: &str, span: Span) -> Error {
    Error::not_supported(format!("function \"{name}\" in FROM is not supported yet"))
        .with_span(span)
}

/// 式を解析するスコープ: 外側（`env.outer`）に、この SELECT のフレーム（`items`）を足したもの。
fn scope_over(env: &QueryEnv<'_>, items: Vec<NsItem>) -> ScopeStack {
    match env.outer {
        Some(o) => o.with_frame(items),
        None => ScopeStack::single(items),
    }
}

/// USING の名前を片側の列から探す（0 個は `42703`、2 個以上は `42702`。位置なし）。
fn find_join_column(cols: &[RteColumn], name: &str, side: &str) -> Result<usize> {
    let mut hits = cols
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == name)
        .map(|(i, _)| i);
    let Some(first) = hits.next() else {
        return Err(Error::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{name}\" specified in USING clause does not exist in {side} table"),
        ));
    };
    if hits.next().is_some() {
        return Err(Error::new(
            sqlstate::AMBIGUOUS_COLUMN,
            format!("common column name \"{name}\" appears more than once in {side} table"),
        ));
    }
    Ok(first)
}

/// 結合の片側（最上位の RTE `rte` の `i` 番目の列）の式。結合ならその展開式、それ以外は `Var`。
fn side_expr(ns: &[NsItem], rte: RteId, i: usize, cols: &[RteColumn]) -> BoundExpr {
    let item = ns.iter().find(|n| n.rel.rte == rte);
    if let Some(exprs) = item.and_then(|n| n.join_exprs.as_ref())
        && let Some(e) = exprs.get(i)
    {
        return e.clone();
    }
    BoundExpr::column(
        crate::expr::Var::user(rte, u16::try_from(i).unwrap_or(u16::MAX)),
        cols[i].ty,
    )
}
