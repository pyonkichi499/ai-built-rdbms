//! `WalReader`: WAL を先頭から読み、終わりを判定する（`m3.md` §4.4、§6.3.5）。
//!
//! 担当 W2 が実装する。ここにあるのは A が置いたスタブ（API は §4.4 のとおり）。

#![allow(dead_code, clippy::unused_self, clippy::needless_pass_by_value)]

use std::sync::Arc;

use super::record::RecordError;
use super::{DecodedRecord, Lsn, WalConfig};
use crate::error::{Error, Result};
use crate::storage::vfs::Vfs;

#[derive(Debug)]
pub struct WalReader {
    cfg: WalConfig,
    pos: Lsn,
    include_switch: bool,
    end: Option<(Lsn, EndReason)>,
    last_start: Option<Lsn>,
}

/// WAL の終わりと判定した理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    ZeroLength,
    BadHeader(RecordError),
    Crc,
    PrevMismatch,
    CrossesSegment,
    MissingSegment(u64),
    BadSegmentHeader(u64),
    TooLong,
}

impl WalReader {
    /// `start` から読む。最初のレコードの prev は検査しない（以後は直前のレコードの開始と一致すること）。
    pub fn open(vfs: Arc<dyn Vfs>, cfg: &WalConfig, start: Lsn) -> WalReader {
        let _ = vfs;
        WalReader {
            cfg: *cfg,
            pos: start,
            include_switch: false,
            end: None,
            last_start: None,
        }
    }

    /// 次のレコード。WAL の終わり（不正なレコードを含む）なら `Ok(None)`。`NotFound` 以外の I/O エラーは
    /// `Err`。SWITCH は返さずに次のセグメントへ進む（dump 用に `include_switch` を立てれば返す）。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<DecodedRecord>> {
        Err(Error::internal("WalReader::next: 担当 W2 が実装"))
    }

    pub fn set_include_switch(&mut self, on: bool) {
        self.include_switch = on;
    }

    /// `None` を返した後で有効。次のレコードを書くべき位置（正規化済み）と理由。
    pub fn end_of_wal(&self) -> Option<(Lsn, EndReason)> {
        self.end.clone()
    }

    /// 最後に返した（SWITCH を含む）有効なレコードの開始 LSN。
    pub fn last_record_start(&self) -> Option<Lsn> {
        self.last_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::vfs::SimVfs;

    #[test]
    fn stub_has_no_end_before_reading() {
        let cfg = WalConfig {
            segment_size: super::super::MIN_WAL_SEGMENT_SIZE,
            system_identifier: 1,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        };
        let mut r = WalReader::open(Arc::new(SimVfs::new(1)), &cfg, Lsn(0x20_0020));
        assert!(r.end_of_wal().is_none());
        assert!(r.last_record_start().is_none());
        r.set_include_switch(true);
        assert!(r.next().is_err());
    }
}
