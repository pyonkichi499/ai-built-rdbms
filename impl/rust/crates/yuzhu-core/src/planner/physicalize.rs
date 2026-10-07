//! 論理プラン → 物理プラン（`m4/04` §3.5・§7、`m4/02` §3.6）。
//!
//! 各論理ノードを物理ノードにし、`ColId → 位置`（`layout`）の表で式の `Column(ColId)` を
//! `PhysCol::Local(位置)` に降ろす。走査の選択は `index_select`、サイズの手がかりは `size`、EXPLAIN の木は
//! 物理ノードを作るたびに積むメモ（`explain_tree::NodeNote`）から `explain_tree::assemble` で作る。
//!
//! 畳み込み: 恒等の `Project` の省略、`Project(Result)` → `Result { exprs }`、`Empty` → `Result { false }`、
//! `Filter(Get)` → `SeqScan` / `IndexScan` の `filter`。

use std::collections::HashMap;

use super::PlanEnv;
use super::explain_tree::{
    DExpr, ECol, ExplainNames, NodeNote, NoteDetail, assemble, count_leaves,
};
use super::index_select::{IndexPick, OrderKey, QualKind, ScanRequest, choose_scan};
use super::logical::{
    ColumnArena, JoinKind, LAggCall, LExpr, LogicalPlan, LogicalQuery, LogicalSubquery,
};
use super::physical::{
    IndexScanKey, IndexScanKeys, PhysAgg, PhysCheck, PhysExpr, PhysicalPlan, PhysicalQuery,
    RangeBound, SortKey, SubPlanDef, SubPlanStrategy,
};
use super::size::{ROWS_PER_BLOCK_EST, SizeCtx};
use super::util::{
    ColSet, EquiKey, and_all, coerce_expr, conjuncts, expr_refs, plan_free_cols, plan_output,
    split_on,
};
use crate::analyzer::bound::SetOpKind;
use crate::catalog::{SystemColumn, builtin};
use crate::error::{Error, Result, Span, sqlstate};
use crate::explain::node::{
    DmlKind, JoinAlgo, QualifyRule, cte_scan_title, dml_title, function_scan_title,
    hash_setop_title, index_scan_title, join_title, seq_scan_title, values_scan_title,
};
use crate::expr::{ColId, CteId, ExprKind, ParamId, PhysCol, SubLinkKind, SubPlanId};
use crate::types::SqlType;

/// 再帰の深さの上限（ルール・build・物理化とも）。超えたら 54001。
pub const MAX_PLAN_DEPTH: usize = 500;

/// 自由な列（相関する外側の列）→ パラメータ。
type ParamMap = HashMap<ColId, ParamId>;

pub fn physicalize(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<PhysicalQuery> {
    let LogicalQuery {
        plan,
        arena,
        output,
        columns,
        ctes,
        ..
    } = q;
    // INSERT の対象表は走査の葉にならないが、PostgreSQL の範囲表には入る（EXPLAIN の列の修飾に効く）。
    let target = usize::from(matches!(plan, LogicalPlan::Insert { .. }));
    let leaves =
        count_leaves(&plan) + ctes.iter().map(|c| count_leaves(&c.plan)).sum::<usize>() + target;
    let mut p = Physicalizer {
        env,
        arena: &arena,
        subplans: Vec::new(),
        sub_notes: Vec::new(),
        ctes: Vec::new(),
        cte_notes: Vec::new(),
        cte_names: Vec::new(),
        single_row_values_as_result: false,
        scan_hint: None,
        order_achieved: false,
        sort_under_limit: false,
        cte_index: HashMap::new(),
        next_param: 0,
        size: SizeCtx::new(env),
        names: env.want_explain.then(|| {
            ExplainNames::new(
                &arena,
                leaves,
                env.explain_verbose,
                env.catalog,
                env.type_env,
            )
        }),
        notes: vec![Vec::new()],
        pending: Vec::new(),
        depth: 0,
    };
    // PostgreSQL は共有する CTE の計画にも計画番号を使う（`SubPlan N` / `InitPlan N` はその後ろから振る）。
    let shared_ctes = ctes.iter().filter(|c| !c.inline && c.refs != 0).count();
    if let Some(n) = p.names.as_mut() {
        n.set_plan_id_base(shared_ctes);
    }
    for (i, cte) in ctes.iter().enumerate() {
        if cte.inline || cte.refs == 0 {
            continue;
        }
        p.notes.push(Vec::new());
        let phys = p.phys(&cte.plan, &ParamMap::new())?;
        let phys = p.ensure_layout(phys, &cte.output)?;
        let notes = p.notes.pop().unwrap_or_default();
        let id = CteId(u16::try_from(i).map_err(|_| too_many("subqueries"))?);
        p.size.register_cte(id, p.size.estimate(&cte.plan)?);
        p.cte_index.insert(id, p.ctes.len());
        p.ctes.push(phys.plan);
        p.cte_notes.push(notes);
        p.cte_names.push(cte.name.clone());
    }
    let is_dml = matches!(
        plan,
        LogicalPlan::Insert { .. } | LogicalPlan::Update { .. } | LogicalPlan::Delete { .. }
    );
    let mut root = p.phys(&plan, &ParamMap::new())?;
    if !is_dml {
        root = p.ensure_layout(root, &output)?;
    }
    let root_notes = p.notes.pop().unwrap_or_default();
    let mut pq = PhysicalQuery {
        root: root.plan,
        subplans: p.subplans,
        ctes: p.ctes,
        n_params: p.next_param,
        output: columns,
        explain: None,
    };
    if p.names.is_some() {
        let (root_tree, subs) =
            assemble(&pq, &p.cte_names, &root_notes, &p.cte_notes, &p.sub_notes)?;
        pq.explain = Some(root_tree);
        for (def, n) in pq.subplans.iter_mut().zip(subs) {
            def.explain = Some(n);
        }
    }
    Ok(pq)
}

fn too_many(what: &str) -> Error {
    Error::new(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "too many {} in query",
            if what == "params" { "parameters" } else { what }
        ),
    )
}

struct Physicalizer<'a> {
    env: &'a PlanEnv<'a>,
    arena: &'a ColumnArena,
    subplans: Vec<SubPlanDef>,
    sub_notes: Vec<Vec<NodeNote>>,
    ctes: Vec<PhysicalPlan>,
    cte_notes: Vec<Vec<NodeNote>>,
    cte_names: Vec<String>,
    /// `INSERT ... VALUES (1 行)` の挿入元（PostgreSQL は `Result` ノードにする）。
    single_row_values_as_result: bool,
    /// `Sort` の直下の走査に渡す順序の要求（`m4/04` §7.4）と、`force_order`。
    scan_hint: Option<(Vec<OrderKey>, bool)>,
    /// 直前に作った走査がその要求を走査順で満たした。
    order_achieved: bool,
    /// 直上の `Limit`（定数の limit）の直下が `Sort`。
    sort_under_limit: bool,
    cte_index: HashMap<CteId, usize>,
    /// 発行した `ParamId` の数（`u16` を超えたら 54000）。
    next_param: usize,
    size: SizeCtx<'a>,
    names: Option<ExplainNames<'a>>,
    /// 作っている木（根・CTE・SubPlan）のメモ。末尾が今の木。
    notes: Vec<Vec<NodeNote>>,
    /// 今の節点の式の中で出会った `SubPlan`。
    pending: Vec<SubPlanId>,
    depth: usize,
}

/// 物理化したノードと、その出力列。
struct Phys {
    plan: PhysicalPlan,
    layout: Vec<ColId>,
}

/// 走査の組み立て結果（INL が使う）。
struct ScanParts<'p> {
    get: &'p LogicalPlan,
    /// 述語（`Get` の直上の `Filter` の conjunct ++ 結合条件）。
    preds: Vec<LExpr>,
    /// `preds[i]` を `filter` に残すか。
    in_filter: Vec<bool>,
}

