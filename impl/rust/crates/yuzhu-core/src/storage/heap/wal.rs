//! HEAP rmgr: INSERT / DELETE / UPDATE の WAL と REDO（`m3.md` §3.9、§4.5、§5.1）。
//!
//! メインデータの `encode` / `decode`、`redo`。

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_pass_by_value
)]

use super::tuple::TupleHeader;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::page::Page;
use crate::txn::{CommandId, Xid};
use crate::types::Tid;
use crate::wal::{DecodedRecord, RedoBuffer, RedoCtx, RmgrId, read_buffer_for_redo};

pub const HEAP_INSERT: u8 = 0x00;
pub const HEAP_DELETE: u8 = 0x10;
pub const HEAP_UPDATE: u8 = 0x20;
/// 行ロック（`SELECT FOR ...` と外部キーの検査。M5 02 章 §3.6）。
pub const HEAP_LOCK: u8 = 0x40;
/// `info` に OR するフラグ。
pub const HEAP_INIT_PAGE: u8 = 0x80;

/// メインデータ（DELETE / UPDATE）の長さ。
pub const HEAP_MAIN_LEN: usize = 24;

fn corrupt(what: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid heap WAL record: {what}"),
    )
    .with_severity(Severity::Panic)
}

/// DELETE のメインデータ（変更後のヘッダの値をそのまま載せる）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapDeleteMain {
    pub offnum: u16,
    pub infomask: u16,
    pub infomask2: u16,
    pub xmax: Xid,
    pub cmax: CommandId,
}

/// UPDATE のメインデータ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapUpdateMain {
    pub old_offnum: u16,
    pub new_offnum: u16,
    pub old_infomask: u16,
    pub old_infomask2: u16,
    pub old_xmax: Xid,
    pub old_cmax: CommandId,
    /// blk1 がない（新旧が同じページ）。
    pub same_page: bool,
}

/// LOCK のメインデータ（02 章 §3.6）。変更後のヘッダの値をそのまま載せる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeapLockMain {
    pub offnum: u16,
    pub infomask: u16,
    pub infomask2: u16,
    /// 予約（読み手は検査しない）。
    pub flags: u8,
    /// `LockTupleMode`（0..=3）。REDO は使わない。MultiXact のときは最強のメンバーの強度。
    pub lock_mode: u8,
    /// 単独の XID、または `MultiXactId`。
    pub xmax: Xid,
}

impl HeapLockMain {
    pub fn encode(&self) -> [u8; HEAP_MAIN_LEN] {
        let mut b = [0u8; HEAP_MAIN_LEN];
        b[0..2].copy_from_slice(&self.offnum.to_le_bytes());
        b[2..4].copy_from_slice(&self.infomask.to_le_bytes());
        b[4..6].copy_from_slice(&self.infomask2.to_le_bytes());
        b[6] = self.flags;
        b[7] = self.lock_mode;
        b[8..16].copy_from_slice(&self.xmax.0.to_le_bytes());
        b
    }

    /// 長さが違う、または `lock_mode > 3` は `XX001`（Panic）。
    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != HEAP_MAIN_LEN {
            return Err(corrupt("LOCK main data has a bad length"));
        }
        if b[7] > 3 {
            return Err(corrupt("LOCK with lock_mode > 3"));
        }
        Ok(HeapLockMain {
            offnum: u16_at(b, 0),
            infomask: u16_at(b, 2),
            infomask2: u16_at(b, 4),
            flags: b[6],
            lock_mode: b[7],
            xmax: Xid(u64_at(b, 8)),
        })
    }

    /// `h` はロックを書いた後のヘッダ。
    pub fn from_header(offnum: u16, h: &TupleHeader, lock_mode: u8) -> Self {
        HeapLockMain {
            offnum,
            infomask: h.infomask,
            infomask2: h.infomask2,
            flags: 0,
            lock_mode,
            xmax: h.xmax,
        }
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or([0; 8]))
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or([0; 4]))
}

