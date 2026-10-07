//! EXPLAIN の木の構築（`m4/04` §3.8・§8。持ち主は L2。整形は `explain/`、式の文字列化は `deparse/`）。
//!
//! 2 段で作る。
//!
//! 1. **メモ（[`NodeNote`]）**: `physicalize` が物理ノードを 1 つ作るたびに、そのノードの表示用の文字列（タイトル・
//!    詳細行・`Output:`・式の中の SubPlan）を、木（根・各 CTE・各 SubPlan）ごとの `Vec<NodeNote>` に**子が先の後順**
//!    で足す。`Project` の省略など、物理ノードを作らなかったときは足さない。
//! 2. **組み立て（[`assemble`]）**: 物理プランが完成した後で物理プランを先行順にたどり、後順の添字でメモを引いて
//!    [`ExplainNode`] の木にする。ここで `Filter` の併合、`Project` の透過、合成ノード（`Hash`、`HashSetOp` の下の
//!    `Append` と `Subquery Scan`）、`InitPlan` / `SubPlan` / CTE の子の付け方（C-21）を行う。
//!
//! 式の文字列化は [`ExplainNames`] が持つ。論理の列（`ColId`）の表示は、列を定義するノード（`Project`・
//! `Aggregate`）が登録した定義の式を**使う場所の規則で**文字列にする（PostgreSQL が上位ノードで子の計算列を
//! 展開し直すのと同じ。修飾の有無は使う行によって変わる。10 §3.5・§3.8）。

use std::collections::{BTreeSet, HashMap};

use super::logical::{ColumnArena, LogicalPlan};
use super::physical::{
    ExplainChild, ExplainDetail, ExplainNode, FilterCounter, PhysExpr, PhysicalPlan, PhysicalQuery,
    RemovedRows, SubPlanDef, SubPlanStrategy, assign_exec_ids,
};
use crate::catalog::CatalogReader;
use crate::deparse::ident::quote_identifier;
use crate::deparse::{
    ColText, ColumnNamer, DeparseCtx, DeparseOptions, SubLinkRenderer, SubPlanLabel, deparse_expr,
};
use crate::error::{Error, Result};
use crate::explain::node::{QualifyRule, row_width, use_prefix};
use crate::expr::{ColId, Expr, ExprKind, ParamId, PhysCol, SubPlanId};
use crate::types::TypeEnv;

/// 表示用の式の列参照。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ECol {
    Col(ColId),
    Param(ParamId),
}

/// 表示用の式（`ColId` / `ParamId` の列参照と `SubPlanId` の副問い合わせ）。
pub type DExpr = Expr<ECol, SubPlanId>;

/// 詳細行 1 つ。
#[derive(Clone, Debug)]
pub struct NoteDetail {
    pub label: &'static str,
    pub text: String,
    /// `Rows Removed by ...` の元（ラベルと数える側）。`sources` は `assemble` が持ち主のノードの番号で埋める。
    pub removed: Option<(&'static str, FilterCounter)>,
}

impl NoteDetail {
    pub fn new(label: &'static str, text: String) -> Self {
        NoteDetail {
            label,
            text,
            removed: None,
        }
    }

    /// `Filter:`（走査・集約・`Filter` の群）。`Rows Removed by Filter` を伴う。
    pub fn filter(text: String) -> Self {
        NoteDetail {
            label: "Filter",
            text,
            removed: Some(("Rows Removed by Filter", FilterCounter::Filter)),
        }
    }

