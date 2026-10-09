//! LK-4c の結合テスト（`m5/01` §7.2 のうち、DDL のロックモード・名前予約・DROP の第 2 段・
//! シーケンス関数の実行時ロック）。`TestCluster` の上でアクタースケジューラが複数セッションを動かす。

use yuzhu_core::error::sqlstate;
use yuzhu_core::testing::TestCluster;
use yuzhu_core::testing::actors::{ActorId, Scheduler, StepOutcome, StepResult};

fn ok(s: &mut Scheduler<'_>, a: ActorId, sql: &str) {
    match s.submit(a, sql).expect("no hang") {
        StepOutcome::Done(Ok(_)) => {}
        other => panic!("{sql}: expected success, got {other:?}"),
    }
}

fn err_code(s: &mut Scheduler<'_>, a: ActorId, sql: &str) -> String {
    match s.submit(a, sql).expect("no hang") {
        StepOutcome::Done(Err(e)) => e.sqlstate.0.to_owned(),
        other => panic!("{sql}: expected an error, got {other:?}"),
    }
}

fn blocks(s: &mut Scheduler<'_>, a: ActorId, sql: &str) {
    match s.submit(a, sql).expect("no hang") {
        StepOutcome::Blocked => {}
        other => panic!("{sql}: expected to block, got {other:?}"),
    }
}

fn settle_one(s: &mut Scheduler<'_>, a: ActorId) -> StepResult {
    settle_all(s, &[a]).remove(0).1
}

/// 指定のアクターがすべて完了するまで `settle` を繰り返す（待ちが解けて起きるのに少しかかる）。
fn settle_all(s: &mut Scheduler<'_>, ids: &[ActorId]) -> Vec<(ActorId, StepResult)> {
    let mut got: Vec<(ActorId, StepResult)> = Vec::new();
    let start = std::time::Instant::now();
    while got.len() < ids.len() {
        assert!(
            start.elapsed().as_secs() < 10,
            "waiters did not finish: {:?}",
            s.trace()
        );
        got.extend(s.settle().expect("settle"));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    ids.iter()
        .map(|i| {
            let p = got.iter().position(|(g, _)| g == i).expect("finished");
            got.remove(p)
        })
        .collect()
}

#[test]
fn drop_waits_for_reader_and_queued_select_sees_42p01() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b, c) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
        s.spawn("c", "postgres").unwrap(),
    );
    ok(&mut s, a, "CREATE TABLE t (id int)");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "SELECT * FROM t");
    // 書き込みは待たない。
    ok(&mut s, b, "INSERT INTO t VALUES (1)");
    blocks(&mut s, b, "DROP TABLE t");
    // C の SELECT は DROP の後ろに並ぶ。
    blocks(&mut s, c, "SELECT * FROM t");
    ok(&mut s, a, "COMMIT");
    let done = settle_all(&mut s, &[b, c]);
    let rb = done.iter().find(|(i, _)| *i == b).unwrap();
    assert!(rb.1.is_ok());
    let rc = done.iter().find(|(i, _)| *i == c).unwrap();
    assert_eq!(
        rc.1.as_ref().unwrap_err().sqlstate,
        sqlstate::UNDEFINED_TABLE
    );
    s.abandon();
}

#[test]
fn create_index_takes_share_and_blocks_writers() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "CREATE TABLE t (id int)");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "CREATE INDEX t_id ON t (id)");
    // Share は RowExclusive と衝突するが AccessShare とは衝突しない。
    ok(&mut s, b, "SELECT * FROM t");
    blocks(&mut s, b, "INSERT INTO t VALUES (1)");
    ok(&mut s, a, "COMMIT");
    assert!(settle_one(&mut s, b).is_ok());
    s.abandon();
}

#[test]
fn name_reservation_commit_gives_42p07() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "CREATE TABLE x (id int)");
    blocks(&mut s, b, "CREATE TABLE x (id int)");
    ok(&mut s, a, "COMMIT");
    let r = settle_one(&mut s, b);
    assert_eq!(r.unwrap_err().sqlstate, sqlstate::DUPLICATE_TABLE);
    s.abandon();
}

#[test]
fn name_reservation_abort_lets_the_second_succeed() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "CREATE TABLE x (id int)");
    blocks(&mut s, b, "CREATE TABLE x (id int)");
    ok(&mut s, a, "ROLLBACK");
    assert!(settle_one(&mut s, b).is_ok());
    s.abandon();
}

#[test]
fn create_while_uncommitted_drop_does_not_wait() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "CREATE TABLE x (id int)");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "DROP TABLE x");
    // 待たずに 42P07（PostgreSQL と同じ。E20）。
    assert_eq!(err_code(&mut s, b, "CREATE TABLE x (id int)"), "42P07");
    ok(&mut s, a, "COMMIT");
    ok(&mut s, b, "CREATE TABLE x (id int)");
    s.abandon();
}

#[test]
fn nextval_holds_row_exclusive_and_drop_sequence_waits() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "CREATE SEQUENCE sq");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "SELECT nextval('sq')");
    blocks(&mut s, b, "DROP SEQUENCE sq");
    ok(&mut s, a, "COMMIT");
    assert!(settle_one(&mut s, b).is_ok());
    s.abandon();
}

#[test]
fn drop_table_waits_for_owned_serial_sequence_user() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let (a, b) = (
        s.spawn("a", "postgres").unwrap(),
        s.spawn("b", "postgres").unwrap(),
    );
    ok(&mut s, a, "CREATE TABLE t (id serial, v int)");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "SELECT nextval('t_id_seq')");
    // 表のロックは別だが、所有シーケンスの AccessExclusive で待つ（LK-D16）。
    blocks(&mut s, b, "DROP TABLE t");
    ok(&mut s, a, "COMMIT");
    assert!(settle_one(&mut s, b).is_ok());
    s.abandon();
}

#[test]
fn lock_table_outside_block_is_25p01() {
    let tc = TestCluster::new();
    let mut s = Scheduler::new(&tc);
    let a = s.spawn("a", "postgres").unwrap();
    ok(&mut s, a, "CREATE TABLE t (id int)");
    assert_eq!(err_code(&mut s, a, "LOCK TABLE t"), "25P01");
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "LOCK TABLE t IN SHARE MODE");
    ok(&mut s, a, "COMMIT");
    s.abandon();
}