impl HeapDeleteMain {
    pub fn encode(&self) -> [u8; HEAP_MAIN_LEN] {
        let mut b = [0u8; HEAP_MAIN_LEN];
        b[0..2].copy_from_slice(&self.offnum.to_le_bytes());
        b[2..4].copy_from_slice(&self.infomask.to_le_bytes());
        b[4..6].copy_from_slice(&self.infomask2.to_le_bytes());
        b[8..16].copy_from_slice(&self.xmax.0.to_le_bytes());
        b[16..20].copy_from_slice(&self.cmax.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != HEAP_MAIN_LEN {
            return Err(corrupt("DELETE main data has a bad length"));
        }
        Ok(HeapDeleteMain {
            offnum: u16_at(b, 0),
            infomask: u16_at(b, 2),
            infomask2: u16_at(b, 4),
            xmax: Xid(u64_at(b, 8)),
            cmax: u32_at(b, 16),
        })
    }
}

impl HeapUpdateMain {
    pub fn encode(&self) -> [u8; HEAP_MAIN_LEN] {
        let mut b = [0u8; HEAP_MAIN_LEN];
        b[0..2].copy_from_slice(&self.old_offnum.to_le_bytes());
        b[2..4].copy_from_slice(&self.new_offnum.to_le_bytes());
        b[4..6].copy_from_slice(&self.old_infomask.to_le_bytes());
        b[6..8].copy_from_slice(&self.old_infomask2.to_le_bytes());
        b[8..16].copy_from_slice(&self.old_xmax.0.to_le_bytes());
        b[16..20].copy_from_slice(&self.old_cmax.to_le_bytes());
        b[20] = u8::from(self.same_page);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != HEAP_MAIN_LEN {
            return Err(corrupt("UPDATE main data has a bad length"));
        }
        Ok(HeapUpdateMain {
            old_offnum: u16_at(b, 0),
            new_offnum: u16_at(b, 2),
            old_infomask: u16_at(b, 4),
            old_infomask2: u16_at(b, 6),
            old_xmax: Xid(u64_at(b, 8)),
            old_cmax: u32_at(b, 16),
            same_page: b[20] & 1 != 0,
        })
    }
}

impl HeapDeleteMain {
    /// The values of the header just written (after stamping `xmax` / `cmax`).
    pub fn from_header(offnum: u16, h: &TupleHeader) -> Self {
        HeapDeleteMain {
            offnum,
            infomask: h.infomask,
            infomask2: h.infomask2,
            xmax: h.xmax,
            cmax: h.cmax,
        }
    }
}

impl HeapUpdateMain {
    /// `h` is the old version's header after stamping.
    pub fn from_header(old_offnum: u16, new_offnum: u16, h: &TupleHeader, same_page: bool) -> Self {
        HeapUpdateMain {
            old_offnum,
            new_offnum,
            old_infomask: h.infomask,
            old_infomask2: h.infomask2,
            old_xmax: h.xmax,
            old_cmax: h.cmax,
            same_page,
        }
    }
}

/// Runs `f` on block `id` when it needs REDO, then sets the page LSN. Any failure is a
/// `Panic` (m3.md §3 rule 4).
fn redo_block(
    ctx: &RedoCtx,
    rec: &DecodedRecord,
    id: u8,
    f: impl FnOnce(&mut Page) -> Result<()>,
) -> Result<()> {
    if let RedoBuffer::NeedsRedo(buf) = read_buffer_for_redo(ctx, rec, id)? {
        let mut g = buf.write()?;
        if let Err(e) = f(g.page_mut()) {
            // Keep the guard's "page_mut needs set_lsn" check quiet: this error stops the
            // startup, and the page is never written (the process ends).
            let lsn = g.page().lsn();
            g.set_lsn(lsn);
            return Err(e.with_severity(Severity::Panic));
        }
        g.set_lsn(rec.end.0);
    }
    Ok(())
}

/// Puts a tuple on the page (initialising it first when `init`) at the line pointer
/// the record names (`Page::add_item_at`: it must be `LP_UNUSED` or `max_offset + 1`).
fn redo_add_tuple(page: &mut Page, init: bool, data: &[u8], offnum: u16) -> Result<()> {
    if init {
        page.init_heap();
    }
    if page.add_item_at(offnum, data) {
        Ok(())
    } else {
        Err(corrupt(&format!(
            "tuple does not fit at offset {offnum} of the page"
        )))
    }
}

/// Rewrites the header of the old version. `new_ctid` is set for UPDATE only.
fn redo_stamp(
    page: &mut Page,
    off: u16,
    infomask: u16,
    infomask2: u16,
    xmax: Xid,
    cmax: CommandId,
    new_ctid: Option<Tid>,
) -> Result<()> {
    let item = page
        .item_mut(off)
        .map_err(|_| corrupt(&format!("no live tuple at offset {off}")))?;
    let mut h = TupleHeader::read(item)?;
    h.infomask = infomask;
    h.infomask2 = infomask2;
    h.xmax = xmax;
    h.cmax = cmax;
    if let Some(c) = new_ctid {
        h.ctid = c;
    }
    h.write(item);
    Ok(())
}

/// LOCK の REDO: xmax・infomask・infomask2 だけを書く（cmax と ctid は触らない）。
fn redo_lock_stamp(page: &mut Page, m: &HeapLockMain) -> Result<()> {
    let item = page
        .item_mut(m.offnum)
        .map_err(|_| corrupt(&format!("no live tuple at offset {}", m.offnum)))?;
    let mut h = TupleHeader::read(item)?;
    h.infomask = m.infomask;
    h.infomask2 = m.infomask2;
    h.xmax = m.xmax;
    h.write(item);
    Ok(())
}

fn check_shape(rec: &DecodedRecord, nblocks: &[usize], main_len: usize) -> Result<()> {
    if !nblocks.contains(&rec.blocks.len()) {
        return Err(corrupt(&format!(
            "{} block references at {}",
            rec.blocks.len(),
            rec.start
        )));
    }
    if rec.main.len() != main_len {
        return Err(corrupt(&format!("main data of {} bytes", rec.main.len())));
    }
    let init = rec.info & HEAP_INIT_PAGE != 0;
    if rec.blocks.first().is_some_and(|b| b.will_init != init) {
        return Err(corrupt("INIT_PAGE does not match WILL_INIT"));
    }
    Ok(())
}

/// HEAP rmgr の REDO（`m3.md` §6.4.4）。失敗を飲み込まない。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    if rec.rmgr != RmgrId::Heap {
        return Err(corrupt("HEAP redo called for a record of another rmgr"));
    }
    let init = rec.info & HEAP_INIT_PAGE != 0;
    match rec.info & 0x70 {
        HEAP_INSERT => {
            check_shape(rec, &[1], 4)?;
            let offnum = u16_at(&rec.main, 0);
            let data = &rec.blocks[0].data;
            redo_block(ctx, rec, 0, |p| redo_add_tuple(p, init, data, offnum))
        }
        HEAP_DELETE => {
            check_shape(rec, &[1], HEAP_MAIN_LEN)?;
            if init {
                return Err(corrupt("DELETE with INIT_PAGE"));
            }
            let m = HeapDeleteMain::decode(&rec.main)?;
            redo_block(ctx, rec, 0, |p| {
                redo_stamp(p, m.offnum, m.infomask, m.infomask2, m.xmax, m.cmax, None)?;
                p.set_prunable(rec.xid);
                Ok(())
            })
        }
        HEAP_UPDATE => {
            check_shape(rec, &[1, 2], HEAP_MAIN_LEN)?;
            let m = HeapUpdateMain::decode(&rec.main)?;
            if m.same_page != (rec.blocks.len() == 1) {
                return Err(corrupt("SAME_PAGE does not match the block references"));
            }
            let new_blk = rec.blocks[0].tag.block;
            let data = &rec.blocks[0].data;
            let ctid = Tid {
                block: new_blk,
                offset: m.new_offnum,
            };
            let stamp = |p: &mut Page| {
                redo_stamp(
                    p,
                    m.old_offnum,
                    m.old_infomask,
                    m.old_infomask2,
                    m.old_xmax,
                    m.old_cmax,
                    Some(ctid),
                )?;
                p.set_prunable(rec.xid);
                Ok(())
            };
            redo_block(ctx, rec, 0, |p| {
                redo_add_tuple(p, init, data, m.new_offnum)?;
                if m.same_page { stamp(p) } else { Ok(()) }
            })?;
            if !m.same_page {
                redo_block(ctx, rec, 1, stamp)?;
            }
            Ok(())
        }
        HEAP_LOCK => {
            check_shape(rec, &[1], HEAP_MAIN_LEN)?;
            if init {
                return Err(corrupt("LOCK with INIT_PAGE"));
            }
            let m = HeapLockMain::decode(&rec.main)?;
            redo_block(ctx, rec, 0, |p| redo_lock_stamp(p, &m))
        }
        k => Err(corrupt(&format!("unknown HEAP info {k:#x}"))),
    }
}