    /// `Join Filter:`。`Rows Removed by Join Filter` を伴う。
    pub fn join_filter(text: String) -> Self {
        NoteDetail {
            label: "Join Filter",
            text,
            removed: Some(("Rows Removed by Join Filter", FilterCounter::JoinFilter)),
        }
    }
}

/// 物理ノード 1 つ分の表示用の文字列。
#[derive(Clone, Debug, Default)]
pub struct NodeNote {
    pub title: String,
    pub details: Vec<NoteDetail>,
    /// VERBOSE の `Output:` 行。`explain_verbose` のときだけ入れる。
    pub output: Vec<String>,
    /// この節点の式の中に現れた `SubPlan` / `InitPlan`。式を降ろした順。
    pub subplans: Vec<SubPlanId>,
    /// コスト欄の width。
    pub width: u32,
    /// `Update` / `Delete` が、入力の群の `Output:` を置き換える文字列（VERBOSE。10 §3.8）。
    pub child_output: Option<Vec<String>>,
}

/// 列名・パラメータ名・SubPlan の表記を引く（deparse のコールバックの実体）。
pub struct ExplainNames<'a> {
    arena: &'a ColumnArena,
    /// `Project` / `Aggregate` が定義した列の式。使うときに規則に応じて文字列にする。
    defs: HashMap<ColId, DExpr>,
    /// `NestedLoopParam` / `SubPlan` のパラメータ → 外側の列（常に修飾して表示する）。
    params: HashMap<ParamId, ECol>,
    /// `SubPlanId` ごとの表記。
    labels: Vec<SubPlanLabel>,
    /// 共有する CTE が使った計画番号の数（`SubPlan N` の N に足す）。
    plan_id_base: usize,
    verbose: bool,
    /// 文全体の範囲表の数の見立て（走査の葉の数）。
    n_rtable: usize,
    catalog: &'a dyn CatalogReader,
    type_env: &'a TypeEnv<'a>,
    /// 集約の結果を包まずに書く（`HAVING` のように集約ノードの中で評価する式）。
    raw_aggs: std::cell::Cell<bool>,
}

impl std::fmt::Debug for ExplainNames<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExplainNames")
            .field("verbose", &self.verbose)
            .field("n_rtable", &self.n_rtable)
            .finish_non_exhaustive()
    }
}

impl<'a> ExplainNames<'a> {
    /// `leaf_count`: 論理プラン（根・CTE・副問い合わせの全部）の走査の葉（`Get`・`FunctionScan`・`Values`・
    /// `CteScan`）の数。`n_rtable` の代わりに修飾の判断に使う。
    pub fn new(
        arena: &'a ColumnArena,
        leaf_count: usize,
        verbose: bool,
        catalog: &'a dyn CatalogReader,
        type_env: &'a TypeEnv<'a>,
    ) -> Self {
        ExplainNames {
            arena,
            defs: HashMap::new(),
            params: HashMap::new(),
            labels: Vec::new(),
            plan_id_base: 0,
            verbose,
            n_rtable: leaf_count,
            catalog,
            type_env,
            raw_aggs: std::cell::Cell::new(false),
        }
    }

    /// 以降の式で集約の結果を包まない（集約ノードに併合する `Filter` の間だけ true）。
    pub fn set_raw_aggs(&self, on: bool) {
        self.raw_aggs.set(on);
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn arena(&self) -> &ColumnArena {
        self.arena
    }

    /// 列 `id` を計算する式を登録する。
    pub fn define(&mut self, id: ColId, e: DExpr) {
        self.defs.insert(id, e);
    }

    /// パラメータ `p` が外側の列 `src` の値であることを登録する。
    pub fn bind_param(&mut self, p: ParamId, src: ECol) {
        self.params.insert(p, src);
    }

    pub fn set_plan_id_base(&mut self, n: usize) {
        self.plan_id_base = n;
    }

    /// 次の `SubPlanId` の表記を足す。
    pub fn push_label(&mut self, strategy: &SubPlanStrategy) {
        let id = self.labels.len() + 1 + self.plan_id_base;
        self.labels.push(match strategy {
            SubPlanStrategy::InitOnce => SubPlanLabel {
                name: format!("InitPlan {id}"),
                hashed: false,
                init_plan: true,
            },
            SubPlanStrategy::Rescan => SubPlanLabel {
                name: format!("SubPlan {id}"),
                hashed: false,
                init_plan: false,
            },
            SubPlanStrategy::Hashed { .. } => SubPlanLabel {
                name: format!("SubPlan {id}"),
                hashed: true,
                init_plan: false,
            },
        });
    }

    /// 物理式（`layout` で `Local` を `ColId` に戻す）を表示用の式にする。
    #[allow(clippy::unused_self)]
    pub fn disp(&self, e: &PhysExpr, layout: &[ColId], subplans: &[SubPlanDef]) -> Result<DExpr> {
        disp_expr(e, layout, subplans)
    }
}

fn disp_expr(e: &PhysExpr, layout: &[ColId], subplans: &[SubPlanDef]) -> Result<DExpr> {
    e.try_map(&mut |n| match &n.kind {
        ExprKind::Column(PhysCol::Local(i)) => {
            let c = layout.get(*i).ok_or_else(|| {
                Error::internal(format!(
                    "EXPLAIN: Local({i}) outside the node's {} columns",
                    layout.len()
                ))
            })?;
            Ok(Some(DExpr::new(
                ExprKind::Column(ECol::Col(*c)),
                n.ty,
                n.span,
            )))
        }
        ExprKind::Column(PhysCol::Param(p)) => Ok(Some(DExpr::new(
            ExprKind::Column(ECol::Param(*p)),
            n.ty,
            n.span,
        ))),
        ExprKind::SubLink { kind, query, .. } => {
            let def = subplans.get(usize::from(query.0)).ok_or_else(|| {
                Error::internal(format!("EXPLAIN: SubPlanId {} is not defined", query.0))
            })?;
            let test = def
                .test
                .as_ref()
                .map(|t| disp_expr(t, layout, subplans).map(Box::new))
                .transpose()?;
            Ok(Some(DExpr::new(
                ExprKind::SubLink {
                    kind: *kind,
                    test,
                    query: *query,
                },
                n.ty,
                n.span,
            )))
        }
        ExprKind::Aggregate(_) => Err(Error::internal("EXPLAIN: aggregate in a physical expr")),
        _ => Ok(None),
    })
}

impl ExplainNames<'_> {
    /// 表示用の式を規則 `rule` で文字列にする。
    pub fn text(&self, e: &DExpr, rule: QualifyRule) -> Result<String> {
        let namer = Namer {
            names: self,
            qualify: use_prefix(rule, self.verbose, self.n_rtable),
        };
        namer.deparse(e)
    }

