//! DML の共有部品（`m4/05` §3.6、§7）。INSERT・UPDATE・COPY が使う。
//!
//! - `insert_with_indexes` / `update_with_indexes`: ヒープを書き、`rel.indexes` の各索引（OID 昇順）に項目を入れる。
//!   一意違反（23505）の DETAIL は `unique_violation_detail` がここで補う（D5-16、C-11）。
//! - `RowBuilder` / `RowChecker`: 挿入する行の組み立てと NOT NULL → CHECK の検査（Insert・Update・COPY が共有）。

use super::ExecCtx;
use super::eval::{eval_const, eval_const_pred};
use crate::catalog::TableDef;
use crate::error::{Error, Result, sqlstate};
use crate::planner::physical::{PhysCheck, PhysExpr};
use crate::storage::{IndexHandle, RelHandle, TmResult, UniqueCheck, UpdateOutcome, WriteCtx};
use crate::types::{Datum, Oid, Row, Tid, TypeEnv, io};

/// 行をヒープに書き、`rel.indexes` の各索引に項目を入れる。
///
/// ヒープの書き込みが失敗したら索引には触らない。索引の挿入が失敗しても、ヒープの版とそれまでに入れた
/// 項目は取り消さない（呼び出し元が `Err` を返し、session がトランザクションを中断する。中断したトランザクションの
/// 版は可視性判定と一意性検査の両方で無視される。§7.1）。
pub fn insert_with_indexes(
    ctx: &mut ExecCtx<'_>,
    rel: &RelHandle,
    w: &WriteCtx,
    row: &[Datum],
) -> Result<Tid> {
    let tid = ctx.storage.insert(rel, w, row)?;
    insert_index_entries(ctx, rel, w, row, tid)?;
    Ok(tid)
}

/// 行の新しい版を書き、`TmResult::Ok` なら各索引に新しい TID の項目を入れる。`Ok` 以外は索引に触らない。
/// キー列が変わらなくても全索引に項目を入れる（HOT なし）。旧版の項目は消さない（VACUUM は M5）。
pub fn update_with_indexes(
    ctx: &mut ExecCtx<'_>,
    rel: &RelHandle,
    w: &WriteCtx,
    tid: Tid,
    new_row: &[Datum],
) -> Result<UpdateOutcome> {
    let out = ctx.storage.update(rel, w, ctx.snapshot, tid, new_row)?;
    if out.result != TmResult::Ok {
        return Ok(out);
    }
    let new_tid = out
        .new_tid
        .ok_or_else(|| Error::internal("update returned Ok without a new TID"))?;
    insert_index_entries(ctx, rel, w, new_row, new_tid)?;
    Ok(out)
}

/// `rel.indexes` の各索引に `(row のキー列, tid)` を入れる。
fn insert_index_entries(
    ctx: &ExecCtx<'_>,
    rel: &RelHandle,
    w: &WriteCtx,
    row: &[Datum],
    tid: Tid,
) -> Result<()> {
    for index in rel.indexes.iter() {
        let key = index_key(index, row)?;
        let check = if index.unique {
            UniqueCheck::Check {
                heap: ctx.storage,
                rel,
                own_xid: w.xid,
            }
        } else {
            UniqueCheck::Skip
        };
        ctx.indexes
            .insert(w, index, &key, tid, check)
            .map_err(|e| complete_unique_error(e, index, &key, ctx.type_env))?;
    }
    Ok(())
}

/// 行から索引のキー列の値を取り出す（式索引はない）。
fn index_key(index: &IndexHandle, row: &[Datum]) -> Result<Vec<Datum>> {
    index
        .columns
        .iter()
        .map(|c| {
            usize::try_from(i32::from(c.attnum) - 1)
                .ok()
                .and_then(|i| row.get(i))
                .cloned()
                .ok_or_else(|| {
                    Error::internal(format!(
                        "index \"{}\" key column {} is out of range of the row",
                        index.name, c.attnum
                    ))
                })
        })
        .collect()
}

