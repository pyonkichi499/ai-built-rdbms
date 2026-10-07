//! レコードのエンコード・デコードと CRC（`m3.md` §3.3、§4.3、§6.3.1）。
//!
//! エンコードは 2 段階。[`encode_record`] が `prev = 0`・`crc = 0` のバイト列を作り（FPW の
//! 判定と画像のコピーをここで行う）、[`seal_record`] が `prev` を書いて CRC を計算する。
//! SWITCH を置くかどうかは長さだけで決まるので、`prev` の確定は位置の決定のあとでよい。

#![allow(clippy::cast_possible_truncation, clippy::doc_markdown)]

use super::{
    BLOCK_REF_HEADER_SIZE, DecodedBlock, DecodedRecord, Lsn, MAX_BLOCK_REFS, MAX_RECORD_LEN,
    RECORD_HEADER_SIZE, RegFlags, RmgrId,
};
use crate::error::{Error, Result};
use crate::storage::page::Page;
use crate::storage::smgr::{BufferTag, ForkNumber, RelFileLocator, RelFileNumber};
use crate::storage::{BLCKSZ, SIZE_OF_PAGE_HEADER};
use crate::txn::Xid;
use crate::util::crc32c::{CRC32C_INIT, crc32c_append, crc32c_finish};

const BLK_HAS_IMAGE: u8 = 0x01;
const BLK_HAS_DATA: u8 = 0x02;
const BLK_WILL_INIT: u8 = 0x04;
const BLK_HAS_HOLE: u8 = 0x08;
const BLK_KNOWN_FLAGS: u8 = 0x0F;

/// 画像のある参照の固定部（24 + hole_offset + hole_length）。
const IMAGE_EXTRA: usize = 4;

/// 登録されたブロック 1 つ分。
#[derive(Debug)]
pub struct BlockReg<'a> {
    pub tag: BufferTag,
    pub page: &'a Page,
    pub flags: RegFlags,
    pub data: Vec<u8>,
}

/// `XLogBeginInsert`〜`XLogRegister*` に相当する組み立て器。スタックに作って使い捨てる。
#[derive(Debug)]
pub struct RecordBuilder<'a> {
    pub rmgr: RmgrId,
    pub info: u8,
    pub xid: Xid,
    pub blocks: Vec<BlockReg<'a>>,
    pub main: Vec<u8>,
}

impl<'a> RecordBuilder<'a> {
    pub fn new(rmgr: RmgrId, info: u8, xid: Xid) -> Self {
        RecordBuilder {
            rmgr,
            info,
            xid,
            blocks: Vec::new(),
            main: Vec::new(),
        }
    }

    /// ページを登録して `block_id` を返す（登録順に 0, 1, ...）。`page` は呼び出し側が
    /// 排他ラッチで持っているもの。`MAX_BLOCK_REFS` を超えたら `debug_assert` で落とし、
    /// リリースでは `insert` が内部エラーを返す。同じ `tag` の二重登録も `debug_assert` で落とす。
    pub fn register_block(&mut self, tag: BufferTag, page: &'a Page, flags: RegFlags) -> u8 {
        debug_assert!(self.blocks.len() < MAX_BLOCK_REFS);
        debug_assert!(
            self.blocks.iter().all(|b| b.tag != tag),
            "block registered twice: {tag:?}"
        );
        self.blocks.push(BlockReg {
            tag,
            page,
            flags,
            data: Vec::new(),
        });
        u8::try_from(self.blocks.len() - 1).unwrap_or(u8::MAX)
    }

    /// ブロックの差分データを足す。
    pub fn block_data(&mut self, id: u8, bytes: &[u8]) {
        if let Some(b) = self.blocks.get_mut(usize::from(id)) {
            b.data.extend_from_slice(bytes);
        } else {
            debug_assert!(false, "unknown block id {id}");
        }
    }

