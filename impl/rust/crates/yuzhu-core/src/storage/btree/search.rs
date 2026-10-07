//! 比較・二分探索・降下（`m4/06-btree.md` §4.5、§5.2）。
//!
//! 境界の向き（D6-1）: 内部ページの項目 `P_i` が支配する範囲は `[P_i, P_{i+1})`。high key `H` は
//! 「左ページの全項目はそれより厳密に小さい」上限で、`K >= H` なら右へ移る。

use std::cmp::Ordering;

use super::meta::BtMeta;
use super::page::{BtSpecial, item, validate_page};
use super::tuple::IndexTuple;
use super::{BT_META_BLOCK, BtCtx, cmp_tid};
use crate::error::{Error, Result};
use crate::storage::IndexHandle;
use crate::storage::IndexKeyColumn;
use crate::storage::heap::tuple::ColumnCursor;
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::types::{Datum, Tid};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rest {
    /// 残り（後ろの列とヒープ TID）が -∞。
    Low,
    /// +∞。
    High,
}

/// 探索キー。`cols` は先頭 n 列（NULL は `Datum::Null`。n <= ncols）。`tid` が `Some` なら全列が揃っているときの
/// 最後のタイブレーク。`cols` が全列でなく、または `tid` が `None` なら、残りは `rest`
/// （`Low` = 同じ接頭辞のどのタプルよりも小さい、`High` = 大きい）として扱う。したがって `rest` を使う
/// キーは、どのタプルとも `Equal` にならない。
#[derive(Clone, Copy, Debug)]
pub struct SearchKey<'a> {
    pub cols: &'a [Datum],
    pub tid: Option<Tid>,
    pub rest: Rest,
}

/// 1 列の比較（NULL どうしは `Equal`、NULL は `nulls_first` に従い先頭か末尾、DESC は非 NULL の比較だけを
/// 反転）。`col.cmp` を使う。`types::cmp::cmp_with_nulls` と同じ規則（D6-13）。
pub(crate) fn cmp_column(col: &IndexKeyColumn, a: &Datum, b: &Datum) -> Ordering {
    match (a.is_null(), b.is_null()) {
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
            let o = (col.cmp)(a, b);
            if col.descending { o.reverse() } else { o }
        }
    }
}

