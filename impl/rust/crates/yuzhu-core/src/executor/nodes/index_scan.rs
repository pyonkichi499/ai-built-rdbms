//! Index Scan ノード（`m4/05` §5.4）。
//!
//! 初回の `next` で `keys` を空の行に対して評価し（`Param` や `InitPlan` の値を読む）、`IndexStore::begin_scan` で
//! 走査を始める。TID ごとにヒープを `fetch` して可視性を判定し（索引は MVCC を知らない）、`filter` を再評価する。
//! `filter` は planner が元の述語すべてを入れるので、区別せずそのまま評価する。

use std::rc::Rc;

use crate::catalog::SystemColumn;
use crate::error::Result;
use crate::executor::eval::{eval, eval_pred};
use crate::executor::instrument::Instrumentation;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{
    FilterCounter, IndexScanKey, IndexScanKeys, PhysExpr, PhysicalPlan,
};
use crate::storage::{
    HeapTuple, IndexHandle, IndexScan, RelHandle, ResolvedScanKeys, ScanDirection,
};
use crate::types::{Datum, Row};

/// `Index Scan` を実行する Executor を作る。`plan` が `IndexScan` でなければ `Error::internal` を返す Executor。
pub(in crate::executor) fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    match plan {
        PhysicalPlan::IndexScan {
            rel,
            index,
            keys,
            direction,
            system_columns,
            filter,
            ..
        } => Box::new(IndexScanExec::new(
            rel.clone(),
            index.clone(),
            keys.clone(),
            *direction,
            system_columns.clone(),
            filter.clone(),
        )),
        _ => Box::new(super::UnsupportedExec::new("Index Scan")),
    }
}