/// `Sort` の入力が `Project*` / `Filter*` を通って `Get` に至り、キーがすべて列の参照のとき、走査に渡す順序の要求
/// （`m4/04` §7.4）。キーの列は `Project` のパススルーをたどって `Get` の列に戻す。
fn order_hint(
    input: &LogicalPlan,
    keys: &[super::logical::LSortKey],
    force: bool,
) -> Option<(Vec<OrderKey>, bool)> {
    use LogicalPlan as L;
    let mut cols: Vec<ColId> = keys
        .iter()
        .map(|k| match &k.expr.kind {
            ExprKind::Column(c) => Some(*c),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let mut p = input;
    loop {
        match p {
            L::Project { input, exprs } => {
                for c in &mut cols {
                    let (_, e) = exprs.iter().find(|(id, _)| id == c)?;
                    let ExprKind::Column(x) = &e.kind else {
                        return None;
                    };
                    *c = *x;
                }
                p = input;
            }
            L::Filter { input, .. } => p = input,
            L::Get { .. } => break,
            _ => return None,
        }
    }
    let keys = keys
        .iter()
        .zip(cols)
        .map(|(k, col)| OrderKey {
            col,
            descending: k.descending,
            nulls_first: k.nulls_first,
        })
        .collect();
    Some((keys, force))
}

impl<'a> Physicalizer<'a> {
    // ----- メモ -------------------------------------------------------------------

    fn explaining(&self) -> bool {
        self.names.is_some()
    }

    /// 物理ノードを 1 つ作ったときに呼ぶ。EXPLAIN でなければ何もしない。
    fn emit(
        &mut self,
        f: impl FnOnce(&ExplainNames<'a>, &[SubPlanDef]) -> Result<NodeNote>,
    ) -> Result<()> {
        let Some(names) = self.names.as_ref() else {
            return Ok(());
        };
        let mut n = f(names, &self.subplans)?;
        n.subplans = std::mem::take(&mut self.pending);
        if let Some(t) = self.notes.last_mut() {
            t.push(n);
        }
        Ok(())
    }

    fn simple_note(
        &mut self,
        title: String,
        details: Vec<NoteDetail>,
        layout: &[ColId],
    ) -> Result<()> {
        self.emit(|names, _| {
            Ok(NodeNote {
                title,
                details,
                output: names.output_of(layout)?,
                width: names.width_of(layout),
                ..NodeNote::default()
            })
        })
    }

    fn verbose(&self) -> bool {
        self.names.as_ref().is_some_and(ExplainNames::verbose)
    }

    fn col_text(&self, cs: &[ColId], rule: QualifyRule) -> Result<Vec<String>> {
        match &self.names {
            Some(n) => cs.iter().map(|c| n.col(*c, rule)).collect(),
            None => Ok(Vec::new()),
        }
    }

    fn new_param(&mut self) -> Result<ParamId> {
        let id = u16::try_from(self.next_param).map_err(|_| too_many("params"))?;
        self.next_param += 1;
        Ok(ParamId(id))
    }

    // ----- 再帰 -------------------------------------------------------------------

    fn phys(&mut self, p: &LogicalPlan, params: &ParamMap) -> Result<Phys> {
        self.depth += 1;
        crate::sql::check_stack_depth()?;
        if self.depth > MAX_PLAN_DEPTH {
            return Err(Error::new(
                sqlstate::STATEMENT_TOO_COMPLEX,
                "stack depth limit exceeded",
            ));
        }
        let r = self.phys_node(p, params);
        self.depth -= 1;
        r
    }

    #[allow(clippy::too_many_lines)]
    fn phys_node(&mut self, p: &LogicalPlan, params: &ParamMap) -> Result<Phys> {
        use LogicalPlan as L;
        match p {
            L::Get { .. } => self.phys_scan(p, Vec::new(), params),
            L::Filter { input, predicate } => {
                let mut preds = conjuncts(predicate.clone());
                let mut inner: &LogicalPlan = input;
                while let L::Filter { input, predicate } = inner {
                    let mut more = conjuncts(predicate.clone());
                    more.append(&mut preds);
                    preds = more;
                    inner = input;
                }
                if matches!(inner, L::Get { .. }) {
                    return self.phys_scan(inner, preds, params);
                }
                let child = self.phys(inner, params)?;
                let pred = and_all(preds);
                let Some(pred) = pred else { return Ok(child) };
                let predicate = self.lower(&pred, &child.layout, params)?;
                let rule = if matches!(
                    inner,
                    L::FunctionScan { .. } | L::Values { .. } | L::CteScan { .. }
                ) {
                    QualifyRule::Scan
                } else {
                    QualifyRule::Upper
                };
                let layout = child.layout.clone();
                let over_agg = matches!(inner, L::Aggregate { .. });
                self.emit(|names, subs| {
                    names.set_raw_aggs(over_agg);
                    let text = names.expr(&predicate, &layout, rule, subs);
                    names.set_raw_aggs(false);
                    Ok(NodeNote {
                        title: "Filter".into(),
                        details: vec![NoteDetail::filter(text?)],
                        width: names.width_of(&layout),
                        ..NodeNote::default()
                    })
                })?;
                Ok(Phys {
                    plan: PhysicalPlan::Filter {
                        input: Box::new(child.plan),
                        predicate,
                    },
                    layout: child.layout,
                })
            }
            L::Values { rows, cols } => {
                let rows = rows
                    .iter()
                    .map(|r| r.iter().map(|e| self.lower(e, &[], params)).collect())
                    .collect::<Result<Vec<Vec<PhysExpr>>>>()?;
                if std::mem::take(&mut self.single_row_values_as_result) && rows.len() == 1 {
                    self.result_note(&rows[0], None, cols)?;
                } else {
                    self.simple_note(values_scan_title(None), Vec::new(), cols)?;
                }
                Ok(Phys {
                    plan: PhysicalPlan::Values { rows },
                    layout: cols.clone(),
                })
            }
            L::FunctionScan {
                func,
                args,
                alias,
                cols,
            } => {
                let args = args
                    .iter()
                    .map(|e| self.lower(e, &[], params))
                    .collect::<Result<Vec<_>>>()?;
                let title = function_scan_title(func.name, alias.as_deref(), self.verbose());
                let fname = func.name;
                self.emit(|names, subs| {
                    // VERBOSE では `Function Call: f(args)` を足す。
                    let mut details = Vec::new();
                    if names.verbose() {
                        let texts = args
                            .iter()
                            .map(|a| names.expr(a, &[], QualifyRule::Scan, subs))
                            .collect::<Result<Vec<_>>>()?;
                        details.push(NoteDetail::new(
                            "Function Call",
                            format!("{fname}({})", texts.join(", ")),
                        ));
                    }
                    Ok(NodeNote {
                        title,
                        details,
                        output: names.output_of(cols)?,
                        width: names.width_of(cols),
                        ..NodeNote::default()
                    })
                })?;
                Ok(Phys {
                    plan: PhysicalPlan::FunctionScan { func, args },
                    layout: cols.clone(),
                })
            }
            L::CteScan { cte, alias, cols } => {
                let idx = *self.cte_index.get(cte).ok_or_else(|| {
                    Error::internal(format!("CteScan refers to the unplanned CTE {}", cte.0))
                })?;
                let title = cte_scan_title(&self.cte_names[idx], alias.as_deref());
                self.simple_note(title, Vec::new(), cols)?;
                Ok(Phys {
                    plan: PhysicalPlan::CteScan { cte: idx },
                    layout: cols.clone(),
                })
            }
            L::Result {
                one_time_filter,
                cols,
            } => {
                if !cols.is_empty() {
                    return Err(Error::internal(
                        "a Result node with columns is not supported",
                    ));
                }
                let otf = one_time_filter
                    .as_ref()
                    .map(|e| self.lower(e, &[], params))
                    .transpose()?;
                self.result_note(&[], otf.as_ref(), &[])?;
                Ok(Phys {
                    plan: PhysicalPlan::Result {
                        exprs: Vec::new(),
                        one_time_filter: otf,
                    },
                    layout: Vec::new(),
                })
            }
            L::Empty { cols } => {
                let otf = PhysExpr::bool_lit(false);
                self.result_note(&[], Some(&otf), cols)?;
                Ok(Phys {
                    plan: PhysicalPlan::Result {
                        exprs: cols
                            .iter()
                            .map(|c| PhysExpr::null_of(self.arena.get(*c).ty))
                            .collect(),
                        one_time_filter: Some(otf),
                    },
                    layout: cols.clone(),
                })
            }
            L::Project { input, exprs } => self.phys_project(input, exprs, params),
            L::Sort { input, keys } => {
                let under_limit = std::mem::take(&mut self.sort_under_limit);
                self.scan_hint =
                    order_hint(input, keys, under_limit || !self.env.settings.enable_sort);
                self.order_achieved = false;
                let child = self.phys(input, params);
                self.scan_hint = None;
                let achieved = std::mem::take(&mut self.order_achieved);
                let child = child?;
                if achieved {
                    return Ok(child);
                }
                let keys = keys
                    .iter()
                    .map(|k| {
                        Ok(SortKey {
                            expr: self.lower(&k.expr, &child.layout, params)?,
                            descending: k.descending,
                            nulls_first: k.nulls_first,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.sort_note(&keys, &child.layout)?;
                Ok(Phys {
                    plan: PhysicalPlan::Sort {
                        input: Box::new(child.plan),
                        keys,
                    },
                    layout: child.layout,
                })
            }
            L::Distinct { input, on } => {
                let child = self.phys(input, params)?;
                match on {
                    None => {
                        let layout = child.layout.clone();
                        let keys = self.col_text(&layout, QualifyRule::Upper)?;
                        self.simple_note(
                            "HashAggregate".into(),
                            vec![NoteDetail::new("Group Key", keys.join(", "))],
                            &layout,
                        )?;
                        Ok(Phys {
                            plan: PhysicalPlan::Distinct {
                                input: Box::new(child.plan),
                            },
                            layout: child.layout,
                        })
                    }
                    Some(on) => {
                        let mut key_cols = Vec::with_capacity(on.len());
                        let mut on_ids = Vec::with_capacity(on.len());
                        for e in on {
                            let ExprKind::Column(c) = &e.kind else {
                                return Err(Error::internal("DISTINCT ON of a non-column"));
                            };
                            let pos =
                                child.layout.iter().position(|x| x == c).ok_or_else(|| {
                                    Error::internal("DISTINCT ON column is not in the input")
                                })?;
                            key_cols.push(pos);
                            on_ids.push(*c);
                        }
                        if let LogicalPlan::Sort { keys, .. } = &**input {
                            let mut head: Vec<ColId> = keys
                                .iter()
                                .take(on.len())
                                .filter_map(|k| match &k.expr.kind {
                                    ExprKind::Column(c) => Some(*c),
                                    _ => None,
                                })
                                .collect();
                            let mut want = on_ids.clone();
                            head.sort();
                            want.sort();
                            want.dedup();
                            head.dedup();
                            if head != want {
                                return Err(Error::internal(
                                    "DISTINCT ON keys are not the first sort keys",
                                ));
                            }
                        }
                        let layout = child.layout.clone();
                        self.simple_note("Unique".into(), Vec::new(), &layout)?;
                        Ok(Phys {
                            plan: PhysicalPlan::Unique {
                                input: Box::new(child.plan),
                                key_cols,
                            },
                            layout: child.layout,
                        })
                    }
                }
            }
            L::Limit {
                input,
                limit,
                offset,
            } => {
                self.sort_under_limit = limit.is_some() && matches!(**input, L::Sort { .. });
                let child = self.phys(input, params)?;
                let limit = limit
                    .as_ref()
                    .map(|e| self.lower(e, &[], params))
                    .transpose()?;
                let offset = offset
                    .as_ref()
                    .map(|e| self.lower(e, &[], params))
                    .transpose()?;
                let layout = child.layout.clone();
                self.simple_note("Limit".into(), Vec::new(), &layout)?;
                Ok(Phys {
                    plan: PhysicalPlan::Limit {
                        input: Box::new(child.plan),
                        limit,
                        offset,
                    },
                    layout: child.layout,
                })
            }
            L::Join {
                kind,
                left,
                right,
                on,
            } => self.phys_join(*kind, left, right, on.as_ref(), params),
            L::Aggregate {
                input,
                group_by,
                aggs,
            } => self.phys_aggregate(input, group_by, aggs, params),
            L::SetOp {
                op,
                all,
                left,
                right,
                cols,
                left_cols,
                right_cols,
            } => self.phys_setop(*op, *all, left, right, cols, left_cols, right_cols, params),
            L::Insert {
                table,
                rel,
                input,
                input_cols,
                column_map,
                defaults,
                checks,
                not_null,
                returning,
            } => {
                self.single_row_values_as_result = {
                    let mut p: &LogicalPlan = input;
                    while let L::Project { input: i, .. } = p {
                        p = i;
                    }
                    matches!(p, L::Values { .. })
                };
                let child = self.phys(input, params);
                self.single_row_values_as_result = false;
                let child = child?;
                let child = self.ensure_layout(child, input_cols)?;
                self.dml_note(DmlKind::Insert, &table.schema, &table.name, None)?;
                Ok(Phys {
                    plan: PhysicalPlan::Insert {
                        rel: rel.clone(),
                        input: Box::new(child.plan),
                        column_map: column_map.clone(),
                        defaults: defaults.clone(),
                        checks: sorted_checks(checks),
                        not_null: not_null.clone(),
                        table_name: table.name.clone(),
                        returning: returning.clone(),
                    },
                    layout: Vec::new(),
                })
            }
            L::Update {
                table,
                rel,
                input,
                old_cols,
                ctid,
                new_values,
                checks,
                not_null,
                returning,
            } => {
                let child = self.phys(input, params)?;
                let mut want = old_cols.clone();
                want.push(*ctid);
                want.extend(new_values.iter().map(|(_, c)| *c));
                let child = self.ensure_layout(child, &want)?;
                let n = old_cols.len();
                // RETURNING のない UPDATE の入力は、新しい値と `ctid` だけを出す（PostgreSQL の `Output: (b + 1), ctid`）。
                let shown = returning.is_none().then(|| {
                    let mut v: Vec<ColId> = new_values.iter().map(|(_, c)| *c).collect();
                    v.push(*ctid);
                    v
                });
                self.dml_note(
                    DmlKind::Update,
                    &table.schema,
                    &table.name,
                    shown.as_deref(),
                )?;
                Ok(Phys {
                    plan: PhysicalPlan::Update {
                        rel: rel.clone(),
                        input: Box::new(child.plan),
                        n_user_cols: n,
                        assigned: new_values
                            .iter()
                            .enumerate()
                            .map(|(i, (attno, _))| (*attno, n + 1 + i))
                            .collect(),
                        checks: sorted_checks(checks),
                        not_null: not_null.clone(),
                        table_name: table.name.clone(),
                        returning: returning.clone(),
                    },
                    layout: Vec::new(),
                })
            }
            L::Delete {
                table,
                rel,
                input,
                old_cols,
                ctid,
                returning,
            } => {
                let child = self.phys(input, params)?;
                let mut want = old_cols.clone();
                want.push(*ctid);
                let child = self.ensure_layout(child, &want)?;
                // RETURNING のない DELETE の入力は `ctid` だけを出す（PostgreSQL の `Output: ctid`）。
                let only_ctid = returning.is_none().then(|| vec![*ctid]);
                self.dml_note(
                    DmlKind::Delete,
                    &table.schema,
                    &table.name,
                    only_ctid.as_deref(),
                )?;
                Ok(Phys {
                    plan: PhysicalPlan::Delete {
                        rel: rel.clone(),
                        input: Box::new(child.plan),
                        n_user_cols: old_cols.len(),
                        returning: returning.clone(),
                    },
                    layout: Vec::new(),
                })
            }
        }
    }

    fn dml_note(
        &mut self,
        kind: DmlKind,
        schema: &str,
        name: &str,
        child_cols: Option<&[ColId]>,
    ) -> Result<()> {
        let title = dml_title(kind, schema, name, None, self.verbose());
        self.emit(|names, _| {
            let child_output = match child_cols {
                Some(cs) if names.verbose() => Some(
                    cs.iter()
                        .map(|c| names.col(*c, QualifyRule::Output))
                        .collect::<Result<Vec<_>>>()?,
                ),
                _ => None,
            };
            Ok(NodeNote {
                title,
                child_output,
                ..NodeNote::default()
            })
        })
    }

    fn result_note(
        &mut self,
        exprs: &[PhysExpr],
        otf: Option<&PhysExpr>,
        layout: &[ColId],
    ) -> Result<()> {
        self.emit(|names, subs| {
            let mut details = Vec::new();
            if let Some(o) = otf {
                details.push(NoteDetail::new(
                    "One-Time Filter",
                    names.expr(o, &[], QualifyRule::Upper, subs)?,
                ));
            }
            let output = if exprs.is_empty() {
                names.output_of(layout)?
            } else {
                names.output_exprs(exprs, &[], subs)?
            };
            Ok(NodeNote {
                title: "Result".into(),
                details,
                output,
                width: names.width_of(layout),
                ..NodeNote::default()
            })
        })
    }

    fn sort_note(&mut self, keys: &[SortKey], layout: &[ColId]) -> Result<()> {
        self.emit(|names, subs| {
            let mut texts = Vec::new();
            for k in keys {
                let mut t = names.expr(&k.expr, layout, QualifyRule::Upper, subs)?;
                if k.descending {
                    t.push_str(" DESC");
                }
                if k.nulls_first != k.descending {
                    t.push_str(if k.nulls_first {
                        " NULLS FIRST"
                    } else {
                        " NULLS LAST"
                    });
                }
                texts.push(t);
            }
            Ok(NodeNote {
                title: "Sort".into(),
                details: vec![NoteDetail::new("Sort Key", texts.join(", "))],
                output: names.output_of(layout)?,
                width: names.width_of(layout),
                ..NodeNote::default()
            })
        })
    }

    // ----- Project ------------------------------------------------------------------

    fn phys_project(
        &mut self,
        input: &LogicalPlan,
        exprs: &[(ColId, LExpr)],
        params: &ParamMap,
    ) -> Result<Phys> {
        let child = self.phys(input, params)?;
        let lowered = exprs
            .iter()
            .map(|(_, e)| self.lower(e, &child.layout, params))
            .collect::<Result<Vec<_>>>()?;
        let layout: Vec<ColId> = exprs.iter().map(|(c, _)| *c).collect();
        if let Some(names) = self.names.as_mut() {
            let defs = layout
                .iter()
                .zip(&lowered)
                .map(|(id, e)| {
                    names
                        .disp(e, &child.layout, &self.subplans)
                        .map(|d| (*id, d))
                })
                .collect::<Result<Vec<_>>>()?;
            for (id, d) in defs {
                if !matches!(d.kind, ExprKind::Column(ECol::Col(c)) if c == id) {
                    names.define(id, d);
                }
            }
        }
        // 畳み込み: FROM なしの SELECT。
        if let PhysicalPlan::Result {
            exprs: inner,
            one_time_filter,
        } = &child.plan
            && inner.is_empty()
        {
            // 子の `Result` のメモを置き換える（ノードは 1 つにする）。`One-Time Filter` は残す。
            if let Some(t) = self.notes.last_mut() {
                t.pop();
            }
            let otf = one_time_filter.clone();
            self.result_note(&lowered, otf.as_ref(), &layout)?;
            return Ok(Phys {
                plan: PhysicalPlan::Result {
                    exprs: lowered,
                    one_time_filter: otf,
                },
                layout,
            });
        }
        if is_identity(&lowered, child.layout.len()) {
            return Ok(Phys {
                plan: child.plan,
                layout,
            });
        }
        // 集約の上の射影は PostgreSQL では集約ノードが行うので、集約の結果は包まずに書く。
        let mut under = input;
        while let LogicalPlan::Filter { input, .. } = under {
            under = input;
        }
        let over_agg = matches!(under, LogicalPlan::Aggregate { .. });
        self.emit(|names, subs| {
            names.set_raw_aggs(over_agg);
            let output = names.output_exprs(&lowered, &child.layout, subs);
            names.set_raw_aggs(false);
            Ok(NodeNote {
                title: "Project".into(),
                output: output?,
                width: names.width_of(&layout),
                ..NodeNote::default()
            })
        })?;
        Ok(Phys {
            plan: PhysicalPlan::Project {
                input: Box::new(child.plan),
                exprs: lowered,
            },
            layout,
        })
    }

    // ----- 走査 ---------------------------------------------------------------------

    fn pick_index(
        &self,
        get: &LogicalPlan,
        preds: &[LExpr],
        outer_cols: &ColSet,
        hint: Option<&(Vec<OrderKey>, bool)>,
    ) -> Option<IndexPick> {
        let LogicalPlan::Get {
            rel,
            table,
            cols,
            system_columns,
            ..
        } = get
        else {
            return None;
        };
        let sys: Vec<ColId> = system_columns.iter().map(|(_, c)| *c).collect();
        let req = ScanRequest {
            table,
            rel,
            cols,
            system_cols: &sys,
            conjuncts: preds,
            outer_cols,
            want_order: hint.map(|h| h.0.as_slice()),
            force_order: hint.is_some_and(|h| h.1),
        };
        choose_scan(&req, self.env.settings)
    }

    fn phys_scan(
        &mut self,
        get: &LogicalPlan,
        preds: Vec<LExpr>,
        params: &ParamMap,
    ) -> Result<Phys> {
        let hint = self.scan_hint.take();
        let pick = self.pick_index(get, &preds, &ColSet::new(), hint.as_ref());
        self.order_achieved = pick.as_ref().is_some_and(|p| p.ordered);
        let in_filter = vec![true; preds.len()];
        self.build_scan(
            &ScanParts {
                get,
                preds,
                in_filter,
            },
            pick.as_ref(),
            params,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn build_scan(
        &mut self,
        parts: &ScanParts<'_>,
        pick: Option<&IndexPick>,
        params: &ParamMap,
    ) -> Result<Phys> {
        let LogicalPlan::Get {
            rel,
            table,
            alias,
            cols,
            system_columns,
        } = parts.get
        else {
            return Err(Error::internal("a scan over a non-Get node"));
        };
        let layout = parts.get.output_cols();
        let types: Vec<SqlType> = table.columns.iter().map(|c| c.ty).collect();
        let sys: Vec<SystemColumn> = system_columns.iter().map(|(sc, _)| *sc).collect();
        // 述語は 1 つずつ 1 回だけ降ろす（SubLink を 2 回降ろすと SubPlan が二重に登録される）。
        let mut lowered: Vec<(usize, PhysExpr)> = Vec::new();
        for (i, p) in parts.preds.iter().enumerate() {
            if parts.in_filter[i] {
                lowered.push((i, self.lower(p, &layout, params)?));
            }
        }
        let and_of = |it: Vec<PhysExpr>| (!it.is_empty()).then(|| PhysExpr::and_all(it));
        let filter = and_of(lowered.iter().map(|(_, e)| e.clone()).collect());
        let Some(pick) = pick else {
            let title =
                seq_scan_title(&table.schema, &table.name, alias.as_deref(), self.verbose());
            let shown = filter.clone();
            let lay = layout.clone();
            self.emit(|names, subs| {
                let mut details = Vec::new();
                if let Some(f) = &shown {
                    details.push(NoteDetail::filter(names.expr(
                        f,
                        &lay,
                        QualifyRule::Scan,
                        subs,
                    )?));
                }
                Ok(NodeNote {
                    title,
                    details,
                    output: names.output_of(&lay)?,
                    width: names.width_of(&lay),
                    ..NodeNote::default()
                })
            })?;
            return Ok(Phys {
                plan: PhysicalPlan::SeqScan {
                    rel: rel.clone(),
                    columns: types,
                    system_columns: sys,
                    filter,
                },
                layout,
            });
        };
        // インデックス走査: 値の式は layout = [] で降ろす。
        let mut keys = IndexScanKeys::default();
        let mut cond_texts: Vec<(ColId, QualKind, Option<PhysExpr>)> = Vec::new();
        let col_of = |attnum: i16| -> Result<ColId> {
            table
                .columns
                .iter()
                .position(|c| c.attnum == attnum)
                .and_then(|i| cols.get(i).copied())
                .ok_or_else(|| Error::internal("an index column is not in the scan"))
        };
        for q in &pick.eq {
            let v = q
                .value
                .as_ref()
                .map(|v| self.lower(v, &[], params))
                .transpose()?;
            keys.eq.push(match &v {
                Some(v) => IndexScanKey::Eq(v.clone()),
                None => IndexScanKey::IsNull,
            });
            cond_texts.push((col_of(q.attnum)?, q.kind, v));
        }
        for (q, is_lower) in pick
            .lower
            .iter()
            .map(|q| (q, true))
            .chain(pick.upper.iter().map(|q| (q, false)))
        {
            let v = q
                .value
                .as_ref()
                .map(|v| self.lower(v, &[], params))
                .transpose()?
                .ok_or_else(|| Error::internal("a range qual without a value"))?;
            let b = RangeBound {
                expr: v.clone(),
                inclusive: matches!(q.kind, QualKind::Ge | QualKind::Le),
            };
            if is_lower {
                keys.lower = Some(b);
            } else {
                keys.upper = Some(b);
            }
            cond_texts.push((col_of(q.attnum)?, q.kind, Some(v)));
        }
        let used = pick.used();
        let index = rel
            .indexes
            .get(pick.index)
            .ok_or_else(|| Error::internal("the picked index is not in the relation"))?
            .clone();
        let backward = pick.direction == super::physical::ScanDirection::Backward;
        let title = index_scan_title(
            &table.indexes[pick.index].name,
            &table.schema,
            &table.name,
            alias.as_deref(),
            self.verbose(),
            backward,
        );
        let rest = and_of(
            lowered
                .iter()
                .filter(|(i, _)| !used.contains(i))
                .map(|(_, e)| e.clone())
                .collect(),
        );
        let lay = layout.clone();
        self.emit(|names, subs| {
            let mut conds = Vec::new();
            for (c, kind, v) in &cond_texts {
                let ty = names.arena().get(*c).ty;
                let cname = names.text(&DExpr::column(ECol::Col(*c), ty), QualifyRule::Scan)?;
                conds.push(match v {
                    Some(v) => format!(
                        "({cname} {} {})",
                        kind.symbol(),
                        names.expr(v, &[], QualifyRule::Scan, subs)?
                    ),
                    None => format!("({cname} IS NULL)"),
                });
            }
            let conds_empty = conds.is_empty();
            let cond = if conds.len() == 1 {
                conds.remove(0)
            } else {
                format!("({})", conds.join(" AND "))
            };
            // 順序のためだけの全走査（キーなし）は `Index Cond` の行を出さない。
            let mut details = if conds_empty {
                Vec::new()
            } else {
                vec![NoteDetail::new("Index Cond", cond)]
            };
            if let Some(r) = &rest {
                details.push(NoteDetail::filter(names.expr(
                    r,
                    &lay,
                    QualifyRule::Scan,
                    subs,
                )?));
            }
            Ok(NodeNote {
                title,
                details,
                output: names.output_of(&lay)?,
                width: names.width_of(&lay),
                ..NodeNote::default()
            })
        })?;
        Ok(Phys {
            plan: PhysicalPlan::IndexScan {
                rel: rel.clone(),
                index,
                keys,
                direction: pick.direction,
                columns: types,
                system_columns: sys,
                filter,
            },
            layout,
        })
    }

    // ----- 結合 ---------------------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn phys_join(
        &mut self,
        kind: JoinKind,
        left: &LogicalPlan,
        right: &LogicalPlan,
        on: Option<&LExpr>,
        params: &ParamMap,
    ) -> Result<Phys> {
        let lset: ColSet = plan_output(left).into_iter().collect();
        let rset: ColSet = plan_output(right).into_iter().collect();
        let split = split_on(on, &lset, &rset, self.env.catalog);
        let s = self.env.settings;
        if kind == JoinKind::Full && split.keys.is_empty() {
            return Err(Error::not_supported(
                "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
            ));
        }
        if kind != JoinKind::Full
            && s.enable_nestloop
            && s.enable_indexscan
            && let Some(p) = self.try_inl(
                kind,
                left,
                right,
                on,
                params,
                // ハッシュ結合が使えない（無効、または等値キーがない）ときは、内側を毎回走査し直すより
                // 索引を引くほうがよいので、サイズの条件を見ない。
                !s.enable_hashjoin || split.keys.is_empty(),
            )?
        {
            return Ok(p);
        }
        let use_hash = !split.keys.is_empty() && (kind == JoinKind::Full || s.enable_hashjoin);
        let build_is_left = use_hash
            && !matches!(kind, JoinKind::Semi | JoinKind::Anti)
            && self.size.estimate(left)? < self.size.estimate(right)?;
        // 子のメモは `children()` の順（probe → build）に積む。
        let (lp, rp) = if build_is_left {
            let rp = self.phys(right, params)?;
            let lp = self.phys(left, params)?;
            (lp, rp)
        } else {
            let lp = self.phys(left, params)?;
            let rp = self.phys(right, params)?;
            (lp, rp)
        };
        let combined: Vec<ColId> = lp.layout.iter().chain(&rp.layout).copied().collect();
        let (lw, rw) = (lp.layout.len(), rp.layout.len());
        let out_layout = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            lp.layout.clone()
        } else {
            combined.clone()
        };
        if use_hash {
            let (mut lk, mut rk, mut kt) = (Vec::new(), Vec::new(), Vec::new());
            let mut shown: Vec<(PhysExpr, PhysExpr, &'static crate::catalog::BuiltinOperator)> =
                Vec::new();
            for k in &split.keys {
                let (l, r) = self.key_pair(k)?;
                let (l, r) = (
                    self.lower(&l, &lp.layout, params)?,
                    self.lower(&r, &rp.layout, params)?,
                );
                shown.push((l.clone(), r.clone(), k.op));
                lk.push(l);
                rk.push(r);
                kt.push(k.key_type);
            }
            let residual = and_all(split.residual.clone())
                .map(|e| self.lower(&e, &combined, params))
                .transpose()?;
            let title = join_title(JoinAlgo::Hash, kind, build_is_left);
            let (llay, rlay, comb, out) = (
                lp.layout.clone(),
                rp.layout.clone(),
                combined.clone(),
                out_layout.clone(),
            );
            let resid = residual.clone();
            self.emit(|names, subs| {
                let mut conds = Vec::new();
                for (l, r, op) in &shown {
                    let (dl, dr) = (names.disp(l, &llay, subs)?, names.disp(r, &rlay, subs)?);
                    let args = if build_is_left {
                        vec![dr, dl]
                    } else {
                        vec![dl, dr]
                    };
                    let e = DExpr::new(
                        ExprKind::Operator { op, args },
                        SqlType::BOOL,
                        Span::default(),
                    );
                    conds.push(names.text(&e, QualifyRule::Upper)?);
                }
                let cond = if conds.len() == 1 {
                    conds.remove(0)
                } else {
                    format!("({})", conds.join(" AND "))
                };
                let mut details = vec![NoteDetail::new("Hash Cond", cond)];
                if let Some(r) = &resid {
                    details.push(NoteDetail::join_filter(names.expr(
                        r,
                        &comb,
                        QualifyRule::Upper,
                        subs,
                    )?));
                }
                Ok(NodeNote {
                    title,
                    details,
                    output: names.output_of(&out)?,
                    width: names.width_of(&out),
                    ..NodeNote::default()
                })
            })?;
            return Ok(Phys {
                plan: PhysicalPlan::HashJoin {
                    kind,
                    left: Box::new(lp.plan),
                    right: Box::new(rp.plan),
                    left_keys: lk,
                    right_keys: rk,
                    key_types: kt,
                    residual,
                    build_is_left,
                    left_width: lw,
                    right_width: rw,
                },
                layout: out_layout,
            });
        }
        let join_filter = on.map(|e| self.lower(e, &combined, params)).transpose()?;
        let mut inner = rp;
        if s.enable_material
            && !inner.plan.uses_params()
            && !matches!(
                inner.plan,
                PhysicalPlan::Materialize { .. }
                    | PhysicalPlan::Values { .. }
                    | PhysicalPlan::Result { .. }
                    | PhysicalPlan::CteScan { .. }
            )
        {
            let lay = inner.layout.clone();
            self.simple_note("Materialize".into(), Vec::new(), &lay)?;
            inner.plan = PhysicalPlan::Materialize {
                input: Box::new(inner.plan),
            };
        }
        self.nl_note(kind, join_filter.as_ref(), &combined, &out_layout)?;
        Ok(Phys {
            plan: PhysicalPlan::NestedLoopJoin {
                kind,
                outer: Box::new(lp.plan),
                inner: Box::new(inner.plan),
                join_filter,
                outer_width: lw,
                inner_width: rw,
            },
            layout: out_layout,
        })
    }

    fn nl_note(
        &mut self,
        kind: JoinKind,
        jf: Option<&PhysExpr>,
        combined: &[ColId],
        out: &[ColId],
    ) -> Result<()> {
        let title = join_title(JoinAlgo::NestedLoop, kind, false);
        self.emit(|names, subs| {
            let mut details = Vec::new();
            if let Some(f) = jf {
                details.push(NoteDetail::join_filter(names.expr(
                    f,
                    combined,
                    QualifyRule::Upper,
                    subs,
                )?));
            }
            Ok(NodeNote {
                title,
                details,
                output: names.output_of(out)?,
                width: names.width_of(out),
                ..NodeNote::default()
            })
        })
    }

    /// キーの両辺を `key_type` にそろえる（論理の式のまま）。
    fn key_pair(&self, k: &EquiKey) -> Result<(LExpr, LExpr)> {
        let l = coerce_expr(k.left.clone(), k.key_type, self.env.catalog);
        let r = coerce_expr(k.right.clone(), k.key_type, self.env.catalog);
        match (l, r) {
            (Some(l), Some(r)) => Ok((l, r)),
            _ => Err(Error::internal(
                "a join key cannot be coerced to its key type",
            )),
        }
    }

    /// 内側 Index Scan の Nested Loop（§7.5.3）。
    #[allow(clippy::too_many_lines)]
    fn try_inl(
        &mut self,
        kind: JoinKind,
        left: &LogicalPlan,
        right: &LogicalPlan,
        on: Option<&LExpr>,
        params: &ParamMap,
        force: bool,
    ) -> Result<Option<Phys>> {
        let orders: &[bool] = if kind == JoinKind::Inner {
            &[false, true]
        } else {
            &[false]
        };
        for &swap in orders {
            let (outer_plan, inner_plan) = if swap { (right, left) } else { (left, right) };
            // 内側は `Get` か `Filter* (Get)`。
            let mut inner_preds: Vec<LExpr> = Vec::new();
            let mut g: &LogicalPlan = inner_plan;
            while let LogicalPlan::Filter { input, predicate } = g {
                let mut more = conjuncts(predicate.clone());
                more.append(&mut inner_preds);
                inner_preds = more;
                g = input;
            }
            let LogicalPlan::Get { rel, table, .. } = g else {
                continue;
            };
            if table.indexes.is_empty() {
                continue;
            }
            let outer_cols: ColSet = plan_output(outer_plan).into_iter().collect();
            let on_conj = on.map(|e| conjuncts(e.clone())).unwrap_or_default();
            let n_inner = inner_preds.len();
            let mut preds = inner_preds.clone();
            preds.extend(on_conj.iter().cloned());
            let Some(pick) = self.pick_index(g, &preds, &outer_cols, None) else {
                continue;
            };
            let uses_outer = pick.uses(|q| {
                q.value
                    .as_ref()
                    .is_some_and(|v| !expr_refs(v).is_disjoint(&outer_cols))
            });
            if !uses_outer {
                continue;
            }
            let probes = self.size.estimate(outer_plan)? * ROWS_PER_BLOCK_EST;
            let probe_cost = if pick.unique_full {
                3.0
            } else if pick.eq.is_empty() {
                12.0
            } else {
                6.0
            };
            let inner_blocks = f64::from(self.size.nblocks(rel)?.max(1));
            if !force && probes * probe_cost >= inner_blocks {
                continue;
            }
            // 採用。
            let used = pick.used();
            let mut in_filter = vec![true; n_inner];
            let mut join_rest = Vec::new();
            for (j, c) in on_conj.iter().enumerate() {
                if used.contains(&(n_inner + j)) {
                    in_filter.push(true);
                } else {
                    in_filter.push(false);
                    join_rest.push(c.clone());
                }
            }
            let outer = self.phys(outer_plan, params)?;
            let mut ext = params.clone();
            let mut np: Vec<(ParamId, PhysExpr)> = Vec::new();
            let mut refs = ColSet::new();
            for (i, p) in preds.iter().enumerate() {
                if in_filter[i] {
                    refs.extend(expr_refs(p));
                }
            }
            for q in pick
                .eq
                .iter()
                .chain(pick.lower.iter())
                .chain(pick.upper.iter())
            {
                if let Some(v) = &q.value {
                    refs.extend(expr_refs(v));
                }
            }
            for c in refs.intersection(&outer_cols) {
                let pos = outer.layout.iter().position(|x| x == c).ok_or_else(|| {
                    Error::internal(
                        "an outer column of an inner index scan is not in the outer plan",
                    )
                })?;
                let pid = self.new_param()?;
                ext.insert(*c, pid);
                np.push((
                    pid,
                    PhysExpr::column(PhysCol::Local(pos), self.arena.get(*c).ty),
                ));
                if let Some(n) = self.names.as_mut() {
                    n.bind_param(pid, ECol::Col(*c));
                }
            }
            let inner = self.build_scan(
                &ScanParts {
                    get: g,
                    preds,
                    in_filter,
                },
                Some(&pick),
                &ext,
            )?;
            let combined: Vec<ColId> = outer.layout.iter().chain(&inner.layout).copied().collect();
            let join_filter = and_all(join_rest)
                .map(|e| self.lower(&e, &combined, params))
                .transpose()?;
            let semi = matches!(kind, JoinKind::Semi | JoinKind::Anti);
            let out_layout = if semi {
                outer.layout.clone()
            } else {
                combined.clone()
            };
            self.nl_note(kind, join_filter.as_ref(), &combined, &out_layout)?;
            let (ow, iw) = (outer.layout.len(), inner.layout.len());
            let nlp = PhysicalPlan::NestedLoopParam {
                kind,
                outer: Box::new(outer.plan),
                inner: Box::new(inner.plan),
                params: np,
                join_filter,
                outer_width: ow,
                inner_width: iw,
            };
            let phys = Phys {
                plan: nlp,
                layout: out_layout,
            };
            if !swap {
                return Ok(Some(phys));
            }
            // 入れ替え: 論理の左 ++ 右に戻す。
            let want: Vec<ColId> = plan_output(left)
                .into_iter()
                .chain(plan_output(right))
                .collect();
            return Ok(Some(self.ensure_layout(phys, &want)?));
        }
        Ok(None)
    }

    // ----- 集約 ---------------------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn phys_aggregate(
        &mut self,
        input: &LogicalPlan,
        group_by: &[(ColId, LExpr)],
        calls: &[(ColId, LAggCall)],
        params: &ParamMap,
    ) -> Result<Phys> {
        let child = self.phys(input, params)?;
        let keys = group_by
            .iter()
            .map(|(_, e)| self.lower(e, &child.layout, params))
            .collect::<Result<Vec<_>>>()?;
        let key_types: Vec<SqlType> = keys.iter().map(|k| k.ty).collect();
        let mut paggs = Vec::with_capacity(calls.len());
        for (_, a) in calls {
            let args = a
                .args
                .iter()
                .map(|e| self.lower(e, &child.layout, params))
                .collect::<Result<Vec<_>>>()?;
            let filter = a
                .filter
                .as_ref()
                .map(|e| self.lower(e, &child.layout, params))
                .transpose()?;
            let mut order_by = Vec::with_capacity(a.order_by.len());
            for k in &a.order_by {
                order_by.push(SortKey {
                    expr: self.lower(&k.expr, &child.layout, params)?,
                    descending: k.descending,
                    nulls_first: k.nulls_first,
                });
            }
            paggs.push(PhysAgg {
                kind: a.func.kind,
                arg_types: args.iter().map(|e| e.ty).collect(),
                args,
                distinct: a.distinct,
                filter,
                order_by,
                result: SqlType::of(a.func.result),
            });
        }
        let layout: Vec<ColId> = group_by
            .iter()
            .map(|(c, _)| *c)
            .chain(calls.iter().map(|(c, _)| *c))
            .collect();
        // 表示用の定義（集約の呼び出しと group の式）。
        if let Some(names) = self.names.as_mut() {
            for ((id, _), k) in group_by.iter().zip(&keys) {
                let d = names.disp(k, &child.layout, &self.subplans)?;
                if !matches!(d.kind, ExprKind::Column(ECol::Col(c)) if c == *id) {
                    names.define(*id, d);
                }
            }
            for ((id, call), pa) in calls.iter().zip(&paggs) {
                let args = pa
                    .args
                    .iter()
                    .map(|e| names.disp(e, &child.layout, &self.subplans))
                    .collect::<Result<Vec<_>>>()?;
                let filter = pa
                    .filter
                    .as_ref()
                    .map(|e| names.disp(e, &child.layout, &self.subplans))
                    .transpose()?;
                let mut order_by = Vec::with_capacity(pa.order_by.len());
                for k in &pa.order_by {
                    order_by.push(crate::expr::AggOrderKey {
                        expr: names.disp(&k.expr, &child.layout, &self.subplans)?,
                        descending: k.descending,
                        nulls_first: k.nulls_first,
                    });
                }
                let d = DExpr::new(
                    ExprKind::Aggregate(Box::new(crate::expr::AggCall {
                        func: call.func,
                        args,
                        distinct: call.distinct,
                        filter,
                        order_by,
                    })),
                    pa.result,
                    Span::default(),
                );
                names.define(*id, d);
            }
        }
        if group_by.is_empty() {
            self.agg_note("Aggregate", &[], &child.layout, &layout)?;
            return Ok(Phys {
                plan: PhysicalPlan::Aggregate {
                    input: Box::new(child.plan),
                    aggs: paggs,
                },
                layout,
            });
        }
        if self.env.settings.enable_hashagg {
            self.agg_note("HashAggregate", &keys, &child.layout, &layout)?;
            return Ok(Phys {
                plan: PhysicalPlan::HashAggregate {
                    input: Box::new(child.plan),
                    keys,
                    key_types,
                    aggs: paggs,
                },
                layout,
            });
        }
        let sort_keys: Vec<SortKey> = keys
            .iter()
            .map(|k| SortKey {
                expr: k.clone(),
                descending: false,
                nulls_first: false,
            })
            .collect();
        self.sort_note(&sort_keys, &child.layout)?;
        let sorted = PhysicalPlan::Sort {
            input: Box::new(child.plan),
            keys: sort_keys,
        };
        self.agg_note("GroupAggregate", &keys, &child.layout, &layout)?;
        Ok(Phys {
            plan: PhysicalPlan::GroupAggregate {
                input: Box::new(sorted),
                keys,
                key_types,
                aggs: paggs,
            },
            layout,
        })
    }

    fn agg_note(
        &mut self,
        title: &str,
        keys: &[PhysExpr],
        child_layout: &[ColId],
        layout: &[ColId],
    ) -> Result<()> {
        self.emit(|names, subs| {
            let mut details = Vec::new();
            if !keys.is_empty() {
                let t = keys
                    .iter()
                    .map(|k| names.expr(k, child_layout, QualifyRule::Upper, subs))
                    .collect::<Result<Vec<_>>>()?;
                details.push(NoteDetail::new("Group Key", t.join(", ")));
            }
            names.set_raw_aggs(true);
            let output = names.output_of(layout);
            names.set_raw_aggs(false);
            Ok(NodeNote {
                title: title.to_owned(),
                details,
                output: output?,
                width: names.width_of(layout),
                ..NodeNote::default()
            })
        })
    }

    // ----- 集合演算 -------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn phys_setop(
        &mut self,
        op: SetOpKind,
        all: bool,
        left: &LogicalPlan,
        right: &LogicalPlan,
        cols: &[ColId],
        left_cols: &[ColId],
        right_cols: &[ColId],
        params: &ParamMap,
    ) -> Result<Phys> {
        if op == SetOpKind::Union {
            let mut branches: Vec<(&LogicalPlan, &[ColId])> = Vec::new();
            collect_union(left, left_cols, all, &mut branches);
            collect_union(right, right_cols, all, &mut branches);
            // `Group Key` の列名は最初の枝の列から取る（PostgreSQL と同じ）。
            let first_cols: Vec<ColId> = branches.first().map_or_else(Vec::new, |b| b.1.to_vec());
            let mut inputs = Vec::with_capacity(branches.len());
            for (plan, want) in branches {
                let p = self.phys(plan, params)?;
                let p = self.ensure_layout(p, want)?;
                inputs.push(p.plan);
            }
            self.simple_note("Append".into(), Vec::new(), cols)?;
            let mut plan = PhysicalPlan::Append { inputs };
            if !all {
                let keys = self.col_text(&first_cols, QualifyRule::Upper)?;
                self.simple_note(
                    "HashAggregate".into(),
                    vec![NoteDetail::new("Group Key", keys.join(", "))],
                    cols,
                )?;
                plan = PhysicalPlan::Distinct {
                    input: Box::new(plan),
                };
            }
            return Ok(Phys {
                plan,
                layout: cols.to_vec(),
            });
        }
        let l = self.phys(left, params)?;
        let l = self.ensure_layout(l, left_cols)?;
        let r = self.phys(right, params)?;
        let r = self.ensure_layout(r, right_cols)?;
        let title = hash_setop_title(op, all)?;
        self.simple_note(title, Vec::new(), cols)?;
        Ok(Phys {
            plan: PhysicalPlan::HashSetOp {
                op,
                all,
                left: Box::new(l.plan),
                right: Box::new(r.plan),
                key_types: cols.iter().map(|c| self.arena.get(*c).ty).collect(),
            },
            layout: cols.to_vec(),
        })
    }

    // ----- 式 -----------------------------------------------------------------------

    /// 式を降ろす。`layout` は評価する行の列、`params` は自由な列のパラメータ。
    fn lower(&mut self, e: &LExpr, layout: &[ColId], params: &ParamMap) -> Result<PhysExpr> {
        e.try_map(&mut |n| match &n.kind {
            ExprKind::Column(c) => {
                let col = if let Some(i) = layout.iter().position(|x| x == c) {
                    PhysCol::Local(i)
                } else if let Some(p) = params.get(c) {
                    PhysCol::Param(*p)
                } else {
                    return Err(Error::internal(format!(
                        "unbound column #{} in the physical plan",
                        c.0
                    )));
                };
                Ok(Some(PhysExpr::new(ExprKind::Column(col), n.ty, n.span)))
            }
            ExprKind::Aggregate(_) => Err(Error::internal("an aggregate in a logical plan")),
            ExprKind::SubLink { kind, test, query } => {
                let id = self.plan_sublink(*kind, test.as_deref(), query, layout, params)?;
                Ok(Some(PhysExpr::new(
                    ExprKind::SubLink {
                        kind: *kind,
                        test: None,
                        query: id,
                    },
                    n.ty,
                    n.span,
                )))
            }
            _ => Ok(None),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn plan_sublink(
        &mut self,
        kind: SubLinkKind,
        test: Option<&LExpr>,
        sub: &LogicalSubquery,
        layout: &[ColId],
        params: &ParamMap,
    ) -> Result<SubPlanId> {
        let free = plan_free_cols(&sub.plan);
        let mut inner_params = ParamMap::new();
        let mut sp_params = Vec::new();
        for f in &free {
            let pid = self.new_param()?;
            let ty = self.arena.get(*f).ty;
            let outer = self.lower(&LExpr::column(*f, ty), layout, params)?;
            inner_params.insert(*f, pid);
            sp_params.push((pid, outer));
            if let Some(n) = self.names.as_mut() {
                n.bind_param(pid, ECol::Col(*f));
            }
        }
        let saved = std::mem::take(&mut self.pending);
        self.notes.push(Vec::new());
        let inner = self.phys(&sub.plan, &inner_params)?;
        let inner = self.ensure_layout(inner, &sub.output)?;
        let inner_notes = self.notes.pop().unwrap_or_default();
        self.pending = saved;
        let lowered_test = test.map(|t| self.lower(t, layout, params)).transpose()?;
        let strategy = if !free.is_empty() {
            SubPlanStrategy::Rescan
        } else if kind == SubLinkKind::Any
            && let Some(t) = test
            && let Some((probe_keys, build_keys)) = self.hashed_keys(t, layout, params)?
        {
            SubPlanStrategy::Hashed {
                probe_keys,
                build_keys,
            }
        } else {
            SubPlanStrategy::InitOnce
        };
        let id = u16::try_from(self.subplans.len()).map_err(|_| too_many("subqueries"))?;
        if let Some(n) = self.names.as_mut() {
            n.push_label(&strategy);
        }
        self.subplans.push(SubPlanDef {
            plan: inner.plan,
            kind,
            test: lowered_test,
            params: sp_params,
            strategy,
            explain: None,
        });
        self.sub_notes.push(inner_notes);
        let id = SubPlanId(id);
        if self.explaining() {
            self.pending.push(id);
        }
        Ok(id)
    }

    /// `ANY` の `test` がハッシュ化できるなら `(probe_keys, build_keys)`（§7.7.1 の 5）。
    fn hashed_keys(
        &mut self,
        test: &LExpr,
        layout: &[ColId],
        params: &ParamMap,
    ) -> Result<Option<(Vec<PhysExpr>, Vec<PhysExpr>)>> {
        let (mut probe, mut build) = (Vec::new(), Vec::new());
        let conj = conjuncts(test.clone());
        if conj.is_empty() {
            return Ok(None);
        }
        let has_out = |e: &LExpr| e.any(&mut |n| matches!(n.kind, ExprKind::SubLinkOutput(_)));
        for c in &conj {
            let ExprKind::Operator { op, args } = &c.kind else {
                return Ok(None);
            };
            if op.name != "=" || args.len() != 2 || !builtin::operator_merge_hash(op.oid).1 {
                return Ok(None);
            }
            let (inner_e, outer_e) = match (has_out(&args[0]), has_out(&args[1])) {
                (true, false) => (&args[0], &args[1]),
                (false, true) => (&args[1], &args[0]),
                _ => return Ok(None),
            };
            let cat = self.env.catalog;
            let (ie, oe) = if inner_e.ty.oid == outer_e.ty.oid {
                (inner_e.clone(), outer_e.clone())
            } else if let Some(o) = coerce_expr(outer_e.clone(), inner_e.ty, cat) {
                (inner_e.clone(), o)
            } else if let Some(i) = coerce_expr(inner_e.clone(), outer_e.ty, cat) {
                (i, outer_e.clone())
            } else {
                return Ok(None);
            };
            probe.push(self.lower(&oe, layout, params)?);
            let b = self.lower(&ie, &[], params)?;
            let b = b.try_rewrite(&mut |n| match n.kind {
                ExprKind::SubLinkOutput(i) => {
                    Ok(Some(PhysExpr::column(PhysCol::Local(usize::from(i)), n.ty)))
                }
                _ => Ok(None),
            })?;
            build.push(b);
        }
        Ok(Some((probe, build)))
    }

    /// `p` の列の並びを `want` にそろえる。違えば `Project` を足す。
    fn ensure_layout(&mut self, p: Phys, want: &[ColId]) -> Result<Phys> {
        if p.layout == want {
            return Ok(p);
        }
        let exprs = want
            .iter()
            .map(|w| {
                let pos = p.layout.iter().position(|c| c == w).ok_or_else(|| {
                    Error::internal(format!("column #{} is not in the plan's output", w.0))
                })?;
                Ok(PhysExpr::column(PhysCol::Local(pos), self.arena.get(*w).ty))
            })
            .collect::<Result<Vec<_>>>()?;
        self.emit(|names, _| {
            Ok(NodeNote {
                title: "Project".into(),
                output: names.output_of(want)?,
                width: names.width_of(want),
                ..NodeNote::default()
            })
        })?;
        Ok(Phys {
            plan: PhysicalPlan::Project {
                input: Box::new(p.plan),
                exprs,
            },
            layout: want.to_vec(),
        })
    }
}

/// `UNION` の枝を平坦化して集める（入れ子の `UNION ALL` は常に、外側が重複除去なら入れ子の `UNION` も）。
fn collect_union<'p>(
    p: &'p LogicalPlan,
    want: &'p [ColId],
    outer_all: bool,
    out: &mut Vec<(&'p LogicalPlan, &'p [ColId])>,
) {
    if let LogicalPlan::SetOp {
        op: SetOpKind::Union,
        all,
        left,
        right,
        left_cols,
        right_cols,
        ..
    } = p
        && (*all || !outer_all)
    {
        collect_union(left, left_cols, outer_all, out);
        collect_union(right, right_cols, outer_all, out);
        return;
    }
    out.push((p, want));
}

/// `exprs` が入力の `width` 列をそのまま並べるだけか（`Column(Local(0..width))`）。
fn is_identity(exprs: &[PhysExpr], width: usize) -> bool {
    exprs.len() == width
        && exprs
            .iter()
            .enumerate()
            .all(|(i, e)| matches!(e.kind, ExprKind::Column(PhysCol::Local(p)) if p == i))
}

/// CHECK は名前のバイト列順に評価する（PostgreSQL と同じ）。
fn sorted_checks(checks: &[PhysCheck]) -> Vec<PhysCheck> {
    let mut v = checks.to_vec();
    v.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    v
}

#[cfg(test)]
mod tests {
    use super::super::print::print_physical;
    use super::super::rules::testutil::Fixture;
    use std::fmt::Write as _;

    use super::super::physical::ExplainNode;
    use super::super::plan;

    const SCHEMA: &str = "create table t(a int4, b int4 unique, c text);
        create table u(a int4, d int4, e text);
        create table p(id int4 primary key, v int4);";

    fn fixture() -> Fixture {
        let f = Fixture::new(SCHEMA).expect("schema");
        f.set_nblocks("t", 100).expect("nblocks");
        f.set_nblocks("u", 10).expect("nblocks");
        f.set_nblocks("p", 1000).expect("nblocks");
        f
    }

    fn explain(f: &Fixture, sql: &str, verbose: bool) -> String {
        let stmt = f.analyze(sql).expect("analyze");
        let q = f
            .with_env(|env| {
                plan(
                    &stmt,
                    &super::super::PlanEnv {
                        want_explain: true,
                        explain_verbose: verbose,
                        ..*env
                    },
                )
            })
            .expect("plan");
        let mut out = String::new();
        walk(q.explain.as_ref().expect("explain"), 0, &mut out);
        out
    }

    fn walk(n: &ExplainNode, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        let _ = writeln!(out, "{pad}{}", n.title);
        for d in &n.details {
            let _ = writeln!(out, "{pad}  {}: {}", d.label, d.text);
        }
        for o in &n.output {
            let _ = writeln!(out, "{pad}  Output: {o}");
        }
        for c in &n.children {
            if let Some(l) = &c.label {
                let _ = writeln!(out, "{pad}  {l}");
            }
            walk(&c.node, depth + 1, out);
        }
    }

    fn phys_text(f: &Fixture, sql: &str) -> String {
        let stmt = f.analyze(sql).expect("analyze");
        match f.with_env(|env| plan(&stmt, env)) {
            Ok(q) => print_physical(&q),
            Err(e) => format!("ERROR {} {}\n", e.sqlstate.code(), e.message),
        }
    }

    fn assert_lines(got: &str, want: &[&str]) {
        let got: Vec<&str> = got.lines().collect();
        assert_eq!(got, want, "\n{}", got.join("\n"));
    }

    #[test]
    fn index_scan_keeps_every_predicate_in_the_filter() {
        let f = fixture();
        assert_lines(
            &phys_text(&f, "select * from t where b = 3 and a > 1"),
            &["IndexScan t_b_key Forward cols=3 keys=[= 3] filter=((@1 = 3) AND (@0 > 1))"],
        );
        assert_eq!(
            explain(&f, "select * from t where b = 3 and a > 1", false),
            "Index Scan using t_b_key on t\n  Index Cond: (b = 3)\n  Filter: (a > 1)\n"
        );
        // 逆向きの比較は交換子で列を左にそろえる。
        assert!(phys_text(&f, "select * from t where 3 < b").contains("keys=[> 3]"));
        assert!(phys_text(&f, "select * from t where b is null").contains("keys=[IS NULL]"));
    }

    #[test]
    fn enable_flags_choose_the_scan() {
        let mut f = fixture();
        f.settings.enable_indexscan = false;
        assert!(phys_text(&f, "select * from t where b = 3").starts_with("SeqScan"));
        f.settings.enable_indexscan = true;
        f.settings.enable_seqscan = false;
        assert!(phys_text(&f, "select * from t where b = 3").starts_with("IndexScan"));
        // 候補がなければ設定に関わらず Seq Scan。
        assert!(phys_text(&f, "select * from t where a = 3").starts_with("SeqScan"));
    }

    #[test]
    fn hash_join_builds_the_smaller_side() {
        let f = fixture();
        let t = phys_text(&f, "select t.c from t join u on t.a = u.a");
        assert!(t.contains("HashJoin Inner"), "{t}");
        let e = explain(&f, "select t.c from t join u on t.a = u.a", false);
        // u（10 ブロック）が Hash の下。
        assert!(e.contains("Hash Join\n  Hash Cond: (t.a = u.a)\n"), "{e}");
        assert!(e.contains("  Hash\n    Seq Scan on u\n"), "{e}");
    }

    #[test]
    fn join_algorithm_follows_the_settings() {
        let mut f = fixture();
        f.settings.enable_hashjoin = false;
        let t = phys_text(&f, "select t.c from t join u on t.a = u.a");
        assert!(t.contains("NestedLoop Inner"), "{t}");
        assert!(t.contains("Materialize"), "{t}");
        f.settings.enable_material = false;
        assert!(!phys_text(&f, "select t.c from t join u on t.a = u.a").contains("Materialize"));
        f.settings.enable_nestloop = false;
        assert!(phys_text(&f, "select t.c from t join u on t.a = u.a").contains("NestedLoop"));
        // キーのない結合は常に Nested Loop。
        f.settings.enable_hashjoin = true;
        assert!(phys_text(&f, "select 1 from t, u").contains("NestedLoop"));
    }

    #[test]
    fn full_join_needs_a_hashable_equality() {
        let f = fixture();
        let t = phys_text(&f, "select * from t full join u on t.a < u.a");
        assert!(
            t.starts_with(
                "ERROR 0A000 FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
            ),
            "{t}"
        );
        assert!(
            phys_text(&f, "select * from t full join u on t.a = u.a").contains("HashJoin Full")
        );
    }

    #[test]
    fn inner_index_scan_nested_loop_and_swap() {
        let f = fixture();
        f.set_nblocks("u", 1).expect("nblocks");
        let t = phys_text(&f, "select u.d, p.v from u join p on p.id = u.a");
        assert!(t.contains("NestedLoopParam Inner params=[$0 := @0]"), "{t}");
        assert!(t.contains("IndexScan p_pkey Forward"), "{t}");
        // 出力の並びは論理の左 ++ 右（入れ替えたときは Project で戻す）。
        let sw = phys_text(&f, "select p.v, u.d from p join u on p.id = u.a");
        assert!(sw.contains("NestedLoopParam Inner"), "{sw}");
        let e = explain(&f, "select u.d, p.v from u join p on p.id = u.a", false);
        assert!(e.contains("Nested Loop\n"), "{e}");
        assert!(e.contains("Index Scan using p_pkey on p\n"), "{e}");
        assert!(e.contains("Index Cond: (id = u.a)"), "{e}");
        // 大きい外側では使わない。
        f.set_nblocks("u", 1000).expect("nblocks");
        assert!(
            !phys_text(&f, "select u.d, p.v from u join p on p.id = u.a")
                .contains("NestedLoopParam")
        );
    }

    #[test]
    fn aggregates_and_distinct() {
        let mut f = fixture();
        let t = phys_text(&f, "select a, count(*) from t group by a");
        assert!(
            t.starts_with("HashAggregate keys=[@0] aggs=[CountStar()]"),
            "{t}"
        );
        assert!(phys_text(&f, "select count(*) from t").starts_with("Aggregate"));
        f.settings.enable_hashagg = false;
        let g = phys_text(&f, "select a, count(*) from t group by a");
        assert!(g.starts_with("GroupAggregate"), "{g}");
        assert!(g.contains("  Sort [@0]"), "{g}");
        let d = phys_text(&f, "select distinct a from t");
        assert!(d.starts_with("Distinct"), "{d}");
        let on = phys_text(&f, "select distinct on (a) a, b from t order by a, b");
        assert!(on.starts_with("Unique [0]"), "{on}");
    }

    #[test]
    fn set_operations_flatten_unions() {
        let f = fixture();
        let t = phys_text(
            &f,
            "select a from t union all select a from u union all select a from t",
        );
        assert_eq!(t.matches("Append").count(), 1, "{t}");
        let t = phys_text(&f, "select a from t union select a from u");
        assert!(t.starts_with("Distinct\n  Append"), "{t}");
        let t = phys_text(&f, "select a from t intersect all select a from u");
        assert!(t.starts_with("HashSetOp Intersect all=true"), "{t}");
        let e = explain(&f, "select a from t except select a from u", false);
        assert!(e.starts_with("HashSetOp Except\n"), "{e}");
    }

    #[test]
    fn sublinks_are_classified() {
        let f = fixture();
        let t = phys_text(&f, "select a from t where b > (select min(d) from u)");
        assert!(t.contains("SubPlan 0 Scalar init"), "{t}");
        let t = phys_text(
            &f,
            "select a from t where a = (select max(a) from u where u.d = t.b)",
        );
        assert!(
            t.contains("SubPlan 0 Scalar rescan params=[$0 := @1]"),
            "{t}"
        );
        // 相関のない ANY で等値はハッシュ化できる（ここでは Semi Join に書き換わらない形）。
        let t = phys_text(&f, "select a from t where a not in (select d from u)");
        assert!(t.contains("hashed probe="), "{t}");
        let e = explain(
            &f,
            "select a from t where a not in (select d from u)",
            false,
        );
        assert!(e.contains("(hashed SubPlan 1)"), "{e}");
        let e = explain(
            &f,
            "select a from t where b > (select min(d) from u)",
            false,
        );
        assert!(e.contains("(InitPlan 1).col1"), "{e}");
    }

    #[test]
    fn ctes_are_planned_once() {
        let f = fixture();
        let t = phys_text(
            &f,
            "with c as materialized (select a from t) select * from c, c c2",
        );
        assert_eq!(t.matches("CteScan 0").count(), 2, "{t}");
        assert!(t.contains("\nCTE 0\n"), "{t}");
        // 参照が 1 つの CTE はインライン展開される。
        assert!(!phys_text(&f, "with c as (select a from t) select * from c").contains("CteScan"));
    }

    #[test]
    fn dml_has_the_documented_shape() {
        let f = fixture();
        let t = phys_text(&f, "update t set a = 1 where b = 2");
        assert!(t.starts_with("Update t assigned=[(0, 4)]"), "{t}");
        assert!(t.contains("IndexScan t_b_key"), "{t}");
        assert!(phys_text(&f, "delete from u where d = 1").starts_with("Delete cols=3"));
        assert!(phys_text(&f, "insert into u values (1, 2, 'x')").starts_with("Insert u"));
    }

    #[test]
    fn explain_names_follow_the_qualification_rules() {
        let f = fixture();
        let e = explain(&f, "select a from t where a > 1", false);
        assert_eq!(e, "Seq Scan on t\n  Filter: (a > 1)\n");
        let e = explain(
            &f,
            "select a, count(*) from t group by a having count(*) > 1",
            false,
        );
        assert!(e.contains("Filter: (count(*) > 1)"), "{e}");
        let v = explain(&f, "select t.c from t where a > 1", true);
        assert!(v.contains("Seq Scan on public.t"), "{v}");
        assert!(v.contains("Filter: (t.a > 1)"), "{v}");
    }
}
