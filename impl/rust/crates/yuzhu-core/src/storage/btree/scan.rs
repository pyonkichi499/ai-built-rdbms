//! 索引スキャン（`IndexScan` の実体。`m4/06-btree.md` §4.6、§5.9、§5.10）。
//!
//! 葉を 1 枚ずつ共有ラッチして、範囲に入る項目の TID をまとめてコピーし、ラッチを外す。呼び出しの間は
//! ピンもラッチも持たず（D6-8）、次に読む葉のブロック番号だけを覚える。
//!
//! `next` は `m4/06-btree.md` §4.6 の署名に `wal` を足している（降下に使う `BtCtx` が `Wal` を持つため）。

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;

use super::BtCtx;
use super::page::{BtSpecial, item, validate_page};
use super::search::{Rest, SearchKey, descend, find_first, with_covering_page};
use super::tuple::{IndexTuple, decode_key};
use crate::catalog::opclass::column_comparator;
use crate::error::{Error, Result};
use crate::storage::buffer::BufferPool;
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::storage::{IndexHandle, IndexKeyColumn, ResolvedScanKeys, ScanDirection};
use crate::types::{CmpFn, Datum, Tid};
use crate::wal::Wal;

/// 索引走査の状態。
#[derive(Debug)]
pub struct IndexScan {
    index: Arc<IndexHandle>,
    dir: ScanDirection,
    bounds: ScanBounds,
    state: ScanState,
    /// 現在の葉から集めた一致項目（走査順）。
    buf: VecDeque<Tid>,
    /// 調べた項目の数（テストと EXPLAIN ANALYZE 向け）。
    examined: u64,
    /// 単体テスト用: `FakeIndexStore`（`executor/nodes/test_util.rs`）が返す TID の列。
    #[cfg(test)]
    pub(crate) fake_tids: VecDeque<Tid>,
}

#[derive(Clone, Copy, Debug)]
enum ScanState {
    NotStarted,
    /// 次に読む葉（読んだ時点の右リンク）。`None` は右端まで読んだ。
    Forward {
        next: Option<BlockNumber>,
    },
    /// 最後に読んだ葉（`origin`）と、その左リンク。`None` は最左まで読んだ。
    Backward {
        origin: BlockNumber,
        prev: Option<BlockNumber>,
    },
    Done,
}

#[cfg(test)]
impl IndexScan {
    /// `FakeIndexStore::begin_scan` が使う。`tids` を順に返す走査。
    pub(crate) fn for_test(tids: Vec<Tid>) -> IndexScan {
        let index = crate::storage::btree::testing::index_handle(
            &[(crate::types::SqlType::INT4, false, false)],
            false,
        );
        IndexScan {
            index: Arc::new(index),
            dir: ScanDirection::Forward,
            bounds: ScanBounds::default(),
            state: ScanState::Done,
            buf: VecDeque::new(),
            examined: 0,
            fake_tids: tids.into(),
        }
    }
}

impl IndexScan {
    /// これまでに調べた項目の数。
    pub fn examined(&self) -> u64 {
        self.examined
    }
}

#[derive(Debug)]
struct ScanBound {
    value: Datum,
    inclusive: bool,
    cmp: CmpFn,
}

#[derive(Debug, Default)]
struct ScanBounds {
    /// `eq`（`IS NULL` は `Datum::Null`）。
    prefix: Vec<Datum>,
    /// 列 `j` の値と `prefix[j]` を比べる関数（`IS NULL` の列では使わない）。
    prefix_cmp: Vec<Option<CmpFn>>,
    /// `eq.len()`（`lower` / `upper` があるときだけ）。
    range_col: Option<usize>,
    /// 索引の並びでの最初・最後の境界。
    first: Option<ScanBound>,
    last: Option<ScanBound>,
}

/// 項目が範囲の手前・中・後のどこにあるか（索引の並びで）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    Before,
    In,
    After,
}

/// 列の値と境界の値を、索引の並び（DESC・NULLS FIRST を反映）で比べる。
fn cmp_bound(col: &IndexKeyColumn, cmp: CmpFn, item: &Datum, bound: &Datum) -> Ordering {
    match (item.is_null(), bound.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if col.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if col.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let o = cmp(item, bound);
            if col.descending { o.reverse() } else { o }
        }
    }
}

fn resolve(index: &IndexHandle, col: usize, value: &Datum) -> Result<CmpFn> {
    let c = &index.columns[col];
    column_comparator(c.ty.oid, value).ok_or_else(|| {
        Error::internal(format!(
            "no comparison function for column \"{}\" of index \"{}\" and the scan key value",
            c.name, index.name
        ))
    })
}

impl ScanBounds {
    fn new(index: &IndexHandle, keys: &ResolvedScanKeys) -> Result<ScanBounds> {
        let ncols = index.columns.len();
        let has_range = keys.lower.is_some() || keys.upper.is_some();
        if keys.eq.len() > ncols {
            return Err(Error::internal(format!(
                "index scan has {} equality keys for {ncols} index columns",
                keys.eq.len()
            )));
        }
        if has_range && keys.eq.len() >= ncols {
            return Err(Error::internal(
                "index scan has a range key but no column is left for it",
            ));
        }
        let mut prefix = Vec::with_capacity(keys.eq.len());
        let mut prefix_cmp = Vec::with_capacity(keys.eq.len());
        for (j, e) in keys.eq.iter().enumerate() {
            match e {
                Some(Datum::Null) => {
                    return Err(Error::internal("index scan equality key is a NULL value"));
                }
                Some(v) => {
                    prefix_cmp.push(Some(resolve(index, j, v)?));
                    prefix.push(v.clone());
                }
                None => {
                    prefix_cmp.push(None);
                    prefix.push(Datum::Null);
                }
            }
        }
        let range_col = has_range.then_some(keys.eq.len());
        let mk = |b: &Option<(Datum, bool)>| -> Result<Option<ScanBound>> {
            let (Some((v, inclusive)), Some(rc)) = (b, range_col) else {
                return Ok(None);
            };
            if v.is_null() {
                return Err(Error::internal("index scan range key is a NULL value"));
            }
            Ok(Some(ScanBound {
                value: v.clone(),
                inclusive: *inclusive,
                cmp: resolve(index, rc, v)?,
            }))
        };
        let lower = mk(&keys.lower)?;
        let upper = mk(&keys.upper)?;
        let (first, last) = match range_col {
            Some(rc) if index.columns[rc].descending => (upper, lower),
            _ => (lower, upper),
        };
        Ok(ScanBounds {
            prefix,
            prefix_cmp,
            range_col,
            first,
            last,
        })
    }

