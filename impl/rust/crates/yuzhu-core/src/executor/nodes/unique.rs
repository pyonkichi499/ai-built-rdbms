//! Unique ノード（DISTINCT ON の実装）。`m4/05` §5.9。
//!
//! 入力は `key_cols`（入力の列位置）で整列済み。`key_cols` の値が直前に返した行と**異なる**最初の行だけ返す
//! （等しさは `cmp_datum == Equal`。NULL どうしは等しい）。整列の仮定は検査しない（降順の整列もありうるので、
//! 順序の `debug_assert` は置かない）。課金なし。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::executor::build::{BuildEnv, build_scoped};
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::PhysicalPlan;
use crate::types::{Datum, Row, cmp_datum};

pub struct UniqueExec {
    input: BoxedExecutor,
    key_cols: Vec<usize>,
    prev_key: Option<Vec<Datum>>,
}

impl std::fmt::Debug for UniqueExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniqueExec")
            .field("key_cols", &self.key_cols)
            .finish_non_exhaustive()
    }
}

impl UniqueExec {
    pub fn new(input: BoxedExecutor, key_cols: Vec<usize>) -> Self {
        UniqueExec {
            input,
            key_cols,
            prev_key: None,
        }
    }

    fn key_of(&self, row: &Row) -> Result<Vec<Datum>> {
        self.key_cols
            .iter()
            .map(|&i| {
                row.get(i)
                    .cloned()
                    .ok_or_else(|| Error::internal(format!("Unique key column {i} out of range")))
            })
            .collect()
    }
}

impl Executor for UniqueExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            let key = self.key_of(&row)?;
            let same = self.prev_key.as_ref().is_some_and(|p| {
                p.iter()
                    .zip(&key)
                    .all(|(a, b)| cmp_datum(a, b) == Ordering::Equal)
            });
            if !same {
                self.prev_key = Some(key);
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.prev_key = None;
        self.input.rewind(ctx)
    }
}

pub(crate) fn build_in(plan: &PhysicalPlan, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor {
    let PhysicalPlan::Unique { input, key_cols } = plan else {
        return Box::new(super::UnsupportedExec::new("Unique"));
    };
    Box::new(UniqueExec::new(
        build_scoped(input, env, rewindable),
        key_cols.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{int, lit, null, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::{CountingExec, Fixture, drain};
    use crate::types::SqlType;

    fn node(rows: Vec<Vec<crate::planner::physical::PhysExpr>>, cols: Vec<usize>) -> BoxedExecutor {
        Box::new(UniqueExec::new(Box::new(ValuesExec::new(rows)), cols))
    }

    #[test]
    fn keeps_the_first_row_of_each_key_run() {
        let mut f = Fixture::new();
        let mut e = node(
            vec![
                vec![int(1), text("first")],
                vec![int(1), text("second")],
                vec![int(2), text("third")],
                vec![int(2), text("fourth")],
                vec![int(1), text("fifth")],
            ],
            vec![0],
        );
        let rows = f.run(&mut e).unwrap();
        let names: Vec<_> = rows
            .iter()
            .map(|r| r[1].as_str().unwrap().to_owned())
            .collect();
        // 整列済みの仮定: 連続した同じキーだけをまとめる（最後の 1 は新しい run）。
        assert_eq!(names, ["first", "third", "fifth"]);
    }

    #[test]
    fn nulls_are_equal_and_multi_column_keys() {
        let mut f = Fixture::new();
        let n = || null(SqlType::INT4);
        let mut e = node(
            vec![
                vec![n(), int(1), int(0)],
                vec![n(), int(1), int(1)],
                vec![n(), int(2), int(2)],
                vec![int(1), int(2), int(3)],
            ],
            vec![0, 1],
        );
        let rows = f.run(&mut e).unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r[2].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [0, 2, 3]
        );
    }

    #[test]
    fn numeric_and_bpchar_equality() {
        use yuzhu_numeric::Numeric;
        let mut f = Fixture::new();
        let n = |s: &str| lit(Datum::Numeric(Numeric::parse(s).unwrap()), SqlType::NUMERIC);
        let mut e = node(vec![vec![n("1.10")], vec![n("1.1")], vec![n("2")]], vec![0]);
        assert_eq!(f.run(&mut e).unwrap().len(), 2);
        let b = |s: &str| lit(Datum::BpChar(s.into()), SqlType::TEXT);
        let mut e = node(vec![vec![b("a  ")], vec![b("a")], vec![b("b")]], vec![0]);
        assert_eq!(f.run(&mut e).unwrap().len(), 2);
    }

    #[test]
    fn empty_key_cols_keeps_only_the_first_row() {
        let mut f = Fixture::new();
        let mut e = node(vec![vec![int(1)], vec![int(2)]], vec![]);
        assert_eq!(f.run(&mut e).unwrap().len(), 1);
    }

    #[test]
    fn out_of_range_key_column_is_internal_error() {
        let mut f = Fixture::new();
        let mut e = node(vec![vec![int(1)]], vec![3]);
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            crate::error::sqlstate::INTERNAL_ERROR
        );
    }

    #[test]
    fn rewind_replays_and_no_memory_is_charged() {
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut e = node(vec![vec![int(1)], vec![int(1)], vec![int(2)]], vec![0]);
        let all = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(all.len(), 2);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), all);
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn loop_checks_interrupts_per_input_row() {
        let mut f = Fixture::new();
        f.interrupts.request_cancel();
        let (input, reads, _) = CountingExec::new(vec![vec![Datum::Int4(1)]; 1000]);
        let mut e: BoxedExecutor = Box::new(UniqueExec::new(Box::new(input), vec![0]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert_eq!(reads.get(), 1);
    }
}
