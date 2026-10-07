//! ページの分割と親への伝播・ルート分割（`BTREE_PAGES` の `SPLIT`。`m4/06-btree.md` §5.5、§5.6）。
//!
//! 分割の連鎖全体を 1 レコードで書く。必要なページをすべて確保・ラッチし、全画像をメモリ上で組み立ててから
//! 適用するので、未完了の分割という状態は存在しない。

use super::insert::maxalign;
use super::meta::BtMeta;
use super::page::{BtSpecial, item, validate_page};
use super::search::StackEntry;
use super::tuple::{IndexTuple, leaf_to_pivot, minus_infinity_pivot, pivot_with_downlink};
use super::wal::{PagesReason, log_pages};
use super::{
    BT_FILLFACTOR_LEAF, BT_MAX_LEVEL, BT_META_BLOCK, BT_P_HIKEY, BT_PAGE_USABLE, BTP_LEAF,
    BTP_ROOT, BtCtx,
};
use crate::error::{Error, Result, sqlstate};
use crate::storage::WriteCtx;
use crate::storage::buffer::{CriticalSection, PageWriteGuard, PinnedBuffer};
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber};
use crate::wal::MAX_BLOCK_REFS;

#[cfg(test)]
thread_local! {
    /// テスト用: 1 レコードのブロック数の上限の上書き（06 §7.4 の `54000` の試験）。
    pub(crate) static MAX_REFS_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

fn max_refs() -> usize {
    #[cfg(test)]
    if let Some(n) = MAX_REFS_OVERRIDE.with(std::cell::Cell::get) {
        return n;
    }
    MAX_BLOCK_REFS
}

/// 1 項目の占有（`MAXALIGN(長さ) + 行ポインタ 4`）。
fn cost(len: usize) -> usize {
    maxalign(len) + 4
}

/// 分割点 `k` を選ぶ（06 §5.5）。`sizes` は新しい項目を入れた後のデータ項目の大きさ（内部ページの
/// `sizes[0]` は -∞ ピボットの 8）、`hk_right` は元のページの high key の大きさ、`newpos` は `sizes` の中の
/// 新しい項目の位置。左 = `items[..k]` + high key（`items[k]` から作る）、右 = `items[k..]`。
#[allow(clippy::needless_range_loop)]
pub(crate) fn choose_split(
    sizes: &[usize],
    hk_right: Option<usize>,
    rightmost: bool,
    is_leaf: bool,
    newpos: usize,
) -> Result<usize> {
    let n = sizes.len();
    let total: usize = sizes.iter().map(|&s| cost(s)).sum();
    let mut prefix = 0usize;
    let mut best: Option<(usize, usize)> = None; // (差, k)
    // 内部ページは両側に子が 2 つ以上残る k を優先する（1 つだけの子のページが連なって木が高くなるのを避ける）。
    let mut best_wide: Option<(usize, usize)> = None;
    let mut append_best: Option<usize> = None;
    let mut min_feasible: Option<usize> = None;
    let append = is_leaf && rightmost && n >= 1 && newpos == n - 1;
    let fill_limit = BT_PAGE_USABLE * BT_FILLFACTOR_LEAF / 100;
    for k in 0..n {
        if k >= 1 {
            let sep_len = if is_leaf {
                maxalign(sizes[k]) + 8
            } else {
                sizes[k]
            };
            let left = cost(sep_len) + prefix;
            let mut right = hk_right.map_or(0, cost) + (total - prefix);
            if !is_leaf {
                right = right + 12 - cost(sizes[k]);
            }
            if left <= BT_PAGE_USABLE && right <= BT_PAGE_USABLE {
                if min_feasible.is_none() {
                    min_feasible = Some(k);
                }
                if left <= fill_limit {
                    append_best = Some(k);
                }
                let diff = left.abs_diff(right);
                if !is_leaf && k >= 2 && n - k >= 2 && best_wide.is_none_or(|(d, _)| diff < d) {
                    best_wide = Some((diff, k));
                }
                if best.is_none_or(|(d, _)| diff < d) {
                    best = Some((diff, k));
                }
            }
        }
        prefix += cost(sizes[k]);
    }
    let k = if append {
        append_best.or(min_feasible)
    } else {
        best_wide.or(best).map(|(_, k)| k)
    };
    k.ok_or_else(|| Error::internal("btree page cannot be split: no feasible split point"))
}

/// 計画の 1 レベル。
struct Level {
    block: BlockNumber,
    /// 計画時の元のページの写し。
    page: Box<Page>,
    sp: BtSpecial,
    plan: Plan,
}

enum Plan {
    Split {
        /// 新しい項目を入れた後のデータ項目（レベル 1 以上は `ins_idx` の位置が、下のレベルの区切り）。
        items: Vec<Vec<u8>>,
        ins_idx: usize,
        k: usize,
        q: Option<(BlockNumber, Box<Page>)>,
    },
    Absorb {
        ins_off: u16,
    },
}

#[derive(Clone, Copy)]
enum Role {
    /// 元のページ `P_l`（ランク `2l`）。
    Left(usize),
    /// 新しい右ページ `R_l`。
    Right(usize),
    /// 元の右隣 `Q_l`（ランク `2l + 1`）。
    Neighbor(usize),
    NewRoot,
    Meta,
}

struct Slot {
    role: Role,
    /// 元のページ（なければ新規）のブロック。
    old_block: Option<BlockNumber>,
    /// 計画時の `(pd_lsn, special.next)`。
    expect: Option<(u64, BlockNumber)>,
}

impl Slot {
    /// ラッチを取る順（下 → 上、左 → 右、メタは木のページの後、新しいページは最後）。
    fn rank(&self) -> usize {
        match self.role {
            Role::Left(l) => 2 * l,
            Role::Neighbor(l) => 2 * l + 1,
            Role::Meta => 100_000,
            Role::Right(i) => 200_000 + i,
            Role::NewRoot => 300_000,
        }
    }
}

fn copy_page(p: &Page) -> Box<Page> {
    Box::new(p.clone())
}

fn page_items(ctx: &BtCtx<'_>, page: &Page, block: BlockNumber, from: u16) -> Result<Vec<Vec<u8>>> {
    (from..=page.max_offset())
        .map(|off| Ok(item(ctx, page, block, off)?.to_vec()))
        .collect()
}

/// 葉（または親）の項目のうち、分割の後に上へ渡す区切りの形を作る。`downlink` は右ページ。
fn separator(ctx: &BtCtx<'_>, is_leaf: bool, it: &[u8], downlink: BlockNumber) -> Vec<u8> {
    if is_leaf {
        leaf_to_pivot(it, downlink, ctx.index.columns.len())
    } else {
        pivot_with_downlink(it, downlink)
    }
}

/// 親のダウンリンクを探す（`_bt_getstackbuf`）。`(親のブロック, 写し, ダウンリンクの行ポインタ番号)`。
fn find_parent(
    ctx: &BtCtx<'_>,
    entry: StackEntry,
    child: BlockNumber,
    level: u32,
) -> Result<(BlockNumber, Box<Page>, u16)> {
    let nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut blk = entry.block;
    let mut hops = 0u32;
    loop {
        let pin = ctx.pool.read_buffer(ctx.tag(blk))?;
        let g = pin.read_tree()?;
        let sp = validate_page(ctx, &g, blk, Some(level))?;
        let max = g.max_offset();
        let first = sp.first_data_offset();
        let is_link = |off: u16| -> Result<bool> {
            let t = IndexTuple(item(ctx, &g, blk, off)?);
            Ok(t.is_pivot() && t.downlink() == child)
        };
        if blk == entry.block
            && entry.offset >= first
            && entry.offset <= max
            && is_link(entry.offset)?
        {
            return Ok((blk, copy_page(&g), entry.offset));
        }
        for off in first..=max {
            if is_link(off)? {
                return Ok((blk, copy_page(&g), off));
            }
        }
        if sp.next == 0 {
            return Err(Error::internal(format!(
                "could not find parent downlink for block {child} in index \"{}\"",
                ctx.index.name
            )));
        }
        blk = sp.next;
        hops += 1;
        if hops > nblocks {
            return Err(ctx.corrupted(blk, "sibling chain is cyclic"));
        }
    }
}

fn too_many_levels(ctx: &BtCtx<'_>) -> Error {
    Error::new(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "index \"{}\" cannot be split: too many levels",
            ctx.index.name
        ),
    )
}

