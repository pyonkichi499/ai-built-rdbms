//! 並行ストレスの土台（`m5/10` §4.1・§4.2・§5.4・§6.7。TS-2）。
//!
//! ワークロード（S1〜S7）は対象に依存しない [`StressTarget`] の上に書く。プロセス内の
//! [`InprocTarget`] と、wire（yuzhu-server、PostgreSQL 17）に同じものを向けられる。
//!
//! 失敗すると `target/stress-failure-<seed>.log` に、違反した不変条件、観測値、シード、
//! スレッドごとの直近 256 操作、（プロセス内なら）`lock_status()` を書く。再現:
//!
//! ```text
//! YUZHU_STRESS_SEED=<seed> YUZHU_STRESS_THREADS=<n> cargo test -p yuzhu-core --test stress <name>
//! ```
//!
//! 同じシードでもスレッドの順序は再現しない（再現するのは各スレッドの操作列だけ）。

mod checker;
pub mod inproc;
pub mod ring;
pub mod rng;
pub mod target;
mod watchdog;
pub mod workloads;

use std::collections::BTreeMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub use self::inproc::InprocTarget;
pub use self::ring::{OpRecord, OpRing};
pub use self::rng::Xoshiro;
pub use self::target::{
    ControlSnapshot, QueryResult, StressConn, StressError, StressInternals, StressTarget,
};
use crate::txn::IsolationLevel;

/// ワークロードが接続するデータベースとユーザー。
pub const STRESS_DATABASE: &str = "postgres";
pub const STRESS_USER: &str = "postgres";
/// 同じトランザクションのやり直しの上限（超えたら失敗）。
pub const MAX_RETRIES: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationMix {
    Rc,
    Rr,
    /// スレッド番号の偶数が RC、奇数が RR。
    Mixed,
}

#[derive(Clone, Debug)]
pub struct StressConfig {
    /// `YUZHU_STRESS_SEED`。省略時は時刻から決めて表示する。
    pub seed: u64,
    /// `YUZHU_STRESS_THREADS`
    pub threads: usize,
    /// `YUZHU_STRESS_SECS`（PR は 5 秒、夜間は 600 秒）
    pub duration: Duration,
    pub isolation: IsolationMix,
    /// この時間、1 件もコミットがなければ失敗（既定 20 秒）。
    pub watchdog: Duration,
    /// 許容する SQLSTATE（リトライ対象に加えて、そのワークロードで正常なもの）。
    pub allowed: &'static [&'static str],
}

