//! 論理プラン（`m4/00-contracts.md` §8、`m4/02-pipeline-refactor.md` §3.5）。
//!
//! 列は `ColId`（1 つの文の中で一意）で参照する。各 `ColId` は**ちょうど 1 つのノードが定義する**
//! （同じ値を並べ直すだけのパススルー `(id, Column(id))` は定義ではない。02-D4）。
//! 不変条件 L1〜L10 は [`LogicalQuery::validate`] が検査する。

#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use super::physical::{PhysCheck, PhysExpr};
use crate::analyzer::bound::OutputColumn;
pub use crate::analyzer::bound::{CteMaterialize, SetOpKind};
use crate::catalog::{BuiltinFunction, SystemColumn, TableDef};
use crate::error::{Error, Result};
use crate::expr::{AggCall, ColId, CteId, Expr, ExprKind, SubLinkKind};
use crate::storage::RelHandle;
use crate::types::{Oid, SqlType};

pub type LExpr = Expr<ColId, Box<LogicalSubquery>>;
pub type LAggCall = AggCall<ColId, Box<LogicalSubquery>>;

#[derive(Debug, Clone)]
pub struct LogicalSubquery {
    pub plan: LogicalPlan,
    pub output: Vec<ColId>,
}

impl LogicalSubquery {
    /// 副問い合わせが外側から参照している列（`plan.outer_refs()`）。
    pub fn outer_refs(&self) -> BTreeSet<ColId> {
        self.plan.outer_refs()
    }
}

/// 1 つの文の中で一意な列の台帳。副問い合わせ・CTE とも共有する。
#[derive(Debug, Default, Clone)]
pub struct ColumnArena {
    cols: Vec<ColumnInfo>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    /// 表示用の修飾名（`t`）。EXPLAIN と deparse が使う。
    pub qualifier: Option<String>,
    pub ty: SqlType,
    /// ベーステーブルの列そのものなら `(表の OID, attnum)`。
    pub origin: Option<(Oid, i16)>,
}

impl ColumnArena {
    pub fn add(&mut self, info: ColumnInfo) -> ColId {
        let id = ColId(u32::try_from(self.cols.len()).unwrap_or(u32::MAX));
        self.cols.push(info);
        id
    }

    /// 範囲外の `ColId` は呼び出し側のバグ（`validate` が先に検出する）。
    pub fn get(&self, id: ColId) -> &ColumnInfo {
        &self.cols[id.0 as usize]
    }

    pub fn try_get(&self, id: ColId) -> Option<&ColumnInfo> {
        self.cols.get(id.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.cols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cols.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct LogicalQuery {
    pub plan: LogicalPlan,
    pub arena: ColumnArena,
    /// 出力列（resjunk を除く可視列。`plan` の出力列の先頭）。
    pub output: Vec<ColId>,
    pub columns: Vec<OutputColumn>,
    pub ctes: Vec<LogicalCte>,
    pub n_subplans_hint: usize,
}

#[derive(Debug, Clone)]
pub struct LogicalCte {
    pub name: String,
    pub plan: LogicalPlan,
    pub output: Vec<ColId>,
    pub refs: u32,
    pub materialize: CteMaterialize,
    pub inline: bool,
}

/// RIGHT / CROSS は `build` が Left / Inner に直す。Semi / Anti はサブクエリの書き換えだけが作る。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    Inner,
    Left,
    Full,
    Semi,
    Anti,
}

#[derive(Debug, Clone)]
pub struct LSortKey {
    pub expr: LExpr,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// ベーステーブルの走査。`cols` は attnum 順のユーザー列、`system_columns` は参照された分だけ。
    Get {
        rel: RelHandle,
        table: Arc<TableDef>,
        alias: Option<String>,
        cols: Vec<ColId>,
        system_columns: Vec<(SystemColumn, ColId)>,
    },
    Values {
        rows: Vec<Vec<LExpr>>,
        cols: Vec<ColId>,
    },
    FunctionScan {
        func: &'static BuiltinFunction,
        args: Vec<LExpr>,
        alias: Option<String>,
        cols: Vec<ColId>,
    },
    CteScan {
        cte: CteId,
        alias: Option<String>,
        cols: Vec<ColId>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: LExpr,
    },
    /// 出力 = `exprs` の並び（列の刈り込みもここで表す）。
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(ColId, LExpr)>,
    },
    /// 出力 = left の出力 ++ right の出力（Semi / Anti は left の出力だけ）。`on = None` は直積。
    Join {
        kind: JoinKind,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        on: Option<LExpr>,
    },
    /// 出力 = group_by の列 ++ aggs の列（having は Filter として上に置く）。
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<(ColId, LExpr)>,
        aggs: Vec<(ColId, LAggCall)>,
    },
    /// `on = None`: 全列で重複除去。`Some`: DISTINCT ON（入力は on の式で整列済み）。
    Distinct {
        input: Box<LogicalPlan>,
        on: Option<Vec<LExpr>>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<LSortKey>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<LExpr>,
        offset: Option<LExpr>,
    },
    SetOp {
        op: SetOpKind,
        all: bool,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        cols: Vec<ColId>,
        left_cols: Vec<ColId>,
        right_cols: Vec<ColId>,
    },
    /// FROM なし: 1 行。`one_time_filter` が偽なら 0 行。
    Result {
        one_time_filter: Option<LExpr>,
        cols: Vec<ColId>,
    },
    /// 定数畳み込みで 0 行と分かった部分（置き換えた部分木の出力列と同じ ID を使ってよい）。
    Empty { cols: Vec<ColId> },
    Insert {
        table: Arc<TableDef>,
        rel: RelHandle,
        input: Box<LogicalPlan>,
        input_cols: Vec<ColId>,
        column_map: Vec<Option<usize>>,
        defaults: Vec<Option<PhysExpr>>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        returning: Option<Vec<PhysExpr>>,
    },
    /// `input` の出力: 対象表のユーザー列 ++ ctid ++ 代入する列の新しい値（`new_values` と同じ順）。
    Update {
        table: Arc<TableDef>,
        rel: RelHandle,
        input: Box<LogicalPlan>,
        old_cols: Vec<ColId>,
        ctid: ColId,
        /// `(attnum - 1, 新しい値の ColId)`。
        new_values: Vec<(usize, ColId)>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        returning: Option<Vec<PhysExpr>>,
    },
    Delete {
        table: Arc<TableDef>,
        rel: RelHandle,
        input: Box<LogicalPlan>,
        old_cols: Vec<ColId>,
        ctid: ColId,
        returning: Option<Vec<PhysExpr>>,
    },
}

