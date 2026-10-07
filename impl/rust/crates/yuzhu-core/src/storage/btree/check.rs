//! 木の検査器（`m4/06-btree.md` §4.8、§6.2）。
//!
//! 1 ページずつ共有ラッチして写しを取り、ラッチ・ピンを持ち越さない。違反は最初の 1 件で返す。
//! 全テストの最後と、クラッシュ試験のリカバリの後に走らせる（I13、I14）。
//!
//! `BtCtx` は `Wal` を必要とするが検査器は WAL を使わないので、ここでは `BtCtx` を使わず、
//! 規則の名前（`rule`）つきの違反を自分で作る。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::page::BtSpecial;
use super::tuple::{IndexTuple, decode_key, pivot_with_downlink};
use super::{
    BT_MAGIC, BT_MAX_ITEM_SIZE, BT_MAX_LEVEL, BT_PIVOT_EXTRA, BT_SPECIAL_SIZE, BT_VERSION,
    BTP_KNOWN_MASK, BTP_META, cmp_key_tid, cmp_keys,
};
use crate::error::{Error, Result, sqlstate};
use crate::storage::buffer::BufferPool;
use crate::storage::page::{LpFlags, Page};
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber};
use crate::storage::{BLCKSZ, IndexHandle, RelHandle, SIZE_OF_PAGE_HEADER, TableStore, TupleState};
use crate::txn::Xid;
use crate::types::{Datum, Tid};

/// 検査の統計。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckStats {
    pub levels: u32,
    pub pages: u32,
    pub leaf_pages: u32,
    pub items: u64,
    pub orphan_zero_pages: u32,
}

/// 違反。`rule` は §6.2 の名前、`block` は該当するページ（メタや木全体なら `None`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckViolation {
    pub block: Option<BlockNumber>,
    pub rule: &'static str,
    pub detail: String,
}

#[derive(Debug)]
pub enum CheckError {
    Violation(CheckViolation),
    Io(Error),
}

impl CheckError {
    /// 違反ならその規則名。
    pub fn rule(&self) -> Option<&'static str> {
        match self {
            CheckError::Violation(v) => Some(v.rule),
            CheckError::Io(_) => None,
        }
    }

    /// `XX001` / 元の I/O エラーへ変換する。
    pub fn into_error(self, index: &IndexHandle) -> Error {
        match self {
            CheckError::Io(e) => e,
            CheckError::Violation(v) => Error::new(
                sqlstate::DATA_CORRUPTED,
                format!(
                    "index \"{}\" violates {}{}",
                    index.name,
                    v.rule,
                    v.block
                        .map_or_else(String::new, |b| format!(" at block {b}"))
                ),
            )
            .with_detail(v.detail)
            .with_hint("Please REINDEX it."),
        }
    }
}

impl From<Error> for CheckError {
    fn from(e: Error) -> Self {
        CheckError::Io(e)
    }
}

const ZERO_TID: Tid = Tid {
    block: 0,
    offset: 0,
};

type CheckResult<T> = std::result::Result<T, CheckError>;

fn violation<T>(
    block: Option<BlockNumber>,
    rule: &'static str,
    detail: impl Into<String>,
) -> CheckResult<T> {
    Err(CheckError::Violation(CheckViolation {
        block,
        rule,
        detail: detail.into(),
    }))
}

// ----- ページの解析 ---------------------------------------------------------------

/// 検査済みの項目。
struct Item {
    bytes: Vec<u8>,
    key: Vec<Datum>,
    /// 葉タプルと完全なピボットでは `Some`。
    tid: Option<Tid>,
    is_pivot: bool,
    minus_inf: bool,
    downlink: BlockNumber,
}

/// 親の区切り（下限・上限）。`bytes` は downlink を 0 にしたピボット。
struct Sep {
    key: Vec<Datum>,
    tid: Tid,
    bytes: Vec<u8>,
}

impl Item {
    fn sep(&self) -> Option<Sep> {
        Some(Sep {
            key: self.key.clone(),
            tid: self.tid?,
            bytes: pivot_with_downlink(&self.bytes, 0),
        })
    }
}

/// 検査を通ったページ。
struct PageData {
    block: BlockNumber,
    sp: BtSpecial,
    hikey: Option<Item>,
    /// データ項目（内部ページの先頭は -∞ ピボット）。
    items: Vec<Item>,
}

struct Checker<'a> {
    pool: &'a Arc<BufferPool>,
    index: &'a IndexHandle,
    nblocks: u32,
}

