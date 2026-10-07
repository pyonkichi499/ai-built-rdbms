//! SubPlan / InitPlan / CTE の実行時の状態（`m4/02-pipeline-refactor.md` §3.7.3、
//! `m4/05-executor.md` §3.3）。
//!
//! 型と Executor の貸し借り（`take` / `put_back`）、SubLink の評価（`eval_sublink`）。型は 05 の
//! `SubPlanStates` / `HashedSubPlan` / `CteStates` に従う
//! （11 §7.1 の C-10）。

// X1・X2 が使うまで未使用の型がある。
#![allow(dead_code)]
#![allow(clippy::doc_markdown)]

use std::cmp::Ordering;
use std::collections::HashSet;
use std::rc::Rc;

use super::eval::{eval, eval_with_sub_row};
use super::instrument::{InstrBuild, Instrumentation};
use super::mem::estimate_row_bytes;
use super::{BoxedExecutor, ExecCtx, build};
use crate::error::{Error, Result, sqlstate};
use crate::expr::{SubLinkKind, SubPlanId};
use crate::planner::physical::{PhysExpr, PhysicalQuery, SubPlanDef, SubPlanStrategy};
use crate::types::hash::HashKey;
use crate::types::{Datum, Row, cmp_datum};

/// ハッシュ表のスロット・バケットの添字・印の概算（`05 §8.1`）。
const HASH_ENTRY_OVERHEAD: usize = 48;

/// 子 Executor の状態。`Unbuilt` は最初に使うときに `build` する（遅延）。
pub(crate) enum SlotExec {
    Unbuilt,
    Idle(BoxedExecutor),
    /// 貸し出し中（評価中の再入の検出用）。
    Lent,
}

impl std::fmt::Debug for SlotExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SlotExec::Unbuilt => "Unbuilt",
            SlotExec::Idle(_) => "Idle",
            SlotExec::Lent => "Lent",
        })
    }
}

/// `InitOnce` / `Hashed` が保持する結果。
#[derive(Debug)]
pub(crate) enum SubPlanCache {
    None,
    /// `InitOnce` の Scalar。
    Scalar(Datum),
    /// `InitOnce` の Exists。
    Exists(bool),
    /// `InitOnce` の Any / All（全行を保持）。
    Rows {
        rows: Vec<Row>,
    },
    Hashed(HashedSubPlan),
}

/// ハッシュ化 SubPlan（`05 §4.2.5` の正確な三値論理）。
#[derive(Debug, Default)]
pub struct HashedSubPlan {
    pub n_keys: usize,
    /// キー列がすべて非 NULL の行。
    pub set: HashSet<HashKey>,
    /// NULL を含むキー行。
    pub null_rows: Vec<Vec<Datum>>,
    /// `n_keys >= 2` のときだけ。非 NULL のキー行（NULL を含む probe の部分一致走査用）。
    pub full_rows: Vec<Vec<Datum>>,
}

#[derive(Debug)]
pub(crate) struct SubPlanSlot {
    pub(crate) exec: SlotExec,
    pub(crate) started: bool,
    pub(crate) cache: SubPlanCache,
}

/// `PhysicalQuery.subplans` の実行時の状態。添字 = `SubPlanId`。
#[derive(Debug, Default)]
pub struct SubPlanStates {
    slots: Vec<SubPlanSlot>,
}