impl StressConfig {
    /// 環境変数と既定値（シード = 時刻、スレッド 8、5 秒、ウォッチドッグ 20 秒）から作る。
    pub fn from_env(isolation: IsolationMix, allowed: &'static [&'static str]) -> Self {
        let seed = env_u64("YUZHU_STRESS_SEED").unwrap_or_else(|| {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| {
                    u64::try_from(d.as_nanos() & 0xFFFF_FFFF).unwrap_or(1)
                });
            say(&format!(
                "stress: YUZHU_STRESS_SEED={t} (chosen from the clock)"
            ));
            t
        });
        StressConfig {
            seed,
            threads: env_u64("YUZHU_STRESS_THREADS")
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(8)
                .max(1),
            duration: Duration::from_secs(env_u64("YUZHU_STRESS_SECS").unwrap_or(5)),
            isolation,
            watchdog: Duration::from_secs(20),
            allowed,
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// ワークロードの結果。0 件のリトライは「競合が起きていない = 試験として空」の疑いがあるので、
/// 各ワークロードが下限を主張する。
#[derive(Clone, Debug, Default)]
pub struct StressReport {
    pub committed: u64,
    pub retried_40001: u64,
    pub retried_40p01: u64,
    /// SQLSTATE ごとの、許容したエラーの数。
    pub allowed_errors: BTreeMap<String, u64>,
    pub elapsed: Duration,
    /// `check_live` を全部回した回数。
    pub live_checks: u64,
}

pub trait Invariant: Send + Sync {
    fn name(&self) -> &'static str;
    /// 実行中に別スレッドから繰り返し呼ぶ（RC の 1 文のスナップショットで必ず成り立つもの）。
    ///
    /// # Errors
    ///
    /// 観測値の説明（不変条件の違反）。
    fn check_live(&self, c: &mut dyn StressConn) -> Result<(), String>;
    /// 終了後に 1 回。
    ///
    /// # Errors
    ///
    /// 観測値の説明（不変条件の違反）。
    fn check_final(&self, c: &mut dyn StressConn) -> Result<(), String>;
}

pub trait Workload: Sync {
    fn name(&self) -> &'static str;
    /// # Errors
    ///
    /// 準備に失敗した。
    fn setup(&self, c: &mut dyn StressConn) -> Result<(), StressError>;
    /// 1 スレッドの 1 トランザクション分（`BEGIN` と `COMMIT` は `run_workload` が発行する）。
    /// リトライはしない（`run_workload` が `is_retryable` で繰り返す）。
    ///
    /// # Errors
    ///
    /// 文が失敗した。
    fn txn(
        &self,
        c: &mut dyn StressConn,
        rng: &mut Xoshiro,
        tid: usize,
        iso: IsolationLevel,
    ) -> Result<(), StressError>;
    fn invariants(&self) -> Vec<Box<dyn Invariant>>;
    /// 期待する最小のリトライ数（RR のホットスポットなら 1 以上。0 なら主張しない）。
    fn expect_min_retries(&self, _iso: IsolationLevel) -> u64 {
        0
    }
    /// RC で 40001 が出てはいけないか（既定 true）。
    fn rc_forbids_40001(&self) -> bool {
        true
    }
}

/// 失敗の記録。
#[derive(Clone, Debug)]
pub struct StressFailure {
    pub workload: String,
    pub target: &'static str,
    pub seed: u64,
    pub threads: usize,
    /// 違反した不変条件（または `watchdog`、`worker`、`assertion` など）の名前。
    pub invariant: String,
    pub observed: String,
    /// スレッドごとの直近の操作（添字 = スレッド番号。最後の 2 つはチェッカーと管理接続）。
    pub rings: Vec<Vec<OpRecord>>,
    /// 失敗した時点の `lock_status()`（プロセス内のみ）。
    pub lock_status: Vec<String>,
    pub log_path: Option<PathBuf>,
}

impl StressFailure {
    /// 失敗ログの全文。
    pub fn render(&self) -> String {
        use fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "stress failure: workload={} target={} seed={} threads={}",
            self.workload, self.target, self.seed, self.threads
        );
        let _ = writeln!(s, "invariant: {}", self.invariant);
        let _ = writeln!(s, "observed: {}", self.observed);
        let _ = writeln!(
            s,
            "reproduce: YUZHU_STRESS_SEED={} YUZHU_STRESS_THREADS={} cargo test -p yuzhu-core --test stress {}",
            self.seed, self.threads, self.workload
        );
        let _ = writeln!(s, "\n== lock_status ({} rows) ==", self.lock_status.len());
        for row in &self.lock_status {
            let _ = writeln!(s, "{row}");
        }
        for (i, ring) in self.rings.iter().enumerate() {
            let _ = writeln!(s, "\n== thread {i} (last {} ops) ==", ring.len());
            for r in ring {
                let _ = writeln!(s, "[{:>7}ms] {} => {}", r.t_ms, r.sql, r.outcome);
            }
        }
        s
    }

    fn log_file_path(seed: u64) -> PathBuf {
        let dir = std::env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| PathBuf::from("target"), PathBuf::from);
        dir.join(format!("stress-failure-{seed}.log"))
    }

    fn write_log(&mut self) {
        let path = Self::log_file_path(self.seed);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, self.render()) {
            Ok(()) => self.log_path = Some(path),
            Err(e) => say(&format!("stress: cannot write {}: {e}", path.display())),
        }
    }
}

impl fmt::Display for StressFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stress {} ({}) failed: [{}] {} (seed={}, threads={}{})",
            self.workload,
            self.target,
            self.invariant,
            self.observed,
            self.seed,
            self.threads,
            self.log_path
                .as_ref()
                .map_or_else(String::new, |p| format!(", log={}", p.display()))
        )
    }
}

impl std::error::Error for StressFailure {}

// ----- 実行中の共有状態 -----------------------------------------------------------

struct FirstFailure {
    invariant: String,
    observed: String,
    lock_status: Vec<String>,
}

pub(crate) struct Shared<'a> {
    stop: AtomicBool,
    /// ワーカーが全員戻った。チェッカーとウォッチドッグが終わる合図。
    done: AtomicBool,
    committed: AtomicU64,
    retried_40001: AtomicU64,
    retried_40p01: AtomicU64,
    checks: AtomicU64,
    allowed: Mutex<BTreeMap<String, u64>>,
    failure: Mutex<Option<FirstFailure>>,
    internals: Option<&'a dyn StressInternals>,
}

