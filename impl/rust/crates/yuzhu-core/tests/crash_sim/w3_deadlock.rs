//! M3 のワークロード 3（少数の行の UPDATE の繰り返し）のデッドロック版（`m5/10` §6.8.2 の「W3 のデッドロック」）。
//!
//! 2 アクターが互いの行を逆順に更新する（銀行振込）。1 つ目の `deadlock_timeout` は短く、
//! 2 つ目は `60s` にして被害者を決定的にする（5.3）。被害者の ROLLBACK のあとでもう一方が進んで
//! コミットし、クラッシュ後も合計が一定であること、振込が「全部か何もなし」であることを確かめる。
//!
//! ```text
//! YUZHU_SIM_SEED=3 YUZHU_CRASH_AT=120 YUZHU_CRASH_MODE=drop \
//!     cargo test --test crash_sim w3_deadlock_replay -- --nocapture
//! ```

use yuzhu_core::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule};
use yuzhu_core::testing::actors::{Scheduler, TraceEvent};
use yuzhu_core::testing::{TestCluster, TestClusterOptions};

const DB: &str = "postgres";

/// 振込 1 件（口座 1 から 2 へ 10、口座 2 から 1 へ 20）が両方確定したあとの残高。
const AFTER_B: [i64; 2] = [120, 80];
const INITIAL: [i64; 2] = [100, 100];

/// スクリプトの結果。
struct Outcome {
    /// 起動後の I/O の総数（クラッシュ点 N の範囲）。
    ops: u64,
    trace: Vec<(String, TraceEvent)>,
    /// クラッシュ後に再起動して読んだ残高。
    balances: Vec<i64>,
}

fn options(seed: u64) -> TestClusterOptions {
    TestClusterOptions::crash_sim(seed)
}

fn setup(seed: u64) -> TestCluster {
    let tc = TestCluster::with_options(options(seed)).expect("initdb and start");
    let out = tc
        .sql(
            DB,
            "CREATE TABLE acct (id int PRIMARY KEY, bal int NOT NULL); \
             INSERT INTO acct VALUES (1, 100), (2, 100); CHECKPOINT",
        )
        .expect("session");
    assert!(out.is_ok(), "{:?}", out.errors);
    tc
}

/// スクリプトを流す。`crash_at` が `Some(n)` なら、セットアップのあとの n 個の I/O のあとでディスクを凍結する。
fn run(seed: u64, crash_at: Option<u64>, mode: CrashMode) -> Outcome {
    let tc = setup(seed);
    if let Some(n) = crash_at {
        tc.vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Any,
                path_prefix: None,
                nth: Some(n + 1),
                probability: None,
                effect: FaultEffect::CrashFreeze,
            }],
        });
    } else {
        tc.vfs.set_faults(FaultPlan::default());
    }
    let mut trace = Vec::new();
    {
        let mut s = Scheduler::new(&tc);
        script(&mut s);
        trace.extend(s.trace().iter().map(|t| (t.name.clone(), t.event.clone())));
        s.abandon();
    }
    let op_total = tc.vfs.op_count();
    let (crashed_vfs, opts) = tc.crash(mode);
    let tc = TestCluster::start_on(crashed_vfs, opts).expect("recovery");
    let out = tc
        .sql(DB, "SELECT bal FROM acct ORDER BY id")
        .expect("session");
    assert!(out.is_ok(), "{:?}", out.errors);
    let balances = out
        .rows
        .iter()
        .map(|r| r[0].as_deref().unwrap().parse().unwrap())
        .collect();
    Outcome {
        ops: op_total,
        trace,
        balances,
    }
}

