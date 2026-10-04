//! Executor node: sequential scan (all live rows in insertion order).

use crate::error::Result;
use crate::executor::{ExecCtx, Executor};
use crate::storage::RowId;
use crate::types::{Oid, Row};

#[derive(Debug)]
pub struct SeqScanExec {
    table_oid: Oid,
    /// Snapshot taken on the first `next` call (M1: a full copy, so an
    /// `INSERT INTO t SELECT * FROM t` reads the statement-start contents).
    rows: Option<std::vec::IntoIter<(RowId, Row)>>,
}

impl SeqScanExec {
    pub fn new(table_oid: Oid) -> Self {
        SeqScanExec {
            table_oid,
            rows: None,
        }
    }
}

impl Executor for SeqScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.rows.is_none() {
            self.rows = Some(ctx.storage.scan(self.table_oid)?.into_iter());
        }
        Ok(self
            .rows
            .as_mut()
            .and_then(Iterator::next)
            .map(|(_, row)| row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::nodes::test_util::Fixture;
    use crate::storage::TableStore;
    use crate::types::Datum;

    #[test]
    fn scans_in_insertion_order() {
        let mut f = Fixture::new();
        f.storage.create_table(7).unwrap();
        f.storage.insert(7, vec![Datum::Int4(2)]).unwrap();
        let id = f.storage.insert(7, vec![Datum::Int4(9)]).unwrap();
        f.storage.insert(7, vec![Datum::Int4(1)]).unwrap();
        f.storage.delete_row(7, id).unwrap();
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(7));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![vec![Datum::Int4(2)], vec![Datum::Int4(1)]]
        );
        let mut e: crate::executor::BoxedExecutor = Box::new(SeqScanExec::new(8));
        assert!(f.run(&mut e).is_err());
    }
}
