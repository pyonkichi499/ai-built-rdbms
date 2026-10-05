//! REDO の骨格: `RedoCtx`、`read_buffer_for_redo`、invalid page の追跡、
//! REDO ループ（`m3.md` §4.4、§6.4）。
//!
//! 担当 W2 が実装する。`InvalidPages` は A が置いた単純な実装、そのほかはスタブ。

#![allow(dead_code, clippy::unused_self, clippy::needless_pass_by_value)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use super::{DecodedRecord, Lsn, Wal, WalReader};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::{BufferPool, PinnedBuffer};
use crate::storage::smgr::{BlockNumber, ForkNumber, RelFileLocator, StorageManager};
use crate::txn::Xid;

/// REDO 関数に渡す文脈。`recovery.rs` が作る。
///
/// `wal::redo` は `txn` より下の層なので `Clog` の型を名指しできない。上位の層の部品は
/// `ext` に型を消して入れる（`recovery.rs` が `Arc<Clog>` を入れ、`txn::xact_wal::redo` が
/// `downcast_ref::<Arc<Clog>>()` で取り出す。ほかの用途には使わない）。
pub struct RedoCtx {
    pub pool: Arc<BufferPool>,
    pub smgr: Arc<StorageManager>,
    pub ext: Box<dyn std::any::Any + Send + Sync>,
    pub invalid: Mutex<InvalidPages>,
    /// XLOG のチェックポイントレコードで進める。
    pub next_oid: AtomicU32,
    pub knobs: DebugKnobs,
}

impl std::fmt::Debug for RedoCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedoCtx")
            .field("invalid", &self.invalid)
            .field("next_oid", &self.next_oid)
            .field("knobs", &self.knobs)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum RedoBuffer {
    /// 画像で上書きし、LSN を設定済み。何もしなくてよい。
    Restored,
    /// `page_lsn >= record.end`。適用済み。
    Done,
    /// 適用が必要。`WILL_INIT` なら中身は 0（呼び出し側が初期化する）。
    NeedsRedo(PinnedBuffer),
    /// ファイルかブロックがない（invalid page として記録済み）。飛ばす。
    NotFound,
}

/// PostgreSQL の `XLogReadBufferForRedoExtended` に相当する（§6.4.3）。担当 W2 が実装する。
pub fn read_buffer_for_redo(
    ctx: &RedoCtx,
    rec: &DecodedRecord,
    block_id: u8,
) -> Result<RedoBuffer> {
    let _ = (ctx, rec, block_id);
    Err(Error::internal("read_buffer_for_redo: 担当 W2 が実装"))
}

/// REDO 中に見つかった「ファイルやブロックがない」参照の記録（D14）。
#[derive(Debug, Default)]
pub struct InvalidPages {
    pages: HashMap<(RelFileLocator, ForkNumber), BTreeSet<BlockNumber>>,
}

impl InvalidPages {
    pub fn record(&mut self, rel: RelFileLocator, fork: ForkNumber, blk: BlockNumber) {
        self.pages.entry((rel, fork)).or_default().insert(blk);
    }

    /// リレーションごと消えた（unlink の REDO）。
    pub fn forget_relation(&mut self, rel: RelFileLocator) {
        self.pages.retain(|(r, _), _| *r != rel);
    }

    /// `nblocks` 以降が消えた（truncate の REDO）。
    pub fn forget_from(&mut self, rel: RelFileLocator, fork: ForkNumber, nblocks: BlockNumber) {
        if let Some(set) = self.pages.get_mut(&(rel, fork)) {
            set.retain(|b| *b < nblocks);
            if set.is_empty() {
                self.pages.remove(&(rel, fork));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 空でなければ `Severity::Panic`（XX001 "WAL contains references to invalid pages"、
    /// DETAIL に一覧）。
    pub fn check_empty(&self) -> Result<()> {
        if self.pages.is_empty() {
            return Ok(());
        }
        let mut lines: Vec<String> = self
            .pages
            .iter()
            .flat_map(|((rel, fork), blks)| {
                blks.iter().map(move |b| {
                    format!(
                        "rel {}/{}/{} fork {} blk {}",
                        rel.spc_oid, rel.db_oid, rel.rel_number.0, *fork as u8, b
                    )
                })
            })
            .collect();
        lines.sort();
        Err(Error::new(
            sqlstate::DATA_CORRUPTED,
            "WAL contains references to invalid pages",
        )
        .with_detail(lines.join("\n"))
        .with_severity(Severity::Panic))
    }
}

/// REDO ループ（§5.5 の手順 e-3）。`dispatch` は `recovery.rs` の振り分け表。担当 W2 が実装する。
pub fn run_redo(
    ctx: &RedoCtx,
    wal: &Wal,
    reader: &mut WalReader,
    dispatch: &dyn Fn(&RedoCtx, &DecodedRecord) -> Result<()>,
    on_record: &mut dyn FnMut(&DecodedRecord),
) -> Result<RedoStats> {
    let _ = (ctx, wal, reader, dispatch, on_record);
    Err(Error::internal("run_redo: 担当 W2 が実装"))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RedoStats {
    pub records: u64,
    pub fpi_restored: u64,
    pub done_skipped: u64,
    pub max_xid: Xid,
    pub start: Lsn,
    pub last_start: Lsn,
    pub end: Lsn,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::RelFileNumber;

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    #[test]
    fn empty_invalid_pages_pass() {
        assert!(InvalidPages::default().check_empty().is_ok());
    }

    #[test]
    fn remaining_invalid_pages_are_a_panic_with_detail() {
        let mut ip = InvalidPages::default();
        ip.record(rel(16384), ForkNumber::Main, 7);
        let e = ip.check_empty().unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert!(e.detail.as_deref().unwrap().contains("blk 7"));
    }

    #[test]
    fn forget_relation_and_forget_from() {
        let mut ip = InvalidPages::default();
        ip.record(rel(1), ForkNumber::Main, 3);
        ip.record(rel(1), ForkNumber::Main, 9);
        ip.record(rel(2), ForkNumber::Main, 0);
        ip.forget_from(rel(1), ForkNumber::Main, 5);
        assert!(ip.check_empty().is_err());
        ip.forget_from(rel(1), ForkNumber::Main, 3);
        ip.forget_relation(rel(2));
        assert!(ip.is_empty());
        ip.check_empty().unwrap();
    }
}
