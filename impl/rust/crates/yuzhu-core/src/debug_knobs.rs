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
    // ---- M5（00 §7 F0b-1。10 章 §6.9 の台帳。名前は持ち主の章の定義が正）----
    /// LK（01 §6.7、#14）: `AccessExclusive` が他のモードと衝突しない。
    pub lock_ignore_access_exclusive: bool,
    /// LK（01 §6.7）: ロックの衝突判定が常に「衝突しない」。
    pub lock_ignore_conflicts: bool,
    /// LK（01 §6.7）: 待ち手のモードとの衝突検査（FIFO）を省く。
    pub lock_disable_fifo: bool,
    /// LK（01 §6.7）: 割り込み規則を外す。
    pub lock_disable_breakin: bool,
    /// LK（01 §6.7、#15）: デッドロックを検出しない。
    pub deadlock_detection_off: bool,
    /// LK（01 §6.7、#13）: ロックの解放を WAL の flush と clog より前にする。
    pub release_locks_before_commit_record: bool,
    /// LK・VC 共有（01 §6.7、#2）: `oldest_xmin` が登録済みスナップショットを無視する。
    pub vacuum_ignore_registered_snapshots: bool,
    /// RW（02 §7.6）: `HEAP_LOCK` を WAL に書かない。
    pub skip_heap_lock_wal: bool,
    /// RW（02 §7.6、#10）: 再起動後の古い `MultiXactId` を「メンバーが実行中」と扱う。
    pub multixact_ignore_boundary: bool,
    /// RW（02 §7.6）: タプルロックを取らない。
    pub skip_tuple_lock: bool,
    /// RW（02 §7.6、#7）: 更新・削除が `BeingModified` で待たずに続行する。
    pub skip_update_wait: bool,
    /// RW（02 §7.6、#8）: RR で `Updated` / `Deleted` を 40001 にせず最新版を更新する。
    pub rr_skip_serialization_check: bool,
    /// RW（02 §7.6、#9）: 待った後に WHERE を再評価しない。
    pub epq_skip_recheck: bool,
    /// RW（02 §7.6、#11）: 一意検査が `WaitFor` で待たずに挿入する。
    pub unique_skip_wait: bool,
    /// VC（03 C7、#1）: 第 2 段（インデックスの削除）を省いて第 3 段へ進む。
    pub vacuum_skip_index_pass: bool,
    /// VC（03 C7、#3）: 凍結を WAL に書かない。
    pub freeze_skip_wal: bool,
    /// VC（03 C7、#4）: 制御ファイルの `oldest_xid` を fsync する前に `pg_xact` を消す。
    pub truncate_clog_before_control_fsync: bool,
    /// VC（03 C7、#5）: `yz_relxid` を実際より進めて書く。
    pub relfrozenxid_too_high: bool,
    /// VC（03 C7、#6）: FSM のチェックサム不正をそのまま信じる。
    pub fsm_trust_corrupt: bool,
    /// VC（03 C7、#19）: clog の切り詰めの境界を `next_xid` にする。
    pub clog_truncate_ignore_floor: bool,
    /// FK（07 §7.3a、#12）: 子の検査で親の行に `FOR KEY SHARE` を取らない。
    pub fk_skip_key_share: bool,
    /// AU（08 §7.3a、#18）: SCRAM のクライアント証明を検証しない。
    pub scram_skip_proof_check: bool,
    /// DB（09 §4.7、#16）: コピー前のチェックポイントを省く。
    pub skip_pre_copy_checkpoint: bool,
    /// DB（09 §4.7）: コピーの fsync を省く。
    pub skip_copy_fsync: bool,
    /// DB（09 §4.7、#17）: `DBASE_CREATE` を WAL に入れない。
    pub skip_dbase_wal: bool,
    /// DB（09 §4.7）: DROP でディレクトリの削除を行の削除のコミットより前に行う。
    pub drop_dir_before_commit: bool,
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
                lock_ignore_access_exclusive: false,
                lock_ignore_conflicts: false,
                lock_disable_fifo: false,
                lock_disable_breakin: false,
                deadlock_detection_off: false,
                release_locks_before_commit_record: false,
                vacuum_ignore_registered_snapshots: false,
                skip_heap_lock_wal: false,
                multixact_ignore_boundary: false,
                skip_tuple_lock: false,
                skip_update_wait: false,
                rr_skip_serialization_check: false,
                epq_skip_recheck: false,
                unique_skip_wait: false,
                vacuum_skip_index_pass: false,
                freeze_skip_wal: false,
                truncate_clog_before_control_fsync: false,
                relfrozenxid_too_high: false,
                fsm_trust_corrupt: false,
                clog_truncate_ignore_floor: false,
                fk_skip_key_share: false,
                scram_skip_proof_check: false,
                skip_pre_copy_checkpoint: false,
                skip_copy_fsync: false,
                skip_dbase_wal: false,
                drop_dir_before_commit: false,
            }
        );
    }
}
