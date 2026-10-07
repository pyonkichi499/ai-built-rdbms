//! Executor node: distinct (keeps the first occurrence of each row, so the
//! input order — e.g. from a Sort below — is preserved).
//!
//! Rows are equal when every column is equal by `cmp_datum` semantics
//! (`HashKey`, `types/hash.rs`); NULLs are equal to each other
//! (`IS NOT DISTINCT FROM`). 新しいキーごとに `estimate_row_bytes + HASH_ENTRY_OVERHEAD` を課金し、
//! `rewind` で返す（`m4/05` §5.10・§8.3）。

use std::collections::HashSet;

use crate::error::Result;
use crate::executor::mem::{HASH_ENTRY_OVERHEAD, estimate_row_bytes};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::types::Row;
use crate::types::hash::HashKey;

pub struct DistinctExec {
    input: BoxedExecutor,
    seen: HashSet<HashKey>,
    /// `ctx.mem` に課金した合計。
    charged: usize,
}

impl std::fmt::Debug for DistinctExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistinctExec")
            .field("seen", &self.seen.len())
            .finish_non_exhaustive()
    }
}

impl DistinctExec {
    pub fn new(input: BoxedExecutor) -> Self {
        DistinctExec {
            input,
            seen: HashSet::new(),
            charged: 0,
        }
    }
}

impl Executor for DistinctExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            let key = HashKey(row.clone());
            if self.seen.contains(&key) {
                continue;
            }
            // 失敗したらキーは表に入れない。
            let bytes = estimate_row_bytes(&row) + HASH_ENTRY_OVERHEAD;
            ctx.mem.charge(bytes)?;
            self.charged += bytes;
            self.seen.insert(key);
            return Ok(Some(row));
        }
        Ok(None)
    }

    /// 見た行の集合を空にして課金を返し、子を `rewind` する（Distinct は結果ではなく集合を持つので再利用しない）。
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        ctx.mem.release(std::mem::take(&mut self.charged));
        self.seen.clear();
        self.input.rewind(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, lit, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::Datum;
    use crate::types::SqlType;

    #[test]
    fn removes_duplicates_keeping_order() {
        let mut f = Fixture::new();
        let fl = |v: f64| lit(Datum::Float8(v), SqlType::FLOAT8);
        let input = Box::new(ValuesExec::new(vec![
            vec![int(2), text("a")],
            vec![int(1), null(SqlType::TEXT)],
            vec![int(2), text("a")],
            vec![int(1), null(SqlType::TEXT)],
            vec![int(2), text("b")],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Int4(2), Datum::Text("a".into())],
                vec![Datum::Int4(1), Datum::Null],
                vec![Datum::Int4(2), Datum::Text("b".into())],
            ]
        );
        let input = Box::new(ValuesExec::new(vec![
            vec![fl(0.0)],
            vec![fl(-0.0)],
            vec![fl(f64::NAN)],
            vec![fl(-f64::NAN)],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        assert_eq!(f.run(&mut e).unwrap().len(), 2);
    }

    #[test]
    fn rewind_replays_same_rows() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let input = Box::new(ValuesExec::new(vec![
            vec![int(1)],
            vec![int(1)],
            vec![int(2)],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        e.rewind(&mut ctx).unwrap();
        assert_eq!(e.next(&mut ctx).unwrap(), Some(vec![Datum::Int4(1)]));
        e.rewind(&mut ctx).unwrap();
        let all = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(all, vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]]);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), all);
    }

    #[test]
    fn loop_checks_interrupts_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::ints(1_000);
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(Box::new(input)));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert_eq!(reads.get(), 1);
    }

    fn dedup(rows: Vec<Row>) -> Vec<Row> {
        let exprs = rows
            .into_iter()
            .map(|r| r.into_iter().map(|d| lit(d.clone(), ty_of(&d))).collect())
            .collect();
        let mut f = Fixture::new();
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(Box::new(ValuesExec::new(exprs))));
        f.run(&mut e).unwrap()
    }

    fn ty_of(d: &Datum) -> SqlType {
        match d {
            Datum::Numeric(_) => SqlType::NUMERIC,
            Datum::BpChar(_) => SqlType::TEXT,
            Datum::Date(_) => SqlType::DATE,
            _ => SqlType::INT4,
        }
    }

    #[test]
    fn numeric_bpchar_and_dates_follow_cmp_datum() {
        use yuzhu_datetime::Date;
        use yuzhu_numeric::Numeric;
        let n = |s: &str| Datum::Numeric(Numeric::parse(s).unwrap());
        assert_eq!(
            dedup(vec![vec![n("1.10")], vec![n("1.1")], vec![n("2")]]).len(),
            2
        );
        let b = |s: &str| Datum::BpChar(s.into());
        assert_eq!(
            dedup(vec![vec![b("a  ")], vec![b("a")], vec![b("b")]]).len(),
            2
        );
        assert_eq!(
            dedup(vec![vec![Datum::Date(Date(1))], vec![Datum::Date(Date(1))]]).len(),
            1
        );
    }

    #[test]
    fn charges_new_keys_only_and_releases_on_rewind() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let input = Box::new(ValuesExec::new(vec![
            vec![int(1)],
            vec![int(1)],
            vec![int(2)],
        ]));
        let mut e: BoxedExecutor = Box::new(DistinctExec::new(input));
        let per_row = estimate_row_bytes(&vec![Datum::Int4(1)]) + HASH_ENTRY_OVERHEAD;
        e.next(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), per_row);
        drain(&mut e, &mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 2 * per_row);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn exceeding_the_budget_is_53200_and_exact_limit_passes() {
        let per_row = estimate_row_bytes(&vec![Datum::Int4(0)]) + HASH_ENTRY_OVERHEAD;
        for (limit, ok) in [(3 * per_row, true), (3 * per_row - 1, false)] {
            let mut f = Fixture::new();
            f.mem_limit = limit;
            let (input, _, _) = CountingExec::ints(3);
            let mut e: BoxedExecutor = Box::new(DistinctExec::new(Box::new(input)));
            match f.run(&mut e) {
                Ok(rows) => assert!(ok && rows.len() == 3),
                Err(err) => {
                    assert!(!ok);
                    assert_eq!(err.sqlstate, crate::error::sqlstate::OUT_OF_MEMORY);
                }
            }
        }
    }
}
