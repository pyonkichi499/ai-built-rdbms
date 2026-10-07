//! Hash Join ノード（`m4/05` §5.15）。
//!
//! 「probe 側の保存（PP）」と「build 側の保存（BP）」の 2 つの印で、INNER / LEFT / FULL / SEMI / ANTI
//! × `build_is_left` を 1 つのアルゴリズムで扱う。出力順は probe の順、各 probe 行の中は build の挿入順、
//! 終了段は build の挿入順（決定的）。結合キーの NULL は一致しない。

#![allow(
    clippy::doc_markdown,
    clippy::trivially_copy_pass_by_ref,
    clippy::struct_excessive_bools,
    clippy::fn_params_excessive_bools
)]

use std::collections::HashMap;
use std::rc::Rc;

use crate::error::Result;
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::eval::{eval, eval_pred};
use crate::executor::instrument::Instrumentation;
use crate::executor::mem::estimate_row_bytes;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::logical::JoinKind;
use crate::planner::physical::{FilterCounter, PhysExpr, PhysicalPlan};
use crate::types::hash::HashKey;
use crate::types::{Datum, Row};

/// ハッシュ表のスロット・バケットの添字・印の概算（`05 §8.1`）。
const HASH_ENTRY_OVERHEAD: usize = 48;

/// `Hash Join` を実行する Executor を作る。
pub(in crate::executor) fn build(
    plan: &PhysicalPlan,
    env: &BuildEnv<'_>,
    rewindable: bool,
) -> BoxedExecutor {
    let PhysicalPlan::HashJoin {
        kind,
        left,
        right,
        left_keys,
        right_keys,
        residual,
        build_is_left,
        left_width,
        right_width,
        ..
    } = plan
    else {
        unreachable!("hash_join::build called with {plan:?}");
    };
    let (build_plan, build_keys) = if *build_is_left {
        (&**left, left_keys)
    } else {
        (&**right, right_keys)
    };
    // 表は build 側の子とキー式だけで決まる（C-6）。キー式の `Param` / SubLink も見る。
    let key_probe = PhysicalPlan::Project {
        input: Box::new(PhysicalPlan::Values { rows: Vec::new() }),
        exprs: build_keys.clone(),
    };
    let reuse = free_params(build_plan, env.query).is_empty()
        && free_params(&key_probe, env.query).is_empty();
    let (left_exec, right_exec) = (
        build_scoped(left, env, rewindable),
        build_scoped(right, env, rewindable),
    );
    let (probe, build, probe_keys, build_keys) = if *build_is_left {
        (right_exec, left_exec, right_keys, left_keys)
    } else {
        (left_exec, right_exec, left_keys, right_keys)
    };
    Box::new(HashJoinExec::new(
        HashJoinSpec {
            kind: *kind,
            build_is_left: *build_is_left,
            left_width: *left_width,
            right_width: *right_width,
            probe_keys: probe_keys.clone(),
            build_keys: build_keys.clone(),
            residual: residual.clone(),
            reuse,
            rewindable,
        },
        probe,
        build,
    ))
}

/// probe 側の行を（不一致でも）保存する種類。
fn probe_preserved(kind: JoinKind, build_is_left: bool) -> bool {
    match kind {
        JoinKind::Full => true,
        JoinKind::Left | JoinKind::Anti => !build_is_left,
        JoinKind::Inner | JoinKind::Semi => false,
    }
}

/// build 側の行を保存し、終了段で出す種類。
fn build_preserved(kind: JoinKind, build_is_left: bool) -> bool {
    match kind {
        JoinKind::Full => true,
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti => build_is_left,
        JoinKind::Inner => false,
    }
}

struct BuildRow {
    row: Row,
    matched: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Init,
    Probe,
    /// 終了段（build 側の行を添字から順に見る）。
    Finish(usize),
    Done,
}

