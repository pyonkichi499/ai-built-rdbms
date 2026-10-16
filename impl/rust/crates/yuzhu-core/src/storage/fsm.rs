//! 空き領域マップ（`m5/03` §3.3・§5.7.1。VC-2）。
//!
//! 2 段の木: FSM ブロック 0 が根（葉ごとの最大カテゴリ）、ブロック `1 + L` が葉（ヒープの
//! ブロック `L * 8160 + j` のカテゴリ）。カテゴリは「空き / 32」の 1 バイト。
//!
//! FSM は WAL を書かない導出データ（M5 00 規約 4）。ページの変更は `page_mut_hint` で行い、
//! 壊れていても欠けていても `search` は `None` を返すだけで、呼び出し側の文を失敗させない
//! （`Severity::Panic` だけは伝える）。VACUUM が全ブロックを記録し直す。
//!
//! ラッチの順序: FSM のページのラッチはヒープのページのラッチを持たずに取る。根と葉を
//! 同時に持つこともない。

use std::sync::Arc;

use crate::error::{Result, Severity};
use crate::storage::buffer::{BufferPool, PinnedBuffer};
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator};

pub use crate::storage::page::FsmPageKind;

/// 1 カテゴリが表す空きのバイト数。
pub const FSM_CAT_STEP: usize = 32;
/// 1 ページのスロット数（根が持つ葉の数、葉が持つブロックの数）。
pub const FSM_SLOTS_PER_PAGE: usize = 8160;
/// FSM に載るブロック数の上限（`8160 * 8160`）。
pub const FSM_MAX_BLOCKS: u64 = 66_585_600;

/// FSM のブロック番号・葉の添字を `BlockNumber` にする。`FSM_MAX_BLOCKS` が上限なので必ず収まる。
fn to_block(n: usize) -> BlockNumber {
    BlockNumber::try_from(n).expect("FSM index fits in BlockNumber")
}

/// `search` が 1 回で根を直しながら試す最大回数。
const SEARCH_ATTEMPTS: usize = 3;
/// `next_slot` の位置（u16 LE）。
const OFF_NEXT_SLOT: usize = 26;

/// 記録側のカテゴリ（切り捨て）。
pub fn avail_to_cat(free: usize) -> u8 {
    u8::try_from(free / FSM_CAT_STEP).unwrap_or(u8::MAX)
}

/// 要求側のカテゴリ（切り上げ）。
pub fn needed_to_cat(needed: usize) -> u8 {
    u8::try_from(needed.div_ceil(FSM_CAT_STEP)).unwrap_or(u8::MAX)
}

fn next_slot(page: &Page) -> usize {
    usize::from(u16::from_le_bytes([
        page.0[OFF_NEXT_SLOT],
        page.0[OFF_NEXT_SLOT + 1],
    ]))
}

fn set_next_slot(page: &mut Page, v: usize) {
    let v = u16::try_from(v).unwrap_or(0);
    page.0[OFF_NEXT_SLOT..OFF_NEXT_SLOT + 2].copy_from_slice(&v.to_le_bytes());
}

/// `start` から循環して、`data[i] >= cat` の最初の位置。
fn find_from(data: &[u8], start: usize, cat: u8) -> Option<usize> {
    let start = start.min(data.len());
    (start..data.len())
        .chain(0..start)
        .find(|&i| data[i] >= cat)
}

#[derive(Debug)]
pub struct FreeSpaceMap {
    pool: Arc<BufferPool>,
}

impl FreeSpaceMap {
    pub fn new(pool: Arc<BufferPool>) -> FreeSpaceMap {
        FreeSpaceMap { pool }
    }

    fn tag(rel: RelFileLocator, block: BlockNumber) -> BufferTag {
        BufferTag {
            rel,
            fork: ForkNumber::Fsm,
            block,
        }
    }