#[derive(Debug)]
pub struct IndexScanExec {
    rel: RelHandle,
    index: IndexHandle,
    keys: IndexScanKeys,
    direction: ScanDirection,
    system_columns: Vec<SystemColumn>,
    filter: Option<PhysExpr>,
    /// 初回の `next` で `begin_scan` する。`rewind` で `None` に戻す。
    scan: Option<IndexScan>,
    /// キーに NULL があり 0 行と決まった。
    empty: bool,
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl IndexScanExec {
    pub fn new(
        rel: RelHandle,
        index: IndexHandle,
        keys: IndexScanKeys,
        direction: ScanDirection,
        system_columns: Vec<SystemColumn>,
        filter: Option<PhysExpr>,
    ) -> Self {
        IndexScanExec {
            rel,
            index,
            keys,
            direction,
            system_columns,
            filter,
            scan: None,
            empty: false,
            counters: None,
        }
    }

    /// キー式を評価する。NULL に当たったら `None`（`col = NULL`・`col > NULL` は決して真にならない）。
    fn resolve_keys(&self, ctx: &mut ExecCtx<'_>) -> Result<Option<ResolvedScanKeys>> {
        let empty = Row::new();
        let mut eq = Vec::with_capacity(self.keys.eq.len());
        for k in &self.keys.eq {
            match k {
                IndexScanKey::IsNull => eq.push(None),
                IndexScanKey::Eq(e) => {
                    let v = eval(e, &empty, ctx)?;
                    if v.is_null() {
                        return Ok(None);
                    }
                    eq.push(Some(v));
                }
            }
        }
        let mut bound = |b: &Option<crate::planner::physical::RangeBound>| -> Result<
            Option<Option<(Datum, bool)>>,
        > {
            let Some(b) = b else {
                return Ok(Some(None));
            };
            let v = eval(&b.expr, &empty, ctx)?;
            Ok(if v.is_null() {
                None
            } else {
                Some(Some((v, b.inclusive)))
            })
        };
        let Some(lower) = bound(&self.keys.lower)? else {
            return Ok(None);
        };
        let Some(upper) = bound(&self.keys.upper)? else {
            return Ok(None);
        };
        Ok(Some(ResolvedScanKeys { eq, lower, upper }))
    }

    /// ユーザー列 ++ `system_columns`（`SeqScanExec` と同じ形）。
    fn row_of(&self, t: HeapTuple) -> Row {
        let mut row = t.row;
        for c in &self.system_columns {
            row.push(match c {
                SystemColumn::Ctid => Datum::Tid(t.tid),
                SystemColumn::Xmin => Datum::Xid(t.xmin.to_external()),
                SystemColumn::Cmin => Datum::Cid(t.cmin),
                SystemColumn::Xmax => Datum::Xid(t.xmax.to_external()),
                SystemColumn::Cmax => Datum::Cid(t.cmax),
                SystemColumn::TableOid => Datum::Oid(self.rel.oid),
            });
        }
        row
    }
}

impl Executor for IndexScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        loop {
            ctx.check_interrupts()?;
            if self.scan.is_none() {
                if self.empty {
                    return Ok(None);
                }
                let Some(keys) = self.resolve_keys(ctx)? else {
                    self.empty = true;
                    return Ok(None);
                };
                self.scan = Some(ctx.indexes.begin_scan(&self.index, &keys, self.direction)?);
            }
            let Some(scan) = self.scan.as_mut() else {
                return Ok(None);
            };
            let Some(tid) = ctx.indexes.scan_next(scan)? else {
                return Ok(None);
            };
            // 不可視・死んだ版（旧版の項目は残っている）は捨てる。
            let Some(tuple) = ctx.storage.fetch(&self.rel, ctx.snapshot, tid)? else {
                continue;
            };
            let row = self.row_of(tuple);
            let Some(filter) = &self.filter else {
                return Ok(Some(row));
            };
            if eval_pred(filter, &row, ctx)? == Some(true) {
                return Ok(Some(row));
            }
            if let Some((id, instr)) = &self.counters {
                instr.add_removed(*id, FilterCounter::Filter, 1);
            }
        }
    }

    /// 次の `next` でキーを評価し直して `begin_scan` する（NestedLoopParam の inner では `Param` が変わる）。
    /// 落とした行の数は 0 に戻さない。
    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.scan = None;
        self.empty = false;
        Ok(())
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.counters = Some((id, Rc::clone(instr)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::catalog::fake::table_def;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{GT, col, int, null, op, param};
    use crate::executor::nodes::test_util::{Fixture, drain, index_on};
    use crate::planner::physical::RangeBound;
    use crate::storage::TableStore;
    use crate::types::{SqlType, Tid};
    use std::sync::Arc;

    const T: u32 = 7;

    fn rel() -> RelHandle {
        let c = |name: &str, attnum, ty| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null: false,
            default: None,
            identity: None,
        };
        let mut rel = RelHandle::from_table(&table_def(
            T,
            "t",
            vec![c("a", 1, SqlType::INT4), c("b", 2, SqlType::TEXT)],
            vec![],
        ));
        rel.indexes = Arc::from(vec![index_on(
            100,
            "t_a_idx",
            "t",
            false,
            &[(1, "a", SqlType::INT4)],
        )]);
        rel
    }

    fn row(a: Option<i32>, b: &str) -> Row {
        vec![a.map_or(Datum::Null, Datum::Int4), Datum::Text(b.into())]
    }

    /// a = 3, 1, 2, 2, NULL, 5（b は "r0".."r5"。TID の offset は 1..6）。
    fn fixture(rel: &RelHandle) -> Fixture {
        let f = Fixture::new();
        for (i, a) in [Some(3), Some(1), Some(2), Some(2), None, Some(5)]
            .into_iter()
            .enumerate()
        {
            f.insert_indexed(rel, &row(a, &format!("r{i}")));
        }
        f
    }

    fn scan(
        rel: &RelHandle,
        keys: IndexScanKeys,
        direction: ScanDirection,
        filter: Option<PhysExpr>,
    ) -> IndexScanExec {
        IndexScanExec::new(
            rel.clone(),
            rel.indexes[0].clone(),
            keys,
            direction,
            vec![SystemColumn::Ctid],
            filter,
        )
    }

    fn bs(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r[1] {
                Datum::Text(s) => s.clone(),
                d => panic!("{d:?}"),
            })
            .collect()
    }

    fn eq(e: PhysExpr) -> IndexScanKeys {
        IndexScanKeys {
            eq: vec![IndexScanKey::Eq(e)],
            ..IndexScanKeys::default()
        }
    }

    fn range(lower: Option<(i32, bool)>, upper: Option<(i32, bool)>) -> IndexScanKeys {
        let b = |(v, inclusive)| RangeBound {
            expr: int(v),
            inclusive,
        };
        IndexScanKeys {
            eq: vec![],
            lower: lower.map(b),
            upper: upper.map(b),
        }
    }

    fn run(f: &mut Fixture, e: IndexScanExec) -> Vec<Row> {
        f.run(&mut (Box::new(e) as BoxedExecutor)).unwrap()
    }

    #[test]
    fn equality_returns_matching_rows_in_tid_order_with_system_columns() {
        let rel = rel();
        let mut f = fixture(&rel);
        let rows = run(&mut f, scan(&rel, eq(int(2)), ScanDirection::Forward, None));
        assert_eq!(bs(&rows), ["r2", "r3"]);
        assert_eq!(
            rows[0][2],
            Datum::Tid(Tid {
                block: 0,
                offset: 3
            })
        );
        assert_eq!(
            rows[1][2],
            Datum::Tid(Tid {
                block: 0,
                offset: 4
            })
        );
        // Backward は逆順。
        let rows = run(
            &mut f,
            scan(&rel, eq(int(2)), ScanDirection::Backward, None),
        );
        assert_eq!(bs(&rows), ["r3", "r2"]);
    }

    #[test]
    fn ranges_respect_inclusive_flags_and_exclude_nulls() {
        let rel = rel();
        let mut f = fixture(&rel);
        let go = |f: &mut Fixture, k, d| bs(&run(f, scan(&rel, k, d, None)));
        // 全件（NULL は範囲に入らない）。索引順 = a の昇順、同値は TID 順。
        assert_eq!(
            go(
                &mut f,
                range(Some((i32::MIN, true)), None),
                ScanDirection::Forward
            ),
            ["r1", "r2", "r3", "r0", "r5"]
        );
        assert_eq!(
            go(
                &mut f,
                range(Some((2, false)), Some((5, false))),
                ScanDirection::Forward
            ),
            ["r0"]
        );
        assert_eq!(
            go(
                &mut f,
                range(Some((2, true)), Some((5, true))),
                ScanDirection::Forward
            ),
            ["r2", "r3", "r0", "r5"]
        );
        assert_eq!(
            go(
                &mut f,
                range(None, Some((2, true))),
                ScanDirection::Backward
            ),
            ["r3", "r2", "r1"]
        );
    }

    #[test]
    fn is_null_key_finds_null_entries() {
        let rel = rel();
        let mut f = fixture(&rel);
        let keys = IndexScanKeys {
            eq: vec![IndexScanKey::IsNull],
            ..IndexScanKeys::default()
        };
        assert_eq!(
            bs(&run(&mut f, scan(&rel, keys, ScanDirection::Forward, None))),
            ["r4"]
        );
    }

    #[test]
    fn null_keys_return_no_rows() {
        let rel = rel();
        let mut f = fixture(&rel);
        for keys in [
            eq(null(SqlType::INT4)),
            IndexScanKeys {
                lower: Some(RangeBound {
                    expr: null(SqlType::INT4),
                    inclusive: true,
                }),
                ..IndexScanKeys::default()
            },
            IndexScanKeys {
                upper: Some(RangeBound {
                    expr: null(SqlType::INT4),
                    inclusive: false,
                }),
                ..IndexScanKeys::default()
            },
        ] {
            assert!(run(&mut f, scan(&rel, keys, ScanDirection::Forward, None)).is_empty());
        }
    }

    #[test]
    fn rewind_reevaluates_the_keys() {
        let rel = rel();
        let mut f = fixture(&rel);
        f.query.n_params = 1;
        let mut e: BoxedExecutor = Box::new(scan(
            &rel,
            eq(param(0, SqlType::INT4)),
            ScanDirection::Forward,
            None,
        ));
        let mut ctx = f.ctx();
        // NULL のパラメータ: 0 行で、以後も 0 行のまま。
        assert!(drain(&mut e, &mut ctx).unwrap().is_empty());
        assert!(e.next(&mut ctx).unwrap().is_none());
        // 値を変えて rewind: 空の印も消えて、新しい値で探し直す。
        ctx.set_param(crate::expr::ParamId(0), Datum::Int4(2))
            .unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!(bs(&drain(&mut e, &mut ctx).unwrap()), ["r2", "r3"]);
        ctx.set_param(crate::expr::ParamId(0), Datum::Int4(5))
            .unwrap();
        e.rewind(&mut ctx).unwrap();
        assert_eq!(bs(&drain(&mut e, &mut ctx).unwrap()), ["r5"]);
        // 読み切った後の rewind でも同じ値で先頭から。
        e.rewind(&mut ctx).unwrap();
        assert_eq!(bs(&drain(&mut e, &mut ctx).unwrap()), ["r5"]);
    }

    #[test]
    fn invisible_tids_are_dropped() {
        let rel = rel();
        let mut f = fixture(&rel);
        // r2（TID 3）を削除する。索引の項目は残る。
        let w = crate::storage::WriteCtx {
            xid: crate::txn::Xid(3),
            cid: 0,
        };
        let snap = f.snapshot.clone();
        f.storage
            .delete(
                &rel,
                &w,
                &snap,
                Tid {
                    block: 0,
                    offset: 3,
                },
            )
            .unwrap();
        assert_eq!(f.indexes.len(100), 6);
        let rows = run(&mut f, scan(&rel, eq(int(2)), ScanDirection::Forward, None));
        assert_eq!(bs(&rows), ["r3"]);
    }

    #[test]
    fn filter_is_reevaluated_and_removed_rows_are_counted() {
        let rel = rel();
        let mut f = fixture(&rel);
        // 索引のキー（a >= 2）とは別に、元の述語 a > 2 をそのまま再評価する。
        let filter = op(&GT, col(0, SqlType::INT4), int(2));
        let mut e = scan(
            &rel,
            range(Some((2, true)), None),
            ScanDirection::Forward,
            Some(filter),
        );
        let instr = Rc::new(Instrumentation::new(1));
        e.set_counters(0, &instr);
        let rows = run(&mut f, e);
        assert_eq!(bs(&rows), ["r0", "r5"]);
        assert_eq!(instr.node(0).removed_filter.get(), 2);
    }

    #[test]
    fn stops_on_interrupt_and_propagates_index_errors() {
        let rel = rel();
        let mut f = fixture(&rel);
        f.interrupts.request_cancel();
        let mut e: BoxedExecutor = Box::new(scan(&rel, eq(int(2)), ScanDirection::Forward, None));
        assert_eq!(
            f.run(&mut e).unwrap_err().sqlstate,
            sqlstate::QUERY_CANCELED
        );
    }

    #[test]
    fn build_creates_an_index_scan_node() {
        let rel = rel();
        let mut f = fixture(&rel);
        let plan = PhysicalPlan::IndexScan {
            rel: rel.clone(),
            index: rel.indexes[0].clone(),
            keys: eq(int(5)),
            direction: ScanDirection::Forward,
            columns: vec![SqlType::INT4, SqlType::TEXT],
            system_columns: vec![],
            filter: None,
        };
        let mut e = build(&plan);
        assert_eq!(bs(&f.run(&mut e).unwrap()), ["r5"]);
    }
}
