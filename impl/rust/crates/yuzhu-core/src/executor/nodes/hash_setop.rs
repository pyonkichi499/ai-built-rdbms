//! HashSetOp ノード（INTERSECT / EXCEPT [ALL]、UNION も実装）。`m4/05` §5.20、D5-13。
//!
//! 左を全部読み、次に右を全部読む。行全体を `HashKey`（NULL どうしは等しい）にして、
//! `Entry { row, left, right }` を初出順に持つ。出力は entry ごとの個数（`entry_count`）だけ行を繰り返す。
//! 新しい entry ごとに `2 * estimate_row_bytes(row) + HASH_ENTRY_OVERHEAD` を課金する。
//! `rewind`: 両方の子が `reuse` なら出力位置を戻す。そうでなければ構築し直す（課金を返す）。
//! `rewindable = false` なら出力し終えた時点で課金を返す。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::collections::HashMap;

use crate::analyzer::query::SetOpKind;
use crate::error::Result;
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::mem::{HASH_ENTRY_OVERHEAD, estimate_row_bytes};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::Row;
use crate::types::hash::HashKey;

struct Entry {
    row: Row,
    left: u64,
    right: u64,
}

#[allow(clippy::struct_excessive_bools)]
pub struct HashSetOpExec {
    op: SetOpKind,
    all: bool,
    left: BoxedExecutor,
    right: BoxedExecutor,
    reuse: bool,
    rewindable: bool,
    entries: Vec<Entry>,
    built: bool,
    /// 出力中の entry の添字と、その entry の残りの個数。
    pos: usize,
    remaining: u64,
    charged: usize,
}

impl std::fmt::Debug for HashSetOpExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashSetOpExec")
            .field("all", &self.all)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// entry が出力に出す行数。
fn entry_count(op: SetOpKind, all: bool, left: u64, right: u64) -> u64 {
    match op {
        SetOpKind::Intersect => {
            if left > 0 && right > 0 {
                if all { left.min(right) } else { 1 }
            } else {
                0
            }
        }
        SetOpKind::Except => {
            if all {
                left.saturating_sub(right)
            } else {
                u64::from(left > 0 && right == 0)
            }
        }
        SetOpKind::Union => {
            if all {
                left + right
            } else {
                u64::from(left + right > 0)
            }
        }
    }
}

impl HashSetOpExec {
    pub fn new(
        op: SetOpKind,
        all: bool,
        left: BoxedExecutor,
        right: BoxedExecutor,
        reuse: bool,
        rewindable: bool,
    ) -> Self {
        HashSetOpExec {
            op,
            all,
            left,
            right,
            reuse,
            rewindable,
            entries: Vec::new(),
            built: false,
            pos: 0,
            remaining: 0,
            charged: 0,
        }
    }

    fn build_entries(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        let mut index: HashMap<HashKey, usize> = HashMap::new();
        while let Some(row) = self.left.next(ctx)? {
            ctx.check_interrupts()?;
            let key = HashKey(row);
            if let Some(&i) = index.get(&key) {
                self.entries[i].left += 1;
                continue;
            }
            let bytes = 2 * estimate_row_bytes(&key.0) + HASH_ENTRY_OVERHEAD;
            ctx.mem.charge(bytes)?;
            self.charged += bytes;
            self.entries.push(Entry {
                row: key.0.clone(),
                left: 1,
                right: 0,
            });
            index.insert(key, self.entries.len() - 1);
        }
        while let Some(row) = self.right.next(ctx)? {
            ctx.check_interrupts()?;
            let key = HashKey(row);
            if let Some(&i) = index.get(&key) {
                self.entries[i].right += 1;
            } else if matches!(self.op, SetOpKind::Union) {
                let bytes = 2 * estimate_row_bytes(&key.0) + HASH_ENTRY_OVERHEAD;
                ctx.mem.charge(bytes)?;
                self.charged += bytes;
                self.entries.push(Entry {
                    row: key.0.clone(),
                    left: 0,
                    right: 1,
                });
                index.insert(key, self.entries.len() - 1);
            }
        }
        Ok(())
    }

    fn drop_entries(&mut self, ctx: &ExecCtx<'_>) {
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.entries = Vec::new();
    }
}

