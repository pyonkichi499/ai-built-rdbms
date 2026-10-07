//! NestedLoopJoin / NestedLoopParam ノード（`m4/05` §5.13・§5.14）。
//!
//! 1 つの構造体で両方を扱う（`params` が空なら NestedLoopJoin）。出力は `outer ++ inner`
//! （SEMI / ANTI は `outer` だけ）。FULL は実行できない（planner が Hash Join にする）。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::rc::Rc;

use crate::error::{Error, Result};
use crate::executor::build::{BuildEnv, build_scoped};
use crate::executor::eval::{eval, eval_pred};
use crate::executor::instrument::Instrumentation;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::expr::ParamId;
use crate::planner::logical::JoinKind;
use crate::planner::physical::{FilterCounter, PhysExpr, PhysicalPlan};
use crate::types::{Datum, Row};

/// `Nested Loop` を実行する Executor を作る。inner は `rewind` されるので `rewindable = true` で作る。
pub(in crate::executor) fn build(
    plan: &PhysicalPlan,
    env: &BuildEnv<'_>,
    rewindable: bool,
) -> BoxedExecutor {
    match plan {
        PhysicalPlan::NestedLoopJoin {
            kind,
            outer,
            inner,
            join_filter,
            inner_width,
            ..
        } => Box::new(NestedLoopExec::new(
            *kind,
            build_scoped(outer, env, rewindable),
            build_scoped(inner, env, true),
            Vec::new(),
            join_filter.clone(),
            *inner_width,
        )),
        PhysicalPlan::NestedLoopParam {
            kind,
            outer,
            inner,
            params,
            join_filter,
            inner_width,
            ..
        } => Box::new(NestedLoopExec::new(
            *kind,
            build_scoped(outer, env, rewindable),
            build_scoped(inner, env, true),
            params.clone(),
            join_filter.clone(),
            *inner_width,
        )),
        other => unreachable!("nested_loop::build called with {other:?}"),
    }
}

pub struct NestedLoopExec {
    kind: JoinKind,
    outer: BoxedExecutor,
    inner: BoxedExecutor,
    /// 外側の行ごとに評価して `ctx.params` に設定する（NestedLoopParam）。
    params: Vec<(ParamId, PhysExpr)>,
    join_filter: Option<PhysExpr>,
    inner_width: usize,
    outer_row: Option<Row>,
    matched: bool,
    /// `inner.next` を 1 回でも呼んだか（次の外側の行の前に `rewind` が要る）。
    inner_dirty: bool,
    /// 外側が尽きた（以後は子を呼ばずに `None`）。
    done: bool,
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl std::fmt::Debug for NestedLoopExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NestedLoopExec")
            .field("kind", &self.kind)
            .field("join_filter", &self.join_filter)
            .finish_non_exhaustive()
    }
}

impl NestedLoopExec {
    pub fn new(
        kind: JoinKind,
        outer: BoxedExecutor,
        inner: BoxedExecutor,
        params: Vec<(ParamId, PhysExpr)>,
        join_filter: Option<PhysExpr>,
        inner_width: usize,
    ) -> Self {
        NestedLoopExec {
            kind,
            outer,
            inner,
            params,
            join_filter,
            inner_width,
            outer_row: None,
            matched: false,
            inner_dirty: false,
            done: false,
            counters: None,
        }
    }

    /// 次の外側の行を取り、`params` を設定し、必要なら inner を先頭に戻す。尽きたら `false`。
    fn advance_outer(&mut self, ctx: &mut ExecCtx<'_>) -> Result<bool> {
        let Some(o) = self.outer.next(ctx)? else {
            self.done = true;
            return Ok(false);
        };
        self.matched = false;
        if !self.params.is_empty() {
            // 先に全部評価してから代入する（式が `Param` を読みうる）。
            let vals = self
                .params
                .iter()
                .map(|(_, e)| eval(e, &o, ctx))
                .collect::<Result<Vec<_>>>()?;
            for ((p, _), v) in self.params.iter().zip(vals) {
                ctx.set_param(*p, v)?;
            }
        }
        self.outer_row = Some(o);
        if self.inner_dirty {
            self.inner.rewind(ctx)?;
        }
        Ok(true)
    }
}