impl Checker<'_> {
    fn ncols(&self) -> usize {
        self.index.columns.len()
    }

    fn read(&self, block: BlockNumber) -> CheckResult<Box<Page>> {
        let tag = BufferTag {
            rel: self.index.locator,
            fork: ForkNumber::Main,
            block,
        };
        let pin = self.pool.read_buffer(tag)?;
        let g = pin.read_tree()?;
        Ok(Box::new(Page(g.0)))
    }

    fn decode_item(&self, block: BlockNumber, off: u16, bytes: &[u8]) -> CheckResult<Item> {
        let ncols = self.ncols();
        let t = match IndexTuple::validate(bytes, ncols) {
            Ok(t) => t,
            Err(why) => return violation(Some(block), "page.item", format!("item {off}: {why}")),
        };
        if t.size() != bytes.len() || !bytes.len().is_multiple_of(8) {
            return violation(
                Some(block),
                "page.item",
                format!("item {off}: size {} != lp_len {}", t.size(), bytes.len()),
            );
        }
        let limit = BT_MAX_ITEM_SIZE + if t.is_pivot() { BT_PIVOT_EXTRA } else { 0 };
        if bytes.len() > limit {
            return violation(
                Some(block),
                "page.item",
                format!("item {off}: size {} exceeds {limit}", bytes.len()),
            );
        }
        let natts = if t.is_pivot() { t.pivot_natts() } else { ncols };
        let key = match decode_key(self.index, &t, natts) {
            Ok(k) => k,
            Err(e) => {
                return violation(
                    Some(block),
                    "page.item",
                    format!("item {off}: cannot decode the key: {}", e.message),
                );
            }
        };
        Ok(Item {
            bytes: bytes.to_vec(),
            key,
            tid: t.heap_tid(),
            is_pivot: t.is_pivot(),
            minus_inf: t.is_minus_infinity(),
            downlink: if t.is_pivot() { t.downlink() } else { 0 },
        })
    }

    /// 1 ページの規則（`page.*`）を検査して、項目を復号した結果を返す。
    #[allow(clippy::too_many_lines)]
    fn parse_page(&self, block: BlockNumber, page: &Page) -> CheckResult<PageData> {
        let ncols = self.ncols();
        if page.is_all_zero() || page.is_new() {
            return violation(Some(block), "page.zero", "the page is all zero");
        }
        if usize::from(page.special()) != BLCKSZ - BT_SPECIAL_SIZE {
            return violation(
                Some(block),
                "page.special",
                format!("pd_special = {}", page.special()),
            );
        }
        let lower = usize::from(page.lower());
        let upper = usize::from(page.upper());
        let special = usize::from(page.special());
        if lower < SIZE_OF_PAGE_HEADER || lower > upper || upper > special {
            return violation(
                Some(block),
                "page.lp",
                format!("pd_lower = {lower}, pd_upper = {upper}"),
            );
        }
        let Some(sp) = BtSpecial::read(page) else {
            return violation(Some(block), "page.special", "special area is not 16 bytes");
        };
        if sp.cycleid != 0 {
            return violation(
                Some(block),
                "page.special",
                format!("cycleid = {}", sp.cycleid),
            );
        }
        if sp.flags & !BTP_KNOWN_MASK != 0 || sp.flags & BTP_META != 0 {
            return violation(
                Some(block),
                "page.flags",
                format!("flags = 0x{:04x}", sp.flags),
            );
        }
        if sp.is_leaf() != (sp.level == 0) || sp.level >= BT_MAX_LEVEL {
            return violation(
                Some(block),
                "page.flags",
                format!("LEAF flag does not match level {}", sp.level),
            );
        }

        // 行ポインタ。
        let max = page.max_offset();
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(usize::from(max));
        for off in 1..=max {
            let Ok(id) = page.item_id(off) else {
                return violation(
                    Some(block),
                    "page.lp",
                    format!("line pointer {off} is out of range"),
                );
            };
            if id.flags != LpFlags::Normal {
                return violation(
                    Some(block),
                    "page.lp",
                    format!("line pointer {off} is not LP_NORMAL"),
                );
            }
            ranges.push((
                usize::from(id.off),
                usize::from(id.off) + usize::from(id.len),
            ));
        }
        ranges.sort_unstable();
        if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
            return violation(Some(block), "page.lp", "item bodies overlap");
        }

        // 項目。
        let mut decoded: Vec<Item> = Vec::with_capacity(usize::from(max));
        for off in 1..=max {
            let Ok(bytes) = page.item(off) else {
                return violation(
                    Some(block),
                    "page.lp",
                    format!("line pointer {off} is out of range"),
                );
            };
            decoded.push(self.decode_item(block, off, bytes)?);
        }

        let first = usize::from(sp.first_data_offset()) - 1;
        let hikey = if sp.next != 0 {
            if decoded.is_empty() {
                return violation(Some(block), "page.hikey", "next != 0 but no high key");
            }
            Some(decoded.remove(0))
        } else {
            None
        };
        debug_assert!(first <= 1);
        if let Some(hk) = &hikey
            && (!hk.is_pivot
                || hk.minus_inf
                || hk.key.len() != ncols
                || hk.tid.is_none()
                || hk.downlink != 0)
        {
            return violation(
                Some(block),
                "page.hikey",
                "the high key is not a full pivot with downlink 0",
            );
        }

        // 種類。
        let items = decoded;
        if sp.is_leaf() {
            if let Some(i) = items.iter().position(|it| it.is_pivot) {
                return violation(
                    Some(block),
                    "page.kind",
                    format!("a pivot at data item {}", i + 1),
                );
            }
        } else {
            if items.is_empty() {
                return violation(
                    Some(block),
                    "page.kind",
                    "an internal page has no downlinks",
                );
            }
            if let Some(i) = items.iter().position(|it| !it.is_pivot) {
                return violation(
                    Some(block),
                    "page.kind",
                    format!("a leaf tuple at data item {}", i + 1),
                );
            }
            if !items[0].minus_inf {
                return violation(
                    Some(block),
                    "page.first_pivot",
                    "the first data item is not a minus infinity pivot",
                );
            }
            if let Some(i) = items.iter().skip(1).position(|it| it.minus_inf) {
                return violation(
                    Some(block),
                    "page.first_pivot",
                    format!("a minus infinity pivot at data item {}", i + 2),
                );
            }
            for (i, it) in items.iter().enumerate().skip(1) {
                if it.key.len() != ncols || it.tid.is_none() {
                    return violation(
                        Some(block),
                        "page.kind",
                        format!("data item {} is not a full pivot", i + 1),
                    );
                }
            }
        }

        // 順序と high key。
        let real: Vec<&Item> = items.iter().filter(|it| !it.minus_inf).collect();
        for w in real.windows(2) {
            if cmp_item(self.index, w[0], w[1]) != Ordering::Less {
                return violation(
                    Some(block),
                    "page.order",
                    "data items are not in strictly ascending order",
                );
            }
        }
        if let Some(hk) = &hikey
            && let Some(last) = real.last()
            && cmp_item(self.index, last, hk) != Ordering::Less
        {
            return violation(
                Some(block),
                "page.hikey",
                "a data item is not smaller than the high key",
            );
        }
        Ok(PageData {
            block,
            sp,
            hikey,
            items,
        })
    }
}