impl<'a> Shared<'a> {
    fn new(internals: Option<&'a dyn StressInternals>) -> Self {
        Shared {
            stop: AtomicBool::new(false),
            done: AtomicBool::new(false),
            committed: AtomicU64::new(0),
            retried_40001: AtomicU64::new(0),
            retried_40p01: AtomicU64::new(0),
            checks: AtomicU64::new(0),
            allowed: Mutex::new(BTreeMap::new()),
            failure: Mutex::new(None),
            internals,
        }
    }

    /// 最初の失敗だけを覚え、全スレッドに止まるよう伝える。
    fn fail(&self, invariant: &str, observed: String) {
        let mut slot = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            let lock_status = self
                .internals
                .map(|i| i.lock_status().iter().map(|r| format!("{r:?}")).collect())
                .unwrap_or_default();
            *slot = Some(FirstFailure {
                invariant: invariant.to_owned(),
                observed,
                lock_status,
            });
        }
        drop(slot);
        self.stop.store(true, Ordering::SeqCst);
    }

    fn has_failed(&self) -> bool {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// 接続を包んで、すべての操作を `OpRing` に記録する。
struct RecordingConn {
    inner: Box<dyn StressConn>,
    ring: Arc<Mutex<OpRing>>,
    thread: u16,
    start: Instant,
}

impl RecordingConn {
    fn record<T>(&self, sql: &str, r: &Result<T, StressError>, ok: impl Fn(&T) -> String) {
        let outcome = match r {
            Ok(v) => ok(v),
            Err(e) => format!("ERROR {e}"),
        };
        let rec = OpRecord {
            t_ms: u32::try_from(self.start.elapsed().as_millis()).unwrap_or(u32::MAX),
            thread: self.thread,
            sql: sql.to_owned(),
            outcome,
        };
        self.ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(rec);
    }
}

fn describe(r: &QueryResult) -> String {
    format!("ok rows={} affected={}", r.rows.len(), r.affected)
}

fn with_params(sql: &str, params: &[Option<&str>]) -> String {
    if params.is_empty() {
        sql.to_owned()
    } else {
        format!("{sql} -- {params:?}")
    }
}

impl StressConn for RecordingConn {
    fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError> {
        let r = self.inner.simple(sql);
        self.record(sql, &r, |v| format!("ok statements={}", v.len()));
        r
    }
    fn extended(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError> {
        let r = self.inner.extended(sql, params);
        self.record(&with_params(sql, params), &r, describe);
        r
    }
    fn prepared(&mut self, name: &str, sql: &str) -> Result<(), StressError> {
        let r = self.inner.prepared(name, sql);
        self.record(&format!("PREPARE {name} AS {sql}"), &r, |()| "ok".into());
        r
    }
    fn execute_prepared(
        &mut self,
        name: &str,
        params: &[Option<&str>],
    ) -> Result<QueryResult, StressError> {
        let r = self.inner.execute_prepared(name, params);
        self.record(
            &with_params(&format!("EXECUTE {name}"), params),
            &r,
            describe,
        );
        r
    }
    fn backend_pid(&self) -> i32 {
        self.inner.backend_pid()
    }
    fn cancel(&self) -> Result<(), StressError> {
        self.inner.cancel()
    }
}

/// stderr に 1 行出す。ストレステスト専用（失敗ログの場所とシードを実行ログに残す）。
#[allow(clippy::print_stderr)]
pub(super) fn say(msg: &str) {
    eprintln!("{msg}");
}

fn isolation_for(mix: IsolationMix, tid: usize) -> IsolationLevel {
    match mix {
        IsolationMix::Rc => IsolationLevel::ReadCommitted,
        IsolationMix::Rr => IsolationLevel::RepeatableRead,
        IsolationMix::Mixed => {
            if tid.is_multiple_of(2) {
                IsolationLevel::ReadCommitted
            } else {
                IsolationLevel::RepeatableRead
            }
        }
    }
}

fn begin_sql(iso: IsolationLevel) -> &'static str {
    match iso {
        IsolationLevel::RepeatableRead => "BEGIN ISOLATION LEVEL REPEATABLE READ",
        IsolationLevel::ReadUncommitted => "BEGIN ISOLATION LEVEL READ UNCOMMITTED",
        IsolationLevel::ReadCommitted => "BEGIN ISOLATION LEVEL READ COMMITTED",
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic".into())
}

fn lock_ring(ring: &Arc<Mutex<OpRing>>) -> Vec<OpRecord> {
    ring.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .records()
}

/// 1 スレッド分のループ。
fn worker(
    shared: &Shared<'_>,
    w: &dyn Workload,
    cfg: &StressConfig,
    conn: &mut dyn StressConn,
    tid: usize,
) {
    let iso = isolation_for(cfg.isolation, tid);
    let mut rng = Xoshiro::for_thread(cfg.seed, tid);
    while !shared.stop.load(Ordering::SeqCst) {
        let saved = rng.clone();
        let mut attempts = 0u32;
        loop {
            rng = saved.clone();
            let r = conn
                .simple(begin_sql(iso))
                .and_then(|_| w.txn(conn, &mut rng, tid, iso))
                .and_then(|()| conn.simple("COMMIT"));
            let Err(e) = r else {
                shared.committed.fetch_add(1, Ordering::SeqCst);
                break;
            };
            let _ = conn.simple("ROLLBACK");
            if e.is_internal() {
                shared.fail("internal-error", format!("thread {tid}: {e}"));
                return;
            }
            if e.is_retryable() {
                if e.sqlstate == "40001" {
                    if iso == IsolationLevel::ReadCommitted && w.rc_forbids_40001() {
                        shared.fail("rc-no-40001", format!("thread {tid} (RC): {e}"));
                        return;
                    }
                    shared.retried_40001.fetch_add(1, Ordering::SeqCst);
                } else {
                    shared.retried_40p01.fetch_add(1, Ordering::SeqCst);
                }
                attempts += 1;
                if attempts > MAX_RETRIES {
                    shared.fail(
                        "retry-limit",
                        format!("thread {tid}: more than {MAX_RETRIES} retries of one transaction; last: {e}"),
                    );
                    return;
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_micros(100 * u64::from(attempts.min(10))));
                continue;
            }
            if cfg.allowed.contains(&e.sqlstate.as_str()) {
                *shared
                    .allowed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(e.sqlstate.clone())
                    .or_insert(0) += 1;
                break;
            }
            shared.fail(
                "unexpected-error",
                format!("thread {tid}: SQLSTATE {} is not allowed: {e}", e.sqlstate),
            );
            return;
        }
    }
}

fn connect_recording(
    target: &dyn StressTarget,
    ring: &Arc<Mutex<OpRing>>,
    thread: u16,
    start: Instant,
) -> Result<RecordingConn, StressError> {
    Ok(RecordingConn {
        inner: target.connect(STRESS_DATABASE, STRESS_USER)?,
        ring: Arc::clone(ring),
        thread,
        start,
    })
}

/// 10 章 §5.4 の流れでワークロードを走らせる。
///
/// # Errors
///
/// 不変条件の違反、許容しない SQLSTATE、ウォッチドッグ、主張の不成立（失敗ログも書く）。
#[allow(clippy::too_many_lines)] // 10 章 §5.4 の手順（番号付き）を 1 関数で追えるようにしている。
#[allow(clippy::result_large_err)] // StressFailure は失敗ログの材料を全部持つ。失敗時にしか作らない。
pub fn run_workload(
    target: &dyn StressTarget,
    w: &dyn Workload,
    cfg: &StressConfig,
) -> Result<StressReport, StressFailure> {
    let start = Instant::now();
    let nrings = cfg.threads + 2;
    let rings: Vec<Arc<Mutex<OpRing>>> = (0..nrings)
        .map(|_| Arc::new(Mutex::new(OpRing::new())))
        .collect();
    let shared = Shared::new(target.internals());

    let make_failure =
        |shared: &Shared<'_>, invariant: String, observed: String, lock: Vec<String>| {
            let _ = shared;
            let mut f = StressFailure {
                workload: w.name().to_owned(),
                target: target.label(),
                seed: cfg.seed,
                threads: cfg.threads,
                invariant,
                observed,
                rings: rings.iter().map(lock_ring).collect(),
                lock_status: lock,
                log_path: None,
            };
            f.write_log();
            say(&f.to_string());
            f
        };
    let fail_now = |invariant: &str, observed: String| {
        let lock = shared
            .internals
            .map(|i| i.lock_status().iter().map(|r| format!("{r:?}")).collect())
            .unwrap_or_default();
        make_failure(&shared, invariant.to_owned(), observed, lock)
    };

    // 1. 管理接続で setup。
    let admin_ring = &rings[cfg.threads + 1];
    let mut admin = connect_recording(target, admin_ring, u16::MAX, start)
        .map_err(|e| fail_now("connect", format!("admin connection: {e}")))?;
    w.setup(&mut admin)
        .map_err(|e| fail_now("setup", e.to_string()))?;
    let invariants = w.invariants();

    // スレッドごとの接続（接続の失敗はここで止める）。
    let mut conns = Vec::with_capacity(cfg.threads);
    for (tid, ring) in rings.iter().enumerate().take(cfg.threads) {
        let c = connect_recording(target, ring, u16::try_from(tid).unwrap_or(u16::MAX), start)
            .map_err(|e| fail_now("connect", format!("thread {tid}: {e}")))?;
        conns.push(c);
    }
    let mut checker_conn = connect_recording(
        target,
        &rings[cfg.threads],
        u16::try_from(cfg.threads).unwrap_or(u16::MAX),
        start,
    )
    .map_err(|e| fail_now("connect", format!("checker: {e}")))?;

    // 2〜5. ワーカー、チェッカー、ウォッチドッグ。
    let run_start = Instant::now();
    std::thread::scope(|scope| {
        let shared = &shared;
        let rings_for_hang = &rings;
        let invs = &invariants;
        scope.spawn(move || {
            watchdog::run(shared, cfg.watchdog, &|| {
                // 戻らないワーカーがいるまま中断する前に、ログを残す。
                let (inv, obs, lock) = shared
                    .failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map_or_else(
                        || {
                            (
                                "watchdog".to_owned(),
                                "workers did not stop".to_owned(),
                                Vec::new(),
                            )
                        },
                        |f| {
                            (
                                f.invariant.clone(),
                                f.observed.clone(),
                                f.lock_status.clone(),
                            )
                        },
                    );
                let mut f = StressFailure {
                    workload: w.name().to_owned(),
                    target: target.label(),
                    seed: cfg.seed,
                    threads: cfg.threads,
                    invariant: inv,
                    observed: format!("{obs} (workers did not stop; process aborted)"),
                    rings: rings_for_hang.iter().map(lock_ring).collect(),
                    lock_status: lock,
                    log_path: None,
                };
                f.write_log();
            });
        });
        scope.spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| {
                checker::run(shared, invs, &mut checker_conn);
            }));
            if let Err(p) = r {
                shared.fail("checker-panic", panic_message(&*p));
            }
        });
        let handles: Vec<_> = conns
            .into_iter()
            .enumerate()
            .map(|(tid, mut conn)| {
                scope.spawn(move || {
                    let r =
                        catch_unwind(AssertUnwindSafe(|| worker(shared, w, cfg, &mut conn, tid)));
                    if let Err(p) = r {
                        shared.fail(
                            "worker-panic",
                            format!("thread {tid}: {}", panic_message(&*p)),
                        );
                    }
                })
            })
            .collect();
        // 時間切れまで待つ（失敗で早く止まる）。
        while run_start.elapsed() < cfg.duration && !shared.stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5).min(cfg.duration));
        }
        shared.stop.store(true, Ordering::SeqCst);
        for h in handles {
            let _ = h.join();
        }
        shared.done.store(true, Ordering::SeqCst);
    });

    let elapsed = run_start.elapsed();
    if shared.has_failed() {
        let f = shared
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("checked above");
        return Err(make_failure(
            &shared,
            f.invariant,
            f.observed,
            f.lock_status,
        ));
    }

    // 5. 終了後の検査。
    for inv in &invariants {
        if let Err(observed) = inv.check_final(&mut admin) {
            return Err(fail_now(inv.name(), format!("(final) {observed}")));
        }
    }

    let report = StressReport {
        committed: shared.committed.load(Ordering::SeqCst),
        retried_40001: shared.retried_40001.load(Ordering::SeqCst),
        retried_40p01: shared.retried_40p01.load(Ordering::SeqCst),
        allowed_errors: shared
            .allowed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
        elapsed,
        live_checks: shared.checks.load(Ordering::SeqCst),
    };

    // 6. 主張。
    if report.committed == 0 {
        return Err(fail_now(
            "assertion",
            "no transaction committed in the whole run".into(),
        ));
    }
    let retries = report.retried_40001 + report.retried_40p01;
    let need = match cfg.isolation {
        IsolationMix::Rc => w.expect_min_retries(IsolationLevel::ReadCommitted),
        IsolationMix::Rr => w.expect_min_retries(IsolationLevel::RepeatableRead),
        IsolationMix::Mixed => w
            .expect_min_retries(IsolationLevel::ReadCommitted)
            .max(w.expect_min_retries(IsolationLevel::RepeatableRead)),
    };
    if retries < need {
        return Err(fail_now(
            "expect-min-retries",
            format!(
                "expected at least {need} retries, saw {retries} (40001={}, 40P01={}); the run may have had no contention",
                report.retried_40001, report.retried_40p01
            ),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    type Reply = Arc<dyn Fn(&str) -> Result<Vec<QueryResult>, StressError> + Send + Sync>;

    /// 何も実行しない対象。`simple` には偽の応答を返す。
    struct Fake {
        reply: Reply,
        connects: AtomicUsize,
    }

    struct FakeConn {
        reply: Reply,
    }

    impl StressConn for FakeConn {
        fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError> {
            (self.reply)(sql)
        }
        fn extended(&mut self, sql: &str, _: &[Option<&str>]) -> Result<QueryResult, StressError> {
            Ok((self.reply)(sql)?.pop().unwrap_or_default())
        }
        fn prepared(&mut self, _: &str, _: &str) -> Result<(), StressError> {
            Ok(())
        }
        fn execute_prepared(
            &mut self,
            name: &str,
            _: &[Option<&str>],
        ) -> Result<QueryResult, StressError> {
            Ok((self.reply)(name)?.pop().unwrap_or_default())
        }
        fn backend_pid(&self) -> i32 {
            1
        }
        fn cancel(&self) -> Result<(), StressError> {
            Ok(())
        }
    }

    impl StressTarget for Fake {
        fn connect(&self, _: &str, _: &str) -> Result<Box<dyn StressConn>, StressError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeConn {
                reply: Arc::clone(&self.reply),
            }))
        }
        fn internals(&self) -> Option<&dyn StressInternals> {
            None
        }
        fn label(&self) -> &'static str {
            "fake"
        }
    }

    fn fake() -> Fake {
        Fake {
            reply: Arc::new(|_| Ok(vec![QueryResult::default()])),
            connects: AtomicUsize::new(0),
        }
    }

    type TxnFn = dyn Fn(&mut dyn StressConn, &mut Xoshiro, usize, IsolationLevel) -> Result<(), StressError>
        + Send
        + Sync;

    /// 与えた関数でふるまうワークロード。
    struct Scripted {
        name: &'static str,
        txn: Box<TxnFn>,
        invs: fn() -> Vec<Box<dyn Invariant>>,
        min_retries: u64,
    }

    impl Workload for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }
        fn setup(&self, c: &mut dyn StressConn) -> Result<(), StressError> {
            c.simple("CREATE TABLE st_fake ()").map(|_| ())
        }
        fn txn(
            &self,
            c: &mut dyn StressConn,
            rng: &mut Xoshiro,
            tid: usize,
            iso: IsolationLevel,
        ) -> Result<(), StressError> {
            (self.txn)(c, rng, tid, iso)
        }
        fn invariants(&self) -> Vec<Box<dyn Invariant>> {
            (self.invs)()
        }
        fn expect_min_retries(&self, _: IsolationLevel) -> u64 {
            self.min_retries
        }
    }

    fn scripted(
        name: &'static str,
        txn: impl Fn(
            &mut dyn StressConn,
            &mut Xoshiro,
            usize,
            IsolationLevel,
        ) -> Result<(), StressError>
        + Send
        + Sync
        + 'static,
    ) -> Scripted {
        Scripted {
            name,
            txn: Box::new(txn),
            invs: Vec::new,
            min_retries: 0,
        }
    }

    fn cfg(seed: u64, isolation: IsolationMix, allowed: &'static [&'static str]) -> StressConfig {
        StressConfig {
            seed,
            threads: 4,
            duration: Duration::from_millis(300),
            isolation,
            watchdog: Duration::from_secs(20),
            allowed,
        }
    }

    fn cleanup(f: &StressFailure) {
        if let Some(p) = &f.log_path {
            let _ = std::fs::remove_file(p);
        }
    }

    fn select_one(c: &mut dyn StressConn) -> Result<(), StressError> {
        c.simple("SELECT 1").map(|_| ())
    }

    #[test]
    fn healthy_workload_passes() {
        let t = fake();
        let w = scripted("healthy", |c, _, _, _| select_one(c));
        let r = run_workload(&t, &w, &cfg(1, IsolationMix::Mixed, &[])).unwrap();
        assert!(r.committed > 0);
        assert_eq!(r.retried_40001 + r.retried_40p01, 0);
        assert!(r.live_checks > 0, "the checker loop runs");
        // ワーカー 4 + チェッカー + 管理接続
        assert_eq!(t.connects.load(Ordering::SeqCst), 6);
    }

    struct Broken(AtomicU64);
    impl Invariant for Broken {
        fn name(&self) -> &'static str {
            "always-three"
        }
        fn check_live(&self, _: &mut dyn StressConn) -> Result<(), String> {
            // 4 回目から違反する。
            if self.0.fetch_add(1, Ordering::SeqCst) >= 3 {
                Err("sum=4 (expected 3)".into())
            } else {
                Ok(())
            }
        }
        fn check_final(&self, _: &mut dyn StressConn) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn broken_invariant_fails_early_and_writes_log() {
        let t = fake();
        let mut w = scripted("broken_live", |c, _, _, _| select_one(c));
        w.invs = || vec![Box::new(Broken(AtomicU64::new(0)))];
        let mut c = cfg(9001, IsolationMix::Rc, &[]);
        c.duration = Duration::from_secs(30);
        let started = Instant::now();
        let f = run_workload(&t, &w, &c).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stops early on failure"
        );
        assert_eq!(f.invariant, "always-three");
        assert!(f.observed.contains("sum=4"));
        assert_eq!(f.seed, 9001);
        let path = f.log_path.clone().expect("log written");
        assert!(path.ends_with("stress-failure-9001.log"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("invariant: always-three"));
        assert!(text.contains("YUZHU_STRESS_SEED=9001"));
        assert!(text.contains("SELECT 1"), "the op ring is in the log");
        cleanup(&f);
    }

    struct BadFinal;
    impl Invariant for BadFinal {
        fn name(&self) -> &'static str {
            "final-check"
        }
        fn check_live(&self, _: &mut dyn StressConn) -> Result<(), String> {
            Ok(())
        }
        fn check_final(&self, _: &mut dyn StressConn) -> Result<(), String> {
            Err("lost update".into())
        }
    }

    #[test]
    fn broken_final_invariant_fails() {
        let t = fake();
        let mut w = scripted("broken_final", |c, _, _, _| select_one(c));
        w.invs = || vec![Box::new(BadFinal)];
        let f = run_workload(&t, &w, &cfg(9002, IsolationMix::Rc, &[])).unwrap_err();
        assert_eq!(f.invariant, "final-check");
        assert!(f.observed.contains("lost update"));
        cleanup(&f);
    }

    #[test]
    fn stalled_workload_trips_the_watchdog() {
        let t = fake();
        // 全スレッドが 3 秒止まる。ウォッチドッグは 300ms。
        let w = scripted("stalled", |_, _, _, _| {
            std::thread::sleep(Duration::from_secs(3));
            Ok(())
        });
        let mut c = cfg(9003, IsolationMix::Rc, &[]);
        c.watchdog = Duration::from_millis(300);
        c.duration = Duration::from_secs(30);
        let f = run_workload(&t, &w, &c).unwrap_err();
        assert_eq!(f.invariant, "watchdog");
        assert!(f.observed.contains("no transaction committed"));
        cleanup(&f);
    }

    #[test]
    fn internal_error_xx000_fails_even_if_listed() {
        let t = fake();
        let w = scripted("internal", |_, _, _, _| {
            Err(StressError::new("XX000", "tuple concurrently updated"))
        });
        let f = run_workload(&t, &w, &cfg(9004, IsolationMix::Rc, &["XX000"])).unwrap_err();
        assert_eq!(f.invariant, "internal-error");
        assert!(f.observed.contains("tuple concurrently updated"));
        cleanup(&f);
    }

    #[test]
    fn listed_sqlstate_is_counted_and_unlisted_fails() {
        let t = fake();
        let w = scripted("unique", |c, rng, _, _| {
            select_one(c)?;
            if rng.chance(50) {
                Err(StressError::new("23505", "duplicate key"))
            } else {
                Ok(())
            }
        });
        let r = run_workload(&t, &w, &cfg(1, IsolationMix::Rc, &["23505"])).unwrap();
        assert!(r.allowed_errors.get("23505").copied().unwrap_or(0) > 0);
        assert!(r.committed > 0);

        let f = run_workload(&t, &w, &cfg(9005, IsolationMix::Rc, &[])).unwrap_err();
        assert_eq!(f.invariant, "unexpected-error");
        assert!(f.observed.contains("23505"));
        cleanup(&f);
    }

    #[test]
    fn retryable_errors_are_retried_and_counted() {
        let t = fake();
        let w = scripted("retry", |c, rng, tid, iso| {
            select_one(c)?;
            if iso == IsolationLevel::RepeatableRead && rng.chance(30) {
                return Err(StressError::new("40001", "could not serialize access"));
            }
            if tid == 0 && rng.chance(20) {
                return Err(StressError::new("40P01", "deadlock detected"));
            }
            Ok(())
        });
        let r = run_workload(&t, &w, &cfg(1, IsolationMix::Mixed, &[])).unwrap();
        assert!(r.retried_40001 > 0 && r.retried_40p01 > 0, "{r:?}");
        assert!(r.committed > 0);
    }

    #[test]
    fn rc_40001_is_a_failure() {
        let t = fake();
        let w = scripted("rc40001", |_, _, _, _| {
            Err(StressError::new("40001", "could not serialize access"))
        });
        let f = run_workload(&t, &w, &cfg(9006, IsolationMix::Rc, &[])).unwrap_err();
        assert_eq!(f.invariant, "rc-no-40001");
        cleanup(&f);
    }

    #[test]
    fn too_many_retries_fail() {
        let t = fake();
        let w = scripted("spin", |_, _, _, _| {
            Err(StressError::new("40P01", "deadlock detected"))
        });
        let mut c = cfg(9007, IsolationMix::Rc, &[]);
        c.duration = Duration::from_secs(60);
        let f = run_workload(&t, &w, &c).unwrap_err();
        assert_eq!(f.invariant, "retry-limit");
        cleanup(&f);
    }

    #[test]
    fn min_retries_assertion() {
        let t = fake();
        let mut w = scripted("no_contention", |c, _, _, _| select_one(c));
        w.min_retries = 1;
        let f = run_workload(&t, &w, &cfg(9008, IsolationMix::Rr, &[])).unwrap_err();
        assert_eq!(f.invariant, "expect-min-retries");
        cleanup(&f);
    }

    #[test]
    fn worker_panic_is_reported() {
        let t = fake();
        let w = scripted("panics", |_, _, _, _| panic!("lock order violated"));
        let f = run_workload(&t, &w, &cfg(9009, IsolationMix::Rc, &[])).unwrap_err();
        assert_eq!(f.invariant, "worker-panic");
        assert!(f.observed.contains("lock order violated"));
        cleanup(&f);
    }

    #[test]
    #[allow(clippy::many_single_char_names)] // テスト内の短い名前。
    fn retry_replays_the_same_random_sequence() {
        // やり直しの 2 回目は、1 回目と同じ乱数の列から始まる。
        let seen = Arc::new(Mutex::new(Vec::<(usize, u64, bool)>::new()));
        let log = Arc::clone(&seen);
        let t = fake();
        let w = scripted("replay", move |c, rng, tid, _| {
            select_one(c)?;
            let first = rng.next_u64();
            let mut s = log.lock().unwrap();
            let again = s.iter().any(|(t2, v, _)| *t2 == tid && *v == first);
            let fail_this = !again && s.iter().filter(|(t2, _, _)| *t2 == tid).count() % 2 == 0;
            s.push((tid, first, fail_this));
            drop(s);
            if fail_this {
                Err(StressError::new("40P01", "deadlock detected"))
            } else {
                Ok(())
            }
        });
        let mut c = cfg(1, IsolationMix::Rc, &[]);
        c.threads = 1;
        let r = run_workload(&t, &w, &c).unwrap();
        assert!(r.retried_40p01 > 0);
        let s = seen.lock().unwrap();
        // 失敗した試行の直後の試行は、同じ値を引いている。
        let mut replays = 0;
        for pair in s.windows(2) {
            if pair[0].2 {
                assert_eq!(pair[0].1, pair[1].1);
                replays += 1;
            }
        }
        assert!(replays > 0);
    }

    #[test]
    fn config_from_env_defaults() {
        let c = StressConfig::from_env(IsolationMix::Rc, &["23505"]);
        assert!(c.threads >= 1);
        assert_eq!(c.watchdog, Duration::from_secs(20));
        assert_eq!(c.allowed, &["23505"]);
    }
}
