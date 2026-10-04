//! Executor node: INSERT.
//!
//! Per input row (already coerced to the column types): build the table
//! row via `column_map` / `defaults` → NOT NULL checks in column order
//! (23502) → CHECK constraints (23514; NULL passes) → `storage.insert` →
//! undo log entry. Emits no rows; the count is `rows_affected`.

use crate::analyzer::{BoundCheck, BoundExpr};
use crate::catalog::TableDef;
use crate::error::{Error, Result, sqlstate};
use crate::executor::eval::eval_bool;
use crate::executor::{BoxedExecutor, ExecCtx, Executor, eval};
use crate::txn::UndoEntry;
use crate::types::{Datum, Oid, Row, io};

pub struct InsertExec {
    table_oid: Oid,
    input: BoxedExecutor,
    column_map: Vec<Option<usize>>,
    defaults: Vec<Option<BoundExpr>>,
    checks: Vec<BoundCheck>,
    not_null: Vec<bool>,
    count: u64,
    done: bool,
}

impl std::fmt::Debug for InsertExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertExec")
            .field("table_oid", &self.table_oid)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl InsertExec {
    pub fn new(
        table_oid: Oid,
        input: BoxedExecutor,
        column_map: Vec<Option<usize>>,
        defaults: Vec<Option<BoundExpr>>,
        checks: Vec<BoundCheck>,
        not_null: Vec<bool>,
    ) -> Self {
        InsertExec {
            table_oid,
            input,
            column_map,
            defaults,
            checks,
            not_null,
            count: 0,
            done: false,
        }
    }

    fn build_row(&self, input: &Row, ctx: &ExecCtx<'_>) -> Result<Row> {
        let empty = Row::new();
        self.column_map
            .iter()
            .enumerate()
            .map(|(i, src)| match src {
                Some(j) => input
                    .get(*j)
                    .cloned()
                    .ok_or_else(|| Error::internal(format!("input column {j} out of range"))),
                None => match self.defaults.get(i).and_then(Option::as_ref) {
                    Some(e) => eval(e, &empty, ctx),
                    None => Ok(Datum::Null),
                },
            })
            .collect()
    }

    fn insert_one(&mut self, input: &Row, ctx: &mut ExecCtx<'_>) -> Result<()> {
        let row = self.build_row(input, ctx)?;
        let needs_table = self
            .not_null
            .iter()
            .zip(&row)
            .any(|(nn, d)| *nn && d.is_null())
            || !self.checks.is_empty();
        if needs_table {
            let table = ctx.catalog.table_by_oid(self.table_oid).ok_or_else(|| {
                Error::internal(format!(
                    "relation with OID {} does not exist",
                    self.table_oid
                ))
            })?;
            check_constraints(&table, &row, &self.not_null, &self.checks, ctx)?;
        }
        let row_id = ctx.storage.insert(self.table_oid, row)?;
        ctx.txn.record(UndoEntry::Inserted {
            table_oid: self.table_oid,
            row_id,
        });
        self.count += 1;
        Ok(())
    }
}

/// NOT NULL then CHECK, as PostgreSQL's `ExecConstraints`.
fn check_constraints(
    table: &TableDef,
    row: &Row,
    not_null: &[bool],
    checks: &[BoundCheck],
    ctx: &ExecCtx<'_>,
) -> Result<()> {
    for (i, d) in row.iter().enumerate() {
        if d.is_null() && not_null.get(i).copied().unwrap_or(false) {
            let col = table.columns.get(i).map_or("?", |c| c.name.as_str());
            return Err(Error::new(
                sqlstate::NOT_NULL_VIOLATION,
                format!(
                    "null value in column \"{col}\" of relation \"{}\" violates not-null constraint",
                    table.name
                ),
            )
            .with_detail(failing_row(table, row)));
        }
    }
    for check in checks {
        if eval_bool(&check.expr, row, ctx.session)? == Some(false) {
            return Err(Error::new(
                sqlstate::CHECK_VIOLATION,
                format!(
                    "new row for relation \"{}\" violates check constraint \"{}\"",
                    table.name, check.name
                ),
            )
            .with_detail(failing_row(table, row)));
        }
    }
    Ok(())
}