    /// メインデータを足す。
    pub fn main_data(&mut self, bytes: &[u8]) {
        self.main.extend_from_slice(bytes);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    TooShort,
    BadLength,
    BadCrc,
    BadRmgr,
    BadInfo,
    BadBlockRef(&'static str),
    BadPrev,
}

// ----- エンコード -------------------------------------------------------------

/// 標準のページの穴 `(offset, length)`。形が不正、または長さ 0 なら `None`。
fn hole_range(page: &Page) -> Option<(usize, usize)> {
    let lower = usize::from(page.lower());
    let upper = usize::from(page.upper());
    (SIZE_OF_PAGE_HEADER <= lower && lower < upper && upper <= BLCKSZ)
        .then(|| (lower, upper - lower))
}

/// `prev = 0`・`crc = 0` のレコードのバイト列（`tot_len` ちょうど。パディングなし）を作る。
/// 画像を付けるかの判定には `redo` と `full_page_writes` を使う（`m3.md` §6.3.1）。
/// `tot_len > MAX_RECORD_LEN` やブロック数超過は内部エラー。
pub fn encode_record(
    rec: &RecordBuilder<'_>,
    redo: Lsn,
    full_page_writes: bool,
) -> Result<Vec<u8>> {
    if rec.blocks.len() > MAX_BLOCK_REFS {
        return Err(Error::internal(format!(
            "too many block references in WAL record: {}",
            rec.blocks.len()
        )));
    }
    let mut est = RECORD_HEADER_SIZE + rec.main.len();
    for b in &rec.blocks {
        est += BLOCK_REF_HEADER_SIZE + IMAGE_EXTRA + BLCKSZ + b.data.len();
    }
    // 見積もりは上限なので、超えても実際には収まることがある。実長は組み立てたあとで検査する。
    let mut out = Vec::with_capacity(est.min(MAX_RECORD_LEN + 1));
    out.extend_from_slice(&[0u8; 8]); // tot_len, crc
    out.extend_from_slice(&0u64.to_le_bytes()); // prev
    out.extend_from_slice(&rec.xid.0.to_le_bytes());
    out.push(rec.rmgr as u8);
    out.push(rec.info);
    out.push(rec.blocks.len() as u8);
    out.push(0);
    let main_len = u32::try_from(rec.main.len())
        .map_err(|_| Error::internal("WAL record main data too long"))?;
    out.extend_from_slice(&main_len.to_le_bytes());

    for (id, b) in rec.blocks.iter().enumerate() {
        let will_init = b.flags.contains(RegFlags::WILL_INIT);
        let need_image = !will_init
            && (b.flags.contains(RegFlags::FORCE_IMAGE)
                || (!b.flags.contains(RegFlags::NO_IMAGE)
                    && full_page_writes
                    && b.page.lsn() <= redo.0));
        let hole = if need_image && b.flags.contains(RegFlags::STANDARD) {
            hole_range(b.page)
        } else {
            None
        };
        let with_data =
            (!need_image || b.flags.contains(RegFlags::KEEP_DATA)) && !b.data.is_empty();

        let mut flags = 0u8;
        if need_image {
            flags |= BLK_HAS_IMAGE;
        }
        if with_data {
            flags |= BLK_HAS_DATA;
        }
        if will_init {
            flags |= BLK_WILL_INIT;
        }
        if hole.is_some() {
            flags |= BLK_HAS_HOLE;
        }
        let data_len = if with_data { b.data.len() } else { 0 };
        let data_len32 =
            u32::try_from(data_len).map_err(|_| Error::internal("WAL block data too long"))?;

        out.push(id as u8);
        out.push(flags);
        out.push(b.tag.fork as u8);
        out.push(0);
        out.extend_from_slice(&b.tag.rel.spc_oid.to_le_bytes());
        out.extend_from_slice(&b.tag.rel.db_oid.to_le_bytes());
        out.extend_from_slice(&b.tag.rel.rel_number.0.to_le_bytes());
        out.extend_from_slice(&b.tag.block.to_le_bytes());
        out.extend_from_slice(&data_len32.to_le_bytes());
        if need_image {
            let (ho, hl) = hole.unwrap_or((0, 0));
            out.extend_from_slice(&(ho as u16).to_le_bytes());
            out.extend_from_slice(&(hl as u16).to_le_bytes());
            out.extend_from_slice(&b.page.0[..ho_or_all(hole)]);
            if let Some((ho, hl)) = hole {
                out.extend_from_slice(&b.page.0[ho + hl..]);
            }
        }
        if with_data {
            out.extend_from_slice(&b.data);
        }
        if out.len() > MAX_RECORD_LEN {
            break;
        }
    }
    out.extend_from_slice(&rec.main);
    if out.len() > MAX_RECORD_LEN {
        return Err(Error::internal(format!(
            "WAL record too long: {} bytes (max {MAX_RECORD_LEN})",
            out.len()
        )));
    }
    let tot_len = out.len() as u32;
    out[0..4].copy_from_slice(&tot_len.to_le_bytes());
    Ok(out)
}

/// 画像の前半（穴の手前、穴がなければページ全体）の長さ。
fn ho_or_all(hole: Option<(usize, usize)>) -> usize {
    hole.map_or(BLCKSZ, |(ho, _)| ho)
}

/// `prev` を書き、CRC を計算して埋める。`buf` は [`encode_record`] の戻り値（パディングなし）。
/// CRC の計算順は「本体 `[8, tot_len)` → ヘッダ `[0, 4)`」。
pub fn seal_record(buf: &mut [u8], prev: Lsn) {
    buf[8..16].copy_from_slice(&prev.0.to_le_bytes());
    let crc = compute_crc(buf);
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
}

fn compute_crc(buf: &[u8]) -> u32 {
    let mut st = crc32c_append(CRC32C_INIT, &buf[8..]);
    st = crc32c_append(st, &buf[0..4]);
    crc32c_finish(st)
}

/// 封をしたレコードの `prev`（reader が PrevMismatch の判定に使う）。`buf` は 16 バイト以上。
pub fn record_prev(buf: &[u8]) -> Lsn {
    Lsn(u64::from_le_bytes(buf[8..16].try_into().expect("8 bytes")))
}

/// ヘッダの `tot_len`。`buf` は 4 バイト以上。
pub fn record_tot_len(buf: &[u8]) -> u32 {
    u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes"))
}

// ----- デコード ---------------------------------------------------------------

/// rmgr ごとに有効な `info`（`m3.md` §3.5）。予約値は不正。
fn info_is_valid(rmgr: RmgrId, info: u8) -> bool {
    if info & 0x0F != 0 {
        return false;
    }
    match rmgr {
        RmgrId::Xlog => matches!(info, 0x00 | 0x10 | 0x20 | 0x30 | 0x40 | 0x50),
        RmgrId::Xact | RmgrId::Smgr => matches!(info, 0x00 | 0x10),
        RmgrId::Heap => matches!(info & 0x7F, 0x00 | 0x10 | 0x20),
        // BTREE_INSERT_LEAF = 0x00、BTREE_PAGES = 0x10、SEQ_LOG = 0x00（00 §13.4）。
        RmgrId::Btree => matches!(info, 0x00 | 0x10),
        RmgrId::Seq => info == 0x00,
    }
}

fn fork_from_u8(v: u8) -> Option<ForkNumber> {
    match v {
        0 => Some(ForkNumber::Main),
        1 => Some(ForkNumber::Fsm),
        2 => Some(ForkNumber::VisibilityMap),
        3 => Some(ForkNumber::Init),
        _ => None,
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().expect("8 bytes"))
}

/// デコード（reader と単体テストが使う）。長さ・フラグ・CRC の検査をすべて行う。
///
/// `buf` は `start` に置かれたレコードの先頭から（`tot_len` 以上のバイトを含むこと）。
/// `prev` が 0 でなく `start` 以上なら `BadPrev`（直前のレコードとの一致は reader が確かめる）。
#[allow(clippy::too_many_lines)]
pub fn decode_record(buf: &[u8], start: Lsn) -> std::result::Result<DecodedRecord, RecordError> {
    if buf.len() < RECORD_HEADER_SIZE {
        return Err(RecordError::TooShort);
    }
    let tot_len = u32_at(buf, 0) as usize;
    if !(RECORD_HEADER_SIZE..=MAX_RECORD_LEN).contains(&tot_len) {
        return Err(RecordError::BadLength);
    }
    if buf.len() < tot_len {
        return Err(RecordError::TooShort);
    }
    let buf = &buf[..tot_len];
    if compute_crc(buf) != u32_at(buf, 4) {
        return Err(RecordError::BadCrc);
    }
    let prev = u64_at(buf, 8);
    if prev != 0 && prev >= start.0 {
        return Err(RecordError::BadPrev);
    }
    let xid = Xid(u64_at(buf, 16));
    let rmgr = RmgrId::from_u8(buf[24]).ok_or(RecordError::BadRmgr)?;
    let info = buf[25];
    if !info_is_valid(rmgr, info) || buf[27] != 0 {
        return Err(RecordError::BadInfo);
    }
    let nblocks = usize::from(buf[26]);
    if nblocks > MAX_BLOCK_REFS {
        return Err(RecordError::BadBlockRef("nblocks"));
    }
    let main_len = u32_at(buf, 28) as usize;

    let mut pos = RECORD_HEADER_SIZE;
    let mut blocks = Vec::with_capacity(nblocks);
    for idx in 0..nblocks {
        if tot_len - pos < BLOCK_REF_HEADER_SIZE {
            return Err(RecordError::BadBlockRef("truncated reference"));
        }
        let h = &buf[pos..];
        let id = h[0];
        let flags = h[1];
        if usize::from(id) != idx {
            return Err(RecordError::BadBlockRef("block_id"));
        }
        if flags & !BLK_KNOWN_FLAGS != 0 {
            return Err(RecordError::BadBlockRef("flags"));
        }
        let has_image = flags & BLK_HAS_IMAGE != 0;
        let has_data = flags & BLK_HAS_DATA != 0;
        let will_init = flags & BLK_WILL_INIT != 0;
        let has_hole = flags & BLK_HAS_HOLE != 0;
        if will_init && has_image {
            return Err(RecordError::BadBlockRef("WILL_INIT with image"));
        }
        if has_hole && !has_image {
            return Err(RecordError::BadBlockRef("hole without image"));
        }
        let fork = fork_from_u8(h[2]).ok_or(RecordError::BadBlockRef("fork"))?;
        if h[3] != 0 {
            return Err(RecordError::BadBlockRef("reserved"));
        }
        let tag = BufferTag {
            rel: RelFileLocator {
                spc_oid: u32_at(h, 4),
                db_oid: u32_at(h, 8),
                rel_number: RelFileNumber(u32_at(h, 12)),
            },
            fork,
            block: u32_at(h, 16),
        };
        let data_len = u32_at(h, 20) as usize;
        if !has_data && data_len != 0 {
            return Err(RecordError::BadBlockRef("data_len without HAS_DATA"));
        }
        pos += BLOCK_REF_HEADER_SIZE;

        let mut image = None;
        if has_image {
            if tot_len - pos < IMAGE_EXTRA {
                return Err(RecordError::BadBlockRef("truncated image header"));
            }
            let ho = usize::from(u16_at(buf, pos));
            let hl = usize::from(u16_at(buf, pos + 2));
            pos += IMAGE_EXTRA;
            if has_hole {
                if ho < SIZE_OF_PAGE_HEADER || ho + hl > BLCKSZ {
                    return Err(RecordError::BadBlockRef("hole range"));
                }
            } else if ho != 0 || hl != 0 {
                return Err(RecordError::BadBlockRef("hole without HAS_HOLE"));
            }
            let stored = BLCKSZ - hl;
            if tot_len - pos < stored {
                return Err(RecordError::BadBlockRef("truncated image"));
            }
            let mut page = Box::new(Page::zeroed());
            if has_hole {
                page.0[..ho].copy_from_slice(&buf[pos..pos + ho]);
                page.0[ho + hl..].copy_from_slice(&buf[pos + ho..pos + stored]);
            } else {
                page.0.copy_from_slice(&buf[pos..pos + BLCKSZ]);
            }
            pos += stored;
            image = Some(page);
        }
        if tot_len - pos < data_len {
            return Err(RecordError::BadBlockRef("truncated data"));
        }
        let data = buf[pos..pos + data_len].to_vec();
        pos += data_len;
        blocks.push(DecodedBlock {
            id,
            tag,
            will_init,
            image,
            data,
        });
    }
    if tot_len - pos != main_len {
        return Err(RecordError::BadLength);
    }
    let main = buf[pos..].to_vec();
    Ok(DecodedRecord {
        start,
        end: Lsn(start.0 + super::segment::align8(tot_len as u64)),
        xid,
        rmgr,
        info,
        blocks,
        main,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::{ForkNumber, RelFileLocator, RelFileNumber};

    fn tag(block: u32) -> BufferTag {
        BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(16384),
            },
            fork: ForkNumber::Main,
            block,
        }
    }

    fn sealed(rec: &RecordBuilder<'_>, redo: u64, fpw: bool, prev: u64) -> Vec<u8> {
        let mut b = encode_record(rec, Lsn(redo), fpw).unwrap();
        seal_record(&mut b, Lsn(prev));
        b
    }

    /// 修正したバイト列の CRC を計算し直す。
    fn reseal(b: &mut [u8]) {
        let crc = compute_crc(b);
        b[4..8].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn builder_assigns_sequential_block_ids_and_collects_data() {
        let p0 = Page([0; BLCKSZ]);
        let p1 = Page([1; BLCKSZ]);
        let mut b = RecordBuilder::new(RmgrId::Heap, 0x20, Xid(7));
        let a = b.register_block(tag(3), &p0, RegFlags::STANDARD);
        let c = b.register_block(tag(4), &p1, RegFlags::STANDARD | RegFlags::WILL_INIT);
        assert_eq!((a, c), (0, 1));
        b.block_data(1, &[1, 2]);
        b.block_data(1, &[3]);
        b.main_data(&[9, 9]);
        assert_eq!(b.blocks[1].data, vec![1, 2, 3]);
        assert_eq!(b.main, vec![9, 9]);
        assert_eq!(b.blocks[0].tag.block, 3);
    }

    /// m3.md §3.4 の HEAP_INSERT（FPW なし）。CRC 以外はバイト列で固定する。
    #[test]
    fn heap_insert_example_bytes() {
        let mut page = Page::zeroed();
        page.init_heap();
        page.set_lsn(0x5000); // redo より新しい → 画像なし
        let tuple: Vec<u8> = (0..51u8).collect();
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(9));
        let id = rb.register_block(tag(3), &page, RegFlags::STANDARD);
        rb.block_data(id, &tuple);
        rb.main_data(&[5, 0, 0, 0]);
        let b = sealed(&rb, 0x1000, true, 0x2000);
        assert_eq!(b.len(), 111);
        assert_eq!(u32_at(&b, 0), 111);
        assert_eq!(u64_at(&b, 8), 0x2000);
        assert_eq!(u64_at(&b, 16), 9);
        assert_eq!(&b[24..32], &[3, 0, 1, 0, 4, 0, 0, 0]);
        let mut blk = vec![0u8, 0x02, 0, 0];
        blk.extend_from_slice(&1663u32.to_le_bytes());
        blk.extend_from_slice(&5u32.to_le_bytes());
        blk.extend_from_slice(&16384u32.to_le_bytes());
        blk.extend_from_slice(&3u32.to_le_bytes());
        blk.extend_from_slice(&51u32.to_le_bytes());
        assert_eq!(&b[32..56], &blk[..]);
        assert_eq!(&b[56..107], &tuple[..]);
        assert_eq!(&b[107..111], &[5, 0, 0, 0]);
        // 終端 LSN は 112 に切り上がる。
        let d = decode_record(&b, Lsn(0x3000)).unwrap();
        assert_eq!(d.end, Lsn(0x3000 + 112));
        assert_eq!(d.xid, Xid(9));
        assert_eq!(d.rmgr, RmgrId::Heap);
        assert_eq!(d.blocks.len(), 1);
        assert_eq!(d.blocks[0].tag, tag(3));
        assert!(d.blocks[0].image.is_none());
        assert_eq!(d.blocks[0].data, tuple);
        assert_eq!(d.main, vec![5, 0, 0, 0]);
    }

    #[test]
    fn fpw_decision_follows_redo_and_flags() {
        let mut page = Page::zeroed();
        page.init_heap();
        page.set_lsn(0x1000);
        let mk = |flags: RegFlags, redo: u64, fpw: bool| {
            let mut rb = RecordBuilder::new(RmgrId::Heap, 0x10, Xid(1));
            let id = rb.register_block(tag(0), &page, flags);
            rb.block_data(id, &[1, 2, 3]);
            let b = sealed(&rb, redo, fpw, 0);
            decode_record(&b, Lsn(0x9000)).unwrap()
        };
        // page.lsn <= redo → 画像あり、差分なし。
        let d = mk(RegFlags::STANDARD, 0x1000, true);
        assert!(d.blocks[0].image.is_some());
        assert!(d.blocks[0].data.is_empty());
        // page.lsn > redo → 画像なし。
        let d = mk(RegFlags::STANDARD, 0x0FFF, true);
        assert!(d.blocks[0].image.is_none());
        assert_eq!(d.blocks[0].data, vec![1, 2, 3]);
        // FPW 無効。
        assert!(
            mk(RegFlags::STANDARD, 0x1000, false).blocks[0]
                .image
                .is_none()
        );
        // NO_IMAGE。
        let d = mk(RegFlags::STANDARD | RegFlags::NO_IMAGE, 0x1000, true);
        assert!(d.blocks[0].image.is_none());
        // FORCE_IMAGE は redo と無関係。
        let d = mk(RegFlags::STANDARD | RegFlags::FORCE_IMAGE, 0, false);
        assert!(d.blocks[0].image.is_some());
        // KEEP_DATA は画像があっても差分を残す。
        let d = mk(RegFlags::STANDARD | RegFlags::KEEP_DATA, 0x1000, true);
        assert!(d.blocks[0].image.is_some());
        assert_eq!(d.blocks[0].data, vec![1, 2, 3]);
        // WILL_INIT は画像を付けない。
        let d = mk(RegFlags::STANDARD | RegFlags::WILL_INIT, 0x1000, true);
        assert!(d.blocks[0].image.is_none());
        assert!(d.blocks[0].will_init);
    }

    #[test]
    fn image_round_trips_with_and_without_hole() {
        let mut page = Page::zeroed();
        page.init_heap();
        page.add_item(&[7u8; 40]).unwrap();
        page.add_item(&[8u8; 100]).unwrap();
        page.set_lsn(1);
        let (ho, hl) = hole_range(&page).unwrap();
        assert!(hl > 0);

        let mut rb = RecordBuilder::new(RmgrId::Xlog, 0x40, Xid(0));
        rb.register_block(tag(1), &page, RegFlags::STANDARD | RegFlags::FORCE_IMAGE);
        let b = sealed(&rb, 100, true, 0);
        assert_eq!(b.len(), 32 + 28 + (BLCKSZ - hl));
        assert_eq!(b[33] & BLK_HAS_HOLE, BLK_HAS_HOLE);
        assert_eq!(u16_at(&b, 56) as usize, ho);
        let d = decode_record(&b, Lsn(0x100)).unwrap();
        assert_eq!(d.blocks[0].image.as_ref().unwrap().0[..], page.0[..]);

        // STANDARD でなければ穴なし。
        let mut rb = RecordBuilder::new(RmgrId::Xlog, 0x40, Xid(0));
        rb.register_block(tag(1), &page, RegFlags::FORCE_IMAGE);
        let b = sealed(&rb, 100, true, 0);
        assert_eq!(b.len(), 32 + 28 + BLCKSZ);
        let d = decode_record(&b, Lsn(0x100)).unwrap();
        assert_eq!(d.blocks[0].image.as_ref().unwrap().0[..], page.0[..]);

        // 形が不正なページ（lower > upper）は穴なしで全体。
        let mut bad = Page::zeroed();
        bad.0[12..14].copy_from_slice(&5000u16.to_le_bytes());
        bad.0[14..16].copy_from_slice(&100u16.to_le_bytes());
        bad.0[5000] = 0xAB;
        assert!(hole_range(&bad).is_none());
        let mut rb = RecordBuilder::new(RmgrId::Xlog, 0x40, Xid(0));
        rb.register_block(tag(1), &bad, RegFlags::STANDARD | RegFlags::FORCE_IMAGE);
        let b = sealed(&rb, 100, true, 0);
        let d = decode_record(&b, Lsn(0x100)).unwrap();
        assert_eq!(d.blocks[0].image.as_ref().unwrap().0[..], bad.0[..]);

        // 長さ 0 の穴（空のページ全体が埋まっている）は HAS_HOLE を立てない。
        let mut full = Page::zeroed();
        full.0[12..14].copy_from_slice(&200u16.to_le_bytes());
        full.0[14..16].copy_from_slice(&200u16.to_le_bytes());
        assert!(hole_range(&full).is_none());
    }

    #[test]
    fn multiple_blocks_round_trip() {
        let p = Page::zeroed();
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x80, Xid(3));
        for i in 0..5u32 {
            let id = rb.register_block(tag(i), &p, RegFlags::NO_IMAGE);
            rb.block_data(id, &vec![i as u8; i as usize]);
        }
        rb.main_data(b"main");
        let b = sealed(&rb, 0, true, 0x10);
        let d = decode_record(&b, Lsn(0x20)).unwrap();
        assert_eq!(d.blocks.len(), 5);
        for (i, blk) in d.blocks.iter().enumerate() {
            assert_eq!(usize::from(blk.id), i);
            assert_eq!(blk.tag.block, i as u32);
            assert_eq!(blk.data, vec![i as u8; i]);
        }
        assert_eq!(d.main, b"main");
        assert_eq!(d.info, 0x80);
    }

    #[test]
    fn header_only_record_is_32_bytes() {
        let rb = RecordBuilder::new(RmgrId::Xlog, 0x30, Xid(0));
        let b = sealed(&rb, 0, true, 0x18);
        assert_eq!(b.len(), 32);
        let d = decode_record(&b, Lsn(0x40)).unwrap();
        assert_eq!(d.end, Lsn(0x60));
        assert_eq!(record_prev(&b), Lsn(0x18));
        assert_eq!(record_tot_len(&b), 32);
    }

    #[test]
    fn decode_rejects_bad_input() {
        let p = Page::zeroed();
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        let id = rb.register_block(tag(0), &p, RegFlags::NO_IMAGE);
        rb.block_data(id, &[1, 2, 3, 4]);
        rb.main_data(&[1, 2]);
        let good = sealed(&rb, 0, true, 0x10);
        let start = Lsn(0x100);
        assert!(decode_record(&good, start).is_ok());

        assert_eq!(
            decode_record(&good[..20], start).unwrap_err(),
            RecordError::TooShort
        );
        assert_eq!(
            decode_record(&good[..good.len() - 1], start).unwrap_err(),
            RecordError::TooShort
        );
        // 末尾に余分なバイト（パディング）があっても通る。
        let mut padded = good.clone();
        padded.extend_from_slice(&[0; 5]);
        assert!(decode_record(&padded, start).is_ok());

        let mutate = |f: &dyn Fn(&mut Vec<u8>), seal: bool| {
            let mut b = good.clone();
            f(&mut b);
            if seal {
                reseal(&mut b);
            }
            decode_record(&b, start).unwrap_err()
        };
        assert_eq!(mutate(&|b| b[40] ^= 1, false), RecordError::BadCrc);
        assert_eq!(mutate(&|b| b[0] = 0, false), RecordError::BadLength);
        assert_eq!(
            mutate(
                &|b| b[0..4].copy_from_slice(&(MAX_RECORD_LEN as u32 + 1).to_le_bytes()),
                false
            ),
            RecordError::BadLength
        );
        assert_eq!(mutate(&|b| b[24] = 9, true), RecordError::BadRmgr);
        assert_eq!(mutate(&|b| b[25] = 0x01, true), RecordError::BadInfo);
        assert_eq!(mutate(&|b| b[25] = 0x40, true), RecordError::BadInfo);
        assert_eq!(mutate(&|b| b[27] = 1, true), RecordError::BadInfo);
        assert_eq!(
            mutate(&|b| b[26] = 33, true),
            RecordError::BadBlockRef("nblocks")
        );
        assert_eq!(
            mutate(&|b| b[32] = 1, true),
            RecordError::BadBlockRef("block_id")
        );
        assert_eq!(
            mutate(&|b| b[33] |= 0x10, true),
            RecordError::BadBlockRef("flags")
        );
        assert_eq!(
            mutate(&|b| b[33] |= BLK_HAS_IMAGE | BLK_WILL_INIT, true),
            RecordError::BadBlockRef("WILL_INIT with image")
        );
        assert_eq!(
            mutate(&|b| b[33] |= BLK_HAS_HOLE, true),
            RecordError::BadBlockRef("hole without image")
        );
        assert_eq!(
            mutate(&|b| b[33] &= !BLK_HAS_DATA, true),
            RecordError::BadBlockRef("data_len without HAS_DATA")
        );
        assert_eq!(
            mutate(&|b| b[28..32].copy_from_slice(&3u32.to_le_bytes()), true),
            RecordError::BadLength
        );
        assert_eq!(
            mutate(&|b| b[52..56].copy_from_slice(&99u32.to_le_bytes()), true),
            RecordError::BadBlockRef("truncated data")
        );
        assert_eq!(
            mutate(&|b| b[8..16].copy_from_slice(&0x100u64.to_le_bytes()), true),
            RecordError::BadPrev
        );
    }

    #[test]
    fn decode_rejects_bad_hole() {
        let mut page = Page::zeroed();
        page.init_heap();
        page.add_item(&[1u8; 10]).unwrap();
        let mut rb = RecordBuilder::new(RmgrId::Xlog, 0x40, Xid(0));
        rb.register_block(tag(0), &page, RegFlags::STANDARD | RegFlags::FORCE_IMAGE);
        let good = sealed(&rb, 0, true, 0);
        let start = Lsn(0x100);
        assert!(decode_record(&good, start).is_ok());
        let with = |ho: u16, hl: u16, flags: u8| {
            let mut b = good.clone();
            b[33] = flags;
            b[56..58].copy_from_slice(&ho.to_le_bytes());
            b[58..60].copy_from_slice(&hl.to_le_bytes());
            reseal(&mut b);
            decode_record(&b, start)
        };
        let f = BLK_HAS_IMAGE | BLK_HAS_HOLE;
        assert_eq!(
            with(23, 10, f).unwrap_err(),
            RecordError::BadBlockRef("hole range")
        );
        assert_eq!(
            with(100, 9000, f).unwrap_err(),
            RecordError::BadBlockRef("hole range")
        );
        assert_eq!(
            with(100, 10, BLK_HAS_IMAGE).unwrap_err(),
            RecordError::BadBlockRef("hole without HAS_HOLE")
        );
    }

    #[test]
    fn encode_rejects_oversize_and_too_many_blocks() {
        let p = Page::zeroed();
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        rb.main_data(&vec![0u8; MAX_RECORD_LEN]);
        assert!(encode_record(&rb, Lsn(0), true).is_err());
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        rb.main_data(&vec![0u8; MAX_RECORD_LEN - RECORD_HEADER_SIZE]);
        assert_eq!(
            encode_record(&rb, Lsn(0), true).unwrap().len(),
            MAX_RECORD_LEN
        );
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        for i in 0..=MAX_BLOCK_REFS {
            rb.blocks.push(BlockReg {
                tag: tag(i as u32),
                page: &p,
                flags: RegFlags::NO_IMAGE,
                data: Vec::new(),
            });
        }
        assert!(encode_record(&rb, Lsn(0), true).is_err());
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_tag_asserts() {
        let p = Page::zeroed();
        let mut rb = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        rb.register_block(tag(1), &p, RegFlags::NONE);
        rb.register_block(tag(1), &p, RegFlags::NONE);
    }
}