    fn read(&self, rel: RelFileLocator, block: BlockNumber) -> Result<Option<PinnedBuffer>> {
        if !self.pool.smgr().exists(rel, ForkNumber::Fsm)? {
            return Ok(None);
        }
        self.pool.read_buffer_zero_on_error(Self::tag(rel, block))
    }

    /// `Severity::Panic` 以外の失敗は WARNING にして「FSM なし」として扱う。
    fn soften<T>(r: Result<T>, what: &str) -> Result<Option<T>> {
        match r {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.severity == Severity::Panic => Err(e),
            Err(e) => {
                crate::util::warn(&format!("free space map {what} failed: {e}"));
                Ok(None)
            }
        }
    }

    /// FSM のファイルと、ブロック `0..need` を用意する。競合して多めに伸びてもよい。
    fn ensure(&self, rel: RelFileLocator, need: BlockNumber) -> Result<()> {
        let smgr = self.pool.smgr();
        if !smgr.exists(rel, ForkNumber::Fsm)?
            && let Err(e) = smgr.create(rel, ForkNumber::Fsm)
            && !smgr.exists(rel, ForkNumber::Fsm)?
        {
            return Err(e);
        }
        while smgr.nblocks(rel, ForkNumber::Fsm)? < need {
            drop(self.pool.extend(rel, ForkNumber::Fsm)?);
        }
        Ok(())
    }

    /// `needed`（MAXALIGN 済みのタプルの大きさ）以上の空きがあるとされるブロック。
    /// なければ、または FSM が読めないときは `None`。
    pub fn search(&self, rel: RelFileLocator, needed: usize) -> Result<Option<BlockNumber>> {
        Ok(Self::soften(self.search_inner(rel, needed), "search")?.flatten())
    }

    fn search_inner(&self, rel: RelFileLocator, needed: usize) -> Result<Option<BlockNumber>> {
        // カテゴリ 0 は「記録なし」と同じなので、要求は 1 以上にする。
        let cat = needed_to_cat(needed).max(1);
        for _ in 0..SEARCH_ATTEMPTS {
            let Some(root) = self.read(rel, 0)? else {
                return Ok(None);
            };
            let i = {
                let g = root.read()?;
                if g.fsm_kind() != Some(FsmPageKind::Root) {
                    return Ok(None);
                }
                match find_from(g.fsm_data(), next_slot(&g), cat) {
                    Some(i) => i,
                    None => return Ok(None),
                }
            };
            let leaf_block = 1 + to_block(i);
            let Some(leaf) = self.read(rel, leaf_block)? else {
                Self::set_root_entry(&root, i, 0)?;
                continue;
            };
            let (found, max) = {
                let g = leaf.read()?;
                if g.fsm_kind() == Some(FsmPageKind::Leaf) {
                    let data = g.fsm_data();
                    (
                        find_from(data, next_slot(&g), cat),
                        data.iter().copied().max().unwrap_or(0),
                    )
                } else {
                    (None, 0)
                }
            };
            if let Some(j) = found {
                // 負荷の分散用のヒント。ラッチが取れなければ省く。
                Self::hint_next(&leaf, j + 1)?;
                Self::hint_next(&root, i)?;
                return Ok(Some(to_block(i * FSM_SLOTS_PER_PAGE + j)));
            }
            // 根が古い。葉の最大に直して探し直す。
            Self::set_root_entry(&root, i, max)?;
        }
        Ok(None)
    }

    /// `next_slot` のヒントを書く。ラッチが取れない、または変わらないときは何もしない。
    fn hint_next(buf: &PinnedBuffer, v: usize) -> Result<()> {
        if let Some(mut w) = buf.try_write()?
            && next_slot(w.page()) != v
        {
            set_next_slot(w.page_mut_hint(), v);
        }
        Ok(())
    }

