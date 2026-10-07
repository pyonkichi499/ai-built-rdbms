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
    /// B1（任意。06-Q16）: B+Tree の分割の連鎖を 2 本の `BTREE_PAGES` に分ける（原子性を壊す）。
    pub btree_split_in_two_records: bool,
    /// B2: `IndexStore::insert` を N 回に 1 回、何もせずに成功させる（索引項目の取りこぼし。I13 が検出するべき）。0 で無効。
    pub btree_lossy_insert_every: u64,
    /// Q1: `SeqRun.wal_lsn` を自分が書いた分だけにする（PostgreSQL と同じ穴）。
    pub seq_ignore_foreign_wal: bool,
    /// Q1: `SEQ_LOG` の REDO が、ページの LSN が新しければ飛ばす。
    pub seq_redo_skip_if_page_newer: bool,
    /// Q1: チェックポイント後の最初の `nextval` で `force_log` を使わない。
    pub seq_no_force_log: bool,
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
                btree_split_in_two_records: false,
                btree_lossy_insert_every: 0,
                seq_ignore_foreign_wal: false,
                seq_redo_skip_if_page_newer: false,
                seq_no_force_log: false,
            }
        );
    }
}