impl Executor for NestedLoopExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.kind == JoinKind::Full {
            return Err(Error::internal("FULL JOIN is not supported by Nested Loop"));
        }
        if self.done {
            return Ok(None);
        }
        loop {
            ctx.check_interrupts()?;
            if self.outer_row.is_none() && !self.advance_outer(ctx)? {
                return Ok(None);
            }
            let irow = self.inner.next(ctx)?;
            self.inner_dirty = true;
            let Some(irow) = irow else {
                // inner を読み切った。
                let Some(o) = self.outer_row.take() else {
                    continue;
                };
                match self.kind {
                    JoinKind::Left if !self.matched => {
                        let mut out = o;
                        out.extend(std::iter::repeat_n(Datum::Null, self.inner_width));
                        return Ok(Some(out));
                    }
                    JoinKind::Anti if !self.matched => return Ok(Some(o)),
                    _ => continue,
                }
            };
            let Some(o) = self.outer_row.as_ref() else {
                continue;
            };
            let mut joined = Vec::with_capacity(o.len() + irow.len());
            joined.extend_from_slice(o);
            joined.extend(irow);
            if let Some(f) = &self.join_filter
                && eval_pred(f, &joined, ctx)? != Some(true)
            {
                if let Some((id, instr)) = &self.counters {
                    instr.add_removed(*id, FilterCounter::JoinFilter, 1);
                }
                continue;
            }
            self.matched = true;
            match self.kind {
                JoinKind::Inner | JoinKind::Left => return Ok(Some(joined)),
                // 最初の一致で外側の行を出して次へ（inner は途中のまま。`inner_dirty` が `rewind` を保証する）。
                JoinKind::Semi => return Ok(self.outer_row.take()),
                // 一致があれば出さない。
                JoinKind::Anti => self.outer_row = None,
                JoinKind::Full => unreachable!("checked above"),
            }
        }
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.outer.rewind(ctx)?;
        self.outer_row = None;
        self.matched = false;
        self.done = false;
        // `inner_dirty` は維持する（inner が途中の可能性があるので、次の外側の行で必ず `rewind` する）。
        Ok(())
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.counters = Some((id, Rc::clone(instr)));
    }
}