    /// 根の `data[l]` を書く（変わらなければ dirty にしない）。
    fn set_root_entry(root: &PinnedBuffer, l: usize, v: u8) -> Result<()> {
        let mut g = root.write()?;
        if g.page().fsm_kind().is_none() {
            g.page_mut_hint().init_fsm(FsmPageKind::Root);
        }
        if g.page().fsm_data()[l] != v {
            g.page_mut_hint().fsm_data_mut()[l] = v;
        }
        Ok(())
    }

    /// ブロックの空き（`Page::free_space` の値）を記録する。
    pub fn record(&self, rel: RelFileLocator, blk: BlockNumber, free_bytes: usize) -> Result<()> {
        self.record_batch(rel, &[(blk, free_bytes)])
    }

    /// 同じ葉に属するものを 1 回のラッチでまとめて書く（VACUUM 用）。
    pub fn record_batch(
        &self,
        rel: RelFileLocator,
        entries: &[(BlockNumber, usize)],
    ) -> Result<()> {
        let mut sorted: Vec<(BlockNumber, usize)> = entries
            .iter()
            .copied()
            .filter(|(b, _)| u64::from(*b) < FSM_MAX_BLOCKS)
            .collect();
        if sorted.is_empty() {
            return Ok(());
        }
        sorted.sort_by_key(|(b, _)| *b);
        Self::soften(self.record_sorted(rel, &sorted), "record")?;
        Ok(())
    }

    fn record_sorted(&self, rel: RelFileLocator, sorted: &[(BlockNumber, usize)]) -> Result<()> {
        for group in sorted
            .chunk_by(|a, b| a.0 as usize / FSM_SLOTS_PER_PAGE == b.0 as usize / FSM_SLOTS_PER_PAGE)
        {
            let l = group[0].0 as usize / FSM_SLOTS_PER_PAGE;
            let leaf_block = 1 + to_block(l);
            self.ensure(rel, leaf_block + 1)?;
            let Some(leaf) = self
                .pool
                .read_buffer_zero_on_error(Self::tag(rel, leaf_block))?
            else {
                continue;
            };
            let max = {
                let mut g = leaf.write()?;
                if g.page().fsm_kind().is_none() {
                    g.page_mut_hint().init_fsm(FsmPageKind::Leaf);
                }
                for &(blk, free) in group {
                    let j = blk as usize % FSM_SLOTS_PER_PAGE;
                    let cat = avail_to_cat(free);
                    if g.page().fsm_data()[j] != cat {
                        g.page_mut_hint().fsm_data_mut()[j] = cat;
                    }
                }
                g.page().fsm_data().iter().copied().max().unwrap_or(0)
            };
            // 葉のラッチを外してから根へ。
            if let Some(root) = self.pool.read_buffer_zero_on_error(Self::tag(rel, 0))? {
                Self::set_root_entry(&root, l, max)?;
            }
        }
        Ok(())
    }

    /// `nblocks` 以降のカテゴリを 0 にし、根を直し、FSM のファイルを葉
    /// `ceil(nblocks / 8160)` までに切り詰める（WAL なし）。呼び出し側が排他の
    /// バリアを持つ。FSM のファイルがなければ何もしない。
    pub fn truncate(&self, rel: RelFileLocator, nblocks: BlockNumber) -> Result<()> {
        Self::soften(self.truncate_inner(rel, nblocks), "truncate")?;
        Ok(())
    }

