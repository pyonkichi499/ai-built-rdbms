//! Executor node: UPDATE (`m4/05` §7.6). 入力の行は
//! 「対象表のユーザー列（`n_user_cols` 個）++ ctid ++ 新しい値（`assigned.len()` 個）」。
//!
//! 代入式は planner の Project が旧い行に対して評価済み（`SET a = b, b = a` は入れ替えになる）。ここは位置だけを
//! 見る: 旧行の先頭 `n_user_cols` 列を複製し、`assigned` の位置の値で置き換える → NOT NULL → CHECK
//! （ヒープに触る前）→ `update_with_indexes`。`returning` が無ければ行を返さず、件数は `rows_affected`。

use crate::error::{Error, Result};
use crate::executor::dml::{RowChecker, already_modified, tid_at, update_with_indexes};
use crate::executor::eval::eval;
use crate::executor::{BoxedExecutor, ExecCtx, Executor};
use crate::planner::physical::{PhysCheck, PhysExpr};
use crate::storage::{RelHandle, TmResult};
use crate::types::Row;

pub struct UpdateExec {
    rel: RelHandle,
    table_name: String,
    input: BoxedExecutor,
    n_user_cols: usize,
    /// `(attnum - 1, 入力の列位置)`。
    assigned: Vec<(usize, usize)>,
    checker: RowChecker,
    returning: Option<Vec<PhysExpr>>,
    count: u64,
    done: bool,
}

impl std::fmt::Debug for UpdateExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateExec")
            .field("table", &self.table_name)
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl UpdateExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rel: RelHandle,
        table_name: String,
        input: BoxedExecutor,
        n_user_cols: usize,
        assigned: Vec<(usize, usize)>,
        checks: Vec<PhysCheck>,
        not_null: Vec<bool>,
        returning: Option<Vec<PhysExpr>>,
    ) -> Self {
        let checker = RowChecker::new(rel.oid, table_name.clone(), not_null, checks);
        UpdateExec {
            rel,
            table_name,
            input,
            n_user_cols,
            assigned,
            checker,
            returning,
            count: 0,
            done: false,
        }
    }

    /// 更新後の行。幅が合わない入力は `XX000`。
    fn new_row(&self, input: &Row) -> Result<Row> {
        let want = self.n_user_cols + 1 + self.assigned.len();
        if input.len() != want {
            return Err(Error::internal(format!(
                "expected {want} input columns (user columns, ctid and new values), got {}",
                input.len()
            )));
        }
        let mut new: Row = input[..self.n_user_cols].to_vec();
        for (col, pos) in &self.assigned {
            let v = input
                .get(*pos)
                .ok_or_else(|| Error::internal(format!("new value position {pos} out of range")))?;
            *new.get_mut(*col).ok_or_else(|| {
                Error::internal(format!("assignment to column {col} out of range"))
            })? = v.clone();
        }
        Ok(new)
    }

    /// 1 行を更新する。RETURNING があり、更新した行ならその行を返す。
    fn update_one(&mut self, input: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let new = self.new_row(input)?;
        let tid = tid_at(input, self.n_user_cols)?;
        self.checker.check(ctx, &new)?;
        let w = ctx.write_ctx()?;
        let out = update_with_indexes(ctx, &self.rel, &w, tid, &new)?;
        match out.result {
            TmResult::Ok => self.count += 1,
            TmResult::SelfModified { cmax } => {
                if cmax != w.cid {
                    return Err(already_modified("updated"));
                }
                return Ok(None);
            }
            other => {
                return Err(Error::internal(format!(
                    "unexpected result of update on table \"{}\": {other:?}",
                    self.table_name
                )));
            }
        }
        let Some(returning) = &self.returning else {
            return Ok(None);
        };
        returning
            .iter()
            .map(|e| eval(e, &new, ctx))
            .collect::<Result<Row>>()
            .map(Some)
    }
}

impl Executor for UpdateExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        while let Some(input) = self.input.next(ctx)? {
            ctx.check_interrupts()?;
            if let Some(out) = self.update_one(&input, ctx)? {
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
    use crate::storage::TmResult;
    use crate::types::{Datum, Oid, SqlType, Tid};

    const T: Oid = 16384;

    fn table() -> crate::catalog::TableDef {
        let c = |name: &str, attnum, ty| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null: false,
            default: None,
            identity: None,
        };
        table_def(
            T,
            "t",
            vec![
                c("a", 1, SqlType::INT4),
                c("b", 2, SqlType::TEXT),
                c("c", 3, SqlType::INT4),
            ],
            vec![],
        )
    }

    fn fixture(rows: &[(i32, &str, i32)]) -> Fixture {
        let mut f = Fixture::new();
        for (a, b, c) in rows {
            f.storage.add_row(
                T,
                vec![Datum::Int4(*a), Datum::Text((*b).into()), Datum::Int4(*c)],
            );
        }
        f.catalog.put_table(std::sync::Arc::new(table()));
        f
    }

    fn ctid(offset: u16) -> crate::planner::physical::PhysExpr {
        crate::executor::eval::tests::lit(
            Datum::Tid(Tid { block: 0, offset }),
            SqlType::of(crate::types::oid::TID),
        )
    }

