//! 層 1 のクラッシュ試験（`m3.md` §7.5）。担当 T。
//!
//! `SimVfs` の上で、起動後の I/O の N 番目の直前でディスクを凍結し、`abandon` → `crash(mode)` →
//! `Cluster::open`（リカバリ）→ 不変条件 I1〜I12 の検査をくり返す。
//!
//! 失敗したら、メッセージに再現用の環境変数が出る。
//!
//! ```text
//! YUZHU_SIM_SEED=3 YUZHU_CRASH_AT=120 YUZHU_CRASH_WORKLOAD=bank YUZHU_CRASH_MODE=drop \
//!     cargo test --test crash_sim replay -- --nocapture
//! ```
//!
//! 環境変数: `YUZHU_SIM_SEED`（必須。これがあるときだけ `replay` が動く）、`YUZHU_CRASH_AT`（N。
//! 省略すると障害なしで最後まで走らせてからクラッシュ）、`YUZHU_CRASH_WORKLOAD`（既定 `bank`）、
//! `YUZHU_CRASH_MUTATION`（変異の名前。`mutation.rs` の `mutations()`）、`YUZHU_CRASH_MODE`（`drop` / `keep` / `random50` / `torn512-50` など。既定 `drop`）、
//! `YUZHU_CRASH_NTX`（トランザクション数）、`YUZHU_CRASH_ROUNDS`（クラッシュの回数）。
//! 長時間のランダム実行は `#[ignore]`（`cargo test --test crash_sim -- --ignored`。
//! `YUZHU_SOAK_SEEDS` で件数）。

#![allow(clippy::too_many_lines, clippy::result_large_err)]

mod invariants;
mod model;
mod mutation;
mod workload;

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use yuzhu_core::storage::vfs::{
    CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs, Vfs,
};
use yuzhu_core::testing::{TestCluster, TestClusterOptions, cluster_options};
use yuzhu_core::{Cluster, DebugKnobs};

use invariants::{
    PreCrash, Violation, check_after_recovery, dump_tables, pre_crash, pre_recovery_scan, violation,
};
use model::Model;
use workload::{Arm, FaultKind, Rng, Run, Stop, Workload};

// ----- 1 件の試験の仕様 ---------------------------------------------------------

/// クラッシュのしかた（再現用に文字列で表せる）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ModeSpec {
    Drop,
    Keep,
    /// 未 sync の操作を確率 `percent`% で残す。
    Random(u32),
    /// `sector` バイトごとに確率 `percent`% で残す（`sector = 1` は任意のバイト境界）。
    Torn {
        sector: usize,
        percent: u32,
    },
}

impl ModeSpec {
    fn crash_mode(self) -> CrashMode {
        let p = |percent: u32| f64::from(percent) / 100.0;
        match self {
            ModeSpec::Drop => CrashMode::DropUnsynced,
            ModeSpec::Keep => CrashMode::KeepAll,
            ModeSpec::Random(c) => CrashMode::RandomSubset {
                keep_probability: p(c),
            },
            ModeSpec::Torn { sector, percent } => CrashMode::TornSectors {
                sector,
                keep_probability: p(percent),
            },
        }
    }

    fn parse(s: &str) -> ModeSpec {
        let bad =
            || -> ! { panic!("bad YUZHU_CRASH_MODE {s:?} (drop, keep, randomNN, tornSECTOR-NN)") };
        match s {
            "drop" => ModeSpec::Drop,
            "keep" => ModeSpec::Keep,
            _ => {
                if let Some(n) = s.strip_prefix("random") {
                    ModeSpec::Random(n.parse().unwrap_or_else(|_| bad()))
                } else if let Some(r) = s.strip_prefix("torn") {
                    let (sector, percent) = r.split_once('-').unwrap_or_else(|| bad());
                    ModeSpec::Torn {
                        sector: sector.parse().unwrap_or_else(|_| bad()),
                        percent: percent.parse().unwrap_or_else(|_| bad()),
                    }
                } else {
                    bad()
                }
            }
        }
    }
}

impl fmt::Display for ModeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeSpec::Drop => f.write_str("drop"),
            ModeSpec::Keep => f.write_str("keep"),
            ModeSpec::Random(p) => write!(f, "random{p}"),
            ModeSpec::Torn { sector, percent } => write!(f, "torn{sector}-{percent}"),
        }
    }
}

/// 障害の仕掛け方（リカバリ中のクラッシュ I9、fsync の失敗 I11 を含む）。
#[derive(Clone, Debug)]
pub(crate) struct CaseSpec {
    pub(crate) workload: &'static str,
    pub(crate) seed: u64,
    /// 1 回目のワークロードで仕掛けるもの。
    pub(crate) arm: Arm,
    pub(crate) mode: ModeSpec,
    /// 1 ラウンドのトランザクション数。
    pub(crate) ntx: u32,
    /// 「ワークロード → クラッシュ → リカバリ → 検査」の回数（I10）。2 回目以降の N と mode は
    /// シードから決める。
    pub(crate) rounds: u32,
    /// リカバリの I/O の N 番目でもう一度クラッシュさせる（I9）。1 回目のリカバリだけ。
    pub(crate) recovery_crash_at: Option<u64>,
    /// リカバリのあと、何も書かずにもう一度クラッシュ・リカバリして内容が同じことを確かめる（I9）。
    pub(crate) double_recovery: bool,
    pub(crate) knobs: DebugKnobs,
    pub(crate) ignore_sync_dir: bool,
    /// 2 回目以降のラウンドの N の上限（1 回目の I/O の総数を目安に）。
    pub(crate) later_ops: u64,
    /// 各ラウンドのワークロードが終わったら、クラッシュの前に正常停止を試みる。
    pub(crate) shutdown: bool,
    /// 仕込んだ変異の名前（再現用。`mutation.rs` の `mutations()`）。
    pub(crate) mutation: Option<&'static str>,
    /// リカバリの後、検査の前にカタログへ傷を付ける（コミットする）。
    pub(crate) damage: Option<mutation::CatalogDamage>,
}

