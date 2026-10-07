//! Executor node: INSERT.
//!
//! Per input row (already coerced to the column types): build the table
//! row via `column_map` / `defaults` ([`RowBuilder`]) → NOT NULL checks in column order
//! (23502) → CHECK constraints (23514; NULL passes) ([`RowChecker`]) → `insert_with_indexes`
//! (with the transaction's `WriteCtx`). Without `returning` the node emits no rows and the
//! count is `rows_affected`; with it, one row per inserted row.

use crate::error::{Error, Result};
use crate::executor::dml::{RowBuilder, RowChecker, insert_with_indexes};
use crate::executor::eval::eval;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{PhysCheck, PhysExpr};
use crate::storage::RelHandle;
use crate::types::Row;

pub struct InsertExec {
    rel: RelHandle,
    table_name: String,
    input: BoxedExecutor,
    builder: RowBuilder,
    checker: RowChecker,
    returning: Option<Vec<PhysExpr>>,
    count: u64,
    done: bool,
}

impl std::fmt::Debug for InsertExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertExec")
            .field("table", &self.table_name)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl InsertExec {
    pub fn new(
        rel: RelHandle,
        table_name: String,
        input: BoxedExecutor,
        column_map: Vec<Option<usize>>,
        defaults: Vec<Option<PhysExpr>>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
    ) -> Self {
        InsertExec::with_returning(
            rel, table_name, input, column_map, defaults, checks, not_null, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_returning(
        rel: RelHandle,
        table_name: String,
        input: BoxedExecutor,
        column_map: Vec<Option<usize>>,
        defaults: Vec<Option<PhysExpr>>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        returning: Option<Vec<PhysExpr>>,
    ) -> Self {
        let checker = RowChecker::new(rel.oid, table_name.clone(), not_null, checks);
        InsertExec {
            rel,
            table_name,
            input,
            builder: RowBuilder::new(column_map, defaults),
            checker,
            returning,
            count: 0,
            done: false,
        }
    }

    /// 1 行を書く。RETURNING があればその行を返す。
    fn insert_one(&mut self, input: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let row = self.builder.build(ctx, input)?;
        self.checker.check(ctx, &row)?;
        let w = ctx.write_ctx()?;
        insert_with_indexes(ctx, &self.rel, &w, &row)?;
        self.count += 1;
        let Some(returning) = &self.returning else {
            return Ok(None);
        };
        returning
            .iter()
            .map(|e| eval(e, &row, ctx))
            .collect::<Result<Row>>()
            .map(Some)
    }
}

impl Executor for InsertExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        while let Some(input) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            if let Some(out) = self.insert_one(&input, ctx)? {
                return Ok(Some(out));
            }
        }
        self.done = true;
        Ok(None)
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        Err(Error::internal("rewind is not supported for DML nodes"))
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::catalog::fake::table_def;
    use crate::error::sqlstate;
    use crate::executor::eval::tests::{GT, col, int, null, op, text};
    use crate::executor::nodes::ValuesExec;
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::Datum;
    use crate::types::{Oid, SqlType};
    use std::sync::Arc;

    const T: Oid = 16384;

    fn fixture() -> Fixture {
        let mut f = Fixture::new();
        let c = |name: &str, attnum, ty, not_null| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: None,
            identity: None,
        };
        f.catalog.put_table(Arc::new(table_def(
            T,
            "t",
            vec![
                c("a", 1, SqlType::INT4, true),
                c("b", 2, SqlType::TEXT, false),
                c("c", 3, SqlType::INT4, false),
            ],
            vec![],
        )));
        f
    }

    fn check_a_gt_0() -> PhysCheck {
        PhysCheck {
            name: "t_a_check".into(),
            expr: op(&GT, col(0, SqlType::INT4), int(0)),
        }
    }

    fn rel() -> RelHandle {
        RelHandle::from_table(&table_def(T, "t", vec![], vec![]))
    }

    fn insert(rows: Vec<Vec<PhysExpr>>, column_map: Vec<Option<usize>>) -> InsertExec {
        InsertExec::new(
            rel(),
            "t".into(),
            Box::new(ValuesExec::new(rows)),
            column_map,
            vec![None, None, Some(int(42))],
            vec![check_a_gt_0()],
            vec![true, false, false],
        )
    }

    fn scan(f: &Fixture) -> Vec<Row> {
        f.storage.rows(T)
    }

    #[test]
    fn inserts_with_defaults() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(insert(
            vec![vec![text("x"), int(1)], vec![null(SqlType::TEXT), int(2)]],
            vec![Some(1), Some(0), None],
        ));
        assert!(f.run(&mut e).unwrap().is_empty());
        assert_eq!(e.rows_affected(), 2);
        assert_eq!(
            scan(&f),
            vec![
                vec![Datum::Int4(1), Datum::Text("x".into()), Datum::Int4(42)],
                vec![Datum::Int4(2), Datum::Null, Datum::Int4(42)],
            ]
        );
    }

    #[test]
    fn not_null_violation() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(insert(
            vec![
                vec![int(1), text("ok")],
                vec![null(SqlType::INT4), text("x")],
            ],
            vec![Some(0), Some(1), None],
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(
            err.message,
            "null value in column \"a\" of relation \"t\" violates not-null constraint"
        );
        assert_eq!(
            err.detail.as_deref(),
            Some("Failing row contains (null, x, 42).")
        );
        // The first row was inserted before the violation was found.
        assert_eq!(scan(&f).len(), 1);
    }

    #[test]
    fn check_violation_and_null_passes() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(insert(
            vec![vec![int(0), null(SqlType::TEXT)]],
            vec![Some(0), Some(1), None],
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::CHECK_VIOLATION);
        assert_eq!(
            err.message,
            "new row for relation \"t\" violates check constraint \"t_a_check\""
        );
        assert_eq!(
            err.detail.as_deref(),
            Some("Failing row contains (0, null, 42).")
        );
        // CHECK on a nullable column: NULL result passes.
        let mut e: BoxedExecutor = Box::new(InsertExec::new(
            rel(),
            "t".into(),
            Box::new(ValuesExec::new(vec![vec![int(5)]])),
            vec![Some(0), None, None],
            vec![None, None, None],
            vec![PhysCheck {
                name: "t_c_check".into(),
                expr: op(&GT, col(2, SqlType::INT4), int(0)),
            }],
            vec![true, false, false],
        ));
        f.run(&mut e).unwrap();
        assert_eq!(e.rows_affected(), 1);
    }

    #[test]
    fn failing_row_clips_long_values() {
        let f = fixture();
        let table = f.catalog.table_by_oid(T).unwrap().unwrap();
        let long = "é".repeat(40);
        let d = crate::executor::dml::failing_row_detail(
            &table,
            &vec![Datum::Int4(1), Datum::Text(long), Datum::Null],
            &crate::types::TypeEnv::default(),
        );
        assert_eq!(
            d,
            format!("Failing row contains (1, {}..., null).", "é".repeat(32))
        );
    }
    use crate::catalog::CatalogReader;

    #[test]
    fn returning_emits_a_row_per_inserted_row() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(InsertExec::with_returning(
            rel(),
            "t".into(),
            Box::new(ValuesExec::new(vec![vec![int(1)], vec![int(2)]])),
            vec![Some(0), None, None],
            vec![None, None, Some(int(42))],
            vec![],
            vec![true, false, false],
            Some(vec![col(2, SqlType::INT4), col(0, SqlType::INT4)]),
        ));
        assert_eq!(
            f.run(&mut e).unwrap(),
            vec![
                vec![Datum::Int4(42), Datum::Int4(1)],
                vec![Datum::Int4(42), Datum::Int4(2)]
            ]
        );
        assert_eq!(e.rows_affected(), 2);
    }

    #[test]
    fn dml_rewind_is_internal_error() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(insert(vec![], vec![Some(0), Some(1), None]));
        let mut ctx = f.ctx();
        let err = e.rewind(&mut ctx).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn stops_on_cancel_per_input_row() {
        let mut f = fixture();
        f.interrupts.request_cancel();
        let mut e: BoxedExecutor = Box::new(insert(
            vec![vec![int(1), text("x")]],
            vec![Some(0), Some(1), None],
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(scan(&f).is_empty());
    }
}