/// 23505 の DETAIL と `s` / `t` / `n` を、無ければ補う。他のエラーはそのまま返す（D5-16）。
fn complete_unique_error(
    mut e: Error,
    index: &IndexHandle,
    key: &[Datum],
    env: &TypeEnv<'_>,
) -> Error {
    if e.sqlstate != sqlstate::UNIQUE_VIOLATION {
        return e;
    }
    if e.detail.as_deref().is_none_or(str::is_empty) {
        e = e.with_detail(unique_violation_detail(index, key, env));
    }
    if e.table().is_none() || e.schema().is_none() {
        e = e.with_table(index.schema.clone(), index.table_name.clone());
    }
    if e.constraint().is_none() {
        e = e.with_constraint(index.name.clone());
    }
    e
}

/// `Key (a, b)=(1, x) already exists.`。値は切り詰めず、引用符もつけない（NULL は検査しないので現れないが、
/// 万一なら空文字にする）。
pub fn unique_violation_detail(index: &IndexHandle, key: &[Datum], env: &TypeEnv<'_>) -> String {
    let names: Vec<&str> = index.columns.iter().map(|c| c.name.as_str()).collect();
    let vals: Vec<String> = index
        .columns
        .iter()
        .zip(key)
        .map(|(c, d)| io::output_text_env(d, c.ty, env).unwrap_or_default())
        .collect();
    format!(
        "Key ({})=({}) already exists.",
        names.join(", "),
        vals.join(", ")
    )
}

/// 挿入する行の組み立て（Insert ノードと COPY が共有）。
#[derive(Debug, Clone)]
pub struct RowBuilder {
    column_map: Vec<Option<usize>>,
    defaults: Vec<Option<PhysExpr>>,
}

impl RowBuilder {
    pub fn new(column_map: Vec<Option<usize>>, defaults: Vec<Option<PhysExpr>>) -> RowBuilder {
        RowBuilder {
            column_map,
            defaults,
        }
    }

    /// 表の列 i ごとに、`column_map[i] = Some(j)` なら `input[j]`、`None` なら `defaults[i]`（なければ NULL）を
    /// `eval_const` で評価する。
    pub fn build(&self, ctx: &ExecCtx<'_>, input: &Row) -> Result<Row> {
        let empty = Row::new();
        let ec = ctx.eval_ctx();
        self.column_map
            .iter()
            .enumerate()
            .map(|(i, src)| match src {
                Some(j) => input
                    .get(*j)
                    .cloned()
                    .ok_or_else(|| Error::internal(format!("input column {j} out of range"))),
                None => match self.defaults.get(i).and_then(Option::as_ref) {
                    Some(e) => eval_const(e, &empty, &ec),
                    None => Ok(Datum::Null),
                },
            })
            .collect()
    }
}

/// NOT NULL と CHECK の検査（Insert・Update・COPY が共有）。NOT NULL（列順）→ CHECK（名前のバイト順）。
#[derive(Debug, Clone)]
pub struct RowChecker {
    rel_oid: Oid,
    table_name: String,
    not_null: Vec<bool>,
    checks: Vec<PhysCheck>,
}

impl RowChecker {
    /// `checks` は名前のバイト順に整列する（PostgreSQL の `ExecConstraints` と同じ。D5-19）。
    pub fn new(
        rel_oid: Oid,
        table_name: String,
        not_null: Vec<bool>,
        mut checks: Vec<PhysCheck>,
    ) -> RowChecker {
        checks.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        RowChecker {
            rel_oid,
            table_name,
            not_null,
            checks,
        }
    }