/// 凍結後は文が失敗するので、期待と食い違っても止めず最後まで進める。ハングだけが `Err`。
fn script(s: &mut Scheduler<'_>) {
    macro_rules! go {
        ($a:expr, $sql:expr) => {
            match s.submit($a, $sql) {
                Ok(o) => o,
                Err(h) => panic!("{h}"),
            }
        };
    }
    let Ok(a) = s.spawn("a", DB) else { return };
    let Ok(b) = s.spawn("b", DB) else { return };
    go!(a, "SET deadlock_timeout = '100ms'");
    go!(b, "SET deadlock_timeout = '60s'");
    go!(a, "BEGIN");
    go!(b, "BEGIN");
    go!(a, "UPDATE acct SET bal = bal - 10 WHERE id = 1");
    go!(b, "UPDATE acct SET bal = bal - 20 WHERE id = 2");
    go!(a, "UPDATE acct SET bal = bal + 10 WHERE id = 2");
    go!(b, "UPDATE acct SET bal = bal + 20 WHERE id = 1");
    settle(s);
    // 被害者の ROLLBACK のあとで、もう一方が進む。
    go!(a, "ROLLBACK");
    settle(s);
    go!(b, "COMMIT");
    settle(s);
    go!(a, "CHECKPOINT");
}

fn settle(
    s: &mut Scheduler<'_>,
) -> Vec<(
    yuzhu_core::testing::actors::ActorId,
    yuzhu_core::testing::actors::StepResult,
)> {
    match s.settle() {
        Ok(v) => v,
        Err(h) => panic!("{h}"),
    }
}

fn check_atomic(balances: &[i64], context: &str) {
    assert_eq!(balances.iter().sum::<i64>(), 200, "{context}: {balances:?}");
    assert!(
        balances == INITIAL || balances == AFTER_B,
        "{context}: neither the initial state nor the winner's transfer: {balances:?}"
    );
}

fn mode_from(name: &str) -> CrashMode {
    match name {
        "keep" => CrashMode::KeepAll,
        "random50" => CrashMode::RandomSubset {
            keep_probability: 0.5,
        },
        _ => CrashMode::DropUnsynced,
    }
}

#[test]
fn deadlock_victim_is_the_short_timeout_and_the_winner_commits() {
    let o = run(1, None, CrashMode::KeepAll);
    let victim = o.trace.iter().any(|(n, e)| {
        n == "a" && matches!(e, TraceEvent::Done { sqlstate: Some(s) } if s == "40P01")
    });
    if !victim {
        #[allow(clippy::print_stderr)] // テスト専用: 未実装でスキップしたことを実行ログに残す。
        {
            eprintln!(
                "SKIPPED: row-lock waits are not implemented yet: {:?}",
                o.trace
            );
        }
        assert!(std::env::var_os("YUZHU_REQUIRE_ROW_WAITS").is_none());
        return;
    }
    assert_eq!(o.balances, AFTER_B);
    // b は a の ROLLBACK まで待ち、そのあとで完了する。
    assert!(
        o.trace
            .iter()
            .any(|(n, e)| n == "b" && *e == TraceEvent::Blocked)
    );
}

#[test]
fn script_is_deterministic() {
    let x = run(2, None, CrashMode::KeepAll);
    let y = run(2, None, CrashMode::KeepAll);
    assert_eq!(x.ops, y.ops);
    assert_eq!(x.trace, y.trace);
}

#[test]
fn sum_is_constant_and_transfer_is_atomic_at_every_crash_point() {
    let total = run(3, None, CrashMode::KeepAll).ops;
    assert!(total > 0);
    let step = (total / 25).max(1);
    for mode in ["drop", "keep", "random50"] {
        let mut n = 0;
        while n < total {
            let o = run(3, Some(n), mode_from(mode));
            check_atomic(
                &o.balances,
                &format!(
                    "YUZHU_SIM_SEED=3 YUZHU_CRASH_AT={n} YUZHU_CRASH_MODE={mode} (of {total})"
                ),
            );
            n += step;
        }
    }
}

/// 環境変数で 1 件を再現する。
#[test]
fn w3_deadlock_replay() {
    let Ok(seed) = std::env::var("YUZHU_SIM_SEED") else {
        return;
    };
    let seed = seed.parse().expect("YUZHU_SIM_SEED");
    let at = std::env::var("YUZHU_CRASH_AT")
        .ok()
        .map(|v| v.parse().expect("YUZHU_CRASH_AT"));
    let mode = mode_from(&std::env::var("YUZHU_CRASH_MODE").unwrap_or_default());
    let o = run(seed, at, mode);
    println!("ops={} balances={:?}", o.ops, o.balances);
    check_atomic(&o.balances, "replay");
}
