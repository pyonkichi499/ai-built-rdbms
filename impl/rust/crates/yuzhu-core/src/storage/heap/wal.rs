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

/// Puts a tuple on the page (initialising it first when `init`) and checks that it
/// lands on the line pointer the record names.
fn redo_add_tuple(page: &mut Page, init: bool, data: &[u8], offnum: u16) -> Result<()> {
    if init {
        page.init_heap();
    }
    match page.add_item(data) {
        Some(off) if off == offnum => Ok(()),
        Some(off) => Err(corrupt(&format!(
            "tuple landed on offset {off}, the record says {offnum}"
        ))),
        None => Err(corrupt("tuple does not fit on the page")),
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
                redo_stamp(p, m.offnum, m.infomask, m.infomask2, m.xmax, m.cmax, None)
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
                )
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
        k => Err(corrupt(&format!("unknown HEAP info {k:#x}"))),
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
