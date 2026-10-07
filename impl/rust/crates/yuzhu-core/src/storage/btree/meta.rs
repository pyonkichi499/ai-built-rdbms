//! メタページ（ブロック 0）と `init_index`（`m4/06-btree.md` §3.3、§5.7）。

use std::sync::Arc;

use super::page::BtSpecial;
use super::wal::{PagesReason, log_pages};
use super::{
    BT_FIRST_ROOT_BLOCK, BT_MAGIC, BT_MAX_LEVEL, BT_META_BLOCK, BT_SPECIAL_SIZE, BT_VERSION,
    BTP_LEAF, BTP_META, BTP_ROOT, BtCtx,
};
use crate::error::{Error, Result, sqlstate};
use crate::storage::buffer::{BufferPool, CriticalSection};
use crate::storage::page::Page;
use crate::storage::smgr::ForkNumber;
use crate::storage::{IndexHandle, WriteCtx};
use crate::wal::Wal;

/// メタデータの大きさ（ページヘッダの直後から 32 バイト）。
const META_BODY_SIZE: usize = 32;
const META_LOWER: u16 = 56;

/// `fastroot` / `fastlevel` は `root` / `level` と同じ値なので持たない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtMeta {
    pub root: u32,
    pub level: u32,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl BtMeta {
    /// `magic`・`version`・`root != 0`・`fastroot == root`・`fastlevel == level`・`level < 64`・special を検査する。
    pub(crate) fn read(ctx: &BtCtx<'_>, page: &Page) -> Result<BtMeta> {
        let name = &ctx.index.name;
        if page.is_new() || page.is_all_zero() {
            return Err(ctx.zero_page(BT_META_BLOCK));
        }
        let body = page.body();
        if body.len() < META_BODY_SIZE {
            return Err(Error::new(
                sqlstate::DATA_CORRUPTED,
                format!("index \"{name}\" is not a btree"),
            ));
        }
        let magic = u32_at(body, 0);
        if magic != BT_MAGIC {
            return Err(Error::new(
                sqlstate::DATA_CORRUPTED,
                format!("index \"{name}\" is not a btree"),
            ));
        }
        let version = u32_at(body, 4);
        if version != BT_VERSION {
            return Err(Error::new(
                sqlstate::DATA_CORRUPTED,
                format!(
                    "version mismatch in index \"{name}\": file version {version}, current version {BT_VERSION}"
                ),
            ));
        }
        let Some(sp) = BtSpecial::read(page) else {
            return Err(ctx.corrupted(BT_META_BLOCK, "unexpected special area size"));
        };
        if sp.flags != BTP_META || sp.prev != 0 || sp.next != 0 || sp.level != 0 || sp.cycleid != 0
        {
            return Err(ctx.corrupted(BT_META_BLOCK, "unexpected metapage special area"));
        }
        let root = u32_at(body, 8);
        let level = u32_at(body, 12);
        let fastroot = u32_at(body, 16);
        let fastlevel = u32_at(body, 20);
        if root == 0 {
            return Err(ctx.corrupted(BT_META_BLOCK, "metapage has no root"));
        }
        if fastroot != root || fastlevel != level {
            return Err(ctx.corrupted(BT_META_BLOCK, "unexpected fast root"));
        }
        if level >= BT_MAX_LEVEL {
            return Err(ctx.corrupted(BT_META_BLOCK, "unexpected level"));
        }
        Ok(BtMeta { root, level })
    }

    /// §3.6 例 1 のメタページ（`pd_lsn` = 0）。
    pub fn to_page(&self) -> Box<Page> {
        let mut page = Box::new(Page::zeroed());
        page.init_special(BT_SPECIAL_SIZE);
        page.set_lower(META_LOWER);
        self.write(&mut page);
        BtSpecial {
            prev: 0,
            next: 0,
            level: 0,
            flags: BTP_META,
            cycleid: 0,
        }
        .write(&mut page);
        page
    }

    /// body だけ書き換える（`pd_lower` が 56 のページ）。
    pub fn write(&self, page: &mut Page) {
        let body = page.body_mut();
        if body.len() < META_BODY_SIZE {
            return;
        }
        body[..META_BODY_SIZE].fill(0);
        body[0..4].copy_from_slice(&BT_MAGIC.to_le_bytes());
        body[4..8].copy_from_slice(&BT_VERSION.to_le_bytes());
        body[8..12].copy_from_slice(&self.root.to_le_bytes());
        body[12..16].copy_from_slice(&self.level.to_le_bytes());
        body[16..20].copy_from_slice(&self.root.to_le_bytes());
        body[20..24].copy_from_slice(&self.level.to_le_bytes());
    }
}

/// §3.6 例 8 のブロック 1（`flags = LEAF | ROOT`）。
pub(crate) fn empty_root_leaf() -> Box<Page> {
    let mut page = Box::new(Page::zeroed());
    page.init_special(BT_SPECIAL_SIZE);
    BtSpecial {
        prev: 0,
        next: 0,
        level: 0,
        flags: BTP_LEAF | BTP_ROOT,
        cycleid: 0,
    }
    .write(&mut page);
    page
}

