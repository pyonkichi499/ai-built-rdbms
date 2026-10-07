//! Bound → 論理プラン（`m4/02` §3.5、`m4/04` §3.4・§5）。
//!
//! SELECT の段の積み方（`m4/02` §3.4.1）: 行の生成（JOIN）→ `Filter` → 集約（`Aggregate` + HAVING の
//! `Filter`）→ `Project`（全 targets。resjunk を含む）→ `Sort` → `Distinct`（DISTINCT ON は `Sort` の
//! 後、ALL は resjunk の除去の後）→ `Project`（resjunk の除去）→ `Limit`。
//!
//! 列は `ColId` で参照する（04-D1: パススルーは `ColId` を引き継ぎ、計算した列にだけ新しい ID を発行する）。
//! `Var.levels_up` は rtable を持つスコープ（`BoundSelect`・DML・VALUES の各行）の入れ子の深さで、
//! `Builder.scopes` の積みを上から数える（11 §7.1 の C-3）。CTE は `BoundQuery` ごとの枠
//! （`cte_frames`）で引く（`RteKind::CteRef.levels_up`）。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::logical::{
    ColumnArena, ColumnInfo, JoinKind, LAggCall, LExpr, LSortKey, LogicalCte, LogicalPlan,
    LogicalQuery, LogicalSubquery,
};
use super::physical::{PhysCheck, PhysExpr};
use super::util::{self, ColSet, Volatility};
use super::{MAX_PLAN_DEPTH, PlanEnv};
use crate::analyzer::bound::{
    BoundCheck, BoundCte, BoundDelete, BoundDistinct, BoundExpr, BoundInsert, BoundQuery,
    BoundReturning, BoundSelect, BoundSetExpr, BoundStatement, BoundUpdate, CteMaterialize,
    FromItem, JoinType, OutputColumn, Rte, RteKind, UpdateSource,
};
use crate::catalog::{SystemColumn, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{
    AggCall, AggOrderKey, ColId, CteId, ExprKind, RteId, SYSTEM_COL_BASE, SubLinkKind,
    lower_single_rel, system_col_from_index,
};
use crate::storage::RelHandle;
use crate::types::{SqlType, oid};

/// `Bound` を論理プランにする。
pub fn build_statement(stmt: &BoundStatement, _env: &PlanEnv<'_>) -> Result<LogicalQuery> {
    let mut b = Builder {
        arena: ColumnArena::default(),
        scopes: Vec::new(),
        cte_frames: Vec::new(),
        ctes: Vec::new(),
        n_sublinks: 0,
        depth: 0,
        rel_names: HashSet::new(),
    };
    let built = match stmt {
        BoundStatement::Select(q) => b.build_query(q)?,
        BoundStatement::Insert(i) => b.build_insert(i)?,
        BoundStatement::Update(u) => b.build_update(u)?,
        BoundStatement::Delete(d) => b.build_delete(d)?,
        BoundStatement::Copy(_)
        | BoundStatement::Explain(_)
        | BoundStatement::Ddl(_)
        | BoundStatement::Checkpoint => {
            return Err(Error::internal("utility statements are not planned"));
        }
    };
    Ok(LogicalQuery {
        plan: built.plan,
        arena: b.arena,
        output: built.output,
        columns: built.columns,
        ctes: b.ctes,
        n_subplans_hint: b.n_sublinks,
    })
}

struct Builder {
    arena: ColumnArena,
    /// SELECT のスコープの積み（`Var.levels_up` は「積みの上から数えて何段目か」）。
    scopes: Vec<SelectScope>,
    /// `BoundQuery` ごとの CTE の枠（`RteKind::CteRef.levels_up` は積みの上から数えた段）。
    cte_frames: Vec<CteFrame>,
    /// 共有する CTE（`LogicalQuery.ctes` になる）。
    ctes: Vec<LogicalCte>,
    n_sublinks: usize,
    depth: usize,
    /// 文全体で使った走査の名前（PostgreSQL の `set_rtable_names` と同じく、重複は `name_1` のように改める）。
    rel_names: HashSet<String>,
}

/// 1 つのスコープの `Var` → `LExpr` の対応表。
#[derive(Default)]
struct SelectScope {
    /// `rte_cols[rte.0][i]` = その RTE の i 番目の列を表す式（`Column(ColId)`）。
    rte_cols: Vec<Vec<LExpr>>,
    /// システム列: `Get` が発行した `ColId`。キーは `(RteId, Var.col)`。
    sys_cols: HashMap<(RteId, u16), ColId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CteState {
    Inline,
    /// 共有する。最初の参照で build して `Some(CteId)` にする。
    Shared(Option<CteId>),
    Unreferenced,
}

struct CteBinding {
    cte: Rc<BoundCte>,
    state: CteState,
}

/// `BoundQuery` 1 つにつき 1 枠。`scope_depth` は枠を積んだときの `scopes.len()`（CTE の本体を build する
/// とき `scopes` をここまで切り詰める）。
struct CteFrame {
    scope_depth: usize,
    bindings: Vec<CteBinding>,
}

/// `build_query` の結果。
struct Built {
    /// 根のプラン。出力列は可視列（DML は空）。
    plan: LogicalPlan,
    output: Vec<ColId>,
    columns: Vec<OutputColumn>,
}

/// `build_select` の結果: 全 targets の `Project`（または VALUES）とその列。
struct Projected {
    plan: LogicalPlan,
    /// 各 target の `ColId`（resjunk を含む）。
    cols: Vec<ColId>,
}

/// `Var` が参照したシステム列。
#[derive(Clone, Copy)]
struct SysRef {
    rte: RteId,
    col: u16,
    ty: SqlType,
}

/// 集約の組み立ての途中経過（04 §5.4）。
struct AggState {
    /// 集約の入力（FROM + WHERE）の出力列。
    input_cols: ColSet,
    /// `(出力の ID, 入力側の式)`。従属列が足されることがある。
    group: Vec<(ColId, LExpr)>,
    aggs: Vec<(ColId, LAggCall)>,
}

fn depth_error() -> Error {
    Error::new(
        sqlstate::STATEMENT_TOO_COMPLEX,
        "stack depth limit exceeded",
    )
}

impl Builder {
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        crate::sql::check_stack_depth()?;
        if self.depth > MAX_PLAN_DEPTH {
            return Err(depth_error());
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// 新しい `ColId`（`u32` を超えたら 54000）。
    fn new_col(
        &mut self,
        name: &str,
        qualifier: Option<&str>,
        ty: SqlType,
        origin: Option<(crate::types::Oid, i16)>,
    ) -> Result<ColId> {
        if u32::try_from(self.arena.len()).is_err() {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many columns in query",
            ));
        }
        Ok(self.arena.add(ColumnInfo {
            name: name.to_owned(),
            qualifier: qualifier.map(str::to_owned),
            ty,
            origin,
        }))
    }

    // ----- 問い合わせ ---------------------------------------------------------------------

    fn build_query(&mut self, q: &BoundQuery) -> Result<Built> {
        self.enter()?;
        let r = self.push_cte_frame(q).and_then(|()| {
            let r = self.build_query_inner(q);
            self.cte_frames.pop();
            r
        });
        self.leave();
        r
    }

    fn build_query_inner(&mut self, q: &BoundQuery) -> Result<Built> {
        let (projected, n_visible, distinct) = match &q.body {
            BoundSetExpr::Select(s) => {
                let p = self.build_select(s, &q.columns)?;
                (p, s.n_visible, s.distinct.clone())
            }
            BoundSetExpr::Values { rows, types } => {
                let p = self.build_values_body(rows, types, &q.columns)?;
                (p, types.len(), BoundDistinct::None)
            }
            BoundSetExpr::SetOp { .. } => return self.build_set_op(q),
        };
        let Projected { mut plan, cols } = projected;
        let output: Vec<ColId> = cols[..n_visible].to_vec();

        // 整列・重複除去・resjunk の除去（04 §5.5）。
        let mut keys: Vec<LSortKey> = q
            .order_by
            .iter()
            .map(|k| LSortKey {
                expr: self.col_expr(cols[k.target]),
                descending: k.descending,
                nulls_first: k.nulls_first,
            })
            .collect();
        match &distinct {
            BoundDistinct::None | BoundDistinct::All => {
                let mut post: Option<Vec<(ColId, LExpr)>> = None;
                if !keys.is_empty() {
                    if matches!(distinct, BoundDistinct::None) {
                        let key_cols: HashSet<ColId> =
                            q.order_by.iter().map(|k| cols[k.target]).collect();
                        (plan, post) = self.postpone_volatile(plan, &cols, n_visible, &key_cols);
                    }
                    plan = LogicalPlan::Sort {
                        input: Box::new(plan),
                        keys,
                    };
                }
                if let Some(exprs) = post {
                    // PostgreSQL は整列キーでない揮発性の式を Sort の後に評価する。
                    plan = LogicalPlan::Project {
                        input: Box::new(plan),
                        exprs,
                    };
                } else if cols.len() > n_visible {
                    plan = self.drop_resjunk(plan, &output);
                }
                if matches!(distinct, BoundDistinct::All) {
                    plan = LogicalPlan::Distinct {
                        input: Box::new(plan),
                        on: None,
                    };
                }
            }
            BoundDistinct::On(pos) => {
                // 先頭から ON の式に一致している個数 k を数え、`positions[k..]` を昇順で足す（C-23）。
                let k = q
                    .order_by
                    .iter()
                    .zip(pos)
                    .take_while(|(o, p)| o.target == **p)
                    .count();
                for p in pos.iter().skip(k) {
                    keys.push(LSortKey {
                        expr: self.col_expr(cols[*p]),
                        descending: false,
                        nulls_first: false,
                    });
                }
                plan = LogicalPlan::Sort {
                    input: Box::new(plan),
                    keys,
                };
                plan = LogicalPlan::Distinct {
                    input: Box::new(plan),
                    on: Some(pos.iter().map(|p| self.col_expr(cols[*p])).collect()),
                };
                if cols.len() > n_visible {
                    plan = self.drop_resjunk(plan, &output);
                }
            }
        }

        let plan = self.build_limit(plan, q)?;
        Ok(Built {
            plan,
            output,
            columns: q.columns.clone(),
        })
    }

    /// 整列キーでない揮発性の可視 targets を `Project` から外し、Sort の後の `Project` に回す。
    /// 戻り値は（Sort の下に置くプラン, Sort の上の `Project` の式。無ければ `None`）。
    fn postpone_volatile(
        &self,
        plan: LogicalPlan,
        cols: &[ColId],
        n_visible: usize,
        key_cols: &HashSet<ColId>,
    ) -> (LogicalPlan, Option<Vec<(ColId, LExpr)>>) {
        let LogicalPlan::Project { input, mut exprs } = plan else {
            return (plan, None);
        };
        let postponed: Vec<usize> = (0..n_visible.min(exprs.len()))
            .filter(|&i| {
                !key_cols.contains(&cols[i])
                    && util::expr_volatility(&exprs[i].1) == util::Volatility::Volatile
            })
            .collect();
        if postponed.is_empty() {
            return (LogicalPlan::Project { input, exprs }, None);
        }
        let input_cols: HashSet<ColId> = input.output_cols().into_iter().collect();
        let mut post: Vec<(ColId, LExpr)> = Vec::with_capacity(n_visible);
        let mut moved: HashMap<usize, LExpr> = HashMap::new();
        for &i in &postponed {
            moved.insert(i, exprs[i].1.clone());
        }
        // 後段の式が参照する入力列は、下の Project にパススルーとして足す。
        let mut needed: Vec<ColId> = Vec::new();
        for e in moved.values() {
            for c in util::expr_refs(e) {
                if input_cols.contains(&c) && !needed.contains(&c) {
                    needed.push(c);
                }
            }
        }
        let mut kept: Vec<(ColId, LExpr)> = Vec::new();
        for (i, (id, e)) in exprs.drain(..).enumerate() {
            if moved.contains_key(&i) {
                continue;
            }
            kept.push((id, e));
        }
        for c in needed {
            if !kept.iter().any(|(id, _)| *id == c) {
                kept.push((c, self.col_expr(c)));
            }
        }
        for (i, id) in cols.iter().enumerate().take(n_visible) {
            match moved.remove(&i) {
                Some(e) => post.push((*id, e)),
                None => post.push((*id, self.col_expr(*id))),
            }
        }
        (LogicalPlan::Project { input, exprs: kept }, Some(post))
    }

    /// resjunk の除去（先頭の可視列だけを並べ直すパススルーの `Project`）。
    fn drop_resjunk(&self, plan: LogicalPlan, output: &[ColId]) -> LogicalPlan {
        LogicalPlan::Project {
            input: Box::new(plan),
            exprs: output.iter().map(|c| (*c, self.col_expr(*c))).collect(),
        }
    }

    fn build_limit(&mut self, plan: LogicalPlan, q: &BoundQuery) -> Result<LogicalPlan> {
        if q.limit.is_none() && q.offset.is_none() {
            return Ok(plan);
        }
        let limit = q.limit.as_ref().map(|e| self.lower_limit(e)).transpose()?;
        let offset = q.offset.as_ref().map(|e| self.lower_limit(e)).transpose()?;
        Ok(LogicalPlan::Limit {
            input: Box::new(plan),
            limit,
            offset,
        })
    }

    /// LIMIT / OFFSET の式。自分のスコープの列は見えない（B8）ので、空のスコープを 1 つ積んで下ろす
    /// （`levels_up = 1` が外側の SELECT を指す）。
    fn lower_limit(&mut self, e: &BoundExpr) -> Result<LExpr> {
        self.scopes.push(SelectScope::default());
        let r = self.lower(e);
        self.scopes.pop();
        r
    }

    /// 本体が単独の VALUES の `BoundQuery`。各列は `column1`…（`*VALUES*`）。
    fn build_values_body(
        &mut self,
        rows: &[Vec<BoundExpr>],
        types: &[SqlType],
        columns: &[OutputColumn],
    ) -> Result<Projected> {
        // 各行は rtable が空の 1 スコープ（03 §3.2.1、11 §7.1 の C-3）。
        self.scopes.push(SelectScope::default());
        let lowered: Result<Vec<Vec<LExpr>>> = rows
            .iter()
            .map(|r| r.iter().map(|e| self.lower(e)).collect())
            .collect();
        self.scopes.pop();
        let mut cols = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let name = columns
                .get(i)
                .map_or_else(|| format!("column{}", i + 1), |c| c.name.clone());
            cols.push(self.new_col(&name, Some("*VALUES*"), *ty, None)?);
        }
        Ok(Projected {
            plan: LogicalPlan::Values {
                rows: lowered?,
                cols: cols.clone(),
            },
            cols,
        })
    }

    // ----- SELECT -------------------------------------------------------------------------

    /// FROM から targets まで（`Project` の上までを作る。整列・DISTINCT・LIMIT は `build_query`）。
    fn build_select(&mut self, s: &BoundSelect, columns: &[OutputColumn]) -> Result<Projected> {
        self.enter()?;
        let mut sys = Vec::new();
        s.walk_exprs(0, &mut |e, depth| collect_system(e, depth, &mut sys));

        self.scopes.push(SelectScope {
            rte_cols: vec![Vec::new(); s.rtable.len()],
            sys_cols: HashMap::new(),
        });
        let r = self.build_select_in_scope(s, columns, &sys);
        self.scopes.pop();
        self.leave();
        r
    }

    fn build_select_in_scope(
        &mut self,
        s: &BoundSelect,
        columns: &[OutputColumn],
        sys: &[SysRef],
    ) -> Result<Projected> {
        // 行の生成。
        let mut plan = self.build_from_list(&s.from, &s.rtable, sys)?;

        // 絞り込み。
        if let Some(f) = &s.filter {
            let predicate = self.lower(f)?;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        // 集約。
        let mut agg = None;
        if s.has_agg || !s.group_by.is_empty() || s.having.is_some() {
            let mut st = AggState {
                input_cols: plan.output_cols().into_iter().collect(),
                group: Vec::new(),
                aggs: Vec::new(),
            };
            self.build_group_keys(&s.group_by, &mut st)?;
            agg = Some(st);
        }

        // 出力式（集約があれば集約の後の形）。HAVING も先に下ろす（集約と従属列を集めるため）。
        let mut lowered = Vec::with_capacity(s.targets.len());
        let mut having = None;
        for t in &s.targets {
            lowered.push(match &mut agg {
                Some(st) => self.lower_post_agg(t, st)?,
                None => self.lower(t)?,
            });
        }
        if let (Some(h), Some(st)) = (&s.having, &mut agg) {
            having = Some(self.lower_post_agg(h, st)?);
        }
        if let Some(st) = agg {
            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                group_by: st.group,
                aggs: st.aggs,
            };
            if let Some(predicate) = having {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate,
                };
            }
        }

        // Project。
        let input_cols: HashSet<ColId> = plan.output_cols().into_iter().collect();
        let mut used: HashSet<ColId> = HashSet::new();
        let mut exprs: Vec<(ColId, LExpr)> = Vec::with_capacity(lowered.len());
        for (i, e) in lowered.into_iter().enumerate() {
            let id = match &e.kind {
                // 同じ値を並べ直すだけ: `ColId` を再利用する（パススルー。04-D1）。
                ExprKind::Column(c) if input_cols.contains(c) && used.insert(*c) => *c,
                _ => {
                    let origin = match &e.kind {
                        ExprKind::Column(c) => self.arena.get(*c).origin,
                        _ => None,
                    };
                    let name = if i < s.n_visible {
                        columns.get(i).map(|c| c.name.clone())
                    } else {
                        None
                    };
                    self.new_col(name.as_deref().unwrap_or("?column?"), None, e.ty, origin)?
                }
            };
            exprs.push((id, e));
        }
        let cols = exprs.iter().map(|(c, _)| *c).collect();
        Ok(Projected {
            plan: LogicalPlan::Project {
                input: Box::new(plan),
                exprs,
            },
            cols,
        })
    }

    // ----- FROM ---------------------------------------------------------------------------

    /// カンマ区切りの項目を左深い直積の木にする。FROM なしは `Result`（1 行）。
    fn build_from_list(
        &mut self,
        from: &[FromItem],
        rtable: &[Rte],
        sys: &[SysRef],
    ) -> Result<LogicalPlan> {
        let mut plan: Option<LogicalPlan> = None;
        for item in from {
            let next = self.build_from_item(item, rtable, sys)?;
            plan = Some(match plan {
                None => next,
                Some(left) => cross(left, next),
            });
        }
        Ok(plan.unwrap_or(LogicalPlan::Result {
            one_time_filter: None,
            cols: Vec::new(),
        }))
    }

    fn build_from_item(
        &mut self,
        item: &FromItem,
        rtable: &[Rte],
        sys: &[SysRef],
    ) -> Result<LogicalPlan> {
        self.enter()?;
        let r = self.build_from_item_inner(item, rtable, sys);
        self.leave();
        r
    }

    fn build_from_item_inner(
        &mut self,
        item: &FromItem,
        rtable: &[Rte],
        sys: &[SysRef],
    ) -> Result<LogicalPlan> {
        match item {
            FromItem::Scan(id) => self.build_rte_leaf(*id, rtable, sys),
            FromItem::Join {
                kind,
                left,
                right,
                on,
                ..
            } => {
                let l = self.build_from_item(left, rtable, sys)?;
                let r = self.build_from_item(right, rtable, sys)?;
                let on = on.as_ref().map(|e| self.lower(e)).transpose()?;
                let (kind, l, r, on) = match kind {
                    JoinType::Inner => (JoinKind::Inner, l, r, on),
                    JoinType::Cross => (JoinKind::Inner, l, r, None),
                    JoinType::Left => (JoinKind::Left, l, r, on),
                    // RIGHT は左右を入れ替えて LEFT にする。元の列順に戻す `Project` は作らない（04-D3）。
                    JoinType::Right => (JoinKind::Left, r, l, on),
                    JoinType::Full => (JoinKind::Full, l, r, on),
                };
                Ok(LogicalPlan::Join {
                    kind,
                    left: Box::new(l),
                    right: Box::new(r),
                    on,
                })
            }
        }
    }

    /// 範囲表の葉（`Table` / `Subquery` / `Values` / `Function` / `CteRef`）。
    fn build_rte_leaf(&mut self, id: RteId, rtable: &[Rte], sys: &[SysRef]) -> Result<LogicalPlan> {
        let rte = rtable
            .get(usize::from(id.0))
            .ok_or_else(|| Error::internal(format!("RteId {} is out of range", id.0)))?;
        match &rte.kind {
            RteKind::Table { table } => Ok(self.build_get(id, rte, table, sys)),
            RteKind::Values { rows } => {
                let lowered: Vec<Vec<LExpr>> = rows
                    .iter()
                    .map(|r| r.iter().map(|e| self.lower(e)).collect::<Result<_>>())
                    .collect::<Result<_>>()?;
                let qualifier = rte.refname.clone().unwrap_or_else(|| "*VALUES*".to_owned());
                let cols = self.rte_new_cols(rte, &qualifier)?;
                self.set_rte_cols(id, &cols);
                Ok(LogicalPlan::Values {
                    rows: lowered,
                    cols,
                })
            }
            RteKind::Subquery { query } => {
                // 新しい ID も `Project` も作らず、内側の出力列をそのまま使う（04-D2）。
                let built = self.build_query(query)?;
                if built.output.len() != rte.columns.len() {
                    return Err(Error::internal(
                        "a derived table has a different number of columns than its RTE",
                    ));
                }
                self.set_rte_cols(id, &built.output);
                Ok(built.plan)
            }
            RteKind::Function { call } => {
                let ExprKind::Function { func, args } = &call.kind else {
                    return Err(Error::internal("a function RTE does not hold a call"));
                };
                let args = args.iter().map(|a| self.lower(a)).collect::<Result<_>>()?;
                let qualifier = rte.refname.clone().unwrap_or_else(|| func.name.to_owned());
                let cols = self.rte_new_cols(rte, &qualifier)?;
                self.set_rte_cols(id, &cols);
                Ok(LogicalPlan::FunctionScan {
                    func,
                    args,
                    alias: rte.refname.clone().filter(|n| n.as_str() != func.name),
                    cols,
                })
            }
            RteKind::CteRef { levels_up, cte } => self.build_cte_ref(id, rte, *levels_up, *cte),
            RteKind::Join { .. } => Err(Error::internal("a join RTE is not a scan")),
        }
    }

    /// `rte.columns` の列ごとに新しい `ColId`（`Values` / `FunctionScan` / `CteScan`）。
    fn rte_new_cols(&mut self, rte: &Rte, qualifier: &str) -> Result<Vec<ColId>> {
        rte.columns
            .iter()
            .map(|c| self.new_col(&c.name, Some(qualifier), c.ty, None))
            .collect()
    }

    fn unique_rel_name(&mut self, base: &str) -> String {
        let mut name = base.to_owned();
        let mut n = 0u32;
        while !self.rel_names.insert(name.clone()) {
            n += 1;
            name = format!("{base}_{n}");
        }
        name
    }

    /// `Get`: ユーザー列と、参照されたシステム列（`sys` のうちこの RTE のもの。出現順）。
    fn build_get(
        &mut self,
        id: RteId,
        rte: &Rte,
        table: &Arc<TableDef>,
        sys: &[SysRef],
    ) -> LogicalPlan {
        let qualifier = self.unique_rel_name(rte.refname.as_deref().unwrap_or(&table.name));
        let cols: Vec<ColId> = rte
            .columns
            .iter()
            .zip(&table.columns)
            .map(|(rc, def)| {
                self.arena.add(ColumnInfo {
                    name: rc.name.clone(),
                    qualifier: Some(qualifier.clone()),
                    ty: rc.ty,
                    origin: Some((table.oid, def.attnum)),
                })
            })
            .collect();
        self.set_rte_cols(id, &cols);
        let mut system_columns = Vec::new();
        for r in sys.iter().filter(|r| r.rte == id) {
            let Some(sc) = system_col_from_index(r.col - SYSTEM_COL_BASE) else {
                continue;
            };
            let cid = self.arena.add(ColumnInfo {
                name: system_column_name(sc).to_owned(),
                qualifier: Some(qualifier.clone()),
                ty: r.ty,
                origin: None,
            });
            system_columns.push((sc, cid));
            if let Some(scope) = self.scopes.last_mut() {
                scope.sys_cols.insert((id, r.col), cid);
            }
        }
        LogicalPlan::Get {
            rel: RelHandle::from_table(table),
            table: Arc::clone(table),
            alias: (qualifier != table.name).then_some(qualifier),
            cols,
            system_columns,
        }
    }

    fn set_rte_cols(&mut self, id: RteId, cols: &[ColId]) {
        let exprs: Vec<LExpr> = cols.iter().map(|c| self.col_expr(*c)).collect();
        if let Some(scope) = self.scopes.last_mut() {
            let i = usize::from(id.0);
            if scope.rte_cols.len() <= i {
                scope.rte_cols.resize(i + 1, Vec::new());
            }
            scope.rte_cols[i] = exprs;
        }
    }

    // ----- 集約 ---------------------------------------------------------------------------

    /// GROUP BY の各式を下ろして `st.group` にする（04 §5.4 の 1）。
    fn build_group_keys(&mut self, group_by: &[BoundExpr], st: &mut AggState) -> Result<()> {
        let mut used: HashSet<ColId> = HashSet::new();
        for g in group_by {
            let e = self.lower(g)?;
            if st.group.iter().any(|(_, k)| util::expr_eq(k, &e)) {
                continue;
            }
            let id = match &e.kind {
                ExprKind::Column(c) if st.input_cols.contains(c) && used.insert(*c) => *c,
                _ => {
                    let (name, origin) = match &e.kind {
                        ExprKind::Column(c) => {
                            let i = self.arena.get(*c);
                            (i.name.clone(), i.origin)
                        }
                        _ => ("?column?".to_owned(), None),
                    };
                    let id = self.new_col(&name, None, e.ty, origin)?;
                    used.insert(id);
                    id
                }
            };
            st.group.push((id, e));
        }
        Ok(())
    }

    /// 集約の後の式に書き換える（04 §5.4 の 2）。
    fn lower_post_agg(&mut self, e: &BoundExpr, st: &mut AggState) -> Result<LExpr> {
        e.try_map(&mut |n| {
            self.enter()?;
            let r = self.post_agg_node(n, st);
            self.leave();
            r
        })
    }

    fn post_agg_node(&mut self, n: &BoundExpr, st: &mut AggState) -> Result<Option<LExpr>> {
        match &n.kind {
            ExprKind::Aggregate(call) => {
                let args = call
                    .args
                    .iter()
                    .map(|a| self.lower(a))
                    .collect::<Result<Vec<_>>>()?;
                let filter = call.filter.as_ref().map(|f| self.lower(f)).transpose()?;
                let order_by = call
                    .order_by
                    .iter()
                    .map(|k| {
                        Ok(AggOrderKey {
                            expr: self.lower(&k.expr)?,
                            descending: k.descending,
                            nulls_first: k.nulls_first,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let existing = st.aggs.iter().find(|(_, a)| {
                    a.func.oid == call.func.oid
                        && a.distinct == call.distinct
                        && a.args.len() == args.len()
                        && a.args.iter().zip(&args).all(|(x, y)| util::expr_eq(x, y))
                        && a.order_by.len() == order_by.len()
                        && a.order_by.iter().zip(&order_by).all(|(x, y)| {
                            x.descending == y.descending
                                && x.nulls_first == y.nulls_first
                                && util::expr_eq(&x.expr, &y.expr)
                        })
                        && match (&a.filter, &filter) {
                            (None, None) => true,
                            (Some(x), Some(y)) => util::expr_eq(x, y),
                            _ => false,
                        }
                });
                let id = if let Some((id, _)) = existing {
                    *id
                } else {
                    let id = self.new_col(call.func.name, None, n.ty, None)?;
                    st.aggs.push((
                        id,
                        AggCall {
                            func: call.func,
                            args,
                            distinct: call.distinct,
                            filter,
                            order_by,
                        },
                    ));
                    id
                };
                Ok(Some(LExpr::new(ExprKind::Column(id), n.ty, n.span)))
            }
            ExprKind::SubLink { .. } => {
                let le = self.lower_sublink(n, Some(&mut *st))?;
                // 内側が参照する入力の列は、group key になければ従属列として足す。
                for c in util::expr_refs(&le) {
                    if st.input_cols.contains(&c) && !st.group.iter().any(|(id, _)| *id == c) {
                        st.group.push((c, self.col_expr(c)));
                    }
                }
                Ok(Some(le))
            }
            _ if !n.contains_aggregate() && !n.contains_sublink() => {
                let le = self.lower(n)?;
                if let Some((id, _)) = st.group.iter().find(|(_, k)| util::expr_eq(k, &le)) {
                    return Ok(Some(LExpr::new(ExprKind::Column(*id), le.ty, n.span)));
                }
                if let ExprKind::Column(c) = &le.kind {
                    if st.input_cols.contains(c) && !st.group.iter().any(|(id, _)| id == c) {
                        // 主キーへの関数従属で許された列: group key に足す（結果は変わらない）。
                        st.group.push((*c, le.clone()));
                    }
                    return Ok(Some(le));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    // ----- 式 -----------------------------------------------------------------------------

    /// `Column(c)`（型は台帳から）。
    fn col_expr(&self, c: ColId) -> LExpr {
        LExpr::column(c, self.arena.get(c).ty)
    }

    /// `Var` → `ColId`、`SubLink` → `LogicalSubquery`。`Aggregate` は集約の外では内部エラー。
    fn lower(&mut self, e: &BoundExpr) -> Result<LExpr> {
        if util::expr_depth_exceeds(e, MAX_PLAN_DEPTH) {
            return Err(depth_error());
        }
        e.try_map(&mut |n| match &n.kind {
            ExprKind::Column(v) => Ok(Some(self.resolve_var(*v, n.span)?)),
            ExprKind::Aggregate(_) => Err(Error::internal(
                "an aggregate call outside of a SELECT list or HAVING",
            )),
            ExprKind::SubLink { .. } => self.lower_sublink(n, None).map(Some),
            _ => Ok(None),
        })
    }

    fn resolve_var(&self, v: crate::expr::Var, span: Span) -> Result<LExpr> {
        let depth = usize::from(v.levels_up);
        let scope = self
            .scopes
            .len()
            .checked_sub(depth + 1)
            .and_then(|i| self.scopes.get(i))
            .ok_or_else(|| {
                Error::internal(format!("Var {v:?} refers to a scope that does not exist"))
            })?;
        let mut e = if v.is_system() {
            let c = scope.sys_cols.get(&(v.rte, v.col)).ok_or_else(|| {
                Error::internal(format!("system column {v:?} was not collected by the scan"))
            })?;
            self.col_expr(*c)
        } else {
            scope
                .rte_cols
                .get(usize::from(v.rte.0))
                .and_then(|cols| cols.get(usize::from(v.col)))
                .cloned()
                .ok_or_else(|| Error::internal(format!("Var {v:?} is not a column of its scope")))?
        };
        e.span = span;
        Ok(e)
    }

    /// `SubLink`（04 §5.8）。`agg` が `Some` のとき（集約の後の式の中）、`test` も集約の後の形に直す。
    fn lower_sublink(&mut self, n: &BoundExpr, agg: Option<&mut AggState>) -> Result<LExpr> {
        let ExprKind::SubLink { kind, test, query } = &n.kind else {
            return Err(Error::internal("lower_sublink on a non-SubLink"));
        };
        self.n_sublinks += 1;
        let built = self.build_query(query)?;
        match kind {
            SubLinkKind::Scalar if built.output.len() != 1 => {
                return Err(Error::internal(
                    "a scalar subquery must have exactly one column",
                ));
            }
            SubLinkKind::Exists | SubLinkKind::Scalar | SubLinkKind::Any | SubLinkKind::All => {}
        }
        let test = match (test, agg) {
            (Some(t), Some(st)) => Some(Box::new(self.lower_post_agg(t, st)?)),
            (Some(t), None) => Some(Box::new(self.lower(t)?)),
            (None, _) => None,
        };
        if let Some(t) = &test {
            let n_out = built.output.len();
            let mut bad = false;
            t.walk(&mut |x| {
                if let ExprKind::SubLinkOutput(i) = &x.kind
                    && usize::from(*i) >= n_out
                {
                    bad = true;
                }
                true
            });
            if bad {
                return Err(Error::internal("SubLinkOutput is out of range"));
            }
        }
        Ok(LExpr::new(
            ExprKind::SubLink {
                kind: *kind,
                test,
                query: Box::new(LogicalSubquery {
                    plan: built.plan,
                    output: built.output,
                }),
            },
            n.ty,
            n.span,
        ))
    }

    // ----- 集合演算 -----------------------------------------------------------------------

    fn build_set_op(&mut self, q: &BoundQuery) -> Result<Built> {
        let BoundSetExpr::SetOp {
            op,
            all,
            left,
            right,
            left_coerce,
            right_coerce,
            types,
        } = &q.body
        else {
            return Err(Error::internal("build_set_op on a non-SetOp"));
        };
        let l = self.build_query(left)?;
        let r = self.build_query(right)?;
        let (lplan, lcols) = self.coerce_arm(l, left_coerce.as_deref())?;
        let (rplan, rcols) = self.coerce_arm(r, right_coerce.as_deref())?;
        if lcols.len() != types.len() || rcols.len() != types.len() {
            return Err(Error::internal(
                "a set operation arm has a different number of columns than its types",
            ));
        }
        let mut cols = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let name = q
                .columns
                .get(i)
                .map_or("?column?", |c| c.name.as_str())
                .to_owned();
            cols.push(self.new_col(&name, None, *ty, None)?);
        }
        let mut plan = LogicalPlan::SetOp {
            op: *op,
            all: *all,
            left: Box::new(lplan),
            right: Box::new(rplan),
            cols: cols.clone(),
            left_cols: lcols,
            right_cols: rcols,
        };
        let keys: Vec<LSortKey> = q
            .order_by
            .iter()
            .map(|k| LSortKey {
                expr: self.col_expr(cols[k.target]),
                descending: k.descending,
                nulls_first: k.nulls_first,
            })
            .collect();
        if !keys.is_empty() {
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }
        let plan = self.build_limit(plan, q)?;
        Ok(Built {
            plan,
            output: cols,
            columns: q.columns.clone(),
        })
    }

    /// 腕の出力を共通型に直す（`coerce` が `Some` のとき上に `Project`）。
    fn coerce_arm(
        &mut self,
        arm: Built,
        coerce: Option<&[BoundExpr]>,
    ) -> Result<(LogicalPlan, Vec<ColId>)> {
        let Some(exprs) = coerce else {
            return Ok((arm.plan, arm.output));
        };
        let rte_cols: Vec<LExpr> = arm.output.iter().map(|c| self.col_expr(*c)).collect();
        self.scopes.push(SelectScope {
            rte_cols: vec![rte_cols],
            sys_cols: HashMap::new(),
        });
        let lowered: Result<Vec<LExpr>> = exprs.iter().map(|e| self.lower(e)).collect();
        self.scopes.pop();
        let mut project = Vec::with_capacity(exprs.len());
        for (k, e) in lowered?.into_iter().enumerate() {
            let name = arm
                .columns
                .get(k)
                .map_or("?column?", |c| c.name.as_str())
                .to_owned();
            let id = self.new_col(&name, None, e.ty, None)?;
            project.push((id, e));
        }
        let cols = project.iter().map(|(c, _)| *c).collect();
        Ok((
            LogicalPlan::Project {
                input: Box::new(arm.plan),
                exprs: project,
            },
            cols,
        ))
    }

    // ----- CTE ----------------------------------------------------------------------------

    /// `q` の CTE を数えて枠を積む（`CTE がなくても`積む。`levels_up` の数え方を合わせる）。
    fn push_cte_frame(&mut self, q: &BoundQuery) -> Result<()> {
        let mut bindings = Vec::with_capacity(q.ctes.len());
        for (i, cte) in q.ctes.iter().enumerate() {
            let refs = count_refs_query(q, 0, i);
            let (vol, correlated) = cte_properties(&cte.query);
            let state = decide_cte(cte, refs, vol, correlated)?;
            bindings.push(CteBinding {
                cte: Rc::new(cte.clone()),
                state,
            });
        }
        self.cte_frames.push(CteFrame {
            scope_depth: self.scopes.len(),
            bindings,
        });
        Ok(())
    }

    /// CTE の本体を、宣言した位置の文脈（枠と `scopes`）で build する。
    fn build_cte_body(&mut self, frame: usize, cte: &BoundCte) -> Result<Built> {
        let scope_depth = self.cte_frames[frame].scope_depth;
        let saved_scopes = self.scopes.split_off(scope_depth.min(self.scopes.len()));
        let saved_frames = self.cte_frames.split_off(frame + 1);
        let r = self.build_query(&cte.query);
        self.cte_frames.truncate(frame + 1);
        self.cte_frames.extend(saved_frames);
        self.scopes.truncate(scope_depth);
        self.scopes.extend(saved_scopes);
        r
    }

    fn build_cte_ref(
        &mut self,
        id: RteId,
        rte: &Rte,
        levels_up: u16,
        cte: CteId,
    ) -> Result<LogicalPlan> {
        let frame = self
            .cte_frames
            .len()
            .checked_sub(usize::from(levels_up) + 1)
            .ok_or_else(|| Error::internal("a CTE reference points outside of every query"))?;
        let (bound, state) = {
            let b = self.cte_frames[frame]
                .bindings
                .get(usize::from(cte.0))
                .ok_or_else(|| Error::internal("a CTE reference is out of range"))?;
            (Rc::clone(&b.cte), b.state)
        };
        match state {
            CteState::Inline => {
                let built = self.build_cte_body(frame, &bound)?;
                if built.output.len() != rte.columns.len() {
                    return Err(Error::internal(
                        "a CTE reference has a different number of columns than its query",
                    ));
                }
                self.set_rte_cols(id, &built.output);
                Ok(built.plan)
            }
            CteState::Shared(existing) => {
                #[allow(clippy::single_match_else)]
                let cte_id = match existing {
                    Some(c) => c,
                    None => {
                        let built = self.build_cte_body(frame, &bound)?;
                        let c = CteId(u16::try_from(self.ctes.len()).map_err(|_| {
                            Error::new(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "too many subqueries in query",
                            )
                        })?);
                        self.ctes.push(LogicalCte {
                            name: bound.name.clone(),
                            plan: built.plan,
                            output: built.output,
                            refs: 0,
                            materialize: bound.materialize,
                            inline: false,
                        });
                        self.cte_frames[frame].bindings[usize::from(cte.0)].state =
                            CteState::Shared(Some(c));
                        c
                    }
                };
                let out_len = self.ctes[usize::from(cte_id.0)].output.len();
                if out_len != rte.columns.len() {
                    return Err(Error::internal(
                        "a CTE reference has a different number of columns than its query",
                    ));
                }
                let qualifier = self.unique_rel_name(rte.refname.as_deref().unwrap_or(&bound.name));
                let mut cols = Vec::with_capacity(out_len);
                for (i, c) in rte.columns.iter().enumerate() {
                    let src = self.ctes[usize::from(cte_id.0)].output[i];
                    let ty = self.arena.get(src).ty;
                    cols.push(self.new_col(&c.name, Some(&qualifier), ty, None)?);
                }
                self.ctes[usize::from(cte_id.0)].refs += 1;
                self.set_rte_cols(id, &cols);
                Ok(LogicalPlan::CteScan {
                    cte: cte_id,
                    alias: (qualifier != bound.name).then_some(qualifier),
                    cols,
                })
            }
            CteState::Unreferenced => Err(Error::internal(
                "a reference to a CTE that was counted as unreferenced",
            )),
        }
    }

    // ----- DML ----------------------------------------------------------------------------

    fn build_insert(&mut self, i: &BoundInsert) -> Result<Built> {
        let src = self.build_query(&i.source)?;
        let (input, input_cols) = match &i.coercions {
            None => (src.plan, src.output),
            Some(exprs) => {
                // `Var { rte: 0, col: k }` は挿入元の k 番目の出力列。
                let rte_cols: Vec<LExpr> = src.output.iter().map(|c| self.col_expr(*c)).collect();
                self.scopes.push(SelectScope {
                    rte_cols: vec![rte_cols],
                    sys_cols: HashMap::new(),
                });
                let lowered: Result<Vec<LExpr>> = exprs.iter().map(|e| self.lower(e)).collect();
                self.scopes.pop();
                let input_set: HashSet<ColId> = src.output.iter().copied().collect();
                let mut used = HashSet::new();
                let mut project = Vec::with_capacity(exprs.len());
                for (k, e) in lowered?.into_iter().enumerate() {
                    let id = match &e.kind {
                        ExprKind::Column(c) if input_set.contains(c) && used.insert(*c) => *c,
                        _ => {
                            let name = src
                                .columns
                                .get(k)
                                .map_or_else(|| "?column?".to_owned(), |c| c.name.clone());
                            self.new_col(&name, None, e.ty, None)?
                        }
                    };
                    project.push((id, e));
                }
                let cols = project.iter().map(|(c, _)| *c).collect();
                (
                    LogicalPlan::Project {
                        input: Box::new(src.plan),
                        exprs: project,
                    },
                    cols,
                )
            }
        };
        let (returning, columns) = lower_returning(i.returning.as_ref())?;
        Ok(Built {
            plan: LogicalPlan::Insert {
                table: Arc::clone(&i.table),
                rel: RelHandle::from_table(&i.table),
                input: Box::new(input),
                input_cols,
                column_map: i.column_map.clone(),
                defaults: i
                    .defaults
                    .iter()
                    .map(|d| d.as_ref().map(lower_single_rel).transpose())
                    .collect::<Result<_>>()?,
                checks: lower_checks(&i.checks)?,
                not_null: i.table.columns.iter().map(|c| c.not_null).collect(),
                returning,
            },
            output: Vec::new(),
            columns,
        })
    }

    fn build_update(&mut self, u: &BoundUpdate) -> Result<Built> {
        let mut sys = Vec::new();
        {
            let mut visit = |e: &BoundExpr, d: u16| collect_system(e, d, &mut sys);
            if let Some(e) = &u.filter {
                visit_dml_expr(e, 0, &mut visit);
            }
            for (_, s) in &u.assignments {
                if let UpdateSource::Expr(e) | UpdateSource::Default(Some(e)) = s {
                    visit_dml_expr(e, 0, &mut visit);
                }
            }
            visit_dml_from(&u.rtable, &u.from, &mut visit);
        }
        let dml = self.build_dml_scan(&u.rtable, &u.from, u.filter.as_ref(), sys)?;
        let table = dml.table;

        let r = self.build_update_project(u, &table, &dml.old_cols, dml.ctid);
        self.scopes.pop();
        let (project, new_values) = r?;
        let (returning, columns) = lower_returning(u.returning.as_ref())?;
        Ok(Built {
            plan: LogicalPlan::Update {
                rel: RelHandle::from_table(&table),
                input: Box::new(LogicalPlan::Project {
                    input: Box::new(dml.plan),
                    exprs: project,
                }),
                old_cols: dml.old_cols,
                ctid: dml.ctid,
                new_values,
                checks: lower_checks(&u.checks)?,
                not_null: u.not_null.clone(),
                returning,
                table,
            },
            output: Vec::new(),
            columns,
        })
    }

    #[allow(clippy::type_complexity)]
    fn build_update_project(
        &mut self,
        u: &BoundUpdate,
        table: &Arc<TableDef>,
        old_cols: &[ColId],
        ctid: ColId,
    ) -> Result<(Vec<(ColId, LExpr)>, Vec<(usize, ColId)>)> {
        let mut project: Vec<(ColId, LExpr)> =
            old_cols.iter().map(|c| (*c, self.col_expr(*c))).collect();
        project.push((ctid, self.col_expr(ctid)));
        let mut new_values = Vec::with_capacity(u.assignments.len());
        for (attno, source) in &u.assignments {
            let def = table
                .columns
                .get(*attno)
                .ok_or_else(|| Error::internal("assignment target is out of range"))?;
            let value = match source {
                UpdateSource::Expr(e) | UpdateSource::Default(Some(e)) => self.lower(e)?,
                UpdateSource::Default(None) => LExpr::null_of(def.ty),
            };
            let id = self.new_col(&def.name, None, def.ty, None)?;
            project.push((id, value));
            new_values.push((*attno, id));
        }
        Ok((project, new_values))
    }

    fn build_delete(&mut self, d: &BoundDelete) -> Result<Built> {
        let mut sys = Vec::new();
        {
            let mut visit = |e: &BoundExpr, depth: u16| collect_system(e, depth, &mut sys);
            if let Some(f) = &d.filter {
                visit_dml_expr(f, 0, &mut visit);
            }
            visit_dml_from(&d.rtable, &d.from, &mut visit);
        }
        let dml = self.build_dml_scan(&d.rtable, &d.from, d.filter.as_ref(), sys)?;
        self.scopes.pop();
        let mut project: Vec<(ColId, LExpr)> = dml
            .old_cols
            .iter()
            .map(|c| (*c, self.col_expr(*c)))
            .collect();
        project.push((dml.ctid, self.col_expr(dml.ctid)));
        let (returning, columns) = lower_returning(d.returning.as_ref())?;
        let table = dml.table;
        Ok(Built {
            plan: LogicalPlan::Delete {
                rel: RelHandle::from_table(&table),
                input: Box::new(LogicalPlan::Project {
                    input: Box::new(dml.plan),
                    exprs: project,
                }),
                old_cols: dml.old_cols,
                ctid: dml.ctid,
                returning,
                table,
            },
            output: Vec::new(),
            columns,
        })
    }

    /// UPDATE / DELETE の入力: 対象表の `Get`（`Ctid` を必ず出す）、`FROM` / `USING` の項目との直積、
    /// `Filter`。成功したらスコープは積んだままにする（代入式を下ろす。pop は呼び出し側）。
    fn build_dml_scan(
        &mut self,
        rtable: &[Rte],
        from: &[FromItem],
        filter: Option<&BoundExpr>,
        mut sys: Vec<SysRef>,
    ) -> Result<DmlScan> {
        let Some(RteKind::Table { table }) = rtable.first().map(|r| &r.kind) else {
            return Err(Error::internal(
                "the target of a DML statement is not a table",
            ));
        };
        // `Ctid` を先頭に（DELETE では、他のシステム列が無ければ物理化が `Project` を省ける）。
        sys.retain(|r| !(r.rte == RteId(0) && r.col == SYSTEM_COL_BASE));
        sys.insert(
            0,
            SysRef {
                rte: RteId(0),
                col: SYSTEM_COL_BASE,
                ty: SqlType::of(oid::TID),
            },
        );
        self.scopes.push(SelectScope {
            rte_cols: vec![Vec::new(); rtable.len()],
            sys_cols: HashMap::new(),
        });
        let r = self.build_dml_scan_in_scope(rtable, table, from, filter, &sys);
        if r.is_err() {
            self.scopes.pop();
        }
        r
    }

    fn build_dml_scan_in_scope(
        &mut self,
        rtable: &[Rte],
        table: &Arc<TableDef>,
        from: &[FromItem],
        filter: Option<&BoundExpr>,
        sys: &[SysRef],
    ) -> Result<DmlScan> {
        let mut plan = self.build_get(RteId(0), &rtable[0], table, sys);
        let LogicalPlan::Get {
            cols,
            system_columns,
            ..
        } = &plan
        else {
            return Err(Error::internal("Get expected"));
        };
        let old_cols = cols.clone();
        let ctid = system_columns
            .iter()
            .find(|(sc, _)| *sc == SystemColumn::Ctid)
            .map(|(_, c)| *c)
            .ok_or_else(|| Error::internal("the target scan has no ctid"))?;
        for item in from {
            let next = self.build_from_item(item, rtable, sys)?;
            plan = cross(plan, next);
        }
        if let Some(f) = filter {
            let predicate = self.lower(f)?;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }
        Ok(DmlScan {
            plan,
            table: Arc::clone(table),
            old_cols,
            ctid,
        })
    }
}

/// `build_dml_scan` の結果。
struct DmlScan {
    plan: LogicalPlan,
    table: Arc<TableDef>,
    old_cols: Vec<ColId>,
    ctid: ColId,
}

/// 条件のない内部結合（カンマ区切りの FROM、`UPDATE ... FROM`）。
fn cross(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
    LogicalPlan::Join {
        kind: JoinKind::Inner,
        left: Box::new(left),
        right: Box::new(right),
        on: None,
    }
}

// ----- CTE の判定 -------------------------------------------------------------------------

/// CTE の本体の揮発性（式すべての最大）と、宣言より外のスコープを参照するか（相関）。
fn cte_properties(q: &BoundQuery) -> (Volatility, bool) {
    let mut vol = Volatility::Immutable;
    let mut correlated = false;
    q.walk_exprs(0, &mut |e, depth| {
        vol = vol.max(util::node_volatility(e));
        if let ExprKind::Column(v) = &e.kind
            && v.levels_up > depth
        {
            correlated = true;
        }
    });
    (vol, correlated)
}

/// インラインの判定（04-D4、11 §7.1 の C-22）。
fn decide_cte(cte: &BoundCte, refs: u32, vol: Volatility, correlated: bool) -> Result<CteState> {
    let inline = refs >= 1
        && vol != Volatility::Volatile
        && ((cte.materialize == CteMaterialize::Default && refs == 1)
            || cte.materialize == CteMaterialize::Never);
    Ok(match (refs, inline) {
        (0, _) => CteState::Unreferenced,
        (_, true) => CteState::Inline,
        _ if correlated
            && (cte.materialize == CteMaterialize::Always || vol == Volatility::Volatile) =>
        {
            return Err(Error::not_supported(format!(
                "WITH query \"{}\" that references an outer query level is not supported",
                cte.name
            )));
        }
        _ if correlated => CteState::Inline,
        _ => CteState::Shared(None),
    })
}

/// `q` の CTE `target` への参照を数える。`level` は、いま見ている問い合わせから `q` までの入れ子の深さ。
fn count_refs_query(q: &BoundQuery, level: u16, target: usize) -> u32 {
    let mut n = 0;
    for c in &q.ctes {
        n += count_refs_query(&c.query, level + 1, target);
    }
    match &q.body {
        BoundSetExpr::Select(s) => n += count_refs_select(s, level, target),
        BoundSetExpr::Values { rows, .. } => {
            for e in rows.iter().flatten() {
                n += count_refs_expr(e, level, target);
            }
        }
        BoundSetExpr::SetOp { left, right, .. } => {
            n += count_refs_query(left, level + 1, target)
                + count_refs_query(right, level + 1, target);
        }
    }
    for e in q.limit.iter().chain(&q.offset) {
        n += count_refs_expr(e, level, target);
    }
    n
}

fn count_refs_select(s: &BoundSelect, level: u16, target: usize) -> u32 {
    let mut n = 0;
    for rte in &s.rtable {
        match &rte.kind {
            RteKind::CteRef { levels_up, cte }
                if *levels_up == level && usize::from(cte.0) == target =>
            {
                n += 1;
            }
            RteKind::Subquery { query } => n += count_refs_query(query, level + 1, target),
            RteKind::Values { rows } => {
                for e in rows.iter().flatten() {
                    n += count_refs_expr(e, level, target);
                }
            }
            RteKind::Function { call } => n += count_refs_expr(call, level, target),
            RteKind::Table { .. } | RteKind::Join { .. } | RteKind::CteRef { .. } => {}
        }
    }
    for item in &s.from {
        n += count_refs_from(item, level, target);
    }
    for e in s
        .filter
        .iter()
        .chain(&s.group_by)
        .chain(s.having.iter())
        .chain(&s.targets)
    {
        n += count_refs_expr(e, level, target);
    }
    n
}

fn count_refs_from(item: &FromItem, level: u16, target: usize) -> u32 {
    match item {
        FromItem::Scan(_) => 0,
        FromItem::Join {
            left, right, on, ..
        } => {
            count_refs_from(left, level, target)
                + count_refs_from(right, level, target)
                + on.as_ref().map_or(0, |e| count_refs_expr(e, level, target))
        }
    }
}

fn count_refs_expr(e: &BoundExpr, level: u16, target: usize) -> u32 {
    let mut n = 0;
    e.walk(&mut |x| {
        if let ExprKind::SubLink { query, .. } = &x.kind {
            n += count_refs_query(query, level + 1, target);
        }
        true
    });
    n
}

// ----- システム列の事前走査 ---------------------------------------------------------------

/// 式 `e` と、その中の `SubLink` の問い合わせの式を `f(node, depth)` で訪れる。
fn visit_dml_expr(e: &BoundExpr, depth: u16, f: &mut dyn FnMut(&BoundExpr, u16)) {
    e.walk(&mut |n| {
        f(n, depth);
        if let ExprKind::SubLink { query, .. } = &n.kind {
            query.walk_exprs(depth + 1, f);
        }
        true
    });
}

/// DML の `FROM` / `USING` の項目の式（結合の ON、VALUES の行、関数の引数、導出表の中）を訪れる。
fn visit_dml_from(rtable: &[Rte], from: &[FromItem], f: &mut dyn FnMut(&BoundExpr, u16)) {
    fn items(item: &FromItem, f: &mut dyn FnMut(&BoundExpr, u16)) {
        if let FromItem::Join {
            left, right, on, ..
        } = item
        {
            items(left, f);
            items(right, f);
            if let Some(e) = on {
                visit_dml_expr(e, 0, f);
            }
        }
    }
    for rte in rtable {
        match &rte.kind {
            RteKind::Values { rows } => {
                for e in rows.iter().flatten() {
                    visit_dml_expr(e, 0, f);
                }
            }
            RteKind::Function { call } => visit_dml_expr(call, 0, f),
            RteKind::Subquery { query } => query.walk_exprs(1, f),
            RteKind::Table { .. } | RteKind::Join { .. } | RteKind::CteRef { .. } => {}
        }
    }
    for item in from {
        items(item, f);
    }
}

fn system_column_name(sc: SystemColumn) -> &'static str {
    match sc {
        SystemColumn::Ctid => "ctid",
        SystemColumn::Xmin => "xmin",
        SystemColumn::Cmin => "cmin",
        SystemColumn::Xmax => "xmax",
        SystemColumn::Cmax => "cmax",
        SystemColumn::TableOid => "tableoid",
    }
}

/// `node` が現在のスコープ（`levels_up == depth`）のシステム列なら `out` に足す（初出の順）。
fn collect_system(node: &BoundExpr, depth: u16, out: &mut Vec<SysRef>) {
    if let ExprKind::Column(v) = &node.kind
        && v.is_system()
        && v.levels_up == depth
        && !out.iter().any(|r| r.rte == v.rte && r.col == v.col)
    {
        out.push(SysRef {
            rte: v.rte,
            col: v.col,
            ty: node.ty,
        });
    }
}

fn lower_checks(checks: &[BoundCheck]) -> Result<Vec<PhysCheck>> {
    checks
        .iter()
        .map(|c| {
            Ok(PhysCheck {
                name: c.name.clone(),
                expr: lower_single_rel(&c.expr)?,
            })
        })
        .collect()
}

fn lower_returning(
    r: Option<&BoundReturning>,
) -> Result<(Option<Vec<PhysExpr>>, Vec<OutputColumn>)> {
    match r {
        None => Ok((None, Vec::new())),
        Some(r) => Ok((
            Some(
                r.targets
                    .iter()
                    .map(lower_single_rel)
                    .collect::<Result<_>>()?,
            ),
            r.columns.clone(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::print::print_logical;
    use super::super::rules::testutil::Fixture;

    fn fx() -> Fixture {
        Fixture::new(
            "create table t(a int4, b int4, c text); \
             create table u(a int4, d int4, e text); \
             create table p(id int4 primary key, v int4);",
        )
        .unwrap()
    }

    /// 04 §5.10: build の不変条件（`validate` と、Join の種類・集約式の不在）を代表的な SQL で確かめる。
    #[test]
    fn build_invariants_hold() {
        let f = fx();
        for sql in [
            "select t.c, count(*) from t join u on t.a = u.a where u.d > 1 group by t.c order by 2 desc",
            "select * from t left join u using (a)",
            "select * from t right join u on t.a = u.a",
            "select a from t where exists (select 1 from u where u.a = t.a)",
            "select a, (select max(d) from u where u.a = t.a) from t",
            "with x as (select a from t) select * from x join x y on x.a = y.a",
            "select a from t union select a from u order by 1 limit 3",
            "select distinct on (a, b) a, b, c from t order by a",
            "update t set b = u.d from u where t.a = u.a",
            "select * from (select a, b + 1 as x from t) s where s.x = 3",
            "select a from t group by a having count(*) > 1",
            "select id, v from p group by id",
        ] {
            let q = f
                .build(sql)
                .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            crate::planner::validate::validate_logical(&q, "build")
                .unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            let text = print_logical(&q);
            assert!(
                !text.contains("Join Semi") && !text.contains("Join Anti"),
                "{sql}\n{text}"
            );
            assert!(
                !text.contains("count(*)") || text.contains("Aggregate"),
                "{sql}\n{text}"
            );
        }
    }
}