impl Executor for HashSetOpExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if !self.built {
            self.build_entries(ctx)?;
            self.built = true;
            self.pos = 0;
            self.remaining = 0;
            if let Some(e) = self.entries.first() {
                self.remaining = entry_count(self.op, self.all, e.left, e.right);
            }
        }
        loop {
            let Some(e) = self.entries.get(self.pos) else {
                return Ok(None);
            };
            if self.remaining > 0 {
                self.remaining -= 1;
                let row = e.row.clone();
                if self.remaining == 0 {
                    self.advance(ctx);
                }
                return Ok(Some(row));
            }
            self.advance(ctx);
        }
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.built && self.reuse && self.rewindable {
            self.pos = 0;
            self.remaining = self
                .entries
                .first()
                .map_or(0, |e| entry_count(self.op, self.all, e.left, e.right));
            return Ok(());
        }
        self.drop_entries(ctx);
        self.built = false;
        self.pos = 0;
        self.remaining = 0;
        self.left.rewind(ctx)?;
        self.right.rewind(ctx)
    }
}

impl HashSetOpExec {
    /// 次の entry へ進み、その個数を用意する。最後を過ぎ、`rewindable = false` なら課金を返す。
    fn advance(&mut self, ctx: &ExecCtx<'_>) {
        self.pos += 1;
        if let Some(e) = self.entries.get(self.pos) {
            self.remaining = entry_count(self.op, self.all, e.left, e.right);
            return;
        }
        self.remaining = 0;
        if !self.rewindable {
            self.drop_entries(ctx);
            // 以降は常に None（`entries` は空、`pos` は範囲外）。
            self.pos = 0;
        }
    }
}

pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::HashSetOp {
        op,
        all,
        left,
        right,
        ..
    } = plan
    else {
        return Box::new(super::UnsupportedExec::new("HashSetOp"));
    };
    Box::new(HashSetOpExec::new(
        *op,
        *all,
        build_scoped(left, env, rewindable),
        build_scoped(right, env, rewindable),
        free_params(plan, env.query).is_empty(),
        rewindable,
    ))
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::Datum;

    /// `Some(i)` は整数、`None` は NULL の 1 列の行。
    fn rows(v: &[Option<i32>]) -> Vec<Row> {
        v.iter()
            .map(|i| vec![i.map_or(Datum::Null, Datum::Int4)])
            .collect()
    }

    fn setop(op: SetOpKind, all: bool, l: &[Option<i32>], r: &[Option<i32>]) -> Vec<Option<i32>> {
        let mut f = Fixture::new();
        let (a, _, _) = CountingExec::new(rows(l));
        let (b, _, _) = CountingExec::new(rows(r));
        let mut e: BoxedExecutor = Box::new(HashSetOpExec::new(
            op,
            all,
            Box::new(a),
            Box::new(b),
            true,
            false,
        ));
        f.run(&mut e)
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_i64().map(|v| i32::try_from(v).unwrap()))
            .collect()
    }

    fn left() -> Vec<Option<i32>> {
        vec![Some(1), Some(1), Some(1), Some(2), Some(2), None, None]
    }

    fn right() -> Vec<Option<i32>> {
        vec![Some(1), Some(1), Some(2), Some(2), Some(2), None]
    }

    fn sorted(mut v: Vec<Option<i32>>) -> Vec<Option<i32>> {
        v.sort();
        v
    }

    /// 05 §5.20 の実機の照合値。
    #[test]
    fn intersect_and_except_match_postgres() {
        assert_eq!(
            sorted(setop(SetOpKind::Intersect, true, &left(), &right())),
            vec![None, Some(1), Some(1), Some(2), Some(2)]
        );
        assert_eq!(
            sorted(setop(SetOpKind::Except, true, &left(), &right())),
            vec![None, Some(1)]
        );
        assert!(setop(SetOpKind::Except, false, &left(), &right()).is_empty());
        assert_eq!(
            sorted(setop(SetOpKind::Intersect, false, &left(), &right())),
            vec![None, Some(1), Some(2)]
        );
    }

    #[test]
    fn output_is_in_first_seen_order() {
        assert_eq!(
            setop(SetOpKind::Intersect, false, &left(), &right()),
            vec![Some(1), Some(2), None]
        );
        assert_eq!(
            setop(
                SetOpKind::Except,
                false,
                &[Some(3), Some(1), Some(2)],
                &[Some(1)]
            ),
            vec![Some(3), Some(2)]
        );
    }

    #[test]
    fn union_distinct_and_all() {
        assert_eq!(
            setop(
                SetOpKind::Union,
                false,
                &[Some(1), Some(1), None],
                &[None, Some(2), Some(1)]
            ),
            vec![Some(1), None, Some(2)]
        );
        assert_eq!(
            sorted(setop(
                SetOpKind::Union,
                true,
                &[Some(1), Some(1), None],
                &[None, Some(2), Some(1)]
            )),
            vec![None, None, Some(1), Some(1), Some(1), Some(2)]
        );
    }

    #[test]
    fn empty_sides() {
        assert!(setop(SetOpKind::Intersect, true, &[], &right()).is_empty());
        assert!(setop(SetOpKind::Intersect, true, &left(), &[]).is_empty());
        assert!(setop(SetOpKind::Except, true, &[], &right()).is_empty());
        assert_eq!(
            sorted(setop(SetOpKind::Except, true, &[Some(1), Some(1)], &[])),
            vec![Some(1), Some(1)]
        );
        assert_eq!(
            setop(SetOpKind::Except, false, &[Some(1), Some(1)], &[]),
            vec![Some(1)]
        );
        assert!(setop(SetOpKind::Union, true, &[], &[]).is_empty());
    }

    #[test]
    fn all_variants_agree_with_a_naive_reference() {
        // 小さな全組み合わせで、素朴な多重集合の計算と比べる。
        let vals = [None, Some(1), Some(2)];
        let mut seed = 12345u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let gen_side = |n: &mut dyn FnMut() -> u64| -> Vec<Option<i32>> {
                (0..(n() % 7)).map(|_| vals[(n() % 3) as usize]).collect()
            };
            let l = gen_side(&mut next);
            let r = gen_side(&mut next);
            for all in [false, true] {
                let count =
                    |v: &[Option<i32>], x: Option<i32>| v.iter().filter(|y| **y == x).count();
                let mut want_i = vec![];
                let mut want_e = vec![];
                for x in vals {
                    let (a, b) = (count(&l, x), count(&r, x));
                    let (ni, ne) = if all {
                        (a.min(b), a.saturating_sub(b))
                    } else {
                        (usize::from(a > 0 && b > 0), usize::from(a > 0 && b == 0))
                    };
                    want_i.extend(std::iter::repeat_n(x, ni));
                    want_e.extend(std::iter::repeat_n(x, ne));
                }
                assert_eq!(
                    sorted(setop(SetOpKind::Intersect, all, &l, &r)),
                    sorted(want_i)
                );
                assert_eq!(
                    sorted(setop(SetOpKind::Except, all, &l, &r)),
                    sorted(want_e)
                );
            }
        }
    }

    #[test]
    fn rewind_reuses_or_rebuilds() {
        for (reuse, want_reads, want_rewinds) in [(true, 4, 0), (false, 8, 2)] {
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let (a, ra, rwa) = CountingExec::new(rows(&[Some(1), Some(2)]));
            let (b, rb, rwb) = CountingExec::new(rows(&[Some(2), Some(3)]));
            let mut e: BoxedExecutor = Box::new(HashSetOpExec::new(
                SetOpKind::Union,
                false,
                Box::new(a),
                Box::new(b),
                reuse,
                true,
            ));
            let first = drain(&mut e, &mut ctx).unwrap();
            assert_eq!(first.len(), 3);
            e.rewind(&mut ctx).unwrap();
            assert_eq!(drain(&mut e, &mut ctx).unwrap(), first);
            assert_eq!(ra.get() + rb.get(), want_reads);
            assert_eq!(rwa.get() + rwb.get(), want_rewinds);
        }
    }

    #[test]
    fn charges_per_entry_and_releases() {
        let per = 2 * estimate_row_bytes(&vec![Datum::Int4(1)]) + HASH_ENTRY_OVERHEAD;
        let mk = |rewindable| {
            let (a, _, _) = CountingExec::new(rows(&[Some(1), Some(1), Some(2)]));
            let (b, _, _) = CountingExec::new(rows(&[Some(1), Some(9)]));
            HashSetOpExec::new(
                SetOpKind::Intersect,
                false,
                Box::new(a),
                Box::new(b),
                false,
                rewindable,
            )
        };
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e: BoxedExecutor = Box::new(mk(true));
        drain(&mut e, &mut ctx).unwrap();
        // 左の異なる 2 行だけ（Intersect は右だけの行を作らない）。
        assert_eq!(ctx.mem.used(), 2 * per);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        let mut e: BoxedExecutor = Box::new(mk(false));
        drain(&mut e, &mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        assert!(e.next(&mut ctx).unwrap().is_none());
        // 上限ちょうどは成功、1 足りないと失敗。
        for (limit, ok) in [(2 * per, true), (2 * per - 1, false)] {
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let mut e: BoxedExecutor = Box::new(mk(false));
            let r = f.run(&mut e);
            assert_eq!(r.is_ok(), ok);
            if let Err(err) = r {
                assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
            }
        }
    }

    #[test]
    fn interrupts_are_checked_in_both_build_loops() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (a, ra, _) = CountingExec::ints(1000);
        let (b, rb, _) = CountingExec::ints(1000);
        let mut e: BoxedExecutor = Box::new(HashSetOpExec::new(
            SetOpKind::Except,
            false,
            Box::new(a),
            Box::new(b),
            true,
            false,
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(ra.get() + rb.get() <= 1);
    }
}
