//! EXPLAIN ANALYZE の計測（`m4/10` §3.10、11 §7.1 の C-2）。
//!
//! - `Instrumentation` / `NodeCounters`: ノードごとの計測値。`ExecCtx` と同じスレッドだけで使うので `Cell` で持つ
//!   （`Rc` で共有され、ノードは `&self` で数える）。
//! - `Instrumented`: 各 Executor を包み、`next` / `rewind` を PostgreSQL の `Instrumentation`
//!   （`InstrStartNode` / `InstrStopNode` / `InstrEndLoop`）と同じ規則で計る。
//! - `InstrBuild`: `PhysicalPlan` のノードのアドレスから通し番号（`assign_exec_ids`）を引く表。
//!   `build_scoped` が各ノードを作った直後に `wrap` を呼ぶ（子の作る順に依存しない）。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{BoxedExecutor, ExecCtx, Executor};
use crate::error::Result;
use crate::planner::physical::{FilterCounter, PhysicalPlan, PhysicalQuery, assign_exec_ids};
use crate::types::Row;

/// ノード 1 つの計測値（PostgreSQL の `Instrumentation`）。
#[derive(Debug, Default)]
pub struct NodeCounters {
    /// 現在のループ（`rewind` から次の `rewind` まで）の計測。
    pub running: Cell<bool>,
    pub cycle_total: Cell<Duration>,
    pub cycle_first: Cell<Duration>,
    pub cycle_rows: Cell<u64>,
    /// `end_loop` で足し込んだ合計。
    pub loops: Cell<u64>,
    pub rows: Cell<u64>,
    pub startup: Cell<Duration>,
    pub total: Cell<Duration>,
    /// スキャン・結合・Filter が内部で数える（`FilterCounter::Filter` / `JoinFilter`）。
    pub removed_filter: Cell<u64>,
    pub removed_join_filter: Cell<u64>,
}

impl NodeCounters {
    /// `running` でなければ何もしない。現在のループの値を合計に足して 0 に戻す（InstrEndLoop）。
    pub fn end_loop(&self) {
        if !self.running.get() {
            return;
        }
        self.loops.set(self.loops.get() + 1);
        self.rows.set(self.rows.get() + self.cycle_rows.get());
        self.startup
            .set(self.startup.get() + self.cycle_first.get());
        self.total.set(self.total.get() + self.cycle_total.get());
        self.cycle_rows.set(0);
        self.cycle_first.set(Duration::ZERO);
        self.cycle_total.set(Duration::ZERO);
        self.running.set(false);
    }

    /// `FilterCounter` に対応する落とした行の合計。
    pub fn removed(&self, counter: FilterCounter) -> u64 {
        match counter {
            FilterCounter::Filter => self.removed_filter.get(),
            FilterCounter::JoinFilter => self.removed_join_filter.get(),
        }
    }
}

/// 文の間 1 つ。番号は `planner::physical::assign_exec_ids` の通し番号。
#[derive(Debug)]
pub struct Instrumentation {
    nodes: Vec<NodeCounters>,
    timing: bool,
}

impl Instrumentation {
    /// 時間も計る。
    pub fn new(n_nodes: usize) -> Self {
        Self::with_timing(n_nodes, true)
    }

    /// `timing = false`（`TIMING OFF`）なら `Instant` を取らず、`startup` / `total` は 0 のまま。
    pub fn with_timing(n_nodes: usize, timing: bool) -> Self {
        Instrumentation {
            nodes: (0..n_nodes).map(|_| NodeCounters::default()).collect(),
            timing,
        }
    }

    /// 時間を計るか。
    pub fn timing(&self) -> bool {
        self.timing
    }

    /// 描画の前に全ノードで呼ぶ（`running` なら `end_loop`）。
    pub fn finish(&self) {
        for n in &self.nodes {
            n.end_loop();
        }
    }

    /// 範囲外の番号は `None`。
    pub fn get(&self, id: usize) -> Option<&NodeCounters> {
        self.nodes.get(id)
    }