    /// 物理式を規則 `rule` で文字列にする（`layout` はその式を評価する行の列）。
    pub fn expr(
        &self,
        e: &PhysExpr,
        layout: &[ColId],
        rule: QualifyRule,
        subplans: &[SubPlanDef],
    ) -> Result<String> {
        self.text(&self.disp(e, layout, subplans)?, rule)
    }

    /// 列 `c` の参照としての表示（計算列は `(` `)` で包む。`Output:` の規則）。
    pub fn col(&self, c: ColId, rule: QualifyRule) -> Result<String> {
        self.text(&DExpr::column(ECol::Col(c), self.arena.get(c).ty), rule)
    }

    /// `layout` の各列の表示（`Output:` の規則 O）。VERBOSE でなければ空。
    pub fn output_of(&self, layout: &[ColId]) -> Result<Vec<String>> {
        if !self.verbose {
            return Ok(Vec::new());
        }
        layout
            .iter()
            .map(|c| self.col(*c, QualifyRule::Output))
            .collect()
    }

    /// 式の並びを `Output:` の要素にする（定義するノード用。包まない）。VERBOSE でなければ空。
    pub fn output_exprs(
        &self,
        exprs: &[PhysExpr],
        layout: &[ColId],
        subplans: &[SubPlanDef],
    ) -> Result<Vec<String>> {
        if !self.verbose {
            return Ok(Vec::new());
        }
        exprs
            .iter()
            .map(|e| self.expr(e, layout, QualifyRule::Output, subplans))
            .collect()
    }

    /// コスト欄の width（`layout` の列の型の幅の和）。
    pub fn width_of(&self, layout: &[ColId]) -> u32 {
        row_width(layout.iter().map(|c| self.arena.get(*c).ty))
    }
}

impl SubLinkRenderer<SubPlanId> for ExplainNames<'_> {
    fn label(&self, q: &SubPlanId) -> Result<SubPlanLabel> {
        self.labels
            .get(usize::from(q.0))
            .cloned()
            .ok_or_else(|| Error::internal(format!("EXPLAIN: no label for SubPlan {}", q.0)))
    }
}

/// 列の表示を引く `ColumnNamer`（修飾するかどうかは `qualify`）。
struct Namer<'n, 'a> {
    names: &'n ExplainNames<'a>,
    qualify: bool,
}

impl Namer<'_, '_> {
    fn deparse(&self, e: &DExpr) -> Result<String> {
        let cx = DeparseCtx {
            opts: DeparseOptions::EXPLAIN,
            namer: self,
            sublinks: Some(self.names),
            type_env: self.names.type_env,
            catalog: self.names.catalog,
        };
        deparse_expr(e, &cx)
    }