impl CaseSpec {
    pub(crate) fn new(workload: &'static str, seed: u64) -> CaseSpec {
        CaseSpec {
            workload,
            seed,
            arm: Arm::None,
            mode: ModeSpec::Drop,
            ntx: SMALL_NTX,
            rounds: 1,
            recovery_crash_at: None,
            double_recovery: false,
            knobs: DebugKnobs {
                assert_wal_before_data: true,
                ..DebugKnobs::default()
            },
            ignore_sync_dir: false,
            later_ops: 200,
            shutdown: false,
            mutation: None,
            damage: None,
        }
    }

    fn repro(&self) -> String {
        use std::fmt::Write as _;
        let mut env = format!("YUZHU_SIM_SEED={}", self.seed);
        match self.arm {
            Arm::CrashAt(n) => write!(env, " YUZHU_CRASH_AT={n}"),
            Arm::FsyncFailAt(n) => write!(env, " YUZHU_FSYNC_FAIL_AT={n}"),
            Arm::FaultAt { op, n, kind } => write!(
                env,
                " YUZHU_FAULT_AT={}:{n}:{}",
                workload::fault_op_name(op),
                kind.name()
            ),
            Arm::None => Ok(()),
        }
        .expect("write to a String");
        write!(
            env,
            " YUZHU_CRASH_WORKLOAD={} YUZHU_CRASH_MODE={} YUZHU_CRASH_NTX={} YUZHU_CRASH_ROUNDS={} YUZHU_LATER_OPS={}",
            self.workload, self.mode, self.ntx, self.rounds, self.later_ops
        )
        .expect("write to a String");
        if let Some(n) = self.recovery_crash_at {
            write!(env, " YUZHU_RECOVERY_CRASH_AT={n}").expect("write to a String");
        }
        if self.double_recovery {
            env.push_str(" YUZHU_DOUBLE_RECOVERY=1");
        }
        if self.shutdown {
            env.push_str(" YUZHU_SHUTDOWN=1");
        }
        if let Some(m) = self.mutation {
            write!(env, " YUZHU_CRASH_MUTATION='{m}'").expect("write to a String");
        }
        format!("{env} cargo test --test crash_sim replay -- --nocapture")
    }
}

/// 失敗した試験。
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) spec: CaseSpec,
    pub(crate) round: u32,
    pub(crate) violation: Violation,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "crash test failed: {} (workload {}, seed {}, arm {:?}, mode {}, round {}, recovery_crash_at {:?})\n  reproduce: {}",
            self.violation,
            self.spec.workload,
            self.spec.seed,
            self.spec.arm,
            self.spec.mode,
            self.round,
            self.spec.recovery_crash_at,
            self.spec.repro()
        )
    }
}

pub(crate) const SMALL_NTX: u32 = 4;
pub(crate) const LARGE_NTX: u32 = 25;
/// この数以上のトランザクションを流す版を「大きい版」とする（`indexed_table` の索引の一括構築）。
pub(crate) const BIG_NTX: u32 = 20;
/// `wal_fill` が WAL を 2 MiB のセグメントの外まで進める数。
pub(crate) const WAL_FILL_NTX: u32 = 110;

fn options_for(spec: &CaseSpec, w: &Workload) -> TestClusterOptions {
    let mut o = TestClusterOptions::crash_sim(spec.seed);
    o.nframes = w.nframes;
    if w.name == "indexed_table" && spec.ntx >= BIG_NTX {
        // 一括構築の 1 レコードは 33 ページを同時にピンする（`BT_BUILD_BATCH_PAGES` = 32 + メタ）。
        // 06 §7.11 の `shared_buffers = 24` では `XX000 no unpinned buffers available` になるので、
        // 構築を通す大きい版だけ 48 にする（実装の不具合として報告済み）。
        o.nframes = 48;
    }
    o.knobs = spec.knobs;
    o
}

fn dyn_vfs(vfs: &SimVfs) -> Arc<dyn Vfs> {
    Arc::new(vfs.clone())
}

fn open_cluster(vfs: &SimVfs, opts: &TestClusterOptions) -> Result<Arc<Cluster>, Violation> {
    let v = dyn_vfs(vfs);
    let co = cluster_options(opts);
    match catch_unwind(AssertUnwindSafe(|| Cluster::open(v, co))) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(violation(
            "startup",
            format!(
                "recovery failed: {} {:?}: {}",
                e.sqlstate.0, e.severity, e.message
            ),
        )),
        Err(p) => Err(violation(
            "startup",
            format!("recovery panicked: {}", panic_text(&p)),
        )),
    }
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "(non-string panic)".into())
}

/// ワークロードを障害なしで最後まで走らせ、起動後の I/O の総数を返す（N の範囲を決める）。
pub(crate) fn measure(workload: &'static str, seed: u64, ntx: u32) -> u64 {
    let w = workload::by_name(workload);
    let mut spec = CaseSpec::new(workload, seed);
    spec.ntx = ntx;
    let opts = options_for(&spec, &w);
    let TestCluster { vfs, cluster, .. } =
        TestCluster::with_options(opts.clone()).expect("initdb and start");
    let mut run = Run::new(
        vfs,
        cluster,
        &opts,
        Model::default(),
        Arm::None,
        w.checkpoint_every,
    );
    let mut rng = Rng::new(seed);
    match (w.run)(&mut run, &mut rng, 0, ntx) {
        Ok(()) => run.ops_done(),
        Err(e) => panic!("the workload {workload} fails without any fault: {e:?}"),
    }
}

/// 障害なしで走らせ、（ワークロードの終わりまでの I/O の数, 正常停止の終わりまでの I/O の数）を返す。
pub(crate) fn measure_with_shutdown(workload: &'static str, seed: u64, ntx: u32) -> (u64, u64) {
    let w = workload::by_name(workload);
    let mut spec = CaseSpec::new(workload, seed);
    spec.ntx = ntx;
    let opts = options_for(&spec, &w);
    let TestCluster { vfs, cluster, .. } =
        TestCluster::with_options(opts.clone()).expect("initdb and start");
    let mut run = Run::new(
        vfs,
        cluster,
        &opts,
        Model::default(),
        Arm::None,
        w.checkpoint_every,
    );
    let mut rng = Rng::new(seed);
    if let Err(e) = (w.run)(&mut run, &mut rng, 0, ntx) {
        panic!("the workload {workload} fails without any fault: {e:?}");
    }
    let ran = run.ops_done();
    if let Err(e) = run.shutdown() {
        panic!("shutdown of {workload} fails without any fault: {e:?}");
    }
    (ran, run.ops_done())
}