    pub fn node(&self, id: usize) -> &NodeCounters {
        &self.nodes[id]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 述語で落とした行を数える。範囲外の番号は無視する。
    pub fn add_removed(&self, id: usize, counter: FilterCounter, n: u64) {
        let Some(c) = self.nodes.get(id) else {
            return;
        };
        let cell = match counter {
            FilterCounter::Filter => &c.removed_filter,
            FilterCounter::JoinFilter => &c.removed_join_filter,
        };
        cell.set(cell.get() + n);
    }
}

/// 計測つきの Executor。`inner` の `next` / `rewind` を計る（PostgreSQL の `InstrStartNode` / `InstrStopNode`）。
pub struct Instrumented {
    inner: BoxedExecutor,
    id: usize,
    instr: Rc<Instrumentation>,
}

impl std::fmt::Debug for Instrumented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instrumented")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Instrumented {
    pub fn new(inner: BoxedExecutor, id: usize, instr: Rc<Instrumentation>) -> Self {
        Instrumented { inner, id, instr }
    }
}

impl Executor for Instrumented {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let started = self.instr.timing().then(Instant::now);
        let result = self.inner.next(ctx);
        let Some(node) = self.instr.get(self.id) else {
            return result;
        };
        if let Some(t) = started {
            node.cycle_total.set(node.cycle_total.get() + t.elapsed());
        }
        // 最初の `next` が戻った時点（行でも `None` でも。エラーでも）で「実行した」とする。
        if !node.running.get() {
            node.running.set(true);
            node.cycle_first.set(node.cycle_total.get());
        }
        if matches!(result, Ok(Some(_))) {
            node.cycle_rows.set(node.cycle_rows.get() + 1);
        }
        result
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if let Some(node) = self.instr.get(self.id) {
            node.end_loop();
        }
        self.inner.rewind(ctx)
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.inner.set_counters(id, instr);
    }

    fn rows_affected(&self) -> u64 {
        self.inner.rows_affected()
    }
}

/// `PhysicalPlan` のノードのアドレスから通し番号を引き、作った Executor を `Instrumented` で包む。
///
/// 番号は `assign_exec_ids` と同じ（根 → `subplans` の昇順 → `ctes` の昇順。各木は先行順）。アドレスで引くので、
/// ノードが子を作る順に依存しない。表にないノード（物理化の途中で作り直した木など）は包まない。
#[derive(Debug)]
pub struct InstrBuild {
    ids: HashMap<usize, usize>,
    instr: Rc<Instrumentation>,
}

fn addr(plan: &PhysicalPlan) -> usize {
    std::ptr::from_ref(plan) as usize
}

fn visit(plan: &PhysicalPlan, next: &mut usize, ids: &mut HashMap<usize, usize>) {
    ids.insert(addr(plan), *next);
    *next += 1;
    for c in plan.children() {
        visit(c, next, ids);
    }
}

impl InstrBuild {
    /// `q` の全ノード（根・`subplans`・`ctes`）の表を作る。`q` は実行の間、同じ場所にあること。
    pub fn new(q: &PhysicalQuery, instr: &Rc<Instrumentation>) -> Self {
        let layout = assign_exec_ids(q);
        let mut ids = HashMap::new();
        let mut next = layout.root;
        visit(&q.root, &mut next, &mut ids);
        for (s, start) in q.subplans.iter().zip(&layout.subplans) {
            let mut next = *start;
            visit(&s.plan, &mut next, &mut ids);
        }
        for (c, start) in q.ctes.iter().zip(&layout.ctes) {
            let mut next = *start;
            visit(c, &mut next, &mut ids);
        }
        InstrBuild {
            ids,
            instr: Rc::clone(instr),
        }
    }

    /// `plan` の通し番号。
    pub fn id_of(&self, plan: &PhysicalPlan) -> Option<usize> {
        self.ids.get(&addr(plan)).copied()
    }

    /// `exec`（`plan` から作った）に `set_counters` を呼び、`Instrumented` で包む。
    pub fn wrap(&self, plan: &PhysicalPlan, mut exec: BoxedExecutor) -> BoxedExecutor {
        let Some(id) = self.id_of(plan) else {
            return exec;
        };
        exec.set_counters(id, &self.instr);
        Box::new(Instrumented::new(exec, id, Rc::clone(&self.instr)))
    }
}