pub struct HashJoinExec {
    kind: JoinKind,
    build_is_left: bool,
    left_width: usize,
    right_width: usize,
    probe: BoxedExecutor,
    build: BoxedExecutor,
    probe_keys: Vec<PhysExpr>,
    build_keys: Vec<PhysExpr>,
    residual: Option<PhysExpr>,
    /// `rewind` で表を保持してよいか（build 側の部分木とキー式が外側の `Param` に依存しない）。
    reuse: bool,
    rewindable: bool,
    pp: bool,
    bp: bool,
    /// BP の種類のときだけ、キーが NULL の行も保持する。
    rows: Vec<BuildRow>,
    table: HashMap<HashKey, usize>,
    /// 挿入順の `rows` の添字（出力順を決定的にする）。
    buckets: Vec<Vec<usize>>,
    built: bool,
    charged: usize,
    phase: Phase,
    cur: Option<Row>,
    bucket: Option<usize>,
    pos: usize,
    cur_matched: bool,
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl std::fmt::Debug for HashJoinExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashJoinExec")
            .field("kind", &self.kind)
            .field("build_is_left", &self.build_is_left)
            .field("reuse", &self.reuse)
            .finish_non_exhaustive()
    }
}

/// `keys` を `row` に対して評価する。2 つ目の値は「NULL を含むか」。
fn eval_keys(keys: &[PhysExpr], row: &Row, ctx: &mut ExecCtx<'_>) -> Result<(Vec<Datum>, bool)> {
    let vals = keys
        .iter()
        .map(|e| eval(e, row, ctx))
        .collect::<Result<Vec<_>>>()?;
    let has_null = vals.iter().any(Datum::is_null);
    Ok((vals, has_null))
}

fn nulls(n: usize) -> impl Iterator<Item = Datum> {
    std::iter::repeat_n(Datum::Null, n)
}

/// `HashJoinExec::new` の静的な設定。
#[derive(Debug)]
pub struct HashJoinSpec {
    pub kind: JoinKind,
    pub build_is_left: bool,
    pub left_width: usize,
    pub right_width: usize,
    /// probe 行に対して評価する。
    pub probe_keys: Vec<PhysExpr>,
    /// build 行に対して評価する。
    pub build_keys: Vec<PhysExpr>,
    /// `left ++ right` に対して評価する。
    pub residual: Option<PhysExpr>,
    /// `rewind` で表を保持してよいか。
    pub reuse: bool,
    pub rewindable: bool,
}

impl HashJoinExec {
    pub fn new(spec: HashJoinSpec, probe: BoxedExecutor, build: BoxedExecutor) -> Self {
        let HashJoinSpec {
            kind,
            build_is_left,
            left_width,
            right_width,
            probe_keys,
            build_keys,
            residual,
            reuse,
            rewindable,
        } = spec;
        HashJoinExec {
            kind,
            build_is_left,
            left_width,
            right_width,
            probe,
            build,
            probe_keys,
            build_keys,
            residual,
            reuse,
            rewindable,
            pp: probe_preserved(kind, build_is_left),
            bp: build_preserved(kind, build_is_left),
            rows: Vec::new(),
            table: HashMap::new(),
            buckets: Vec::new(),
            built: false,
            charged: 0,
            phase: Phase::Init,
            cur: None,
            bucket: None,
            pos: 0,
            cur_matched: false,
            counters: None,
        }
    }

    fn build_table(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        while let Some(r) = self.build.next(ctx)? {
            ctx.check_interrupts()?;
            let (key, has_null) = eval_keys(&self.build_keys, &r, ctx)?;
            if has_null {
                // 一致できない。BP の種類は不一致として終了段で出すので保持する。
                if self.bp {
                    let bytes = estimate_row_bytes(&r);
                    ctx.mem.charge(bytes)?;
                    self.charged += bytes;
                    self.rows.push(BuildRow {
                        row: r,
                        matched: false,
                    });
                }
                continue;
            }
            let bytes = estimate_row_bytes(&r) + estimate_row_bytes(&key) + HASH_ENTRY_OVERHEAD;
            ctx.mem.charge(bytes)?;
            self.charged += bytes;
            let idx = self.rows.len();
            self.rows.push(BuildRow {
                row: r,
                matched: false,
            });
            let next_bucket = self.buckets.len();
            let b = *self.table.entry(HashKey(key)).or_insert(next_bucket);
            if b == next_bucket {
                self.buckets.push(Vec::new());
            }
            self.buckets[b].push(idx);
        }
        self.built = true;
        Ok(())
    }

