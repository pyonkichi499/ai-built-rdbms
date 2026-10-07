//! インデックスタプル（葉タプル、ピボット、-∞ ピボット。`m4/06-btree.md` §3.5、§4.5）。
//!
//! ```text
//! 0  u32  t_tid.block   葉: ヒープ TID のブロック / ピボット: 子のブロック（high key は 0）
//! 4  u16  t_tid.offset  葉: ヒープ TID の行ポインタ番号 / ピボット: 下位 12 ビット = キー属性数、0x1000 = 末尾にヒープ TID
//! 6  u16  t_info        bit15 NULL あり、bit14 可変長あり、bit13 ピボット、bit0..12 = タプルの大きさ（8 の倍数）
//! 8  [u8; 4]            NULL ビットマップ（NULL があるときだけ。列 i が非 NULL ならビット i が 1）
//! 8 または 16 ...       列データ（ヒープと同じ符号化。整列の基準はタプルの先頭）
//! ```

use super::{
    BT_MAX_ITEM_SIZE, BT_PIVOT_EXTRA, BT_PIVOT_HEAP_TID_ATTR, BT_PIVOT_NATTS_MASK,
    INDEX_ALT_TID_MASK, INDEX_MAX_KEYS, INDEX_NULL_MASK, INDEX_SIZE_MASK, INDEX_TUPLE_HEADER_SIZE,
    INDEX_VAR_MASK,
};
use crate::error::{Error, Result, sqlstate};
use crate::storage::heap::tuple::{ColumnCursor, encode_attr};
use crate::storage::{IndexHandle, MAXALIGN};
use crate::types::{Datum, Tid};

/// NULL ビットマップの大きさ（`INDEX_MAX_KEYS` = 32 ビット）。
const NULL_BITMAP_SIZE: usize = INDEX_MAX_KEYS / 8;
/// NULL を含むタプルのデータの開始位置（`MAXALIGN(8 + 4)`）。
const DATA_OFFSET_WITH_NULLS: usize = 16;

fn maxalign(n: usize) -> usize {
    (n + MAXALIGN - 1) & !(MAXALIGN - 1)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// インデックスタプルのバイト列（行ポインタが指す全体）。検査は `validate` で行い、
/// アクセサは検査済みを前提にする。
#[derive(Clone, Copy, Debug)]
pub struct IndexTuple<'a>(pub &'a [u8]);

impl<'a> IndexTuple<'a> {
    /// 大きさ・ビット・ピボットの形を検査する。理由（`XX001` の DETAIL に付く）は静的な文字列。
    pub fn validate(
        bytes: &'a [u8],
        ncols: usize,
    ) -> std::result::Result<IndexTuple<'a>, &'static str> {
        if bytes.len() < INDEX_TUPLE_HEADER_SIZE {
            return Err("tuple is shorter than its header");
        }
        let info = u16_at(bytes, 6);
        let size = usize::from(info & INDEX_SIZE_MASK);
        if size != bytes.len() || !size.is_multiple_of(MAXALIGN) {
            return Err("bad tuple size");
        }
        if ncols == 0 || ncols > INDEX_MAX_KEYS {
            return Err("bad number of key columns");
        }
        let t = IndexTuple(bytes);
        if info & INDEX_NULL_MASK != 0 && size < DATA_OFFSET_WITH_NULLS {
            return Err("tuple is shorter than its null bitmap");
        }
        if info & INDEX_ALT_TID_MASK == 0 {
            // 葉タプル。
            return Ok(t);
        }
        let offset = u16_at(bytes, 4);
        let natts = usize::from(offset & BT_PIVOT_NATTS_MASK);
        let has_tid = offset & BT_PIVOT_HEAP_TID_ATTR != 0;
        if offset & !(BT_PIVOT_NATTS_MASK | BT_PIVOT_HEAP_TID_ATTR) != 0 {
            return Err("unexpected pivot flags");
        }
        if natts == 0 {
            // -∞ ピボット。
            if has_tid
                || size != INDEX_TUPLE_HEADER_SIZE
                || info & !(INDEX_ALT_TID_MASK | INDEX_SIZE_MASK) != 0
            {
                return Err("malformed minus infinity pivot");
            }
            return Ok(t);
        }
        // M4 は切り詰めない: 完全なピボット（属性数 = ncols、TID あり）だけ。
        if natts != ncols || !has_tid {
            return Err("unexpected number of pivot attributes");
        }
        if size < t.data_offset() + BT_PIVOT_EXTRA {
            return Err("pivot is shorter than its heap TID");
        }
        Ok(t)
    }

