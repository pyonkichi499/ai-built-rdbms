//! 変異テスト（ハーネス自体が壊れを検出できることの確認。`m3.md` §7.5、m3-recovery §9.6）。
//!
//! 本物の経路を `DebugKnobs`（と `SimVfs::set_ignore_sync_dir`）でわざと壊し、既定のシード集合の
//! うち少なくとも 1 つで、期待する不変条件の違反が見つかることを確かめる。見つからなければ、
//! ハーネスが壊れを見逃している。

use std::sync::Arc;

use yuzhu_core::catalog::depend::{DropItem, DropKind, DropPlan, ObjectAddress};
use yuzhu_core::catalog::store::DependFilter;
use yuzhu_core::storage::{UniqueCheck, WriteCtx};
use yuzhu_core::testing::user_relation_defs;
use yuzhu_core::txn::WaitCtl;
use yuzhu_core::types::{Datum, Tid};
use yuzhu_core::{Cluster, InterruptFlag};

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

// ----- M4: 索引・シーケンス・カタログの変異（11 §3.6.3）---------------------------

/// リカバリ後にカタログへ入れる傷（コミットする）。`check_catalog` の各条件が検出するべきもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogDamage {
    /// 制約が所有する索引の `pg_depend`（索引 → 制約、`i`）の行を消す（条件 4）。
    DependRow,
    /// 索引の `pg_index` の行（と `pg_class` の行）を消す（条件 2・3・5）。
    IndexRow,
}

/// 傷を付ける。索引が（まだ）ないなら `Ok(false)`。
pub(crate) fn damage_catalog(
    cluster: &Arc<Cluster>,
    d: CatalogDamage,
) -> std::result::Result<bool, String> {
    let defs = user_relation_defs(cluster, "postgres").map_err(|e| e.message)?;
    let Some((table, idx)) = defs.iter().find_map(|t| {
        t.indexes
            .iter()
            .find(|i| i.constraint.is_some())
            .map(|i| (t, i))
    }) else {
        return Ok(false);
    };
    let (db, _) = cluster
        .connect("postgres", "postgres")
        .map_err(|e| e.message)?;
    let tm = cluster.txn_manager();
    let interrupts = InterruptFlag::default();
    let (xid, guard) = tm
        .begin_write(
            u64::MAX - 1,
            &WaitCtl {
                lock_timeout: None,
                interrupts: &interrupts,
            },
        )
        .map_err(|e| e.message)?;
    let w = WriteCtx { xid, cid: 0 };
    let snap = tm.snapshot(Some(xid), 0);
    let done = match d {
        CatalogDamage::DependRow => {
            let con = idx.constraint.as_ref().expect("filtered above");
            db.catalog
                .delete_dependencies(
                    &w,
                    &snap,
                    DependFilter::Exact {
                        dependent: ObjectAddress::relation(idx.oid),
                        referenced: ObjectAddress::constraint(con.oid),
                    },
                )
                .map(|_| ())
        }
        CatalogDamage::IndexRow => db
            .catalog
            .drop_objects(
                &w,
                &snap,
                &DropPlan {
                    items: vec![DropItem {
                        addr: ObjectAddress::relation(idx.oid),
                        kind: DropKind::Index,
                        description: format!("index {} on {}", idx.name, table.name),
                        locator: None,
                        owner_table: None,
                        attnum: None,
                    }],
                    cascaded: Vec::new(),
                },
            )
            .map(|_| ()),
    };
    done.map_err(|e| e.message)?;
    tm.commit(xid, &[]).map_err(|e| e.message)?;
    drop(guard);
    Ok(true)
}