    /// 構築後の最初の段階。構築側が空で probe 行を出す必要がなければ probe を読まない。
    fn phase_after_build(&self) -> Phase {
        if self.rows.is_empty() && !self.pp {
            Phase::Finish(0)
        } else {
            Phase::Probe
        }
    }

    fn count_removed(&self) {
        if let Some((id, instr)) = &self.counters {
            instr.add_removed(*id, FilterCounter::JoinFilter, 1);
        }
    }

    /// `left ++ right` の並びに連結する。
    fn join_rows(&self, build_row: &Row, probe_row: &Row) -> Row {
        let (l, r) = if self.build_is_left {
            (build_row, probe_row)
        } else {
            (probe_row, build_row)
        };
        let mut out = Vec::with_capacity(l.len() + r.len());
        out.extend_from_slice(l);
        out.extend_from_slice(r);
        out
    }

    /// probe 行 `p` が不一致のとき（PP の種類）に出す行。
    fn unmatched_probe(&self, p: Row) -> Row {
        match self.kind {
            _ if self.build_is_left => {
                // P = right、build（left）側を NULL にする。
                nulls(self.left_width).chain(p).collect()
            }
            _ => p.into_iter().chain(nulls(self.right_width)).collect(),
        }
    }

    /// build 行 `b` が不一致のとき（BP の種類）に出す行。
    fn unmatched_build(&self, b: &Row) -> Row {
        if self.build_is_left {
            b.iter().cloned().chain(nulls(self.right_width)).collect()
        } else {
            nulls(self.left_width).chain(b.iter().cloned()).collect()
        }
    }

    fn finish_phase(&mut self, ctx: &mut ExecCtx<'_>, mut i: usize) -> Result<Option<Row>> {
        while i < self.rows.len() {
            ctx.check_interrupts()?;
            let b = &self.rows[i];
            let out = match self.kind {
                JoinKind::Left | JoinKind::Full if self.bp && !b.matched => {
                    Some(self.unmatched_build(&b.row))
                }
                JoinKind::Semi if self.build_is_left && b.matched => Some(b.row.clone()),
                JoinKind::Anti if self.build_is_left && !b.matched => Some(b.row.clone()),
                _ => None,
            };
            i += 1;
            if out.is_some() {
                self.phase = Phase::Finish(i);
                return Ok(out);
            }
        }
        self.phase = Phase::Done;
        if !self.rewindable {
            // 読み切ったら課金を返す（D5-22）。次の `rewind` は構築し直す。
            self.release(ctx);
        }
        Ok(None)
    }

    fn release(&mut self, ctx: &mut ExecCtx<'_>) {
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.rows = Vec::new();
        self.table = HashMap::new();
        self.buckets = Vec::new();
        self.built = false;
    }
}

impl Executor for HashJoinExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        loop {
            match self.phase {
                Phase::Done => return Ok(None),
                Phase::Init => {
                    self.build_table(ctx)?;
                    self.phase = self.phase_after_build();
                }
                Phase::Finish(i) => return self.finish_phase(ctx, i),
                Phase::Probe => {
                    if let Some(out) = self.probe_step(ctx)? {
                        return Ok(Some(out));
                    }
                }
            }
        }
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.reuse && self.built {
            for r in &mut self.rows {
                r.matched = false;
            }
            self.phase = self.phase_after_build();
        } else {
            ctx.mem.release(std::mem::take(&mut self.charged));
            self.rows.clear();
            self.table.clear();
            self.buckets.clear();
            self.built = false;
            self.phase = Phase::Init;
            self.build.rewind(ctx)?;
        }
        self.probe.rewind(ctx)?;
        self.cur = None;
        self.bucket = None;
        self.pos = 0;
        self.cur_matched = false;
        Ok(())
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.counters = Some((id, Rc::clone(instr)));
    }
}

