//! REPEATABLE READ の結合テスト（`m5/02` §5.8・§6.7、RW-D10 / D11 / D17）。
//! `TestCluster` の上でアクタースケジューラが複数セッションを動かす。

use yuzhu_core::testing::TestCluster;
use yuzhu_core::testing::actors::{ActorId, Rows, Scheduler, StepOutcome, StepResult};

fn rows(s: &mut Scheduler<'_>, a: ActorId, sql: &str) -> Rows {
    match s.submit(a, sql).expect("no hang") {
        StepOutcome::Done(Ok(r)) => r,
        other => panic!("{sql}: expected success, got {other:?}"),
    }
}

fn ok(s: &mut Scheduler<'_>, a: ActorId, sql: &str) {
    rows(s, a, sql);
}

fn one(s: &mut Scheduler<'_>, a: ActorId, sql: &str) -> String {
    rows(s, a, sql)[0][0].clone().unwrap_or_default()
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
        other @ StepOutcome::Done(_) => panic!("{sql}: expected to block, got {other:?}"),
    }
}

fn settle_one(s: &mut Scheduler<'_>, a: ActorId) -> StepResult {
    let start = std::time::Instant::now();
    loop {
        assert!(start.elapsed().as_secs() < 10, "{:?}", s.trace());
        for (i, r) in s.settle().expect("settle") {
            if i == a {
                return r;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn setup(tc: &TestCluster) -> (Scheduler<'_>, ActorId, ActorId) {
    let mut s = Scheduler::new(tc);
    let a = s.spawn("a", "postgres").unwrap();
    let b = s.spawn("b", "postgres").unwrap();
    ok(&mut s, a, "CREATE TABLE t (id int PRIMARY KEY, v int)");
    ok(&mut s, a, "INSERT INTO t VALUES (1, 10), (2, 20)");
    (s, a, b)
}

#[test]
fn reads_are_repeatable_and_phantom_free() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT sum(v) FROM t"), "30");
    ok(&mut s, b, "UPDATE t SET v = 11 WHERE id = 1");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    ok(&mut s, b, "DELETE FROM t WHERE id = 2");
    assert_eq!(one(&mut s, a, "SELECT sum(v) FROM t"), "30");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    ok(&mut s, a, "COMMIT");
    assert_eq!(one(&mut s, a, "SELECT sum(v) FROM t"), "41");
    s.abandon();
}

#[test]
fn rc_sees_committed_changes_between_statements() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, a, "BEGIN");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "3");
    ok(&mut s, a, "COMMIT");
    s.abandon();
}

#[test]
fn concurrent_update_and_delete_are_40001() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT v FROM t WHERE id = 1"), "10");
    ok(&mut s, b, "UPDATE t SET v = 11 WHERE id = 1");
    ok(&mut s, b, "DELETE FROM t WHERE id = 2");
    assert_eq!(
        err_code(&mut s, a, "UPDATE t SET v = v + 1 WHERE id = 1"),
        "40001"
    );
    ok(&mut s, a, "ROLLBACK");
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "1");
    ok(&mut s, b, "UPDATE t SET v = 12 WHERE id = 1");
    assert_eq!(err_code(&mut s, a, "DELETE FROM t WHERE id = 1"), "40001");
    assert_eq!(err_code(&mut s, a, "SELECT * FROM t FOR UPDATE"), "25P02");
    ok(&mut s, a, "ROLLBACK");
    s.abandon();
}

#[test]
fn select_for_update_on_a_changed_row_is_40001() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    ok(&mut s, b, "UPDATE t SET v = 11 WHERE id = 1");
    assert_eq!(
        err_code(&mut s, a, "SELECT * FROM t WHERE id = 1 FOR UPDATE"),
        "40001"
    );
    ok(&mut s, a, "ROLLBACK");
    s.abandon();
}

#[test]
fn waiting_then_abort_continues_and_commit_is_40001() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    // 相手が abort したら続行する。
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT v FROM t WHERE id = 1"), "10");
    ok(&mut s, b, "BEGIN");
    ok(&mut s, b, "UPDATE t SET v = 99 WHERE id = 1");
    blocks(&mut s, a, "UPDATE t SET v = v + 1 WHERE id = 1");
    ok(&mut s, b, "ROLLBACK");
    assert!(settle_one(&mut s, a).is_ok());
    assert_eq!(one(&mut s, a, "SELECT v FROM t WHERE id = 1"), "11");
    ok(&mut s, a, "COMMIT");
    // 相手が commit したら 40001。
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT v FROM t WHERE id = 1"), "11");
    ok(&mut s, b, "BEGIN");
    ok(&mut s, b, "UPDATE t SET v = 99 WHERE id = 1");
    blocks(&mut s, a, "UPDATE t SET v = v + 1 WHERE id = 1");
    ok(&mut s, b, "COMMIT");
    let r = settle_one(&mut s, a);
    assert_eq!(r.unwrap_err().sqlstate.0, "40001");
    ok(&mut s, a, "ROLLBACK");
    s.abandon();
}