/// `yuzhu-waldump` 用の 1 行の説明。
pub fn describe(rec: &DecodedRecord) -> String {
    let init = if rec.info & HEAP_INIT_PAGE != 0 {
        " INIT_PAGE"
    } else {
        ""
    };
    match rec.info & 0x70 {
        HEAP_INSERT if rec.main.len() >= 2 => {
            format!("INSERT off {}{init}", u16_at(&rec.main, 0))
        }
        HEAP_DELETE => match HeapDeleteMain::decode(&rec.main) {
            Ok(m) => format!(
                "DELETE off {} xmax {} cmax {} infomask 0x{:04X} infomask2 0x{:04X}",
                m.offnum, m.xmax.0, m.cmax, m.infomask, m.infomask2
            ),
            Err(_) => "DELETE (malformed)".to_string(),
        },
        HEAP_UPDATE => match HeapUpdateMain::decode(&rec.main) {
            Ok(m) => format!(
                "UPDATE old_off {} new_off {} old_xmax {}{}{init}",
                m.old_offnum,
                m.new_offnum,
                m.old_xmax.0,
                if m.same_page { " SAME_PAGE" } else { "" }
            ),
            Err(_) => "UPDATE (malformed)".to_string(),
        },
        HEAP_LOCK => match HeapLockMain::decode(&rec.main) {
            Ok(m) => format!(
                "LOCK off {} xmax {} mode {} infomask 0x{:04X} infomask2 0x{:04X}",
                m.offnum,
                m.xmax.0,
                ["KeyShare", "Share", "NoKeyExclusive", "Exclusive"][usize::from(m.lock_mode)],
                m.infomask,
                m.infomask2
            ),
            Err(_) => "LOCK (malformed)".to_string(),
        },
        k => format!("UNKNOWN 0x{k:02X}{init}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_main_round_trips_and_layout_matches() {
        let m = HeapDeleteMain {
            offnum: 5,
            infomask: 0x0800,
            infomask2: 0x0001,
            xmax: Xid(0x1_0000_0001),
            cmax: 7,
        };
        let b = m.encode();
        assert_eq!(&b[0..2], &[5, 0]);
        assert_eq!(&b[8..16], &0x1_0000_0001u64.to_le_bytes());
        assert_eq!(&b[20..24], &[0; 4]);
        assert_eq!(HeapDeleteMain::decode(&b).unwrap(), m);
    }

    #[test]
    fn update_main_round_trips_and_layout_matches() {
        let m = HeapUpdateMain {
            old_offnum: 2,
            new_offnum: 9,
            old_infomask: 1,
            old_infomask2: 2,
            old_xmax: Xid(44),
            old_cmax: 3,
            same_page: true,
        };
        let b = m.encode();
        assert_eq!(b[20], 1);
        assert_eq!(&b[21..24], &[0; 3]);
        assert_eq!(HeapUpdateMain::decode(&b).unwrap(), m);
        let mut m2 = m;
        m2.same_page = false;
        assert!(!HeapUpdateMain::decode(&m2.encode()).unwrap().same_page);
    }

    #[test]
    fn bad_length_is_a_panic_level_error() {
        let e = HeapDeleteMain::decode(&[0; 23]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert!(HeapUpdateMain::decode(&[0; 25]).is_err());
    }

    #[test]
    fn init_page_flag_leaves_the_kind_nibble_alone() {
        for k in [HEAP_INSERT, HEAP_DELETE, HEAP_UPDATE] {
            assert_eq!((k | HEAP_INIT_PAGE) & 0x70, k & 0x70);
        }
    }

    #[test]
    fn insert_record_matches_the_documented_example() {
        use crate::storage::smgr::{BufferTag, ForkNumber, RelFileLocator, RelFileNumber};
        use crate::wal::record::encode_record;
        use crate::wal::{Lsn, RecordBuilder, RegFlags};

        let mut page = Page::zeroed();
        page.init_heap();
        let tag = BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(16384),
            },
            fork: ForkNumber::Main,
            block: 3,
        };
        let tuple = [0xABu8; 51];
        let mut rec = RecordBuilder::new(RmgrId::Heap, HEAP_INSERT, Xid(7));
        let b = rec.register_block(tag, &page, RegFlags::STANDARD);
        rec.block_data(b, &tuple);
        rec.main_data(&[5, 0, 0, 0]);
        let buf = encode_record(&rec, Lsn(u64::MAX), false).unwrap();
        assert_eq!(buf.len(), 111);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 111);
        assert_eq!(&buf[16..24], &7u64.to_le_bytes());
        assert_eq!(&buf[24..28], &[3, 0x00, 1, 0]);
        assert_eq!(&buf[28..32], &4u32.to_le_bytes());
        // block reference: id 0, HAS_DATA, main fork, (1663, 5, 16384), block 3, 51 bytes
        assert_eq!(&buf[32..34], &[0, 0x02]);
        assert_eq!(&buf[36..40], &1663u32.to_le_bytes());
        assert_eq!(&buf[44..48], &16384u32.to_le_bytes());
        assert_eq!(&buf[48..52], &3u32.to_le_bytes());
        assert_eq!(&buf[52..56], &51u32.to_le_bytes());
        assert_eq!(&buf[56..107], &tuple);
        assert_eq!(&buf[107..111], &[5, 0, 0, 0]);
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use crate::storage::smgr::{BufferTag, ForkNumber, RelFileLocator, RelFileNumber};
    use crate::wal::record::encode_record;
    use crate::wal::{DecodedBlock, Lsn, RecordBuilder, RegFlags};

    fn tag() -> BufferTag {
        BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(16384),
            },
            fork: ForkNumber::Main,
            block: 3,
        }
    }

    fn share_main() -> HeapLockMain {
        HeapLockMain {
            offnum: 5,
            infomask: 0x00D3,
            infomask2: 0x0004,
            flags: 0,
            lock_mode: 1,
            xmax: Xid(0x0102),
        }
    }

    fn decoded(info: u8, main: Vec<u8>, will_init: bool) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(0x0100_0120),
            end: Lsn(0x0100_0170),
            xid: Xid(0x0102),
            rmgr: RmgrId::Heap,
            info,
            blocks: vec![DecodedBlock {
                id: 0,
                tag: tag(),
                will_init,
                image: None,
                data: vec![],
            }],
            main,
        }
    }

    fn page_with_tuple() -> Page {
        let mut page = Page::zeroed();
        page.init_heap();
        let mut t = vec![0u8; 48];
        TupleHeader {
            xmin: Xid(100),
            xmax: Xid::INVALID,
            cmin: 2,
            cmax: 9,
            ctid: Tid {
                block: 3,
                offset: 1,
            },
            infomask2: 0x0004,
            infomask: 0x0800 | 0x0100,
            hoff: 40,
        }
        .write(&mut t);
        assert!(page.add_item_at(1, &t));
        page
    }

    #[test]
    fn main_round_trips_and_layout_matches() {
        let b = share_main().encode();
        assert_eq!(
            b[..16],
            [5, 0, 0xD3, 0, 4, 0, 0, 1, 0x02, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&b[16..], &[0; 8]);
        assert_eq!(HeapLockMain::decode(&b).unwrap(), share_main());
    }

    #[test]
    fn lock_mode_above_3_and_bad_length_are_panic_errors() {
        let mut b = share_main().encode();
        b[7] = 4;
        let e = HeapLockMain::decode(&b).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert!(HeapLockMain::decode(&b[..23]).is_err());
        // Reserved bytes may be non-zero.
        let mut b = share_main().encode();
        b[6] = 0xFF;
        b[20] = 7;
        assert!(HeapLockMain::decode(&b).is_ok());
    }

    #[test]
    fn record_matches_the_documented_example() {
        let page = Page::zeroed();
        let mut rec = RecordBuilder::new(RmgrId::Heap, HEAP_LOCK, Xid(0x0102));
        rec.register_block(tag(), &page, RegFlags::STANDARD);
        rec.main_data(&share_main().encode());
        let buf = encode_record(&rec, Lsn(u64::MAX), false).unwrap();
        assert_eq!(buf.len(), 80);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 80);
        assert_eq!(&buf[16..24], &0x0102u64.to_le_bytes());
        assert_eq!(&buf[24..28], &[3, 0x40, 1, 0]);
        assert_eq!(&buf[28..32], &24u32.to_le_bytes());
        // id 0, flags 0 (no data, no image), main fork, (1663, 5, 16384), block 3, 0 bytes.
        assert_eq!(&buf[32..34], &[0, 0x00]);
        assert_eq!(&buf[36..40], &1663u32.to_le_bytes());
        assert_eq!(&buf[44..48], &16384u32.to_le_bytes());
        assert_eq!(&buf[48..52], &3u32.to_le_bytes());
        assert_eq!(&buf[52..56], &0u32.to_le_bytes());
        assert_eq!(
            &buf[56..72],
            &[5, 0, 0xD3, 0, 4, 0, 0, 1, 0x02, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&buf[72..80], &[0; 8]);
    }

    #[test]
    fn redo_stamp_writes_only_xmax_and_masks_and_is_idempotent() {
        let mut page = page_with_tuple();
        let before = TupleHeader::read(page.item(1).unwrap()).unwrap();
        let m = HeapLockMain {
            offnum: 1,
            ..share_main()
        };
        redo_lock_stamp(&mut page, &m).unwrap();
        let once = page.clone();
        let h = TupleHeader::read(page.item(1).unwrap()).unwrap();
        assert_eq!(h.xmax, Xid(0x0102));
        assert_eq!(h.infomask, 0x00D3);
        assert_eq!(h.infomask2, 0x0004);
        assert_eq!(
            (h.xmin, h.cmin, h.cmax, h.ctid),
            (before.xmin, before.cmin, before.cmax, before.ctid)
        );
        redo_lock_stamp(&mut page, &m).unwrap();
        assert_eq!(page.0[..], once.0[..]);
    }

    #[test]
    fn redo_stamp_on_a_missing_tuple_is_a_panic_error() {
        let mut page = page_with_tuple();
        let m = HeapLockMain {
            offnum: 9,
            ..share_main()
        };
        assert_eq!(
            redo_lock_stamp(&mut page, &m).unwrap_err().severity,
            Severity::Panic
        );
    }

    #[test]
    fn shape_checks_reject_init_page_and_bad_main() {
        let ok = decoded(HEAP_LOCK, share_main().encode().to_vec(), false);
        assert!(check_shape(&ok, &[1], HEAP_MAIN_LEN).is_ok());
        let init = decoded(
            HEAP_LOCK | HEAP_INIT_PAGE,
            share_main().encode().to_vec(),
            true,
        );
        // The shape is consistent, so REDO itself refuses INIT_PAGE on LOCK.
        assert!(check_shape(&init, &[1], HEAP_MAIN_LEN).is_ok());
        let short = decoded(HEAP_LOCK, vec![0; 23], false);
        assert!(check_shape(&short, &[1], HEAP_MAIN_LEN).is_err());
    }

    #[test]
    fn describe_shows_lock_fields() {
        let r = decoded(HEAP_LOCK, share_main().encode().to_vec(), false);
        assert_eq!(
            describe(&r),
            "LOCK off 5 xmax 258 mode Share infomask 0x00D3 infomask2 0x0004"
        );
        let bad = decoded(HEAP_LOCK, vec![0; 3], false);
        assert_eq!(describe(&bad), "LOCK (malformed)");
        let del = decoded(
            HEAP_DELETE,
            HeapDeleteMain {
                offnum: 2,
                infomask: 0x0800,
                infomask2: 1,
                xmax: Xid(7),
                cmax: 3,
            }
            .encode()
            .to_vec(),
            false,
        );
        assert_eq!(
            describe(&del),
            "DELETE off 2 xmax 7 cmax 3 infomask 0x0800 infomask2 0x0001"
        );
    }
}
