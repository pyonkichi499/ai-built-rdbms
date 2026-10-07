//! 一括構築（`m4/06-btree.md` §5.8）。
//!
//! 入力は `(キー, TID)` を木の順序で整列したもの。葉を左から右へ全部作ってから上のレベルを作る。
//! メモリには 1 ページ分の項目と、ページごとの区切り 1 つ、書き出し待ちの最大 32 ページだけを持つ。
//! ページは 32 枚ずつ `BTREE_PAGES`（`Build`）で書く。

use std::cmp::Ordering;
use std::iter::Peekable;

use super::meta::BtMeta;
use super::page::{BtSpecial, validate_page};
use super::tuple::{
    IndexTuple, form_index_tuple, leaf_to_pivot, minus_infinity_pivot, pivot_with_downlink,
};
use super::wal::{PagesReason, log_pages};
use super::{
    BT_BUILD_BATCH_PAGES, BT_FILLFACTOR_INNER, BT_FILLFACTOR_LEAF, BT_FIRST_ROOT_BLOCK,
    BT_MAX_LEVEL, BT_META_BLOCK, BT_PAGE_USABLE, BTP_LEAF, BTP_ROOT, BtCtx, cmp_key_tid, cmp_keys,
};
use crate::error::{Error, Result, sqlstate};
use crate::storage::buffer::{CriticalSection, PinnedBuffer};
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::storage::{BuildStats, BuildUnique, WriteCtx};
use crate::types::{Datum, Tid};

/// 行ポインタ 1 本の大きさ。
const LP_SIZE: usize = 4;

fn cost(len: usize) -> usize {
    len + LP_SIZE
}

/// `BuildUnique::Yes` の重複（`23505`、DETAIL なし。§6.4）。
fn unique_violation_for_build(ctx: &BtCtx<'_>) -> Error {
    Error::new(
        sqlstate::UNIQUE_VIOLATION,
        format!("could not create unique index \"{}\"", ctx.index.name),
    )
    .with_table(ctx.index.schema.clone(), ctx.index.table_name.clone())
    .with_constraint(ctx.index.name.clone())
}

/// 葉の項目の流れ。整列と一意性を検査しながら葉タプルを作る。
struct LeafItems<'a, 'c, 'e> {
    ctx: &'a BtCtx<'c>,
    entries: Peekable<&'e mut (dyn Iterator<Item = (Vec<Datum>, Tid)> + 'e)>,
    prev: Option<(Vec<Datum>, Tid)>,
    unique: bool,
    count: u64,
}

impl Iterator for LeafItems<'_, '_, '_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let (key, tid) = self.entries.next()?;
        let index = self.ctx.index;
        let tuple = match form_index_tuple(index, &key, tid) {
            Ok(t) => t,
            Err(e) => return Some(Err(e)),
        };
        if let Some((pk, pt)) = &self.prev {
            if cmp_key_tid(index, pk, *pt, &key, tid) != Ordering::Less {
                return Some(Err(Error::internal(format!(
                    "the input of the index build is not sorted (index \"{}\")",
                    index.name
                ))));
            }
            if self.unique
                && !key.iter().any(Datum::is_null)
                && cmp_keys(index, pk, &key) == Ordering::Equal
            {
                return Some(Err(unique_violation_for_build(self.ctx)));
            }
        }
        self.prev = Some((key, tid));
        self.count += 1;
        Some(Ok(tuple))
    }
}

struct Builder<'a, 'c> {
    ctx: &'a BtCtx<'c>,
    w: &'a WriteCtx,
    /// 書き出し待ちのページ（ブロックは連続）。
    batch: Vec<(BlockNumber, Box<Page>)>,
    /// 次に作るページのブロック番号。
    next_block: BlockNumber,
}