    /// `t_info` の下位 13 ビット。
    pub fn size(&self) -> usize {
        usize::from(u16_at(self.0, 6) & INDEX_SIZE_MASK)
    }

    pub fn has_nulls(&self) -> bool {
        u16_at(self.0, 6) & INDEX_NULL_MASK != 0
    }

    pub fn has_varlena(&self) -> bool {
        u16_at(self.0, 6) & INDEX_VAR_MASK != 0
    }

    pub fn is_pivot(&self) -> bool {
        u16_at(self.0, 6) & INDEX_ALT_TID_MASK != 0
    }

    /// 列データの開始位置（NULL なしなら 8、あれば 16）。
    pub fn data_offset(&self) -> usize {
        if self.has_nulls() {
            DATA_OFFSET_WITH_NULLS
        } else {
            INDEX_TUPLE_HEADER_SIZE
        }
    }

    /// 葉タプル: ヒープ TID。ピボット: TID を持てば末尾の 6 バイト、持たなければ `None`（-∞ ピボット）。
    pub fn heap_tid(&self) -> Option<Tid> {
        if !self.is_pivot() {
            return Some(Tid {
                block: u32_at(self.0, 0),
                offset: u16_at(self.0, 4),
            });
        }
        if u16_at(self.0, 4) & BT_PIVOT_HEAP_TID_ATTR == 0 {
            return None;
        }
        let n = self.0.len();
        Some(Tid {
            block: u32_at(self.0, n - 6),
            offset: u16_at(self.0, n - 2),
        })
    }

    /// ピボットの `t_tid.block`（子のブロック。high key は 0）。
    pub fn downlink(&self) -> u32 {
        u32_at(self.0, 0)
    }

    /// ピボットの属性数（葉タプルには意味がない。呼び出し側が `is_pivot` で使い分ける）。
    pub fn pivot_natts(&self) -> usize {
        usize::from(u16_at(self.0, 4) & BT_PIVOT_NATTS_MASK)
    }

    /// ピボットで属性数 0（どんな探索キーよりも小さい）。
    pub fn is_minus_infinity(&self) -> bool {
        self.is_pivot() && self.pivot_natts() == 0
    }

    /// 列 `i` が NULL か（NULL ビットマップを見る）。
    pub fn is_null_at(&self, i: usize) -> bool {
        self.has_nulls() && self.0[INDEX_TUPLE_HEADER_SIZE + i / 8] & (1 << (i % 8)) == 0
    }
}