    fn truncate_inner(&self, rel: RelFileLocator, nblocks: BlockNumber) -> Result<()> {
        let smgr = self.pool.smgr();
        if !smgr.exists(rel, ForkNumber::Fsm)? {
            return Ok(());
        }
        let l0 = nblocks as usize / FSM_SLOTS_PER_PAGE;
        let r = nblocks as usize % FSM_SLOTS_PER_PAGE;
        let mut root_from = l0;
        if r != 0 {
            root_from = l0 + 1;
            if let Some(leaf) = self.read(rel, 1 + to_block(l0))? {
                let max = {
                    let mut g = leaf.write()?;
                    if g.page().fsm_kind() == Some(FsmPageKind::Leaf) {
                        if g.page().fsm_data()[r..].iter().any(|&c| c != 0) {
                            g.page_mut_hint().fsm_data_mut()[r..].fill(0);
                        }
                        g.page().fsm_data().iter().copied().max().unwrap_or(0)
                    } else {
                        0
                    }
                };
                if let Some(root) = self.read(rel, 0)? {
                    Self::set_root_entry(&root, l0, max)?;
                }
            }
        }
        if let Some(root) = self.read(rel, 0)? {
            let mut g = root.write()?;
            if g.page().fsm_kind() == Some(FsmPageKind::Root)
                && g.page().fsm_data()[root_from..].iter().any(|&c| c != 0)
            {
                g.page_mut_hint().fsm_data_mut()[root_from..].fill(0);
            }
        }
        let keep = 1 + nblocks.div_ceil(to_block(FSM_SLOTS_PER_PAGE));
        self.pool
            .drop_relation_buffers_from(rel, ForkNumber::Fsm, keep)?;
        smgr.truncate(rel, ForkNumber::Fsm, keep)
    }

