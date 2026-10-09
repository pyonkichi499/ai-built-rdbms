//! S0: INSERT と `SELECT ... FOR UPDATE` の並行（行ロックと XID ロックだけで書ける土台の確認）。
//!
//! `st_s0(id PK, n)` の行を `FOR UPDATE` で握って `n` を 1 増やし、同じトランザクションで
//! `st_s0_log` に 1 行足す。ほかに `st_s0_ins` へ素の INSERT を行う。
//! 不変条件: (a) 1 文の中で `sum(n) = count(st_s0_log)`、(b) `count(st_s0_ins)` は単調非減少、
//! (c) 終了後に `sum(n)` が成功した増分の数と一致。

use std::sync::atomic::{AtomicI64, Ordering};

use yuzhu_core::testing::stress::{
    InprocTarget, Invariant, IsolationMix, StressConfig, StressConn, StressError, Workload,
    Xoshiro, run_workload,
};
use yuzhu_core::txn::IsolationLevel;

const ROWS: i64 = 8;

struct S0;

fn err_text(e: &StressError) -> String {
    e.to_string()
}

impl Workload for S0 {
    fn name(&self) -> &'static str {
        "s0"
    }

    fn setup(&self, c: &mut dyn StressConn) -> Result<(), StressError> {
        c.simple(
            "CREATE TABLE st_s0 (id int PRIMARY KEY, n int NOT NULL);\
             CREATE TABLE st_s0_log (tid int NOT NULL, id int NOT NULL);\
             CREATE TABLE st_s0_ins (tid int NOT NULL, seq int NOT NULL)",
        )?;
        for id in 0..ROWS {
            c.simple(&format!("INSERT INTO st_s0 VALUES ({id}, 0)"))?;
        }
        Ok(())
    }

    fn txn(
        &self,
        c: &mut dyn StressConn,
        rng: &mut Xoshiro,
        tid: usize,
        _iso: IsolationLevel,
    ) -> Result<(), StressError> {
        if rng.chance(30) {
            let seq = rng.below(1_000_000);
            c.simple(&format!("INSERT INTO st_s0_ins VALUES ({tid}, {seq})"))?;
            return Ok(());
        }
        let id = rng.range(0, ROWS - 1);
        c.simple(&format!("SELECT n FROM st_s0 WHERE id = {id} FOR UPDATE"))?;
        c.simple(&format!("UPDATE st_s0 SET n = n + 1 WHERE id = {id}"))?;
        c.simple(&format!("INSERT INTO st_s0_log VALUES ({tid}, {id})"))?;
        Ok(())
    }

    fn invariants(&self) -> Vec<Box<dyn Invariant>> {
        vec![
            Box::new(SumMatchesLog),
            Box::new(InsMonotonic(AtomicI64::new(0))),
        ]
    }
}

struct SumMatchesLog;

impl Invariant for SumMatchesLog {
    fn name(&self) -> &'static str {
        "s0-sum-equals-log"
    }

    fn check_live(&self, c: &mut dyn StressConn) -> Result<(), String> {
        let r = c
            .simple("SELECT (SELECT sum(n) FROM st_s0), (SELECT count(*) FROM st_s0_log)")
            .map_err(|e| err_text(&e))?;
        let row = r.last().and_then(|q| q.rows.first()).ok_or("no row")?;
        if row[0] == row[1] {
            Ok(())
        } else {
            Err(format!("sum(n)={:?} count(log)={:?}", row[0], row[1]))
        }
    }

    fn check_final(&self, c: &mut dyn StressConn) -> Result<(), String> {
        self.check_live(c)
    }
}

struct InsMonotonic(AtomicI64);

impl Invariant for InsMonotonic {
    fn name(&self) -> &'static str {
        "s0-insert-count-monotonic"
    }

    fn check_live(&self, c: &mut dyn StressConn) -> Result<(), String> {
        let n = c
            .simple("SELECT count(*) FROM st_s0_ins")
            .map_err(|e| err_text(&e))?
            .pop()
            .and_then(|q| q.scalar_i64())
            .ok_or("no count")?;
        let prev = self.0.fetch_max(n, Ordering::SeqCst);
        if n >= prev {
            Ok(())
        } else {
            Err(format!("count went from {prev} to {n}"))
        }
    }

    fn check_final(&self, _: &mut dyn StressConn) -> Result<(), String> {
        Ok(())
    }
}

fn run(mix: IsolationMix, allowed: &'static [&'static str]) {
    let target = InprocTarget::new();
    let mut cfg = StressConfig::from_env(mix, allowed);
    // RR では FOR UPDATE が 40001 になる。RC では出てはいけない。
    cfg.threads = cfg.threads.min(8);
    match run_workload(&target, &S0, &cfg) {
        Ok(r) => {
            eprintln!("s0 {mix:?}: {r:?}");
            assert!(r.committed > 0);
        }
        Err(f) => panic!("{f}"),
    }
}

#[test]
fn s0_read_committed() {
    run(IsolationMix::Rc, &[]);
}

#[test]
fn s0_mixed() {
    run(IsolationMix::Mixed, &[]);
}