    fn base(&self, id: ColId) -> Result<ColText> {
        let info = self.names.arena.try_get(id).ok_or_else(|| {
            Error::internal(format!("EXPLAIN: ColId {} is outside the arena", id.0))
        })?;
        let name = quote_identifier(&info.name);
        Ok(match (&info.qualifier, self.qualify) {
            (Some(q), true) => ColText::plain(format!("{}.{name}", quote_identifier(q))),
            _ => ColText::plain(name),
        })
    }
}

impl ColumnNamer<ECol> for Namer<'_, '_> {
    fn name(&self, col: &ECol) -> Result<ColText> {
        match col {
            ECol::Param(p) => {
                let src = self.names.params.get(p).ok_or_else(|| {
                    Error::internal(format!("EXPLAIN: no source for parameter {}", p.0))
                })?;
                // 外側の列を参照するパラメータは、規則に関係なく常に修飾する。
                Namer {
                    names: self.names,
                    qualify: true,
                }
                .name(src)
            }
            ECol::Col(id) => match self.names.defs.get(id) {
                Some(def) => match &def.kind {
                    ExprKind::Column(inner) => self.name(inner),
                    ExprKind::Aggregate(_) if self.names.raw_aggs.get() => {
                        Ok(ColText::plain(self.deparse(def)?))
                    }
                    _ => Ok(ColText::computed(self.deparse(def)?)),
                },
                None => self.base(*id),
            },
        }
    }
}

// ----- 葉の数 -----------------------------------------------------------------------

/// 論理プラン（式の中の副問い合わせを含む）の走査の葉の数。
pub fn count_leaves(p: &LogicalPlan) -> usize {
    let own = usize::from(matches!(
        p,
        LogicalPlan::Get { .. }
            | LogicalPlan::FunctionScan { .. }
            | LogicalPlan::Values { .. }
            | LogicalPlan::CteScan { .. }
            // FROM のない SELECT は PostgreSQL の範囲表に `RTE_RESULT` として入る。
            | LogicalPlan::Result { .. }
    ));
    let mut n = own;
    for e in p.exprs() {
        e.walk(&mut |x| {
            if let ExprKind::SubLink { query, .. } = &x.kind {
                n += count_leaves(&query.plan);
            }
            true
        });
    }
    n + p.children().into_iter().map(count_leaves).sum::<usize>()
}

// ----- 組み立て ---------------------------------------------------------------------

struct Group {
    node: ExplainNode,
    /// まだ付けていない SubPlan（併合・透過したノードのものを含む）。
    pend: Vec<SubPlanId>,
}

struct Asm<'a> {
    q: &'a PhysicalQuery,
    notes: &'a [NodeNote],
    base: usize,
    pre: usize,
    post: usize,
    /// 組み立て済みの `SubPlan` の木（添字は `SubPlanId`。自分より小さいものだけ参照できる）。
    sub_nodes: &'a [ExplainNode],
    /// 共有する CTE が使った計画番号の数（`SubPlan N` の N に足す）。
    plan_id_base: u32,
}

fn detail_of(d: &NoteDetail, owner: usize) -> ExplainDetail {
    ExplainDetail {
        label: d.label,
        text: d.text.clone(),
        removed: d.removed.map(|(label, counter)| RemovedRows {
            label,
            sources: vec![(owner, counter)],
        }),
    }
}

