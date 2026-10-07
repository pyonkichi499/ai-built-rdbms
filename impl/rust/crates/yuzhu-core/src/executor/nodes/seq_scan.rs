//! Executor node: sequential scan. Emits the user columns followed by the
//! requested system columns (`m2.md` §4.7). 走査中に `filter` を評価し、真の行だけ返す。

use std::rc::Rc;

use crate::catalog::SystemColumn;
use crate::error::Result;
use crate::executor::eval::eval_pred;
use crate::executor::instrument::Instrumentation;
use crate::executor::{ExecCtx, Executor};
use crate::planner::physical::{FilterCounter, PhysExpr};
use crate::storage::{HeapScan, HeapTuple, RelHandle};
use crate::types::{Datum, Row};

#[derive(Debug)]
pub struct SeqScanExec {
    rel: RelHandle,
    system_columns: Vec<SystemColumn>,
    filter: Option<PhysExpr>,
    /// Started on the first `next` call, with the statement's snapshot.
    scan: Option<HeapScan>,
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl SeqScanExec {
    pub fn new(rel: RelHandle, system_columns: Vec<SystemColumn>) -> Self {
        SeqScanExec::with_filter(rel, system_columns, None)
    }

    pub fn with_filter(
        rel: RelHandle,
        system_columns: Vec<SystemColumn>,
        filter: Option<PhysExpr>,
    ) -> Self {
        SeqScanExec {
            rel,
            system_columns,
            filter,
            scan: None,
            counters: None,
        }
    }

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

impl Executor for SeqScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        loop {
            ctx.check_interrupts()?;
            if self.scan.is_none() {
                self.scan = Some(ctx.storage.begin_scan(&self.rel, ctx.snapshot)?);
            }
            let Some(scan) = self.scan.as_mut() else {
                return Ok(None);
            };
            let Some(tuple) = ctx.storage.scan_next(scan)? else {
                return Ok(None);
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

    /// 次の `next` で同じスナップショットのまま先頭から読み直す。
    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.scan = None;
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
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::{SqlType, Tid};

    fn rel() -> RelHandle {
        let col = ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
            identity: None,
        };
        RelHandle::from_table(&table_def(7, "t", vec![col], vec![]))
    }

    #[test]
    fn scans_rows_in_storage_order_with_system_columns() {
        let mut f = Fixture::new();
        let rel = rel();
        f.storage.add_row(rel.oid, vec![Datum::Int4(2)]);
        f.storage.add_row(rel.oid, vec![Datum::Int4(1)]);
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(
            rel.clone(),
            vec![SystemColumn::Ctid, SystemColumn::TableOid],
        ));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![
                    Datum::Int4(2),
                    Datum::Tid(Tid {
                        block: 0,
                        offset: 1
                    }),
                    Datum::Oid(7)
                ],
                vec![
                    Datum::Int4(1),
                    Datum::Tid(Tid {
                        block: 0,
                        offset: 2
                    }),
                    Datum::Oid(7)
                ],
            ]
        );
    }

    #[test]
    fn all_system_columns_have_their_types_of_values() {
        let mut f = Fixture::new();
        let rel = rel();
        f.storage.add_row(rel.oid, vec![Datum::Int4(1)]);
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(
            rel,
            vec![
                SystemColumn::Xmax,
                SystemColumn::Xmin,
                SystemColumn::Cmin,
                SystemColumn::Cmax,
            ],
        ));
        // FakeStore rows: xmin = BOOTSTRAP (1), xmax = INVALID (0), cids 0.
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![
                Datum::Int4(1),
                Datum::Xid(0),
                Datum::Xid(1),
                Datum::Cid(0),
                Datum::Cid(0)
            ]]
        );
    }

    #[test]
    fn filter_removes_rows_and_rewind_replays() {
        use crate::executor::eval::tests::{GT, col, int, op};
        use crate::executor::nodes::test_util::drain;
        let mut f = Fixture::new();
        let rel = rel();
        for v in [1, 5, 3] {
            f.storage.add_row(rel.oid, vec![Datum::Int4(v)]);
        }
        let instr = Rc::new(Instrumentation::new(1));
        let mut s =
            SeqScanExec::with_filter(rel, vec![], Some(op(&GT, col(0, SqlType::INT4), int(2))));
        s.set_counters(0, &instr);
        let mut e: crate::executor::BoxedExecutor = Box::new(s);
        let mut ctx = f.ctx();
        e.rewind(&mut ctx).unwrap();
        let want = vec![vec![Datum::Int4(5)], vec![Datum::Int4(3)]];
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), want);
        e.rewind(&mut ctx).unwrap();
        assert_eq!(e.next(&mut ctx).unwrap(), Some(vec![Datum::Int4(5)]));
        e.rewind(&mut ctx).unwrap();
        assert_eq!(drain(&mut e, &mut ctx).unwrap(), want);
        // 3 回走査して、そのたびに値 1 の行が落ちる。
        assert_eq!(instr.node(0).removed_filter.get(), 3);
    }

    #[test]
    fn stops_on_shutdown_request() {
        let mut f = Fixture::new();
        f.interrupts.request_terminate();
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(rel(), vec![]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::ADMIN_SHUTDOWN);
    }
}