    /// ブロックのカテゴリ（テストと検査用）。記録がなければ 0。
    pub fn category(&self, rel: RelFileLocator, blk: BlockNumber) -> Result<u8> {
        if u64::from(blk) >= FSM_MAX_BLOCKS {
            return Ok(0);
        }
        let leaf = 1 + to_block(blk as usize / FSM_SLOTS_PER_PAGE);
        let Some(buf) = self.read(rel, leaf)? else {
            return Ok(0);
        };
        let g = buf.read()?;
        if g.fsm_kind() != Some(FsmPageKind::Leaf) {
            return Ok(0);
        }
        Ok(g.fsm_data()[blk as usize % FSM_SLOTS_PER_PAGE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::testing::{TestStorage, test_rel};
    use crate::storage::vfs::Vfs as _;

    fn setup() -> (TestStorage, FreeSpaceMap, RelFileLocator) {
        let ts = TestStorage::new();
        let fsm = FreeSpaceMap::new(Arc::clone(ts.pool()));
        (ts, fsm, test_rel(7000))
    }

    #[test]
    fn category_boundaries() {
        assert_eq!(avail_to_cat(0), 0);
        assert_eq!(avail_to_cat(31), 0);
        assert_eq!(avail_to_cat(32), 1);
        assert_eq!(avail_to_cat(8160), 255);
        assert_eq!(avail_to_cat(8164), 255);
        assert_eq!(avail_to_cat(8200), 255);
        assert_eq!(needed_to_cat(0), 0);
        assert_eq!(needed_to_cat(1), 1);
        assert_eq!(needed_to_cat(32), 1);
        assert_eq!(needed_to_cat(33), 2);
        assert_eq!(needed_to_cat(8160), 255);
        assert_eq!(needed_to_cat(8200), 255);
    }

    #[test]
    fn search_without_a_file_is_none() {
        let (ts, fsm, rel) = setup();
        assert_eq!(fsm.search(rel, 100).unwrap(), None);
        assert_eq!(fsm.category(rel, 3).unwrap(), 0);
        fsm.truncate(rel, 5).unwrap();
        ts.assert_clean();
    }

    #[test]
    fn record_then_search_and_the_documented_bytes() {
        let (ts, fsm, rel) = setup();
        fsm.record(rel, 0, 8164).unwrap();
        fsm.record(rel, 1, 0).unwrap();
        fsm.record(rel, 2, 100).unwrap();
        assert_eq!(fsm.category(rel, 2).unwrap(), 3);
        assert_eq!(fsm.search(rel, 96).unwrap(), Some(0));
        let leaf = ts
            .pool()
            .read_buffer_zero_on_error(FreeSpaceMap::tag(rel, 1))
            .unwrap()
            .unwrap();
        let g = leaf.read().unwrap();
        assert_eq!(&g.fsm_data()[..4], &[0xFF, 0x00, 0x03, 0x00]);
        assert_eq!(g.fsm_kind(), Some(FsmPageKind::Leaf));
        drop(g);
        let root = ts
            .pool()
            .read_buffer_zero_on_error(FreeSpaceMap::tag(rel, 0))
            .unwrap()
            .unwrap();
        assert_eq!(root.read().unwrap().fsm_data()[0], 0xFF);
        drop((leaf, root));
        // Too big for any block but the empty one; nothing fits 8160+ except cat 255.
        fsm.record(rel, 0, 40).unwrap();
        assert_eq!(fsm.search(rel, 96).unwrap(), Some(2));
        assert_eq!(fsm.search(rel, 200).unwrap(), None);
        ts.assert_clean();
    }

    #[test]
    fn next_slot_rotates_through_equal_blocks() {
        let (ts, fsm, rel) = setup();
        for b in 0..3 {
            fsm.record(rel, b, 500).unwrap();
        }
        let got: Vec<_> = (0..4)
            .map(|_| fsm.search(rel, 400).unwrap().unwrap())
            .collect();
        assert_eq!(got, vec![0, 1, 2, 0]);
        ts.assert_clean();
    }

    #[test]
    fn second_leaf_and_the_upper_limit() {
        let (ts, fsm, rel) = setup();
        fsm.record(rel, 8160, 1000).unwrap();
        fsm.record(rel, 9000, 2000).unwrap();
        assert_eq!(fsm.search(rel, 1500).unwrap(), Some(9000));
        assert_eq!(fsm.search(rel, 900).unwrap(), Some(8160));
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Fsm).unwrap(), 3);
        fsm.record(rel, BlockNumber::try_from(FSM_MAX_BLOCKS).unwrap(), 8000)
            .unwrap();
        fsm.record(rel, u32::MAX, 8000).unwrap();
        assert_eq!(
            fsm.category(rel, BlockNumber::try_from(FSM_MAX_BLOCKS).unwrap())
                .unwrap(),
            0
        );
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Fsm).unwrap(), 3);
        ts.assert_clean();
    }

    #[test]
    fn a_stale_root_is_repaired() {
        let (ts, fsm, rel) = setup();
        fsm.record(rel, 5, 4000).unwrap();
        fsm.record(rel, 5, 0).unwrap();
        // Make the root claim space that the leaf does not have.
        let root = ts
            .pool()
            .read_buffer_zero_on_error(FreeSpaceMap::tag(rel, 0))
            .unwrap()
            .unwrap();
        root.write().unwrap().page_mut_hint().fsm_data_mut()[0] = 200;
        drop(root);
        assert_eq!(fsm.search(rel, 3000).unwrap(), None);
        let root = ts
            .pool()
            .read_buffer_zero_on_error(FreeSpaceMap::tag(rel, 0))
            .unwrap()
            .unwrap();
        assert_eq!(root.read().unwrap().fsm_data()[0], 0);
        drop(root);
        // A root entry whose leaf block is missing is cleared too.
        fsm.record(rel, 8160 * 3, 4000).unwrap();
        ts.pool()
            .drop_relation_buffers_from(rel, ForkNumber::Fsm, 2)
            .unwrap();
        ts.smgr().truncate(rel, ForkNumber::Fsm, 2).unwrap();
        assert_eq!(fsm.search(rel, 3000).unwrap(), None);
        ts.assert_clean();
    }

    #[test]
    fn truncate_clears_the_tail_and_shortens_the_file() {
        let (ts, fsm, rel) = setup();
        fsm.record_batch(
            rel,
            &[
                (3, 4000),
                (10, 4000),
                (8160 + 4, 4000),
                (8160 * 2 + 1, 4000),
            ],
        )
        .unwrap();
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Fsm).unwrap(), 4);
        fsm.truncate(rel, 8160 + 5).unwrap();
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Fsm).unwrap(), 3);
        assert_eq!(fsm.category(rel, 8160 + 4).unwrap(), 125);
        assert_eq!(fsm.category(rel, 8160 * 2 + 1).unwrap(), 0);
        fsm.truncate(rel, 5).unwrap();
        assert_eq!(ts.smgr().nblocks(rel, ForkNumber::Fsm).unwrap(), 2);
        assert_eq!(fsm.category(rel, 3).unwrap(), 125);
        assert_eq!(fsm.category(rel, 10).unwrap(), 0);
        assert_eq!(fsm.search(rel, 3000).unwrap(), Some(3));
        fsm.truncate(rel, 3).unwrap();
        assert_eq!(fsm.search(rel, 3000).unwrap(), None);
        ts.assert_clean();
    }

    #[test]
    fn record_batch_groups_by_leaf_and_accepts_any_order() {
        let (ts, fsm, rel) = setup();
        fsm.record_batch(rel, &[(9000, 640), (2, 320), (1, 64), (8159, 96)])
            .unwrap();
        assert_eq!(fsm.category(rel, 1).unwrap(), 2);
        assert_eq!(fsm.category(rel, 2).unwrap(), 10);
        assert_eq!(fsm.category(rel, 8159).unwrap(), 3);
        assert_eq!(fsm.category(rel, 9000).unwrap(), 20);
        fsm.record_batch(rel, &[]).unwrap();
        ts.assert_clean();
    }

    #[test]
    fn damaged_pages_read_as_empty_and_do_not_fail() {
        let (ts, fsm, rel) = setup();
        fsm.record(rel, 1, 4000).unwrap();
        // Unknown kind.
        let root = ts
            .pool()
            .read_buffer_zero_on_error(FreeSpaceMap::tag(rel, 0))
            .unwrap()
            .unwrap();
        root.write().unwrap().page_mut_hint().0[24] = 9;
        drop(root);
        assert_eq!(fsm.search(rel, 100).unwrap(), None);
        // Checksum damage on disk.
        ts.pool().flush_relation_buffers(rel).unwrap();
        ts.pool()
            .drop_relation_buffers_from(rel, ForkNumber::Fsm, 0)
            .unwrap();
        let path = crate::storage::smgr::relpath(rel, ForkNumber::Fsm, 0);
        let f = ts
            .vfs
            .open(&path, crate::storage::vfs::OpenMode::ReadWrite)
            .unwrap();
        f.write_all_at(&[0xAB; 16], 100).unwrap();
        assert_eq!(fsm.search(rel, 100).unwrap(), None);
        // Only the root page was damaged; the leaf still holds its entry.
        // The next record repairs the map.
        fsm.record(rel, 1, 4000).unwrap();
        assert_eq!(fsm.search(rel, 100).unwrap(), Some(1));
        ts.assert_clean();
    }

    #[test]
    fn all_zero_pages_are_empty() {
        let (ts, fsm, rel) = setup();
        ts.smgr().create(rel, ForkNumber::Fsm).unwrap();
        drop(ts.pool().extend(rel, ForkNumber::Fsm).unwrap());
        assert_eq!(fsm.search(rel, 100).unwrap(), None);
        fsm.record(rel, 4, 800).unwrap();
        assert_eq!(fsm.search(rel, 100).unwrap(), Some(4));
        ts.assert_clean();
    }

    #[test]
    fn the_fsm_writes_no_wal() {
        let (ts, fsm, rel) = setup();
        let before = ts.wal().insert_lsn();
        fsm.record(rel, 3, 900).unwrap();
        fsm.record_batch(rel, &[(1, 100), (9000, 100)]).unwrap();
        fsm.search(rel, 50).unwrap();
        fsm.truncate(rel, 2).unwrap();
        assert_eq!(ts.wal().insert_lsn(), before);
    }
}
