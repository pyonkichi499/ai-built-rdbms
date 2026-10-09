//! 準備済み文（PREPARE / EXECUTE / DEALLOCATE / DISCARD。XQ が持つ）。M5 の実装で埋める。
//!
//! F0b-2 は `Session.ext` の口だけを作った。XQ-5 が文とポータルの表を足す（04 §6.2.1）。

/// Extended Query と準備済み文の状態（`Session.ext`）。XQ-2・XQ-5 が項目を足す。
#[derive(Debug, Default)]
pub(super) struct ExtStore {}

#[allow(clippy::unused_self)] // XQ-2 / XQ-5 が項目を足すまでの口。
impl ExtStore {
    /// トランザクションの終わり（コミットとロールバックの先頭、XID の確定の前）に呼ぶ。
    /// ポータルを全部捨てる。
    pub(super) fn end_of_transaction(&mut self) {}

    /// 無名の文と無名のポータルを捨てる（Simple Query の `Q` の先頭。00 §4.7）。
    pub(super) fn discard_unnamed(&mut self) {}
}