/// 葉タプルを作る。`MAXALIGN` 後の大きさが `BT_MAX_ITEM_SIZE` を超えたら `54000`（D6-5）。
/// `tid` は葉のヒープ TID（エラーメッセージにも使う）。
pub fn form_index_tuple(index: &IndexHandle, key: &[Datum], tid: Tid) -> Result<Vec<u8>> {
    let ncols = index.columns.len();
    if key.len() != ncols {
        return Err(Error::internal(format!(
            "index \"{}\" has {ncols} key columns but the key has {} values",
            index.name,
            key.len()
        )));
    }
    if ncols == 0 || ncols > INDEX_MAX_KEYS {
        return Err(Error::internal(format!(
            "index \"{}\" has an invalid number of key columns: {ncols}",
            index.name
        )));
    }
    let has_nulls = key.iter().any(Datum::is_null);
    let data_offset = if has_nulls {
        DATA_OFFSET_WITH_NULLS
    } else {
        INDEX_TUPLE_HEADER_SIZE
    };
    let mut buf = vec![0u8; data_offset];
    let mut has_var = false;
    let mut bits = [0u8; NULL_BITMAP_SIZE];
    for (i, (col, d)) in index.columns.iter().zip(key).enumerate() {
        if d.is_null() {
            continue;
        }
        bits[i / 8] |= 1 << (i % 8);
        has_var |= encode_attr(&mut buf, &col.attr, d)?;
    }
    let size = maxalign(buf.len());
    if size > BT_MAX_ITEM_SIZE {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "index row size {size} exceeds btree version 4 maximum {BT_MAX_ITEM_SIZE} for index \"{}\"",
                index.name
            ),
        )
        .with_detail(format!(
            "Index row references tuple ({},{}) in relation \"{}\".",
            tid.block, tid.offset, index.table_name
        ))
        .with_hint(
            "Values larger than 1/3 of a buffer page cannot be indexed.\n\
             Consider a function index of an MD5 hash of the value, or use full text indexing.",
        )
        .with_table(index.schema.clone(), index.table_name.clone())
        .with_constraint(index.name.clone()));
    }
    buf.resize(size, 0);
    buf[0..4].copy_from_slice(&tid.block.to_le_bytes());
    buf[4..6].copy_from_slice(&tid.offset.to_le_bytes());
    let mut info = u16::try_from(size).unwrap_or(0) & INDEX_SIZE_MASK;
    if has_nulls {
        info |= INDEX_NULL_MASK;
        buf[INDEX_TUPLE_HEADER_SIZE..INDEX_TUPLE_HEADER_SIZE + NULL_BITMAP_SIZE]
            .copy_from_slice(&bits);
    }
    if has_var {
        info |= INDEX_VAR_MASK;
    }
    buf[6..8].copy_from_slice(&info.to_le_bytes());
    Ok(buf)
}

/// 葉タプルまたは完全なピボットから、先頭 `n` 列のキーを復号する（NULL は `Datum::Null`）。
pub fn decode_key(index: &IndexHandle, t: &IndexTuple<'_>, n: usize) -> Result<Vec<Datum>> {
    let ncols = index.columns.len();
    if n > ncols || (t.is_pivot() && n > t.pivot_natts()) {
        return Err(Error::internal(format!(
            "cannot decode {n} key columns of index \"{}\"",
            index.name
        )));
    }
    let mut cur = ColumnCursor::new(t.0, t.data_offset());
    let mut out = Vec::with_capacity(n);
    for (i, col) in index.columns.iter().take(n).enumerate() {
        if t.is_null_at(i) {
            out.push(Datum::Null);
        } else {
            out.push(cur.read_attr(&col.attr)?);
        }
    }
    Ok(out)
}

/// 完全なピボットを作る（葉タプル → 長さ `S + 8`、ヒープ TID を末尾の 6 バイトへ）。
/// `downlink` は `t_tid.block` に入れる（high key は 0）。
pub fn leaf_to_pivot(leaf: &[u8], downlink: u32, ncols: usize) -> Vec<u8> {
    let size = leaf.len() + BT_PIVOT_EXTRA;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(leaf);
    out.resize(size, 0);
    let leaf_info = u16_at(leaf, 6);
    let tid_block = &leaf[0..4];
    let tid_offset = &leaf[4..6];
    out[size - 6..size - 2].copy_from_slice(tid_block);
    out[size - 2..size].copy_from_slice(tid_offset);
    out[0..4].copy_from_slice(&downlink.to_le_bytes());
    let natts = u16::try_from(ncols).unwrap_or(0) & BT_PIVOT_NATTS_MASK;
    out[4..6].copy_from_slice(&(natts | BT_PIVOT_HEAP_TID_ATTR).to_le_bytes());
    let info = (u16::try_from(size).unwrap_or(0) & INDEX_SIZE_MASK)
        | INDEX_ALT_TID_MASK
        | (leaf_info & (INDEX_NULL_MASK | INDEX_VAR_MASK));
    out[6..8].copy_from_slice(&info.to_le_bytes());
    out
}

/// 完全なピボットの `downlink` だけを差し替えた複製。
pub fn pivot_with_downlink(pivot: &[u8], downlink: u32) -> Vec<u8> {
    let mut out = pivot.to_vec();
    out[0..4].copy_from_slice(&downlink.to_le_bytes());
    out
}

