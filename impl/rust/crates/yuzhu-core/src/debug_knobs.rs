//! 変異テスト専用のスイッチ（`m3.md` §4.10、D29）。
//!
//! 既定はすべて無効。CLI・設定ファイル・SQL からは変えられず、テストが
//! `ClusterOptions` / `StackConfig` / `WalConfig` で渡す。

/// 本物の経路をわざと壊して、クラッシュ試験のハーネスが壊れを検出できることを
/// 確かめるためのスイッチ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct DebugKnobs {
    /// W1: FPW を付けない。
    pub disable_full_page_writes: bool,
    /// E: コミットで WAL を flush しない。
    pub skip_commit_flush: bool,
    /// C: ページの書き出し前に WAL を flush しない。
    pub skip_wal_before_data: bool,
    /// E: チェックポイントで clog を書かない。
    pub skip_clog_flush_at_checkpoint: bool,
    /// B: 制御ファイルをスロット A に上書きし続ける。
    pub single_slot_control_file: bool,
    /// W2: REDO で `page_lsn >= end` の判定を外す（二重適用）。
    pub redo_ignore_page_lsn: bool,
    /// C: 書き出し時に `page_lsn <= flushed` を検査して panic する（試験では常に true）。
    pub assert_wal_before_data: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_disabled() {
        assert_eq!(
            DebugKnobs::default(),
            DebugKnobs {
                disable_full_page_writes: false,
                skip_commit_flush: false,
                skip_wal_before_data: false,
                skip_clog_flush_at_checkpoint: false,
                single_slot_control_file: false,
                redo_ignore_page_lsn: false,
                assert_wal_before_data: false,
            }
        );
    }
}