/// 葉 `g`（排他ガード）に入らない `tuple`（行ポインタ番号 `pos`）を、分割して入れる（§5.5）。
#[allow(clippy::too_many_lines)]
pub(crate) fn split_and_insert(
    ctx: &BtCtx<'_>,
    w: &WriteCtx,
    pin: &PinnedBuffer,
    g: PageWriteGuard<'_>,
    stack: &[StackEntry],
    tuple: &[u8],
    pos: u16,
) -> Result<()> {
    let leaf_block = pin.tag().block;

    // ----- フェーズ 1: 計画（ページは変えない）-----
    let mut stack: Vec<StackEntry> = stack.to_vec();
    let mut levels: Vec<Level> = Vec::new();
    let mut cur_block = leaf_block;
    let mut cur_page = copy_page(g.page());
    let mut ins_off = pos;
    let mut ins_item: Vec<u8> = tuple.to_vec();
    let mut new_root = false;
    let mut l = 0usize;
    loop {
        let sp = BtSpecial::read(&cur_page)
            .ok_or_else(|| ctx.corrupted(cur_block, "no special area"))?;
        let first = sp.first_data_offset();
        if l >= 1 && cur_page.free_space_unbounded() >= maxalign(ins_item.len()) {
            levels.push(Level {
                block: cur_block,
                page: cur_page,
                sp,
                plan: Plan::Absorb { ins_off },
            });
            break;
        }
        let mut items = page_items(ctx, &cur_page, cur_block, first)?;
        let ins_idx = usize::from(ins_off - first);
        if ins_idx > items.len() {
            return Err(Error::internal("btree insert position is out of range"));
        }
        items.insert(ins_idx, ins_item);
        let sizes: Vec<usize> = items.iter().map(Vec::len).collect();
        let hk = if sp.next != 0 {
            Some(item(ctx, &cur_page, cur_block, BT_P_HIKEY)?.len())
        } else {
            None
        };
        let is_leaf = sp.is_leaf();
        let k = choose_split(&sizes, hk, sp.is_rightmost(), is_leaf, ins_idx)?;
        let q = if sp.next != 0 {
            let qpin = ctx.pool.read_buffer(ctx.tag(sp.next))?;
            let qg = qpin.read_tree()?;
            validate_page(ctx, &qg, sp.next, Some(sp.level))?;
            Some((sp.next, copy_page(&qg)))
        } else {
            None
        };
        let next_ins = separator(ctx, is_leaf, &items[k], 0);
        let root = sp.is_root();
        levels.push(Level {
            block: cur_block,
            page: cur_page,
            sp,
            plan: Plan::Split {
                items,
                ins_idx,
                k,
                q,
            },
        });
        if root {
            if !stack.is_empty() {
                return Err(Error::internal(
                    "btree root page found with a non-empty stack",
                ));
            }
            let mp = ctx.pool.read_buffer(ctx.tag(BT_META_BLOCK))?;
            let mg = mp.read_tree()?;
            let meta = BtMeta::read(ctx, &mg)?;
            if meta.root != cur_block {
                return Err(Error::internal(
                    "btree meta root differs from the root page",
                ));
            }
            new_root = true;
            break;
        }
        let entry = stack
            .pop()
            .ok_or_else(|| Error::internal("btree descent stack is empty for a non-root page"))?;
        let child_level = levels[l].sp.level;
        let (pblk, ppage, dl_off) = find_parent(ctx, entry, cur_block, child_level + 1)?;
        cur_block = pblk;
        cur_page = ppage;
        ins_off = dl_off + 1;
        ins_item = next_ins;
        l += 1;
    }

    // スロット（`log_pages` の登録順 = §3.7 の block_id 順）。
    let mut slots: Vec<Slot> = Vec::new();
    let mut nsplit = 0usize;
    for (l, lv) in levels.iter().enumerate() {
        let expect = Some((lv.page.lsn(), lv.sp.next));
        match &lv.plan {
            Plan::Split { q, .. } => {
                slots.push(Slot {
                    role: Role::Left(l),
                    old_block: Some(lv.block),
                    expect,
                });
                slots.push(Slot {
                    role: Role::Right(nsplit),
                    old_block: None,
                    expect: None,
                });
                nsplit += 1;
                if let Some((qb, qp)) = q {
                    slots.push(Slot {
                        role: Role::Neighbor(l),
                        old_block: Some(*qb),
                        expect: Some((qp.lsn(), BtSpecial::read(qp).map_or(0, |s| s.next))),
                    });
                }
            }
            Plan::Absorb { .. } => slots.push(Slot {
                role: Role::Left(l),
                old_block: Some(lv.block),
                expect,
            }),
        }
    }
    let top = &levels[levels.len() - 1];
    let new_level = top.sp.level + 1;
    if new_root {
        if new_level >= BT_MAX_LEVEL {
            return Err(too_many_levels(ctx));
        }
        slots.push(Slot {
            role: Role::NewRoot,
            old_block: None,
            expect: None,
        });
        slots.push(Slot {
            role: Role::Meta,
            old_block: Some(BT_META_BLOCK),
            expect: None,
        });
    }
    if slots.len() > max_refs() {
        return Err(too_many_levels(ctx));
    }
    let nnew = nsplit + usize::from(new_root);

    // ----- フェーズ 2: 確保（失敗しても木は無傷）-----
    let mut new_pins: Vec<Option<PinnedBuffer>> = Vec::with_capacity(nnew);
    for _ in 0..nnew {
        new_pins.push(Some(
            ctx.pool.extend_tree(ctx.index.locator, ForkNumber::Main)?,
        ));
    }
    let new_blocks: Vec<BlockNumber> = new_pins
        .iter()
        .map(|p| p.as_ref().map_or(0, |p| p.tag().block))
        .collect();

    // ----- フェーズ 3: ラッチと検証 -----
    // ピンを先に作り終えてからガードを作る（ガードがピンを借用する）。スロット 0 は呼び出し側の `g`。
    let mut pins: Vec<Option<PinnedBuffer>> = Vec::with_capacity(slots.len());
    let mut tags: Vec<BufferTag> = Vec::with_capacity(slots.len());
    for (i, s) in slots.iter().enumerate() {
        let p = match s.role {
            Role::Right(j) => new_pins[j].take(),
            Role::NewRoot => new_pins[nsplit].take(),
            _ if i == 0 => None,
            _ => Some(ctx.pool.read_buffer(ctx.tag(s.old_block.unwrap_or(0)))?),
        };
        let tag = match (&p, s.old_block) {
            (Some(p), _) => p.tag(),
            (None, _) => pin.tag(),
        };
        tags.push(tag);
        pins.push(p);
    }
    let mut order: Vec<usize> = (1..slots.len()).collect();
    order.sort_by_key(|&i| slots[i].rank());
    let mut held: Vec<Option<PageWriteGuard<'_>>> = (0..slots.len()).map(|_| None).collect();
    held[0] = Some(g);
    for &i in &order {
        if let Some(p) = pins[i].as_ref() {
            held[i] = Some(p.write_tree()?);
        }
    }
    for (i, s) in slots.iter().enumerate() {
        if let (Some((lsn, next)), Some(g)) = (s.expect, held[i].as_ref()) {
            let cur_next = BtSpecial::read(g.page()).map_or(u32::MAX, |sp| sp.next);
            if g.page().lsn() != lsn || cur_next != next {
                return Err(Error::internal("btree page changed during split planning"));
            }
        }
    }

    // ----- フェーズ 4: 画像の組み立て（メモリ上）-----
    let mut images: Vec<Box<Page>> = Vec::with_capacity(slots.len());
    let mut carry: Option<Vec<u8>> = None;
    let mut r_idx = 0usize;
    let mut top_left = 0;
    let internal = || Error::internal("btree split image does not fit on a page");
    for (l, lv) in levels.iter().enumerate() {
        match &lv.plan {
            Plan::Split {
                items,
                ins_idx,
                k,
                q,
            } => {
                let mut items = items.clone();
                if l >= 1 {
                    items[*ins_idx] = carry
                        .take()
                        .ok_or_else(|| Error::internal("btree split lost a separator"))?;
                }
                let k = *k;
                let is_leaf = lv.sp.is_leaf();
                let rblk = new_blocks[r_idx];
                r_idx += 1;
                let hk_left = separator(ctx, is_leaf, &items[k], 0);
                let up = separator(ctx, is_leaf, &items[k], rblk);
                let left_sp = BtSpecial {
                    prev: lv.sp.prev,
                    next: rblk,
                    level: lv.sp.level,
                    flags: lv.sp.flags & !BTP_ROOT,
                    cycleid: 0,
                };
                let right_sp = BtSpecial {
                    prev: lv.block,
                    next: lv.sp.next,
                    level: lv.sp.level,
                    flags: lv.sp.flags & BTP_LEAF,
                    cycleid: 0,
                };
                let mut left_items: Vec<&[u8]> = Vec::with_capacity(k + 1);
                left_items.push(&hk_left);
                left_items.extend(items[..k].iter().map(Vec::as_slice));
                let orig_hk = if lv.sp.next != 0 {
                    Some(item(ctx, &lv.page, lv.block, BT_P_HIKEY)?.to_vec())
                } else {
                    None
                };
                let first_right = if is_leaf {
                    items[k].clone()
                } else {
                    minus_infinity_pivot(IndexTuple(&items[k]).downlink()).to_vec()
                };
                let mut right_items: Vec<&[u8]> = Vec::with_capacity(items.len() - k + 1);
                if let Some(h) = &orig_hk {
                    right_items.push(h);
                }
                right_items.push(&first_right);
                right_items.extend(items[k + 1..].iter().map(Vec::as_slice));
                images.push(
                    Page::build_with_items(&left_sp.to_bytes(), &left_items)
                        .ok_or_else(internal)?,
                );
                images.push(
                    Page::build_with_items(&right_sp.to_bytes(), &right_items)
                        .ok_or_else(internal)?,
                );
                if let Some((_, qp)) = q {
                    let mut qp = qp.clone();
                    let mut qsp = BtSpecial::read(&qp).ok_or_else(internal)?;
                    qsp.prev = rblk;
                    qsp.write(&mut qp);
                    images.push(qp);
                }
                carry = Some(up);
                top_left = lv.block;
            }
            Plan::Absorb { ins_off } => {
                let sep = carry
                    .take()
                    .ok_or_else(|| Error::internal("btree split lost a separator"))?;
                let mut p = lv.page.clone();
                p.insert_item_at(*ins_off, &sep).ok_or_else(internal)?;
                images.push(p);
            }
        }
    }
    if new_root {
        let sep = carry
            .take()
            .ok_or_else(|| Error::internal("btree split lost a separator"))?;
        let root_blk = new_blocks[nsplit];
        let root_sp = BtSpecial {
            prev: 0,
            next: 0,
            level: new_level,
            flags: BTP_ROOT,
            cycleid: 0,
        };
        let inf = minus_infinity_pivot(top_left);
        images.push(
            Page::build_with_items(&root_sp.to_bytes(), &[&inf[..], &sep[..]])
                .ok_or_else(internal)?,
        );
        images.push(
            BtMeta {
                root: root_blk,
                level: new_level,
            }
            .to_page(),
        );
    }
    debug_assert_eq!(images.len(), slots.len());

    // ----- フェーズ 5: 適用（CriticalSection）-----
    let mut guards: Vec<(BufferTag, PageWriteGuard<'_>)> = Vec::with_capacity(slots.len());
    for (i, h) in held.into_iter().enumerate() {
        let g = h.ok_or_else(|| Error::internal("btree split slot without a latch"))?;
        guards.push((tags[i], g));
    }
    let cs = CriticalSection::enter(ctx.pool);
    for ((_, g), image) in guards.iter_mut().zip(&images) {
        g.page_mut().0.copy_from_slice(&image.0);
    }
    if ctx.knobs.btree_split_in_two_records && guards.len() >= 2 {
        // 変異: 左ページと残りを別のレコードに分ける（分割の原子性を壊す。I14）。
        let (first, rest) = guards.split_at_mut(1);
        log_pages(ctx.wal, w, PagesReason::Split, first).map_err(|e| cs.escalate(e))?;
        log_pages(ctx.wal, w, PagesReason::Split, rest).map_err(|e| cs.escalate(e))?;
    } else {
        log_pages(ctx.wal, w, PagesReason::Split, &mut guards).map_err(|e| cs.escalate(e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    use super::*;
    use crate::storage::btree::testing::{
        Lcg, TestIndex, check_tree, new_index, new_index_with, put, tid_of, tree_level,
    };
    use crate::storage::btree::wal::{BTREE_PAGES, PagesReason};
    use crate::storage::testing::TestStorage;
    use crate::types::Tid;
    use crate::types::{Datum, SqlType};
    use crate::wal::{DecodedRecord, RmgrId};

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];

    fn ins_int(t: &TestIndex, k: i32, n: u32) {
        put(t, &[Datum::Int4(k)], tid_of(n)).unwrap();
    }

    fn split_records(recs: &[DecodedRecord]) -> Vec<&DecodedRecord> {
        recs.iter()
            .filter(|r| {
                r.rmgr == RmgrId::Btree
                    && r.info == BTREE_PAGES
                    && r.main[0] == PagesReason::Split as u8
            })
            .collect()
    }

    // ----- choose_split（§7.4） -----

    fn brute(
        sizes: &[usize],
        hk: Option<usize>,
        rightmost: bool,
        leaf: bool,
        newpos: usize,
    ) -> Option<usize> {
        let n = sizes.len();
        let mut feasible = Vec::new();
        for k in 1..n {
            let sep = if leaf {
                maxalign(sizes[k]) + 8
            } else {
                sizes[k]
            };
            let left = cost(sep) + sizes[..k].iter().map(|&s| cost(s)).sum::<usize>();
            let mut right = hk.map_or(0, cost) + sizes[k..].iter().map(|&s| cost(s)).sum::<usize>();
            if !leaf {
                right = right - cost(sizes[k]) + 12;
            }
            if left <= BT_PAGE_USABLE && right <= BT_PAGE_USABLE {
                feasible.push((k, left, right));
            }
        }
        if leaf && rightmost && newpos == n - 1 {
            feasible
                .iter()
                .filter(|f| f.1 <= 7336)
                .map(|f| f.0)
                .max()
                .or(feasible.first().map(|f| f.0))
        } else {
            let wide: Vec<_> = feasible
                .iter()
                .filter(|f| !leaf && f.0 >= 2 && n - f.0 >= 2)
                .collect();
            let pool: Vec<_> = if wide.is_empty() {
                feasible.iter().collect()
            } else {
                wide
            };
            pool.iter()
                .min_by_key(|f| (f.1.abs_diff(f.2), f.0))
                .map(|f| f.0)
        }
    }

    #[test]
    fn choose_split_matches_a_brute_force_search() {
        let mut r = Lcg(7);
        let mut chosen = 0;
        for case in 0..3000 {
            let leaf = case % 2 == 0;
            let mut sizes: Vec<usize> = Vec::new();
            let mut used = 0usize;
            let max_item = if r.below(3) == 0 { 2704 } else { 400 };
            // 満杯に近い 1 ページ分 + 新しい項目 1 件。
            loop {
                let s = 8 * (1 + usize::try_from(r.below((max_item / 8) as u64)).unwrap());
                if used + cost(s) > BT_PAGE_USABLE - cost(24) {
                    break;
                }
                used += cost(s);
                sizes.push(s);
            }
            sizes.push(8 * (1 + usize::try_from(r.below((max_item / 8) as u64)).unwrap()));
            if !leaf {
                sizes[0] = 8;
            }
            if sizes.len() < 3 {
                continue;
            }
            let hk = (r.below(2) == 0).then_some(24usize);
            let rightmost = hk.is_none();
            let newpos = usize::try_from(r.below(sizes.len() as u64))
                .unwrap()
                .max(usize::from(!leaf));
            let want = brute(&sizes, hk, rightmost, leaf, newpos);
            let got = choose_split(&sizes, hk, rightmost, leaf, newpos).ok();
            assert_eq!(
                got, want,
                "sizes {sizes:?} hk {hk:?} leaf {leaf} newpos {newpos}"
            );
            if got.is_some() {
                chosen += 1;
            }
        }
        assert!(chosen > 1000);
    }

    #[test]
    fn choose_split_always_feasible_with_maximum_items() {
        // 最大の項目ばかりでも、high key + データ 2 件が入るので実行可能な k がある（§3.8）。
        for leaf in [true, false] {
            for n in 3..=4 {
                let mut sizes = vec![2704; n];
                if !leaf {
                    sizes[0] = 8;
                    for s in &mut sizes[1..] {
                        *s = 2712;
                    }
                }
                for hk in [None, Some(2712)] {
                    for newpos in 0..n {
                        // 1 ページに入らない大きさの組だけを対象にする。
                        let total: usize = sizes.iter().map(|&s| cost(s)).sum();
                        if total <= BT_PAGE_USABLE {
                            continue;
                        }
                        let k = choose_split(&sizes, hk, hk.is_none(), leaf, newpos);
                        assert!(k.is_ok(), "leaf {leaf} n {n} hk {hk:?} newpos {newpos}");
                    }
                }
            }
        }
    }

    // ----- 例 9 と容量（§3.6、§3.8、§7.1） -----

    #[test]
    fn sequential_inserts_split_as_in_example_9() {
        let t = new_index(&INT, false);
        for i in 1..=407 {
            ins_int(&t, i, i as u32);
        }
        assert_eq!(
            t.pool()
                .nblocks(t.locator(), crate::storage::smgr::ForkNumber::Main)
                .unwrap(),
            2
        );
        let before = t.wal().insert_lsn();
        ins_int(&t, 408, 408);
        let recs = t.read_wal_from(before);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.end.0 - r.start.0, 8568, "tot_len = 8568");
        assert_eq!(r.blocks.len(), 4);
        let blocks: Vec<u32> = r.blocks.iter().map(|b| b.tag.block).collect();
        assert_eq!(blocks, vec![1, 2, 3, 0]);
        let info = check_tree(&t);
        assert_eq!(info.height, 2);
        assert_eq!(info.pages_per_level, vec![2, 1]);
        assert_eq!(info.entries.len(), 408);
        let pages = t.snapshot_pages();
        // 左: high key + 365、右: 43。
        assert_eq!(pages[1].max_offset(), 366);
        assert_eq!(pages[2].max_offset(), 43);
        assert_eq!(pages[3].max_offset(), 2);
        t.assert_clean();
    }

    #[test]
    fn sequential_leaves_stay_at_365_and_internal_fanout_is_291() {
        let t = new_index(&INT, false);
        let n = 365 * 292 + 10;
        for i in 1..=n {
            ins_int(&t, i, i as u32);
        }
        let info = check_tree(&t);
        assert_eq!(info.entries.len(), n as usize);
        assert_eq!(info.height, 3, "the right-most internal page split once");
        let pages = t.snapshot_pages();
        // 葉は 365 件ずつ（high key を除く）。最後の葉だけ端数。
        let full = pages
            .iter()
            .filter(|p| {
                let sp = crate::storage::btree::page::BtSpecial::read(p).unwrap();
                sp.is_leaf() && sp.next != 0
            })
            .all(|p| p.max_offset() == 366);
        assert!(full);
        t.assert_clean();
    }

    fn check_all(t: &TestIndex, expect: &mut [(i32, u32)]) {
        let info = check_tree(t);
        expect.sort_unstable();
        let got: Vec<(i32, Tid)> = info
            .entries
            .iter()
            .map(|(k, tid)| match k[0] {
                Datum::Int4(v) => (v, *tid),
                _ => panic!("not int4"),
            })
            .collect();
        let want: Vec<(i32, Tid)> = expect.iter().map(|&(k, n)| (k, tid_of(n))).collect();
        assert_eq!(got.len(), want.len());
        assert!(got == want, "entries differ from the model");
        t.assert_clean();
    }

    #[test]
    fn descending_random_and_duplicate_heavy_inserts() {
        for mode in 0..3 {
            let t = new_index(&INT, false);
            let mut model = Vec::new();
            let mut r = Lcg(99 + mode);
            for n in 0..2500u32 {
                let k = match mode {
                    0 => 100_000 - n as i32,
                    1 => i32::try_from(r.below(1_000_000)).unwrap(),
                    _ => i32::try_from(r.below(7)).unwrap(),
                };
                ins_int(&t, k, n);
                model.push((k, n));
            }
            check_all(&t, &mut model);
        }
    }

    #[test]
    fn splits_in_the_middle_of_a_rightmost_leaf_and_in_non_rightmost_leaves() {
        // 先に飛び飛びの偶数を入れ、後から奇数を詰めると、右端でない葉と右端の葉の途中への挿入が起きる。
        let t = new_index(&INT, false);
        let mut model = Vec::new();
        for n in 0..1500u32 {
            ins_int(&t, (n * 2) as i32, n);
            model.push(((n * 2) as i32, n));
        }
        let mut r = Lcg(5);
        for (i, p) in r.permutation(1500).into_iter().enumerate() {
            let k = (p * 2 + 1) as i32;
            let n = 10_000 + i as u32;
            ins_int(&t, k, n);
            model.push((k, n));
        }
        check_all(&t, &mut model);
        let recs = t.read_wal_from(t.start);
        assert!(split_records(&recs).len() > 5);
    }

    // ----- 高さ 4 以上、shared_buffers の下限（§7.4） -----

    fn text_key(n: usize, len: usize) -> Datum {
        let mut s = format!("{n:06}");
        while s.len() < len {
            s.push('x');
        }
        Datum::Text(s)
    }

    fn wide_tree(ts: TestStorage, n: usize, stop_at_height: Option<u32>) -> TestIndex {
        let t = new_index_with(ts, &[(SqlType::TEXT, false, false)], false);
        let mut r = Lcg(1);
        for (i, p) in r.permutation(n).into_iter().enumerate() {
            put(&t, &[text_key(p, 2692)], tid_of(u32::try_from(i).unwrap())).unwrap();
            if stop_at_height.is_some_and(|h| tree_level(&t) + 1 >= h) {
                break;
            }
        }
        t
    }

    fn assert_wide_tree_is_sound(t: &TestIndex, min_height: u32) {
        let info = check_tree(t);
        assert!(info.height >= min_height, "height {}", info.height);
        let keys: Vec<String> = info
            .entries
            .iter()
            .map(|(k, _)| match &k[0] {
                Datum::Text(s) => s[..6].to_string(),
                _ => panic!(),
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        // 1 レコードのブロック数は 3h + 1 以下。
        let recs = t.read_wal_from(t.start);
        let h = info.height as usize;
        let max_blocks = split_records(&recs)
            .iter()
            .map(|r| r.blocks.len())
            .max()
            .unwrap();
        assert!(max_blocks <= 3 * h + 1, "{max_blocks} > 3*{h}+1");
        t.assert_clean();
    }

    #[test]
    fn wide_keys_reach_height_4_in_a_16_frame_pool() {
        let t = wide_tree(TestStorage::small(16, 1024), 300, Some(4));
        assert_wide_tree_is_sound(&t, 4);
        assert_eq!(check_tree(&t).height, 4);
    }

    #[test]
    fn wide_keys_build_a_tall_tree() {
        // 完全なピボット（切り詰めなし）が 2.7KB なので内部ページの子は 2〜3 個で、木はすぐ高くなる。
        let t = wide_tree(TestStorage::new(), 300, Some(9));
        assert_wide_tree_is_sound(&t, 9);
    }

    #[test]
    fn a_split_chain_needing_more_blocks_than_allowed_fails_without_touching_the_tree() {
        let t = new_index(&INT, false);
        for i in 1..=407 {
            ins_int(&t, i, i as u32);
        }
        let before = t.snapshot_pages();
        MAX_REFS_OVERRIDE.with(|c| c.set(Some(3)));
        let e = put(&t, &[Datum::Int4(408)], tid_of(408)).unwrap_err();
        MAX_REFS_OVERRIDE.with(|c| c.set(None));
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            e.message,
            "index \"t_idx\" cannot be split: too many levels"
        );
        let after = t.snapshot_pages();
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert!(a.0 == b.0, "a page changed");
        }
        check_tree(&t);
        // 上限を戻せば入る。
        ins_int(&t, 408, 408);
        assert_eq!(check_tree(&t).entries.len(), 408);
        t.assert_clean();
    }
}