/// 作成済みの空のファイルに、メタページ（ブロック 0）と空のルート葉（ブロック 1）を `BTREE_PAGES`（`INIT`）
/// で書く（§5.7）。
pub fn init_index(
    pool: &Arc<BufferPool>,
    wal: &Wal,
    w: &WriteCtx,
    index: &IndexHandle,
) -> Result<()> {
    let rel = index.locator;
    if pool.nblocks(rel, ForkNumber::Main)? != 0 {
        return Err(Error::internal("index storage is not empty"));
    }
    let meta_pin = pool.extend(rel, ForkNumber::Main)?;
    let root_pin = pool.extend(rel, ForkNumber::Main)?;
    if meta_pin.tag().block != BT_META_BLOCK || root_pin.tag().block != BT_FIRST_ROOT_BLOCK {
        return Err(Error::internal("unexpected block numbers for a new index"));
    }
    let gm = meta_pin.write_tree()?;
    let gr = root_pin.write_tree()?;
    let mut guards = [(meta_pin.tag(), gm), (root_pin.tag(), gr)];
    let cs = CriticalSection::enter(pool);
    *guards[0].1.page_mut() = *BtMeta {
        root: BT_FIRST_ROOT_BLOCK,
        level: 0,
    }
    .to_page();
    *guards[1].1.page_mut() = *empty_root_leaf();
    log_pages(wal, w, PagesReason::Init, &mut guards).map_err(|e| cs.escalate(e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::testing::{TestIndex, test_index};
    use crate::types::SqlType;

    fn idx() -> TestIndex {
        test_index(&[(SqlType::INT4, false, false)], false)
    }

    #[test]
    fn meta_page_bytes_match_example_1() {
        let page = BtMeta { root: 1, level: 0 }.to_page();
        assert_eq!(page.lower(), 56);
        assert_eq!(page.upper(), 8176);
        assert_eq!(page.special(), 8176);
        assert_eq!(&page.0[10..12], &[0, 0]);
        assert_eq!(&page.0[18..20], &[0x01, 0x20]);
        assert_eq!(&page.0[24..28], &[0x62, 0x31, 0x05, 0x00]);
        assert_eq!(&page.0[28..32], &[1, 0, 0, 0]);
        assert_eq!(&page.0[32..36], &[1, 0, 0, 0]);
        assert_eq!(&page.0[40..44], &[1, 0, 0, 0]);
        assert!(page.0[44..56].iter().all(|&b| b == 0));
        assert_eq!(&page.0[8188..8190], &[0x08, 0x00]);
        // 非 0 のバイトはこれだけ（残りは 0）。
        let nonzero = page.0.iter().filter(|&&b| b != 0).count();
        assert_eq!(nonzero, 14);
    }

    #[test]
    fn meta_read_round_trip_and_checks() {
        let t = idx();
        let ctx = t.ctx();
        let m = BtMeta { root: 5, level: 2 };
        let page = m.to_page();
        assert_eq!(BtMeta::read(&ctx, &page).unwrap(), m);

        let bad = |f: &dyn Fn(&mut Page)| {
            let mut p = m.to_page();
            f(&mut p);
            let e = BtMeta::read(&ctx, &p).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
            e
        };
        let e = bad(&|p| p.0[24] ^= 0xFF);
        assert!(e.message.contains("is not a btree"));
        let e = bad(&|p| p.0[28] = 9);
        assert!(e.message.contains("version mismatch"));
        bad(&|p| p.0[32..36].copy_from_slice(&0u32.to_le_bytes()));
        bad(&|p| p.0[40..44].copy_from_slice(&9u32.to_le_bytes()));
        bad(&|p| p.0[44..48].copy_from_slice(&9u32.to_le_bytes()));
        bad(&|p| {
            p.0[36..40].copy_from_slice(&64u32.to_le_bytes());
            p.0[44..48].copy_from_slice(&64u32.to_le_bytes());
        });
        bad(&|p| p.0[8188] = 0x0B);
        let e = BtMeta::read(&ctx, &Page::zeroed()).unwrap_err();
        assert!(e.message.contains("zero page"));
    }

    #[test]
    fn empty_root_leaf_shape() {
        let p = empty_root_leaf();
        assert_eq!(p.lower(), 24);
        assert_eq!(p.upper(), 8176);
        let sp = BtSpecial::read(&p).unwrap();
        assert_eq!(sp.flags, 0x0003);
        assert!(sp.is_rightmost());
    }

    #[test]
    fn init_index_writes_meta_and_root() {
        let t = idx();
        let w = t.write_ctx();
        init_index(t.pool(), t.wal(), &w, &t.handle).unwrap();
        assert_eq!(
            t.pool()
                .nblocks(t.handle.locator, ForkNumber::Main)
                .unwrap(),
            2
        );
        let ctx = t.ctx();
        let pin = t.pool().read_buffer(ctx.tag(0)).unwrap();
        let m = BtMeta::read(&ctx, &pin.read().unwrap()).unwrap();
        assert_eq!(m, BtMeta { root: 1, level: 0 });
        let pin1 = t.pool().read_buffer(ctx.tag(1)).unwrap();
        let g = pin1.read().unwrap();
        assert!(g.lsn() > 0);
        super::super::page::validate_page(&ctx, &g, 1, Some(0)).unwrap();
        drop(g);
        drop(pin);
        drop(pin1);
        // 2 回目は拒否される。
        let e = init_index(t.pool(), t.wal(), &w, &t.handle).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        t.assert_clean();
    }
}
