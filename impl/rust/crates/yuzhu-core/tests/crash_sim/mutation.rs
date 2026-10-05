//! 変異テスト（ハーネス自体が壊れを検出できることの確認。`m3.md` §7.5、m3-recovery §9.6）。
//!
//! 本物の経路を `DebugKnobs`（と `SimVfs::set_ignore_sync_dir`）でわざと壊し、既定のシード集合の
//! うち少なくとも 1 つで、期待する不変条件の違反が見つかることを確かめる。見つからなければ、
//! ハーネスが壊れを見逃している。

use crate::workload::{Arm, Rng};
use crate::{CaseSpec, Failure, LARGE_NTX, ModeSpec, SMALL_NTX, measure, run_case};

/// 変異 1 つ。
pub(crate) struct Mutation {
    pub(crate) name: &'static str,
    /// 検出されたときの不変条件（`Violation::inv`）としてよいもの。
    pub(crate) expect: &'static [&'static str],
    /// 試すワークロード。
    pub(crate) workloads: &'static [&'static str],
    /// 試すクラッシュのしかた。
    pub(crate) modes: &'static [ModeSpec],
    /// 変異を `spec` に仕込む。
    pub(crate) apply: fn(&mut CaseSpec),
}

/// 既定のシード集合。
pub(crate) const SEEDS: &[u64] = &[11, 12, 13, 14, 15, 16];

/// 1 シードあたりに試す N の数（終わりまで走らせる N なしも必ず試す）。
const POINTS_PER_SEED: u64 = 6;

pub(crate) fn mutations() -> Vec<Mutation> {
    vec![
        Mutation {
            name: "no full page writes",
            expect: &["I4", "I8", "I1", "I2", "startup"],
            workloads: &["hot_update", "small_pool"],
            modes: &[
                ModeSpec::Torn {
                    sector: 512,
                    percent: 50,
                },
                ModeSpec::Torn {
                    sector: 4096,
                    percent: 50,
                },
            ],
            apply: |s| s.knobs.disable_full_page_writes = true,
        },
        Mutation {
            name: "no flush at commit",
            expect: &["I1"],
            workloads: &["bank", "applog"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.skip_commit_flush = true,
        },
        Mutation {
            name: "break WAL-before-data",
            expect: &["I2", "I4", "I1", "startup"],
            workloads: &["small_pool", "applog"],
            modes: &[ModeSpec::Drop],
            apply: |s| {
                s.knobs.skip_wal_before_data = true;
                s.knobs.assert_wal_before_data = false;
            },
        },
        Mutation {
            name: "no clog flush at checkpoint",
            expect: &["I1", "I7"],
            workloads: &["bank", "hot_update"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.skip_clog_flush_at_checkpoint = true,
        },
        Mutation {
            name: "single-slot control file",
            expect: &["startup"],
            workloads: &["bank", "hot_update"],
            modes: &[ModeSpec::Torn {
                sector: 1,
                percent: 50,
            }],
            apply: |s| s.knobs.single_slot_control_file = true,
        },
        Mutation {
            name: "REDO ignores page_lsn",
            expect: &["I4", "I1", "I8", "startup", "I2"],
            workloads: &["hot_update", "bank"],
            modes: &[ModeSpec::Keep],
            // FPW が有効だと、チェックポイント後の最初の変更が全ページイメージで上書きされるので、
            // LSN の判定を外しても二重適用が起きない。FPW も切って判定だけが頼りの状態にする。
            apply: |s| {
                s.knobs.redo_ignore_page_lsn = true;
                s.knobs.disable_full_page_writes = true;
            },
        },
        Mutation {
            name: "no directory fsync",
            expect: &["I1", "I8", "startup"],
            workloads: &["ddl_mix", "small_pool"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.ignore_sync_dir = true,
        },
    ]
}

/// 変異を仕込んだ試験を、シード・ワークロード・N・クラッシュのしかたを変えながら流し、
/// 最初に見つかった違反を返す。
pub(crate) fn detect(m: &Mutation, seeds: &[u64]) -> Option<Failure> {
    for &seed in seeds {
        for (wi, w) in m.workloads.iter().enumerate() {
            for ntx in [SMALL_NTX, LARGE_NTX] {
                // 変異なしの I/O の総数（N の範囲）。
                let total = measure(w, seed, ntx);
                let mut rng = Rng::new(seed ^ (wi as u64) << 16 ^ u64::from(ntx));
                for mode in m.modes {
                    for point in 0..=POINTS_PER_SEED {
                        let mut spec = CaseSpec::new(w, seed);
                        spec.ntx = ntx;
                        spec.mode = *mode;
                        // 最後の 1 回は障害なしで終わりまで走らせる。
                        spec.arm = if point == POINTS_PER_SEED {
                            Arm::None
                        } else {
                            Arm::CrashAt(rng.below(total + 1))
                        };
                        spec.mutation = Some(m.name);
                        (m.apply)(&mut spec);
                        if let Err(f) = run_case(&spec) {
                            return Some(f);
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::print_stderr)]
    fn check(name: &str) {
        let m = mutations()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no mutation {name}"));
        match detect(&m, SEEDS) {
            None => panic!("mutation {:?} was not detected by any seed", m.name),
            Some(f) => {
                eprintln!("mutation {:?} detected: {f}", m.name);
                assert!(
                    m.expect.contains(&f.violation.inv),
                    "mutation {:?} was detected as {}, expected one of {:?}\n{f}",
                    m.name,
                    f.violation.inv,
                    m.expect
                );
            }
        }
    }

    #[test]
    fn detects_missing_full_page_writes() {
        check("no full page writes");
    }

    #[test]
    fn detects_missing_commit_flush() {
        check("no flush at commit");
    }

    #[test]
    fn detects_broken_wal_before_data() {
        check("break WAL-before-data");
    }

    #[test]
    fn detects_missing_clog_flush() {
        check("no clog flush at checkpoint");
    }

    #[test]
    fn detects_single_slot_control_file() {
        check("single-slot control file");
    }

    #[test]
    fn detects_redo_without_page_lsn_check() {
        check("REDO ignores page_lsn");
    }

    #[test]
    fn detects_missing_directory_fsync() {
        check("no directory fsync");
    }

    #[test]
    fn every_mutation_is_listed_once() {
        let names: Vec<&str> = mutations().iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert_eq!(names.len(), 7);
    }

    /// 変異を仕込まなければ、同じ手順で何も見つからない（偽陽性がない）。
    #[test]
    fn unmutated_run_is_clean() {
        let m = Mutation {
            name: "none",
            expect: &[],
            workloads: &["bank", "small_pool"],
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
        if let Some(f) = detect(&m, &SEEDS[..2]) {
            panic!("{f}");
        }
    }
}