impl LExpr {
    /// 式が参照する `ColId`（`SubLink` の query が外側から参照する分を含む）。
    pub fn free_cols(&self, out: &mut BTreeSet<ColId>) {
        self.walk(&mut |e| {
            match &e.kind {
                ExprKind::Column(c) => {
                    out.insert(*c);
                }
                ExprKind::SubLink { query, .. } => out.extend(query.outer_refs()),
                _ => {}
            }
            true
        });
    }

    /// `Column(c)` を `map[c]` に置き換える。`SubLink` の query の中の外側参照も置き換える
    /// （`ColId` は文の中で一意なので、query の中の `c` は常に外側の `c` を指し、全体で置換して安全）。
    pub fn substitute(&self, map: &HashMap<ColId, LExpr>) -> Result<LExpr> {
        self.try_rewrite(&mut |e| match &e.kind {
            ExprKind::Column(c) => Ok(map.get(c).cloned()),
            ExprKind::SubLink { kind, test, query } => {
                let mut q = (**query).clone();
                q.plan.substitute_all(map)?;
                let test = test
                    .as_deref()
                    .map(|t| t.substitute(map).map(Box::new))
                    .transpose()?;
                Ok(Some(Expr {
                    kind: ExprKind::SubLink {
                        kind: *kind,
                        test,
                        query: Box::new(q),
                    },
                    ty: e.ty,
                    span: e.span,
                }))
            }
            _ => Ok(None),
        })
    }
}

fn agg_mut(call: &mut LAggCall) -> impl Iterator<Item = &mut LExpr> {
    call.args
        .iter_mut()
        .chain(call.filter.iter_mut())
        .chain(call.order_by.iter_mut().map(|k| &mut k.expr))
}

