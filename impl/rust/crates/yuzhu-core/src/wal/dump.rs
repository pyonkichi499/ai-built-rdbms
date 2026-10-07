//! レコードを人が読める形で出す（`yuzhu-waldump` とテスト用。`m3.md` §6.12）。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::xlog::{
    CheckpointRecord, XLOG_CHECKPOINT_ONLINE, XLOG_CHECKPOINT_REDO, XLOG_CHECKPOINT_SHUTDOWN,
    XLOG_FPI, XLOG_FPI_FOR_HINT, XLOG_NOOP, XLOG_SWITCH,
};
use super::{DecodedRecord, Lsn, RmgrId};

/// `pg_waldump` と同じ `%X/%08X`。
fn lsn_str(l: Lsn) -> String {
    format!("{:X}/{:08X}", l.0 >> 32, l.0 & 0xFFFF_FFFF)
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

fn xlog_desc(rec: &DecodedRecord) -> String {
    match rec.info {
        XLOG_CHECKPOINT_SHUTDOWN | XLOG_CHECKPOINT_ONLINE => match CheckpointRecord::decode(rec) {
            Ok(c) => format!(
                "CHECKPOINT_{} redo {}; next_xid {}; next_oid {}; kind {:?}; fpw {}",
                if rec.info == XLOG_CHECKPOINT_ONLINE {
                    "ONLINE"
                } else {
                    "SHUTDOWN"
                },
                c.redo,
                c.next_xid.0,
                c.next_oid,
                c.kind,
                c.full_page_writes
            ),
            Err(_) => "CHECKPOINT (invalid)".to_string(),
        },
        XLOG_CHECKPOINT_REDO => "CHECKPOINT_REDO".into(),
        XLOG_SWITCH => "SWITCH".into(),
        XLOG_FPI => "FPI".into(),
        XLOG_NOOP => "NOOP".into(),
        XLOG_FPI_FOR_HINT => "FPI_FOR_HINT".into(),
        i => format!("UNKNOWN 0x{i:02X}"),
    }
}

fn xact_desc(rec: &DecodedRecord) -> String {
    let name = if rec.info == 0x10 { "ABORT" } else { "COMMIT" };
    let nrels = u32_at(&rec.main, 8).unwrap_or(0);
    let time = rec
        .main
        .get(0..8)
        .and_then(|s| s.try_into().ok())
        .map_or(0, i64::from_le_bytes);
    let mut s = format!("{name} {time}");
    if nrels > 0 {
        let _ = write!(s, "; rels:");
        for i in 0..nrels as usize {
            let o = 16 + 12 * i;
            if let (Some(a), Some(b), Some(c)) = (
                u32_at(&rec.main, o),
                u32_at(&rec.main, o + 4),
                u32_at(&rec.main, o + 8),
            ) {
                let _ = write!(s, " {a}/{b}/{c}");
            }
        }
    }
    s
}

fn smgr_desc(rec: &DecodedRecord) -> String {
    let m = &rec.main;
    let rel = format!(
        "{}/{}/{}",
        u32_at(m, 0).unwrap_or(0),
        u32_at(m, 4).unwrap_or(0),
        u32_at(m, 8).unwrap_or(0)
    );
    let fork = m.get(12).copied().unwrap_or(0);
    if rec.info == 0x10 {
        format!(
            "TRUNCATE {rel} fork {fork} to {} blocks",
            u32_at(m, 16).unwrap_or(0)
        )
    } else {
        format!("CREATE {rel} fork {fork}")
    }
}

fn heap_desc(rec: &DecodedRecord) -> String {
    let m = &rec.main;
    let init = if rec.info & 0x80 != 0 {
        " INIT_PAGE"
    } else {
        ""
    };
    match rec.info & 0x70 {
        0x00 => format!("INSERT off {}{init}", u16_at(m, 0).unwrap_or(0)),
        0x10 => format!(
            "DELETE off {} xmax {} cmax {} infomask 0x{:04X} infomask2 0x{:04X}",
            u16_at(m, 0).unwrap_or(0),
            u64_at(m, 8).unwrap_or(0),
            u32_at(m, 16).unwrap_or(0),
            u16_at(m, 2).unwrap_or(0),
            u16_at(m, 4).unwrap_or(0)
        ),
        0x20 => format!(
            "UPDATE old_off {} new_off {} old_xmax {}{}{init}",
            u16_at(m, 0).unwrap_or(0),
            u16_at(m, 2).unwrap_or(0),
            u64_at(m, 8).unwrap_or(0),
            if m.get(20).is_some_and(|f| f & 1 != 0) {
                " SAME_PAGE"
            } else {
                ""
            }
        ),
        t => format!("UNKNOWN 0x{t:02X}{init}"),
    }
}

fn desc(rec: &DecodedRecord) -> String {
    match rec.rmgr {
        RmgrId::Xlog => xlog_desc(rec),
        RmgrId::Xact => xact_desc(rec),
        RmgrId::Smgr => smgr_desc(rec),
        RmgrId::Heap => heap_desc(rec),
        // 各モジュールの `describe`（本実装は B1 / Q1）。
        RmgrId::Btree => crate::storage::btree::wal::describe(rec),
        RmgrId::Seq => crate::storage::sequence::describe(rec),
    }
}

/// 1 行の説明。例: `rmgr: Heap len: 112 xid: 1234 lsn: 0/01000120 desc: INSERT off 5 blkref #0: rel 1663/5/16384 blk 3`。
/// `len` は `end - start`（8 バイトに切り上げた長さ。`tot_len` はデコード後に残らない）。
pub fn format_record(rec: &DecodedRecord) -> String {
    format_record_with_prev(rec, None)
}

/// `prev`（直前のレコードの開始 LSN。`DecodedRecord` が持たないので呼び出し側が渡す）を含める版。
pub fn format_record_with_prev(rec: &DecodedRecord, prev: Option<Lsn>) -> String {
    let mut s = format!(
        "rmgr: {:?} len: {} xid: {} lsn: {}",
        rec.rmgr,
        rec.end.0 - rec.start.0,
        rec.xid.0,
        lsn_str(rec.start)
    );
    if let Some(p) = prev {
        let _ = write!(s, " prev: {}", lsn_str(p));
    }
    let _ = write!(s, " desc: {}", desc(rec));
    for b in &rec.blocks {
        let _ = write!(
            s,
            " blkref #{}: rel {}/{}/{} fork {:?} blk {}",
            b.id,
            b.tag.rel.spc_oid,
            b.tag.rel.db_oid,
            b.tag.rel.rel_number.0,
            b.tag.fork,
            b.tag.block
        );
        if b.image.is_some() {
            s.push_str(" FPW");
        }
        if b.will_init {
            s.push_str(" WILL_INIT");
        }
    }
    s
}

/// `--stats` 用の集計（rmgr ごとのレコード数・バイト数・FPI の個数）。
#[derive(Debug, Default)]
pub struct DumpStats {
    by_rmgr: BTreeMap<u8, (u64, u64, u64)>,
}

impl DumpStats {
    pub fn add(&mut self, rec: &DecodedRecord) {
        let e = self.by_rmgr.entry(rec.rmgr as u8).or_default();
        e.0 += 1;
        e.1 += rec.end.0 - rec.start.0;
        e.2 += rec.blocks.iter().filter(|b| b.image.is_some()).count() as u64;
    }

    pub fn format(&self) -> String {
        let mut s = String::new();
        for (id, (n, bytes, fpi)) in &self.by_rmgr {
            let name = RmgrId::from_u8(*id).map_or_else(|| id.to_string(), |r| format!("{r:?}"));
            let _ = writeln!(s, "{name:<6} records: {n} bytes: {bytes} fpi: {fpi}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::Page;
    use crate::storage::smgr::{BufferTag, ForkNumber, RelFileLocator, RelFileNumber};
    use crate::txn::Xid;
    use crate::wal::DecodedBlock;

    fn rec(rmgr: RmgrId, info: u8, main: Vec<u8>, blocks: Vec<DecodedBlock>) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(0x0100_0120),
            end: Lsn(0x0100_0190),
            xid: Xid(1234),
            rmgr,
            info,
            blocks,
            main,
        }
    }

    fn blk(image: bool) -> DecodedBlock {
        DecodedBlock {
            id: 0,
            tag: BufferTag {
                rel: RelFileLocator {
                    spc_oid: 1663,
                    db_oid: 5,
                    rel_number: RelFileNumber(16384),
                },
                fork: ForkNumber::Main,
                block: 3,
            },
            will_init: false,
            image: image.then(|| Box::new(Page::zeroed())),
            data: vec![],
        }
    }

    #[test]
    fn heap_insert_line() {
        let s = format_record_with_prev(
            &rec(RmgrId::Heap, 0x00, vec![5, 0, 0, 0], vec![blk(false)]),
            Some(Lsn(0x0100_00E8)),
        );
        assert_eq!(
            s,
            "rmgr: Heap len: 112 xid: 1234 lsn: 0/01000120 prev: 0/010000E8 desc: INSERT off 5 blkref #0: rel 1663/5/16384 fork Main blk 3"
        );
    }

    #[test]
    fn other_rmgrs_and_fpw() {
        let mut m = vec![0u8; 16];
        m[8] = 1;
        m.extend_from_slice(&[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
        let s = format_record(&rec(RmgrId::Xact, 0x10, m, vec![]));
        assert!(s.contains("ABORT") && s.contains("rels: 1/2/3"), "{s}");
        let s = format_record(&rec(
            RmgrId::Smgr,
            0x10,
            {
                let mut v = vec![0u8; 20];
                v[0] = 1;
                v[16] = 4;
                v
            },
            vec![],
        ));
        assert!(s.contains("TRUNCATE 1/0/0 fork 0 to 4 blocks"), "{s}");
        let s = format_record(&rec(RmgrId::Xlog, XLOG_FPI, vec![], vec![blk(true)]));
        assert!(s.contains("desc: FPI") && s.contains("FPW"), "{s}");
        let s = format_record(&rec(RmgrId::Heap, 0x80, vec![1, 0, 0, 0], vec![]));
        assert!(s.contains("INSERT off 1 INIT_PAGE"), "{s}");
    }

    #[test]
    fn truncated_main_data_does_not_panic() {
        for r in [RmgrId::Xlog, RmgrId::Xact, RmgrId::Smgr, RmgrId::Heap] {
            for info in [0x00u8, 0x10, 0x20, 0x30, 0x80] {
                let _ = format_record(&rec(r, info, vec![1], vec![]));
            }
        }
    }

    #[test]
    fn stats_aggregate() {
        let mut st = DumpStats::default();
        st.add(&rec(RmgrId::Heap, 0, vec![], vec![blk(true)]));
        st.add(&rec(RmgrId::Heap, 0, vec![], vec![]));
        let s = st.format();
        assert!(
            s.contains("Heap") && s.contains("records: 2") && s.contains("fpi: 1"),
            "{s}"
        );
    }
}
