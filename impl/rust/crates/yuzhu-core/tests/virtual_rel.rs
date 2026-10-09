//! 仮想リレーション `pg_locks` の SQL からの確認（M5 LK-5。`spec/design/m5/01-lock-txn.md` §6.5）。
//!
//! セッションが `RuntimeInfo::lock_status` を配線するのは LK-4 なので、ここでは行の中身ではなく
//! カタログ・解析・計画・実行の通しと、DML・DDL のエラーを確かめる。

use yuzhu_core::testing::{TestCluster, run_sql};

fn cluster() -> (TestCluster, yuzhu_core::Session) {
    let c = TestCluster::new();
    let s = c.session("postgres").unwrap();
    (c, s)
}

fn first_error(sql: &str) -> yuzhu_core::Error {
    let (_c, mut s) = cluster();
    let out = run_sql(&mut s, sql);
    out.errors.into_iter().next().expect("an error")
}

#[test]
fn pg_class_has_a_view_row_for_pg_locks() {
    let (_c, mut s) = cluster();
    let out = run_sql(
        &mut s,
        "select relkind, relnamespace::regnamespace::text, relfilenode, relnatts from pg_class where relname = 'pg_locks'",
    );
    assert!(out.is_ok(), "{:?}", out.errors);
    assert_eq!(out.text_rows(), vec![vec!["v", "pg_catalog", "0", "16"]]);
}

#[test]
fn pg_attribute_lists_the_sixteen_columns() {
    let (_c, mut s) = cluster();
    let out = run_sql(
        &mut s,
        "select attname, atttypid::regtype::text from pg_attribute where attrelid = 'pg_locks'::regclass and attnum > 0 order by attnum",
    );
    assert!(out.is_ok(), "{:?}", out.errors);
    let rows = out.text_rows();
    assert_eq!(rows.len(), 16);
    assert_eq!(rows[0], vec!["locktype", "text"]);
    assert_eq!(rows[6], vec!["transactionid", "xid"]);
    assert_eq!(rows[11], vec!["pid", "integer"]);
    assert_eq!(rows[15], vec!["waitstart", "timestamp with time zone"]);
}

#[test]
fn select_from_pg_locks_runs() {
    let (_c, mut s) = cluster();
    for sql in [
        "select * from pg_locks",
        "select count(*) from pg_locks where relation = 0 and granted",
        "select locktype, mode from pg_locks l join pg_class c on c.oid = l.relation order by 1, 2",
        "select pid, ctid from pg_locks",
    ] {
        let out = run_sql(&mut s, sql);
        // 最後の文は PG ではビューに ctid が無いので失敗してもよい。実行できる形だけ確かめる。
        if !sql.contains("ctid") {
            assert!(out.is_ok(), "{sql}: {:?}", out.errors);
        }
    }
    let out = run_sql(&mut s, "select count(*) from pg_locks");
    assert!(out.is_ok(), "{:?}", out.errors);
}

#[test]
fn explain_shows_a_scan_on_pg_locks() {
    let (_c, mut s) = cluster();
    let out = run_sql(&mut s, "explain select * from pg_locks where granted");
    assert!(out.is_ok(), "{:?}", out.errors);
    assert!(out.text_rows()[0][0].contains("pg_locks"), "{:?}", out.rows);
}

#[test]
fn dml_on_pg_locks_is_55000() {
    for (sql, msg) in [
        (
            "insert into pg_locks (pid) values (1)",
            "cannot insert into view \"pg_locks\"",
        ),
        (
            "update pg_locks set pid = 1",
            "cannot update view \"pg_locks\"",
        ),
        (
            "delete from pg_locks",
            "cannot delete from view \"pg_locks\"",
        ),
    ] {
        let e = first_error(sql);
        assert_eq!(e.sqlstate.code(), "55000", "{sql}");
        assert_eq!(e.message, msg);
        assert!(e.detail.is_some() && e.hint.is_some(), "{sql}");
    }
}

#[test]
fn truncate_and_drop_table_on_pg_locks_are_42809() {
    let e = first_error("truncate pg_locks");
    assert_eq!(e.sqlstate.code(), "42809");
    assert_eq!(e.message, "\"pg_locks\" is not a table");
    let e = first_error("drop table pg_locks");
    assert_eq!(e.sqlstate.code(), "42809");
    assert_eq!(e.message, "\"pg_locks\" is not a table");
    assert_eq!(e.hint.as_deref(), Some("Use DROP VIEW to remove a view."));
}

#[test]
fn pg_blocking_pids_returns_an_empty_array_for_an_idle_backend() {
    let (_c, mut s) = cluster();
    let out = run_sql(
        &mut s,
        "select pg_blocking_pids(pg_backend_pid()), pg_typeof(pg_blocking_pids(0))",
    );
    assert!(out.is_ok(), "{:?}", out.errors);
    assert_eq!(out.text_rows(), vec![vec!["{}", "integer[]"]]);
}