/// M4 のワークロードは DROP・TRUNCATE・ROLLBACK で関係ファイルを消す。チェックポイントの後始末（`finish_pending_unlinks`）の
/// `SyncDir` の失敗は警告だけで次のチェックポイントにやり直す設計（完了したチェックポイントの後なので止めない）。
/// なのでその失敗は「握りつぶし」ではない（孤児は D15 で許す）。
fn leftover_cleanup_may_absorb(spec: &CaseSpec) -> bool {
    matches!(
        spec.arm,
        Arm::FaultAt {
            op: FaultOp::SyncDir,
            ..
        }
    ) && workload::M4_WORKLOAD_NAMES.contains(&spec.workload)
}

/// 1 件を実行する。
pub(crate) fn run_case(spec: &CaseSpec) -> Result<(), Failure> {
    let w = workload::by_name(spec.workload);
    let opts = options_for(spec, &w);
    let fail = |round: u32, v: Violation| Failure {
        spec: spec.clone(),
        round,
        violation: v,
    };

    let TestCluster {
        mut vfs,
        mut cluster,
        ..
    } = TestCluster::with_options(opts.clone()).expect("initdb and start");
    vfs.set_ignore_sync_dir(spec.ignore_sync_dir);
    let mut model = Model::default();
    let mut rng = Rng::new(spec.seed ^ 0x5eed);
    let mut wl_rng = Rng::new(spec.seed);

    for round in 0..spec.rounds {
        let (arm, mode) = if round == 0 {
            (spec.arm, spec.mode)
        } else {
            (
                Arm::CrashAt(rng.below(spec.later_ops + 1)),
                random_mode(&mut rng),
            )
        };
        let mut run = Run::new(
            vfs.clone(),
            Arc::clone(&cluster),
            &opts,
            model,
            arm,
            w.checkpoint_every,
        );
        let ran_to_end = match (w.run)(&mut run, &mut wl_rng, round, spec.ntx) {
            Ok(()) if run.arm_must_stop() && run.fired() && !leftover_cleanup_may_absorb(spec) => {
                return Err(fail(
                    round,
                    violation(
                        "I11",
                        "the injected fault fired but the workload ran to the end without an error (an I/O error was swallowed)",
                    ),
                ));
            }
            Ok(()) => true,
            Err(Stop::Crashed) => false,
            Err(Stop::Bug(m)) => return Err(fail(round, violation("workload", m))),
        };
        if ran_to_end && matches!(spec.arm, Arm::None) && !spec.shutdown && round == 0 {
            // 障害なしで最後まで走った: 孤児ファイル（後始末漏れ）がない。
            if let Err(Stop::Bug(m)) = run.rollback_open() {
                return Err(fail(round, violation("workload", m)));
            }
            if let Err(v) = invariants::check_no_orphans(&run.cluster) {
                return Err(fail(round, v));
            }
        }
        if ran_to_end && spec.shutdown {
            // 正常停止（ShuttingDown → Shutdown チェックポイント → ShutDown）。I/O のどこかで凍結してもよい。
            if let Err(Stop::Bug(m)) = run.shutdown() {
                return Err(fail(round, violation("shutdown", m)));
            }
        }
        let unknown = run.unknown.clone();
        let committed = run.model.clone();
        let crash: PreCrash = pre_crash(&run.cluster, !unknown.is_empty());
        run.drop_sessions();
        drop(run);
        cluster.abandon();

        // クラッシュとリカバリ（I9: 1 回目のリカバリの途中でもう一度クラッシュさせる）。
        let mut disk = vfs.crash(mode.crash_mode());
        if round == 0
            && let Some(n) = spec.recovery_crash_at
        {
            // 検査用の読み取りも I/O に数えるので、先に済ませてから仕掛ける。
            if let Err(v) = pre_recovery_scan(&disk) {
                return Err(fail(round, v));
            }
            disk.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Any,
                    path_prefix: None,
                    nth: Some(n + 1),
                    probability: None,
                    effect: FaultEffect::CrashFreeze,
                }],
            });
            if let Ok(c) = open_cluster(&disk, &opts) {
                // 凍結の前にリカバリが終わった。
                c.abandon();
            }
            disk = disk.crash(random_mode(&mut rng).crash_mode());
        }
        let pre = match pre_recovery_scan(&disk) {
            Ok(p) => p,
            Err(v) => return Err(fail(round, v)),
        };
        let mut recovered = match open_cluster(&disk, &opts) {
            Ok(c) => c,
            Err(v) => return Err(fail(round, v)),
        };
        if spec.double_recovery && round == 0 {
            let first = match dump_tables(&recovered) {
                Ok(d) => d,
                Err(v) => return Err(fail(round, v)),
            };
            recovered.abandon();
            disk = disk.crash(ModeSpec::Drop.crash_mode());
            recovered = match open_cluster(&disk, &opts) {
                Ok(c) => c,
                Err(v) => return Err(fail(round, v)),
            };
            match dump_tables(&recovered) {
                Ok(second) if second == first => {}
                Ok(second) => {
                    return Err(fail(
                        round,
                        violation(
                            "I9",
                            format!(
                                "a second recovery changed the contents: {}",
                                model::diff(&first, &second)
                            ),
                        ),
                    ));
                }
                Err(v) => return Err(fail(round, v)),
            }
        }
        if let Some(d) = spec.damage
            && let Err(m) = mutation::damage_catalog(&recovered, d)
        {
            return Err(fail(
                round,
                violation("workload", format!("cannot damage the catalog: {m}")),
            ));
        }
        let checked = catch_unwind(AssertUnwindSafe(|| {
            check_after_recovery(
                &recovered, &disk, &committed, &unknown, &pre, &crash, w.check,
            )
        }));
        match checked {
            Ok(Ok((m, _applied))) => model = m,
            Ok(Err(v)) => return Err(fail(round, v)),
            Err(p) => {
                return Err(fail(
                    round,
                    violation("I8", format!("a check panicked: {}", panic_text(&p))),
                ));
            }
        }
        vfs = disk;
        cluster = recovered;
    }
    Ok(())
}