/// 項目どうしの `(キー, TID)` の比較（どちらも TID を持つこと）。
fn cmp_item(index: &IndexHandle, a: &Item, b: &Item) -> Ordering {
    let zero = Tid {
        block: 0,
        offset: 0,
    };
    cmp_key_tid(
        index,
        &a.key,
        a.tid.unwrap_or(zero),
        &b.key,
        b.tid.unwrap_or(zero),
    )
}

fn cmp_item_sep(index: &IndexHandle, a: &Item, s: &Sep) -> Ordering {
    let zero = Tid {
        block: 0,
        offset: 0,
    };
    cmp_key_tid(index, &a.key, a.tid.unwrap_or(zero), &s.key, s.tid)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// メタページの検査（`meta.*`）。`(root, level)` を返す。
fn check_meta(ck: &Checker<'_>) -> CheckResult<(BlockNumber, u32)> {
    if ck.nblocks < 2 {
        return violation(
            None,
            "meta.root",
            format!("the index has {} blocks", ck.nblocks),
        );
    }
    let page = ck.read(0)?;
    if page.is_all_zero() || page.is_new() {
        return violation(Some(0), "meta.page", "the metapage is all zero");
    }
    let Some(sp) = BtSpecial::read(&page) else {
        return violation(Some(0), "meta.page", "special area is not 16 bytes");
    };
    if sp.flags != BTP_META || sp.prev != 0 || sp.next != 0 || sp.level != 0 || sp.cycleid != 0 {
        return violation(Some(0), "meta.page", "unexpected metapage special area");
    }
    let body = page.body();
    if body.len() < 24 {
        return violation(Some(0), "meta.page", "the metapage body is too short");
    }
    if u32_at(body, 0) != BT_MAGIC {
        return violation(
            Some(0),
            "meta.magic",
            format!("magic = 0x{:08x}", u32_at(body, 0)),
        );
    }
    if u32_at(body, 4) != BT_VERSION {
        return violation(
            Some(0),
            "meta.version",
            format!("version = {}", u32_at(body, 4)),
        );
    }
    let (root, level, fastroot, fastlevel) = (
        u32_at(body, 8),
        u32_at(body, 12),
        u32_at(body, 16),
        u32_at(body, 20),
    );
    if root < 1 || root >= ck.nblocks {
        return violation(Some(0), "meta.root", format!("root = {root}"));
    }
    if fastroot != root || fastlevel != level {
        return violation(
            Some(0),
            "meta.root",
            format!("fastroot = {fastroot}, fastlevel = {fastlevel}"),
        );
    }
    if level >= BT_MAX_LEVEL {
        return violation(Some(0), "meta.root", format!("level = {level}"));
    }
    Ok((root, level))
}

struct Bound {
    lower: Option<Sep>,
    upper: Option<Sep>,
}

/// 構造の検査（I14）。ピンもラッチも持ち越さない（1 ページずつ共有ラッチして写しを取る）。
/// ページを別の時刻に読むので、書き手が止まっているときにだけ使う（並行すると偽の違反が出る）。
pub fn check_structure(
    pool: &Arc<BufferPool>,
    index: &IndexHandle,
) -> std::result::Result<CheckStats, CheckError> {
    let ck = Checker {
        pool,
        index,
        nblocks: pool.nblocks(index.locator, ForkNumber::Main)?,
    };
    let (root, root_level) = check_meta(&ck)?;

    let mut seen: HashSet<BlockNumber> = HashSet::new();
    let mut stats = CheckStats {
        levels: root_level + 1,
        ..CheckStats::default()
    };
    let mut start = root;
    let mut bounds: HashMap<BlockNumber, Bound> = HashMap::new();
    bounds.insert(
        root,
        Bound {
            lower: None,
            upper: None,
        },
    );
    let mut expected: Option<Vec<BlockNumber>> = None;

    for level in (0..=root_level).rev() {
        let pages = walk_chain(&ck, start, level, root, &mut seen, level == root_level)?;
        if level == root_level && pages.len() != 1 {
            return violation(Some(root), "meta.root", "the root page has siblings");
        }
        if let Some(exp) = expected.take() {
            compare_children(&exp, &pages)?;
        }
        let mut next_bounds: HashMap<BlockNumber, Bound> = HashMap::new();
        let mut children: Vec<BlockNumber> = Vec::new();
        for pd in &pages {
            let b = bounds.remove(&pd.block).unwrap_or(Bound {
                lower: None,
                upper: None,
            });
            check_bounds(index, pd, &b)?;
            if level == 0 {
                stats.leaf_pages += 1;
                stats.items += pd.items.len() as u64;
                continue;
            }
            let n = pd.items.len();
            for (i, it) in pd.items.iter().enumerate() {
                if it.downlink == 0 || it.downlink >= ck.nblocks {
                    return violation(
                        Some(pd.block),
                        "tree.children",
                        format!("downlink {} is out of range", it.downlink),
                    );
                }
                let lower = if i == 0 {
                    b.lower.as_ref().map(clone_sep)
                } else {
                    it.sep()
                };
                let upper = if i + 1 < n {
                    pd.items[i + 1].sep()
                } else {
                    b.upper.as_ref().map(clone_sep)
                };
                children.push(it.downlink);
                next_bounds.insert(it.downlink, Bound { lower, upper });
            }
        }
        if level > 0 {
            let Some(&first) = children.first() else {
                return violation(None, "tree.children", "no downlinks at the level");
            };
            start = first;
            bounds = next_bounds;
            expected = Some(children);
        }
    }
    stats.pages = u32::try_from(seen.len()).unwrap_or(u32::MAX);

    for block in 1..ck.nblocks {
        if seen.contains(&block) {
            continue;
        }
        let page = ck.read(block)?;
        if page.is_all_zero() {
            stats.orphan_zero_pages += 1;
        } else {
            return violation(
                Some(block),
                "tree.orphan",
                "a page with contents is not reachable from the root",
            );
        }
    }
    Ok(stats)
}

fn clone_sep(s: &Sep) -> Sep {
    Sep {
        key: s.key.clone(),
        tid: s.tid,
        bytes: s.bytes.clone(),
    }
}

/// 1 つのレベルのチェーン（最左から `next` を辿る）を読み、`page.*`・`link.*`・`tree.root_flag` を検査する。
fn walk_chain(
    ck: &Checker<'_>,
    start: BlockNumber,
    level: u32,
    root: BlockNumber,
    seen: &mut HashSet<BlockNumber>,
    is_root_level: bool,
) -> CheckResult<Vec<PageData>> {
    let mut pages: Vec<PageData> = Vec::new();
    let mut chain: HashSet<BlockNumber> = HashSet::new();
    let mut blk = start;
    let mut prev = 0;
    loop {
        if blk == 0 || blk >= ck.nblocks {
            return violation(
                Some(blk),
                if pages.is_empty() {
                    "tree.children"
                } else {
                    "link.pair"
                },
                format!("block {blk} is out of range"),
            );
        }
        if !chain.insert(blk) {
            return violation(Some(blk), "link.cycle", "the sibling chain is cyclic");
        }
        let page = ck.read(blk)?;
        let pd = ck.parse_page(blk, &page)?;
        if pd.sp.level != level {
            let rule = if pages.is_empty() && !is_root_level {
                "tree.downlink_level"
            } else {
                "link.level"
            };
            return violation(
                Some(blk),
                rule,
                format!("level {} where {level} is expected", pd.sp.level),
            );
        }
        if pd.sp.prev != prev {
            return violation(
                Some(blk),
                "link.pair",
                format!("prev = {} where {prev} is expected", pd.sp.prev),
            );
        }
        if pd.sp.is_root() != (blk == root) {
            return violation(Some(blk), "tree.root_flag", "the ROOT flag is wrong");
        }
        seen.insert(blk);
        let next = pd.sp.next;
        pages.push(pd);
        if next == 0 {
            break;
        }
        if chain.contains(&next) {
            return violation(Some(blk), "link.cycle", "the sibling chain is cyclic");
        }
        prev = blk;
        blk = next;
    }
    Ok(pages)
}

/// 親の子の並びと、1 つ下のレベルのチェーンの突き合わせ。
fn compare_children(expected: &[BlockNumber], pages: &[PageData]) -> CheckResult<()> {
    let actual: Vec<BlockNumber> = pages.iter().map(|p| p.block).collect();
    if expected == actual.as_slice() {
        return Ok(());
    }
    let exp_set: HashSet<BlockNumber> = expected.iter().copied().collect();
    let act_set: HashSet<BlockNumber> = actual.iter().copied().collect();
    if let Some(b) = actual.iter().find(|b| !exp_set.contains(b)) {
        return violation(
            Some(*b),
            "tree.downlink_missing",
            "a page in the sibling chain has no downlink in its parent level",
        );
    }
    if let Some(b) = expected.iter().find(|b| !act_set.contains(b)) {
        return violation(
            Some(*b),
            "tree.downlink_extra",
            "a downlink points outside the sibling chain",
        );
    }
    violation(
        None,
        "tree.children",
        "the downlinks do not match the sibling chain",
    )
}

/// 子のページが親の区切りの範囲に入ること（`tree.bounds`）と、high key が区切りと一致すること
/// （`tree.child_hikey`）。
fn check_bounds(index: &IndexHandle, pd: &PageData, b: &Bound) -> CheckResult<()> {
    for it in pd.items.iter().filter(|it| !it.minus_inf) {
        if let Some(lo) = &b.lower
            && cmp_item_sep(index, it, lo) == Ordering::Less
        {
            return violation(
                Some(pd.block),
                "tree.bounds",
                "an item is smaller than the lower bound given by the parent",
            );
        }
        if let Some(up) = &b.upper
            && cmp_item_sep(index, it, up) != Ordering::Less
        {
            return violation(
                Some(pd.block),
                "tree.bounds",
                "an item is not smaller than the upper bound given by the parent",
            );
        }
    }
    match (&pd.hikey, &b.upper) {
        (None, None) => Ok(()),
        (Some(hk), Some(up)) => {
            if pivot_with_downlink(&hk.bytes, 0) == up.bytes {
                Ok(())
            } else {
                violation(
                    Some(pd.block),
                    "tree.child_hikey",
                    "the high key differs from the separator in the parent",
                )
            }
        }
        _ => violation(
            Some(pd.block),
            "tree.child_hikey",
            "the high key and the parent separator do not agree on the right edge",
        ),
    }
}

/// 全葉項目 `(キー, TID)` を木の順序で返す（テスト用）。壊れた木は `XX001`。
pub fn dump_entries(pool: &Arc<BufferPool>, index: &IndexHandle) -> Result<Vec<(Vec<Datum>, Tid)>> {
    dump_inner(pool, index).map_err(|e| e.into_error(index))
}

fn dump_inner(pool: &Arc<BufferPool>, index: &IndexHandle) -> CheckResult<Vec<(Vec<Datum>, Tid)>> {
    let ck = Checker {
        pool,
        index,
        nblocks: pool.nblocks(index.locator, ForkNumber::Main)?,
    };
    let (root, root_level) = check_meta(&ck)?;
    let mut blk = root;
    for level in (1..=root_level).rev() {
        let pd = ck.parse_page(blk, &*ck.read(blk)?)?;
        if pd.sp.level != level {
            return violation(Some(blk), "tree.downlink_level", "unexpected level");
        }
        let Some(first) = pd.items.first() else {
            return violation(Some(blk), "page.kind", "an internal page has no downlinks");
        };
        blk = first.downlink;
        if blk == 0 || blk >= ck.nblocks {
            return violation(Some(pd.block), "tree.children", "downlink is out of range");
        }
    }
    let mut out = Vec::new();
    let mut hops = 0u32;
    loop {
        let pd = ck.parse_page(blk, &*ck.read(blk)?)?;
        if pd.sp.level != 0 {
            return violation(Some(blk), "link.level", "unexpected level");
        }
        for it in &pd.items {
            out.push((it.key.clone(), it.tid.unwrap_or(ZERO_TID)));
        }
        if pd.sp.next == 0 {
            return Ok(out);
        }
        blk = pd.sp.next;
        hops += 1;
        if blk >= ck.nblocks || hops > ck.nblocks {
            return violation(Some(pd.block), "link.cycle", "the sibling chain is cyclic");
        }
    }
}

/// ヒープとの突き合わせの指定。
#[derive(Debug)]
pub struct HeapCheck<'a> {
    pub heap: &'a dyn TableStore,
    pub rel: &'a RelHandle,
    pub own: Option<Xid>,
    pub check_unique_live: bool,
}