    fn classify(&self, index: &IndexHandle, key: &[Datum]) -> Placement {
        for (j, p) in self.prefix.iter().enumerate() {
            let col = &index.columns[j];
            let cmp = self.prefix_cmp[j].unwrap_or(col.cmp);
            match cmp_bound(col, cmp, &key[j], p) {
                Ordering::Less => return Placement::Before,
                Ordering::Greater => return Placement::After,
                Ordering::Equal => {}
            }
        }
        let Some(rc) = self.range_col else {
            return Placement::In;
        };
        let col = &index.columns[rc];
        let x = &key[rc];
        if x.is_null() {
            return if col.nulls_first {
                Placement::Before
            } else {
                Placement::After
            };
        }
        if let Some(f) = &self.first {
            let c = cmp_bound(col, f.cmp, x, &f.value);
            if c == Ordering::Less || (c == Ordering::Equal && !f.inclusive) {
                return Placement::Before;
            }
        }
        if let Some(l) = &self.last {
            let c = cmp_bound(col, l.cmp, x, &l.value);
            if c == Ordering::Greater || (c == Ordering::Equal && !l.inclusive) {
                return Placement::After;
            }
        }
        Placement::In
    }

    /// 開始位置の探索キー（`find_first(.., strict = false)` = 最初の項目 `>= key`）。
    fn start_key(&self, index: &IndexHandle, dir: ScanDirection) -> (Vec<Datum>, Rest) {
        let mut cols = self.prefix.clone();
        let nulls_first = self.range_col.map(|rc| index.columns[rc].nulls_first);
        match dir {
            ScanDirection::Forward => {
                if let Some(f) = &self.first {
                    cols.push(f.value.clone());
                    (cols, if f.inclusive { Rest::Low } else { Rest::High })
                } else if nulls_first == Some(true) {
                    cols.push(Datum::Null);
                    (cols, Rest::High)
                } else {
                    (cols, Rest::Low)
                }
            }
            ScanDirection::Backward => {
                if let Some(l) = &self.last {
                    cols.push(l.value.clone());
                    (cols, if l.inclusive { Rest::High } else { Rest::Low })
                } else if nulls_first == Some(false) {
                    cols.push(Datum::Null);
                    (cols, Rest::Low)
                } else {
                    (cols, Rest::High)
                }
            }
        }
    }
}

/// `begin_scan` の本体。I/O はしない（境界の検査と比較関数の解決だけ）。
pub(crate) fn begin(
    index: &IndexHandle,
    keys: &ResolvedScanKeys,
    dir: ScanDirection,
) -> Result<IndexScan> {
    let bounds = ScanBounds::new(index, keys)?;
    Ok(IndexScan {
        index: Arc::new(index.clone()),
        dir,
        bounds,
        state: ScanState::NotStarted,
        buf: VecDeque::new(),
        examined: 0,
        #[cfg(test)]
        fake_tids: VecDeque::new(),
    })
}

/// 1 枚の葉から集めた結果。
struct Collected {
    tids: Vec<Tid>,
    /// 範囲を抜けた（これ以上の葉は読まない）。
    done: bool,
    examined: u64,
}

/// 葉 `page` の行ポインタ番号 `offs` を順に調べて、範囲に入る TID を集める。
fn collect(
    ctx: &BtCtx<'_>,
    bounds: &ScanBounds,
    page: &Page,
    blk: BlockNumber,
    backward: bool,
    offs: impl Iterator<Item = u16>,
) -> Result<Collected> {
    let ncols = ctx.index.columns.len();
    let mut out = Collected {
        tids: Vec::new(),
        done: false,
        examined: 0,
    };
    for off in offs {
        let bytes = item(ctx, page, blk, off)?;
        let t = IndexTuple(bytes);
        if t.is_pivot() {
            return Err(ctx.corrupted(blk, "a pivot tuple in a leaf page"));
        }
        let key = decode_key(ctx.index, &t, ncols)?;
        out.examined += 1;
        match (bounds.classify(ctx.index, &key), backward) {
            (Placement::In, _) => out.tids.push(
                t.heap_tid()
                    .ok_or_else(|| ctx.corrupted(blk, "a leaf tuple without a heap TID"))?,
            ),
            (Placement::After, false) | (Placement::Before, true) => {
                out.done = true;
                break;
            }
            (Placement::Before, false) | (Placement::After, true) => {}
        }
    }
    Ok(out)
}

fn nonzero(b: BlockNumber) -> Option<BlockNumber> {
    (b != 0).then_some(b)
}

/// `origin` の左隣の葉を探して、その共有ラッチの下で `f` を呼ぶ（PostgreSQL の `_bt_walk_left`。
/// ページの削除がないので 4 段目は不要）。`start` が分割されていたら右へ寄って `origin` の手前を探す。
fn walk_left<R>(
    ctx: &BtCtx<'_>,
    start: BlockNumber,
    origin: BlockNumber,
    f: &mut dyn FnMut(&Page, BlockNumber, &BtSpecial) -> Result<R>,
) -> Result<R> {
    let mut nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut blk = start;
    let mut i = 0u32;
    while i <= nblocks {
        i += 1;
        #[cfg(test)]
        super::testing::fire(super::testing::HookPoint::WalkLeftBeforeLatch { target: blk });
        let pin = ctx.pool.read_buffer(ctx.tag(blk))?;
        let g = pin.read_tree()?;
        let sp = validate_page(ctx, &g, blk, Some(0))?;
        if sp.next == origin {
            return f(&g, blk, &sp);
        }
        if sp.next == 0 {
            return Err(ctx.corrupted(
                blk,
                format!("cannot find the left sibling of block {origin}"),
            ));
        }
        blk = sp.next;
        if i > nblocks {
            // 並行する分割でファイルが伸びたかもしれないので、読み直す。
            nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
        }
    }
    Err(ctx.corrupted(start, "sibling chain is cyclic"))
}

