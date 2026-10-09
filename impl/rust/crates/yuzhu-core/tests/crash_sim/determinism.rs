//! 決定性の試験（`m5/10` §5.3）。同じシードのワークロードを 3 回流し、`SimVfs` の操作列
//! （種別・パス・オフセット・長さ）のハッシュが一致することを確かめる。
//!
//! これが壊れるとクラッシュ点 N を再現できないので、CI ではクラッシュ試験より先に走らせる
//! （`cargo test --test crash_sim determinism`）。

use yuzhu_core::testing::actors::{OpLog, OpRecord, Scheduler, StepOutcome};
use yuzhu_core::testing::{TestCluster, TestClusterOptions, run_sql};

const DB: &str = "postgres";

/// 1 アクターのワークロード。ハッシュ・操作数・操作列を返す。
fn single_actor(seed: u64) -> (u64, usize, Vec<OpRecord>) {
    let (tc, log) = TestCluster::with_recorder(TestClusterOptions::crash_sim(seed)).unwrap();
    let mut s = tc.session(DB).unwrap();
    for sql in [
        "CREATE TABLE d (id int PRIMARY KEY, v text)",
        "INSERT INTO d SELECT g, repeat('x', 100) FROM generate_series(1, 300) g",
        "BEGIN",
        "UPDATE d SET v = 'y' WHERE id % 3 = 0",
        "DELETE FROM d WHERE id % 7 = 0",
        "COMMIT",
        "CHECKPOINT",
        "SELECT count(*) FROM d",
    ] {
        let out = run_sql(&mut s, sql);
        assert!(out.is_ok(), "{sql}: {:?}", out.errors);
    }
    drop(s);
    (log.hash(), log.len(), log.ops())
}

/// 3 アクター。アクターの切り替えを `Scheduler` が直列にする。
fn actors(seed: u64) -> (u64, usize, Vec<OpRecord>) {
    let (tc, log) = TestCluster::with_recorder(TestClusterOptions::crash_sim(seed)).unwrap();
    let out = tc
        .sql(DB, "CREATE TABLE d (id int PRIMARY KEY, v int); INSERT INTO d VALUES (1, 0), (2, 0), (3, 0)")
        .unwrap();
    assert!(out.is_ok(), "{:?}", out.errors);
    let mut s = Scheduler::new(&tc);
    let ids: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|n| s.spawn(n, DB).unwrap())
        .collect();
    for round in 0..5 {
        for (i, &id) in ids.iter().enumerate() {
            let sql = format!("UPDATE d SET v = v + {} WHERE id = {}", round + 1, i + 1);
            match s.submit(id, &sql).unwrap() {
                StepOutcome::Done(r) => {
                    r.unwrap();
                }
                StepOutcome::Blocked => panic!("{sql} blocked"),
            }
            s.settle().unwrap();
        }
    }
    s.abandon();
    (log.hash(), log.len(), log.ops())
}

fn assert_same(label: &str, run: impl Fn(u64) -> (u64, usize, Vec<OpRecord>)) {
    let first = run(11);
    assert!(first.1 > 50, "{label}: too few operations: {}", first.1);
    for i in 1..3 {
        let next = run(11);
        assert_eq!(
            first.1, next.1,
            "{label} run {i}: the number of operations differs"
        );
        if first.0 != next.0 {
            let at = OpLog::first_difference(&first.2, &next.2);
            panic!(
                "{label} run {i}: the VFS operation hash differs at op {at:?}: {:?} vs {:?}",
                at.and_then(|k| first.2.get(k)),
                at.and_then(|k| next.2.get(k))
            );
        }
    }
}

#[test]
fn determinism_single_actor_workload() {
    assert_same("single actor", single_actor);
}

#[test]
fn determinism_scheduled_actors() {
    assert_same("three actors", actors);
}