/// 結合ノードのテスト用の部品（入力、割り込み、参照実装）。
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::executor::eval::tests::{EQ, GT, col, int, null, op};
    use crate::executor::nodes::test_util::Fixture;
    use crate::expr::ExprKind;
    use crate::types::SqlType;

    pub(crate) type Pair = (Option<i32>, Option<i32>);

    /// 行を返すだけの入力。`interrupt_at` 番目の行を返すときに停止要求を立てる。
    #[derive(Debug)]
    pub(crate) struct RowsExec {
        rows: Vec<Row>,
        pos: usize,
        reads: Rc<Cell<usize>>,
        rewinds: Rc<Cell<usize>>,
        interrupt_at: Option<usize>,
    }

    impl RowsExec {
        pub(crate) fn new(rows: Vec<Row>) -> Self {
            RowsExec {
                rows,
                pos: 0,
                reads: Rc::new(Cell::new(0)),
                rewinds: Rc::new(Cell::new(0)),
                interrupt_at: None,
            }
        }

        pub(crate) fn pairs(rows: &[Pair]) -> Self {
            RowsExec::new(rows.iter().map(|p| pair_row(*p)).collect())
        }

        pub(crate) fn interrupt_at(mut self, n: usize) -> Self {
            self.interrupt_at = Some(n);
            self
        }

        /// 読んだ行数と `rewind` の回数のハンドル。
        pub(crate) fn counters(&self) -> (Rc<Cell<usize>>, Rc<Cell<usize>>) {
            (Rc::clone(&self.reads), Rc::clone(&self.rewinds))
        }
    }

    impl Executor for RowsExec {
        fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
            let Some(r) = self.rows.get(self.pos) else {
                return Ok(None);
            };
            self.pos += 1;
            self.reads.set(self.reads.get() + 1);
            if self.interrupt_at == Some(self.reads.get()) {
                ctx.interrupts.request_terminate();
            }
            Ok(Some(r.clone()))
        }

        fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
            self.pos = 0;
            self.rewinds.set(self.rewinds.get() + 1);
            Ok(())
        }
    }

    pub(crate) fn opt(v: Option<i32>) -> Datum {
        v.map_or(Datum::Null, Datum::Int4)
    }

    pub(crate) fn pair_row(p: Pair) -> Row {
        vec![opt(p.0), opt(p.1)]
    }

    pub(crate) fn pairs_plan(rows: &[Pair]) -> PhysicalPlan {
        PhysicalPlan::Values {
            rows: rows
                .iter()
                .map(|(a, b)| {
                    [a, b]
                        .iter()
                        .map(|v| v.map_or_else(|| null(SqlType::INT4), int))
                        .collect()
                })
                .collect(),
        }
    }

    pub(crate) fn and(args: Vec<PhysExpr>) -> PhysExpr {
        PhysExpr::new(
            ExprKind::And(args),
            SqlType::BOOL,
            crate::error::Span::default(),
        )
    }

    /// `left.key = right.key`（結合した行の 0 列目と 2 列目）。
    pub(crate) fn key_eq() -> PhysExpr {
        op(&EQ, col(0, SqlType::INT4), col(2, SqlType::INT4))
    }

    /// `left.val > right.val`（1 列目と 3 列目）。
    pub(crate) fn val_gt() -> PhysExpr {
        op(&GT, col(1, SqlType::INT4), col(3, SqlType::INT4))
    }

    /// 素朴な 3 重ループの参照実装（キーの NULL は一致しない。`residual` が NULL なら不一致）。
    pub(crate) fn reference(
        kind: JoinKind,
        left: &[Pair],
        right: &[Pair],
        residual: bool,
    ) -> Vec<Row> {
        let matches = |l: &Pair, r: &Pair| {
            let key = matches!((l.0, r.0), (Some(a), Some(b)) if a == b);
            let res = !residual || matches!((l.1, r.1), (Some(a), Some(b)) if a > b);
            key && res
        };
        let mut out = Vec::new();
        for l in left {
            let hits: Vec<&Pair> = right.iter().filter(|r| matches(l, r)).collect();
            match kind {
                JoinKind::Inner | JoinKind::Left | JoinKind::Full => {
                    for r in &hits {
                        out.push([pair_row(*l), pair_row(**r)].concat());
                    }
                    if hits.is_empty() && kind != JoinKind::Inner {
                        out.push([pair_row(*l), vec![Datum::Null; 2]].concat());
                    }
                }
                JoinKind::Semi if !hits.is_empty() => out.push(pair_row(*l)),
                JoinKind::Anti if hits.is_empty() => out.push(pair_row(*l)),
                _ => {}
            }
        }
        if kind == JoinKind::Full {
            for r in right {
                if !left.iter().any(|l| matches(l, r)) {
                    out.push([vec![Datum::Null; 2], pair_row(*r)].concat());
                }
            }
        }
        out
    }

    /// 行の多重集合として比べるための整列。
    pub(crate) fn sorted(mut rows: Vec<Row>) -> Vec<String> {
        let mut v: Vec<String> = rows.drain(..).map(|r| format!("{r:?}")).collect();
        v.sort();
        v
    }

    /// 決定的な擬似乱数（xorshift64）。
    pub(crate) struct Rng(pub u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// 0..=3 のキー（5 分の 1 が NULL）。
        pub(crate) fn key(&mut self) -> Option<i32> {
            if self.below(5) == 0 {
                None
            } else {
                Some(i32::try_from(self.below(4)).unwrap_or(0))
            }
        }

        pub(crate) fn val(&mut self) -> Option<i32> {
            if self.below(6) == 0 {
                None
            } else {
                Some(i32::try_from(self.below(5)).unwrap_or(0))
            }
        }

        pub(crate) fn rows(&mut self, n: usize) -> Vec<Pair> {
            (0..n).map(|_| (self.key(), self.val())).collect()
        }
    }

    pub(crate) fn run_plan(plan: &PhysicalPlan, n_params: usize) -> Result<Vec<Row>> {
        let mut f = Fixture::with_params(n_params);
        let mut e = crate::executor::build::build(plan);
        f.run(&mut e)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{EQ, col, op, param};
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::types::SqlType;

    fn nlj(kind: JoinKind, l: &[Pair], r: &[Pair], filter: Option<PhysExpr>) -> PhysicalPlan {
        PhysicalPlan::NestedLoopJoin {
            kind,
            outer: Box::new(pairs_plan(l)),
            inner: Box::new(pairs_plan(r)),
            join_filter: filter,
            outer_width: 2,
            inner_width: 2,
        }
    }

    fn materialized(plan: PhysicalPlan) -> PhysicalPlan {
        match plan {
            PhysicalPlan::NestedLoopJoin {
                kind,
                outer,
                inner,
                join_filter,
                outer_width,
                inner_width,
            } => PhysicalPlan::NestedLoopJoin {
                kind,
                outer,
                inner: Box::new(PhysicalPlan::Materialize { input: inner }),
                join_filter,
                outer_width,
                inner_width,
            },
            other => other,
        }
    }

    /// NestedLoopParam: 外側のキーを `$0` で inner の `Filter` に渡す。
    fn nlp(kind: JoinKind, l: &[Pair], r: &[Pair], residual: Option<PhysExpr>) -> PhysicalPlan {
        PhysicalPlan::NestedLoopParam {
            kind,
            outer: Box::new(pairs_plan(l)),
            inner: Box::new(PhysicalPlan::Filter {
                input: Box::new(pairs_plan(r)),
                predicate: op(&EQ, col(0, SqlType::INT4), param(0, SqlType::INT4)),
            }),
            params: vec![(ParamId(0), col(0, SqlType::INT4))],
            join_filter: residual,
            outer_width: 2,
            inner_width: 2,
        }
    }

    #[test]
    fn nested_loops_agree_with_the_reference() {
        let kinds = [
            JoinKind::Inner,
            JoinKind::Left,
            JoinKind::Semi,
            JoinKind::Anti,
        ];
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let (nl, nr) = match seed {
                1 => (0, 0),
                2 => (0, 6),
                3 => (6, 0),
                _ => (
                    usize::try_from(rng.below(41)).unwrap(),
                    usize::try_from(rng.below(41)).unwrap(),
                ),
            };
            let (l, r) = (rng.rows(nl), rng.rows(nr));
            for kind in kinds {
                for residual in [false, true] {
                    let want = sorted(reference(kind, &l, &r, residual));
                    let filter = if residual {
                        and(vec![key_eq(), val_gt()])
                    } else {
                        key_eq()
                    };
                    let plain = nlj(kind, &l, &r, Some(filter));
                    let got = sorted(run_plan(&plain, 0).unwrap());
                    assert_eq!(
                        got, want,
                        "NLJ seed={seed} kind={kind:?} residual={residual}"
                    );
                    let mat = materialized(plain);
                    let got = sorted(run_plan(&mat, 0).unwrap());
                    assert_eq!(got, want, "NLJ+Materialize seed={seed} kind={kind:?}");
                    let res = residual.then(val_gt);
                    let got = sorted(run_plan(&nlp(kind, &l, &r, res), 1).unwrap());
                    assert_eq!(
                        got, want,
                        "NLP seed={seed} kind={kind:?} residual={residual}"
                    );
                }
            }
        }
    }

    #[test]
    fn full_join_is_an_internal_error() {
        let plan = nlj(JoinKind::Full, &[(Some(1), Some(1))], &[], None);
        let e = run_plan(&plan, 0).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn join_filter_null_is_a_mismatch_and_left_extends() {
        let l = [(Some(1), None)];
        let r = [(Some(1), Some(3))];
        // val_gt は NULL（左の val が NULL）→ 不一致。
        let got = run_plan(&nlj(JoinKind::Left, &l, &r, Some(val_gt())), 0).unwrap();
        assert_eq!(
            got,
            vec![vec![Datum::Int4(1), Datum::Null, Datum::Null, Datum::Null]]
        );
        // ANTI は NULL を一致に数えない（NOT EXISTS）。
        let got = run_plan(&nlj(JoinKind::Anti, &l, &r, Some(val_gt())), 0).unwrap();
        assert_eq!(got, vec![vec![Datum::Int4(1), Datum::Null]]);
        assert!(
            run_plan(&nlj(JoinKind::Semi, &l, &r, Some(val_gt())), 0)
                .unwrap()
                .is_empty()
        );
    }

    fn exec_with(
        kind: JoinKind,
        outer: RowsExec,
        inner: RowsExec,
        filter: Option<PhysExpr>,
    ) -> BoxedExecutor {
        Box::new(NestedLoopExec::new(
            kind,
            Box::new(outer),
            Box::new(inner),
            Vec::new(),
            filter,
            2,
        ))
    }

    #[test]
    fn semi_stops_the_inner_early_and_rewinds_it_for_the_next_outer_row() {
        let outer = RowsExec::pairs(&[(Some(1), None), (Some(1), None), (Some(2), None)]);
        let inner = RowsExec::pairs(&[(Some(1), None), (Some(1), None), (Some(2), None)]);
        let (reads, rewinds) = inner.counters();
        let mut f = Fixture::new();
        let mut e = exec_with(JoinKind::Semi, outer, inner, Some(key_eq()));
        let rows = f.run(&mut e).unwrap();
        assert_eq!(rows.len(), 3);
        // 1 行目: 1 行読んで一致。2 行目: rewind して 1 行。3 行目: rewind して 3 行。
        assert_eq!(reads.get(), 1 + 1 + 3);
        assert_eq!(rewinds.get(), 2);
        // 枯渇後は子を呼ばずに None を返し続ける。
        let mut ctx = f.ctx();
        assert!(e.next(&mut ctx).unwrap().is_none());
        assert_eq!(reads.get(), 5);
    }

    #[test]
    fn rewind_restarts_the_join() {
        let outer = RowsExec::pairs(&[(Some(1), None), (Some(2), None)]);
        let inner = RowsExec::pairs(&[(Some(1), None), (Some(2), None)]);
        let mut f = Fixture::new();
        let mut e = exec_with(JoinKind::Inner, outer, inner, Some(key_eq()));
        let mut ctx = f.ctx();
        let first = drain(&mut e, &mut ctx).unwrap();
        e.rewind(&mut ctx).unwrap();
        let second = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first, second);
        // 途中で `rewind` しても先頭から（inner が途中のまま残っていても）。
        e.rewind(&mut ctx).unwrap();
        e.next(&mut ctx).unwrap().unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), first);
    }

    #[test]
    fn empty_sides() {
        let some = [(Some(1), Some(1))];
        assert!(
            run_plan(&nlj(JoinKind::Inner, &some, &[], None), 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            run_plan(&nlj(JoinKind::Inner, &[], &some, None), 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            run_plan(&nlj(JoinKind::Left, &some, &[], None), 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            run_plan(&nlj(JoinKind::Anti, &some, &[], None), 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn counts_rows_removed_by_join_filter() {
        use crate::executor::instrument::Instrumentation;
        let outer = RowsExec::pairs(&[(Some(1), None), (Some(2), None)]);
        let inner = RowsExec::pairs(&[(Some(1), None), (Some(1), None), (Some(3), None)]);
        let mut f = Fixture::new();
        let mut e = exec_with(JoinKind::Inner, outer, inner, Some(key_eq()));
        let instr = Rc::new(Instrumentation::new(1));
        e.set_counters(0, &instr);
        let rows = f.run(&mut e).unwrap();
        assert_eq!(rows.len(), 2);
        // 候補 6 組のうち 4 組が落ちる。
        assert_eq!(instr.node(0).removed_join_filter.get(), 4);
        assert_eq!(instr.node(0).removed_filter.get(), 0);
    }

    #[test]
    fn interrupts_stop_the_inner_loop_and_the_outer_read() {
        // inner の 3 行目で停止要求 → 内側ループの次の周回で止まる。
        let outer = RowsExec::pairs(&[(Some(1), None)]);
        let inner = RowsExec::pairs(&vec![(Some(9), None); 50]).interrupt_at(3);
        let (reads, _) = inner.counters();
        let mut f = Fixture::new();
        let mut e = exec_with(JoinKind::Inner, outer, inner, Some(key_eq()));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::ADMIN_SHUTDOWN);
        assert_eq!(reads.get(), 3);
        // outer の読みで止まる。
        let outer = RowsExec::pairs(&vec![(Some(1), None); 50]).interrupt_at(2);
        let inner = RowsExec::pairs(&[]);
        let mut f = Fixture::new();
        let mut e = exec_with(JoinKind::Inner, outer, inner, None);
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            sqlstate::ADMIN_SHUTDOWN
        );
    }
}