    /// 書く前の行を検査する。表の定義（エラーメッセージに使う名前）は、検査が要るときだけカタログから引く。
    pub fn check(&self, ctx: &ExecCtx<'_>, row: &Row) -> Result<()> {
        let needs_table = self
            .not_null
            .iter()
            .zip(row)
            .any(|(nn, d)| *nn && d.is_null())
            || !self.checks.is_empty();
        if !needs_table {
            return Ok(());
        }
        let table = ctx.catalog.table_by_oid(self.rel_oid)?.ok_or_else(|| {
            Error::internal(format!(
                "relation \"{}\" with OID {} does not exist",
                self.table_name, self.rel_oid
            ))
        })?;
        let env = ctx.type_env;
        for (i, d) in row.iter().enumerate() {
            if d.is_null() && self.not_null.get(i).copied().unwrap_or(false) {
                let col = table.columns.get(i).map_or("?", |c| c.name.as_str());
                return Err(Error::new(
                    sqlstate::NOT_NULL_VIOLATION,
                    format!(
                        "null value in column \"{col}\" of relation \"{}\" violates not-null constraint",
                        table.name
                    ),
                )
                .with_detail(failing_row_detail(&table, row, env))
                .with_table(table.schema.clone(), table.name.clone())
                .with_column(col));
            }
        }
        let ec = ctx.eval_ctx();
        for check in &self.checks {
            if eval_const_pred(&check.expr, row, &ec)? == Some(false) {
                return Err(Error::new(
                    sqlstate::CHECK_VIOLATION,
                    format!(
                        "new row for relation \"{}\" violates check constraint \"{}\"",
                        table.name, check.name
                    ),
                )
                .with_detail(failing_row_detail(&table, row, env))
                .with_table(table.schema.clone(), table.name.clone())
                .with_constraint(check.name.clone()));
            }
        }
        Ok(())
    }
}

/// Maximum bytes of each value shown in `Failing row contains (...)`.
const MAX_FIELD_LEN: usize = 64;