#[test]
fn first_snapshot_is_taken_before_the_table_lock_wait() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, b, "BEGIN");
    ok(&mut s, b, "LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    blocks(&mut s, a, "SELECT count(*) FROM t");
    ok(&mut s, b, "COMMIT");
    let r = settle_one(&mut s, a).unwrap();
    assert_eq!(r[0][0].as_deref(), Some("2"));
    ok(&mut s, a, "COMMIT");
    // RC は待ちの後のスナップショットなので見える。
    ok(&mut s, b, "BEGIN");
    ok(&mut s, b, "LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
    ok(&mut s, b, "INSERT INTO t VALUES (4, 40)");
    ok(&mut s, a, "BEGIN");
    blocks(&mut s, a, "SELECT count(*) FROM t");
    ok(&mut s, b, "COMMIT");
    let r = settle_one(&mut s, a).unwrap();
    assert_eq!(r[0][0].as_deref(), Some("4"));
    ok(&mut s, a, "COMMIT");
    s.abandon();
}

#[test]
fn lock_table_first_defers_the_snapshot() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, b, "BEGIN");
    ok(&mut s, b, "LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    blocks(&mut s, a, "LOCK TABLE t IN ACCESS SHARE MODE");
    ok(&mut s, b, "COMMIT");
    assert!(settle_one(&mut s, a).is_ok());
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "3");
    ok(&mut s, a, "COMMIT");
    s.abandon();
}

#[test]
fn set_transaction_and_snapshot_rules() {
    let tc = TestCluster::new();
    let (mut s, a, _b) = setup(&tc);
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "SHOW transaction_isolation");
    ok(&mut s, a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        one(&mut s, a, "SHOW transaction_isolation"),
        "repeatable read"
    );
    ok(&mut s, a, "SELECT 1");
    // 同じレベルは成功、違うレベルは 25001。
    ok(&mut s, a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        err_code(&mut s, a, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"),
        "25001"
    );
    ok(&mut s, a, "ROLLBACK");
    // SERIALIZABLE は 0A000。
    assert_eq!(
        err_code(&mut s, a, "BEGIN ISOLATION LEVEL SERIALIZABLE"),
        "0A000"
    );
    ok(&mut s, a, "BEGIN");
    assert_eq!(
        err_code(&mut s, a, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
        "0A000"
    );
    ok(&mut s, a, "ROLLBACK");
    // SHOW / SET / LOCK TABLE はスナップショットを取らない。
    ok(&mut s, a, "BEGIN");
    ok(&mut s, a, "SET application_name = 'x'");
    ok(&mut s, a, "LOCK TABLE t");
    ok(&mut s, a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    ok(&mut s, a, "ROLLBACK");
    s.abandon();
}

#[test]
fn default_transaction_isolation_and_chain() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(
        &mut s,
        a,
        "SET default_transaction_isolation = 'repeatable read'",
    );
    assert_eq!(
        one(&mut s, a, "SHOW default_transaction_isolation"),
        "repeatable read"
    );
    ok(&mut s, a, "BEGIN");
    assert_eq!(
        one(&mut s, a, "SHOW transaction_isolation"),
        "repeatable read"
    );
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    // COMMIT AND CHAIN は同じレベルで、スナップショットは取り直す。
    ok(&mut s, a, "COMMIT AND CHAIN");
    assert_eq!(
        one(&mut s, a, "SHOW transaction_isolation"),
        "repeatable read"
    );
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "3");
    ok(&mut s, a, "COMMIT");
    // 暗黙のトランザクションでも RR で、並行更新に当たれば 40001 になる。
    assert_eq!(
        err_code(
            &mut s,
            a,
            "SET default_transaction_isolation = 'serializable'"
        ),
        "0A000"
    );
    s.abandon();
}

#[test]
fn rr_inserts_and_reads_do_not_fail() {
    let tc = TestCluster::new();
    let (mut s, a, b) = setup(&tc);
    ok(&mut s, a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "2");
    ok(&mut s, b, "INSERT INTO t VALUES (3, 30)");
    ok(&mut s, a, "INSERT INTO t VALUES (4, 40)");
    assert_eq!(one(&mut s, a, "SELECT count(*) FROM t"), "3");
    ok(&mut s, a, "COMMIT");
    s.abandon();
}