impl Asm<'_> {
    #[allow(clippy::unused_self)]
    fn node_of(&self, note: &NodeNote, exec_id: usize, children: Vec<ExplainChild>) -> ExplainNode {
        ExplainNode {
            title: note.title.clone(),
            details: note.details.iter().map(|d| detail_of(d, exec_id)).collect(),
            output: note.output.clone(),
            children,
            exec_id,
            width: note.width,
        }
    }

    /// 付けていない SubPlan（InitPlan 以外）を、`SubPlanId` 昇順で普通の子の後に付ける。
    fn finalize(&self, g: Group) -> Result<ExplainNode> {
        let mut node = g.node;
        let ids: BTreeSet<SubPlanId> = g.pend.into_iter().collect();
        for id in ids {
            let def = self.q.subplans.get(usize::from(id.0));
            if matches!(def.map(|d| &d.strategy), Some(SubPlanStrategy::InitOnce)) {
                continue;
            }
            node.children.push(ExplainChild {
                label: Some(format!(
                    "SubPlan {}",
                    u32::from(id.0) + 1 + self.plan_id_base
                )),
                node: self.sub(id)?.clone(),
            });
        }
        Ok(node)
    }

    fn sub(&self, id: SubPlanId) -> Result<&ExplainNode> {
        self.sub_nodes.get(usize::from(id.0)).ok_or_else(|| {
            Error::internal(format!(
                "EXPLAIN: SubPlan {} is used before it is assembled",
                id.0
            ))
        })
    }

    fn child(label: Option<String>, node: ExplainNode) -> ExplainChild {
        ExplainChild { label, node }
    }

    #[allow(clippy::too_many_lines)]
    fn walk(&mut self, plan: &PhysicalPlan) -> Result<Group> {
        use PhysicalPlan as P;
        let my_exec = self.base + self.pre;
        self.pre += 1;
        let mut groups = Vec::new();
        for c in plan.children() {
            groups.push(self.walk(c)?);
        }
        let note_idx = self.post;
        self.post += 1;
        let note = self.notes.get(note_idx).ok_or_else(|| {
            Error::internal("EXPLAIN: fewer notes than physical nodes (physicalize bug)")
        })?;
        let pend: Vec<SubPlanId> = note.subplans.clone();
        match plan {
            P::Filter { .. } => {
                let mut g = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: Filter without input"))?;
                for d in &note.details {
                    if let Some(existing) = g.node.details.iter_mut().find(|x| x.label == d.label)
                        && d.label == "Filter"
                    {
                        existing.text = format!("({} AND {})", existing.text, d.text);
                        if let (Some(r), Some((_, c))) = (existing.removed.as_mut(), d.removed) {
                            r.sources.push((my_exec, c));
                        }
                    } else {
                        g.node.details.push(detail_of(d, my_exec));
                    }
                }
                g.node.exec_id = my_exec;
                g.node.width = note.width;
                g.pend.extend(pend);
                Ok(g)
            }
            P::Project { .. } => {
                let mut g = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: Project without input"))?;
                if !note.output.is_empty() {
                    g.node.output.clone_from(&note.output);
                }
                g.node.exec_id = my_exec;
                g.node.width = note.width;
                g.pend.extend(pend);
                Ok(g)
            }
            P::Result { .. }
            | P::Values { .. }
            | P::SeqScan { .. }
            | P::IndexScan { .. }
            | P::FunctionScan { .. }
            | P::CteScan { .. } => Ok(Group {
                node: self.node_of(note, my_exec, Vec::new()),
                pend,
            }),
            P::HashJoin { .. } => {
                // `children()` は probe → build の順（`build_is_left` を織り込み済み）。
                let build = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: HashJoin without inputs"))?;
                let probe = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: HashJoin without inputs"))?;
                let build = self.finalize(build)?;
                let hash = ExplainNode {
                    title: "Hash".to_owned(),
                    details: Vec::new(),
                    output: build.output.clone(),
                    exec_id: build.exec_id,
                    width: build.width,
                    children: vec![Self::child(None, build)],
                };
                let probe = self.finalize(probe)?;
                Ok(Group {
                    node: self.node_of(
                        note,
                        my_exec,
                        vec![Self::child(None, probe), Self::child(None, hash)],
                    ),
                    pend,
                })
            }
            P::HashSetOp { .. } => {
                let right = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: HashSetOp without inputs"))?;
                let left = groups
                    .pop()
                    .ok_or_else(|| Error::internal("EXPLAIN: HashSetOp without inputs"))?;
                let mut scans = Vec::new();
                for (i, g) in [left, right].into_iter().enumerate() {
                    let child = self.finalize(g)?;
                    scans.push(Self::child(
                        None,
                        ExplainNode {
                            title: format!("Subquery Scan on \"*SELECT* {}\"", i + 1),
                            details: Vec::new(),
                            output: child.output.clone(),
                            exec_id: child.exec_id,
                            width: child.width,
                            children: vec![Self::child(None, child)],
                        },
                    ));
                }
                let append = ExplainNode {
                    title: "Append".to_owned(),
                    details: Vec::new(),
                    output: Vec::new(),
                    exec_id: scans.first().map_or(my_exec, |c| c.node.exec_id),
                    width: note.width,
                    children: scans,
                };
                Ok(Group {
                    node: self.node_of(note, my_exec, vec![Self::child(None, append)]),
                    pend,
                })
            }
            P::Update { .. } | P::Delete { .. } => {
                let mut children = Vec::new();
                for g in groups {
                    let mut n = self.finalize(g)?;
                    if let Some(o) = &note.child_output {
                        n.output.clone_from(o);
                    }
                    children.push(Self::child(None, n));
                }
                Ok(Group {
                    node: self.node_of(note, my_exec, children),
                    pend,
                })
            }
            _ => {
                let mut children = Vec::new();
                for g in groups {
                    children.push(Self::child(None, self.finalize(g)?));
                }
                Ok(Group {
                    node: self.node_of(note, my_exec, children),
                    pend,
                })
            }
        }
    }
}