impl HashJoinExec {
    /// probe を 1 歩進める。出力があれば `Some`。`None` は「まだ続ける」（`phase` が変わることもある）。
    fn probe_step(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if self.cur.is_none() {
            let Some(p) = self.probe.next(ctx)? else {
                self.phase = Phase::Finish(0);
                return Ok(None);
            };
            let (key, has_null) = eval_keys(&self.probe_keys, &p, ctx)?;
            self.bucket = if has_null {
                None
            } else {
                self.table.get(&HashKey(key)).copied()
            };
            self.cur = Some(p);
            self.pos = 0;
            self.cur_matched = false;
        }
        let single_match =
            matches!(self.kind, JoinKind::Semi | JoinKind::Anti) && !self.build_is_left;
        let emit_join = matches!(self.kind, JoinKind::Inner | JoinKind::Left | JoinKind::Full);
        if let Some(b) = self.bucket {
            while self.pos < self.buckets[b].len() {
                let i = self.buckets[b][self.pos];
                self.pos += 1;
                ctx.check_interrupts()?;
                let Some(p) = self.cur.as_ref() else {
                    break;
                };
                // SEMI / ANTI（probe 側を出す）で residual が無ければ、連結した行は要らない。
                let joined = if emit_join || self.residual.is_some() {
                    Some(self.join_rows(&self.rows[i].row, p))
                } else {
                    None
                };
                if let (Some(res), Some(j)) = (&self.residual, &joined)
                    && eval_pred(res, j, ctx)? != Some(true)
                {
                    self.count_removed();
                    continue;
                }
                self.cur_matched = true;
                if self.bp {
                    self.rows[i].matched = true;
                }
                if emit_join {
                    return Ok(joined);
                }
                if single_match {
                    break;
                }
            }
        }
        // この probe 行の bucket を見終えた。
        let Some(p) = self.cur.take() else {
            return Ok(None);
        };
        self.bucket = None;
        Ok(match self.kind {
            JoinKind::Semi if !self.build_is_left && self.cur_matched => Some(p),
            JoinKind::Anti if !self.build_is_left && !self.cur_matched => Some(p),
            JoinKind::Left | JoinKind::Full if self.pp && !self.cur_matched => {
                Some(self.unmatched_probe(p))
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{col, param};
    use crate::executor::nodes::nested_loop::testing::*;
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::expr::ParamId;
    use crate::types::SqlType;

    const KINDS: [JoinKind; 5] = [
        JoinKind::Inner,
        JoinKind::Left,
        JoinKind::Full,
        JoinKind::Semi,
        JoinKind::Anti,
    ];

    fn hj_plan(
        kind: JoinKind,
        l: &[Pair],
        r: &[Pair],
        build_is_left: bool,
        residual: bool,
    ) -> PhysicalPlan {
        PhysicalPlan::HashJoin {
            kind,
            left: Box::new(pairs_plan(l)),
            right: Box::new(pairs_plan(r)),
            left_keys: vec![col(0, SqlType::INT4)],
            right_keys: vec![col(0, SqlType::INT4)],
            key_types: vec![SqlType::INT4],
            residual: residual.then(val_gt),
            build_is_left,
            left_width: 2,
            right_width: 2,
        }
    }

    /// `HashJoinExec` を直接作る（`left` / `right` の実行器を差し込む）。
    fn exec(
        kind: JoinKind,
        build_is_left: bool,
        left: RowsExec,
        right: RowsExec,
        residual: bool,
        reuse: bool,
        rewindable: bool,
    ) -> HashJoinExec {
        let (probe, build) = if build_is_left {
            (right, left)
        } else {
            (left, right)
        };
        HashJoinExec::new(
            HashJoinSpec {
                kind,
                build_is_left,
                left_width: 2,
                right_width: 2,
                probe_keys: vec![col(0, SqlType::INT4)],
                build_keys: vec![col(0, SqlType::INT4)],
                residual: residual.then(val_gt),
                reuse,
                rewindable,
            },
            Box::new(probe),
            Box::new(build),
        )
    }

    /// 全 kind × `build_is_left` × residual × 200 シード（NULL キー・重複キー・片側または両側が空）。
    #[test]
    fn hash_join_agrees_with_the_reference_in_every_combination() {
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
            let (nl, nr) = match seed {
                1 => (0, 0),
                2 => (0, 7),
                3 => (7, 0),
                _ => (
                    usize::try_from(rng.below(41)).unwrap(),
                    usize::try_from(rng.below(41)).unwrap(),
                ),
            };
            let (l, r) = (rng.rows(nl), rng.rows(nr));
            for kind in KINDS {
                for residual in [false, true] {
                    let want = sorted(reference(kind, &l, &r, residual));
                    for build_is_left in [false, true] {
                        let plan = hj_plan(kind, &l, &r, build_is_left, residual);
                        let got = sorted(run_plan(&plan, 0).unwrap());
                        assert_eq!(
                            got, want,
                            "seed={seed} kind={kind:?} build_is_left={build_is_left} residual={residual}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn output_order_is_probe_order_then_build_order() {
        // left がプローブ、right がビルド（INNER）。right の挿入順に並ぶ。
        let l = [(Some(1), Some(10)), (Some(2), Some(20))];
        let r = [(Some(2), Some(1)), (Some(1), Some(2)), (Some(1), Some(3))];
        let got = run_plan(&hj_plan(JoinKind::Inner, &l, &r, false, false), 0).unwrap();
        let firsts: Vec<_> = got.iter().map(|x| (x[1].clone(), x[3].clone())).collect();
        assert_eq!(
            firsts,
            vec![
                (Datum::Int4(10), Datum::Int4(2)),
                (Datum::Int4(10), Datum::Int4(3)),
                (Datum::Int4(20), Datum::Int4(1)),
            ]
        );
    }

    #[test]
    fn null_keys_never_match_but_are_kept_as_unmatched() {
        let l = [(None, Some(1)), (Some(1), Some(1))];
        let r = [(None, Some(0)), (Some(1), Some(0))];
        let got = sorted(run_plan(&hj_plan(JoinKind::Full, &l, &r, false, false), 0).unwrap());
        assert_eq!(got.len(), 3);
        for b in [false, true] {
            let anti = run_plan(&hj_plan(JoinKind::Anti, &l, &r, b, false), 0).unwrap();
            assert_eq!(anti, vec![pair_row((None, Some(1)))], "build_is_left={b}");
            let semi = run_plan(&hj_plan(JoinKind::Semi, &l, &r, b, false), 0).unwrap();
            assert_eq!(
                semi,
                vec![pair_row((Some(1), Some(1)))],
                "build_is_left={b}"
            );
        }
    }

    #[test]
    fn empty_build_side_skips_the_probe_unless_it_must_emit_probe_rows() {
        let probe_rows = [(Some(1), Some(1)), (Some(2), Some(2))];
        for (kind, reads_probe) in [
            (JoinKind::Inner, false),
            (JoinKind::Semi, false),
            (JoinKind::Left, true),
            (JoinKind::Full, true),
            (JoinKind::Anti, true),
        ] {
            let left = RowsExec::pairs(&probe_rows);
            let right = RowsExec::pairs(&[]);
            let (reads, _) = left.counters();
            let mut f = Fixture::new();
            let mut e: BoxedExecutor = Box::new(exec(kind, false, left, right, false, true, true));
            let rows = f.run(&mut e).unwrap();
            assert_eq!(reads.get() > 0, reads_probe, "{kind:?}");
            assert_eq!(rows.len(), if reads_probe { 2 } else { 0 }, "{kind:?}");
        }
    }

    #[test]
    fn rewind_reuses_the_table_when_allowed_and_rebuilds_otherwise() {
        let l = [(Some(1), Some(5)), (Some(2), Some(5)), (Some(9), Some(5))];
        let r = [(Some(1), Some(1)), (Some(2), Some(1)), (Some(2), Some(2))];
        for kind in KINDS {
            for reuse in [true, false] {
                let left = RowsExec::pairs(&l);
                let right = RowsExec::pairs(&r);
                let (build_reads, build_rewinds) = right.counters();
                let (_, probe_rewinds) = left.counters();
                let mut f = Fixture::new();
                let mut ctx = f.ctx();
                let mut e: BoxedExecutor =
                    Box::new(exec(kind, false, left, right, true, reuse, true));
                let first = sorted(drain(&mut e, &mut ctx).unwrap());
                let used = ctx.mem.used();
                assert_eq!(build_reads.get(), 3);
                e.rewind(&mut ctx).unwrap();
                let second = sorted(drain(&mut e, &mut ctx).unwrap());
                assert_eq!(first, second, "{kind:?} reuse={reuse}");
                assert_eq!(probe_rewinds.get(), 1);
                if reuse {
                    assert_eq!((build_reads.get(), build_rewinds.get()), (3, 0), "{kind:?}");
                } else {
                    assert_eq!((build_reads.get(), build_rewinds.get()), (6, 1), "{kind:?}");
                }
                // 再構築しても課金は積み上がらない。
                assert_eq!(ctx.mem.used(), used, "{kind:?} reuse={reuse}");
            }
        }
    }

    #[test]
    fn non_rewindable_returns_the_charge_after_the_last_row() {
        for kind in KINDS {
            let left = RowsExec::pairs(&[(Some(1), Some(1)), (None, Some(2))]);
            let right = RowsExec::pairs(&[(Some(1), Some(0)), (None, Some(0)), (Some(3), Some(0))]);
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let mut e: BoxedExecutor = Box::new(exec(kind, true, left, right, false, true, false));
            let _ = drain(&mut e, &mut ctx).unwrap();
            assert_eq!(ctx.mem.used(), 0, "{kind:?}");
            assert!(e.next(&mut ctx).unwrap().is_none());
        }
        // rewindable なら保持する。
        let left = RowsExec::pairs(&[(Some(1), Some(1))]);
        let right = RowsExec::pairs(&[(Some(1), Some(0))]);
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor =
            Box::new(exec(JoinKind::Inner, false, left, right, false, true, true));
        let _ = drain(&mut e, &mut ctx).unwrap();
        assert!(ctx.mem.used() > 0);
    }

    #[test]
    fn memory_charge_matches_the_estimate_and_the_limit_is_exact() {
        let r = [(Some(1), Some(1)), (Some(2), Some(2)), (None, Some(3))];
        // NULL キーの行は INNER では保持しない。
        let expected: usize = r
            .iter()
            .filter(|p| p.0.is_some())
            .map(|p| {
                estimate_row_bytes(&pair_row(*p))
                    + estimate_row_bytes(&vec![opt(p.0)])
                    + HASH_ENTRY_OVERHEAD
            })
            .sum();
        let run = |limit: usize| {
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let mut ctx = f.ctx();
            let mut e: BoxedExecutor = Box::new(exec(
                JoinKind::Inner,
                false,
                RowsExec::pairs(&[(Some(1), Some(0))]),
                RowsExec::pairs(&r),
                false,
                true,
                true,
            ));
            let res = drain(&mut e, &mut ctx);
            (res, ctx.mem.used())
        };
        let (ok, used) = run(expected);
        assert!(ok.is_ok());
        assert_eq!(used, expected);
        let (err, _) = run(expected - 1);
        let err = err.unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::OUT_OF_MEMORY);
        assert!(err.message.starts_with("out of memory"));
    }

    #[test]
    fn counts_rows_removed_by_residual() {
        use crate::executor::instrument::Instrumentation;
        // 一致候補 4 組のうち、residual（left.val > right.val）が真になるのは 1 組。
        let left = RowsExec::pairs(&[(Some(1), Some(5)), (Some(1), Some(0))]);
        let right = RowsExec::pairs(&[(Some(1), Some(1)), (Some(1), Some(9))]);
        let mut f = Fixture::new();
        let mut e: BoxedExecutor =
            Box::new(exec(JoinKind::Inner, false, left, right, true, true, true));
        let instr = Rc::new(Instrumentation::new(1));
        e.set_counters(0, &instr);
        assert_eq!(f.run(&mut e).unwrap().len(), 1);
        assert_eq!(instr.node(0).removed_join_filter.get(), 3);
    }

    #[test]
    fn interrupts_are_checked_in_build_probe_and_bucket_loops() {
        let many = |n: usize, key: i32| vec![(Some(key), Some(0)); n];
        // 構築: ビルド側の 3 行目で要求 → 構築ループが止まる。
        let (probe, build) = (
            RowsExec::pairs(&many(1, 1)),
            RowsExec::pairs(&many(50, 1)).interrupt_at(3),
        );
        let (reads, _) = build.counters();
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(exec(
            JoinKind::Inner,
            false,
            probe,
            build,
            false,
            true,
            true,
        ));
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            sqlstate::ADMIN_SHUTDOWN
        );
        assert_eq!(reads.get(), 3);
        // probe: 一致しない probe 行の 3 行目で要求 → probe ループが止まる。
        let (probe, build) = (
            RowsExec::pairs(&many(50, 9)).interrupt_at(3),
            RowsExec::pairs(&many(1, 1)),
        );
        let (reads, _) = probe.counters();
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(exec(
            JoinKind::Inner,
            false,
            probe,
            build,
            false,
            true,
            true,
        ));
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            sqlstate::ADMIN_SHUTDOWN
        );
        assert_eq!(reads.get(), 3);
        // bucket: probe 1 行が要求を立て、同じキーの 50 行の候補を走査する前に止まる。
        let (probe, build) = (
            RowsExec::pairs(&many(1, 1)).interrupt_at(1),
            RowsExec::pairs(&many(50, 1)),
        );
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(exec(
            JoinKind::Inner,
            false,
            probe,
            build,
            false,
            true,
            true,
        ));
        let mut ctx = f.ctx();
        assert_eq!(
            e.next(&mut ctx).unwrap_err().sqlstate,
            sqlstate::ADMIN_SHUTDOWN
        );
    }

    /// build 側のキー式が `Param` を読むなら、`rewind` で再利用しない（C-6）。
    #[test]
    fn build_reuse_follows_free_params_of_the_build_side_and_keys() {
        let mk = |build_keys: Vec<PhysExpr>, right: PhysicalPlan| PhysicalPlan::HashJoin {
            kind: JoinKind::Inner,
            left: Box::new(pairs_plan(&[(Some(1), Some(1))])),
            right: Box::new(right),
            left_keys: vec![col(0, SqlType::INT4)],
            right_keys: build_keys,
            key_types: vec![SqlType::INT4],
            residual: None,
            build_is_left: false,
            left_width: 2,
            right_width: 2,
        };
        let build_reuse = |plan: &PhysicalPlan| {
            let mut f = Fixture::with_params(1);
            let mut ctx = f.ctx();
            let mut e = crate::executor::build::build(plan);
            // 1 回目: $0 = 1。
            ctx.set_param(ParamId(0), Datum::Int4(1)).unwrap();
            let a = drain(&mut e, &mut ctx).unwrap().len();
            ctx.set_param(ParamId(0), Datum::Int4(2)).unwrap();
            e.rewind(&mut ctx).unwrap();
            let b = drain(&mut e, &mut ctx).unwrap().len();
            (a, b)
        };
        let plain = pairs_plan(&[(Some(1), Some(0))]);
        // どちらも Param に依存しない: 同じ結果。
        assert_eq!(
            build_reuse(&mk(vec![col(0, SqlType::INT4)], plain.clone())),
            (1, 1)
        );
        // キー式が Param を読む: $0 = 1 なら一致、$0 = 2 なら不一致（再利用していたら 1 のまま）。
        assert_eq!(
            build_reuse(&mk(vec![param(0, SqlType::INT4)], plain)),
            (1, 0)
        );
        // build 側の子が Param に依存する場合も作り直す。
        let dependent = PhysicalPlan::Filter {
            input: Box::new(pairs_plan(&[(Some(1), Some(0)), (Some(2), Some(0))])),
            predicate: crate::executor::eval::tests::op(
                &crate::executor::eval::tests::EQ,
                col(0, SqlType::INT4),
                param(0, SqlType::INT4),
            ),
        };
        assert_eq!(
            build_reuse(&mk(vec![col(0, SqlType::INT4)], dependent)),
            (1, 0)
        );
    }
}
