//! VACUUM（VC。03 §4.2）。
//!
//! VC-3a が型・環境・関数の口を置いた。**本体（3 段階。VC-3b）が `vacuum_relation` / `analyze_relation` /
//! `update_datfrozenxid_and_truncate_clog` の本体を書く**。それまでの 3 関数は何もせずに成功する
//! （M4 の `VACUUM` が何もしなかったのと同じ）。

pub mod autovacuum;
pub mod driver;
pub mod stats;

use std::sync::Arc;

use crate::backend::BackendId;
use crate::engine::{Cluster, DatabaseHandle};
use crate::error::Result;
use crate::interrupt::InterruptFlag;
use crate::session::Notice;
use crate::txn::Xid;
use crate::txn::lock::{LockScope, WaitCtl};
use crate::types::Oid;
use crate::wal::Lsn;

pub use crate::analyzer::ddl_ext::vacuum::{IndexCleanup, VacuumOptions};

/// 新品のデータベース（何も書いていない template0）の `yz_datxid` の値。大きいので最小値に影響しない。
pub const DATFROZEN_PRISTINE: Xid = Xid(i64::MAX as u64);
/// PG の `REL_TRUNCATE_MINIMUM`。
pub const REL_TRUNCATE_MINIMUM: u32 = 1000;
/// PG の `REL_TRUNCATE_FRACTION`。
pub const REL_TRUNCATE_FRACTION: u32 = 16;

/// VACUUM / ANALYZE を動かす環境。呼び出し側が文脈から作る。
#[allow(missing_debug_implementations)]
pub struct VacuumEnv<'a> {
    pub cluster: &'a Cluster,
    pub db: &'a Arc<DatabaseHandle>,
    pub backend: BackendId,
    pub wait: &'a WaitCtl<'a>,
    pub interrupts: &'a InterruptFlag,
    /// VERBOSE（INFO）と WARNING の出力先。
    pub notices: &'a mut Vec<Notice>,
    /// SQL: `Transaction`（`commit_and_restart` で外れる）。autovacuum: `Session`（自分で解放する）。
    pub lock_scope: LockScope,
    /// 外側のトランザクションで動く ANALYZE だけ。
    pub own_xid: Option<Xid>,
    /// 真なら待ち手が現れたら中断する。
    pub is_autovacuum: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexVacStats {
    pub index_oid: Oid,
    pub pages: u32,
    pub tuples_removed: u64,
    pub tuples_remaining: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VacuumRelStats {
    pub rel_pages_start: u32,
    pub rel_pages_end: u32,
    pub pages_scanned: u32,
    pub tuples_deleted: u64,
    pub new_live_tuples: u64,
    pub recently_dead_tuples: u64,
    pub frozen_tuples: u64,
    pub frozen_pages: u32,
    pub lpdead_items: u64,
    pub pages_with_lpdead: u32,
    pub index_scans: u32,
    pub index_stats: Vec<IndexVacStats>,
    pub removable_cutoff: Xid,
    pub old_relfrozenxid: Xid,
    pub new_relfrozenxid: Xid,
    pub truncated_to: Option<u32>,
    pub last_lsn: Lsn,
}

impl VacuumRelStats {
    /// 何もしなかった結果（VC-3b までの口が返す）。
    pub fn untouched() -> VacuumRelStats {
        VacuumRelStats {
            rel_pages_start: 0,
            rel_pages_end: 0,
            pages_scanned: 0,
            tuples_deleted: 0,
            new_live_tuples: 0,
            recently_dead_tuples: 0,
            frozen_tuples: 0,
            frozen_pages: 0,
            lpdead_items: 0,
            pages_with_lpdead: 0,
            index_scans: 0,
            index_stats: Vec::new(),
            removable_cutoff: Xid::INVALID,
            old_relfrozenxid: Xid::INVALID,
            new_relfrozenxid: Xid::INVALID,
            truncated_to: None,
            last_lsn: Lsn::INVALID,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason {
    /// 表でない（`relkind`）。
    NotATable(char),
    /// 並行して消えた。
    Gone,
    LockNotAvailable,
    CancelledByWaiter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VacuumRelOutcome {
    Done(VacuumRelStats),
    Skipped(SkipReason),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClogTruncateOutcome {
    pub datfrozenxid_advanced: bool,
    pub oldest_xid: Xid,
    pub segments_removed: u32,
}

/// 1 テーブルの VACUUM（03 §5.3）。`ShareUpdateExclusive` を取るところから。
/// Transaction スコープなら解放は呼び出し側のコミット。
///
/// 本体は VC-3b。いまは何もしない。
pub fn vacuum_relation(
    _env: &mut VacuumEnv<'_>,
    _relid: Oid,
    _opts: &VacuumOptions,
) -> Result<VacuumRelOutcome> {
    Ok(VacuumRelOutcome::Done(VacuumRelStats::untouched()))
}

/// ANALYZE（03 §5.8）。`relpages` と `reltuples` だけ。`columns` は attnum（空なら全列）。
///
/// 本体は VC-3b。いまは何もしない。
pub fn analyze_relation(
    _env: &mut VacuumEnv<'_>,
    _relid: Oid,
    _columns: &[i16],
    _verbose: bool,
    _skip_locked: bool,
) -> Result<()> {
    Ok(())
}

/// データベースの凍結境界を進め、進んだら clog を切り詰める（03 §5.5）。
/// VACUUM の最後と autovacuum の 1 データベースごとに呼ぶ。
///
/// 本体は VC-3b / VC-4。いまは何もしない。
pub fn update_datfrozenxid_and_truncate_clog(
    _env: &mut VacuumEnv<'_>,
) -> Result<ClogTruncateOutcome> {
    Ok(ClogTruncateOutcome::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_follow_the_design() {
        assert_eq!(DATFROZEN_PRISTINE, Xid(u64::try_from(i64::MAX).unwrap()));
        assert_eq!((REL_TRUNCATE_MINIMUM, REL_TRUNCATE_FRACTION), (1000, 16));
    }

    #[test]
    fn pristine_boundary_matches_the_catalog_row() {
        assert_eq!(
            i64::try_from(DATFROZEN_PRISTINE.0).unwrap(),
            crate::catalog::rows::DATFROZEN_PRISTINE
        );
    }

    #[test]
    fn default_options_follow_the_design() {
        let o = VacuumOptions::default();
        assert!(o.truncate && !o.verbose && !o.analyze && !o.freeze && !o.skip_locked);
        assert_eq!(o.index_cleanup, IndexCleanup::Auto);
        assert!(!o.only_database_stats && !o.skip_database_stats && o.tid_capacity.is_none());
    }
}