/// `Failing row contains (v1, v2, ...).`（PostgreSQL の `ExecBuildSlotValueDescription`: NULL は `null`、
/// 長い値は 64 バイトで切って `...`）。
pub fn failing_row_detail(table: &TableDef, row: &Row, env: &TypeEnv<'_>) -> String {
    let vals: Vec<String> = row
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let ty = table
                .columns
                .get(i)
                .map_or(crate::types::SqlType::TEXT, |c| c.ty);
            match io::output_text_env(d, ty, env) {
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

/// 入力の `[user columns..., ctid, ...]` から ctid を取り出す。
pub(crate) fn tid_at(input: &Row, pos: usize) -> Result<Tid> {
    match input.get(pos) {
        Some(Datum::Tid(t)) => Ok(*t),
        other => Err(Error::internal(format!(
            "input column {pos} must be a ctid, got {other:?}"
        ))),
    }
}

/// 1 つのコマンドで 2 回変更された行の `27000`。
pub(crate) fn already_modified(verb: &str) -> Error {
    Error::new(
        sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION,
        format!(
            "tuple to be {verb} was already modified by an operation triggered by the current command"
        ),
    )
    .with_hint(
        "Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::catalog::fake::table_def;
    use crate::executor::BoxedExecutor;
    use crate::executor::eval::tests::{GT, col, int, null, op, text};
    use crate::executor::nodes::test_util::{Fixture, index_on};
    use crate::executor::nodes::{DeleteExec, InsertExec, UpdateExec, ValuesExec};
    use crate::storage::TableStore;
    use crate::types::SqlType;
    use std::sync::Arc;

    const T: Oid = 16400;
    const PK: Oid = 100;
    const UQ: Oid = 101;

    fn def() -> TableDef {
        let c = |name: &str, attnum, ty, not_null| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: None,
            identity: None,
        };
        table_def(
            T,
            "e5",
            vec![
                c("a", 1, SqlType::INT4, true),
                c("u1", 2, SqlType::INT4, false),
                c("u2", 3, SqlType::TEXT, false),
            ],
            vec![],
        )
    }

    /// `e5(a PRIMARY KEY, u1, u2, UNIQUE (u1, u2))`。索引は OID の昇順。
    fn rel() -> RelHandle {
        let mut rel = RelHandle::from_table(&def());
        rel.indexes = Arc::from(vec![
            index_on(PK, "e5_pkey", "e5", true, &[(1, "a", SqlType::INT4)]),
            index_on(
                UQ,
                "e5_u1_u2_key",
                "e5",
                true,
                &[(2, "u1", SqlType::INT4), (3, "u2", SqlType::TEXT)],
            ),
        ]);
        rel
    }

    fn row(a: i32, u1: Option<i32>, u2: Option<&str>) -> Row {
        vec![
            Datum::Int4(a),
            u1.map_or(Datum::Null, Datum::Int4),
            u2.map_or(Datum::Null, |s| Datum::Text(s.into())),
        ]
    }

    fn put(f: &mut Fixture, rel: &RelHandle, r: &[Datum]) -> Result<Tid> {
        let mut ctx = f.ctx();
        let w = ctx.write_ctx()?;
        insert_with_indexes(&mut ctx, rel, &w, r)
    }

    fn assert_unique_error(e: &Error, constraint: &str, detail: &str) {
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(
            e.message,
            format!("duplicate key value violates unique constraint \"{constraint}\"")
        );
        assert_eq!(e.detail.as_deref(), Some(detail));
        assert_eq!(e.schema(), Some("public"));
        assert_eq!(e.table(), Some("e5"));
        assert_eq!(e.constraint(), Some(constraint));
        assert_eq!(e.column(), None);
    }

    #[test]
    fn insert_adds_an_entry_to_every_index() {
        let mut f = Fixture::new();
        let rel = rel();
        let tid = put(&mut f, &rel, &row(1, Some(10), Some("p"))).unwrap();
        assert_eq!(f.indexes.entries(PK), vec![(vec![Datum::Int4(1)], tid)]);
        assert_eq!(
            f.indexes.entries(UQ),
            vec![(vec![Datum::Int4(10), Datum::Text("p".into())], tid)]
        );
        assert_eq!(f.storage.rows(T).len(), 1);
    }

    #[test]
    fn unique_violation_has_detail_and_fields() {
        let mut f = Fixture::new();
        let rel = rel();
        put(&mut f, &rel, &row(1, Some(10), Some("p"))).unwrap();
        let e = put(&mut f, &rel, &row(1, None, None)).unwrap_err();
        assert_unique_error(&e, "e5_pkey", "Key (a)=(1) already exists.");
        // 複数列
        let e = put(&mut f, &rel, &row(2, Some(10), Some("p"))).unwrap_err();
        assert_unique_error(&e, "e5_u1_u2_key", "Key (u1, u2)=(10, p) already exists.");
    }

    #[test]
    fn the_smallest_oid_index_is_reported_first() {
        let mut f = Fixture::new();
        let rel = rel();
        put(&mut f, &rel, &row(1, Some(10), Some("p"))).unwrap();
        // PK と UNIQUE の両方に違反する。
        let e = put(&mut f, &rel, &row(1, Some(10), Some("p"))).unwrap_err();
        assert_unique_error(&e, "e5_pkey", "Key (a)=(1) already exists.");
    }

    #[test]
    fn null_keys_do_not_conflict_and_long_values_are_not_clipped() {
        let mut f = Fixture::new();
        let rel = rel();
        put(&mut f, &rel, &row(1, None, Some("p"))).unwrap();
        put(&mut f, &rel, &row(2, None, Some("p"))).unwrap();
        put(&mut f, &rel, &row(3, Some(1), None)).unwrap();
        put(&mut f, &rel, &row(4, Some(1), None)).unwrap();
        let long = "x".repeat(100);
        put(&mut f, &rel, &row(5, Some(7), Some(&long))).unwrap();
        let e = put(&mut f, &rel, &row(6, Some(7), Some(&long))).unwrap_err();
        assert_eq!(
            e.detail.as_deref(),
            Some(format!("Key (u1, u2)=(7, {long}) already exists.").as_str())
        );
    }

    #[test]
    fn rows_of_the_same_statement_collide_and_a_failure_keeps_earlier_entries() {
        let mut f = Fixture::new();
        let rel = rel();
        put(&mut f, &rel, &row(1, Some(1), Some("a"))).unwrap();
        // PK の項目は入るが、2 つ目の索引で失敗する。取り消さない。
        let e = put(&mut f, &rel, &row(2, Some(1), Some("a"))).unwrap_err();
        assert_eq!(e.constraint(), Some("e5_u1_u2_key"));
        assert_eq!(f.indexes.len(PK), 2);
        assert_eq!(f.indexes.len(UQ), 1);
        assert_eq!(f.storage.rows(T).len(), 2);
    }

    #[test]
    fn complete_unique_error_keeps_what_the_index_set_and_ignores_other_errors() {
        let rel = rel();
        let env = TypeEnv::default();
        let idx = &rel.indexes[0];
        let key = [Datum::Int4(1)];
        let given = Error::new(sqlstate::UNIQUE_VIOLATION, "dup")
            .with_detail("my detail")
            .with_table("s", "t")
            .with_constraint("c");
        let e = complete_unique_error(given, idx, &key, &env);
        assert_eq!(e.detail.as_deref(), Some("my detail"));
        assert_eq!(
            (e.schema(), e.table(), e.constraint()),
            (Some("s"), Some("t"), Some("c"))
        );
        let other = Error::new(sqlstate::INTERNAL_ERROR, "x");
        let e = complete_unique_error(other, idx, &key, &env);
        assert!(e.detail.is_none() && e.diag.is_none());
        assert_eq!(
            unique_violation_detail(idx, &key, &env),
            "Key (a)=(1) already exists."
        );
    }

    fn update(
        f: &mut Fixture,
        rel: &RelHandle,
        tid: Tid,
        new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        let mut ctx = f.ctx();
        let w = ctx.write_ctx()?;
        update_with_indexes(&mut ctx, rel, &w, tid, new_row)
    }

    #[test]
    fn update_without_key_change_adds_entries_and_does_not_conflict() {
        let mut f = Fixture::new();
        let rel = rel();
        let tid = put(&mut f, &rel, &row(1, Some(10), Some("p"))).unwrap();
        // 一意索引のキーが同じ（旧版は自分が削除済みで `fetch_dirty` が `Invisible`）。
        let out = update(&mut f, &rel, tid, &row(1, Some(10), Some("q"))).unwrap();
        assert_eq!(out.result, TmResult::Ok);
        let new_tid = out.new_tid.unwrap();
        assert_ne!(new_tid, tid);
        // 旧版の項目は残り、新しい版の項目が増える。
        assert_eq!(f.indexes.len(PK), 2);
        assert_eq!(f.indexes.len(UQ), 2);
        assert!(f.indexes.entries(PK).iter().any(|(_, t)| *t == new_tid));
    }

    #[test]
    fn update_to_a_taken_key_is_a_unique_violation() {
        let mut f = Fixture::new();
        let rel = rel();
        put(&mut f, &rel, &row(1, Some(1), Some("a"))).unwrap();
        let tid = put(&mut f, &rel, &row(2, Some(2), Some("b"))).unwrap();
        let e = update(&mut f, &rel, tid, &row(1, Some(2), Some("b"))).unwrap_err();
        assert_unique_error(&e, "e5_pkey", "Key (a)=(1) already exists.");
    }

    #[test]
    fn update_that_is_not_ok_leaves_the_indexes_alone() {
        let mut f = Fixture::new();
        let rel = rel();
        let tid = put(&mut f, &rel, &row(1, Some(1), Some("a"))).unwrap();
        f.storage.force_result(TmResult::SelfModified { cmax: 0 });
        let out = update(&mut f, &rel, tid, &row(1, Some(1), Some("b"))).unwrap();
        assert_eq!(out.result, TmResult::SelfModified { cmax: 0 });
        assert_eq!((f.indexes.len(PK), f.indexes.len(UQ)), (1, 1));
        // 存在しない TID は Invisible。
        let out = update(
            &mut f,
            &rel,
            Tid {
                block: 0,
                offset: 9,
            },
            &row(1, None, None),
        )
        .unwrap();
        assert_eq!(out.result, TmResult::Invisible);
        assert_eq!((f.indexes.len(PK), f.indexes.len(UQ)), (1, 1));
    }

    // ----- RowBuilder -----

    #[test]
    fn row_builder_uses_input_defaults_and_null() {
        let mut f = Fixture::new();
        let ctx = f.ctx();
        let b = RowBuilder::new(vec![Some(1), None, None], vec![None, Some(int(42)), None]);
        let r = b
            .build(&ctx, &vec![Datum::Int4(0), Datum::Int4(9)])
            .unwrap();
        assert_eq!(r, vec![Datum::Int4(9), Datum::Int4(42), Datum::Null]);
        // 入力の列が足りなければ内部エラー。
        let e = b.build(&ctx, &vec![Datum::Int4(0)]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    // ----- RowChecker -----

    fn checker(checks: Vec<PhysCheck>) -> RowChecker {
        RowChecker::new(T, "e5".into(), vec![true, false, true], checks)
    }

    fn check_on(name: &str, column: usize, gt: i32) -> PhysCheck {
        PhysCheck {
            name: name.into(),
            expr: op(&GT, col(column, SqlType::INT4), int(gt)),
        }
    }

    fn fixture_with_table() -> Fixture {
        let mut f = Fixture::new();
        f.catalog.put_table(Arc::new(def()));
        f
    }

    #[test]
    fn not_null_violations_report_the_first_column_with_fields() {
        let mut f = fixture_with_table();
        let ctx = f.ctx();
        // a と u2 が NULL: 列順で a。
        let r = vec![Datum::Null, Datum::Int4(1), Datum::Null];
        let e = checker(vec![]).check(&ctx, &r).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(
            e.message,
            "null value in column \"a\" of relation \"e5\" violates not-null constraint"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Failing row contains (null, 1, null).")
        );
        assert_eq!(
            (e.schema(), e.table(), e.column(), e.constraint()),
            (Some("public"), Some("e5"), Some("a"), None)
        );
        let r = vec![Datum::Int4(2), Datum::Null, Datum::Null];
        let e = checker(vec![]).check(&ctx, &r).unwrap_err();
        assert_eq!(e.column(), Some("u2"));
    }

    #[test]
    fn check_violations_are_found_in_name_order_and_null_passes() {
        let mut f = fixture_with_table();
        let ctx = f.ctx();
        // 名前順は "e5_a_check" < "e5_u1_check"（渡す順に依らない）。
        let c = checker(vec![
            check_on("e5_u1_check", 1, 100),
            check_on("e5_a_check", 0, 100),
        ]);
        let r = row(1, Some(1), Some("p"));
        let e = c.check(&ctx, &r).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::CHECK_VIOLATION);
        assert_eq!(
            e.message,
            "new row for relation \"e5\" violates check constraint \"e5_a_check\""
        );
        assert_eq!(e.detail.as_deref(), Some("Failing row contains (1, 1, p)."));
        assert_eq!(
            (e.schema(), e.table(), e.column(), e.constraint()),
            (Some("public"), Some("e5"), None, Some("e5_a_check"))
        );
        // 最初の違反が a でなければ u1 が報告される。
        let r = row(500, Some(1), Some("p"));
        let e = c.check(&ctx, &r).unwrap_err();
        assert_eq!(e.constraint(), Some("e5_u1_check"));
        // CHECK の結果が NULL なら通す。
        let r = row(500, None, Some("p"));
        c.check(&ctx, &r).unwrap();
    }

    #[test]
    fn a_valid_row_does_not_consult_the_catalog() {
        // 表がカタログに無くても、違反がなければ引かない。
        let mut f = Fixture::new();
        let ctx = f.ctx();
        checker(vec![])
            .check(&ctx, &row(1, None, Some("p")))
            .unwrap();
        // 違反があって表が無ければ内部エラー。
        let e = checker(vec![])
            .check(&ctx, &vec![Datum::Null, Datum::Null, Datum::Null])
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    // ----- ノードと索引 -----

    fn lit_row(r: &[Datum]) -> Vec<PhysExpr> {
        r.iter()
            .map(|d| crate::executor::eval::tests::lit(d.clone(), SqlType::TEXT))
            .collect()
    }

    fn insert_node(rel: &RelHandle, rows: &[Row]) -> BoxedExecutor {
        Box::new(InsertExec::new(
            rel.clone(),
            "e5".into(),
            Box::new(ValuesExec::new(rows.iter().map(|r| lit_row(r)).collect())),
            vec![Some(0), Some(1), Some(2)],
            vec![None, None, None],
            vec![],
            vec![true, false, false],
        ))
    }

    #[test]
    fn insert_node_collides_within_the_same_statement() {
        let mut f = fixture_with_table();
        let rel = rel();
        let mut e = insert_node(
            &rel,
            &[row(1, None, None), row(2, None, None), row(1, None, None)],
        );
        let err = f.run(&mut e).unwrap_err();
        assert_unique_error(&err, "e5_pkey", "Key (a)=(1) already exists.");
        // 先に入れた 2 行と、失敗した行のヒープの版は取り消さない（トランザクションが中断する）。
        assert_eq!(f.storage.rows(T).len(), 3);
        assert_eq!(f.indexes.len(PK), 2);
    }

    #[test]
    fn update_node_maintains_indexes_and_collides_in_physical_order() {
        let mut f = fixture_with_table();
        let rel = rel();
        let t1 = put(&mut f, &rel, &row(1, None, None)).unwrap();
        let t2 = put(&mut f, &rel, &row(2, None, None)).unwrap();
        // UPDATE e5 SET a = a + 1: 物理順で最初の行が `Key (a)=(2)` に当たる。
        let input = |t: Tid, a: i32, new: i32| {
            vec![
                int(a),
                null(SqlType::INT4),
                null(SqlType::TEXT),
                crate::executor::eval::tests::lit(
                    Datum::Tid(t),
                    SqlType::of(crate::types::oid::TID),
                ),
                int(new),
            ]
        };
        let mut e: BoxedExecutor = Box::new(UpdateExec::new(
            rel.clone(),
            "e5".into(),
            Box::new(ValuesExec::new(vec![input(t1, 1, 2), input(t2, 2, 3)])),
            3,
            vec![(0, 4)],
            vec![],
            vec![true, false, false],
            None,
        ));
        let err = f.run(&mut e).unwrap_err();
        assert_unique_error(&err, "e5_pkey", "Key (a)=(2) already exists.");
        // 1 行目は書かれてヒープが更新済み、索引の項目は増えていない（PK で失敗）。
        assert_eq!(f.indexes.len(UQ), 2);
        // 値の衝突が無い UPDATE は索引に項目を足す。
        let mut f = fixture_with_table();
        let t1 = put(&mut f, &rel, &row(1, None, None)).unwrap();
        let mut e: BoxedExecutor = Box::new(UpdateExec::new(
            rel.clone(),
            "e5".into(),
            Box::new(ValuesExec::new(vec![input(t1, 1, 1)])),
            3,
            vec![(0, 4)],
            vec![],
            vec![true, false, false],
            None,
        ));
        f.run(&mut e).unwrap();
        assert_eq!(e.rows_affected(), 1);
        assert_eq!((f.indexes.len(PK), f.indexes.len(UQ)), (2, 2));
    }

    #[test]
    fn delete_node_does_not_touch_indexes() {
        let mut f = fixture_with_table();
        let rel = rel();
        let t1 = put(&mut f, &rel, &row(1, Some(1), Some("a"))).unwrap();
        let mut e: BoxedExecutor = Box::new(DeleteExec::new(
            rel.clone(),
            Box::new(ValuesExec::new(vec![vec![
                int(1),
                int(1),
                text("a"),
                crate::executor::eval::tests::lit(
                    Datum::Tid(t1),
                    SqlType::of(crate::types::oid::TID),
                ),
            ]])),
            3,
        ));
        f.run(&mut e).unwrap();
        assert_eq!(e.rows_affected(), 1);
        assert_eq!((f.indexes.len(PK), f.indexes.len(UQ)), (1, 1));
        assert!(f.storage.rows(T).is_empty());
        // 削除済みの版の項目は、同じキーの再挿入を妨げない（`fetch_dirty` が `Invisible`）。
        put(&mut f, &rel, &row(1, Some(1), Some("a"))).unwrap();
        // `TableStore` が使われていること（未使用 import 防止）。
        assert!(f.storage.storage_exists(rel.locator).is_ok());
    }
}