/// 内部ページの最初のデータ項目（属性 0 個の -∞ ピボット、8 バイト）。
pub fn minus_infinity_pivot(downlink: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&downlink.to_le_bytes());
    b[6..8].copy_from_slice(&(INDEX_ALT_TID_MASK | 8).to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::testing::test_index;
    use crate::types::SqlType;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid { block, offset }
    }

    fn hex(b: &[u8]) -> String {
        b.iter()
            .map(|x| format!("{x:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn example_int4_leaf() {
        // §3.6 例 2: キー 10、TID (0,1)。
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let b = form_index_tuple(&t.handle, &[Datum::Int4(10)], tid(0, 1)).unwrap();
        assert_eq!(hex(&b), "00 00 00 00 01 00 10 00 0A 00 00 00 00 00 00 00");
        let it = IndexTuple::validate(&b, 1).unwrap();
        assert_eq!(it.size(), 16);
        assert_eq!(it.heap_tid(), Some(tid(0, 1)));
        assert!(!it.is_pivot() && !it.has_nulls() && !it.has_varlena());
        assert_eq!(
            decode_key(&t.handle, &it, 1).unwrap(),
            vec![Datum::Int4(10)]
        );
    }

    #[test]
    fn example_text_leaf() {
        // §3.6 例 5: 'hello'、TID (1,2)。
        let t = test_index(&[(SqlType::TEXT, false, false)], false);
        let b = form_index_tuple(&t.handle, &[Datum::Text("hello".into())], tid(1, 2)).unwrap();
        assert_eq!(hex(&b), "01 00 00 00 02 00 10 40 0D 68 65 6C 6C 6F 00 00");
        let it = IndexTuple::validate(&b, 1).unwrap();
        assert!(it.has_varlena());
        assert_eq!(
            decode_key(&t.handle, &it, 1).unwrap(),
            vec![Datum::Text("hello".into())]
        );
    }

    #[test]
    fn example_null_leaf() {
        // §3.6 例 6: (NULL, 'x')、TID (0,5)。
        let t = test_index(
            &[(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            false,
        );
        let b = form_index_tuple(
            &t.handle,
            &[Datum::Null, Datum::Text("x".into())],
            tid(0, 5),
        )
        .unwrap();
        assert_eq!(
            hex(&b),
            "00 00 00 00 05 00 18 C0 02 00 00 00 00 00 00 00 05 78 00 00 00 00 00 00"
        );
        let it = IndexTuple::validate(&b, 2).unwrap();
        assert!(it.has_nulls() && it.has_varlena());
        assert!(it.is_null_at(0) && !it.is_null_at(1));
        assert_eq!(
            decode_key(&t.handle, &it, 2).unwrap(),
            vec![Datum::Null, Datum::Text("x".into())]
        );
        assert_eq!(decode_key(&t.handle, &it, 1).unwrap(), vec![Datum::Null]);
    }

    #[test]
    fn example_pivots() {
        // §3.6 例 3・4: キー 40・TID (0,4) の完全なピボット。
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let leaf = form_index_tuple(&t.handle, &[Datum::Int4(40)], tid(0, 4)).unwrap();
        let high = leaf_to_pivot(&leaf, 0, 1);
        assert_eq!(
            hex(&high),
            "00 00 00 00 01 10 18 20 28 00 00 00 00 00 00 00 00 00 00 00 00 00 04 00"
        );
        let it = IndexTuple::validate(&high, 1).unwrap();
        assert!(it.is_pivot() && !it.is_minus_infinity());
        assert_eq!(it.heap_tid(), Some(tid(0, 4)));
        assert_eq!(it.downlink(), 0);
        assert_eq!(it.pivot_natts(), 1);
        assert_eq!(
            decode_key(&t.handle, &it, 1).unwrap(),
            vec![Datum::Int4(40)]
        );

        let down = pivot_with_downlink(&high, 2);
        assert_eq!(
            hex(&down),
            "02 00 00 00 01 10 18 20 28 00 00 00 00 00 00 00 00 00 00 00 00 00 04 00"
        );
        assert_eq!(IndexTuple(&down).downlink(), 2);

        let inf = minus_infinity_pivot(1);
        assert_eq!(hex(&inf), "01 00 00 00 00 00 08 20");
        let it = IndexTuple::validate(&inf, 1).unwrap();
        assert!(it.is_minus_infinity());
        assert_eq!(it.heap_tid(), None);
        assert_eq!(it.downlink(), 1);
    }

    #[test]
    fn pivot_keeps_null_and_var_bits() {
        let t = test_index(
            &[(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            false,
        );
        let leaf = form_index_tuple(
            &t.handle,
            &[Datum::Null, Datum::Text("x".into())],
            tid(3, 9),
        )
        .unwrap();
        let p = leaf_to_pivot(&leaf, 7, 2);
        assert_eq!(p.len(), leaf.len() + 8);
        let it = IndexTuple::validate(&p, 2).unwrap();
        assert!(it.has_nulls() && it.has_varlena() && it.is_pivot());
        assert_eq!(it.heap_tid(), Some(tid(3, 9)));
        assert_eq!(it.downlink(), 7);
        assert_eq!(
            decode_key(&t.handle, &it, 2).unwrap(),
            vec![Datum::Null, Datum::Text("x".into())]
        );
    }

    #[test]
    fn validate_rejects_malformed() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let leaf = form_index_tuple(&t.handle, &[Datum::Int4(1)], tid(0, 1)).unwrap();
        assert!(IndexTuple::validate(&leaf[..4], 1).is_err());
        assert!(IndexTuple::validate(&leaf[..8], 1).is_err()); // 大きさが t_info と違う
        let mut bad = leaf.clone();
        bad[6] = 0x0C; // 大きさ 12（8 の倍数でない）
        assert!(IndexTuple::validate(&bad, 1).is_err());

        // 属性数が ncols 未満のピボット（M4 では作らない）。
        let full = leaf_to_pivot(&leaf, 1, 1);
        assert!(IndexTuple::validate(&full, 2).is_err());
        // posting（offset の 0x2000）。
        let mut posting = full.clone();
        posting[5] |= 0x20;
        assert!(IndexTuple::validate(&posting, 1).is_err());
        // TID なしの完全なピボット。
        let mut no_tid = full.clone();
        no_tid[5] &= !0x10;
        assert!(IndexTuple::validate(&no_tid, 1).is_err());
        // -∞ ピボットに余計なビット。
        let mut inf = minus_infinity_pivot(1);
        inf[7] |= 0x40;
        assert!(IndexTuple::validate(&inf, 1).is_err());
        // NULL ビットなのに短い。
        let mut short = [0u8; 8];
        short[6..8].copy_from_slice(&0x8008u16.to_le_bytes());
        assert!(IndexTuple::validate(&short, 1).is_err());
    }

    #[test]
    fn tuple_size_limit_boundaries() {
        // 06 §7.2（PostgreSQL 17 の実測と同じ境界）。
        let text = |n: usize| Datum::Text("x".repeat(n));
        let t = test_index(&[(SqlType::TEXT, false, false)], false);
        let ok = form_index_tuple(&t.handle, &[text(2692)], tid(0, 1)).unwrap();
        assert_eq!(ok.len(), 2704);
        let e = form_index_tuple(&t.handle, &[text(2693)], tid(0, 2)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            e.message
                .starts_with("index row size 2712 exceeds btree version 4 maximum 2704 for index"),
            "{}",
            e.message
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Index row references tuple (0,2) in relation \"t\".")
        );
        assert!(
            e.hint
                .as_deref()
                .unwrap()
                .starts_with("Values larger than 1/3")
        );
        assert_eq!(e.constraint(), Some(t.handle.name.as_str()));
        assert_eq!(e.table(), Some("t"));

        let t = test_index(
            &[(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            false,
        );
        form_index_tuple(&t.handle, &[Datum::Int4(1), text(2688)], tid(0, 1)).unwrap();
        assert!(form_index_tuple(&t.handle, &[Datum::Int4(1), text(2689)], tid(0, 1)).is_err());
        form_index_tuple(&t.handle, &[Datum::Null, text(2684)], tid(0, 1)).unwrap();
        assert!(form_index_tuple(&t.handle, &[Datum::Null, text(2685)], tid(0, 1)).is_err());
    }

    #[test]
    fn wrong_key_length_is_internal_error() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let e = form_index_tuple(&t.handle, &[], tid(0, 1)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }
}