/// Maximum bytes of each value shown in `Failing row contains (...)`.
const MAX_FIELD_LEN: usize = 64;

/// `Failing row contains (v1, v2, ...).` (PostgreSQL's
/// `ExecBuildSlotValueDescription`: NULL is `null`, long values are clipped
/// to 64 bytes followed by `...`).
pub fn failing_row(table: &TableDef, row: &Row) -> String {
    let vals: Vec<String> = row
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let ty = table
                .columns
                .get(i)
                .map_or(crate::types::SqlType::TEXT, |c| c.ty);
            match io::output_text(d, ty) {
                None => "null".to_owned(),
                Some(s) if s.len() <= MAX_FIELD_LEN => s,
                Some(s) => {
                    let mut end = MAX_FIELD_LEN;
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &s[..end])
                }
            }
        })
        .collect();
    format!("Failing row contains ({}).", vals.join(", "))
}

impl Executor for InsertExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        while let Some(input) = self.input.next(ctx)? {
            self.insert_one(&input, ctx)?;
        }
        Ok(None)
    }

    fn rows_affected(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::executor::eval::tests::{GT, col, int, null, op, text};
    use crate::executor::nodes::test_util::Fixture;
    use crate::executor::nodes::{SeqScanExec, ValuesExec};
    use crate::storage::TableStore;
    use crate::types::SqlType;
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
        };
        f.catalog.put_table(Arc::new(TableDef {
            oid: T,
            schema: "public".into(),
            name: "t".into(),
            columns: vec![
                c("a", 1, SqlType::INT4, true),
                c("b", 2, SqlType::TEXT, false),
                c("c", 3, SqlType::INT4, false),
            ],
            checks: vec![],
        }));
        f.storage.create_table(T).unwrap();
        f
    }

    fn check_a_gt_0() -> BoundCheck {
        BoundCheck {
            name: "t_a_check".into(),
            expr: op(&GT, col(0, SqlType::INT4), int(0)),
        }
    }

    fn insert(rows: Vec<Vec<BoundExpr>>, column_map: Vec<Option<usize>>) -> InsertExec {
        InsertExec::new(
            T,
            Box::new(ValuesExec::new(rows)),
            column_map,
            vec![None, None, Some(int(42))],
            vec![check_a_gt_0()],
            vec![true, false, false],
        )
    }

    fn scan(f: &mut Fixture) -> Vec<Row> {
        let mut e: BoxedExecutor = Box::new(SeqScanExec::new(T));
        f.run(&mut e).unwrap()
    }

    #[test]
    fn inserts_with_defaults_and_undo() {
        let mut f = fixture();
        let mut e: BoxedExecutor = Box::new(insert(
            vec![vec![text("x"), int(1)], vec![null(SqlType::TEXT), int(2)]],
            vec![Some(1), Some(0), None],
        ));
        assert!(f.run(&mut e).unwrap().is_empty());
        assert_eq!(e.rows_affected(), 2);
        assert_eq!(
            scan(&mut f),
            vec![
                vec![Datum::Int4(1), Datum::Text("x".into()), Datum::Int4(42)],
                vec![Datum::Int4(2), Datum::Null, Datum::Int4(42)],
            ]
        );
        assert_eq!(f.txn.undo.len(), 2);
        assert!(matches!(
            f.txn.undo[1],
            UndoEntry::Inserted { table_oid: T, .. }
        ));
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
        // The first row was inserted and logged; the session undoes it.
        assert_eq!(f.txn.undo.len(), 1);
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
            T,
            Box::new(ValuesExec::new(vec![vec![int(5)]])),
            vec![Some(0), None, None],
            vec![None, None, None],
            vec![BoundCheck {
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
        let table = f.catalog.table_by_oid(T).unwrap();
        let long = "é".repeat(40);
        let d = failing_row(
            &table,
            &vec![Datum::Int4(1), Datum::Text(long), Datum::Null],
        );
        assert_eq!(
            d,
            format!("Failing row contains (1, {}..., null).", "é".repeat(32))
        );
    }
    use crate::catalog::CatalogReader;
}
