//! Executor node: sequential scan. Emits the user columns followed by the
//! requested system columns (`m2.md` §4.7).

use crate::catalog::SystemColumn;
use crate::error::Result;
use crate::executor::{ExecCtx, Executor};
use crate::storage::{HeapScan, HeapTuple, RelHandle};
use crate::types::{Datum, Row};

#[derive(Debug)]
pub struct SeqScanExec {
    rel: RelHandle,
    system_columns: Vec<SystemColumn>,
    /// Started on the first `next` call, with the statement's snapshot.
    scan: Option<HeapScan>,
}

impl SeqScanExec {
    pub fn new(rel: RelHandle, system_columns: Vec<SystemColumn>) -> Self {
        SeqScanExec {
            rel,
            system_columns,
            scan: None,
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
        ctx.check_interrupts()?;
        if self.scan.is_none() {
            self.scan = Some(ctx.storage.begin_scan(&self.rel, ctx.snapshot)?);
        }
        let Some(scan) = self.scan.as_mut() else {
            return Ok(None);
        };
        Ok(ctx.storage.scan_next(scan)?.map(|t| self.row_of(t)))
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
    fn stops_on_shutdown_request() {
        let mut f = Fixture::new();
        f.interrupts.request_terminate();
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(rel(), vec![]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::ADMIN_SHUTDOWN);
    }
}