/// 計測つきの Executor の木を作る（`build_query` と同じ形で、各ノードを `Instrumented` で包む）。
/// `SubPlan` / CTE の Executor は `SubPlanStates` / `CteStates` が最初に使うときに `ExecCtx.instr` から作る。
pub fn build_instrumented(q: &PhysicalQuery, instr: &Rc<Instrumentation>) -> BoxedExecutor {
    let ib = InstrBuild::new(q, instr);
    let env = super::build::BuildEnv {
        query: Some(q),
        instr: Some(&ib),
    };
    super::build::build_scoped(&q.root, &env, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{GT, col, int, op};
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::planner::physical::PhysExpr;
    use crate::types::{Datum, SqlType};

    #[test]
    fn add_removed_and_end_loop() {
        let i = Instrumentation::new(2);
        i.add_removed(1, FilterCounter::Filter, 3);
        i.add_removed(1, FilterCounter::Filter, 2);
        i.add_removed(1, FilterCounter::JoinFilter, 7);
        i.add_removed(9, FilterCounter::Filter, 1); // 範囲外は無視
        assert_eq!(i.node(1).removed_filter.get(), 5);
        assert_eq!(i.node(1).removed(FilterCounter::JoinFilter), 7);
        assert_eq!(i.node(0).removed_filter.get(), 0);
        // running でないノードの end_loop は何もしない。
        i.finish();
        assert_eq!(i.node(0).loops.get(), 0);
        let n = i.node(0);
        n.running.set(true);
        n.cycle_rows.set(4);
        n.end_loop();
        assert_eq!(
            (n.loops.get(), n.rows.get(), n.running.get()),
            (1, 4, false)
        );
        assert!(i.get(2).is_none());
    }

    #[test]
    fn end_loop_adds_cycle_values_and_resets() {
        let i = Instrumentation::new(1);
        let n = i.node(0);
        n.running.set(true);
        n.cycle_rows.set(2);
        n.cycle_first.set(Duration::from_millis(3));
        n.cycle_total.set(Duration::from_millis(10));
        n.end_loop();
        n.running.set(true);
        n.cycle_rows.set(5);
        n.cycle_first.set(Duration::from_millis(1));
        n.cycle_total.set(Duration::from_millis(4));
        i.finish();
        assert_eq!((n.loops.get(), n.rows.get()), (2, 7));
        assert_eq!(n.startup.get(), Duration::from_millis(4));
        assert_eq!(n.total.get(), Duration::from_millis(14));
        assert_eq!(n.cycle_rows.get(), 0);
        assert_eq!(n.cycle_total.get(), Duration::ZERO);
    }

    fn instrumented(n: i32, timing: bool) -> (Instrumented, Rc<Instrumentation>) {
        let instr = Rc::new(Instrumentation::with_timing(1, timing));
        let (e, _, _) = CountingExec::ints(n);
        (Instrumented::new(Box::new(e), 0, Rc::clone(&instr)), instr)
    }

    #[test]
    fn loops_and_rows_across_rewinds() {
        let mut f = Fixture::new();
        let (e, instr) = instrumented(3, true);
        let mut e: BoxedExecutor = Box::new(e);
        let mut ctx = f.ctx();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 3);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 3);
        // 1 行だけ読んで止める。
        e.rewind(&mut ctx).unwrap();
        assert!(e.next(&mut ctx).unwrap().is_some());
        // 読まない rewind はループに数えない。
        e.rewind(&mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        instr.finish();
        let n = instr.node(0);
        assert_eq!((n.loops.get(), n.rows.get()), (3, 7));
        assert!(!n.running.get());
    }

    #[test]
    fn never_executed_has_zero_loops() {
        let mut f = Fixture::new();
        let (e, instr) = instrumented(3, true);
        let mut e: BoxedExecutor = Box::new(e);
        let mut ctx = f.ctx();
        e.rewind(&mut ctx).unwrap();
        instr.finish();
        assert_eq!(instr.node(0).loops.get(), 0);
        assert_eq!(instr.node(0).rows.get(), 0);
    }

    #[test]
    fn exhausted_first_call_still_counts_a_loop() {
        let mut f = Fixture::new();
        let (e, instr) = instrumented(0, true);
        let mut e: BoxedExecutor = Box::new(e);
        let mut ctx = f.ctx();
        assert!(e.next(&mut ctx).unwrap().is_none());
        instr.finish();
        assert_eq!(
            (instr.node(0).loops.get(), instr.node(0).rows.get()),
            (1, 0)
        );
    }

    struct Sleepy(u32);
    impl Executor for Sleepy {
        fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
            std::thread::sleep(Duration::from_millis(3));
            if self.0 == 0 {
                return Ok(None);
            }
            self.0 -= 1;
            Ok(Some(vec![Datum::Int4(1)]))
        }
        fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn timing_on_records_startup_and_total_and_off_does_not() {
        for timing in [true, false] {
            let instr = Rc::new(Instrumentation::with_timing(1, timing));
            let mut e: BoxedExecutor =
                Box::new(Instrumented::new(Box::new(Sleepy(2)), 0, Rc::clone(&instr)));
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 2);
            instr.finish();
            let n = instr.node(0);
            if timing {
                assert!(n.startup.get() >= Duration::from_millis(3));
                assert!(n.total.get() >= Duration::from_millis(9));
                assert!(n.total.get() > n.startup.get());
            } else {
                assert_eq!(n.startup.get(), Duration::ZERO);
                assert_eq!(n.total.get(), Duration::ZERO);
            }
            assert_eq!((n.loops.get(), n.rows.get()), (1, 2));
        }
    }

    fn filter_over_values() -> PhysicalPlan {
        let pred: PhysExpr = op(&GT, col(0, SqlType::INT4), int(1));
        PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Values {
                rows: vec![vec![int(1)], vec![int(2)], vec![int(3)]],
            }),
            predicate: pred,
        }
    }

    #[test]
    fn ids_follow_assign_exec_ids_for_root_subplans_and_ctes() {
        use crate::expr::SubLinkKind;
        use crate::planner::physical::{SubPlanDef, SubPlanStrategy};
        let mut q = PhysicalQuery::single(filter_over_values(), vec![]);
        q.subplans.push(SubPlanDef {
            plan: filter_over_values(),
            kind: SubLinkKind::Scalar,
            test: None,
            params: vec![],
            strategy: SubPlanStrategy::InitOnce,
            explain: None,
        });
        q.ctes.push(filter_over_values());
        let instr = Rc::new(Instrumentation::new(assign_exec_ids(&q).total));
        let ib = InstrBuild::new(&q, &instr);
        assert_eq!(ib.id_of(&q.root), Some(0));
        assert_eq!(ib.id_of(q.root.children()[0]), Some(1));
        assert_eq!(ib.id_of(&q.subplans[0].plan), Some(2));
        assert_eq!(ib.id_of(q.subplans[0].plan.children()[0]), Some(3));
        assert_eq!(ib.id_of(&q.ctes[0]), Some(4));
        assert_eq!(ib.id_of(q.ctes[0].children()[0]), Some(5));
        assert_eq!(instr.len(), 6);
        // 別の木のノードは表にない。
        let other = filter_over_values();
        assert_eq!(ib.id_of(&other), None);
    }

    #[test]
    fn build_instrumented_counts_rows_and_removed() {
        let q = PhysicalQuery::single(filter_over_values(), vec![]);
        let instr = Rc::new(Instrumentation::new(assign_exec_ids(&q).total));
        let mut e = build_instrumented(&q, &instr);
        let mut f = Fixture::with_query(q.clone());
        let mut ctx = f.ctx();
        assert_eq!(drain(&mut e, &mut ctx).unwrap().len(), 2);
        instr.finish();
        let (filter, values) = (instr.node(0), instr.node(1));
        assert_eq!((filter.loops.get(), filter.rows.get()), (1, 2));
        assert_eq!((values.loops.get(), values.rows.get()), (1, 3));
        assert_eq!(filter.removed_filter.get(), 1);
    }
}