/// 構造の検査 + ヒープとの突き合わせ（I13）+（`check_unique_live` なら）生きている版のキーの重複なし。
pub fn check_against_heap(
    pool: &Arc<BufferPool>,
    index: &IndexHandle,
    hc: &HeapCheck<'_>,
) -> std::result::Result<CheckStats, CheckError> {
    let stats = check_structure(pool, index)?;
    let mut entries: HashMap<Tid, Vec<Datum>> = HashMap::new();
    for (key, tid) in dump_inner(pool, index)? {
        if entries.insert(tid, key).is_some() {
            return violation(
                None,
                "heap.duplicate_tid",
                format!(
                    "TID ({},{}) appears twice in the index",
                    tid.block, tid.offset
                ),
            );
        }
    }
    let mut live_keys: Vec<Vec<Datum>> = Vec::new();
    let mut scan = hc.heap.begin_scan_all(hc.rel)?;
    while let Some(t) = hc.heap.scan_next(&mut scan)? {
        let key: Vec<Datum> = index
            .columns
            .iter()
            .map(|c| {
                usize::try_from(c.attnum - 1)
                    .ok()
                    .and_then(|i| t.row.get(i))
                    .cloned()
                    .unwrap_or(Datum::Null)
            })
            .collect();
        let tup_state = hc.heap.tuple_state(&t, hc.own)?;
        match entries.remove(&t.tid) {
            Some(k) => {
                if cmp_keys(index, &k, &key) != Ordering::Equal {
                    return violation(
                        None,
                        "heap.key_mismatch",
                        format!(
                            "the index key of TID ({},{}) differs from the heap",
                            t.tid.block, t.tid.offset
                        ),
                    );
                }
            }
            None => {
                if tup_state != TupleState::InsertAborted {
                    return violation(
                        None,
                        "heap.missing_entry",
                        format!(
                            "the heap tuple at TID ({},{}) has no index entry",
                            t.tid.block, t.tid.offset
                        ),
                    );
                }
            }
        }
        if hc.check_unique_live
            && index.unique
            && tup_state == TupleState::Live
            && !key.iter().any(Datum::is_null)
        {
            live_keys.push(key);
        }
    }
    if let Some(tid) = entries.keys().min_by_key(|t| (t.block, t.offset)) {
        return violation(
            None,
            "heap.dangling_entry",
            format!(
                "the index entry for TID ({},{}) has no heap tuple",
                tid.block, tid.offset
            ),
        );
    }
    if hc.check_unique_live && index.unique {
        live_keys.sort_by(|a, b| cmp_keys(index, a, b));
        if live_keys
            .windows(2)
            .any(|w| cmp_keys(index, &w[0], &w[1]) == Ordering::Equal)
        {
            return violation(
                None,
                "unique.live_duplicate",
                "two live heap versions have the same key",
            );
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::storage::btree::meta::BtMeta;
    use crate::storage::btree::testing::{
        TestIndex, new_index, put, test_index, tid_of, tree_level,
    };
    use crate::storage::btree::tuple::{form_index_tuple, leaf_to_pivot};
    use crate::storage::buffer::PageWriteGuard;
    use crate::storage::testing::test_rel;
    use crate::storage::{
        HeapScan, HeapTuple, TmResult, TupleDesc, UpdateOutcome, WriteCtx, smgr::RelFileLocator,
    };
    use crate::txn::{Snapshot, Xid};
    use crate::types::SqlType;

    fn int_index() -> TestIndex {
        new_index(&[(SqlType::INT4, false, false)], false)
    }

    /// 3 段の木（一括構築で作る。葉が 200 枚を超える）。
    fn big_tree() -> TestIndex {
        let t = int_index();
        let mut it =
            (1..=100_000u32).map(|n| (vec![Datum::Int4(i32::try_from(n).unwrap())], tid_of(n)));
        crate::storage::btree::build::build_index(
            &t.ctx(),
            &t.write_ctx(),
            &mut it,
            crate::storage::BuildUnique::No,
        )
        .unwrap();
        assert_eq!(tree_level(&t), 2);
        t
    }

    fn small_tree() -> TestIndex {
        let t = int_index();
        for n in 1..=1200u32 {
            put(&t, &[Datum::Int4(i32::try_from(n).unwrap())], tid_of(n)).unwrap();
        }
        assert_eq!(tree_level(&t), 1);
        t
    }

    fn rule_of(t: &TestIndex) -> &'static str {
        match check_structure(t.pool(), &t.handle) {
            Err(CheckError::Violation(v)) => v.rule,
            other => panic!("expected a violation, got {other:?}"),
        }
    }

    /// ブロック `blk` のページを `f` で書き換える。
    fn damage(t: &TestIndex, blk: u32, f: impl FnOnce(&mut Page)) {
        let pin = t.pool().read_buffer(t.ctx().tag(blk)).unwrap();
        let mut g: PageWriteGuard<'_> = pin.write_tree().unwrap();
        f(g.page_mut_hint());
    }

    fn page_of(t: &TestIndex, blk: u32) -> Box<Page> {
        let pin = t.pool().read_buffer(t.ctx().tag(blk)).unwrap();
        let g = pin.read_tree().unwrap();
        Box::new(Page(g.0))
    }

    /// ページを項目の並びから作り直す（special は保つ）。
    fn rebuild(t: &TestIndex, blk: u32, f: impl FnOnce(&mut Vec<Vec<u8>>, &mut BtSpecial)) {
        let old = page_of(t, blk);
        let mut sp = BtSpecial::read(&old).unwrap();
        let mut items: Vec<Vec<u8>> = (1..=old.max_offset())
            .map(|o| old.item(o).unwrap().to_vec())
            .collect();
        f(&mut items, &mut sp);
        let refs: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
        let page = Page::build_with_items(&sp.to_bytes(), &refs).unwrap();
        damage(t, blk, |p| *p = *page);
    }

    /// 内部ページのブロック（レベル `level`）と、その子。
    fn children_of(t: &TestIndex, blk: u32) -> Vec<u32> {
        let p = page_of(t, blk);
        let sp = BtSpecial::read(&p).unwrap();
        (sp.first_data_offset()..=p.max_offset())
            .map(|o| IndexTuple(p.item(o).unwrap()).downlink())
            .collect()
    }

    fn root_block(t: &TestIndex) -> u32 {
        let ctx = t.ctx();
        let pin = t.pool().read_buffer(ctx.tag(0)).unwrap();
        let g = pin.read_tree().unwrap();
        BtMeta::read(&ctx, &g).unwrap().root
    }

    fn leaf_tuple(key: i32, n: u32) -> Vec<u8> {
        let t = int_index();
        form_index_tuple(&t.handle, &[Datum::Int4(key)], tid_of(n)).unwrap()
    }

    #[test]
    fn healthy_trees_pass() {
        let t = int_index();
        let s = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(s.levels, 1);
        assert_eq!((s.pages, s.leaf_pages, s.items), (1, 1, 0));
        let t = small_tree();
        let s = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(s.levels, 2);
        assert_eq!(s.items, 1200);
        assert_eq!(s.pages, s.leaf_pages + 1);
        let t = big_tree();
        let s = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(s.levels, 3);
        let e = dump_entries(t.pool(), &t.handle).unwrap();
        assert_eq!(e.len() as u64, s.items);
        assert!(
            e.windows(2)
                .all(|w| cmp_keys(&t.handle, &w[0].0, &w[1].0) != Ordering::Greater)
        );
        t.assert_clean();
    }

    #[test]
    fn orphan_zero_pages_are_counted() {
        let t = small_tree();
        drop(t.pool().extend(t.handle.locator, ForkNumber::Main).unwrap());
        let s = check_structure(t.pool(), &t.handle).unwrap();
        assert_eq!(s.orphan_zero_pages, 1);
        t.assert_clean();
    }

    #[test]
    fn meta_violations() {
        let t = small_tree();
        damage(&t, 0, |p| p.0[24] ^= 0xFF);
        assert_eq!(rule_of(&t), "meta.magic");
        let t = small_tree();
        damage(&t, 0, |p| p.0[28] = 9);
        assert_eq!(rule_of(&t), "meta.version");
        let t = small_tree();
        damage(&t, 0, |p| p.0[32..36].copy_from_slice(&0u32.to_le_bytes()));
        assert_eq!(rule_of(&t), "meta.root");
        let t = small_tree();
        damage(&t, 0, |p| p.0[40..44].copy_from_slice(&2u32.to_le_bytes()));
        assert_eq!(rule_of(&t), "meta.root");
        let t = small_tree();
        damage(&t, 0, |p| p.0[8188] = 0x0A);
        assert_eq!(rule_of(&t), "meta.page");
        let t = small_tree();
        damage(&t, 0, |p| *p = Page::zeroed());
        assert_eq!(rule_of(&t), "meta.page");
    }

    #[test]
    fn meta_root_must_match_the_root_page_level() {
        let t = small_tree();
        let root = root_block(&t);
        // メタの level を 0 にすると、ルートのページの level（1）と合わない。
        damage(&t, 0, |p| {
            BtMeta { root, level: 0 }.write(p);
        });
        assert_eq!(rule_of(&t), "link.level");
    }

    #[test]
    fn internal_items_swapped() {
        let t = small_tree();
        let root = root_block(&t);
        damage(&t, root, |p| {
            // 行ポインタ 2 と 3 を入れ替える。
            let (a, b) = (24 + 4, 24 + 8);
            for i in 0..4 {
                p.0.swap(a + i, b + i);
            }
        });
        assert_eq!(rule_of(&t), "page.order");
    }

    #[test]
    fn leaf_item_above_the_high_key() {
        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        rebuild(&t, leaf, |items, _| {
            let n = items.len();
            items[n - 1] = leaf_tuple(1_000_000, 999_999);
        });
        assert_eq!(rule_of(&t), "page.hikey");
    }

    #[test]
    fn sibling_links() {
        let t = small_tree();
        let kids = children_of(&t, root_block(&t));
        assert!(kids.len() >= 4);
        damage(&t, kids[1], |p| {
            let mut sp = BtSpecial::read(p).unwrap();
            sp.next = kids[3];
            sp.write(p);
        });
        assert_eq!(rule_of(&t), "link.pair");

        let t = small_tree();
        let kids = children_of(&t, root_block(&t));
        damage(&t, kids[2], |p| {
            let mut sp = BtSpecial::read(p).unwrap();
            sp.next = kids[1];
            sp.write(p);
        });
        assert_eq!(rule_of(&t), "link.cycle");

        // 最左の prev が 0 でない。
        let t = small_tree();
        let kids = children_of(&t, root_block(&t));
        damage(&t, kids[0], |p| {
            let mut sp = BtSpecial::read(p).unwrap();
            sp.prev = kids[1];
            sp.write(p);
        });
        assert_eq!(rule_of(&t), "link.pair");
    }

    #[test]
    fn missing_and_extra_downlinks() {
        let t = small_tree();
        let root = root_block(&t);
        rebuild(&t, root, |items, _| {
            items.remove(2);
        });
        assert_eq!(rule_of(&t), "tree.downlink_missing");

        let t = small_tree();
        let root = root_block(&t);
        let kids = children_of(&t, root);
        let extra = t
            .pool()
            .nblocks(t.handle.locator, ForkNumber::Main)
            .unwrap();
        drop(t.pool().extend(t.handle.locator, ForkNumber::Main).unwrap());
        // 末尾に、チェーンにないブロックを指すダウンリンクを足す。
        rebuild(&t, root, |items, _| {
            items.push(leaf_to_pivot(&leaf_tuple(2_000_000, 999_999), extra, 1));
        });
        assert!(kids.len() > 3);
        assert_eq!(rule_of(&t), "tree.downlink_extra");
    }

    #[test]
    fn child_high_key_and_bounds() {
        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        rebuild(&t, leaf, |items, _| {
            // high key のキー値の最下位バイトを +1（大きくなるだけで、データ項目より大きいまま）。
            items[0][8] = items[0][8].wrapping_add(1);
        });
        assert_eq!(rule_of(&t), "tree.child_hikey");

        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[2];
        rebuild(&t, leaf, |items, sp| {
            // 先頭のデータ項目を、親の下限より小さいキーにする。
            let i = usize::from(sp.first_data_offset()) - 1;
            items[i] = leaf_tuple(0, 999_999);
        });
        assert_eq!(rule_of(&t), "tree.bounds");
    }

    #[test]
    fn page_level_violations() {
        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| {
            let mut sp = BtSpecial::read(p).unwrap();
            sp.flags |= 0x0004;
            sp.write(p);
        });
        assert_eq!(rule_of(&t), "page.flags");

        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| {
            let mut sp = BtSpecial::read(p).unwrap();
            sp.flags |= 0x0002;
            sp.write(p);
        });
        assert_eq!(rule_of(&t), "tree.root_flag");

        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| *p = Page::zeroed());
        assert_eq!(rule_of(&t), "page.zero");

        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| {
            // 行ポインタ 3 の lp_flags を LP_DEAD(3) にする。
            let pos = 24 + 2 * 4;
            let id = u32::from_le_bytes([p.0[pos], p.0[pos + 1], p.0[pos + 2], p.0[pos + 3]]);
            let id = (id & !(3 << 15)) | (3 << 15);
            p.0[pos..pos + 4].copy_from_slice(&id.to_le_bytes());
        });
        assert_eq!(rule_of(&t), "page.lp");

        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| p.0[8176 + 14] = 1);
        assert_eq!(rule_of(&t), "page.special");

        // 葉に 1 つだけ pivot が混ざる。
        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        rebuild(&t, leaf, |items, _| {
            items[2] = leaf_to_pivot(&items[2], 0, 1);
        });
        assert_eq!(rule_of(&t), "page.kind");

        // 内部ページの 2 番目に -∞ ピボットが来る。
        let t = small_tree();
        let root = root_block(&t);
        rebuild(&t, root, |items, _| {
            items[1] = crate::storage::btree::tuple::minus_infinity_pivot(5).to_vec();
        });
        assert_eq!(rule_of(&t), "page.first_pivot");

        // タプルの大きさが lp_len と合わない。
        let t = small_tree();
        let leaf = children_of(&t, root_block(&t))[1];
        damage(&t, leaf, |p| {
            let pos = usize::from(p.item_id(3).unwrap().off);
            p.0[pos + 6] = 0x18;
        });
        assert_eq!(rule_of(&t), "page.item");
    }

    #[test]
    fn level_mismatches() {
        let t = big_tree();
        let root = root_block(&t);
        let mids = children_of(&t, root);
        let leaves = children_of(&t, mids[0]);
        // 中間ページの子に、レベルの違うページ（別の中間ページ）を指させる。
        rebuild(&t, mids[0], |items, sp| {
            let i = usize::from(sp.first_data_offset()) - 1;
            items[i] = crate::storage::btree::tuple::minus_infinity_pivot(mids[1]).to_vec();
        });
        let r = rule_of(&t);
        assert!(
            r == "tree.downlink_level" || r == "link.level" || r == "tree.children",
            "{r}"
        );
        assert!(!leaves.is_empty());
    }

    #[test]
    fn orphan_pages() {
        let t = small_tree();
        let pin = t.pool().extend(t.handle.locator, ForkNumber::Main).unwrap();
        {
            let mut g = pin.write_tree().unwrap();
            *g.page_mut_hint() = *crate::storage::btree::meta::empty_root_leaf();
        }
        drop(pin);
        assert_eq!(rule_of(&t), "tree.orphan");
        t.assert_clean();
    }

    #[test]
    fn too_small_index_is_reported() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        assert_eq!(rule_of(&t), "meta.root");
    }

    // ----- ヒープとの突き合わせ -------------------------------------------------

    #[derive(Debug, Default)]
    struct ScanHeap {
        tuples: Mutex<VecDeque<(HeapTuple, TupleState)>>,
        states: Mutex<std::collections::HashMap<Tid, TupleState>>,
    }

    impl ScanHeap {
        fn add(&self, tid: Tid, key: i32, state: TupleState) {
            let t = HeapTuple {
                tid,
                xmin: Xid::FIRST_NORMAL,
                xmax: Xid::INVALID,
                cmin: 0,
                cmax: 0,
                row: vec![Datum::Int4(key)],
            };
            self.states.lock().unwrap().insert(tid, state);
            self.tuples.lock().unwrap().push_back((t, state));
        }
    }

    fn unsupported<T>() -> Result<T> {
        Err(Error::not_supported("ScanHeap"))
    }

    impl TableStore for ScanHeap {
        fn create_storage(&self, _w: &WriteCtx, _rel: RelFileLocator) -> Result<()> {
            unsupported()
        }
        fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
            unsupported()
        }
        fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
            unsupported()
        }
        fn insert(&self, _r: &RelHandle, _w: &WriteCtx, _row: &[Datum]) -> Result<Tid> {
            unsupported()
        }
        fn delete(
            &self,
            _r: &RelHandle,
            _w: &WriteCtx,
            _s: &Snapshot,
            _t: Tid,
        ) -> Result<TmResult> {
            unsupported()
        }
        fn update(
            &self,
            _r: &RelHandle,
            _w: &WriteCtx,
            _s: &Snapshot,
            _t: Tid,
            _n: &[Datum],
        ) -> Result<UpdateOutcome> {
            unsupported()
        }
        fn begin_scan(&self, _r: &RelHandle, _s: &Snapshot) -> Result<HeapScan> {
            unsupported()
        }
        fn scan_next(&self, _scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
            Ok(self.tuples.lock().unwrap().pop_front().map(|x| x.0))
        }
        fn fetch(&self, _r: &RelHandle, _s: &Snapshot, _t: Tid) -> Result<Option<HeapTuple>> {
            unsupported()
        }
        fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan> {
            Ok(HeapScan::new(
                rel.clone(),
                Snapshot {
                    xmin: Xid::INVALID,
                    xmax: Xid::INVALID,
                    xip: vec![],
                    curcid: 0,
                    own_xid: None,
                },
                0,
            ))
        }
        fn tuple_state(&self, t: &HeapTuple, _own: Option<Xid>) -> Result<TupleState> {
            Ok(self.states.lock().unwrap()[&t.tid])
        }
    }

    fn rel() -> RelHandle {
        RelHandle {
            oid: 1,
            locator: test_rel(1),
            desc: Arc::new(TupleDesc { attrs: vec![] }),
            indexes: Arc::from(Vec::new()),
        }
    }

    fn heap_fixture(unique: bool) -> (TestIndex, ScanHeap) {
        let t = new_index(&[(SqlType::INT4, false, false)], unique);
        let h = ScanHeap::default();
        for n in 1..=50u32 {
            let key = i32::try_from(n).unwrap();
            put(&t, &[Datum::Int4(key)], tid_of(n)).unwrap();
            h.add(tid_of(n), key, TupleState::Live);
        }
        (t, h)
    }

    fn heap_rule(t: &TestIndex, h: &ScanHeap, unique_live: bool) -> Option<&'static str> {
        let r = rel();
        let hc = HeapCheck {
            heap: h,
            rel: &r,
            own: None,
            check_unique_live: unique_live,
        };
        match check_against_heap(t.pool(), &t.handle, &hc) {
            Ok(_) => None,
            Err(CheckError::Violation(v)) => Some(v.rule),
            Err(e) => panic!("{e:?}"),
        }
    }

    #[test]
    fn heap_check_passes_and_detects() {
        let (t, h) = heap_fixture(false);
        assert_eq!(heap_rule(&t, &h, false), None);

        // ヒープのタプルが消えた。
        let (t, h) = heap_fixture(false);
        h.tuples.lock().unwrap().remove(10);
        h.states.lock().unwrap().remove(&tid_of(11));
        assert_eq!(heap_rule(&t, &h, false), Some("heap.dangling_entry"));

        // キーが合わない。
        let (t, h) = heap_fixture(false);
        h.tuples.lock().unwrap()[4].0.row = vec![Datum::Int4(777)];
        assert_eq!(heap_rule(&t, &h, false), Some("heap.key_mismatch"));

        // 索引にない版（中断していない）。
        let (t, h) = heap_fixture(false);
        h.add(tid_of(900), 900, TupleState::Live);
        assert_eq!(heap_rule(&t, &h, false), Some("heap.missing_entry"));

        // 中断した版は索引になくてよい。
        let (t, h) = heap_fixture(false);
        h.add(tid_of(900), 900, TupleState::InsertAborted);
        assert_eq!(heap_rule(&t, &h, false), None);
    }

    #[test]
    fn heap_check_unique_live() {
        let extra = |dead_only: bool| {
            let (t, h) = heap_fixture(true);
            // 同じキーを持つ死んだ版（索引にもある）は重複ではない。
            put(&t, &[Datum::Int4(5)], tid_of(500)).unwrap();
            h.add(tid_of(500), 5, TupleState::DeadCommitted);
            if !dead_only {
                // 生きている版が 2 つ。
                put(&t, &[Datum::Int4(6)], tid_of(501)).unwrap();
                h.add(tid_of(501), 6, TupleState::Live);
            }
            (t, h)
        };
        let (t, h) = extra(true);
        assert_eq!(heap_rule(&t, &h, true), None);
        let (t, h) = extra(false);
        assert_eq!(heap_rule(&t, &h, true), Some("unique.live_duplicate"));
        let (t, h) = extra(false);
        assert_eq!(heap_rule(&t, &h, false), None);
    }
}