/// `key` を `item` と比べた結果（`Less` = key が小さい）。`item` が -∞ ピボットなら常に `Greater`。
pub(crate) fn cmp_key_item(
    index: &IndexHandle,
    key: &SearchKey<'_>,
    item: &IndexTuple<'_>,
) -> Result<Ordering> {
    if item.is_minus_infinity() {
        return Ok(Ordering::Greater);
    }
    let ncols = index.columns.len();
    let item_natts = if item.is_pivot() {
        item.pivot_natts().min(ncols)
    } else {
        ncols
    };
    let nkey = key.cols.len().min(ncols);
    let ncmp = nkey.min(item_natts);
    let mut cur = ColumnCursor::new(item.0, item.data_offset());
    for i in 0..ncmp {
        let col = &index.columns[i];
        let v = if item.is_null_at(i) {
            Datum::Null
        } else {
            cur.read_attr(&col.attr)?
        };
        let o = cmp_column(col, &key.cols[i], &v);
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    if nkey > item_natts {
        // 切り詰められた項目の残りは -∞。
        return Ok(Ordering::Greater);
    }
    if nkey < ncols {
        return Ok(rest_order(key.rest));
    }
    match key.tid {
        Some(t) => {
            let it = item
                .heap_tid()
                .ok_or_else(|| Error::internal("index item without a heap TID"))?;
            Ok(cmp_tid(t, it))
        }
        None => Ok(rest_order(key.rest)),
    }
}

fn rest_order(rest: Rest) -> Ordering {
    match rest {
        Rest::Low => Ordering::Less,
        Rest::High => Ordering::Greater,
    }
}

/// 二分探索。データ項目の範囲 `[first_data_offset, max_offset]` で、`strict == false`: `key <= item` となる
/// 最初の行ポインタ番号（下限）、`strict == true`: `key < item` となる最初の番号（上限）。なければ
/// `max_offset + 1`。
pub(crate) fn find_first(
    ctx: &BtCtx<'_>,
    page: &Page,
    block: BlockNumber,
    sp: &BtSpecial,
    key: &SearchKey<'_>,
    strict: bool,
) -> Result<u16> {
    let max = page.max_offset();
    let mut lo = sp.first_data_offset();
    let mut hi = max + 1;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let bytes = item(ctx, page, block, mid)?;
        let o = cmp_key_item(ctx.index, key, &IndexTuple(bytes))?;
        let go_left = if strict {
            o == Ordering::Less
        } else {
            o != Ordering::Greater
        };
        if go_left {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

/// `key` がこのページの範囲に入るか（右端、または `key < high key`）。偽なら右へ移る。
pub(crate) fn page_covers(
    ctx: &BtCtx<'_>,
    page: &Page,
    block: BlockNumber,
    sp: &BtSpecial,
    key: &SearchKey<'_>,
) -> Result<bool> {
    if sp.is_rightmost() {
        return Ok(true);
    }
    let bytes = item(ctx, page, block, super::BT_P_HIKEY)?;
    Ok(cmp_key_item(ctx.index, key, &IndexTuple(bytes))? == Ordering::Less)
}

/// 降りたページと、辿ったダウンリンクの行ポインタ番号。
#[derive(Clone, Copy, Debug)]
pub struct StackEntry {
    pub block: BlockNumber,
    pub offset: u16,
}

#[derive(Clone, Debug)]
pub struct Descent {
    pub leaf: BlockNumber,
    /// 上（ルート）から下の順。
    pub stack: Vec<StackEntry>,
    pub root_level: u32,
}

/// メタを読み、ルートから葉まで降りる（§5.2）。ラッチ・ピンは返さない。返す葉のブロックは「key が属する
/// はずの葉」で、呼び出し側がラッチして `page_covers` で確かめ、外れていれば右へ移る。
pub(crate) fn descend(ctx: &BtCtx<'_>, key: &SearchKey<'_>) -> Result<Descent> {
    let meta = {
        let pin = ctx.pool.read_buffer(ctx.tag(BT_META_BLOCK))?;
        let g = pin.read_tree()?;
        BtMeta::read(ctx, &g)?
    };
    let mut nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    if meta.root >= nblocks {
        return Err(ctx.corrupted(BT_META_BLOCK, "root block is out of range"));
    }
    #[cfg(test)]
    super::testing::fire(super::testing::HookPoint::DescendAfterMeta);

    let mut stack: Vec<StackEntry> = Vec::new();
    let mut blk = meta.root;
    let mut lvl = meta.level;
    loop {
        let mut hops = 0u32;
        // 右へ移動（L&Y）。ラッチは外してから次を取る。
        let (child, off) = loop {
            let pin = ctx.pool.read_buffer(ctx.tag(blk))?;
            let g = pin.read_tree()?;
            let sp = validate_page(ctx, &g, blk, Some(lvl))?;
            if !page_covers(ctx, &g, blk, &sp, key)? {
                blk = sp.next;
                hops += 1;
                if hops > nblocks {
                    // 並行する分割でファイルが伸びたかもしれないので、読み直してから判定する。
                    nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
                    if hops > nblocks {
                        return Err(ctx.corrupted(blk, "sibling chain is cyclic"));
                    }
                }
                continue;
            }
            if lvl == 0 {
                return Ok(Descent {
                    leaf: blk,
                    stack,
                    root_level: meta.level,
                });
            }
            let pos = find_first(ctx, &g, blk, &sp, key, true)?;
            if pos <= sp.first_data_offset() {
                return Err(ctx.corrupted(blk, "no downlink found"));
            }
            let off = pos - 1;
            let bytes = item(ctx, &g, blk, off)?;
            let t = IndexTuple(bytes);
            if !t.is_pivot() {
                return Err(ctx.corrupted(blk, "invalid index tuple: downlink is not a pivot"));
            }
            break (t.downlink(), off);
        };
        stack.push(StackEntry {
            block: blk,
            offset: off,
        });
        if child != 0 && child >= nblocks {
            // 降下の途中で書き手がファイルを伸ばした場合に備えて、読み直す。
            nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
        }
        if child == 0 || child >= nblocks {
            return Err(ctx.corrupted(blk, "downlink is out of range"));
        }
        blk = child;
        lvl -= 1;
        #[cfg(test)]
        super::testing::fire(super::testing::HookPoint::DescendBetweenLevels { level: lvl });
    }
}

/// ブロック `start`（レベル `level`）から、`key` が属するページまで右へ移って、その共有ラッチの下で `f` を
/// 呼ぶ。`f` の戻り値を返す。
pub(crate) fn with_covering_page<R>(
    ctx: &BtCtx<'_>,
    start: BlockNumber,
    level: u32,
    key: &SearchKey<'_>,
    f: &mut dyn FnMut(&Page, BlockNumber, &BtSpecial) -> Result<R>,
) -> Result<R> {
    let mut nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut blk = start;
    let mut hops = 0u32;
    loop {
        let pin = ctx.pool.read_buffer(ctx.tag(blk))?;
        let g = pin.read_tree()?;
        let sp = validate_page(ctx, &g, blk, Some(level))?;
        if page_covers(ctx, &g, blk, &sp, key)? {
            return f(&g, blk, &sp);
        }
        blk = sp.next;
        hops += 1;
        if hops > nblocks {
            nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
            if hops > nblocks {
                return Err(ctx.corrupted(blk, "sibling chain is cyclic"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::meta::BtMeta;
    use crate::storage::btree::testing::{HookPoint, TestIndex, set_test_hook, test_index};
    use crate::storage::btree::tuple::{form_index_tuple, leaf_to_pivot, minus_infinity_pivot};
    use crate::storage::btree::{BTP_LEAF, BTP_ROOT};
    use crate::types::SqlType;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn k(c: &[Datum]) -> SearchKey<'_> {
        SearchKey {
            cols: c,
            tid: None,
            rest: Rest::Low,
        }
    }

    fn tid(n: u16) -> Tid {
        Tid {
            block: 0,
            offset: n,
        }
    }

    fn leaf(t: &TestIndex, k: i32, n: u16) -> Vec<u8> {
        form_index_tuple(&t.handle, &[Datum::Int4(k)], tid(n)).unwrap()
    }

    fn special(prev: u32, next: u32, level: u32, flags: u16) -> [u8; 16] {
        BtSpecial {
            prev,
            next,
            level,
            flags,
            cycleid: 0,
        }
        .to_bytes()
    }

    /// ブロック 0 メタ、1 = 葉 (10,20,30 | high key 40)、2 = 葉 (40,50)、3 = ルート。
    fn two_level_tree() -> TestIndex {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let (l10, l20, l30, l40, l50) = (
            leaf(&t, 10, 1),
            leaf(&t, 20, 2),
            leaf(&t, 30, 3),
            leaf(&t, 40, 4),
            leaf(&t, 50, 5),
        );
        let high = leaf_to_pivot(&l40, 0, 1);
        let p1 = Page::build_with_items(&special(0, 2, 0, BTP_LEAF), &[&high, &l10, &l20, &l30])
            .unwrap();
        let p2 = Page::build_with_items(&special(1, 0, 0, BTP_LEAF), &[&l40, &l50]).unwrap();
        let inf = minus_infinity_pivot(1);
        let sep = leaf_to_pivot(&l40, 2, 1);
        let root = Page::build_with_items(&special(0, 0, 1, BTP_ROOT), &[&inf, &sep]).unwrap();
        let meta = BtMeta { root: 3, level: 1 }.to_page();
        t.install_pages(&[meta, p1, p2, root]);
        t
    }

    fn key(k: i32, t: Option<Tid>, rest: Rest, cols: &[Datum]) -> (Vec<Datum>, Option<Tid>, Rest) {
        let _ = k;
        (cols.to_vec(), t, rest)
    }

    #[test]
    fn cmp_column_matches_cmp_with_nulls() {
        let vals = [Datum::Null, Datum::Int4(1), Datum::Int4(2), Datum::Int4(-5)];
        for desc in [false, true] {
            for nf in [false, true] {
                let t = test_index(&[(SqlType::INT4, desc, nf)], false);
                for a in &vals {
                    for b in &vals {
                        assert_eq!(
                            cmp_column(&t.handle.columns[0], a, b),
                            crate::types::cmp::cmp_with_nulls(a, b, desc, nf),
                            "{a:?} {b:?} desc={desc} nf={nf}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cmp_key_item_rules() {
        let t = test_index(
            &[(SqlType::INT4, false, false), (SqlType::INT4, false, false)],
            false,
        );
        let it = form_index_tuple(&t.handle, &[Datum::Int4(5), Datum::Int4(7)], tid(3)).unwrap();
        let item = IndexTuple(&it);
        let cmp = |cols: &[Datum], tid: Option<Tid>, rest| {
            cmp_key_item(&t.handle, &SearchKey { cols, tid, rest }, &item).unwrap()
        };
        let full = [Datum::Int4(5), Datum::Int4(7)];
        assert_eq!(cmp(&full, Some(tid(3)), Rest::Low), Ordering::Equal);
        assert_eq!(cmp(&full, Some(tid(2)), Rest::Low), Ordering::Less);
        assert_eq!(cmp(&full, Some(tid(4)), Rest::Low), Ordering::Greater);
        assert_eq!(cmp(&full, None, Rest::Low), Ordering::Less);
        assert_eq!(cmp(&full, None, Rest::High), Ordering::Greater);
        let prefix = [Datum::Int4(5)];
        assert_eq!(cmp(&prefix, None, Rest::Low), Ordering::Less);
        assert_eq!(cmp(&prefix, None, Rest::High), Ordering::Greater);
        assert_eq!(cmp(&[Datum::Int4(4)], None, Rest::High), Ordering::Less);
        assert_eq!(cmp(&[Datum::Int4(6)], None, Rest::Low), Ordering::Greater);
        assert_eq!(
            cmp(&[Datum::Int4(5), Datum::Int4(8)], Some(tid(0)), Rest::Low),
            Ordering::Greater
        );
        // -∞ ピボットは常に key より小さい。
        let inf = minus_infinity_pivot(1);
        let o = cmp_key_item(
            &t.handle,
            &SearchKey {
                cols: &[],
                tid: None,
                rest: Rest::Low,
            },
            &IndexTuple(&inf),
        )
        .unwrap();
        assert_eq!(o, Ordering::Greater);
        let _ = key(0, None, Rest::Low, &[]);
    }

    #[test]
    fn cmp_key_item_with_nulls_and_desc() {
        let t = test_index(&[(SqlType::INT4, true, true)], false);
        let null_item = form_index_tuple(&t.handle, &[Datum::Null], tid(1)).unwrap();
        let five = form_index_tuple(&t.handle, &[Datum::Int4(5)], tid(1)).unwrap();
        // NULLS FIRST: NULL が最小。
        assert_eq!(
            cmp_key_item(&t.handle, &k(&[Datum::Int4(9)]), &IndexTuple(&null_item)).unwrap(),
            Ordering::Greater
        );
        // DESC: 9 は 5 より前。
        assert_eq!(
            cmp_key_item(&t.handle, &k(&[Datum::Int4(9)]), &IndexTuple(&five)).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            cmp_key_item(&t.handle, &k(&[Datum::Null]), &IndexTuple(&null_item)).unwrap(),
            Ordering::Less // rest = Low
        );
    }

    #[test]
    fn binary_search_and_covers() {
        let t = two_level_tree();
        let ctx = t.ctx();
        let pin = t.pool().read_buffer(ctx.tag(1)).unwrap();
        let g = pin.read().unwrap();
        let sp = validate_page(&ctx, &g, 1, Some(0)).unwrap();
        let first = |k: i32, strict: bool| {
            let cols = [Datum::Int4(k)];
            find_first(
                &ctx,
                &g,
                1,
                &sp,
                &SearchKey {
                    cols: &cols,
                    tid: None,
                    rest: Rest::Low,
                },
                strict,
            )
            .unwrap()
        };
        // 行ポインタ 1 = high key、2..=4 = 10、20、30。
        assert_eq!(first(5, false), 2);
        assert_eq!(first(10, false), 2);
        assert_eq!(first(15, false), 3);
        assert_eq!(first(30, false), 4);
        assert_eq!(first(31, false), 5);
        // rest = Low のキーはタプルと等しくならないので strict でも同じ。
        assert_eq!(first(20, true), 3);
        let covers = |k: i32| {
            let cols = [Datum::Int4(k)];
            page_covers(
                &ctx,
                &g,
                1,
                &sp,
                &SearchKey {
                    cols: &cols,
                    tid: None,
                    rest: Rest::Low,
                },
            )
            .unwrap()
        };
        assert!(covers(39));
        // (40, -∞) は high key (40, (0,4)) より小さいので左（木の順序）。
        assert!(covers(40));
        assert!(!covers(41));
        let c40 = [Datum::Int4(40)];
        let high = SearchKey {
            cols: &c40,
            tid: None,
            rest: Rest::High,
        };
        assert!(!page_covers(&ctx, &g, 1, &sp, &high).unwrap());
        // 完全なキー (40, (0,4)) は high key と等しい: 右へ。(40, (0,3)) は左。
        let cols = [Datum::Int4(40)];
        let exact = |n| SearchKey {
            cols: &cols,
            tid: Some(tid(n)),
            rest: Rest::Low,
        };
        assert!(!page_covers(&ctx, &g, 1, &sp, &exact(4)).unwrap());
        assert!(page_covers(&ctx, &g, 1, &sp, &exact(3)).unwrap());
    }

    #[test]
    fn descend_finds_the_leaf_and_stack() {
        let t = two_level_tree();
        let ctx = t.ctx();
        let d = |k: i32| {
            let cols = [Datum::Int4(k)];
            descend(
                &ctx,
                &SearchKey {
                    cols: &cols,
                    tid: None,
                    rest: Rest::Low,
                },
            )
            .unwrap()
        };
        let a = d(5);
        assert_eq!((a.leaf, a.root_level), (1, 1));
        assert_eq!(a.stack.len(), 1);
        assert_eq!((a.stack[0].block, a.stack[0].offset), (3, 1));
        let b = d(39);
        assert_eq!((b.leaf, b.stack[0].offset), (1, 1));
        let c = d(40);
        assert_eq!((c.leaf, c.stack[0].offset), (1, 1));
        let c41 = d(41);
        assert_eq!((c41.leaf, c41.stack[0].offset), (2, 2));
        let c40 = [Datum::Int4(40)];
        let e = descend(
            &ctx,
            &SearchKey {
                cols: &c40,
                tid: Some(tid(4)),
                rest: Rest::Low,
            },
        )
        .unwrap();
        assert_eq!(e.leaf, 2);
        assert_eq!(d(1000).leaf, 2);
        t.assert_clean();
    }

    #[test]
    fn descend_moves_right_when_the_parent_is_stale() {
        // ルートの区切りを古くして（子 1 だけを指す）、葉の high key で右へ移る。
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let (l10, l40) = (leaf(&t, 10, 1), leaf(&t, 40, 4));
        let p1 = Page::build_with_items(
            &special(0, 2, 0, BTP_LEAF | BTP_ROOT),
            &[&leaf_to_pivot(&l40, 0, 1), &l10],
        )
        .unwrap();
        let p2 = Page::build_with_items(&special(1, 0, 0, BTP_LEAF), &[&l40]).unwrap();
        t.install_pages(&[BtMeta { root: 1, level: 0 }.to_page(), p1, p2]);
        let ctx = t.ctx();
        let cols = [Datum::Int4(41)];
        let d = descend(
            &ctx,
            &SearchKey {
                cols: &cols,
                tid: None,
                rest: Rest::Low,
            },
        )
        .unwrap();
        assert_eq!(d.leaf, 2);
        let got = with_covering_page(
            &ctx,
            1,
            0,
            &SearchKey {
                cols: &cols,
                tid: None,
                rest: Rest::Low,
            },
            &mut |page, blk, sp| Ok((blk, page.max_offset(), sp.is_rightmost())),
        )
        .unwrap();
        assert_eq!(got, (2, 1, true));
        t.assert_clean();
    }

    #[test]
    fn descend_reports_corruption() {
        let t = two_level_tree();
        let ctx = t.ctx();
        // ブロック 3 のレベルを壊す。
        {
            let pin = t.pool().read_buffer(ctx.tag(3)).unwrap();
            let mut g = pin.write().unwrap();
            let mut sp = BtSpecial::read(g.page()).unwrap();
            sp.level = 2;
            sp.write(g.page_mut());
            let lsn = g.page().lsn();
            g.set_lsn(lsn);
        }
        let cols = [Datum::Int4(1)];
        let e = descend(
            &ctx,
            &SearchKey {
                cols: &cols,
                tid: None,
                rest: Rest::Low,
            },
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::DATA_CORRUPTED);
        t.assert_clean();
    }

    #[test]
    fn descend_survives_file_growth_after_reading_nblocks() {
        use crate::storage::btree::testing::{new_index, put, tid_of};
        let t = Rc::new(new_index(&[(SqlType::INT4, false, false)], false));
        for i in 1..=500 {
            put(&t, &[Datum::Int4(i)], tid_of(i.cast_unsigned())).unwrap();
        }
        let t2 = Rc::clone(&t);
        set_test_hook(Some(Box::new(move |p| {
            if p == HookPoint::DescendAfterMeta {
                for i in 501..=1200 {
                    put(&t2, &[Datum::Int4(i)], tid_of(i.cast_unsigned())).unwrap();
                }
            }
        })));
        let cols = [Datum::Int4(1100)];
        let r = descend(
            &t.ctx(),
            &SearchKey {
                cols: &cols,
                tid: None,
                rest: Rest::Low,
            },
        );
        set_test_hook(None);
        r.expect("descend must not report a false corruption");
    }

    #[test]
    fn descend_hooks_fire_in_order() {
        let t = two_level_tree();
        let ctx = t.ctx();
        let log: Rc<RefCell<Vec<HookPoint>>> = Rc::new(RefCell::new(Vec::new()));
        let l2 = Rc::clone(&log);
        set_test_hook(Some(Box::new(move |p| l2.borrow_mut().push(p))));
        let cols = [Datum::Int4(1)];
        descend(
            &ctx,
            &SearchKey {
                cols: &cols,
                tid: None,
                rest: Rest::Low,
            },
        )
        .unwrap();
        set_test_hook(None);
        assert_eq!(
            *log.borrow(),
            vec![
                HookPoint::DescendAfterMeta,
                HookPoint::DescendBetweenLevels { level: 0 }
            ]
        );
    }
}
