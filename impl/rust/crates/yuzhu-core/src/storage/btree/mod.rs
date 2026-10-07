//! B+Tree（PostgreSQL の nbtree 準拠。`m4/06-btree.md`、`m4/00-contracts.md` §13.5、§15.1）。
//!
//! P0-b の時点では定数と子モジュールの宣言だけで、実装は B1（`insert` `split` `wal` ほか）と
//! B2（`scan` `build` `unique` `check`）が子モジュールの中を埋める。依存の向きは
//! `storage::heap` ← `btree` ← `index_store`。

// B1b・B2b が結線するまで、呼び出し元のない関数がある。
#![allow(dead_code, clippy::many_single_char_names)]

use std::cmp::Ordering;
use std::sync::Arc;

use crate::error::{Error, sqlstate};
use crate::storage::IndexHandle;
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber};
use crate::types::{Datum, Tid};
use crate::wal::Wal;

pub mod build;
pub mod check;
pub mod insert;
pub mod meta;
pub mod page;
pub mod scan;
pub mod search;
pub mod split;
#[cfg(test)]
pub(crate) mod testing;
pub mod tuple;
pub mod unique;
pub mod wal;

// ----- 00 §15.1 -------------------------------------------------------------

pub const BT_MAX_ITEM_SIZE: usize = 2704;
pub const BT_SPECIAL_SIZE: usize = 16;
pub const BT_MAGIC: u32 = 0x0005_3162;
pub const BT_VERSION: u32 = 1;
pub const BT_META_BLOCK: u32 = 0;
pub const BT_FIRST_ROOT_BLOCK: u32 = 1;
pub const BT_FILLFACTOR_LEAF: usize = 90;
pub const BT_FILLFACTOR_INNER: usize = 70;
pub const INDEX_MAX_KEYS: usize = 32;

// ----- 06 §4.1 --------------------------------------------------------------

/// `BLCKSZ - 24 - BT_SPECIAL_SIZE`。
pub const BT_PAGE_USABLE: usize = 8152;
/// 完全なピボット = 葉タプル + 8（末尾に TID 6 バイト + 埋め草 2）。
pub const BT_PIVOT_EXTRA: usize = 8;
pub const BT_MAX_PIVOT_SIZE: usize = BT_MAX_ITEM_SIZE + BT_PIVOT_EXTRA;
/// 一括構築の 1 レコードのページ数（= `MAX_BLOCK_REFS`）。
pub const BT_BUILD_BATCH_PAGES: usize = 32;
/// これ以上の level は破損（`XX001`）。
pub const BT_MAX_LEVEL: u32 = 64;
pub const BT_P_HIKEY: u16 = 1;

pub const INDEX_TUPLE_HEADER_SIZE: usize = 8;
pub const INDEX_SIZE_MASK: u16 = 0x1FFF;
/// ピボット。
pub const INDEX_ALT_TID_MASK: u16 = 0x2000;
/// 可変長の列を含む。
pub const INDEX_VAR_MASK: u16 = 0x4000;
/// NULL を含む（ビットマップあり）。
pub const INDEX_NULL_MASK: u16 = 0x8000;
/// ピボットの `t_tid.offset`: 末尾にヒープ TID。
pub const BT_PIVOT_HEAP_TID_ATTR: u16 = 0x1000;
pub const BT_PIVOT_NATTS_MASK: u16 = 0x0FFF;

pub const BTP_LEAF: u16 = 0x0001;
pub const BTP_ROOT: u16 = 0x0002;
pub const BTP_META: u16 = 0x0008;
/// これ以外のビットが立っていたら `XX001`。
pub const BTP_KNOWN_MASK: u16 = BTP_LEAF | BTP_ROOT | BTP_META;

// ----- 06 §4.5 --------------------------------------------------------------

/// 1 回の操作が使う文脈。軽い値（参照だけ）で、操作ごとに作る。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BtCtx<'a> {
    pub pool: &'a Arc<BufferPool>,
    pub wal: &'a Wal,
    pub index: &'a IndexHandle,
    /// 変異テスト用のスイッチ（`btree_split_in_two_records`）。
    pub knobs: crate::debug_knobs::DebugKnobs,
}

impl BtCtx<'_> {
    /// main フォークのタグ。
    pub(crate) fn tag(&self, block: BlockNumber) -> BufferTag {
        BufferTag {
            rel: self.index.locator,
            fork: ForkNumber::Main,
            block,
        }
    }

    /// `XX001` `index "x" contains a corrupted page at block N`、DETAIL = detail、HINT `Please REINDEX it.`。
    pub(crate) fn corrupted(&self, block: BlockNumber, detail: impl Into<String>) -> Error {
        Error::new(
            sqlstate::DATA_CORRUPTED,
            format!(
                "index \"{}\" contains a corrupted page at block {block}",
                self.index.name
            ),
        )
        .with_detail(detail)
        .with_hint("Please REINDEX it.")
    }

    /// `XX001` `index "x" contains unexpected zero page at block N`、HINT `Please REINDEX it.`。
    pub(crate) fn zero_page(&self, block: BlockNumber) -> Error {
        Error::new(
            sqlstate::DATA_CORRUPTED,
            format!(
                "index \"{}\" contains unexpected zero page at block {block}",
                self.index.name
            ),
        )
        .with_hint("Please REINDEX it.")
    }
}

/// 木の順序そのもの: キー列だけを、各列の cmp・DESC・NULLS FIRST で比べる（NULL どうしは等しい）。
/// 07 の `build_from_heap` のソートが使う。`a.len() == b.len() == index.columns.len()`。
pub fn cmp_keys(index: &IndexHandle, a: &[Datum], b: &[Datum]) -> Ordering {
    debug_assert_eq!(a.len(), index.columns.len());
    debug_assert_eq!(b.len(), index.columns.len());
    for ((col, x), y) in index.columns.iter().zip(a).zip(b) {
        let o = search::cmp_column(col, x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// `cmp_keys` が `Equal` ならヒープ TID の昇順（ブロック、オフセット）。
pub fn cmp_key_tid(
    index: &IndexHandle,
    a: &[Datum],
    a_tid: Tid,
    b: &[Datum],
    b_tid: Tid,
) -> Ordering {
    cmp_keys(index, a, b).then_with(|| cmp_tid(a_tid, b_tid))
}

/// ヒープ TID の順序（ブロック、オフセット）。
pub(crate) fn cmp_tid(a: Tid, b: Tid) -> Ordering {
    (a.block, a.offset).cmp(&(b.block, b.offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BLCKSZ, SIZE_OF_PAGE_HEADER};

    #[test]
    fn constants_are_consistent() {
        assert_eq!(
            BT_PAGE_USABLE,
            BLCKSZ - SIZE_OF_PAGE_HEADER - BT_SPECIAL_SIZE
        );
        assert_eq!(BT_MAX_PIVOT_SIZE, 2712);
        assert_eq!(BT_BUILD_BATCH_PAGES, crate::wal::MAX_BLOCK_REFS);
        assert_eq!(BTP_KNOWN_MASK, 0x000B);
        assert_eq!(wal::BTREE_INSERT_LEAF, 0x00);
        assert_eq!(wal::BTREE_PAGES, 0x10);
    }
}