impl Builder<'_, '_> {
    /// 1 つのレベルを左から右へ作る。区切り（次のレベルへ渡すピボット）の並びを返す。
    /// 区切りが空 = このレベルは 1 ページで、そのページがルート。
    fn level_pass(
        &mut self,
        items: impl Iterator<Item = Result<Vec<u8>>>,
        level: u32,
    ) -> Result<Vec<Vec<u8>>> {
        let ncols = self.ctx.index.columns.len();
        let is_leaf = level == 0;
        let fill = if is_leaf {
            BT_FILLFACTOR_LEAF
        } else {
            BT_FILLFACTOR_INNER
        };
        let limit = BT_PAGE_USABLE * fill / 100;
        let mut cur: Vec<Vec<u8>> = Vec::new();
        let mut used = 0usize;
        let mut prev: BlockNumber = 0;
        let mut seps: Vec<Vec<u8>> = Vec::new();
        for item in items {
            let item = item?;
            let sz = cost(item.len());
            if cur.len() >= 2 && used + sz > limit {
                // ページを閉じる。最後の項目を次のページの最初へ送る（high key の分の空きができる）。
                let x = cur
                    .pop()
                    .ok_or_else(|| Error::internal("empty build page"))?;
                let blk = self.next_block;
                let (hikey, sep) = if is_leaf {
                    (
                        leaf_to_pivot(&x, 0, ncols),
                        leaf_to_pivot(&x, blk + 1, ncols),
                    )
                } else {
                    (pivot_with_downlink(&x, 0), pivot_with_downlink(&x, blk + 1))
                };
                self.emit_page(level, &cur, Some(&hikey), prev, blk + 1, false)?;
                seps.push(sep);
                prev = blk;
                cur = if is_leaf {
                    vec![x]
                } else {
                    vec![minus_infinity_pivot(IndexTuple(&x).downlink()).to_vec()]
                };
                used = cost(cur[0].len());
            }
            used += sz;
            cur.push(item);
        }
        if cur.is_empty() {
            return Err(Error::internal("an index build level has no items"));
        }
        let root = seps.is_empty();
        self.emit_page(level, &cur, None, prev, 0, root)?;
        Ok(seps)
    }

    fn emit_page(
        &mut self,
        level: u32,
        items: &[Vec<u8>],
        hikey: Option<&[u8]>,
        prev: BlockNumber,
        next: BlockNumber,
        root: bool,
    ) -> Result<()> {
        let mut flags = 0;
        if level == 0 {
            flags |= BTP_LEAF;
        }
        if root {
            flags |= BTP_ROOT;
        }
        let sp = BtSpecial {
            prev,
            next,
            level,
            flags,
            cycleid: 0,
        };
        let refs: Vec<&[u8]> = hikey
            .into_iter()
            .chain(items.iter().map(Vec::as_slice))
            .collect();
        let page = Page::build_with_items(&sp.to_bytes(), &refs)
            .ok_or_else(|| Error::internal("a build page overflowed"))?;
        self.batch.push((self.next_block, page));
        self.next_block += 1;
        if self.batch.len() >= BT_BUILD_BATCH_PAGES {
            self.write_record(None)?;
        }
        Ok(())
    }

    /// 書き出し待ちのページ（と、あればメタ）を 1 本の `BTREE_PAGES` で書く。ページが 32 枚でメタの
    /// 空きがなければ、メタだけの `BTREE_PAGES` を別に書く。
    fn write_record(&mut self, meta: Option<BtMeta>) -> Result<()> {
        let pages = std::mem::take(&mut self.batch);
        match meta {
            Some(m) if pages.len() >= BT_BUILD_BATCH_PAGES => {
                self.write_blocks(&pages, None)?;
                self.write_blocks(&[], Some(m))
            }
            m => self.write_blocks(&pages, m),
        }
    }

