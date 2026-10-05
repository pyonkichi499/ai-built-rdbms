//! レコードのエンコード・デコードと CRC（`m3.md` §3.3、§4.3、§6.3.1）。
//!
//! 担当 W1 が実装する。ここにあるのは A が置いたスタブ（組み立て器の入れ物と
//! `decode_record` の雛形）。

#![allow(dead_code, clippy::unused_self, clippy::needless_pass_by_value)]

use super::{DecodedRecord, Lsn, RegFlags, RmgrId};
use crate::storage::page::Page;
use crate::storage::smgr::BufferTag;
use crate::txn::Xid;

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
    /// リリースでは `insert` が内部エラーを返す（W1 が実装）。
    pub fn register_block(&mut self, tag: BufferTag, page: &'a Page, flags: RegFlags) -> u8 {
        debug_assert!(self.blocks.len() < super::MAX_BLOCK_REFS);
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

/// デコード（reader と単体テストが使う）。長さ・フラグ・CRC の検査をすべて行う。
///
/// 担当 W1 が実装する。スタブは常に `TooShort` を返す。
pub fn decode_record(buf: &[u8], start: Lsn) -> Result<DecodedRecord, RecordError> {
    let _ = (buf, start);
    Err(RecordError::TooShort)
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

    #[test]
    fn builder_assigns_sequential_block_ids_and_collects_data() {
        let p0 = Page([0; crate::storage::BLCKSZ]);
        let p1 = Page([1; crate::storage::BLCKSZ]);
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
}