/// 最初の降下と、最初の葉の読み込み。
fn start(ctx: &BtCtx<'_>, scan: &mut IndexScan) -> Result<()> {
    let backward = scan.dir == ScanDirection::Backward;
    let (cols, rest) = scan.bounds.start_key(ctx.index, scan.dir);
    let key = SearchKey {
        cols: &cols,
        tid: None,
        rest,
    };
    let d = descend(ctx, &key)?;
    let bounds = &scan.bounds;
    let (got, blk, sp) = with_covering_page(ctx, d.leaf, 0, &key, &mut |page, blk, sp| {
        let pos = find_first(ctx, page, blk, sp, &key, false)?;
        let got = if backward {
            // 最初の項目 >= key の手前から後ろへ。
            let first = sp.first_data_offset();
            collect(ctx, bounds, page, blk, true, (first..pos).rev())?
        } else {
            collect(ctx, bounds, page, blk, false, pos..=page.max_offset())?
        };
        Ok((got, blk, *sp))
    })?;
    finish_leaf(scan, got, blk, &sp);
    Ok(())
}

fn finish_leaf(scan: &mut IndexScan, got: Collected, blk: BlockNumber, sp: &BtSpecial) {
    scan.examined += got.examined;
    scan.buf.extend(got.tids);
    scan.state = if got.done {
        ScanState::Done
    } else if scan.dir == ScanDirection::Forward {
        ScanState::Forward {
            next: nonzero(sp.next),
        }
    } else {
        ScanState::Backward {
            origin: blk,
            prev: nonzero(sp.prev),
        }
    };
}