impl LogicalPlan {
    /// 出力列の並び（`m4/02` §3.5.1。物理プランの出力の並びと 1 対 1 に対応する）。
    pub fn output_cols(&self) -> Vec<ColId> {
        use LogicalPlan as L;
        match self {
            L::Get {
                cols,
                system_columns,
                ..
            } => cols
                .iter()
                .copied()
                .chain(system_columns.iter().map(|(_, c)| *c))
                .collect(),
            L::Values { cols, .. }
            | L::FunctionScan { cols, .. }
            | L::CteScan { cols, .. }
            | L::SetOp { cols, .. }
            | L::Result { cols, .. }
            | L::Empty { cols } => cols.clone(),
            L::Filter { input, .. }
            | L::Distinct { input, .. }
            | L::Sort { input, .. }
            | L::Limit { input, .. } => input.output_cols(),
            L::Project { exprs, .. } => exprs.iter().map(|(c, _)| *c).collect(),
            L::Join {
                kind, left, right, ..
            } => {
                let mut out = left.output_cols();
                if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    out.extend(right.output_cols());
                }
                out
            }
            L::Aggregate { group_by, aggs, .. } => group_by
                .iter()
                .map(|(c, _)| *c)
                .chain(aggs.iter().map(|(c, _)| *c))
                .collect(),
            L::Insert { .. } | L::Update { .. } | L::Delete { .. } => Vec::new(),
        }
    }

    /// このノードが新しく定義する列（`m4/02` §3.5.1 の右の列）。
    pub fn defines(&self) -> Vec<ColId> {
        use LogicalPlan as L;
        match self {
            L::Get {
                cols,
                system_columns,
                ..
            } => cols
                .iter()
                .copied()
                .chain(system_columns.iter().map(|(_, c)| *c))
                .collect(),
            L::Values { cols, .. }
            | L::FunctionScan { cols, .. }
            | L::CteScan { cols, .. }
            | L::SetOp { cols, .. }
            | L::Result { cols, .. }
            | L::Empty { cols } => cols.clone(),
            L::Project { exprs, .. } => exprs
                .iter()
                .filter(|(id, e)| !matches!(&e.kind, ExprKind::Column(c) if c == id))
                .map(|(id, _)| *id)
                .collect(),
            L::Aggregate { group_by, aggs, .. } => group_by
                .iter()
                .filter(|(id, e)| !matches!(&e.kind, ExprKind::Column(c) if c == id))
                .map(|(id, _)| *id)
                .chain(aggs.iter().map(|(id, _)| *id))
                .collect(),
            L::Filter { .. }
            | L::Join { .. }
            | L::Distinct { .. }
            | L::Sort { .. }
            | L::Limit { .. }
            | L::Insert { .. }
            | L::Update { .. }
            | L::Delete { .. } => Vec::new(),
        }
    }

    /// 子のプラン。左から右（`Join` / `SetOp` は left, right）。DML は input。
    pub fn children(&self) -> Vec<&LogicalPlan> {
        use LogicalPlan as L;
        match self {
            L::Get { .. }
            | L::Values { .. }
            | L::FunctionScan { .. }
            | L::CteScan { .. }
            | L::Result { .. }
            | L::Empty { .. } => vec![],
            L::Filter { input, .. }
            | L::Project { input, .. }
            | L::Aggregate { input, .. }
            | L::Distinct { input, .. }
            | L::Sort { input, .. }
            | L::Limit { input, .. }
            | L::Insert { input, .. }
            | L::Update { input, .. }
            | L::Delete { input, .. } => vec![&**input],
            L::Join { left, right, .. } | L::SetOp { left, right, .. } => {
                vec![&**left, &**right]
            }
        }
    }

    pub fn children_mut(&mut self) -> Vec<&mut LogicalPlan> {
        use LogicalPlan as L;
        match self {
            L::Get { .. }
            | L::Values { .. }
            | L::FunctionScan { .. }
            | L::CteScan { .. }
            | L::Result { .. }
            | L::Empty { .. } => vec![],
            L::Filter { input, .. }
            | L::Project { input, .. }
            | L::Aggregate { input, .. }
            | L::Distinct { input, .. }
            | L::Sort { input, .. }
            | L::Limit { input, .. }
            | L::Insert { input, .. }
            | L::Update { input, .. }
            | L::Delete { input, .. } => vec![&mut **input],
            L::Join { left, right, .. } | L::SetOp { left, right, .. } => {
                vec![&mut **left, &mut **right]
            }
        }
    }

    /// このノードが直接持つ式（子のプランは含まない。`SubLink` の中のプランには降りない）。
    /// `Aggregate` は group_by の式、各集約の args と filter。
    pub fn exprs(&self) -> Vec<&LExpr> {
        use LogicalPlan as L;
        match self {
            L::Values { rows, .. } => rows.iter().flatten().collect(),
            L::FunctionScan { args, .. } => args.iter().collect(),
            L::Filter { predicate, .. } => vec![predicate],
            L::Project { exprs, .. } => exprs.iter().map(|(_, e)| e).collect(),
            L::Join { on, .. } => on.iter().collect(),
            L::Aggregate { group_by, aggs, .. } => {
                let mut out: Vec<&LExpr> = group_by.iter().map(|(_, e)| e).collect();
                for (_, a) in aggs {
                    out.extend(a.args.iter());
                    out.extend(a.filter.iter());
                    out.extend(a.order_by.iter().map(|k| &k.expr));
                }
                out
            }
            L::Distinct { on, .. } => on.iter().flatten().collect(),
            L::Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            L::Limit { limit, offset, .. } => limit.iter().chain(offset.iter()).collect(),
            L::Result {
                one_time_filter, ..
            } => one_time_filter.iter().collect(),
            L::Get { .. }
            | L::CteScan { .. }
            | L::SetOp { .. }
            | L::Empty { .. }
            | L::Insert { .. }
            | L::Update { .. }
            | L::Delete { .. } => vec![],
        }
    }

    pub fn exprs_mut(&mut self) -> Vec<&mut LExpr> {
        use LogicalPlan as L;
        match self {
            L::Values { rows, .. } => rows.iter_mut().flatten().collect(),
            L::FunctionScan { args, .. } => args.iter_mut().collect(),
            L::Filter { predicate, .. } => vec![predicate],
            L::Project { exprs, .. } => exprs.iter_mut().map(|(_, e)| e).collect(),
            L::Join { on, .. } => on.iter_mut().collect(),
            L::Aggregate { group_by, aggs, .. } => {
                let mut out: Vec<&mut LExpr> = group_by.iter_mut().map(|(_, e)| e).collect();
                for (_, a) in aggs {
                    out.extend(agg_mut(a));
                }
                out
            }
            L::Distinct { on, .. } => on.iter_mut().flatten().collect(),
            L::Sort { keys, .. } => keys.iter_mut().map(|k| &mut k.expr).collect(),
            L::Limit { limit, offset, .. } => limit.iter_mut().chain(offset.iter_mut()).collect(),
            L::Result {
                one_time_filter, ..
            } => one_time_filter.iter_mut().collect(),
            L::Get { .. }
            | L::CteScan { .. }
            | L::SetOp { .. }
            | L::Empty { .. }
            | L::Insert { .. }
            | L::Update { .. }
            | L::Delete { .. } => vec![],
        }
    }

    /// この木が自分では定義せずに参照している列: 全ノードの式の `Column` と、式の中の `SubLink` の
    /// query の `outer_refs` から、この木の中で定義された `ColId` を引いたもの。
    pub fn outer_refs(&self) -> BTreeSet<ColId> {
        let mut referenced = BTreeSet::new();
        let mut defined = BTreeSet::new();
        self.collect_refs(&mut referenced, &mut defined);
        referenced.difference(&defined).copied().collect()
    }

    fn collect_refs(&self, referenced: &mut BTreeSet<ColId>, defined: &mut BTreeSet<ColId>) {
        for e in self.exprs() {
            e.free_cols(referenced);
        }
        defined.extend(self.defines());
        for c in self.children() {
            c.collect_refs(referenced, defined);
        }
    }

    /// 木の全ノードの式に `substitute` を適用する（`SubLink` の query の中の外側参照を含む）。
    pub fn substitute_all(&mut self, map: &HashMap<ColId, LExpr>) -> Result<()> {
        for e in self.exprs_mut() {
            *e = e.substitute(map)?;
        }
        for c in self.children_mut() {
            c.substitute_all(map)?;
        }
        Ok(())
    }
}

