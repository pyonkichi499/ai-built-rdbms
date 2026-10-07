//! B+Tree のページ共通（special 領域、読み込み直後の検査。`m4/06-btree.md` §3.2、§4.5、§6.3）。

use super::tuple::IndexTuple;
use super::{BT_MAX_LEVEL, BT_SPECIAL_SIZE, BTP_KNOWN_MASK, BTP_LEAF, BTP_META, BTP_ROOT, BtCtx};
use crate::error::Result;
use crate::storage::page::{LpFlags, Page};
use crate::storage::smgr::BlockNumber;
use crate::storage::{BLCKSZ, SIZE_OF_PAGE_HEADER};

/// special 領域（16 バイト）の中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtSpecial {
    pub prev: BlockNumber,
    pub next: BlockNumber,
    pub level: u32,
    pub flags: u16,
    pub cycleid: u16,
}

impl BtSpecial {
    /// special 領域が 16 バイトでなければ `None`。
    pub fn read(page: &Page) -> Option<BtSpecial> {
        let s = page.special_area();
        if s.len() != BT_SPECIAL_SIZE {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([s[o], s[o + 1], s[o + 2], s[o + 3]]);
        Some(BtSpecial {
            prev: u32_at(0),
            next: u32_at(4),
            level: u32_at(8),
            flags: u16::from_le_bytes([s[12], s[13]]),
            cycleid: u16::from_le_bytes([s[14], s[15]]),
        })
    }

    pub fn to_bytes(&self) -> [u8; BT_SPECIAL_SIZE] {
        let mut b = [0u8; BT_SPECIAL_SIZE];
        b[0..4].copy_from_slice(&self.prev.to_le_bytes());
        b[4..8].copy_from_slice(&self.next.to_le_bytes());
        b[8..12].copy_from_slice(&self.level.to_le_bytes());
        b[12..14].copy_from_slice(&self.flags.to_le_bytes());
        b[14..16].copy_from_slice(&self.cycleid.to_le_bytes());
        b
    }

    /// special 領域へ書く。special 領域が 16 バイトでないページには何もしない。
    pub fn write(&self, page: &mut Page) {
        let s = page.special_area_mut();
        if s.len() == BT_SPECIAL_SIZE {
            s.copy_from_slice(&self.to_bytes());
        }
    }

    pub fn is_leaf(&self) -> bool {
        self.flags & BTP_LEAF != 0
    }

    pub fn is_root(&self) -> bool {
        self.flags & BTP_ROOT != 0
    }

    /// 右隣がない（high key を持たない）。
    pub fn is_rightmost(&self) -> bool {
        self.next == 0
    }

    /// 最初のデータ項目の行ポインタ番号（`P_FIRSTDATAKEY`）。
    pub fn first_data_offset(&self) -> u16 {
        if self.next != 0 { 2 } else { 1 }
    }
}

/// ページを読んだ直後の検査（`m4/06-btree.md` §6.3）。破れていれば `XX001`。
///
/// `expect_level` が `Some` なら、ページの `level` と一致すること（降下で「期待したレベルのページか」を
/// 確かめる。`ROOT` フラグは見ない）。全 0 のページは `zero_page`。メタページ（`META`）は木のページとして
/// 現れてはならない。
pub(crate) fn validate_page(
    ctx: &BtCtx<'_>,
    page: &Page,
    block: BlockNumber,
    expect_level: Option<u32>,
) -> Result<BtSpecial> {
    if page.is_new() || page.is_all_zero() {
        return Err(ctx.zero_page(block));
    }
    let lower = usize::from(page.lower());
    let upper = usize::from(page.upper());
    let special = usize::from(page.special());
    if special != BLCKSZ - BT_SPECIAL_SIZE {
        return Err(ctx.corrupted(block, "unexpected special area size"));
    }
    if lower < SIZE_OF_PAGE_HEADER || lower > upper || upper > special {
        return Err(ctx.corrupted(block, "invalid page header"));
    }
    let Some(sp) = BtSpecial::read(page) else {
        return Err(ctx.corrupted(block, "unexpected special area size"));
    };
    if sp.flags & !BTP_KNOWN_MASK != 0 {
        return Err(ctx.corrupted(
            block,
            format!(
                "reserved flags 0x{:04x} are set",
                sp.flags & !BTP_KNOWN_MASK
            ),
        ));
    }
    if sp.flags & BTP_META != 0 {
        return Err(ctx.corrupted(
            block,
            format!("unexpected flags 0x{:04x} on a tree page", sp.flags),
        ));
    }
    if sp.is_leaf() != (sp.level == 0)
        || sp.level >= BT_MAX_LEVEL
        || expect_level.is_some_and(|l| l != sp.level)
    {
        return Err(ctx.corrupted(block, "unexpected level"));
    }
    if sp.cycleid != 0 {
        return Err(ctx.corrupted(block, "unexpected cycle id"));
    }
    if sp.prev == block || sp.next == block {
        return Err(ctx.corrupted(block, "sibling chain is cyclic"));
    }
    let max = page.max_offset();
    if sp.next != 0 && max < 1 {
        return Err(ctx.corrupted(block, "missing high key"));
    }
    for off in 1..=max {
        match page.item_id(off) {
            Ok(id) if id.flags == LpFlags::Normal => {}
            _ => return Err(ctx.corrupted(block, "unexpected line pointer flags")),
        }
    }
    Ok(sp)
}

/// 項目（行ポインタ `off`）のバイト列。`LP_NORMAL` でなければ、またはインデックスタプルとして
/// 不正なら `XX001`。
pub(crate) fn item<'p>(
    ctx: &BtCtx<'_>,
    page: &'p Page,
    block: BlockNumber,
    off: u16,
) -> Result<&'p [u8]> {
    let id = page
        .item_id(off)
        .map_err(|_| ctx.corrupted(block, "unexpected line pointer flags"))?;
    if id.flags != LpFlags::Normal {
        return Err(ctx.corrupted(block, "unexpected line pointer flags"));
    }
    let bytes = page
        .item(off)
        .map_err(|_| ctx.corrupted(block, "unexpected line pointer flags"))?;
    IndexTuple::validate(bytes, ctx.index.columns.len())
        .map_err(|why| ctx.corrupted(block, format!("invalid index tuple: {why}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::meta::empty_root_leaf;
    use crate::storage::btree::testing::{TestIndex, test_index};
    use crate::types::SqlType;

    fn idx() -> TestIndex {
        test_index(&[(SqlType::INT4, false, false)], false)
    }

    #[test]
    fn special_round_trip() {
        let sp = BtSpecial {
            prev: 7,
            next: 9,
            level: 2,
            flags: BTP_ROOT,
            cycleid: 0,
        };
        let mut page = Page::zeroed();
        page.init_special(BT_SPECIAL_SIZE);
        sp.write(&mut page);
        assert_eq!(BtSpecial::read(&page), Some(sp));
        assert_eq!(&page.0[8176..8180], &7u32.to_le_bytes());
        assert!(sp.is_root() && !sp.is_leaf() && !sp.is_rightmost());
        assert_eq!(sp.first_data_offset(), 2);
        let mut heap = Page::zeroed();
        heap.init_heap();
        assert_eq!(BtSpecial::read(&heap), None);
    }

    #[test]
    fn validate_accepts_empty_root_leaf() {
        let t = idx();
        let ctx = t.ctx();
        let page = empty_root_leaf();
        let sp = validate_page(&ctx, &page, 1, Some(0)).unwrap();
        assert!(sp.is_leaf() && sp.is_root() && sp.is_rightmost());
    }

    fn corrupt_code(r: Result<BtSpecial>) -> (String, Option<String>) {
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::DATA_CORRUPTED);
        (e.message.clone(), e.detail.clone())
    }

    #[test]
    fn validate_detects_damage() {
        let t = idx();
        let ctx = t.ctx();
        let good = empty_root_leaf();

        let zero = Page::zeroed();
        let (m, _) = corrupt_code(validate_page(&ctx, &zero, 7, None));
        assert!(m.contains("unexpected zero page at block 7"), "{m}");

        let mut heap = Page::zeroed();
        heap.init_heap();
        let (_, d) = corrupt_code(validate_page(&ctx, &heap, 1, None));
        assert_eq!(d.as_deref(), Some("unexpected special area size"));

        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.flags |= 0x0004;
        sp.write(&mut p);
        let (_, d) = corrupt_code(validate_page(&ctx, &p, 1, None));
        assert_eq!(d.as_deref(), Some("reserved flags 0x0004 are set"));

        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.level = 1; // LEAF なのに level 1
        sp.write(&mut p);
        let (_, d) = corrupt_code(validate_page(&ctx, &p, 1, None));
        assert_eq!(d.as_deref(), Some("unexpected level"));

        let (_, d) = corrupt_code(validate_page(&ctx, &good, 1, Some(1)));
        assert_eq!(d.as_deref(), Some("unexpected level"));

        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.cycleid = 3;
        sp.write(&mut p);
        let (_, d) = corrupt_code(validate_page(&ctx, &p, 1, None));
        assert_eq!(d.as_deref(), Some("unexpected cycle id"));

        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.next = 1;
        sp.write(&mut p);
        let (_, d) = corrupt_code(validate_page(&ctx, &p, 1, None));
        assert_eq!(d.as_deref(), Some("sibling chain is cyclic"));

        // 右隣があるのに high key がない。
        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.next = 5;
        sp.write(&mut p);
        let (_, d) = corrupt_code(validate_page(&ctx, &p, 1, None));
        assert_eq!(d.as_deref(), Some("missing high key"));

        let mut p = good.clone();
        let mut sp = BtSpecial::read(&p).unwrap();
        sp.flags |= BTP_META;
        sp.write(&mut p);
        assert!(validate_page(&ctx, &p, 1, None).is_err());
    }

    #[test]
    fn validate_detects_bad_line_pointer_and_tuple() {
        let t = idx();
        let ctx = t.ctx();
        let sp = BtSpecial {
            prev: 0,
            next: 0,
            level: 0,
            flags: BTP_LEAF,
            cycleid: 0,
        };
        let leaf = super::super::tuple::form_index_tuple(
            &t.handle,
            &[crate::types::Datum::Int4(5)],
            crate::types::Tid {
                block: 0,
                offset: 1,
            },
        )
        .unwrap();
        let page = Page::build_with_items(&sp.to_bytes(), &[&leaf]).unwrap();
        validate_page(&ctx, &page, 1, Some(0)).unwrap();
        assert_eq!(item(&ctx, &page, 1, 1).unwrap(), &leaf[..]);
        assert!(item(&ctx, &page, 1, 2).is_err());

        // LP_DEAD に書き換える（lp_flags = bits 15..16）。
        let mut dead = page.clone();
        let id = u32::from_le_bytes([dead.0[24], dead.0[25], dead.0[26], dead.0[27]]);
        let id = (id & !(3 << 15)) | (3 << 15);
        dead.0[24..28].copy_from_slice(&id.to_le_bytes());
        let e = validate_page(&ctx, &dead, 1, None).unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("unexpected line pointer flags"));

        // タプルの大きさ（t_info）を壊す。
        let mut bad = leaf.clone();
        bad[6] = 0x18;
        let page = Page::build_with_items(&sp.to_bytes(), &[&bad]).unwrap();
        let e = item(&ctx, &page, 1, 1).unwrap_err();
        assert!(
            e.detail
                .as_deref()
                .unwrap()
                .starts_with("invalid index tuple"),
            "{e:?}"
        );
    }
}