impl SubPlanStates {
    /// スロットだけ作る（Executor は最初に使うときに作る）。
    pub fn new(query: &PhysicalQuery) -> SubPlanStates {
        SubPlanStates {
            slots: query
                .subplans
                .iter()
                .map(|_| SubPlanSlot {
                    exec: SlotExec::Unbuilt,
                    started: false,
                    cache: SubPlanCache::None,
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Executor を借りる。`Ok(None)` は未構築（呼び出し側が `build` して `put_exec` する）。
    /// 貸し出し中なら `Error::internal`（副問い合わせが自分自身を評価している）。
    pub(crate) fn take_exec(&mut self, id: SubPlanId) -> Result<Option<BoxedExecutor>> {
        let slot = self
            .slots
            .get_mut(usize::from(id.0))
            .ok_or_else(|| Error::internal("SubPlan id out of range"))?;
        match std::mem::replace(&mut slot.exec, SlotExec::Lent) {
            SlotExec::Idle(e) => Ok(Some(e)),
            SlotExec::Unbuilt => Ok(None),
            SlotExec::Lent => Err(Error::internal("SubPlan re-entered")),
        }
    }

    /// Executor を借りる（05 §3.3）。未構築なら `SubPlanDef.plan` から作る（`rewindable = true`）。戻り値の
    /// `bool` は「一度でも実行を始めたか」（`rewind` が要るか）。貸し出し中なら `Error::internal`。
    pub(crate) fn take(
        &mut self,
        id: SubPlanId,
        query: &PhysicalQuery,
        instr: Option<&Rc<Instrumentation>>,
    ) -> Result<(BoxedExecutor, bool)> {
        let started = self.slots.get(usize::from(id.0)).is_some_and(|s| s.started);
        if let Some(exec) = self.take_exec(id)? {
            return Ok((exec, started));
        }
        let def = query
            .subplans
            .get(usize::from(id.0))
            .ok_or_else(|| Error::internal("SubPlan id out of range"))?;
        let ib = instr.map(|i| InstrBuild::new(query, i));
        let env = build::BuildEnv {
            query: Some(query),
            instr: ib.as_ref(),
        };
        Ok((build::build_scoped(&def.plan, &env, true), false))
    }

    /// `take` で借りた Executor を戻す。
    pub(crate) fn put_back(&mut self, id: SubPlanId, exec: BoxedExecutor, started: bool) {
        self.put_exec(id, exec);
        if let Some(slot) = self.slots.get_mut(usize::from(id.0)) {
            slot.started = started;
        }
    }

    /// 借りた Executor を戻す（成功・失敗どちらでも）。
    pub(crate) fn put_exec(&mut self, id: SubPlanId, exec: BoxedExecutor) {
        if let Some(slot) = self.slots.get_mut(usize::from(id.0)) {
            slot.exec = SlotExec::Idle(exec);
        }
    }
}

/// CTE 1 つの実行時の状態。
#[derive(Debug)]
pub(crate) struct CteSlot {
    pub(crate) exec: SlotExec,
    pub(crate) rows: Vec<Row>,
    pub(crate) done: bool,
}

/// `PhysicalQuery.ctes` の実行時の状態（`CteScan` の持ち主は X2）。
#[derive(Debug, Default)]
pub struct CteStates {
    /// `CteScan`（X2）が読み書きする。
    pub(crate) slots: Vec<CteSlot>,
}

impl CteStates {
    pub fn new(query: &PhysicalQuery) -> CteStates {
        CteStates {
            slots: query
                .ctes
                .iter()
                .map(|_| CteSlot {
                    exec: SlotExec::Unbuilt,
                    rows: Vec::new(),
                    done: false,
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// `Datum` の真偽値（NULL は `None`）。
fn datum_bool(d: &Datum) -> Result<Option<bool>> {
    match d {
        Datum::Null => Ok(None),
        Datum::Bool(b) => Ok(Some(*b)),
        other => Err(Error::internal(format!(
            "expected a boolean value, got {other:?}"
        ))),
    }
}

fn bool_datum(b: Option<bool>) -> Datum {
    b.map_or(Datum::Null, Datum::Bool)
}

fn too_many_rows() -> Error {
    Error::new(
        sqlstate::CARDINALITY_VIOLATION,
        "more than one row returned by a subquery used as an expression",
    )
}

/// SubLink 1 つを評価する（`row` は SubLink が置かれた式の文脈行。`05 §4.2`）。
pub fn eval_sublink(id: SubPlanId, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum> {
    let query = ctx.query;
    let def = query
        .subplans
        .get(usize::from(id.0))
        .ok_or_else(|| Error::internal("SubPlan id out of range"))?;
    match &def.strategy {
        SubPlanStrategy::Rescan => eval_rescan(id, def, row, ctx),
        SubPlanStrategy::InitOnce => eval_init_once(id, def, row, ctx),
        SubPlanStrategy::Hashed {
            probe_keys,
            build_keys,
        } => eval_hashed(id, def, probe_keys, build_keys, row, ctx),
    }
}

/// `take` して `f` を呼び、成功・失敗どちらでも `put_back` する（`05 §4.2.1`）。
fn with_subplan<R>(
    ctx: &mut ExecCtx<'_>,
    id: SubPlanId,
    f: impl FnOnce(&mut BoxedExecutor, &mut bool, &mut ExecCtx<'_>) -> Result<R>,
) -> Result<R> {
    let query = ctx.query;
    let (mut exec, mut started) = ctx.subplans.take(id, query, ctx.instr.as_ref())?;
    let r = f(&mut exec, &mut started, ctx);
    ctx.subplans.put_back(id, exec, started);
    r
}

/// 一度でも始めていれば先頭に戻し、開始済みにする。
fn restart(exec: &mut BoxedExecutor, started: &mut bool, ctx: &mut ExecCtx<'_>) -> Result<()> {
    if *started {
        exec.rewind(ctx)?;
    }
    *started = true;
    Ok(())
}

fn require_test(def: &SubPlanDef) -> Result<&PhysExpr> {
    def.test
        .as_ref()
        .ok_or_else(|| Error::internal("Any/All SubLink without a test expression"))
}

/// 相関のある副問い合わせ: 外側の行ごとに `params` を設定して `rewind` し、実行する。
fn eval_rescan(id: SubPlanId, def: &SubPlanDef, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum> {
    // 先に全部評価してから代入する（式が `Param` を読みうる）。
    let vals = def
        .params
        .iter()
        .map(|(_, e)| eval(e, row, ctx))
        .collect::<Result<Vec<_>>>()?;
    for ((p, _), v) in def.params.iter().zip(vals) {
        ctx.set_param(*p, v)?;
    }
    with_subplan(ctx, id, |exec, started, ctx| {
        restart(exec, started, ctx)?;
        match def.kind {
            SubLinkKind::Scalar => read_scalar(exec, ctx),
            SubLinkKind::Exists => Ok(Datum::Bool(exec.next(ctx)?.is_some())),
            SubLinkKind::Any | SubLinkKind::All => {
                let test = require_test(def)?;
                let mut acc = Quantifier::new(def.kind);
                while let Some(r) = exec.next(ctx)? {
                    ctx.check_interrupts()?;
                    if acc.push(datum_bool(&eval_with_sub_row(test, row, &r, ctx)?)?) {
                        break;
                    }
                }
                Ok(acc.finish())
            }
        }
    })
}

/// Scalar: 0 行は NULL、2 行以上は 21000。
fn read_scalar(exec: &mut BoxedExecutor, ctx: &mut ExecCtx<'_>) -> Result<Datum> {
    let Some(first) = exec.next(ctx)? else {
        return Ok(Datum::Null);
    };
    let v = first
        .into_iter()
        .next()
        .ok_or_else(|| Error::internal("scalar subquery returned a row without columns"))?;
    if exec.next(ctx)?.is_some() {
        return Err(too_many_rows());
    }
    Ok(v)
}

/// Any / All の三値論理の畳み込み（`05 §4.2.3`）。
struct Quantifier {
    /// Any なら true、All なら false を見たら確定する。
    decisive: bool,
    saw_null: bool,
    decided: bool,
}

impl Quantifier {
    fn new(kind: SubLinkKind) -> Self {
        Quantifier {
            decisive: kind == SubLinkKind::Any,
            saw_null: false,
            decided: false,
        }
    }

    /// 行 1 つの `test` の結果を足す。結果が確定したら `true`（これ以上読まない）。
    fn push(&mut self, t: Option<bool>) -> bool {
        match t {
            Some(b) if b == self.decisive => self.decided = true,
            Some(_) => {}
            None => self.saw_null = true,
        }
        self.decided
    }

    fn finish(&self) -> Datum {
        if self.decided {
            Datum::Bool(self.decisive)
        } else if self.saw_null {
            Datum::Null
        } else {
            Datum::Bool(!self.decisive)
        }
    }
}

/// 非相関: 1 回だけ実行して結果を保持する（`05 §4.2.4`）。
fn eval_init_once(
    id: SubPlanId,
    def: &SubPlanDef,
    row: &Row,
    ctx: &mut ExecCtx<'_>,
) -> Result<Datum> {
    let slot = usize::from(id.0);
    if matches!(ctx.subplans.slots[slot].cache, SubPlanCache::None) {
        let cache = with_subplan(ctx, id, |exec, started, ctx| {
            restart(exec, started, ctx)?;
            Ok(match def.kind {
                SubLinkKind::Scalar => SubPlanCache::Scalar(read_scalar(exec, ctx)?),
                SubLinkKind::Exists => SubPlanCache::Exists(exec.next(ctx)?.is_some()),
                SubLinkKind::Any | SubLinkKind::All => {
                    let mut rows = Vec::new();
                    while let Some(r) = exec.next(ctx)? {
                        ctx.check_interrupts()?;
                        ctx.mem.charge(estimate_row_bytes(&r))?;
                        rows.push(r);
                    }
                    SubPlanCache::Rows { rows }
                }
            })
        })?;
        ctx.subplans.slots[slot].cache = cache;
    }
    match &ctx.subplans.slots[slot].cache {
        SubPlanCache::Scalar(v) => Ok(v.clone()),
        SubPlanCache::Exists(b) => Ok(Datum::Bool(*b)),
        SubPlanCache::Rows { .. } => {
            let test = require_test(def)?;
            // 溜めた行を借りて評価する（`ctx` を可変借用するため、いったん取り出して戻す）。
            let SubPlanCache::Rows { rows } = &mut ctx.subplans.slots[slot].cache else {
                return Err(Error::internal("InitOnce cache changed"));
            };
            let rows = std::mem::take(rows);
            let r = quantify_rows(def.kind, test, &rows, row, ctx);
            ctx.subplans.slots[slot].cache = SubPlanCache::Rows { rows };
            r
        }
        SubPlanCache::None | SubPlanCache::Hashed(_) => {
            Err(Error::internal("InitOnce cache holds an unexpected value"))
        }
    }
}

fn quantify_rows(
    kind: SubLinkKind,
    test: &PhysExpr,
    rows: &[Row],
    row: &Row,
    ctx: &mut ExecCtx<'_>,
) -> Result<Datum> {
    let mut acc = Quantifier::new(kind);
    for r in rows {
        ctx.check_interrupts()?;
        if acc.push(datum_bool(&eval_with_sub_row(test, row, r, ctx)?)?) {
            break;
        }
    }
    Ok(acc.finish())
}

/// 全キー列について、probe が NULL・行が NULL・等しい、のどれか（確定した不一致の列がない）。
fn partial_match(probe: &[Datum], row: &[Datum]) -> bool {
    probe
        .iter()
        .zip(row)
        .all(|(p, r)| p.is_null() || r.is_null() || cmp_datum(p, r) == Ordering::Equal)
}

/// ハッシュ化 SubPlan の構築（最初の評価で 1 回）。
fn build_hashed(
    id: SubPlanId,
    build_keys: &[PhysExpr],
    ctx: &mut ExecCtx<'_>,
) -> Result<HashedSubPlan> {
    with_subplan(ctx, id, |exec, started, ctx| {
        restart(exec, started, ctx)?;
        let mut h = HashedSubPlan {
            n_keys: build_keys.len(),
            ..HashedSubPlan::default()
        };
        while let Some(r) = exec.next(ctx)? {
            ctx.check_interrupts()?;
            let key = build_keys
                .iter()
                .map(|e| eval(e, &r, ctx))
                .collect::<Result<Vec<_>>>()?;
            ctx.mem
                .charge(estimate_row_bytes(&key) + HASH_ENTRY_OVERHEAD)?;
            if key.iter().any(Datum::is_null) {
                h.null_rows.push(key);
            } else {
                if h.n_keys >= 2 {
                    h.full_rows.push(key.clone());
                }
                h.set.insert(HashKey(key));
            }
        }
        Ok(h)
    })
}

/// 非相関の `IN`: ハッシュ集合で探す。PostgreSQL の `ExecHashSubPlan` と同じ三値論理（`05 §4.2.5`）。
fn eval_hashed(
    id: SubPlanId,
    def: &SubPlanDef,
    probe_keys: &[PhysExpr],
    build_keys: &[PhysExpr],
    row: &Row,
    ctx: &mut ExecCtx<'_>,
) -> Result<Datum> {
    if def.kind != SubLinkKind::Any {
        return Err(Error::internal("hashed SubPlan must be an ANY sublink"));
    }
    let slot = usize::from(id.0);
    if matches!(ctx.subplans.slots[slot].cache, SubPlanCache::None) {
        let h = build_hashed(id, build_keys, ctx)?;
        ctx.subplans.slots[slot].cache = SubPlanCache::Hashed(h);
    }
    let probe = probe_keys
        .iter()
        .map(|e| eval(e, row, ctx))
        .collect::<Result<Vec<_>>>()?;
    let SubPlanCache::Hashed(h) = &ctx.subplans.slots[slot].cache else {
        return Err(Error::internal("hashed SubPlan cache holds another value"));
    };
    Ok(bool_datum(h.lookup(&probe)))
}

impl HashedSubPlan {
    /// `probe IN (集合)` の三値論理（NULL は `None`）。
    fn lookup(&self, probe: &[Datum]) -> Option<bool> {
        if self.set.is_empty() && self.null_rows.is_empty() {
            return Some(false);
        }
        let probe_has_null = probe.iter().any(Datum::is_null);
        let null_partial = || self.null_rows.iter().any(|r| partial_match(probe, r));
        if !probe_has_null {
            if self.set.contains(&HashKey(probe.to_vec())) {
                return Some(true);
            }
            return if null_partial() { None } else { Some(false) };
        }
        let partial = if self.n_keys <= 1 {
            // 集合か NULL 行のどちらかが空でない（上で確認済み）ので、NULL の probe は必ず部分一致する。
            true
        } else {
            self.full_rows.iter().any(|r| partial_match(probe, r)) || null_partial()
        };
        if partial { None } else { Some(false) }
    }
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps, clippy::type_complexity)]
mod tests {
    use super::*;
    use crate::executor::{ExecCtx, Executor};

    struct Nop;
    impl Executor for Nop {
        fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
            Ok(None)
        }
        fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn take_and_put_back_detect_reentry() {
        let mut q = PhysicalQuery::empty();
        q.subplans.push(crate::planner::physical::SubPlanDef {
            plan: PhysicalQuery::empty().root,
            kind: crate::expr::SubLinkKind::Exists,
            test: None,
            params: vec![],
            strategy: crate::planner::physical::SubPlanStrategy::InitOnce,
            explain: None,
        });
        let mut s = SubPlanStates::new(&q);
        assert_eq!(s.len(), 1);
        let id = SubPlanId(0);
        // 未構築 → None（貸し出し中になる）。
        assert!(s.take_exec(id).unwrap().is_none());
        match s.take_exec(id) {
            Err(e) => assert_eq!(e.message, "SubPlan re-entered"),
            Ok(_) => panic!("expected a re-entry error"),
        }
        s.put_exec(id, Box::new(Nop));
        assert!(s.take_exec(id).unwrap().is_some());
        assert!(s.take_exec(SubPlanId(7)).is_err());
        assert!(CteStates::new(&q).is_empty());
    }

    fn one_subplan_query(kind: crate::expr::SubLinkKind) -> PhysicalQuery {
        use crate::planner::physical::{PhysicalPlan, SubPlanDef, SubPlanStrategy};
        let mut q = PhysicalQuery::empty();
        q.subplans.push(SubPlanDef {
            plan: PhysicalPlan::Result {
                exprs: vec![],
                one_time_filter: None,
            },
            kind,
            test: None,
            params: vec![],
            strategy: SubPlanStrategy::InitOnce,
            explain: None,
        });
        q
    }

    #[test]
    fn take_builds_lazily_and_put_back_keeps_started() {
        let q = one_subplan_query(crate::expr::SubLinkKind::Exists);
        let mut s = SubPlanStates::new(&q);
        let id = SubPlanId(0);
        let (exec, started) = s.take(id, &q, None).unwrap();
        assert!(!started);
        // 貸し出し中の再入は `XX000`。
        assert_eq!(
            s.take(id, &q, None).err().unwrap().message,
            "SubPlan re-entered"
        );
        s.put_back(id, exec, true);
        let (_, started) = s.take(id, &q, None).unwrap();
        assert!(started);
        assert!(s.take(SubPlanId(3), &q, None).is_err());
    }

    // ---- eval_sublink（05 §10.3）----

    use crate::catalog::BuiltinOperator;
    use crate::error::Span;
    use crate::executor::eval::tests::{EQ, col, int, null, op, param};
    use crate::executor::nodes::test_util::{CountingExec, Fixture};
    use crate::expr::{ExprKind, ParamId};
    use crate::planner::physical::PhysicalPlan;
    use crate::types::{SqlType, oid};

    fn ne_fn(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(cmp_datum(&a[0], &a[1]) != Ordering::Equal))
    }
    fn lt_fn(a: &[Datum]) -> Result<Datum> {
        Ok(Datum::Bool(cmp_datum(&a[0], &a[1]) == Ordering::Less))
    }
    static NE: BuiltinOperator = BuiltinOperator {
        oid: 518,
        name: "<>",
        left: Some(oid::INT4),
        right: oid::INT4,
        result: oid::BOOL,
        func: ne_fn,
    };
    static LT: BuiltinOperator = BuiltinOperator {
        oid: 97,
        name: "<",
        left: Some(oid::INT4),
        right: oid::INT4,
        result: oid::BOOL,
        func: lt_fn,
    };

    fn mk(kind: ExprKind<crate::expr::PhysCol, SubPlanId>, ty: SqlType) -> PhysExpr {
        PhysExpr::new(kind, ty, Span::default())
    }

    fn sublink(kind: SubLinkKind, id: u16) -> PhysExpr {
        let ty = if kind == SubLinkKind::Scalar {
            SqlType::INT4
        } else {
            SqlType::BOOL
        };
        mk(
            ExprKind::SubLink {
                kind,
                test: None,
                query: SubPlanId(id),
            },
            ty,
        )
    }

    fn not(e: PhysExpr) -> PhysExpr {
        mk(ExprKind::Not(Box::new(e)), SqlType::BOOL)
    }

    fn sub_out(i: u16) -> PhysExpr {
        mk(ExprKind::SubLinkOutput(i), SqlType::INT4)
    }

    fn lit_i(v: Option<i32>) -> PhysExpr {
        v.map_or_else(|| null(SqlType::INT4), int)
    }

    fn values_plan(rows: &[Option<i32>]) -> PhysicalPlan {
        PhysicalPlan::Values {
            rows: rows.iter().map(|v| vec![lit_i(*v)]).collect(),
        }
    }

    fn def(
        plan: PhysicalPlan,
        kind: SubLinkKind,
        test: Option<PhysExpr>,
        params: Vec<(ParamId, PhysExpr)>,
        strategy: SubPlanStrategy,
    ) -> SubPlanDef {
        SubPlanDef {
            plan,
            kind,
            test,
            params,
            strategy,
            explain: None,
        }
    }

    fn query_of(defs: Vec<SubPlanDef>) -> PhysicalQuery {
        let mut q = PhysicalQuery::empty();
        q.subplans = defs;
        q
    }

    fn opt_datum(v: Option<i32>) -> Datum {
        v.map_or(Datum::Null, Datum::Int4)
    }

    type OpFn = fn(i32, i32) -> bool;

    fn ref_cmp(f: OpFn, l: Option<i32>, r: Option<i32>) -> Option<bool> {
        Some(f(l?, r?))
    }

    fn ref_quant(any: bool, f: OpFn, lhs: Option<i32>, set: &[Option<i32>]) -> Option<bool> {
        let mut res = Some(!any);
        for r in set {
            match ref_cmp(f, lhs, *r) {
                Some(b) if b == any => return Some(any),
                Some(_) => {}
                None => res = None,
            }
        }
        res
    }

    fn eval_one(q: PhysicalQuery, e: &PhysExpr, row: &Row) -> Result<Datum> {
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        eval(e, row, &mut ctx)
    }

    #[test]
    fn any_all_and_hashed_agree_with_the_three_valued_reference() {
        let sets: [&[Option<i32>]; 7] = [
            &[],
            &[Some(1)],
            &[Some(2)],
            &[None],
            &[Some(1), None],
            &[Some(2), None],
            &[Some(1), Some(2)],
        ];
        let ops: [(&'static BuiltinOperator, OpFn); 3] = [
            (&EQ, |a, b| a == b),
            (&NE, |a, b| a != b),
            (&LT, |a, b| a < b),
        ];
        let mut cases = 0;
        for lhs in [Some(1), Some(2), None] {
            for set in sets {
                for (o, f) in ops {
                    for any in [true, false] {
                        let kind = if any {
                            SubLinkKind::Any
                        } else {
                            SubLinkKind::All
                        };
                        let test = op(o, col(0, SqlType::INT4), sub_out(0));
                        let mut strategies =
                            vec![SubPlanStrategy::Rescan, SubPlanStrategy::InitOnce];
                        if any && o.name == "=" {
                            strategies.push(SubPlanStrategy::Hashed {
                                probe_keys: vec![col(0, SqlType::INT4)],
                                build_keys: vec![col(0, SqlType::INT4)],
                            });
                        }
                        for strategy in strategies {
                            let q = query_of(vec![def(
                                values_plan(set),
                                kind,
                                Some(test.clone()),
                                vec![],
                                strategy.clone(),
                            )]);
                            let e = sublink(kind, 0);
                            let row = vec![opt_datum(lhs)];
                            let want = ref_quant(any, f, lhs, set);
                            let got = eval_one(q.clone(), &e, &row).unwrap();
                            assert_eq!(
                                got,
                                want.map_or(Datum::Null, Datum::Bool),
                                "lhs={lhs:?} set={set:?} op={} any={any} {strategy:?}",
                                o.name
                            );
                            // NOT IN / NOT (x op ALL ...) は外側の Not が NULL を保つ。
                            let got = eval_one(q, &not(e), &row).unwrap();
                            assert_eq!(got, want.map_or(Datum::Null, |b| Datum::Bool(!b)));
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert!(cases > 150);
    }

    /// 05 §4.2.3 の実機の表（t.a = {1, 2, NULL, 2}）。
    #[test]
    fn in_and_not_in_against_a_set_with_null() {
        let set = [Some(1), Some(2), None, Some(2)];
        let any = |lhs: Option<i32>, negate: bool, strategy: SubPlanStrategy| {
            let q = query_of(vec![def(
                values_plan(&set),
                SubLinkKind::Any,
                Some(op(&EQ, col(0, SqlType::INT4), sub_out(0))),
                vec![],
                strategy,
            )]);
            let e = sublink(SubLinkKind::Any, 0);
            let e = if negate { not(e) } else { e };
            eval_one(q, &e, &vec![opt_datum(lhs)]).unwrap()
        };
        let hashed = || SubPlanStrategy::Hashed {
            probe_keys: vec![col(0, SqlType::INT4)],
            build_keys: vec![col(0, SqlType::INT4)],
        };
        for s in [SubPlanStrategy::Rescan, SubPlanStrategy::InitOnce, hashed()] {
            assert_eq!(any(Some(1), false, s.clone()), Datum::Bool(true));
            assert_eq!(any(Some(5), false, s.clone()), Datum::Null);
            assert_eq!(any(Some(5), true, s.clone()), Datum::Null);
            assert_eq!(any(Some(2), true, s.clone()), Datum::Bool(false));
            assert_eq!(any(None, false, s), Datum::Null);
        }
    }

    #[test]
    fn hashed_two_column_matches_postgres() {
        let two = |rows: &[(Option<i32>, Option<i32>)]| PhysicalPlan::Values {
            rows: rows
                .iter()
                .map(|(a, b)| vec![lit_i(*a), lit_i(*b)])
                .collect(),
        };
        let strategy = || SubPlanStrategy::Hashed {
            probe_keys: vec![col(0, SqlType::INT4), col(1, SqlType::INT4)],
            build_keys: vec![col(0, SqlType::INT4), col(1, SqlType::INT4)],
        };
        let n = |v: i32| Some(v);
        let u = [(n(2), n(5)), (n(1), None)];
        let cases: Vec<(
            Vec<(Option<i32>, Option<i32>)>,
            (Option<i32>, Option<i32>),
            Option<bool>,
        )> = vec![
            (vec![(n(2), n(5))], (n(1), None), Some(false)),
            (vec![(n(1), n(5))], (n(1), None), None),
            (vec![(n(2), None)], (n(1), None), Some(false)),
            (vec![(n(1), n(1))], (None, None), None),
            (vec![(n(1), None)], (n(1), n(2)), None),
            (vec![(n(1), None)], (n(3), n(2)), Some(false)),
            (vec![], (n(1), None), Some(false)),
            (u.to_vec(), (n(1), n(2)), None),
            (u.to_vec(), (n(2), n(5)), Some(true)),
            (u.to_vec(), (n(1), n(3)), None),
            (u.to_vec(), (n(3), n(3)), Some(false)),
        ];
        assert_eq!(cases.len(), 11);
        for (set, probe, want) in cases {
            let q = query_of(vec![def(
                two(&set),
                SubLinkKind::Any,
                None,
                vec![],
                strategy(),
            )]);
            let row = vec![opt_datum(probe.0), opt_datum(probe.1)];
            let got = eval_one(q, &sublink(SubLinkKind::Any, 0), &row).unwrap();
            assert_eq!(
                got,
                want.map_or(Datum::Null, Datum::Bool),
                "{set:?} {probe:?}"
            );
        }
    }

    #[test]
    fn hashed_other_kinds_are_internal_errors() {
        let q = query_of(vec![def(
            values_plan(&[Some(1)]),
            SubLinkKind::Exists,
            None,
            vec![],
            SubPlanStrategy::Hashed {
                probe_keys: vec![],
                build_keys: vec![],
            },
        )]);
        let e = eval_one(q, &sublink(SubLinkKind::Exists, 0), &vec![]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn scalar_zero_one_and_two_rows() {
        for strategy in [SubPlanStrategy::Rescan, SubPlanStrategy::InitOnce] {
            let run = |rows: &[Option<i32>]| {
                let q = query_of(vec![def(
                    values_plan(rows),
                    SubLinkKind::Scalar,
                    None,
                    vec![],
                    strategy.clone(),
                )]);
                eval_one(q, &sublink(SubLinkKind::Scalar, 0), &vec![])
            };
            assert_eq!(run(&[]).unwrap(), Datum::Null);
            assert_eq!(run(&[Some(7)]).unwrap(), Datum::Int4(7));
            assert_eq!(run(&[None]).unwrap(), Datum::Null);
            let e = run(&[Some(1), Some(2)]).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::CARDINALITY_VIOLATION);
            assert_eq!(
                e.message,
                "more than one row returned by a subquery used as an expression"
            );
        }
    }

    #[test]
    fn init_once_error_is_not_cached_and_executor_is_returned() {
        let q = query_of(vec![def(
            values_plan(&[Some(1), Some(2)]),
            SubLinkKind::Scalar,
            None,
            vec![],
            SubPlanStrategy::InitOnce,
        )]);
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        let e = sublink(SubLinkKind::Scalar, 0);
        assert!(eval(&e, &vec![], &mut ctx).is_err());
        // 失敗しても Executor は戻っている（`Lent` のままではない）し、キャッシュもない。
        let err = eval(&e, &vec![], &mut ctx).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::CARDINALITY_VIOLATION);
    }

    #[test]
    fn exists_stops_at_the_first_row_and_rewinds_next_time() {
        let q = query_of(vec![def(
            values_plan(&[]),
            SubLinkKind::Exists,
            None,
            vec![],
            SubPlanStrategy::Rescan,
        )]);
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        let (c, reads, rewinds) = CountingExec::ints(5);
        ctx.subplans.put_exec(SubPlanId(0), Box::new(c));
        let e = sublink(SubLinkKind::Exists, 0);
        assert_eq!(eval(&e, &vec![], &mut ctx).unwrap(), Datum::Bool(true));
        assert_eq!((reads.get(), rewinds.get()), (1, 0));
        assert_eq!(eval(&e, &vec![], &mut ctx).unwrap(), Datum::Bool(true));
        assert_eq!((reads.get(), rewinds.get()), (2, 1));
    }

    #[test]
    fn init_once_runs_once_and_lazily() {
        // Exists / Any で、外側 100 行でも副問い合わせは 1 回だけ。
        for (kind, test, want_reads) in [
            (SubLinkKind::Exists, None, 1),
            (
                SubLinkKind::Any,
                Some(op(&EQ, col(0, SqlType::INT4), sub_out(0))),
                4,
            ),
        ] {
            let q = query_of(vec![def(
                values_plan(&[]),
                kind,
                test,
                vec![],
                SubPlanStrategy::InitOnce,
            )]);
            let mut f = Fixture::with_query(q);
            let mut ctx = f.ctx();
            let (c, reads, rewinds) = CountingExec::ints(4);
            ctx.subplans.put_exec(SubPlanId(0), Box::new(c));
            let e = sublink(kind, 0);
            // 評価するまで何も読まない（遅延）。
            assert_eq!(reads.get(), 0);
            let first = eval(&e, &vec![Datum::Int4(1)], &mut ctx).unwrap();
            for _ in 0..100 {
                assert_eq!(eval(&e, &vec![Datum::Int4(1)], &mut ctx).unwrap(), first);
            }
            assert_eq!(reads.get(), want_reads, "{kind:?}");
            assert_eq!(rewinds.get(), 0);
        }
    }

    #[test]
    fn rescan_runs_per_outer_row_with_params() {
        // `(SELECT v FROM (VALUES (1),(2),(3)) WHERE v = $0)` を外側の値で引く。
        let plan = PhysicalPlan::Filter {
            input: Box::new(values_plan(&[Some(1), Some(2), Some(3)])),
            predicate: op(&EQ, col(0, SqlType::INT4), param(0, SqlType::INT4)),
        };
        let mut q = query_of(vec![def(
            plan,
            SubLinkKind::Scalar,
            None,
            vec![(ParamId(0), col(0, SqlType::INT4))],
            SubPlanStrategy::Rescan,
        )]);
        q.n_params = 1;
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        let e = sublink(SubLinkKind::Scalar, 0);
        for (x, want) in [(2, Datum::Int4(2)), (5, Datum::Null), (3, Datum::Int4(3))] {
            assert_eq!(eval(&e, &vec![Datum::Int4(x)], &mut ctx).unwrap(), want);
        }
    }

    /// 副問い合わせの中の副問い合わせが外々側の `Param` を使うとき、Materialize が結果を誤って再利用しない。
    #[test]
    fn nested_subplan_does_not_reuse_stale_results() {
        let inner = PhysicalPlan::Filter {
            input: Box::new(values_plan(&[Some(1), Some(2), Some(3)])),
            predicate: op(&EQ, col(0, SqlType::INT4), param(1, SqlType::INT4)),
        };
        let outer_plan = PhysicalPlan::Materialize {
            input: Box::new(PhysicalPlan::Project {
                input: Box::new(PhysicalPlan::Values {
                    rows: vec![vec![int(0)]],
                }),
                exprs: vec![sublink(SubLinkKind::Exists, 1)],
            }),
        };
        let mut q = query_of(vec![
            def(
                outer_plan,
                SubLinkKind::Scalar,
                None,
                vec![(ParamId(0), col(0, SqlType::INT4))],
                SubPlanStrategy::Rescan,
            ),
            def(
                inner,
                SubLinkKind::Exists,
                None,
                vec![(ParamId(1), param(0, SqlType::INT4))],
                SubPlanStrategy::Rescan,
            ),
        ]);
        q.n_params = 2;
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        let e = mk(
            ExprKind::SubLink {
                kind: SubLinkKind::Scalar,
                test: None,
                query: SubPlanId(0),
            },
            SqlType::BOOL,
        );
        for (x, want) in [(2, true), (5, false), (3, true), (3, true), (9, false)] {
            assert_eq!(
                eval(&e, &vec![Datum::Int4(x)], &mut ctx).unwrap(),
                Datum::Bool(want),
                "x={x}"
            );
        }
    }

    #[test]
    fn reentry_is_internal_error() {
        let q = query_of(vec![def(
            values_plan(&[Some(1)]),
            SubLinkKind::Exists,
            None,
            vec![],
            SubPlanStrategy::Rescan,
        )]);
        let mut f = Fixture::with_query(q);
        let mut ctx = f.ctx();
        // 貸し出し中（評価中）に同じ SubLink をもう一度評価する。
        assert!(ctx.subplans.take_exec(SubPlanId(0)).unwrap().is_none());
        let e = eval(&sublink(SubLinkKind::Exists, 0), &vec![], &mut ctx).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert_eq!(e.message, "SubPlan re-entered");
    }

    #[test]
    fn missing_test_and_bad_id_are_internal_errors() {
        let q = query_of(vec![def(
            values_plan(&[Some(1)]),
            SubLinkKind::Any,
            None,
            vec![],
            SubPlanStrategy::Rescan,
        )]);
        let e = eval_one(q.clone(), &sublink(SubLinkKind::Any, 0), &vec![]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        let e = eval_one(q, &sublink(SubLinkKind::Any, 4), &vec![]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn memory_limit_applies_to_init_once_rows_and_hashed_sets() {
        let rows: Vec<Option<i32>> = (0..100).map(Some).collect();
        for strategy in [
            SubPlanStrategy::InitOnce,
            SubPlanStrategy::Hashed {
                probe_keys: vec![col(0, SqlType::INT4)],
                build_keys: vec![col(0, SqlType::INT4)],
            },
        ] {
            let q = query_of(vec![def(
                values_plan(&rows),
                SubLinkKind::Any,
                Some(op(&EQ, col(0, SqlType::INT4), sub_out(0))),
                vec![],
                strategy,
            )]);
            let mut f = Fixture::with_query(q);
            f.mem_limit = 300;
            let mut ctx = f.ctx();
            let e = eval(
                &sublink(SubLinkKind::Any, 0),
                &vec![Datum::Int4(1)],
                &mut ctx,
            )
            .unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::OUT_OF_MEMORY);
            assert!(e.message.starts_with("out of memory"));
        }
    }

    /// 構築と行の読みのループが 1 行ごとに割り込みを見る。
    #[test]
    fn interrupts_stop_hashed_build_and_rescan_loops() {
        struct Stopper {
            n: usize,
            reads: usize,
        }
        impl Executor for Stopper {
            fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
                self.reads += 1;
                if self.reads == self.n {
                    ctx.interrupts.request_terminate();
                }
                Ok(Some(vec![Datum::Int4(0)]))
            }
            fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
                self.reads = 0;
                Ok(())
            }
        }
        for strategy in [
            SubPlanStrategy::Rescan,
            SubPlanStrategy::Hashed {
                probe_keys: vec![col(0, SqlType::INT4)],
                build_keys: vec![col(0, SqlType::INT4)],
            },
        ] {
            let q = query_of(vec![def(
                values_plan(&[]),
                SubLinkKind::Any,
                // 常に NULL: 読み切るまで終わらない。
                Some(op(&EQ, col(0, SqlType::INT4), null(SqlType::INT4))),
                vec![],
                strategy,
            )]);
            let mut f = Fixture::with_query(q);
            let mut ctx = f.ctx();
            ctx.subplans
                .put_exec(SubPlanId(0), Box::new(Stopper { n: 3, reads: 0 }));
            let e = eval(
                &sublink(SubLinkKind::Any, 0),
                &vec![Datum::Int4(1)],
                &mut ctx,
            )
            .unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::ADMIN_SHUTDOWN);
        }
    }
}