/// 2 回目以降のクラッシュのしかた。
fn random_mode(rng: &mut Rng) -> ModeSpec {
    match rng.below(6) {
        0 => ModeSpec::Drop,
        1 => ModeSpec::Keep,
        2 => ModeSpec::Random(50),
        3 => ModeSpec::Torn {
            sector: 512,
            percent: 50,
        },
        4 => ModeSpec::Torn {
            sector: 4096,
            percent: 50,
        },
        _ => ModeSpec::Torn {
            sector: 1,
            percent: 60,
        },
    }
}

/// 失敗したら panic（メッセージに再現方法が入る）。
pub(crate) fn expect_ok(r: Result<(), Failure>) {
    if let Err(f) = r {
        panic!("{f}");
    }
}

// ----- 試験 ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const WORKLOADS: &[&str] = workload::WORKLOAD_NAMES;

    /// M3 と M4 のワークロード全部（障害注入・物理比較・夜間 soak 用）。
    fn all_workloads() -> Vec<&'static str> {
        WORKLOADS
            .iter()
            .chain(workload::M4_WORKLOAD_NAMES)
            .copied()
            .collect()
    }

    /// M3 のワークロード名の一覧に M4 のものを足す。
    fn with_m4(base: &[&'static str]) -> Vec<&'static str> {
        base.iter()
            .chain(workload::M4_WORKLOAD_NAMES)
            .copied()
            .collect()
    }

    /// 小さい版のトランザクション数（M3 は `SMALL_NTX`）。
    fn small_ntx(w: &str) -> u32 {
        if workload::M4_WORKLOAD_NAMES.contains(&w) {
            m4_small_ntx(w)
        } else {
            SMALL_NTX
        }
    }

    /// 起動後の I/O の通し番号 N をすべて試す（小さいワークロード）。
    fn sweep(workload: &'static str, mode: ModeSpec, stride: u64) {
        sweep_ntx(workload, mode, stride, SMALL_NTX);
    }

    fn sweep_ntx(workload: &'static str, mode: ModeSpec, stride: u64, ntx: u32) {
        let total = measure(workload, 1, ntx);
        let mut n = 0;
        while n <= total {
            let mut spec = CaseSpec::new(workload, 1);
            spec.ntx = ntx;
            spec.arm = Arm::CrashAt(n);
            spec.mode = mode;
            expect_ok(run_case(&spec));
            n += stride;
        }
        // 最後（障害なし）も。
        let mut spec = CaseSpec::new(workload, 1);
        spec.ntx = ntx;
        spec.mode = mode;
        expect_ok(run_case(&spec));
    }

    // ----- M4: ワークロード 6〜8（11 §3.6.1）-----

    const M4: &[&str] = workload::M4_WORKLOAD_NAMES;

    /// 小さい版のトランザクション数（`sequences` は 1 周で部品 A〜J を全部通る数）。
    fn m4_small_ntx(w: &str) -> u32 {
        match w {
            "sequences" => 11,
            "ddl_mix" => 9,
            _ => SMALL_NTX,
        }
    }

    #[test]
    fn m4_workloads_run_without_faults() {
        for w in M4 {
            for ntx in [m4_small_ntx(w), LARGE_NTX] {
                let ops = measure(w, 1, ntx);
                assert!(ops > 0, "{w}");
            }
        }
    }

    /// 小さい版: N を 0 から最後まで全部（`stride` 刻み）。
    fn sweep_m4(workload: &'static str, mode: ModeSpec, stride: u64) {
        sweep_ntx(workload, mode, stride, m4_small_ntx(workload));
    }

    #[test]
    fn indexed_table_every_crash_point() {
        sweep_m4("indexed_table", ModeSpec::Drop, 1);
        sweep_m4("indexed_table", ModeSpec::Keep, 1);
    }

    #[test]
    fn sequences_every_crash_point() {
        sweep_m4("sequences", ModeSpec::Drop, 1);
        sweep_m4("sequences", ModeSpec::Keep, 1);
    }

    #[test]
    fn ddl_mix_every_crash_point() {
        sweep_m4("ddl_mix", ModeSpec::Drop, 1);
        sweep_m4("ddl_mix", ModeSpec::Keep, 1);
    }

    /// 小さい版を、`RandomSubset` と `TornSectors`（512 / 4096 / 1）でも全部の N について試す。
    #[test]
    fn m4_every_crash_point_with_torn_and_random_modes() {
        let modes = [
            ModeSpec::Random(50),
            ModeSpec::Torn {
                sector: 512,
                percent: 50,
            },
            ModeSpec::Torn {
                sector: 4096,
                percent: 50,
            },
            ModeSpec::Torn {
                sector: 1,
                percent: 60,
            },
        ];
        for w in M4 {
            let ntx = m4_small_ntx(w);
            let total = measure(w, 1, ntx);
            for n in 0..=total {
                let mode = modes[usize::try_from(n % modes.len() as u64).unwrap_or(0)];
                let mut spec = CaseSpec::new(w, 1 + 17 * (n + 1));
                spec.ntx = ntx;
                spec.arm = Arm::CrashAt(n);
                spec.mode = mode;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 大きい版: 固定のシード（CI は 24 個 = ワークロードごとに 8）で N とクラッシュのしかた
    /// （`DropUnsynced`、`KeepAll`、`RandomSubset`、`TornSectors { 512 / 4096 }`）を選ぶ。
    #[test]
    fn m4_random_crash_points() {
        for (i, w) in M4.iter().enumerate() {
            let total = measure(w, 100, LARGE_NTX);
            for k in 0..8u64 {
                let seed = 100 + k;
                let mut rng = Rng::new(seed ^ ((i as u64 + 7) << 8));
                let mut spec = CaseSpec::new(w, seed);
                spec.ntx = LARGE_NTX;
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                expect_ok(run_case(&spec));
            }
        }
    }

    /// I10: M4 のワークロードでも、クラッシュ → リカバリ → 続き、をくり返す。
    #[test]
    fn m4_repeated_crashes_and_recoveries() {
        for (i, w) in M4.iter().enumerate() {
            let ntx = m4_small_ntx(w);
            let total = measure(w, 200, ntx);
            for k in 0..3u64 {
                let seed = 200 + k;
                let mut rng = Rng::new(seed ^ ((i as u64 + 7) << 8));
                let mut spec = CaseSpec::new(w, seed);
                spec.ntx = ntx;
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                spec.rounds = 4;
                spec.later_ops = total;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// I9: リカバリの途中でクラッシュしても、再リカバリで同じ内容になる。
    #[test]
    fn m4_crash_during_recovery() {
        for w in M4 {
            let ntx = m4_small_ntx(w);
            let total = measure(w, 300, ntx);
            for k in 0..4u64 {
                let mut rng = Rng::new(300 + k);
                let mut spec = CaseSpec::new(w, 300 + k);
                spec.ntx = ntx;
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                spec.recovery_crash_at = Some(rng.below(120));
                spec.double_recovery = true;
                spec.rounds = 2;
                spec.later_ops = total;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 正常停止（停止チェックポイント）の各 I/O でクラッシュさせても、M4 の不変条件が成り立つ。
    #[test]
    fn m4_shutdown_at_every_io_point() {
        for w in M4 {
            let ntx = m4_small_ntx(w);
            let (ran, done) = measure_with_shutdown(w, 1, ntx);
            for n in ran.saturating_sub(3)..=done + 1 {
                let mut spec = CaseSpec::new(w, 1);
                spec.ntx = ntx;
                spec.shutdown = true;
                spec.arm = Arm::CrashAt(n);
                spec.mode = ModeSpec::Drop;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 大きい版の `indexed_table` は、33 ページ以上の索引を一括構築する（32 ページごとのレコードを通す）。
    #[test]
    fn indexed_table_builds_an_index_of_more_than_32_pages() {
        let w = workload::by_name("indexed_table");
        let mut best = 0;
        for seed in 1..=6u64 {
            let mut spec = CaseSpec::new("indexed_table", seed);
            spec.ntx = LARGE_NTX;
            let opts = options_for(&spec, &w);
            let TestCluster { vfs, cluster, .. } =
                TestCluster::with_options(opts.clone()).expect("initdb and start");
            let mut run = Run::new(
                vfs,
                Arc::clone(&cluster),
                &opts,
                Model::default(),
                Arm::None,
                w.checkpoint_every,
            );
            let mut rng = Rng::new(seed);
            (w.run)(&mut run, &mut rng, 0, LARGE_NTX).expect("the workload runs without faults");
            let defs =
                yuzhu_core::testing::user_relation_defs(&cluster, "postgres").expect("catalog");
            for idx in defs.iter().flat_map(|t| t.indexes.iter()) {
                if idx.name == "t_big_ix" {
                    let n = cluster
                        .stack()
                        .pool
                        .nblocks(idx.locator, yuzhu_core::storage::smgr::ForkNumber::Main)
                        .expect("nblocks");
                    best = best.max(n);
                }
            }
            run.drop_sessions();
            cluster.abandon();
        }
        assert!(
            best >= 34,
            "the largest bulk-built index has only {best} blocks"
        );
    }

    // ----- M4 の変異テスト（11 §3.6.3）: 既定のシード集合のうち少なくとも 1 つで検出する -----

    /// 変異 `name` を既定のシード集合（`mutation::SEEDS`）で試し、期待する不変条件の違反で検出されることを確かめる。
    #[allow(clippy::print_stderr)]
    fn detects_m4(name: &str) {
        let m = mutation::m4_mutations()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no mutation {name}"));
        match mutation::detect(&m, mutation::SEEDS) {
            None => panic!("mutation {:?} was not detected by any seed", m.name),
            Some(f) => {
                eprintln!("mutation {:?} detected: {f}", m.name);
                assert!(
                    m.expect.contains(&f.violation.inv),
                    "mutation {:?} was detected as {} (expected one of {:?})\n{f}",
                    m.name,
                    f.violation.inv,
                    m.expect
                );
            }
        }
    }

    #[test]
    fn detects_index_without_full_page_writes() {
        detects_m4("index: no full page writes");
    }

    #[test]
    fn detects_index_broken_wal_before_data() {
        detects_m4("index: break WAL-before-data");
    }

    #[test]
    fn detects_index_redo_without_page_lsn_check() {
        detects_m4("index: REDO ignores page_lsn");
    }

    #[test]
    fn detects_index_split_in_two_records() {
        detects_m4("index: split in two records");
    }

    #[test]
    fn detects_lossy_index_insert() {
        detects_m4("index: lossy insert");
    }

    #[test]
    fn detects_sequence_without_commit_flush() {
        detects_m4("sequence: no flush at commit");
    }

    #[test]
    fn detects_sequence_ignoring_foreign_wal() {
        detects_m4("sequence: ignore foreign WAL");
    }

    #[test]
    fn detects_sequence_redo_skipping_newer_pages() {
        detects_m4("sequence: REDO skips if page newer");
    }

    #[test]
    fn detects_sequence_without_force_log() {
        detects_m4("sequence: no force_log");
    }

    #[test]
    fn detects_lost_pg_depend_row() {
        detects_m4("catalog: lost pg_depend row");
    }

    #[test]
    fn detects_lost_pg_index_row() {
        detects_m4("catalog: lost pg_index row");
    }

    /// M4 の変異は表（11 §3.6.3）の 11 行すべてで、テストが 1 つずつある。
    #[test]
    fn every_m4_mutation_has_a_test() {
        assert_eq!(mutation::m4_mutations().len(), 11);
    }

    /// 変異を仕込まなければ、M4 のワークロードで何も見つからない（偽陽性がない）。
    #[test]
    fn unmutated_m4_runs_are_clean() {
        let m = mutation::Mutation {
            name: "none",
            expect: &[],
            workloads: workload::M4_WORKLOAD_NAMES,
            modes: &[
                ModeSpec::Drop,
                ModeSpec::Torn {
                    sector: 512,
                    percent: 50,
                },
                ModeSpec::Random(50),
            ],
            apply: |_| {},
        };
        if let Some(f) = mutation::detect(&m, &mutation::SEEDS[..2]) {
            panic!("{f}");
        }
    }
    #[test]
    fn workloads_run_without_faults() {
        for w in WORKLOADS {
            let ops = measure(w, 1, SMALL_NTX);
            assert!(ops > 0, "{w}");
        }
    }

    #[test]
    fn replay() {
        let Ok(seed) = std::env::var("YUZHU_SIM_SEED") else {
            return;
        };
        let get = |k: &str| std::env::var(k).ok();
        let workload = get("YUZHU_CRASH_WORKLOAD").unwrap_or_else(|| "bank".into());
        let name = WORKLOADS
            .iter()
            .chain(workload::M4_WORKLOAD_NAMES)
            .chain(std::iter::once(&workload::WAL_FILL))
            .find(|w| **w == workload)
            .unwrap_or_else(|| panic!("unknown workload {workload}"));
        let mut spec = CaseSpec::new(name, seed.parse().expect("YUZHU_SIM_SEED"));
        if let Some(n) = get("YUZHU_CRASH_AT") {
            spec.arm = Arm::CrashAt(n.parse().expect("YUZHU_CRASH_AT"));
        }
        if let Some(n) = get("YUZHU_FSYNC_FAIL_AT") {
            spec.arm = Arm::FsyncFailAt(n.parse().expect("YUZHU_FSYNC_FAIL_AT"));
        }
        if let Some(f) = get("YUZHU_FAULT_AT") {
            let mut it = f.split(':');
            let mut next = || {
                it.next()
                    .unwrap_or_else(|| panic!("bad YUZHU_FAULT_AT {f:?} (op:n:kind)"))
            };
            let op = workload::parse_fault_op(next());
            let n = next().parse().expect("YUZHU_FAULT_AT n");
            let kind = workload::FaultKind::parse(next());
            spec.arm = Arm::FaultAt { op, n, kind };
        }
        if get("YUZHU_SHUTDOWN").is_some_and(|v| v != "0") {
            spec.shutdown = true;
        }
        if let Some(n) = get("YUZHU_LATER_OPS") {
            spec.later_ops = n.parse().expect("YUZHU_LATER_OPS");
        }
        if let Some(n) = get("YUZHU_RECOVERY_CRASH_AT") {
            spec.recovery_crash_at = Some(n.parse().expect("YUZHU_RECOVERY_CRASH_AT"));
        }
        if get("YUZHU_DOUBLE_RECOVERY").is_some_and(|v| v != "0") {
            spec.double_recovery = true;
        }
        if let Some(m) = get("YUZHU_CRASH_MODE") {
            spec.mode = ModeSpec::parse(&m);
        }
        if let Some(n) = get("YUZHU_CRASH_NTX") {
            spec.ntx = n.parse().expect("YUZHU_CRASH_NTX");
        }
        if let Some(n) = get("YUZHU_CRASH_ROUNDS") {
            spec.rounds = n.parse().expect("YUZHU_CRASH_ROUNDS");
        }
        if let Some(name) = get("YUZHU_CRASH_MUTATION") {
            let m = mutation::mutations()
                .into_iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("unknown mutation {name:?}"));
            spec.mutation = Some(m.name);
            (m.apply)(&mut spec);
        }
        expect_ok(run_case(&spec));
    }

    #[test]
    fn mode_text_round_trips() {
        for m in [
            ModeSpec::Drop,
            ModeSpec::Keep,
            ModeSpec::Random(50),
            ModeSpec::Torn {
                sector: 512,
                percent: 30,
            },
        ] {
            assert_eq!(ModeSpec::parse(&m.to_string()), m);
        }
    }

    #[test]
    fn bank_every_crash_point_drop_unsynced() {
        sweep("bank", ModeSpec::Drop, 1);
    }

    #[test]
    fn bank_every_crash_point_keep_all() {
        sweep("bank", ModeSpec::Keep, 1);
    }

    #[test]
    fn applog_every_crash_point() {
        sweep("applog", ModeSpec::Drop, 1);
        sweep("applog", ModeSpec::Keep, 2);
    }

    #[test]
    fn hot_update_every_crash_point() {
        sweep("hot_update", ModeSpec::Drop, 1);
        sweep("hot_update", ModeSpec::Keep, 2);
    }

    #[test]
    fn ddl_plain_every_crash_point() {
        sweep("ddl_plain", ModeSpec::Drop, 1);
        sweep("ddl_plain", ModeSpec::Keep, 2);
    }

    #[test]
    fn small_pool_every_crash_point() {
        sweep("small_pool", ModeSpec::Drop, 1);
        sweep("small_pool", ModeSpec::Keep, 2);
    }

    /// 大きいワークロード: シードで N とクラッシュのしかたを選ぶ。
    #[test]
    fn random_crash_points() {
        for (i, w) in WORKLOADS.iter().enumerate() {
            let total = measure(w, 100, LARGE_NTX);
            for k in 0..4u64 {
                let seed = 100 + k;
                let mut rng = Rng::new(seed ^ ((i as u64) << 8));
                let mut spec = CaseSpec::new(w, seed);
                spec.ntx = LARGE_NTX;
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 障害なしで走らせたあとの `pg_wal` のセグメント番号。
    fn segments_after_run(workload: &'static str, ntx: u32) -> Vec<u64> {
        let w = workload::by_name(workload);
        let spec = CaseSpec::new(workload, 1);
        let opts = options_for(&spec, &w);
        let TestCluster { vfs, cluster, .. } =
            TestCluster::with_options(opts.clone()).expect("initdb and start");
        let mut run = Run::new(
            vfs.clone(),
            cluster,
            &opts,
            Model::default(),
            Arm::None,
            w.checkpoint_every,
        );
        let mut rng = Rng::new(1);
        (w.run)(&mut run, &mut rng, 0, ntx).expect("the workload runs without faults");
        yuzhu_core::wal::segment::list_segments(&vfs).expect("list WAL segments")
    }

    /// WAL が 2 MiB のセグメントを越える（切り替え・新セグメントの作成・古いセグメントの削除が
    /// 実際に起きる）ワークロードの前提。
    #[test]
    fn wal_fill_switches_and_removes_segments() {
        let segs = segments_after_run(workload::WAL_FILL, WAL_FILL_NTX);
        assert!(
            segs.iter().any(|s| *s >= 2),
            "no segment switch happened: {segs:?}"
        );
        assert!(
            segs.iter().all(|s| *s >= 2),
            "the first segment was never removed by a checkpoint: {segs:?}"
        );
    }

    /// セグメントの作成（tmp → fsync → rename → `sync_dir`）と古いセグメントの削除をまたぐ N を、
    /// ワークロード全体にわたって等間隔に試す。
    #[test]
    fn wal_segment_switch_and_removal_crash_points() {
        let total = measure(workload::WAL_FILL, 1, WAL_FILL_NTX);
        let stride = (total / 70).max(1);
        for (i, n) in (0..=total)
            .step_by(usize::try_from(stride).unwrap_or(1))
            .enumerate()
        {
            let mut spec = CaseSpec::new(workload::WAL_FILL, 1);
            spec.ntx = WAL_FILL_NTX;
            spec.arm = Arm::CrashAt(n);
            spec.mode = if i % 2 == 0 {
                ModeSpec::Drop
            } else {
                ModeSpec::Torn {
                    sector: 512,
                    percent: 50,
                }
            };
            expect_ok(run_case(&spec));
        }
    }

    /// `wal_consistency_checking` 相当: 障害なしで動かしたあとの全ページと、同じ WAL だけから
    /// REDO で作ったページ（最後のチェックポイント以降を捨てたディスクをリカバリ）が一致する。
    /// 可視性に効かない項目（ctid 連鎖、cmax など）のずれも見つける。
    #[test]
    fn redo_rebuilds_the_same_pages_as_runtime() {
        for w in all_workloads().iter().chain([&workload::WAL_FILL]) {
            let ntx = if *w == workload::WAL_FILL {
                30
            } else {
                LARGE_NTX
            };
            let wl = workload::by_name(w);
            let mut spec = CaseSpec::new(w, 5);
            spec.ntx = ntx;
            let opts = options_for(&spec, &wl);
            // 同じ手順を 2 回（決定的）。1 回目は WAL だけを残して REDO、2 回目は実行時のまま書き出す。
            let play = || {
                let TestCluster { vfs, cluster, .. } =
                    TestCluster::with_options(opts.clone()).expect("initdb and start");
                let mut run = Run::new(
                    vfs.clone(),
                    Arc::clone(&cluster),
                    &opts,
                    Model::default(),
                    Arm::None,
                    wl.checkpoint_every,
                );
                let mut rng = Rng::new(5);
                (wl.run)(&mut run, &mut rng, 0, ntx).unwrap_or_else(|e| panic!("{w}: {e:?}"));
                run.rollback_open()
                    .unwrap_or_else(|e| panic!("{w}: rollback: {e:?}"));
                run.drop_sessions();
                cluster
                    .wal()
                    .flush(cluster.wal().insert_lsn())
                    .expect("flush the WAL");
                (vfs, cluster)
            };
            // REDO のページ: ページは最後のチェックポイントのまま、WAL は全部あるディスクをリカバリし、
            // リカバリ終了のチェックポイントで書き出す。
            let (vfs, cluster) = play();
            let image = vfs.crash(CrashMode::DropUnsynced);
            cluster.abandon();
            let recovered = open_cluster(&image, &opts).unwrap_or_else(|v| panic!("{w}: {v}"));
            recovered.abandon();
            let redone = image.crash(CrashMode::KeepAll);
            // 実行時のページ: 全部書き出す。
            let (vfs, cluster) = play();
            cluster.checkpoint().expect("checkpoint");
            let runtime = vfs.crash(CrashMode::KeepAll);
            cluster.abandon();
            let rel_seg_blocks = opts.rel_seg_blocks;
            if let Err(v) = invariants::compare_physical(&runtime, &redone, rel_seg_blocks) {
                panic!("workload {w}: {v}");
            }
        }
    }

    /// I10・I12: ワークロード → クラッシュ → リカバリ → 続き、を数回くり返す。
    #[test]
    fn repeated_crashes_and_recoveries() {
        for (i, w) in WORKLOADS.iter().enumerate() {
            let total = measure(w, 200, SMALL_NTX);
            for k in 0..3u64 {
                let seed = 200 + k;
                let mut rng = Rng::new(seed ^ ((i as u64) << 8));
                let mut spec = CaseSpec::new(w, seed);
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                spec.rounds = 4;
                spec.later_ops = total;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// I9: リカバリの途中でクラッシュしても、再リカバリで同じ内容になる。2 回続けてリカバリしても同じ。
    #[test]
    fn crash_during_recovery_and_double_recovery() {
        for w in ["bank", "hot_update", "ddl_plain"] {
            let total = measure(w, 300, SMALL_NTX);
            for k in 0..6u64 {
                let mut rng = Rng::new(300 + k);
                let mut spec = CaseSpec::new(w, 300 + k);
                spec.arm = Arm::CrashAt(rng.below(total + 1));
                spec.mode = random_mode(&mut rng);
                spec.recovery_crash_at = Some(rng.below(120));
                spec.double_recovery = true;
                spec.rounds = 2;
                spec.later_ops = total;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// I11: fsync が EIO を返したら PANIC になり、そのあとのリカバリで I1〜I8 が成り立つ。
    /// 起きた障害を握りつぶして最後まで走ったら違反（`run_case`）。N は 0 から 60 まで全部。
    #[test]
    fn fsync_failure_panics_and_recovers() {
        for w in with_m4(&["bank", "applog", "ddl_plain"]) {
            for n in 0..60u64 {
                let mut spec = CaseSpec::new(w, 400 + n);
                spec.ntx = small_ntx(w);
                spec.arm = Arm::FsyncFailAt(n);
                spec.mode = if n % 2 == 0 {
                    ModeSpec::Drop
                } else {
                    ModeSpec::Random(50)
                };
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 失敗した fsync のデータを忘れるディスク（`FsyncFailAndForget`）でも、リカバリで I1〜I8 が成り立つ。
    #[test]
    fn fsync_failure_with_forgotten_data_recovers() {
        for w in with_m4(&["bank", "applog", "ddl_plain", "small_pool"]) {
            for n in (0..60u64).step_by(3) {
                let mut spec = CaseSpec::new(w, 450 + n);
                spec.ntx = small_ntx(w);
                spec.arm = Arm::FaultAt {
                    op: FaultOp::Sync,
                    n,
                    kind: FaultKind::ForgetFsync,
                };
                spec.mode = ModeSpec::Drop;
                expect_ok(run_case(&spec));
            }
        }
    }

    /// 障害モデルの抜け: `SyncDir` / `Rename` / `Open` / `Write` の EIO と、書き込みの途中失敗（ENOSPC）。
    /// `SyncDir` と `Rename` の失敗を握りつぶして最後まで走ったら違反（`Arm::must_stop`）。
    #[test]
    fn io_errors_other_than_fsync_are_not_swallowed() {
        let kinds = [
            (FaultOp::SyncDir, FaultKind::Eio),
            (FaultOp::Rename, FaultKind::Eio),
            (FaultOp::Open, FaultKind::Eio),
            (FaultOp::Write, FaultKind::Eio),
            (FaultOp::Write, FaultKind::ShortWrite),
        ];
        for w in with_m4(&["bank", "ddl_plain", "small_pool"]) {
            for (op, kind) in kinds {
                for n in (0..40u64).step_by(2) {
                    let mut spec = CaseSpec::new(w, 500 + n);
                    spec.ntx = small_ntx(w);
                    spec.arm = Arm::FaultAt { op, n, kind };
                    spec.mode = if n % 4 == 0 {
                        ModeSpec::Drop
                    } else {
                        ModeSpec::Random(50)
                    };
                    expect_ok(run_case(&spec));
                }
            }
        }
    }

    /// WAL の末尾の破れ・ページの破れ・書き込み順の入れ替え（TornSectors / RandomSubset）でも、
    /// すべての N を試す。N ごとにシードも変える（クラッシュの残し方が変わる）。
    #[test]
    fn every_crash_point_with_torn_and_random_modes() {
        let modes = [
            ModeSpec::Random(50),
            ModeSpec::Torn {
                sector: 512,
                percent: 50,
            },
            ModeSpec::Torn {
                sector: 4096,
                percent: 50,
            },
            ModeSpec::Torn {
                sector: 1,
                percent: 60,
            },
        ];
        for w in WORKLOADS {
            let total = measure(w, 1, SMALL_NTX);
            for n in 0..=total {
                let mode = modes[usize::try_from(n % modes.len() as u64).unwrap_or(0)];
                // 3 つに 1 つの N は、2 通りのシードを試す（クラッシュの残し方が変わる）。
                let seeds: &[u64] = if n % 3 == 0 {
                    &[1, 1 + 17 * (n + 1)]
                } else {
                    &[1]
                };
                for &seed in seeds {
                    let mut spec = CaseSpec::new(w, seed);
                    spec.arm = Arm::CrashAt(n);
                    spec.mode = mode;
                    expect_ok(run_case(&spec));
                }
            }
        }
    }

    /// 正常停止の経路: 停止チェックポイントの各 I/O でクラッシュさせる。また、最後まで停止できたら、
    /// 再起動後（クリーン起動）に内容が残っている。
    #[test]
    fn shutdown_at_every_io_point_and_clean_restart() {
        for w in ["bank", "applog", "ddl_plain", "hot_update"] {
            let total = measure_with_shutdown(w, 1, SMALL_NTX);
            let base = total.0;
            // ワークロードの最後の数 I/O（ROLLBACK）から、停止の最後まで。
            for n in base.saturating_sub(3)..=total.1 + 1 {
                for mode in [ModeSpec::Drop, ModeSpec::Random(50)] {
                    let mut spec = CaseSpec::new(w, 1);
                    spec.shutdown = true;
                    spec.arm = Arm::CrashAt(n);
                    spec.mode = mode;
                    expect_ok(run_case(&spec));
                }
            }
            // 停止まで障害なし（Drop でクラッシュしても、クリーンに停止していれば中身が残る）。
            let mut spec = CaseSpec::new(w, 2);
            spec.shutdown = true;
            expect_ok(run_case(&spec));
        }
    }

    /// 再現コマンドには、リカバリ中のクラッシュ・二重リカバリ・`later_ops`・停止・変異が入る。
    #[test]
    fn repro_carries_the_whole_spec() {
        let mut spec = CaseSpec::new("hot_update", 301);
        spec.arm = Arm::FaultAt {
            op: FaultOp::SyncDir,
            n: 3,
            kind: FaultKind::Eio,
        };
        spec.rounds = 2;
        spec.later_ops = 77;
        spec.recovery_crash_at = Some(17);
        spec.double_recovery = true;
        spec.shutdown = true;
        spec.mutation = Some("no directory fsync");
        let r = spec.repro();
        for part in [
            "YUZHU_SIM_SEED=301",
            "YUZHU_FAULT_AT=syncdir:3:eio",
            "YUZHU_LATER_OPS=77",
            "YUZHU_RECOVERY_CRASH_AT=17",
            "YUZHU_DOUBLE_RECOVERY=1",
            "YUZHU_SHUTDOWN=1",
            "YUZHU_CRASH_MUTATION='no directory fsync'",
        ] {
            assert!(r.contains(part), "{part} is missing from {r}");
        }
    }

    /// 長時間のランダム実行（夜間）。
    #[test]
    #[ignore = "long random run (nightly)"]
    fn soak() {
        let seeds: u64 = std::env::var("YUZHU_SOAK_SEEDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        for seed in 1000..1000 + seeds {
            let mut rng = Rng::new(seed);
            let all = all_workloads();
            let w = all[usize::try_from(rng.below(all.len() as u64)).unwrap_or(0)];
            let total = measure(w, seed, LARGE_NTX);
            let mut spec = CaseSpec::new(w, seed);
            spec.ntx = LARGE_NTX;
            spec.arm = Arm::CrashAt(rng.below(total + 1));
            spec.mode = random_mode(&mut rng);
            spec.rounds = 1 + u32::try_from(rng.below(4)).unwrap_or(0);
            spec.later_ops = total;
            spec.double_recovery = rng.chance(30);
            if rng.chance(30) {
                spec.recovery_crash_at = Some(rng.below(200));
            }
            expect_ok(run_case(&spec));
        }
    }
}