/// M4 の変異。各変異は「既定のシード集合のうち少なくとも 1 つで検出する」。`workloads` の名前は
/// 11 §3.6.1 のワークロード 6〜8（`indexed_table`、`sequences`、`ddl_mix`）。
pub(crate) fn m4_mutations() -> Vec<Mutation> {
    const IDX_EXPECT: &[&str] = &["I14", "I13", "I8", "I2", "I4", "startup"];
    vec![
        Mutation {
            name: "index: no full page writes",
            expect: &["I14", "I13", "I8", "startup"],
            workloads: &["indexed_table"],
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
            name: "index: break WAL-before-data",
            expect: IDX_EXPECT,
            workloads: &["indexed_table"],
            modes: &[ModeSpec::Drop],
            apply: |s| {
                s.knobs.skip_wal_before_data = true;
                s.knobs.assert_wal_before_data = false;
            },
        },
        Mutation {
            name: "index: REDO ignores page_lsn",
            expect: IDX_EXPECT,
            workloads: &["indexed_table"],
            modes: &[ModeSpec::Keep],
            apply: |s| {
                s.knobs.redo_ignore_page_lsn = true;
                s.knobs.disable_full_page_writes = true;
            },
        },
        Mutation {
            name: "index: split in two records",
            expect: IDX_EXPECT,
            workloads: &["indexed_table"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.btree_split_in_two_records = true,
        },
        Mutation {
            name: "index: lossy insert",
            expect: &["I13", "I4"],
            workloads: &["indexed_table"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.btree_lossy_insert_every = 7,
        },
        Mutation {
            name: "sequence: no flush at commit",
            expect: &["I15", "I1", "I4"],
            workloads: &["sequences"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.skip_commit_flush = true,
        },
        Mutation {
            name: "sequence: ignore foreign WAL",
            expect: &["I15"],
            workloads: &["sequences"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.seq_ignore_foreign_wal = true,
        },
        Mutation {
            name: "sequence: REDO skips if page newer",
            expect: &["I15", "I8"],
            workloads: &["sequences"],
            modes: &[ModeSpec::Drop, ModeSpec::Keep],
            apply: |s| s.knobs.seq_redo_skip_if_page_newer = true,
        },
        Mutation {
            name: "sequence: no force_log",
            expect: &["I15"],
            workloads: &["sequences"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.knobs.seq_no_force_log = true,
        },
        Mutation {
            name: "catalog: lost pg_depend row",
            expect: &["I16"],
            workloads: &["ddl_mix"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.damage = Some(CatalogDamage::DependRow),
        },
        Mutation {
            name: "catalog: lost pg_index row",
            expect: &["I16"],
            workloads: &["ddl_mix"],
            modes: &[ModeSpec::Drop],
            apply: |s| s.damage = Some(CatalogDamage::IndexRow),
        },
    ]
}

/// どの不変条件を、どの変異が検出するか（`expect` の先頭が主たる検出先）。
pub(crate) fn detectors_of(inv: &str) -> Vec<&'static str> {
    m4_mutations()
        .into_iter()
        .filter(|m| m.expect.contains(&inv))
        .map(|m| m.name)
        .collect()
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
    // ----- M4 -----

    use yuzhu_core::storage::btree::check::check_structure;
    use yuzhu_core::testing::{TestCluster, TestClusterOptions};

    #[test]
    fn m4_mutation_names_are_unique() {
        let names: Vec<&str> = m4_mutations().iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    /// どの不変条件にも、それを検出する変異が最低 1 つある（11 §3.6.3）。
    #[test]
    fn every_new_invariant_has_a_detecting_mutation() {
        for inv in ["I13", "I14", "I15", "I16"] {
            assert!(!detectors_of(inv).is_empty(), "no mutation detects {inv}");
        }
        assert!(detectors_of("I13").contains(&"index: lossy insert"));
        assert!(detectors_of("I16").contains(&"catalog: lost pg_depend row"));
    }

    /// M4 の変異が使うノブが、本当に `DebugKnobs` にある（名前だけでなく効くこと）。
    #[test]
    fn m4_mutations_set_their_knobs() {
        let mut s = CaseSpec::new("bank", 1);
        for m in m4_mutations() {
            (m.apply)(&mut s);
        }
        assert!(s.knobs.btree_split_in_two_records);
        assert!(s.knobs.seq_ignore_foreign_wal);
        assert!(s.knobs.seq_redo_skip_if_page_newer);
        assert!(s.knobs.seq_no_force_log);
        assert_eq!(s.knobs.btree_lossy_insert_every, 7);
        assert!(s.damage.is_some());
    }

    #[test]
    fn the_lossy_knob_drops_every_nth_insert_and_delegates_the_rest() {
        let mut opts = TestClusterOptions::crash_sim(3);
        opts.knobs.btree_lossy_insert_every = 3;
        let tc = TestCluster::with_options(opts).unwrap();
        {
            let mut s = tc.session("postgres").unwrap();
            let out = yuzhu_core::testing::run_sql(&mut s, "CREATE TABLE t (k int PRIMARY KEY)");
            assert!(out.is_ok(), "{:?}", out.errors);
        }
        let h = tc
            .index_handle("postgres", "t_pkey")
            .unwrap()
            .expect("t_pkey");
        let lossy = tc.cluster.indexes();
        let w = WriteCtx {
            xid: yuzhu_core::txn::Xid(3),
            cid: 0,
        };
        for k in 1..=9u32 {
            lossy
                .insert(
                    &w,
                    &h,
                    &[Datum::Int4(i32::try_from(k).unwrap())],
                    Tid {
                        block: 0,
                        offset: u16::try_from(k).unwrap(),
                    },
                    UniqueCheck::Skip,
                )
                .unwrap();
        }
        let stats = check_structure(tc.pool(), &h).unwrap();
        assert_eq!(stats.items, 6);
    }

    #[test]
    fn catalog_damage_is_applied_only_when_an_index_exists() {
        let tc = TestCluster::with_options(TestClusterOptions::crash_sim(4)).unwrap();
        assert!(!damage_catalog(&tc.cluster, CatalogDamage::DependRow).unwrap());
    }
}