    fn write_blocks(&self, pages: &[(BlockNumber, Box<Page>)], meta: Option<BtMeta>) -> Result<()> {
        if pages.is_empty() && meta.is_none() {
            return Ok(());
        }
        let ctx = self.ctx;
        // ピンだけを先に全部取る（拡張はラッチを持たずに行う）。ブロック 1 は init_index が作った葉。
        let mut pins: Vec<PinnedBuffer> = Vec::with_capacity(pages.len() + 1);
        for (blk, _) in pages {
            let pin = if *blk == BT_FIRST_ROOT_BLOCK {
                ctx.pool.read_buffer(ctx.tag(*blk))?
            } else {
                ctx.pool.extend(ctx.index.locator, ForkNumber::Main)?
            };
            if pin.tag().block != *blk {
                return Err(Error::internal(format!(
                    "index build expected block {blk} but got block {}",
                    pin.tag().block
                )));
            }
            pins.push(pin);
        }
        if meta.is_some() {
            pins.push(ctx.pool.read_buffer(ctx.tag(BT_META_BLOCK))?);
        }
        let mut guards = Vec::with_capacity(pins.len());
        for pin in &pins {
            guards.push((pin.tag(), pin.write_tree()?));
        }
        let cs = CriticalSection::enter(ctx.pool);
        for ((_, g), (_, img)) in guards.iter_mut().zip(pages) {
            *g.page_mut() = (**img).clone();
        }
        if let (Some(m), Some((_, g))) = (meta, guards.last_mut()) {
            *g.page_mut() = *m.to_page();
        }
        log_pages(ctx.wal, self.w, PagesReason::Build, &mut guards).map_err(|e| cs.escalate(e))?;
        Ok(())
    }
}