    /// 入力の行 = `[a, b, c, ctid, 新しい値…]`。
    fn update(
        input_rows: Vec<Vec<crate::planner::physical::PhysExpr>>,
        assigned: Vec<(usize, usize)>,
        checks: Vec<PhysCheck>,
    ) -> UpdateExec {
        UpdateExec::new(
            RelHandle::from_table(&table()),
            "t".into(),
            Box::new(ValuesExec::new(input_rows)),
            3,
            assigned,
            checks,
            vec![true, false, false],
            None,
        )
    }

    #[test]
    fn update_node_uses_positions_only() {
        let mut f = fixture(&[(1, "x", 10)]);
        // 旧行 (1, x, 10)、新しい値 (99, 7)。assigned: 列 0 ← 位置 4、列 2 ← 位置 5。
        let row = vec![int(1), text("x"), int(10), ctid(1), int(99), int(7)];
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row], vec![(0, 4), (2, 5)], vec![]));
        assert!(f.run(&mut e).unwrap().is_empty());
        assert_eq!(e.rows_affected(), 1);
        assert_eq!(
            f.storage.rows(T),
            vec![vec![
                Datum::Int4(99),
                Datum::Text("x".into()),
                Datum::Int4(7)
            ]]
        );
    }

    #[test]
    fn malformed_input_rows_are_internal_errors() {
        let mut f = fixture(&[(1, "x", 10)]);
        // 幅が合わない（新しい値が足りない）。
        let short = vec![int(1), text("x"), int(10), ctid(1)];
        let e = f
            .run(&mut (Box::new(update(vec![short], vec![(0, 4)], vec![])) as _))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        // ctid の位置が ctid でない。
        let bad = vec![int(1), text("x"), int(10), int(5), int(9)];
        let e = f
            .run(&mut (Box::new(update(vec![bad], vec![(0, 4)], vec![])) as _))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert_eq!(f.storage.rows(T).len(), 1);
    }

    #[test]
    fn check_violation_is_found_before_writing_and_in_name_order() {
        let mut f = fixture(&[(1, "x", 10)]);
        let checks = vec![
            PhysCheck {
                name: "t_b_check".into(),
                expr: op(&GT, col(0, SqlType::INT4), int(100)),
            },
            PhysCheck {
                name: "t_a_check".into(),
                expr: op(&GT, col(0, SqlType::INT4), int(50)),
            },
        ];
        let row = vec![int(1), text("x"), int(10), ctid(1), int(0)];
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row], vec![(0, 4)], checks));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::CHECK_VIOLATION);
        assert!(err.message.contains("t_a_check"), "{}", err.message);
        assert_eq!(
            err.detail.as_deref(),
            Some("Failing row contains (0, x, 10).")
        );
        // ヒープは書かれていない。
        assert!(f.storage.writes().is_empty());
    }

    #[test]
    fn not_null_violation_leaves_the_row() {
        let mut f = fixture(&[(1, "x", 10)]);
        let row = vec![int(1), text("x"), int(10), ctid(1), null(SqlType::INT4)];
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row], vec![(0, 4)], vec![]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(f.storage.rows(T)[0][0], Datum::Int4(1));
    }

    #[test]
    fn self_modified_rows_and_other_results() {
        let row = || vec![int(1), text("x"), int(10), ctid(1), int(5)];
        // 同じコマンドで変更済み: 飛ばして数えない。
        let mut f = fixture(&[(1, "x", 10)]);
        f.storage.force_result(TmResult::SelfModified { cmax: 0 });
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row()], vec![(0, 4)], vec![]));
        f.run(&mut e).unwrap();
        assert_eq!(e.rows_affected(), 0);
        // 以前のコマンド: 27000。
        let mut f = fixture(&[(1, "x", 10)]);
        f.storage.force_result(TmResult::SelfModified { cmax: 7 });
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row()], vec![(0, 4)], vec![]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION);
    }

    #[test]
    fn returning_uses_the_new_row_and_dml_rewind_is_internal_error() {
        let mut f = fixture(&[(1, "x", 10)]);
        let row = vec![int(1), text("x"), int(10), ctid(1), int(99)];
        let mut u = update(vec![row], vec![(0, 4)], vec![]);
        u.returning = Some(vec![col(0, SqlType::INT4)]);
        let mut e: crate::executor::BoxedExecutor = Box::new(u);
        assert_eq!(f.run(&mut e).unwrap(), vec![vec![Datum::Int4(99)]]);
        let mut ctx = f.ctx();
        assert_eq!(
            e.rewind(&mut ctx).unwrap_err().sqlstate,
            sqlstate::INTERNAL_ERROR
        );
    }

    #[test]
    fn stops_on_cancel_per_input_row() {
        let mut f = fixture(&[(1, "x", 10)]);
        f.interrupts.request_cancel();
        let row = vec![int(1), text("x"), int(10), ctid(1), int(99)];
        let mut e: crate::executor::BoxedExecutor =
            Box::new(update(vec![row], vec![(0, 4)], vec![]));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::QUERY_CANCELED);
        assert_eq!(f.storage.rows(T)[0][0], Datum::Int4(1));
    }
}
