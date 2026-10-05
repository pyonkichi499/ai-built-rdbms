//! `Wal`: 挿入、flush、FPW の判定、REDO 点、リカバリモード（`m3.md` §4.3、§6.3.2〜）。
//!
//! 担当 W1 が実装する。ここにあるのは A が置いたスタブ（API は §4.3 のとおり）。
//! 構築系は「未実装」の内部エラーを返す。

#![allow(dead_code, clippy::unused_self, clippy::needless_pass_by_value)]

use std::sync::Arc;

use super::{Inserted, Lsn, RecordBuilder, WalConfig};
use crate::error::{Error, Result};
use crate::storage::buffer::WalFlush;
use crate::storage::vfs::Vfs;

fn not_implemented(what: &str) -> Error {
    Error::internal(format!("{what}: 担当 W1 が実装"))
}

#[derive(Debug)]
pub struct Wal {
    cfg: WalConfig,
}

impl Wal {
    /// initdb 用。セグメント 1 を作り、書き込みモードで開く（挿入位置 = `seg_size + 32`、prev = 0）。
    pub fn initialize(vfs: Arc<dyn Vfs>, cfg: WalConfig) -> Result<Arc<Wal>> {
        let _ = (vfs, cfg);
        Err(not_implemented("Wal::initialize"))
    }

    /// 正常停止後の起動。`insert_pos` = チェックポイントレコードの end（正規化する）、
    /// prev = その start。
    pub fn open_at(
        vfs: Arc<dyn Vfs>,
        cfg: WalConfig,
        insert_pos: Lsn,
        prev: Lsn,
    ) -> Result<Arc<Wal>> {
        let _ = (vfs, cfg, insert_pos, prev);
        Err(not_implemented("Wal::open_at"))
    }

    /// クラッシュリカバリ用。insert は内部エラーを返し、`flush_to` は「REDO 済みの位置以下」だけを許す。
    pub fn open_for_recovery(vfs: Arc<dyn Vfs>, cfg: WalConfig) -> Arc<Wal> {
        let _ = vfs;
        Arc::new(Wal { cfg })
    }

    /// REDO ループが 1 レコード適用するたびに呼ぶ。
    pub fn note_replayed(&self, end: Lsn) {
        let _ = end;
    }

    /// REDO の後。`end_of_wal` 以降のセグメントの残りを 0 で埋めて sync、後続のセグメントを消して
    /// `sync_dir` し、書き込みモードに切り替える（§6.3.6）。
    pub fn finish_recovery(&self, end_of_wal: Lsn, last_record_start: Lsn) -> Result<()> {
        let _ = (end_of_wal, last_record_start);
        Err(not_implemented("Wal::finish_recovery"))
    }

    /// レコードを WAL バッファに足す。FPW の判定・画像のコピー・prev の設定・CRC の計算は挿入
    /// Mutex の中で行う。戻り値の end を、登録した全ページに `set_lsn` するのは呼び出し側の責任。
    /// 失敗は `Severity::Panic`。
    pub fn insert(&self, rec: RecordBuilder<'_>) -> Result<Inserted> {
        let _ = rec;
        Err(not_implemented("Wal::insert"))
    }

    /// `upto` までを write + `sync_data` する。`flushed_lsn() >= upto` なら何もしない。
    pub fn flush(&self, upto: Lsn) -> Result<()> {
        let _ = upto;
        Err(not_implemented("Wal::flush"))
    }

    /// 次のレコードの開始位置（正規化済み）。
    pub fn insert_lsn(&self) -> Lsn {
        Lsn::INVALID
    }

    pub fn flushed_lsn(&self) -> Lsn {
        Lsn::INVALID
    }

    /// FPW の判定に使う REDO 点。
    pub fn redo_lsn(&self) -> Lsn {
        Lsn::INVALID
    }

    /// オンラインのチェックポイント: 挿入 Mutex の中で `CHECKPOINT_REDO` を挿入し、その開始位置を
    /// REDO 点にする。
    pub fn begin_checkpoint_online(&self) -> Result<Lsn> {
        Err(not_implemented("Wal::begin_checkpoint_online"))
    }

    /// 停止・リカバリ終了のチェックポイント（他に書き手がいない）: 現在の挿入位置を REDO 点にする。
    pub fn begin_checkpoint_quiet(&self) -> Result<Lsn> {
        Err(not_implemented("Wal::begin_checkpoint_quiet"))
    }

    /// REDO 点を含むセグメントより前のセグメントを消す。
    pub fn remove_segments_before(&self, redo: Lsn) -> Result<()> {
        let _ = redo;
        Err(not_implemented("Wal::remove_segments_before"))
    }

    /// 現在の REDO 点からの WAL のバイト数（WAL 量の契機）。
    pub fn bytes_since_redo(&self) -> u64 {
        0
    }

    pub fn is_poisoned(&self) -> bool {
        false
    }

    pub fn config(&self) -> &WalConfig {
        &self.cfg
    }
}

impl WalFlush for Wal {
    /// 書き込みモードは `flush(Lsn(lsn))`、リカバリモードは replayed 以下か検査するだけ。
    fn flush_to(&self, lsn: u64) -> Result<()> {
        self.flush(Lsn(lsn))
    }

    fn redo_ptr(&self) -> u64 {
        self.redo_lsn().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::vfs::SimVfs;

    fn cfg() -> WalConfig {
        WalConfig {
            segment_size: super::super::MIN_WAL_SEGMENT_SIZE,
            system_identifier: 42,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        }
    }

    #[test]
    fn stub_reports_not_implemented_instead_of_succeeding() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        assert!(Wal::initialize(Arc::clone(&vfs), cfg()).is_err());
        let w = Wal::open_for_recovery(vfs, cfg());
        assert_eq!(w.config().system_identifier, 42);
        assert!(w.flush_to(8).is_err());
        assert_eq!(w.insert_lsn(), Lsn::INVALID);
        assert!(!w.is_poisoned());
    }
}