/// `IndexStore::build` の本体。`init_index` 済みで項目のない索引にだけ使える。
pub(crate) fn build_index(
    ctx: &BtCtx<'_>,
    w: &WriteCtx,
    entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>,
    unique: BuildUnique,
) -> Result<BuildStats> {
    let rel = ctx.index.locator;
    let nblocks = ctx.pool.nblocks(rel, ForkNumber::Main)?;
    if nblocks != 2 {
        return Err(Error::internal(format!(
            "index \"{}\" must be freshly initialized to be built (it has {nblocks} blocks)",
            ctx.index.name
        )));
    }
    {
        let pin = ctx.pool.read_buffer(ctx.tag(BT_FIRST_ROOT_BLOCK))?;
        let g = pin.read_tree()?;
        let sp = validate_page(ctx, &g, BT_FIRST_ROOT_BLOCK, Some(0))?;
        if g.max_offset() != 0 || !sp.is_root() {
            return Err(Error::internal(format!(
                "index \"{}\" must be empty to be built",
                ctx.index.name
            )));
        }
    }

    let mut leaf = LeafItems {
        ctx,
        entries: entries.peekable(),
        prev: None,
        unique: unique == BuildUnique::Yes,
        count: 0,
    };
    if leaf.entries.peek().is_none() {
        return Ok(BuildStats {
            tuples: 0,
            pages: nblocks,
            levels: 0,
        });
    }

    let mut b = Builder {
        ctx,
        w,
        batch: Vec::new(),
        next_block: BT_FIRST_ROOT_BLOCK,
    };
    let mut level = 0u32;
    let mut first_block = BT_FIRST_ROOT_BLOCK;
    let mut seps = b.level_pass(&mut leaf, 0)?;
    let tuples = leaf.count;
    while !seps.is_empty() {
        level += 1;
        if level >= BT_MAX_LEVEL {
            return Err(Error::internal("an index build exceeded the maximum level"));
        }
        let this_first = b.next_block;
        let input = std::iter::once(Ok(minus_infinity_pivot(first_block).to_vec()))
            .chain(seps.into_iter().map(Ok));
        seps = b.level_pass(input, level)?;
        first_block = this_first;
    }
    // 最後に作ったページがルート。1 ページだけの木はメタ（root = 1、level = 0）を書き換えない。
    let root = b.next_block - 1;
    let meta = (root != BT_FIRST_ROOT_BLOCK || level != 0).then_some(BtMeta { root, level });
    b.write_record(meta)?;
    Ok(BuildStats {
        tuples,
        pages: ctx.pool.nblocks(rel, ForkNumber::Main)?,
        levels: level,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

    use proptest::prelude::*;

    use super::*;
    use crate::error::sqlstate;
    use crate::storage::btree::check::{check_structure, dump_entries};
    use crate::storage::btree::testing::{TestIndex, new_index, put, tid_of, tree_level};
    use crate::storage::btree::wal::{BTREE_PAGES, PagesMain};
    use crate::storage::{ResolvedScanKeys, ScanDirection};
    use crate::types::SqlType;
    use crate::wal::RmgrId;

    type Entry = (Vec<Datum>, Tid);

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];

    fn int(v: i64) -> Datum {
        Datum::Int4(i32::try_from(v).unwrap())
    }

    fn ints(n: u32) -> Vec<Entry> {
        (1..=n)
            .map(|i| (vec![int(i64::from(i))], tid_of(i)))
            .collect()
    }

    fn do_build(t: &TestIndex, entries: Vec<Entry>, unique: BuildUnique) -> Result<BuildStats> {
        let mut it = entries.into_iter();
        build_index(&t.ctx(), &t.write_ctx(), &mut it, unique)
    }

    fn built(cols: &[(SqlType, bool, bool)], entries: Vec<Entry>) -> (TestIndex, BuildStats) {
        let t = new_index(cols, false);
        let s = do_build(&t, entries, BuildUnique::No).unwrap();
        (t, s)
    }

    fn all_tids(t: &TestIndex, dir: ScanDirection) -> Vec<Tid> {
        let mut s =
            crate::storage::btree::scan::begin(&t.handle, &ResolvedScanKeys::default(), dir)
                .unwrap();
        let mut out = Vec::new();
        while let Some(x) = crate::storage::btree::scan::next(t.pool(), t.wal(), &mut s).unwrap() {
            out.push(x);
        }
        out
    }

    fn page(t: &TestIndex, blk: u32) -> Box<Page> {
        let pin = t.pool().read_buffer(t.ctx().tag(blk)).unwrap();
        let g = pin.read_tree().unwrap();
        Box::new(Page(g.0))
    }

    fn build_records(t: &TestIndex) -> Vec<crate::wal::DecodedRecord> {
        t.read_wal_from(t.start)
            .into_iter()
            .filter(|r| r.rmgr == RmgrId::Btree && r.info == BTREE_PAGES)
            .filter(|r| PagesMain::decode(&r.main).unwrap().reason == PagesReason::Build)
            .collect()
    }

    #[test]
    fn empty_input_does_nothing() {
        let t = new_index(&INT, false);
        let before = t.read_wal_from(t.start).len();
        let s = do_build(&t, vec![], BuildUnique::No).unwrap();
        assert_eq!(
            s,
            BuildStats {
                tuples: 0,
                pages: 2,
                levels: 0
            }
        );
        assert_eq!(t.read_wal_from(t.start).len(), before);
        assert!(build_records(&t).is_empty());
        check_structure(t.pool(), &t.handle).unwrap();
    }

    #[test]
    fn one_entry_and_one_page() {
        let (t, s) = built(&INT, ints(1));
        assert_eq!(
            s,
            BuildStats {
                tuples: 1,
                pages: 2,
                levels: 0
            }
        );
        // メタは書き換えない（root = 1、level = 0）。BTREE_PAGES は葉 1 枚だけ。
        let recs = build_records(&t);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].blocks.len(), 1);
        assert_eq!(recs[0].blocks[0].tag.block, 1);
        assert_eq!(check_structure(t.pool(), &t.handle).unwrap().items, 1);
        // 1 ページに収まる最大（葉 90% = 7336 バイト）。
        let (t, s) = built(&INT, ints(366));
        assert_eq!((s.pages, s.levels), (2, 0));
        assert_eq!(page(&t, 1).max_offset(), 366);
        t.assert_clean();
        // 1 件増えると 2 枚になる。
        let (t, s) = built(&INT, ints(367));
        assert_eq!((s.pages, s.levels), (4, 1));
        check_structure(t.pool(), &t.handle).unwrap();
    }

    #[test]
    fn fill_factors_are_respected() {
        let (t, s) = built(&INT, ints(100_000));
        assert_eq!(s.levels, 2);
        let info = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(info.items, 100_000);
        let n = t
            .pool()
            .nblocks(t.handle.locator, ForkNumber::Main)
            .unwrap();
        let mut leaves = 0u32;
        let mut internal = 0;
        for b in 1..n {
            let p = page(&t, b);
            let sp = BtSpecial::read(&p).unwrap();
            let first = sp.first_data_offset();
            let used: usize = (first..=p.max_offset())
                .map(|o| cost(p.item(o).unwrap().len()))
                .sum();
            if sp.is_leaf() {
                leaves += 1;
                assert!(used <= 7336, "leaf {b}: {used}");
                if sp.next != 0 {
                    assert!(used > 7336 - 40, "leaf {b} is underfilled: {used}");
                }
            } else {
                internal += 1;
                assert!(used <= 5706, "internal {b}: {used}");
            }
        }
        assert_eq!(leaves, info.leaf_pages);
        assert!(internal >= 2);
        t.assert_clean();
    }

    /// 木のページ数が `target` 以上になる最小の件数。
    fn smallest_n_with_pages(target: u32) -> u32 {
        let (mut lo, mut hi) = (1u32, 400 * target);
        while lo < hi {
            let mid = u32::midpoint(lo, hi);
            let (_, s) = built(&INT, ints(mid));
            if s.pages > target {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    #[test]
    fn batches_of_32_pages_and_the_meta_record() {
        for target in [31u32, 32, 33, 64, 65] {
            let n = smallest_n_with_pages(target);
            let (t, s) = built(&INT, ints(n));
            let tree_pages = s.pages - 1;
            assert_eq!(tree_pages, target, "n = {n}");
            let recs = build_records(&t);
            // 全ブロックが連続（1, 2, ...）で、1 レコードは 32 枚まで（メタを足して 33 まで）。
            let mut expect = 1u32;
            for (i, r) in recs.iter().enumerate() {
                let last = i + 1 == recs.len();
                let blocks: Vec<u32> = r.blocks.iter().map(|b| b.tag.block).collect();
                let (pages, meta): (&[u32], bool) = match blocks.split_last() {
                    Some((0, rest)) => (rest, true),
                    _ => (&blocks[..], false),
                };
                assert!(pages.len() <= 32, "{blocks:?}");
                assert!(
                    !meta || last,
                    "the meta block must be in the last record only"
                );
                for b in pages {
                    assert_eq!(*b, expect);
                    expect += 1;
                }
                if !last {
                    assert_eq!(pages.len(), 32);
                }
                assert!(r.blocks.iter().all(|b| b.image.is_some()));
            }
            assert_eq!(expect - 1, tree_pages);
            // ルートが 1 ページでなければメタが最後のレコードの最後にある。
            let last = recs.last().unwrap();
            assert_eq!(last.blocks.last().unwrap().tag.block, 0);
            if tree_pages % 32 == 0 {
                // 32 枚で空きがなければ、メタだけの BTREE_PAGES を別に書く。
                assert_eq!(last.blocks.len(), 1, "target {target}");
                assert_eq!(recs.len(), usize::try_from(tree_pages / 32 + 1).unwrap());
            } else {
                assert_eq!(recs.len(), usize::try_from(tree_pages / 32 + 1).unwrap());
            }
            check_structure(t.pool(), &t.handle).unwrap();
            assert_eq!(dump_entries(t.pool(), &t.handle).unwrap().len(), n as usize);
            t.assert_clean();
        }
    }

    #[test]
    fn stats_and_levels() {
        let (t, s) = built(&INT, ints(100_000));
        assert_eq!(s.tuples, 100_000);
        assert_eq!(
            s.pages,
            t.pool()
                .nblocks(t.handle.locator, ForkNumber::Main)
                .unwrap()
        );
        assert_eq!(s.levels, 2);
        assert_eq!(tree_level(&t), 2);
    }

    #[test]
    fn wide_keys_make_a_tall_tree() {
        let cols = [(SqlType::TEXT, false, false)];
        let key = |i: u32| vec![Datum::Text(format!("{i:05}{}", "x".repeat(2600)))];
        let entries: Vec<Entry> = (0..400).map(|i| (key(i), tid_of(i + 1))).collect();
        let (t, s) = built(&cols, entries);
        assert!(s.levels >= 4, "levels = {}", s.levels);
        let info = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(info.items, 400);
        // 最大の大きさのキーだけ。
        let max = |i: u32| vec![Datum::Text(format!("{i:05}{}", "y".repeat(2687)))];
        let entries: Vec<Entry> = (0..60).map(|i| (max(i), tid_of(i + 1))).collect();
        let (t, s) = built(&cols, entries);
        assert!(s.levels >= 3);
        assert_eq!(check_structure(t.pool(), &t.handle).unwrap().items, 60);
        // 1800 バイトのキー。
        let k18 = |i: u32| vec![Datum::Text(format!("{i:05}{}", "z".repeat(1795)))];
        let entries: Vec<Entry> = (0..200).map(|i| (k18(i), tid_of(i + 1))).collect();
        let (t, _) = built(&cols, entries);
        assert_eq!(check_structure(t.pool(), &t.handle).unwrap().items, 200);
    }

    #[test]
    fn inserts_after_a_build_keep_the_structure() {
        let (t, _) = built(&INT, ints(5000));
        for i in 0..3000u32 {
            put(&t, &[int(i64::from(i) * 5 / 3)], tid_of(10_000 + i)).unwrap();
        }
        let info = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(info.items, 8000);
        t.assert_clean();
    }

    #[test]
    fn unique_yes_detects_adjacent_duplicates() {
        let t = new_index(&INT, true);
        let mut e = ints(10);
        e.push((vec![int(5)], tid_of(500)));
        e.sort_by(|a, b| crate::storage::btree::cmp_key_tid(&t.handle, &a.0, a.1, &b.0, b.1));
        let err = do_build(&t, e.clone(), BuildUnique::Yes).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(err.message, "could not create unique index \"t_idx\"");
        assert!(err.detail.is_none());
        let diag = err.diag.as_deref().unwrap();
        assert_eq!(diag.schema.as_deref(), Some("public"));
        assert_eq!(diag.table.as_deref(), Some("t"));
        assert_eq!(diag.constraint.as_deref(), Some("t_idx"));
        // No は重複を通す。
        let t = new_index(&INT, true);
        let s = do_build(&t, e, BuildUnique::No).unwrap();
        assert_eq!(s.tuples, 11);
        // NULL を含むキーの重複は成功する。
        let t = new_index(&INT, true);
        let nulls: Vec<Entry> = (1..=5).map(|i| (vec![Datum::Null], tid_of(i))).collect();
        assert_eq!(do_build(&t, nulls, BuildUnique::Yes).unwrap().tuples, 5);
        let cols2 = [(SqlType::INT4, false, false), (SqlType::INT4, false, false)];
        let t = new_index(&cols2, true);
        let e2 = vec![
            (vec![int(1), Datum::Null], tid_of(1)),
            (vec![int(1), Datum::Null], tid_of(2)),
            (vec![int(1), int(2)], tid_of(3)),
        ];
        // 並びは (1, 2), (1, NULL), (1, NULL)（NULLS LAST）。
        let mut e2s = e2;
        e2s.sort_by(|a, b| crate::storage::btree::cmp_key_tid(&t.handle, &a.0, a.1, &b.0, b.1));
        assert_eq!(do_build(&t, e2s, BuildUnique::Yes).unwrap().tuples, 3);
    }

    #[test]
    fn unsorted_or_repeated_input_is_an_internal_error() {
        let t = new_index(&INT, false);
        let e = vec![(vec![int(2)], tid_of(1)), (vec![int(1)], tid_of(2))];
        let err = do_build(&t, e, BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        let t = new_index(&INT, false);
        let e = vec![(vec![int(1)], tid_of(1)), (vec![int(1)], tid_of(1))];
        let err = do_build(&t, e, BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        // 同じキーで TID の昇順は可。
        let t = new_index(&INT, false);
        let e = vec![(vec![int(1)], tid_of(1)), (vec![int(1)], tid_of(2))];
        assert_eq!(do_build(&t, e, BuildUnique::No).unwrap().tuples, 2);
    }

    #[test]
    fn preconditions_are_checked() {
        // init_index 前。
        let t = crate::storage::btree::testing::test_index(&INT, false);
        let err = do_build(&t, ints(3), BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        // 項目のある索引。
        let t = new_index(&INT, false);
        put(&t, &[int(1)], tid_of(1)).unwrap();
        let err = do_build(&t, ints(3), BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
        // 2 回目の構築。
        let (t, _) = built(&INT, ints(3000));
        let err = do_build(&t, ints(3), BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn too_large_a_row_is_54000_with_the_entry_tid() {
        let cols = [(SqlType::TEXT, false, false)];
        let t = new_index(&cols, false);
        let e = vec![
            (vec![Datum::Text("a".into())], tid_of(1)),
            (vec![Datum::Text("b".repeat(2693))], tid_of(2)),
        ];
        let err = do_build(&t, e, BuildUnique::No).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            err.message
                .starts_with("index row size 2712 exceeds btree version 4 maximum 2704")
        );
        assert_eq!(
            err.detail.as_deref(),
            Some("Index row references tuple (0,3) in relation \"t\".")
        );
        // 何も書かれていない。
        assert!(build_records(&t).is_empty());
    }

    #[test]
    fn redo_of_the_build_records_reproduces_the_pages() {
        let (t, _) = built(&INT, ints(20_000));
        let recs = t.read_wal_from(t.start);
        let t2 = crate::storage::btree::testing::test_index(&INT, false);
        let ctx = t2.redo_ctx();
        for r in recs.iter().filter(|r| r.rmgr == RmgrId::Btree) {
            crate::storage::btree::wal::redo(&ctx, r).unwrap();
        }
        let (a, b) = (t.snapshot_pages(), t2.snapshot_pages());
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert!(x.0[..] == y.0[..], "block {i} differs");
        }
        // 説明も出せる。
        let d = crate::storage::btree::wal::describe(
            recs.iter().rev().find(|r| r.info == BTREE_PAGES).unwrap(),
        );
        assert!(d.starts_with("BTREE_PAGES reason=BUILD"), "{d}");
    }

    #[test]
    fn build_equals_sequential_inserts() {
        let cols = [(SqlType::INT4, true, true), (SqlType::TEXT, false, false)];
        let mut rng = crate::storage::btree::testing::Lcg(5);
        let entries: Vec<Entry> = (0..3000u32)
            .map(|i| {
                let a = if rng.below(10) == 0 {
                    Datum::Null
                } else {
                    int(i64::try_from(rng.below(50)).unwrap())
                };
                let b = if rng.below(10) == 0 {
                    Datum::Null
                } else {
                    Datum::Text(format!("v{}", rng.below(200)))
                };
                (vec![a, b], tid_of(i + 1))
            })
            .collect();
        let t1 = new_index(&cols, false);
        let mut sorted = entries.clone();
        sorted.sort_by(|a, b| crate::storage::btree::cmp_key_tid(&t1.handle, &a.0, a.1, &b.0, b.1));
        do_build(&t1, sorted, BuildUnique::No).unwrap();
        let t2 = new_index(&cols, false);
        for (k, tid) in &entries {
            put(&t2, k, *tid).unwrap();
        }
        let (d1, d2) = (
            dump_entries(t1.pool(), &t1.handle).unwrap(),
            dump_entries(t2.pool(), &t2.handle).unwrap(),
        );
        assert_eq!(d1.len(), d2.len());
        for (x, y) in d1.iter().zip(&d2) {
            assert_eq!(x.1, y.1);
        }
        for dir in [ScanDirection::Forward, ScanDirection::Backward] {
            assert_eq!(all_tids(&t1, dir), all_tids(&t2, dir));
        }
        check_structure(t1.pool(), &t1.handle).unwrap();
        check_structure(t2.pool(), &t2.handle).unwrap();
    }

    // ----- 性質テスト（§7.3: 一括構築） -----------------------------------------------

    fn schema(i: usize) -> Vec<(SqlType, bool, bool)> {
        match i {
            0 => vec![(SqlType::INT4, false, false)],
            1 => vec![(SqlType::TEXT, true, false)],
            2 => vec![(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            3 => vec![(SqlType::INT4, true, true)],
            4 => vec![(SqlType::INT4, false, false), (SqlType::INT4, false, false)],
            _ => vec![(SqlType::BPCHAR, false, false)],
        }
    }

    fn key_for(schema_i: usize, a: u32, b: u32, nulls: bool) -> Vec<Datum> {
        let n = |d: Datum| {
            if nulls && a.is_multiple_of(7) {
                Datum::Null
            } else {
                d
            }
        };
        match schema_i {
            0 | 3 => vec![n(int(i64::from(a)))],
            1 => vec![n(Datum::Text(format!("s{a:06}")))],
            2 => vec![n(int(i64::from(a % 40))), Datum::Text(format!("t{b}"))],
            4 => vec![n(int(i64::from(a % 5))), int(i64::from(b))],
            _ => vec![n(Datum::BpChar(format!("c{}  ", a % 300)))],
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn built_tree_matches_the_model(
            schema_i in 0usize..6,
            n in 0u32..4000,
            mode in 0u8..4,
            nulls in any::<bool>(),
            seed in any::<u64>(),
        ) {
            let cols = schema(schema_i);
            let t = new_index(&cols, false);
            let mut rng = crate::storage::btree::testing::Lcg(seed);
            let mut entries: Vec<Entry> = (0..n)
                .map(|i| {
                    let a = match mode {
                        0 => i,
                        1 => n - i,
                        2 => rng.below(100_000) as u32,
                        _ => rng.below(30) as u32,
                    };
                    (key_for(schema_i, a, rng.below(400) as u32, nulls), tid_of(i + 1))
                })
                .collect();
            entries.sort_by(|a, b| crate::storage::btree::cmp_key_tid(&t.handle, &a.0, a.1, &b.0, b.1));
            let stats = do_build(&t, entries.clone(), BuildUnique::No).unwrap();
            prop_assert_eq!(stats.tuples, u64::from(n));
            let info = check_structure(t.pool(), &t.handle).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            prop_assert_eq!(info.items, u64::from(n));
            let dumped = dump_entries(t.pool(), &t.handle).unwrap();
            prop_assert_eq!(dumped.len(), entries.len());
            for (d, e) in dumped.iter().zip(&entries) {
                prop_assert_eq!(d.1, e.1);
                prop_assert_eq!(
                    crate::storage::btree::cmp_keys(&t.handle, &d.0, &e.0),
                    std::cmp::Ordering::Equal
                );
            }
            let fwd = all_tids(&t, ScanDirection::Forward);
            let mut bwd = all_tids(&t, ScanDirection::Backward);
            bwd.reverse();
            prop_assert_eq!(&fwd, &bwd);
            prop_assert_eq!(fwd, entries.iter().map(|e| e.1).collect::<Vec<_>>());
        }
    }
}