/// 木 1 本（根・CTE・`SubPlan` のどれか）を `ExplainNode` にする。その木の `InitPlan` は根の子の先頭に付く。
fn assemble_tree(
    q: &PhysicalQuery,
    plan: &PhysicalPlan,
    notes: &[NodeNote],
    base: usize,
    sub_nodes: &[ExplainNode],
) -> Result<ExplainNode> {
    if notes.len() != plan.node_count() {
        return Err(Error::internal(format!(
            "EXPLAIN: {} notes for {} physical nodes (physicalize bug)",
            notes.len(),
            plan.node_count()
        )));
    }
    let mut a = Asm {
        q,
        notes,
        base,
        pre: 0,
        post: 0,
        sub_nodes,
        plan_id_base: u32::try_from(q.ctes.len()).unwrap_or(0),
    };
    let g = a.walk(plan)?;
    let mut root = a.finalize(g)?;
    let init: BTreeSet<SubPlanId> = notes
        .iter()
        .flat_map(|n| n.subplans.iter().copied())
        .filter(|id| {
            matches!(
                q.subplans.get(usize::from(id.0)).map(|d| &d.strategy),
                Some(SubPlanStrategy::InitOnce)
            )
        })
        .collect();
    for (at, id) in init.into_iter().enumerate() {
        root.children.insert(
            at,
            ExplainChild {
                label: Some(format!("InitPlan {}", u32::from(id.0) + 1 + a.plan_id_base)),
                node: a.sub(id)?.clone(),
            },
        );
    }
    Ok(root)
}