/// 次の TID。葉ごとに一致した TID をまとめてコピーし、ページのラッチ・ピンを持ち越さない。
pub(crate) fn next(pool: &Arc<BufferPool>, wal: &Wal, scan: &mut IndexScan) -> Result<Option<Tid>> {
    let index = Arc::clone(&scan.index);
    let ctx = BtCtx {
        pool,
        wal,
        index: &index,
        knobs: crate::debug_knobs::DebugKnobs::default(),
    };
    loop {
        if let Some(t) = scan.buf.pop_front() {
            return Ok(Some(t));
        }
        match scan.state {
            ScanState::Done => return Ok(None),
            ScanState::NotStarted => start(&ctx, scan)?,
            ScanState::Forward { next: None } | ScanState::Backward { prev: None, .. } => {
                scan.state = ScanState::Done;
            }
            ScanState::Forward { next: Some(b) } => {
                #[cfg(test)]
                super::testing::fire(super::testing::HookPoint::ScanBetweenLeaves);
                let pin = pool.read_buffer(ctx.tag(b))?;
                let g = pin.read_tree()?;
                let sp = validate_page(&ctx, &g, b, Some(0))?;
                let first = sp.first_data_offset();
                let got = collect(&ctx, &scan.bounds, &g, b, false, first..=g.max_offset())?;
                drop(g);
                finish_leaf(scan, got, b, &sp);
            }
            ScanState::Backward {
                origin,
                prev: Some(p),
            } => {
                #[cfg(test)]
                super::testing::fire(super::testing::HookPoint::ScanBetweenLeaves);
                let bounds = &scan.bounds;
                let (got, blk, sp) = walk_left(&ctx, p, origin, &mut |page, blk, sp| {
                    let first = sp.first_data_offset();
                    let got = collect(
                        &ctx,
                        bounds,
                        page,
                        blk,
                        true,
                        (first..=page.max_offset()).rev(),
                    )?;
                    Ok((got, blk, *sp))
                })?;
                finish_leaf(scan, got, blk, &sp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    use super::*;
    use crate::error::sqlstate;
    use crate::storage::btree::build::build_index;
    use crate::storage::btree::check::{check_structure, dump_entries};
    use crate::storage::btree::testing::{
        HookPoint, Lcg, TestIndex, new_index, put, set_test_hook, tid_of,
    };
    use crate::storage::btree::{cmp_key_tid, insert as bt_insert};
    use crate::storage::{BuildUnique, UniqueCheck};
    use crate::types::{SqlType, cmp_datum};

    type Entry = (Vec<Datum>, Tid);
    type Cols = [(SqlType, bool, bool)];

    fn int(v: i32) -> Datum {
        Datum::Int4(v)
    }

    fn build_from(cols: &Cols, mut entries: Vec<Entry>) -> TestIndex {
        let t = new_index(cols, false);
        entries.sort_by(|a, b| cmp_key_tid(&t.handle, &a.0, a.1, &b.0, b.1));
        let mut it = entries.into_iter();
        build_index(&t.ctx(), &t.write_ctx(), &mut it, BuildUnique::No).unwrap();
        t
    }

    fn run(t: &TestIndex, keys: &ResolvedScanKeys, dir: ScanDirection) -> (Vec<Tid>, u64) {
        let mut s = begin(&t.handle, keys, dir).unwrap();
        let mut out = Vec::new();
        while let Some(x) = next(t.pool(), t.wal(), &mut s).unwrap() {
            out.push(x);
        }
        (out, s.examined())
    }

    /// 独立したオラクル: 値の順序（`cmp_datum`）だけで一致を決める。
    fn matches(key: &[Datum], keys: &ResolvedScanKeys) -> bool {
        for (j, e) in keys.eq.iter().enumerate() {
            match e {
                None => {
                    if !key[j].is_null() {
                        return false;
                    }
                }
                Some(v) => {
                    if key[j].is_null() || cmp_datum(&key[j], v) != Ordering::Equal {
                        return false;
                    }
                }
            }
        }
        if keys.lower.is_some() || keys.upper.is_some() {
            let x = &key[keys.eq.len()];
            if x.is_null() {
                return false;
            }
            if let Some((v, incl)) = &keys.lower {
                let c = cmp_datum(x, v);
                if c == Ordering::Less || (c == Ordering::Equal && !incl) {
                    return false;
                }
            }
            if let Some((v, incl)) = &keys.upper {
                let c = cmp_datum(x, v);
                if c == Ordering::Greater || (c == Ordering::Equal && !incl) {
                    return false;
                }
            }
        }
        true
    }

    fn oracle(t: &TestIndex, entries: &[Entry], keys: &ResolvedScanKeys) -> Vec<Tid> {
        let mut v: Vec<&Entry> = entries.iter().filter(|e| matches(&e.0, keys)).collect();
        v.sort_by(|a, b| cmp_key_tid(&t.handle, &a.0, a.1, &b.0, b.1));
        v.into_iter().map(|e| e.1).collect()
    }

    fn assert_scan(t: &TestIndex, entries: &[Entry], keys: &ResolvedScanKeys) {
        let want = oracle(t, entries, keys);
        let (fwd, _) = run(t, keys, ScanDirection::Forward);
        assert_eq!(fwd, want, "forward {keys:?}");
        let (bwd, _) = run(t, keys, ScanDirection::Backward);
        let mut rev = want;
        rev.reverse();
        assert_eq!(bwd, rev, "backward {keys:?}");
    }

    /// 全条件の組み合わせ（`eq` が 0..=ncols 列、範囲の包含・排他の全組み合わせ）。
    fn conditions(
        eq_cands: &[Vec<Option<Datum>>],
        range_cands: &[Vec<Datum>],
    ) -> Vec<ResolvedScanKeys> {
        let ncols = eq_cands.len();
        let mut out = Vec::new();
        let mut prefixes: Vec<Vec<Option<Datum>>> = vec![vec![]];
        for eq_len in 0..=ncols {
            for p in &prefixes {
                if eq_len == ncols {
                    out.push(ResolvedScanKeys {
                        eq: p.clone(),
                        lower: None,
                        upper: None,
                    });
                    continue;
                }
                let mut bounds: Vec<Option<(Datum, bool)>> = vec![None];
                for v in &range_cands[eq_len] {
                    bounds.push(Some((v.clone(), true)));
                    bounds.push(Some((v.clone(), false)));
                }
                for lo in &bounds {
                    for up in &bounds {
                        out.push(ResolvedScanKeys {
                            eq: p.clone(),
                            lower: lo.clone(),
                            upper: up.clone(),
                        });
                    }
                }
            }
            if eq_len < ncols {
                let mut next_p = Vec::new();
                for p in &prefixes {
                    for c in &eq_cands[eq_len] {
                        let mut q = p.clone();
                        q.push(c.clone());
                        next_p.push(q);
                    }
                }
                prefixes = next_p;
            }
        }
        out
    }

    fn some_ints(vs: &[i32]) -> Vec<Option<Datum>> {
        vs.iter().map(|&v| Some(int(v))).chain([None]).collect()
    }

    fn single_int_data() -> Vec<Entry> {
        let mut e = Vec::new();
        let mut n = 0;
        for v in 0..40 {
            for _ in 0..60 {
                n += 1;
                e.push((vec![int(v)], tid_of(n)));
            }
        }
        for _ in 0..80 {
            n += 1;
            e.push((vec![Datum::Null], tid_of(n)));
        }
        e
    }

    #[test]
    fn single_column_schemas_match_the_oracle() {
        let ints = [-1, 0, 1, 5, 20, 39, 40, 100];
        let ranges: Vec<Vec<Datum>> = vec![ints.iter().map(|&v| int(v)).collect()];
        let eq: Vec<Vec<Option<Datum>>> = vec![some_ints(&ints)];
        let conds = conditions(&eq, &ranges);
        assert!(conds.len() > 100);
        for cols in [
            [(SqlType::INT4, false, false)],
            [(SqlType::INT4, true, true)],
            [(SqlType::INT4, false, true)],
            [(SqlType::INT4, true, false)],
        ] {
            let entries = single_int_data();
            let t = build_from(&cols, entries.clone());
            check_structure(t.pool(), &t.handle).unwrap();
            for c in &conds {
                assert_scan(&t, &entries, c);
            }
            t.assert_clean();
        }
    }

    #[test]
    fn two_column_schemas_match_the_oracle() {
        let mut entries: Vec<Entry> = Vec::new();
        let mut n = 0;
        for a in 0..8 {
            for b in 0..300 {
                if b % 3 == 0 && a % 2 == 0 {
                    continue;
                }
                n += 1;
                entries.push((vec![int(a), int(b)], tid_of(n)));
            }
        }
        for a in [Some(1), Some(3), None] {
            for b in [Some(7), None, Some(299)] {
                n += 1;
                entries.push((
                    vec![a.map_or(Datum::Null, int), b.map_or(Datum::Null, int)],
                    tid_of(n),
                ));
            }
        }
        let a_vals = [-1, 0, 3, 7, 99];
        let b_vals = [-1, 3, 20, 299, 300];
        let ranges = vec![
            a_vals.iter().map(|&v| int(v)).collect(),
            b_vals.iter().map(|&v| int(v)).collect(),
        ];
        let eq = vec![some_ints(&[0, 3, 7, 99]), some_ints(&[3, 20, 299])];
        let conds = conditions(&eq, &ranges);
        for cols in [
            [(SqlType::INT4, false, false), (SqlType::INT4, false, false)],
            [(SqlType::INT4, true, true), (SqlType::INT4, false, true)],
            [(SqlType::INT4, false, true), (SqlType::INT4, true, false)],
        ] {
            let t = build_from(&cols, entries.clone());
            check_structure(t.pool(), &t.handle).unwrap();
            for c in &conds {
                assert_scan(&t, &entries, c);
            }
        }
    }

    #[test]
    fn int_and_descending_text_matches_the_oracle() {
        let mut entries: Vec<Entry> = Vec::new();
        let mut n = 0;
        for a in 0..6 {
            for b in 0..250 {
                n += 1;
                entries.push((vec![int(a), Datum::Text(format!("t{b:03}"))], tid_of(n)));
            }
            n += 1;
            entries.push((vec![int(a), Datum::Null], tid_of(n)));
        }
        let texts =
            |vs: &[&str]| -> Vec<Datum> { vs.iter().map(|s| Datum::Text((*s).into())).collect() };
        let ranges = vec![
            [0, 2, 5, 9].iter().map(|&v| int(v)).collect(),
            texts(&["a", "t000", "t125", "t249", "t250"]),
        ];
        let eq: Vec<Vec<Option<Datum>>> = vec![
            some_ints(&[0, 2, 5]),
            texts(&["t000", "t100", "t249"])
                .into_iter()
                .map(Some)
                .chain([None])
                .collect(),
        ];
        let conds = conditions(&eq, &ranges);
        let cols = [(SqlType::INT4, false, false), (SqlType::TEXT, true, false)];
        let t = build_from(&cols, entries.clone());
        check_structure(t.pool(), &t.handle).unwrap();
        for c in &conds {
            assert_scan(&t, &entries, c);
        }
    }

    #[test]
    fn index_built_by_inserts_scans_the_same() {
        let entries = single_int_data();
        let t = new_index(&[(SqlType::INT4, true, true)], false);
        let order = Lcg(7).permutation(entries.len());
        for i in order {
            put(&t, &entries[i].0, entries[i].1).unwrap();
        }
        let ranges = vec![[0, 5, 39, 40].iter().map(|&v| int(v)).collect()];
        let eq = vec![some_ints(&[0, 5, 39])];
        for c in conditions(&eq, &ranges) {
            assert_scan(&t, &entries, &c);
        }
    }

    #[test]
    fn null_regions_are_skipped_not_scanned() {
        let entries = single_int_data();
        let total = entries.len() as u64;
        // ASC NULLS LAST: a > 35 は NULL の領域に入ったところで止まる。
        let t = build_from(&[(SqlType::INT4, false, false)], entries.clone());
        let keys = ResolvedScanKeys {
            eq: vec![],
            lower: Some((int(35), false)),
            upper: None,
        };
        let want = oracle(&t, &entries, &keys).len() as u64;
        let (_, ex) = run(&t, &keys, ScanDirection::Forward);
        assert!(ex <= want + 1 && ex < total / 4, "examined {ex} of {total}");
        let (_, ex) = run(&t, &keys, ScanDirection::Backward);
        assert!(ex <= want + 1, "examined {ex}");

        // ASC NULLS FIRST: a < 5 は NULL の領域を飛ばして始まる。
        let t = build_from(&[(SqlType::INT4, false, true)], entries.clone());
        let keys = ResolvedScanKeys {
            eq: vec![],
            lower: None,
            upper: Some((int(5), false)),
        };
        let want = oracle(&t, &entries, &keys).len() as u64;
        let (got, ex) = run(&t, &keys, ScanDirection::Forward);
        assert_eq!(got.len() as u64, want);
        assert!(ex <= want + 1, "examined {ex}");
        let (_, ex) = run(&t, &keys, ScanDirection::Backward);
        assert!(ex <= want + 1, "examined {ex}");
    }

    #[test]
    fn scans_cover_page_boundaries() {
        let entries: Vec<Entry> = (1..=3000u32)
            .map(|i| (vec![int(i32::try_from(i).unwrap())], tid_of(i)))
            .collect();
        let t = build_from(&[(SqlType::INT4, false, false)], entries.clone());
        // 葉の最初・最後の項目を境界にする。
        let ctx = t.ctx();
        let mut bounds = Vec::new();
        let mut blk = 1;
        let mut leaves = 0;
        loop {
            let pin = t.pool().read_buffer(ctx.tag(blk)).unwrap();
            let g = pin.read_tree().unwrap();
            if BtSpecial::read(&g).unwrap().level == 0 {
                let sp = BtSpecial::read(&g).unwrap();
                let key_at = |off| {
                    decode_key(&t.handle, &IndexTuple(g.item(off).unwrap()), 1).unwrap()[0].clone()
                };
                bounds.push(key_at(sp.first_data_offset()));
                bounds.push(key_at(g.max_offset()));
                leaves += 1;
            }
            drop(g);
            drop(pin);
            blk += 1;
            if blk
                >= t.pool()
                    .nblocks(t.handle.locator, ForkNumber::Main)
                    .unwrap()
            {
                break;
            }
        }
        assert!(leaves >= 5);
        for v in bounds {
            for incl in [true, false] {
                for (lo, up) in [
                    (Some((v.clone(), incl)), None),
                    (None, Some((v.clone(), incl))),
                ] {
                    assert_scan(
                        &t,
                        &entries,
                        &ResolvedScanKeys {
                            eq: vec![],
                            lower: lo,
                            upper: up,
                        },
                    );
                }
            }
        }
        // 範囲が全体の外、ちょうど 1 点、空。
        for keys in [
            ResolvedScanKeys {
                eq: vec![],
                lower: Some((int(5000), true)),
                upper: None,
            },
            ResolvedScanKeys {
                eq: vec![],
                lower: None,
                upper: Some((int(0), true)),
            },
            ResolvedScanKeys {
                eq: vec![Some(int(1500))],
                lower: None,
                upper: None,
            },
            ResolvedScanKeys {
                eq: vec![],
                lower: Some((int(10), false)),
                upper: Some((int(10), true)),
            },
        ] {
            assert_scan(&t, &entries, &keys);
        }
        t.assert_clean();
    }

    #[test]
    fn empty_index_and_single_page() {
        let t = new_index(&[(SqlType::INT4, false, false)], false);
        for dir in [ScanDirection::Forward, ScanDirection::Backward] {
            assert!(run(&t, &ResolvedScanKeys::default(), dir).0.is_empty());
        }
        put(&t, &[int(1)], tid_of(1)).unwrap();
        for dir in [ScanDirection::Forward, ScanDirection::Backward] {
            assert_eq!(
                run(&t, &ResolvedScanKeys::default(), dir).0,
                vec![tid_of(1)]
            );
        }
    }

    #[test]
    fn values_of_another_type_of_the_same_family() {
        // int4 の列に int8 の定数。
        let entries: Vec<Entry> = (0..500u32)
            .map(|i| (vec![int(i32::try_from(i).unwrap())], tid_of(i + 1)))
            .collect();
        let t = build_from(&[(SqlType::INT4, false, false)], entries.clone());
        for keys in [
            ResolvedScanKeys {
                eq: vec![Some(Datum::Int8(250))],
                ..ResolvedScanKeys::default()
            },
            ResolvedScanKeys {
                eq: vec![],
                lower: Some((Datum::Int8(490), false)),
                upper: Some((Datum::Int8(1 << 40), true)),
            },
            ResolvedScanKeys {
                eq: vec![],
                lower: Some((Datum::Int8(-(1 << 40)), true)),
                upper: Some((Datum::Int8(3), true)),
            },
        ] {
            assert_scan(&t, &entries, &keys);
        }
        // float4 の列に float8 の定数。
        let entries: Vec<Entry> = (0..300u32)
            .map(|i| (vec![Datum::Float4(i as f32 / 2.0)], tid_of(i + 1)))
            .collect();
        let t = build_from(&[(SqlType::FLOAT4, false, false)], entries.clone());
        for keys in [
            ResolvedScanKeys {
                eq: vec![Some(Datum::Float8(12.5))],
                ..ResolvedScanKeys::default()
            },
            ResolvedScanKeys {
                eq: vec![],
                lower: Some((Datum::Float8(100.25), true)),
                upper: Some((Datum::Float8(110.0), false)),
            },
        ] {
            assert_scan(&t, &entries, &keys);
        }
        // name / varchar の列に text の定数。
        for ty in [SqlType::NAME, SqlType::VARCHAR, SqlType::TEXT] {
            let entries: Vec<Entry> = (0..300u32)
                .map(|i| (vec![Datum::Text(format!("k{i:04}"))], tid_of(i + 1)))
                .collect();
            let t = build_from(&[(ty, false, false)], entries.clone());
            for keys in [
                ResolvedScanKeys {
                    eq: vec![Some(Datum::Text("k0123".into()))],
                    ..ResolvedScanKeys::default()
                },
                ResolvedScanKeys {
                    eq: vec![],
                    lower: Some((Datum::Text("k0250".into()), true)),
                    upper: None,
                },
            ] {
                assert_scan(&t, &entries, &keys);
            }
        }
    }

    #[test]
    fn numeric_scale_does_not_matter() {
        let n = |s: &str| Datum::Numeric(yuzhu_numeric::Numeric::parse(s).unwrap());
        let entries: Vec<Entry> = vec![
            (vec![n("1.0")], tid_of(1)),
            (vec![n("1.00")], tid_of(2)),
            (vec![n("2")], tid_of(3)),
            (vec![n("0.5")], tid_of(4)),
        ];
        let t = build_from(&[(SqlType::NUMERIC, false, false)], entries.clone());
        let keys = ResolvedScanKeys {
            eq: vec![Some(n("1.000"))],
            ..ResolvedScanKeys::default()
        };
        assert_eq!(
            run(&t, &keys, ScanDirection::Forward).0,
            vec![tid_of(1), tid_of(2)]
        );
        assert_scan(&t, &entries, &keys);
    }

    #[test]
    fn bad_scan_keys_are_internal_errors() {
        let t = new_index(
            &[(SqlType::INT4, false, false), (SqlType::INT4, false, false)],
            false,
        );
        let bad = |keys: ResolvedScanKeys| {
            let e = begin(&t.handle, &keys, ScanDirection::Forward).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        };
        bad(ResolvedScanKeys {
            eq: vec![Some(int(1)), Some(int(2)), Some(int(3))],
            ..ResolvedScanKeys::default()
        });
        bad(ResolvedScanKeys {
            eq: vec![Some(int(1)), Some(int(2))],
            lower: Some((int(1), true)),
            upper: None,
        });
        bad(ResolvedScanKeys {
            eq: vec![Some(Datum::Null)],
            ..ResolvedScanKeys::default()
        });
        bad(ResolvedScanKeys {
            eq: vec![],
            lower: Some((Datum::Null, true)),
            upper: None,
        });
        // 解決できない型（date の列に int4 の値）。
        let d = new_index(&[(SqlType::DATE, false, false)], false);
        let e = begin(
            &d.handle,
            &ResolvedScanKeys {
                eq: vec![Some(int(1))],
                ..ResolvedScanKeys::default()
            },
            ScanDirection::Forward,
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn begin_does_no_io() {
        let t = crate::storage::btree::testing::test_index(&[(SqlType::INT4, false, false)], false);
        // ファイルが空（init_index 前）でも begin は成功し、最初の next で初めて失敗する。
        let mut s = begin(
            &t.handle,
            &ResolvedScanKeys::default(),
            ScanDirection::Forward,
        )
        .unwrap();
        assert!(next(t.pool(), t.wal(), &mut s).is_err());
    }

    #[test]
    fn corrupted_pages_are_xx001_in_scans() {
        let entries: Vec<Entry> = (1..=2000u32)
            .map(|i| (vec![int(i32::try_from(i).unwrap())], tid_of(i)))
            .collect();
        let t = build_from(&[(SqlType::INT4, false, false)], entries);
        {
            let pin = t.pool().read_buffer(t.ctx().tag(2)).unwrap();
            let mut g = pin.write_tree().unwrap();
            let mut sp = BtSpecial::read(g.page()).unwrap();
            sp.flags |= 0x0004;
            sp.write(g.page_mut_hint());
        }
        let (mut s, e) = (
            begin(
                &t.handle,
                &ResolvedScanKeys::default(),
                ScanDirection::Forward,
            )
            .unwrap(),
            sqlstate::DATA_CORRUPTED,
        );
        let err = loop {
            match next(t.pool(), t.wal(), &mut s) {
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected corruption"),
                Err(err) => break err,
            }
        };
        assert_eq!(err.sqlstate, e);
        assert!(err.message.contains("contains a corrupted page at block 2"));
        t.assert_clean();
    }

    // ----- 並行する分割（§7.8。単一スレッドで手順を刻む） ----------------------------------

    /// 1 ページに収まらない数の項目を持つ構築済みの木（キー = `1..=n`、TID = `tid_of(キー)`）。
    fn built(n: u32) -> Rc<TestIndex> {
        let entries: Vec<Entry> = (1..=n)
            .map(|i| (vec![int(i32::try_from(i).unwrap())], tid_of(i)))
            .collect();
        Rc::new(build_from(&[(SqlType::INT4, false, false)], entries))
    }

    fn insert_many(t: &TestIndex, keys: &[i32], first_tid: u32) {
        for (i, &k) in keys.iter().enumerate() {
            put(t, &[int(k)], tid_of(first_tid + u32::try_from(i).unwrap())).unwrap();
        }
    }

    fn key_of_block(t: &TestIndex, blk: u32) -> i32 {
        let pin = t.pool().read_buffer(t.ctx().tag(blk)).unwrap();
        let g = pin.read_tree().unwrap();
        let sp = BtSpecial::read(&g).unwrap();
        let k = decode_key(
            &t.handle,
            &IndexTuple(g.item(sp.first_data_offset()).unwrap()),
            1,
        )
        .unwrap();
        match k[0] {
            Datum::Int4(v) => v,
            _ => panic!("int4 key expected"),
        }
    }

    /// 開始時の集合がちょうど 1 回ずつ（方向の順で）、重複なく返ること。
    fn verify(
        t: &TestIndex,
        initial: &[Tid],
        added: &HashMap<Tid, i32>,
        got: &[Tid],
        dir: ScanDirection,
    ) {
        let mut seen = HashSet::new();
        for g in got {
            assert!(seen.insert(*g), "duplicate {g:?}");
        }
        for i in initial {
            assert!(seen.contains(i), "missing {i:?}");
        }
        let key = |t: Tid| -> i32 {
            added
                .get(&t)
                .copied()
                .unwrap_or_else(|| i32::try_from(t.block * 1000 + u32::from(t.offset) - 1).unwrap())
        };
        let keys: Vec<i32> = got.iter().map(|&x| key(x)).collect();
        for w in keys.windows(2) {
            match dir {
                ScanDirection::Forward => assert!(w[0] <= w[1], "{w:?}"),
                ScanDirection::Backward => assert!(w[0] >= w[1], "{w:?}"),
            }
        }
        check_structure(t.pool(), &t.handle).unwrap();
    }

    fn all(t: &TestIndex, s: &mut IndexScan) -> Vec<Tid> {
        let mut out = Vec::new();
        while let Some(x) = next(t.pool(), t.wal(), s).unwrap() {
            out.push(x);
        }
        out
    }

    fn initial_tids(n: u32) -> Vec<Tid> {
        (1..=n).map(tid_of).collect()
    }

    #[test]
    fn forward_scan_survives_splits_between_leaves() {
        for seed in 1..=4u64 {
            let t = built(3000);
            let added = Rc::new(RefCell::new(HashMap::new()));
            let counter = Rc::new(RefCell::new(1_000_000u32));
            let rng = Rc::new(RefCell::new(Lcg(seed)));
            let (t2, a2, c2, r2) = (
                Rc::clone(&t),
                Rc::clone(&added),
                Rc::clone(&counter),
                Rc::clone(&rng),
            );
            set_test_hook(Some(Box::new(move |p| {
                if p != HookPoint::ScanBetweenLeaves {
                    return;
                }
                let keys: Vec<i32> = (0..150)
                    .map(|_| i32::try_from(r2.borrow_mut().below(3000) + 1).unwrap())
                    .collect();
                let first = *c2.borrow();
                insert_many(&t2, &keys, first);
                for (i, k) in keys.iter().enumerate() {
                    a2.borrow_mut()
                        .insert(tid_of(first + u32::try_from(i).unwrap()), *k);
                }
                *c2.borrow_mut() += 150;
            })));
            let mut s = begin(
                &t.handle,
                &ResolvedScanKeys::default(),
                ScanDirection::Forward,
            )
            .unwrap();
            let got = all(&t, &mut s);
            set_test_hook(None);
            assert!(*counter.borrow() > 1_000_000);
            verify(
                &t,
                &initial_tids(3000),
                &added.borrow(),
                &got,
                ScanDirection::Forward,
            );
            t.assert_clean();
        }
    }

    #[test]
    fn forward_scan_survives_a_root_split_during_the_descent() {
        let cols = [(SqlType::TEXT, false, false)];
        let key = |i: u32| vec![Datum::Text(format!("k{i:04}{}", "x".repeat(1500)))];
        for point in [
            HookPoint::DescendAfterMeta,
            HookPoint::DescendBetweenLevels { level: 0 },
        ] {
            let entries: Vec<Entry> = (0..12).map(|i| (key(i * 10), tid_of(i + 1))).collect();
            let t = Rc::new(build_from(&cols, entries));
            let level_before = crate::storage::btree::testing::tree_level(&t);
            let t2 = Rc::clone(&t);
            set_test_hook(Some(Box::new(move |p| {
                if p != point {
                    return;
                }
                // 十分多く入れて、ルートも分割させる。
                for i in 0..60u32 {
                    put(&t2, &key(i * 2 + 1), tid_of(1000 + i)).unwrap();
                }
            })));
            let mut s = begin(
                &t.handle,
                &ResolvedScanKeys::default(),
                ScanDirection::Forward,
            )
            .unwrap();
            let got = all(&t, &mut s);
            set_test_hook(None);
            assert!(crate::storage::btree::testing::tree_level(&t) > level_before);
            let set: HashSet<_> = got.iter().copied().collect();
            assert_eq!(set.len(), got.len(), "duplicates");
            for i in 1..=12 {
                assert!(set.contains(&tid_of(i)), "missing {i}");
            }
            check_structure(t.pool(), &t.handle).unwrap();
            t.assert_clean();
        }
    }

    #[test]
    fn descent_then_split_of_the_target_leaf() {
        let t = built(3000);
        let added = Rc::new(RefCell::new(HashMap::new()));
        let (t2, a2) = (Rc::clone(&t), Rc::clone(&added));
        set_test_hook(Some(Box::new(move |p| {
            if p != (HookPoint::DescendBetweenLevels { level: 0 }) {
                return;
            }
            let keys = vec![1500; 600];
            insert_many(&t2, &keys, 3_000_000);
            for i in 0..600u32 {
                a2.borrow_mut().insert(tid_of(3_000_000 + i), 1500);
            }
        })));
        let keys = ResolvedScanKeys {
            eq: vec![],
            lower: Some((int(1500), true)),
            upper: None,
        };
        let mut s = begin(&t.handle, &keys, ScanDirection::Forward).unwrap();
        let got = all(&t, &mut s);
        set_test_hook(None);
        for tid in initial_tids(3000).into_iter().skip(1499) {
            assert!(got.contains(&tid));
        }
        assert!(got.len() >= 1501 + 600);
        let set: HashSet<_> = got.iter().collect();
        assert_eq!(set.len(), got.len());
    }

    #[test]
    fn backward_scan_survives_a_split_of_the_left_leaf() {
        let t = built(3000);
        let nblocks0 = t
            .pool()
            .nblocks(t.handle.locator, ForkNumber::Main)
            .unwrap();
        let handled = Rc::new(RefCell::new(HashSet::new()));
        let added = Rc::new(RefCell::new(HashMap::new()));
        let (t2, a2) = (Rc::clone(&t), Rc::clone(&added));
        let fired = Rc::new(RefCell::new(0u32));
        let f2 = Rc::clone(&fired);
        let handled2 = Rc::clone(&handled);
        set_test_hook(Some(Box::new(move |p| {
            let handled = &handled2;
            let HookPoint::WalkLeftBeforeLatch { target } = p else {
                return;
            };
            // 最初からあったページを 1 回ずつだけ分割する（分割で増えたページは対象にしない）。
            if target >= nblocks0 || !handled.borrow_mut().insert(target) {
                return;
            }
            let n = *f2.borrow();
            *f2.borrow_mut() += 1;
            let k = key_of_block(&t2, target);
            let first = 4_000_000 + n * 1000;
            insert_many(&t2, &vec![k; 500], first);
            for i in 0..500u32 {
                a2.borrow_mut().insert(tid_of(first + i), k);
            }
        })));
        let mut s = begin(
            &t.handle,
            &ResolvedScanKeys::default(),
            ScanDirection::Backward,
        )
        .unwrap();
        let got = all(&t, &mut s);
        set_test_hook(None);
        assert!(*fired.borrow() >= 4);
        verify(
            &t,
            &initial_tids(3000),
            &added.borrow(),
            &got,
            ScanDirection::Backward,
        );
        t.assert_clean();
    }

    #[test]
    fn backward_scan_survives_a_split_of_its_own_leaf() {
        let t = built(3000);
        let added = Rc::new(RefCell::new(HashMap::new()));
        let (t2, a2) = (Rc::clone(&t), Rc::clone(&added));
        let fired = Rc::new(RefCell::new(0u32));
        let f2 = Rc::clone(&fired);
        set_test_hook(Some(Box::new(move |p| {
            if p != HookPoint::ScanBetweenLeaves {
                return;
            }
            let n = *f2.borrow();
            *f2.borrow_mut() += 1;
            // いま読んだ葉（最初は右端）の範囲へ入れて分割させる。
            let k = 3000 - i32::try_from(n).unwrap() * 360;
            let first = 5_000_000 + n * 1000;
            insert_many(&t2, &vec![k; 500], first);
            for i in 0..500u32 {
                a2.borrow_mut().insert(tid_of(first + i), k);
            }
        })));
        let mut s = begin(
            &t.handle,
            &ResolvedScanKeys::default(),
            ScanDirection::Backward,
        )
        .unwrap();
        let got = all(&t, &mut s);
        set_test_hook(None);
        assert!(*fired.borrow() >= 4);
        verify(
            &t,
            &initial_tids(3000),
            &added.borrow(),
            &got,
            ScanDirection::Backward,
        );
    }

    #[test]
    fn repeated_random_splits_in_both_directions() {
        for (seed, dir) in [
            (11, ScanDirection::Forward),
            (12, ScanDirection::Backward),
            (13, ScanDirection::Backward),
        ] {
            let t = built(2500);
            let added = Rc::new(RefCell::new(HashMap::new()));
            let counter = Rc::new(RefCell::new(6_000_000u32));
            let rng = Rc::new(RefCell::new(Lcg(seed)));
            let (t2, a2, c2, r2) = (
                Rc::clone(&t),
                Rc::clone(&added),
                Rc::clone(&counter),
                Rc::clone(&rng),
            );
            set_test_hook(Some(Box::new(move |_p| {
                let k = i32::try_from(r2.borrow_mut().below(2500) + 1).unwrap();
                let first = *c2.borrow();
                insert_many(&t2, &vec![k; 120], first);
                for i in 0..120u32 {
                    a2.borrow_mut().insert(tid_of(first + i), k);
                }
                *c2.borrow_mut() += 120;
            })));
            let mut s = begin(&t.handle, &ResolvedScanKeys::default(), dir).unwrap();
            let got = all(&t, &mut s);
            set_test_hook(None);
            verify(&t, &initial_tids(2500), &added.borrow(), &got, dir);
        }
    }

    #[test]
    fn writer_and_readers_run_concurrently() {
        use crate::storage::IndexStore;
        use crate::storage::index_store::BtreeStore;
        let t = built(4000);
        let store = Arc::new(BtreeStore::new(Arc::clone(t.pool()), Arc::clone(t.wal())));
        let handle = t.handle.clone();
        let w = t.write_ctx();
        std::thread::scope(|sc| {
            let writer = sc.spawn(|| {
                let mut rng = Lcg(99);
                for i in 0..4000u32 {
                    let k = i32::try_from(rng.below(4000) + 1).unwrap();
                    store
                        .insert(
                            &w,
                            &handle,
                            &[int(k)],
                            tid_of(7_000_000 + i),
                            UniqueCheck::Skip,
                        )
                        .unwrap();
                }
            });
            let readers: Vec<_> = (0..3)
                .map(|r| {
                    let (store, handle) = (&store, &handle);
                    sc.spawn(move || {
                        for round in 0..6 {
                            let dir = if (r + round) % 2 == 0 {
                                ScanDirection::Forward
                            } else {
                                ScanDirection::Backward
                            };
                            let mut s = store
                                .begin_scan(handle, &ResolvedScanKeys::default(), dir)
                                .unwrap();
                            let mut seen = HashSet::new();
                            while let Some(x) = store.scan_next(&mut s).unwrap() {
                                assert!(seen.insert(x), "duplicate {x:?}");
                            }
                            for i in 1..=4000u32 {
                                assert!(seen.contains(&tid_of(i)), "missing {i}");
                            }
                        }
                    })
                })
                .collect();
            writer.join().unwrap();
            for r in readers {
                r.join().unwrap();
            }
        });
        assert_eq!(dump_entries(t.pool(), &t.handle).unwrap().len(), 8000);
        check_structure(t.pool(), &t.handle).unwrap();
        let _ = bt_insert::insert;
    }
}