// ----- 検証（L1〜L10）---------------------------------------------------------

fn bad(rule: &str, msg: impl std::fmt::Display) -> Error {
    Error::internal(format!("invalid logical plan [{rule}] {msg}"))
}

fn node_name(p: &LogicalPlan) -> &'static str {
    use LogicalPlan as L;
    match p {
        L::Get { .. } => "Get",
        L::Values { .. } => "Values",
        L::FunctionScan { .. } => "FunctionScan",
        L::CteScan { .. } => "CteScan",
        L::Filter { .. } => "Filter",
        L::Project { .. } => "Project",
        L::Join { .. } => "Join",
        L::Aggregate { .. } => "Aggregate",
        L::Distinct { .. } => "Distinct",
        L::Sort { .. } => "Sort",
        L::Limit { .. } => "Limit",
        L::SetOp { .. } => "SetOp",
        L::Result { .. } => "Result",
        L::Empty { .. } => "Empty",
        L::Insert { .. } => "Insert",
        L::Update { .. } => "Update",
        L::Delete { .. } => "Delete",
    }
}

/// 全プラン（副問い合わせを含む）の `defines` と `CteScan` を集める。
struct Census {
    defined: HashSet<ColId>,
    cte_scans: Vec<(CteId, usize)>,
}

fn census(p: &LogicalPlan, arena: &ColumnArena, c: &mut Census) -> Result<()> {
    for id in p.defines() {
        if id.0 as usize >= arena.len() {
            return Err(bad(
                "L1",
                format!("{} defines ColId {} outside the arena", node_name(p), id.0),
            ));
        }
        if !c.defined.insert(id) {
            return Err(bad(
                "L1",
                format!(
                    "ColId {} is defined twice (second in {})",
                    id.0,
                    node_name(p)
                ),
            ));
        }
    }
    if let LogicalPlan::CteScan { cte, cols, .. } = p {
        c.cte_scans.push((*cte, cols.len()));
    }
    for e in p.exprs() {
        let mut err = None;
        e.walk(&mut |n| {
            if err.is_none()
                && let ExprKind::SubLink { query, .. } = &n.kind
            {
                err = census(&query.plan, arena, c).err();
            }
            err.is_none()
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    for ch in p.children() {
        census(ch, arena, c)?;
    }
    Ok(())
}

impl LogicalPlan {
    /// `outer`: このプランが外側から参照してよい列（`LogicalSubquery` の中では `SubLink` が置かれた
    /// スコープの列。根では空）。L2〜L8 をこの木に対して検査する（L1・L9・L10 は
    /// [`LogicalQuery::validate`]）。
    pub fn validate(&self, arena: &ColumnArena, outer: &BTreeSet<ColId>) -> Result<()> {
        self.validate_node(arena, outer, true)
    }

    fn validate_node(
        &self,
        arena: &ColumnArena,
        outer: &BTreeSet<ColId>,
        is_root: bool,
    ) -> Result<()> {
        use LogicalPlan as L;
        let name = node_name(self);
        if !is_root && matches!(self, L::Insert { .. } | L::Update { .. } | L::Delete { .. }) {
            return Err(bad("L8", format!("{name} below the root")));
        }
        let out_cols = self.output_cols();
        let mut seen = HashSet::new();
        if let Some(d) = out_cols.iter().find(|c| !seen.insert(**c)) {
            return Err(bad("L3", format!("{name} outputs ColId {} twice", d.0)));
        }
        // 式が見てよい列。
        let mut allowed: BTreeSet<ColId> = outer.clone();
        match self {
            L::Join { left, right, .. } => {
                allowed.extend(left.output_cols());
                allowed.extend(right.output_cols());
            }
            L::Filter { input, .. }
            | L::Project { input, .. }
            | L::Aggregate { input, .. }
            | L::Distinct { input, .. }
            | L::Sort { input, .. } => allowed.extend(input.output_cols()),
            _ => {}
        }
        for e in self.exprs() {
            check_lexpr(e, arena, &allowed, None, name)?;
        }
        match self {
            L::Get {
                table,
                cols,
                system_columns,
                ..
            } => {
                if cols.len() != table.columns.len() {
                    return Err(bad(
                        "L3",
                        format!("Get has {} columns for table {}", cols.len(), table.name),
                    ));
                }
                for (c, def) in cols.iter().zip(&table.columns) {
                    if arena.try_get(*c).map(|i| i.ty) != Some(def.ty) {
                        return Err(bad(
                            "L3",
                            format!("Get column {} type differs from {}", c.0, def.name),
                        ));
                    }
                }
                let _ = system_columns;
            }
            L::Values { rows, cols } => {
                if rows.iter().any(|r| r.len() != cols.len()) {
                    return Err(bad("L3", "Values row width differs from cols"));
                }
            }
            L::SetOp {
                cols,
                left_cols,
                right_cols,
                ..
            } => {
                if left_cols.len() != cols.len() || right_cols.len() != cols.len() {
                    return Err(bad("L6", "SetOp column lists have different lengths"));
                }
                for (i, c) in cols.iter().enumerate() {
                    let t = |id: &ColId| arena.try_get(*id).map(|x| x.ty);
                    if t(c) != t(&left_cols[i]) || t(c) != t(&right_cols[i]) {
                        return Err(bad("L6", format!("SetOp column {i} types differ")));
                    }
                }
            }
            L::Update {
                input,
                old_cols,
                ctid,
                new_values,
                ..
            } => {
                let mut want = old_cols.clone();
                want.push(*ctid);
                want.extend(new_values.iter().map(|(_, c)| *c));
                if input.output_cols() != want {
                    return Err(bad(
                        "L8",
                        "Update input output differs from old_cols ++ [ctid] ++ new_values",
                    ));
                }
            }
            L::Delete {
                input,
                old_cols,
                ctid,
                ..
            } => {
                let mut want = old_cols.clone();
                want.push(*ctid);
                if input.output_cols() != want {
                    return Err(bad(
                        "L8",
                        "Delete input output differs from old_cols ++ [ctid]",
                    ));
                }
            }
            _ => {}
        }
        // 副問い合わせ: SubLink が置かれたノードの列が外側になる。
        for e in self.exprs() {
            let mut err = None;
            e.walk(&mut |n| {
                if err.is_none()
                    && let ExprKind::SubLink { query, .. } = &n.kind
                {
                    err = query.plan.validate_node(arena, &allowed, false).err();
                }
                err.is_none()
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        for c in self.children() {
            c.validate_node(arena, outer, false)?;
        }
        Ok(())
    }
}

/// L2・L4・L5: 式の列の出所、`Aggregate` の不在、`SubLink` / `SubLinkOutput` の規則。
fn check_lexpr(
    e: &LExpr,
    arena: &ColumnArena,
    allowed: &BTreeSet<ColId>,
    test_cols: Option<&[ColId]>,
    node: &str,
) -> Result<()> {
    match &e.kind {
        ExprKind::Column(c) => {
            if c.0 as usize >= arena.len() {
                return Err(bad(
                    "L2",
                    format!("{node}: ColId {} outside the arena", c.0),
                ));
            }
            if !allowed.contains(c) {
                return Err(bad(
                    "L2",
                    format!(
                        "{node}: ColId {} is not an output of its input or an outer column",
                        c.0
                    ),
                ));
            }
            Ok(())
        }
        ExprKind::Aggregate(_) => Err(bad("L4", format!("{node}: Aggregate inside an LExpr"))),
        ExprKind::SubLinkOutput(i) => match test_cols {
            Some(out) if usize::from(*i) < out.len() => Ok(()),
            _ => Err(bad(
                "L5",
                format!("{node}: SubLinkOutput({i}) outside a test or out of range"),
            )),
        },
        ExprKind::SubLink { kind, test, query } => {
            let needs = matches!(kind, SubLinkKind::Any | SubLinkKind::All);
            if needs != test.is_some() {
                return Err(bad(
                    "L5",
                    format!("{node}: SubLink {kind:?} with test = {}", test.is_some()),
                ));
            }
            let plan_out = query.plan.output_cols();
            if let Some(c) = query.output.iter().find(|c| !plan_out.contains(c)) {
                return Err(bad(
                    "L5",
                    format!(
                        "{node}: subquery output ColId {} is not produced by its plan",
                        c.0
                    ),
                ));
            }
            if let Some(t) = test {
                check_lexpr(t, arena, allowed, Some(&query.output), node)?;
            }
            Ok(())
        }
        _ => {
            for c in e.children() {
                check_lexpr(c, arena, allowed, test_cols, node)?;
            }
            Ok(())
        }
    }
}

impl LogicalQuery {
    /// L1〜L10。違反は `Error::internal("invalid logical plan [L2] ...")`（XX000）。純粋関数。
    pub fn validate(&self) -> Result<()> {
        let mut c = Census {
            defined: HashSet::new(),
            cte_scans: Vec::new(),
        };
        census(&self.plan, &self.arena, &mut c)?;
        for cte in &self.ctes {
            census(&cte.plan, &self.arena, &mut c)?;
        }
        let empty = BTreeSet::new();
        self.plan.validate(&self.arena, &empty)?;
        for cte in &self.ctes {
            cte.plan.validate_node(&self.arena, &empty, false)?;
            let out = cte.plan.output_cols();
            if let Some(x) = cte.output.iter().find(|x| !out.contains(x)) {
                return Err(bad(
                    "L9",
                    format!(
                        "CTE {} output ColId {} is not produced by its plan",
                        cte.name, x.0
                    ),
                ));
            }
        }
        let is_dml = matches!(
            self.plan,
            LogicalPlan::Insert { .. } | LogicalPlan::Update { .. } | LogicalPlan::Delete { .. }
        );
        if is_dml {
            if !self.output.is_empty() {
                return Err(bad("L9", "a DML root must have an empty output"));
            }
        } else {
            let out = self.plan.output_cols();
            if self.output.len() > out.len() || self.output[..] != out[..self.output.len()] {
                return Err(bad(
                    "L9",
                    "output is not a prefix of the plan's output columns",
                ));
            }
        }
        // L10
        let mut counts = vec![0u32; self.ctes.len()];
        for (cte, ncols) in &c.cte_scans {
            let i = usize::from(cte.0);
            let Some(def) = self.ctes.get(i) else {
                return Err(bad("L10", format!("CteScan {i} out of range")));
            };
            if def.inline {
                return Err(bad(
                    "L10",
                    format!("CteScan refers to the inlined CTE {}", def.name),
                ));
            }
            if *ncols != def.output.len() {
                return Err(bad(
                    "L10",
                    format!(
                        "CteScan of {} has {ncols} columns, the CTE outputs {}",
                        def.name,
                        def.output.len()
                    ),
                ));
            }
            counts[i] += 1;
        }
        for (def, n) in self.ctes.iter().zip(counts) {
            if !def.inline && def.refs != n {
                return Err(bad(
                    "L10",
                    format!("CTE {} has refs = {} but {n} CteScans", def.name, def.refs),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::error::Span;
    use crate::types::Datum;

    fn table(ncols: i16) -> Arc<TableDef> {
        let cols = (1..=ncols)
            .map(|i| ColumnDef {
                name: format!("c{i}"),
                attnum: i,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            })
            .collect();
        Arc::new(table_def(16384, "t", cols, vec![]))
    }

    fn newcol(arena: &mut ColumnArena, name: &str) -> ColId {
        arena.add(ColumnInfo {
            name: name.into(),
            qualifier: None,
            ty: SqlType::INT4,
            origin: None,
        })
    }

    fn get(arena: &mut ColumnArena, n: i16) -> (LogicalPlan, Vec<ColId>) {
        let t = table(n);
        let cols: Vec<ColId> = (0..n).map(|i| newcol(arena, &format!("c{i}"))).collect();
        let plan = LogicalPlan::Get {
            rel: RelHandle::from_table(&t),
            table: t,
            alias: None,
            cols: cols.clone(),
            system_columns: vec![],
        };
        (plan, cols)
    }

    fn c(id: ColId) -> LExpr {
        Expr::column(id, SqlType::INT4)
    }

    fn eq(a: LExpr, b: LExpr) -> LExpr {
        let op = crate::catalog::builtin::operators_named("=")[0];
        Expr::new(
            ExprKind::Operator {
                op,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn exists(plan: LogicalPlan) -> LExpr {
        Expr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Exists,
                test: None,
                query: Box::new(LogicalSubquery {
                    plan,
                    output: vec![],
                }),
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn filter(input: LogicalPlan, predicate: LExpr) -> LogicalPlan {
        LogicalPlan::Filter {
            input: Box::new(input),
            predicate,
        }
    }

    fn query(arena: ColumnArena, plan: LogicalPlan) -> LogicalQuery {
        let output = plan.output_cols();
        LogicalQuery {
            plan,
            arena,
            output,
            columns: vec![],
            ctes: vec![],
            n_subplans_hint: 0,
        }
    }

    fn msg(r: Result<()>) -> String {
        let e = r.expect_err("expected a validation failure");
        assert_eq!(e.sqlstate.code(), "XX000");
        e.message
    }

    #[test]
    fn output_cols_and_defines_per_node() {
        let mut a = ColumnArena::default();
        let (l, lc) = get(&mut a, 2);
        let (r, rc) = get(&mut a, 1);
        assert_eq!(l.output_cols(), lc);
        assert_eq!(l.defines(), lc);
        let join = |kind| LogicalPlan::Join {
            kind,
            left: Box::new(l.clone()),
            right: Box::new(r.clone()),
            on: None,
        };
        assert_eq!(
            join(JoinKind::Inner).output_cols(),
            vec![lc[0], lc[1], rc[0]]
        );
        assert_eq!(join(JoinKind::Semi).output_cols(), lc);
        assert_eq!(join(JoinKind::Anti).output_cols(), lc);
        assert!(join(JoinKind::Left).defines().is_empty());
        // Project: パススルーは定義でなく、新しい式の列は定義。
        let computed = newcol(&mut a, "x");
        let p = LogicalPlan::Project {
            input: Box::new(filter(l.clone(), eq(c(lc[0]), c(lc[1])))),
            exprs: vec![(lc[1], c(lc[1])), (computed, eq(c(lc[0]), c(lc[0])))],
        };
        assert_eq!(p.output_cols(), vec![lc[1], computed]);
        assert_eq!(p.defines(), vec![computed]);
        // Aggregate: グループキーが Var そのものなら再利用（定義しない）。集約の結果は常に新しい列。
        let agg_col = newcol(&mut a, "count");
        let agg = LogicalPlan::Aggregate {
            input: Box::new(l.clone()),
            group_by: vec![(lc[0], c(lc[0]))],
            aggs: vec![(
                agg_col,
                AggCall {
                    func: &COUNT,
                    args: vec![],
                    distinct: false,
                    filter: None,
                    order_by: Vec::new(),
                },
            )],
        };
        assert_eq!(agg.output_cols(), vec![lc[0], agg_col]);
        assert_eq!(agg.defines(), vec![agg_col]);
        let dml = LogicalPlan::Delete {
            table: table(2),
            rel: RelHandle::from_table(&table(2)),
            input: Box::new(l.clone()),
            old_cols: vec![lc[0]],
            ctid: lc[1],
            returning: None,
        };
        assert!(dml.output_cols().is_empty() && dml.defines().is_empty());
        assert_eq!(dml.children().len(), 1);
        assert_eq!(join(JoinKind::Inner).children().len(), 2);
        assert!(l.children().is_empty());
        assert_eq!(a.len(), 5);
        assert_eq!(a.get(lc[0]).name, "c0");
        assert!(!a.is_empty());
    }

    static COUNT: crate::catalog::BuiltinAggregate = crate::catalog::BuiltinAggregate {
        oid: 2803,
        name: "count",
        args: &[],
        result: crate::types::oid::INT8,
        kind: crate::catalog::AggKind::CountStar,
    };

    #[test]
    fn outer_refs_of_nested_subqueries() {
        let mut a = ColumnArena::default();
        let (outer_get, oc) = get(&mut a, 1);
        let (mid_get, mc) = get(&mut a, 1);
        let (inner_get, ic) = get(&mut a, 1);
        // 最も内側: inner.c = mid.c（mid の列は外側参照）。
        let inner = filter(inner_get, eq(c(ic[0]), c(mc[0])));
        // 中間: EXISTS(inner) AND mid.c = outer.c。
        let mid_pred = Expr::and_all(vec![exists(inner), eq(c(mc[0]), c(oc[0]))]);
        let mid = filter(mid_get, mid_pred);
        // 中間のプランが自分では定義しない列は outer.c だけ（mid.c は自分の Get が定義する）。
        assert_eq!(mid.outer_refs(), BTreeSet::from([oc[0]]));
        let top = filter(outer_get, exists(mid));
        assert!(top.outer_refs().is_empty());
        // free_cols は SubLink の外側参照を含む。
        let e = exists(filter(get(&mut a, 1).0, eq(c(oc[0]), c(oc[0]))));
        let mut set = BTreeSet::new();
        e.free_cols(&mut set);
        assert_eq!(set, BTreeSet::from([oc[0]]));
    }

    #[test]
    fn substitute_reaches_into_subqueries() {
        let mut a = ColumnArena::default();
        let (outer_get, oc) = get(&mut a, 1);
        let (inner_get, ic) = get(&mut a, 1);
        let e = Expr::and_all(vec![
            eq(c(oc[0]), c(oc[0])),
            exists(filter(inner_get, eq(c(ic[0]), c(oc[0])))),
        ]);
        let replacement = newcol(&mut a, "r");
        let map = HashMap::from([(oc[0], c(replacement))]);
        let out = e.substitute(&map).unwrap();
        let mut set = BTreeSet::new();
        out.free_cols(&mut set);
        // ic は副問い合わせの中で定義される列なので自由列ではない。
        assert_eq!(set, BTreeSet::from([replacement]));
        assert!(!set.contains(&oc[0]));
        let _ = outer_get;
        // 元の式は変わらない。
        let mut before = BTreeSet::new();
        e.free_cols(&mut before);
        assert!(before.contains(&oc[0]));
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn valid_plan_passes_and_validate_rejects_each_rule() {
        let mut a = ColumnArena::default();
        let (g, gc) = get(&mut a, 2);
        let good = query(a.clone(), filter(g.clone(), eq(c(gc[0]), c(gc[1]))));
        good.validate().unwrap();

        // L1: 同じ ColId を 2 つの Get が定義 / 台帳の範囲外。
        let dup = LogicalPlan::Join {
            kind: JoinKind::Inner,
            left: Box::new(g.clone()),
            right: Box::new(g.clone()),
            on: None,
        };
        assert!(msg(query(a.clone(), dup).validate()).contains("[L1]"));
        let (g2, _) = get(&mut ColumnArena::default(), 2);
        assert!(msg(query(ColumnArena::default(), g2).validate()).contains("[L1]"));

        // L2: Filter が子にない ColId を参照。
        let mut a3 = a.clone();
        let stray = newcol(&mut a3, "stray");
        assert!(
            msg(query(a3.clone(), filter(g.clone(), eq(c(stray), c(gc[0])))).validate())
                .contains("[L2]")
        );
        // L3: Project の出力に同じ ID が 2 回 / Values の行の幅。
        let dup_out = LogicalPlan::Project {
            input: Box::new(g.clone()),
            exprs: vec![(gc[0], c(gc[0])), (gc[0], c(gc[0]))],
        };
        assert!(msg(query(a.clone(), dup_out).validate()).contains("[L3]"));
        let values = LogicalPlan::Values {
            rows: vec![vec![Expr::literal(Datum::Int4(1), SqlType::INT4); 2]],
            cols: vec![gc[0]],
        };
        let mut a4 = ColumnArena::default();
        newcol(&mut a4, "v");
        assert!(msg(query(a4, values).validate()).contains("[L3]"));
        // L4: LExpr に Aggregate。
        let agg_expr: LExpr = Expr::new(
            ExprKind::Aggregate(Box::new(AggCall {
                func: &COUNT,
                args: vec![],
                distinct: false,
                filter: None,
                order_by: Vec::new(),
            })),
            SqlType::INT8,
            Span::default(),
        );
        assert!(msg(query(a.clone(), filter(g.clone(), agg_expr)).validate()).contains("[L4]"));
        // L5: SubLinkOutput が test の外 / Any に test がない。
        let stray_out: LExpr =
            Expr::new(ExprKind::SubLinkOutput(0), SqlType::INT4, Span::default());
        assert!(msg(query(a.clone(), filter(g.clone(), stray_out)).validate()).contains("[L5]"));
        let mut a5 = a.clone();
        let (sub_get, _) = get(&mut a5, 1);
        let any_no_test: LExpr = Expr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Any,
                test: None,
                query: Box::new(LogicalSubquery {
                    plan: sub_get,
                    output: vec![],
                }),
            },
            SqlType::BOOL,
            Span::default(),
        );
        assert!(msg(query(a5, filter(g.clone(), any_no_test)).validate()).contains("[L5]"));
        // L6: SetOp の列リストの長さ。
        let mut a6 = ColumnArena::default();
        let (l, lc) = get(&mut a6, 1);
        let (r, rc) = get(&mut a6, 1);
        let out = newcol(&mut a6, "o");
        let bad_setop = LogicalPlan::SetOp {
            op: SetOpKind::Union,
            all: true,
            left: Box::new(l),
            right: Box::new(r),
            cols: vec![out],
            left_cols: lc.clone(),
            right_cols: vec![rc[0], rc[0]],
        };
        assert!(msg(query(a6, bad_setop).validate()).contains("[L6]"));
        // L8: Delete の input が old_cols ++ [ctid] と違う / 根でない DML。
        let rel = RelHandle::from_table(&table(2));
        let del = |input: LogicalPlan| LogicalPlan::Delete {
            table: table(2),
            rel: rel.clone(),
            input: Box::new(input),
            old_cols: vec![gc[0]],
            ctid: gc[1],
            returning: None,
        };
        query(a.clone(), del(g.clone())).validate().unwrap();
        let wrong = LogicalPlan::Project {
            input: Box::new(g.clone()),
            exprs: vec![(gc[0], c(gc[0]))],
        };
        assert!(msg(query(a.clone(), del(wrong)).validate()).contains("[L8]"));
        assert!(
            msg(query(a.clone(), filter(del(g.clone()), Expr::bool_lit(true))).validate())
                .contains("[L8]")
        );
        // L9: output が plan の出力の先頭でない。
        let mut bad_out = query(a.clone(), g.clone());
        bad_out.output = vec![gc[1]];
        assert!(msg(bad_out.validate()).contains("[L9]"));
        // L10: CteScan の範囲外 / 列数 / refs。
        let cte_scan = |cols: Vec<ColId>| LogicalPlan::CteScan {
            cte: CteId(0),
            alias: None,
            cols,
        };
        let mut a7 = ColumnArena::default();
        let x = newcol(&mut a7, "x");
        assert!(msg(query(a7.clone(), cte_scan(vec![x])).validate()).contains("[L10]"));
        let mut a8 = ColumnArena::default();
        let (cp, cpc) = get(&mut a8, 1);
        let y = newcol(&mut a8, "y");
        let mut q = query(a8, cte_scan(vec![y]));
        q.ctes.push(LogicalCte {
            name: "w".into(),
            plan: cp,
            output: cpc,
            refs: 1,
            materialize: CteMaterialize::Default,
            inline: false,
        });
        q.validate().unwrap();
        q.ctes[0].refs = 2;
        assert!(msg(q.validate()).contains("[L10]"));
    }
}