/// 物理プランから `ExplainNode` の木を組み立てる。戻り値は根の木と、`SubPlanDef` ごとの木（`subplans` と
/// 同じ添字）。根の木の子の先頭に、共有 CTE（添字順）が `CTE <名前>` のラベルつきで付く（InitPlan の前）。
pub fn assemble(
    q: &PhysicalQuery,
    cte_names: &[String],
    root_notes: &[NodeNote],
    cte_notes: &[Vec<NodeNote>],
    sub_notes: &[Vec<NodeNote>],
) -> Result<(ExplainNode, Vec<ExplainNode>)> {
    if cte_notes.len() != q.ctes.len()
        || sub_notes.len() != q.subplans.len()
        || cte_names.len() != q.ctes.len()
    {
        return Err(Error::internal(
            "EXPLAIN: the number of note lists differs from the number of plans",
        ));
    }
    let ids = assign_exec_ids(q);
    let mut subs: Vec<ExplainNode> = Vec::with_capacity(q.subplans.len());
    for (i, def) in q.subplans.iter().enumerate() {
        let n = assemble_tree(q, &def.plan, &sub_notes[i], ids.subplans[i], &subs)?;
        subs.push(n);
    }
    let mut ctes = Vec::with_capacity(q.ctes.len());
    for (i, plan) in q.ctes.iter().enumerate() {
        ctes.push(assemble_tree(q, plan, &cte_notes[i], ids.ctes[i], &subs)?);
    }
    let mut root = assemble_tree(q, &q.root, root_notes, ids.root, &subs)?;
    for (i, n) in ctes.into_iter().enumerate().rev() {
        root.children.insert(
            0,
            ExplainChild {
                label: Some(format!("CTE {}", cte_names[i])),
                node: n,
            },
        );
    }
    Ok((root, subs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::planner::logical::ColumnInfo;
    use crate::storage::RelHandle;
    use crate::types::{Datum, SqlType};

    fn note(title: &str) -> NodeNote {
        NodeNote {
            title: title.to_owned(),
            ..NodeNote::default()
        }
    }

    fn scan() -> PhysicalPlan {
        let mut cat = FakeCatalog::new("postgres");
        let t = cat.add(&TableBuilder::new("t").column("a", SqlType::INT4));
        PhysicalPlan::SeqScan {
            rel: RelHandle::from_table(&t),
            columns: vec![SqlType::INT4],
            system_columns: vec![],
            filter: None,
        }
    }

    fn query(root: PhysicalPlan) -> PhysicalQuery {
        PhysicalQuery::single(root, Vec::new())
    }

    #[test]
    fn filter_merges_into_scan_and_takes_the_outer_exec_id() {
        let plan = PhysicalPlan::Limit {
            input: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan()),
                predicate: PhysExpr::bool_lit(true),
            }),
            limit: None,
            offset: None,
        };
        let q = query(plan);
        let scan_note = NodeNote {
            title: "Seq Scan on t".into(),
            details: vec![NoteDetail::filter("(a > 1)".into())],
            ..NodeNote::default()
        };
        let filter_note = NodeNote {
            details: vec![NoteDetail::filter("(a < 9)".into())],
            ..note("")
        };
        let notes = vec![scan_note, filter_note, note("Limit")];
        let (root, subs) = assemble(&q, &[], &notes, &[], &[]).unwrap();
        assert!(subs.is_empty());
        assert_eq!(root.title, "Limit");
        assert_eq!(root.exec_id, 0);
        let s = &root.children[0].node;
        assert_eq!(s.title, "Seq Scan on t");
        // 群の exec_id は最も外側（Filter = 1）。2 つの述語は 1 行にまとまり、数える元は 2 つ。
        assert_eq!(s.exec_id, 1);
        assert_eq!(s.details.len(), 1);
        assert_eq!(s.details[0].text, "((a > 1) AND (a < 9))");
        assert_eq!(
            s.details[0].removed.as_ref().unwrap().sources,
            vec![(2, FilterCounter::Filter), (1, FilterCounter::Filter)]
        );
    }

    #[test]
    fn hash_join_gets_a_synthetic_hash_node() {
        let plan = PhysicalPlan::HashJoin {
            kind: super::super::logical::JoinKind::Inner,
            left: Box::new(scan()),
            right: Box::new(scan()),
            left_keys: vec![],
            right_keys: vec![],
            key_types: vec![],
            residual: None,
            build_is_left: true,
            left_width: 1,
            right_width: 1,
        };
        let q = query(plan);
        // children() は [probe, build] = [right, left]。メモも同じ後順。
        let notes = vec![
            note("Seq Scan on right"),
            note("Seq Scan on left"),
            note("Hash Right Join"),
        ];
        let (root, _) = assemble(&q, &[], &notes, &[], &[]).unwrap();
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].node.title, "Seq Scan on right");
        let hash = &root.children[1].node;
        assert_eq!(hash.title, "Hash");
        assert_eq!(hash.children[0].node.title, "Seq Scan on left");
        assert_eq!(hash.exec_id, hash.children[0].node.exec_id);
    }

    #[test]
    fn note_count_mismatch_is_an_internal_error() {
        let q = query(scan());
        let e = assemble(&q, &[], &[], &[], &[]).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn init_plan_goes_to_the_root_and_sub_plan_to_the_displaying_node() {
        use crate::expr::SubLinkKind;
        let sub = |strategy| SubPlanDef {
            plan: scan(),
            kind: SubLinkKind::Scalar,
            test: None,
            params: vec![],
            strategy,
            explain: None,
        };
        let mut q = query(PhysicalPlan::Limit {
            input: Box::new(scan()),
            limit: None,
            offset: None,
        });
        q.subplans = vec![sub(SubPlanStrategy::InitOnce), sub(SubPlanStrategy::Rescan)];
        let mut scan_note = note("Seq Scan on t");
        scan_note.subplans = vec![SubPlanId(1), SubPlanId(0)];
        let notes = vec![scan_note, note("Limit")];
        let sub_notes = vec![vec![note("Seq Scan on s0")], vec![note("Seq Scan on s1")]];
        let (root, subs) = assemble(&q, &[], &notes, &[], &sub_notes).unwrap();
        assert_eq!(subs.len(), 2);
        // 根: InitPlan 1、普通の子。
        assert_eq!(root.children[0].label.as_deref(), Some("InitPlan 1"));
        assert_eq!(root.children[0].node.title, "Seq Scan on s0");
        let scan = &root.children[1].node;
        assert_eq!(scan.title, "Seq Scan on t");
        // 走査のノード: SubPlan 2 だけ（InitPlan はここには付かない）。
        assert_eq!(scan.children.len(), 1);
        assert_eq!(scan.children[0].label.as_deref(), Some("SubPlan 2"));
        // exec_id: 根 0・1、subplans 2・3。
        assert_eq!(subs[0].exec_id, 2);
        assert_eq!(subs[1].exec_id, 3);
    }

    #[test]
    fn ctes_come_first_with_their_names() {
        let mut q = query(scan());
        q.ctes = vec![scan()];
        let (root, _) = assemble(
            &q,
            &["x".to_owned()],
            &[note("Seq Scan on t")],
            &[vec![note("Seq Scan on c")]],
            &[],
        )
        .unwrap();
        assert_eq!(root.children[0].label.as_deref(), Some("CTE x"));
        assert_eq!(root.children[0].node.exec_id, 1);
    }

    #[test]
    fn names_qualify_by_rule_and_wrap_computed_columns() {
        let mut arena = ColumnArena::default();
        let a = arena.add(ColumnInfo {
            name: "a".into(),
            qualifier: Some("t".into()),
            ty: SqlType::INT4,
            origin: Some((1, 1)),
        });
        let x = arena.add(ColumnInfo {
            name: "x".into(),
            qualifier: None,
            ty: SqlType::INT4,
            origin: None,
        });
        let cat = FakeCatalog::new("postgres");
        let type_env = TypeEnv::default();
        let mut names = ExplainNames::new(&arena, 1, false, &cat, &type_env);
        let plus = crate::catalog::builtin::operators_named("+")
            .into_iter()
            .find(|o| o.left == Some(SqlType::INT4.oid) && o.right == SqlType::INT4.oid)
            .unwrap();
        names.define(
            x,
            DExpr::new(
                ExprKind::Operator {
                    op: plus,
                    args: vec![
                        DExpr::column(ECol::Col(a), SqlType::INT4),
                        DExpr::literal(Datum::Int4(1), SqlType::INT4),
                    ],
                },
                SqlType::INT4,
                crate::error::Span::default(),
            ),
        );
        // 単一の葉・非 VERBOSE: どの規則でも修飾しない。
        assert_eq!(names.col(a, QualifyRule::Upper).unwrap(), "a");
        assert_eq!(names.col(x, QualifyRule::Output).unwrap(), "((a + 1))");
        // 葉が 2 つ以上: Output と Upper は修飾、Scan は修飾しない。計算列は使う場所の規則で書き直す。
        let names2 = ExplainNames::new(&arena, 2, false, &cat, &type_env);
        assert_eq!(names2.col(a, QualifyRule::Upper).unwrap(), "t.a");
        assert_eq!(names2.col(a, QualifyRule::Scan).unwrap(), "a");
        // VERBOSE: 走査の条件も修飾する。
        let names3 = ExplainNames::new(&arena, 1, true, &cat, &type_env);
        assert_eq!(names3.col(a, QualifyRule::Scan).unwrap(), "t.a");
    }

    #[test]
    fn parameters_are_always_qualified() {
        let mut arena = ColumnArena::default();
        let a = arena.add(ColumnInfo {
            name: "a".into(),
            qualifier: Some("u".into()),
            ty: SqlType::INT4,
            origin: Some((1, 1)),
        });
        let cat = FakeCatalog::new("postgres");
        let type_env = TypeEnv::default();
        let mut names = ExplainNames::new(&arena, 1, false, &cat, &type_env);
        names.bind_param(ParamId(0), ECol::Col(a));
        let e = DExpr::column(ECol::Param(ParamId(0)), SqlType::INT4);
        assert_eq!(names.text(&e, QualifyRule::Scan).unwrap(), "u.a");
    }

    #[test]
    fn leaf_count_includes_subqueries() {
        let mut cat = FakeCatalog::new("postgres");
        let t = cat.add(&TableBuilder::new("t").column("a", SqlType::INT4));
        let get = || LogicalPlan::Get {
            rel: RelHandle::from_table(&t),
            table: std::sync::Arc::clone(&t),
            alias: None,
            cols: vec![],
            system_columns: vec![],
        };
        let sub = super::super::logical::LogicalSubquery {
            plan: get(),
            output: vec![],
        };
        let pred = crate::planner::logical::LExpr::new(
            ExprKind::SubLink {
                kind: crate::expr::SubLinkKind::Exists,
                test: None,
                query: Box::new(sub),
            },
            SqlType::BOOL,
            crate::error::Span::default(),
        );
        let p = LogicalPlan::Filter {
            input: Box::new(get()),
            predicate: pred,
        };
        assert_eq!(count_leaves(&p), 2);
    }
}
