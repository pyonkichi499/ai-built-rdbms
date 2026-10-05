//! レコードを人が読める形で出す（`yuzhu-waldump` とテスト用。`m3.md` §6.12）。
//!
//! 担当 W2 が実装する。スタブは rmgr・info・xid・LSN だけを出す。

use super::DecodedRecord;

/// 1 行の説明。例: `rmgr: Heap len: 111 xid: 1234 lsn: 0/01000120 desc: ...`。
/// 担当 W2 が `desc:` と `blkref` を rmgr ごとに充実させる。
pub fn format_record(rec: &DecodedRecord) -> String {
    format!(
        "rmgr: {:?} xid: {} lsn: {} end: {} info: 0x{:02X} blocks: {} main_len: {}",
        rec.rmgr,
        rec.xid.0,
        rec.start,
        rec.end,
        rec.info,
        rec.blocks.len(),
        rec.main.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txn::Xid;
    use crate::wal::{Lsn, RmgrId};

    #[test]
    fn stub_format_names_the_basics() {
        let rec = DecodedRecord {
            start: Lsn(0x0100_0120),
            end: Lsn(0x0100_0190),
            xid: Xid(1234),
            rmgr: RmgrId::Heap,
            info: 0,
            blocks: vec![],
            main: vec![0; 4],
        };
        let s = format_record(&rec);
        assert!(s.contains("rmgr: Heap"));
        assert!(s.contains("xid: 1234"));
        assert!(s.contains("lsn: 0/1000120"));
    }
}
