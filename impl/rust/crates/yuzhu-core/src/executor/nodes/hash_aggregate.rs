//! HashAggregate ノード。`m4/05` §5.17。
//!
//! 出力: `keys ++ aggs の結果`、グループごとに 1 行。グループの**初出順**（D5-11）。入力が空なら 0 行。
//! 新しいグループごとに `estimate_row_bytes(key) + AggSet::base_bytes() + HASH_ENTRY_OVERHEAD`
//! を課金する（DISTINCT の集合と `min` / `max` の増分は `AggGroup::charged`）。
//! `rewind`: `reuse` なら読みの位置を 0 に戻す。そうでなければ破棄して（課金を返し）子を `rewind`。
//! `rewindable = false` なら最後のグループを返した時点で課金を返す。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::executor::agg::{AggGroup, AggSet};
use crate::executor::build::{BuildEnv, build_scoped, free_params};
use crate::executor::mem::{HASH_ENTRY_OVERHEAD, estimate_row_bytes};
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::planner::physical::{PhysExpr, PhysicalPlan};
use crate::types::Row;
use crate::types::hash::HashKey;

pub struct HashAggregateExec {
    input: BoxedExecutor,
    keys: Vec<PhysExpr>,
    aggs: AggSet,
    reuse: bool,
    rewindable: bool,
    groups: Vec<(Vec<crate::types::Datum>, AggGroup)>,
    built: bool,
    pos: usize,
    /// `ctx.mem` に課金した合計。
    charged: usize,
}

impl std::fmt::Debug for HashAggregateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashAggregateExec")
            .field("keys", &self.keys.len())
            .field("groups", &self.groups.len())
            .finish_non_exhaustive()
    }
}

impl HashAggregateExec {
    pub fn new(
        input: BoxedExecutor,
        keys: Vec<PhysExpr>,
        aggs: AggSet,
        reuse: bool,
        rewindable: bool,
    ) -> Self {
        HashAggregateExec {
            input,
            keys,
            aggs,
            reuse,
            rewindable,
            groups: Vec::new(),
            built: false,
            pos: 0,
            charged: 0,
        }
    }

    fn build_groups(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.keys.is_empty() {
            return Err(Error::internal(
                "HashAggregate needs grouping keys (the planner uses Aggregate otherwise)",
            ));
        }
        let mut index: HashMap<HashKey, usize> = HashMap::new();
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            let key = self
                .keys
                .iter()
                .map(|e| eval(e, &row, ctx))
                .collect::<Result<Vec<_>>>()?;
            let hk = HashKey(key);
            let i = if let Some(&i) = index.get(&hk) {
                i
            } else {
                let bytes =
                    estimate_row_bytes(&hk.0) + self.aggs.base_bytes() + HASH_ENTRY_OVERHEAD;
                ctx.mem.charge(bytes)?;
                self.charged += bytes;
                self.groups.push((hk.0.clone(), self.aggs.new_group()));
                index.insert(hk, self.groups.len() - 1);
                self.groups.len() - 1
            };
            let g = &mut self.groups[i].1;
            let before = g.charged();
            let res = self.aggs.accumulate(g, &row, ctx);
            self.charged += g.charged() - before;
            res?;
        }
        Ok(())
    }

    fn drop_groups(&mut self, ctx: &ExecCtx<'_>) {
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.groups = Vec::new();
    }
}

impl Executor for HashAggregateExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        ctx.check_interrupts()?;
        if !self.built {
            self.build_groups(ctx)?;
            self.built = true;
            self.pos = 0;
        }
        let Some((key, g)) = self.groups.get(self.pos) else {
            return Ok(None);
        };
        let mut row = key.clone();
        row.extend(self.aggs.finish(g)?);
        self.pos += 1;
        if !self.rewindable && self.pos == self.groups.len() {
            self.drop_groups(ctx);
            self.pos = 0;
        }
        Ok(Some(row))
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        if self.built && self.reuse && self.rewindable {
            self.pos = 0;
            return Ok(());
        }
        self.drop_groups(ctx);
        self.built = false;
        self.pos = 0;
        self.input.rewind(ctx)
    }
}

pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::HashAggregate {
        input, keys, aggs, ..
    } = plan
    else {
        return Box::new(super::UnsupportedExec::new("HashAggregate"));
    };
    Box::new(HashAggregateExec::new(
        build_scoped(input, env, rewindable),
        keys.clone(),
        AggSet::new(aggs),
        free_params(plan, env.query).is_empty(),
        rewindable,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::AggKind;
    use crate::executor::agg::test_support::agg;
    use crate::executor::eval::tests::{col, int, lit, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::{Datum, SqlType};

    fn count_sum() -> AggSet {
        AggSet::new(&[
            agg(AggKind::CountStar, vec![], false, None),
            agg(AggKind::SumInt4, vec![col(1, SqlType::INT4)], false, None),
        ])
    }

    fn node(input: BoxedExecutor, reuse: bool, rewindable: bool) -> BoxedExecutor {
        Box::new(HashAggregateExec::new(
            input,
            vec![col(0, SqlType::TEXT)],
            count_sum(),
            reuse,
            rewindable,
        ))
    }

    fn data() -> BoxedExecutor {
        let k = |s: Option<&str>| s.map_or_else(|| null(SqlType::TEXT), text);
        Box::new(ValuesExec::new(vec![
            vec![k(Some("b")), int(1)],
            vec![k(None), int(2)],
            vec![k(Some("a")), int(3)],
            vec![k(None), int(4)],
            vec![k(Some("b")), null(SqlType::INT4)],
        ]))
    }

    #[test]
    fn groups_in_first_seen_order_with_null_key_as_one_group() {
        let mut f = Fixture::new();
        let mut e = node(data(), true, false);
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Text("b".into()), Datum::Int8(2), Datum::Int8(1)],
                vec![Datum::Null, Datum::Int8(2), Datum::Int8(6)],
                vec![Datum::Text("a".into()), Datum::Int8(1), Datum::Int8(3)],
            ]
        );
    }

    #[test]
    fn empty_input_gives_no_rows() {
        let mut f = Fixture::new();
        let mut e = node(Box::new(ValuesExec::new(vec![])), true, false);
        assert!(f.run(&mut e).unwrap().is_empty());
    }

    #[test]
    fn empty_keys_is_an_internal_error() {
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(HashAggregateExec::new(
            data(),
            vec![],
            count_sum(),
            true,
            false,
        ));
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            crate::error::sqlstate::INTERNAL_ERROR
        );
    }

    #[test]
    fn numeric_and_float_keys_follow_cmp_datum() {
        use yuzhu_numeric::Numeric;
        let n = |s: &str| lit(Datum::Numeric(Numeric::parse(s).unwrap()), SqlType::NUMERIC);
        let mut f = Fixture::new();
        let input = Box::new(ValuesExec::new(vec![
            vec![n("1.10"), int(1)],
            vec![n("1.1"), int(1)],
            vec![n("2"), int(1)],
        ]));
        let mut e: BoxedExecutor = Box::new(HashAggregateExec::new(
            input,
            vec![col(0, SqlType::NUMERIC)],
            count_sum(),
            true,
            false,
        ));
        let rows = f.run(&mut e).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Datum::Int8(2));
    }

    #[test]
    fn rewind_reuses_or_rebuilds() {
        for (reuse, want_reads, want_rewinds) in [(true, 3, 0), (false, 6, 1)] {
            let mut f = Fixture::new();
            let mut ctx = f.ctx();
            let (input, reads, rewinds) = CountingExec::new(vec![
                vec![Datum::Text("x".into()), Datum::Int4(1)],
                vec![Datum::Text("y".into()), Datum::Int4(2)],
                vec![Datum::Text("x".into()), Datum::Int4(3)],
            ]);
            let mut e = node(Box::new(input), reuse, true);
            let first = drain(&mut e, &mut ctx).unwrap();
            assert_eq!(first.len(), 2);
            e.rewind(&mut ctx).unwrap();
            assert_eq!(drain(&mut e, &mut ctx).unwrap(), first);
            assert_eq!(reads.get(), want_reads);
            assert_eq!(rewinds.get(), want_rewinds);
        }
    }

    #[test]
    fn charges_per_group_and_releases() {
        let set = count_sum();
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e = node(data(), false, true);
        drain(&mut e, &mut ctx).unwrap();
        let expect: usize = [
            Datum::Text("b".into()),
            Datum::Null,
            Datum::Text("a".into()),
        ]
        .iter()
        .map(|k| estimate_row_bytes(&vec![k.clone()]) + set.base_bytes() + HASH_ENTRY_OVERHEAD)
        .sum();
        assert_eq!(ctx.mem.used(), expect);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        // rewindable = false: 最後のグループを返したら 0。
        let mut e = node(data(), false, false);
        e.next(&mut ctx).unwrap();
        e.next(&mut ctx).unwrap();
        assert!(ctx.mem.used() > 0);
        e.next(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
        assert!(e.next(&mut ctx).unwrap().is_none());
    }

    #[test]
    fn exact_limit_passes_and_one_less_fails() {
        let set = count_sum();
        let per = |k: &Datum| {
            estimate_row_bytes(&vec![k.clone()]) + set.base_bytes() + HASH_ENTRY_OVERHEAD
        };
        let need = per(&Datum::Text("b".into())) * 2 + per(&Datum::Null);
        for (limit, ok) in [(need, true), (need - 1, false)] {
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let mut e = node(data(), false, false);
            let r = f.run(&mut e);
            assert_eq!(r.is_ok(), ok, "limit {limit}");
            if let Err(err) = r {
                assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
            }
        }
    }

    #[test]
    fn interrupts_are_checked_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(100_000);
        let mut e: BoxedExecutor = Box::new(HashAggregateExec::new(
            Box::new(input),
            vec![col(0, SqlType::INT4)],
            AggSet::new(&[]),
            true,
            false,
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(reads.get() <= 1);
    }
}
