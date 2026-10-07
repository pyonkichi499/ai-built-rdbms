//! 一意性検査（`m4/06-btree.md` §5.4）。
//!
//! 最初の葉（`K_first` が属する葉）の排他ラッチを持ったまま、等値のキーを持つ項目をヒープで
//! `fetch_dirty` して調べる。等値が右の葉へ続くときは右を共有ラッチして読む（左 → 右）。
//! `23505` には `s` / `t` / `n` を付け、DETAIL は付けない（`dml.rs` が補う。11 §7.1 の C-11）。

use std::cmp::Ordering;

use super::page::{BtSpecial, item, validate_page};
use super::search::{Rest, SearchKey, find_first};
use super::tuple::{IndexTuple, decode_key};
use super::{BT_P_HIKEY, BtCtx, cmp_keys};
use crate::error::{Error, Result, sqlstate};
use crate::storage::page::Page;
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::storage::{DirtyResult, RelHandle, TableStore};
use crate::txn::Xid;
use crate::types::Datum;

/// 1 ページを調べた結果。
enum PageResult {
    /// 等値の連続は終わった（または右端）。
    Done,
    /// 等値が右のページへ続く。
    Right(BlockNumber),
}

/// `23505`（`s` / `t` / `n`。DETAIL なし）。
pub(crate) fn unique_violation(ctx: &BtCtx<'_>) -> Error {
    Error::new(
        sqlstate::UNIQUE_VIOLATION,
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            ctx.index.name
        ),
    )
    .with_table(ctx.index.schema.clone(), ctx.index.table_name.clone())
    .with_constraint(ctx.index.name.clone())
}

/// 最初の葉 `first`（呼び出し側が排他ラッチを持っている）から、`key` と等しいキーの項目を順に調べる。
/// ヒープで生きている項目があれば `23505`。`key` は NULL を含まない。
pub(crate) fn check_unique(
    ctx: &BtCtx<'_>,
    first: &Page,
    first_blk: BlockNumber,
    key: &[Datum],
    heap: &dyn TableStore,
    rel: &RelHandle,
    own: Xid,
) -> Result<()> {
    let k = SearchKey {
        cols: key,
        tid: None,
        rest: Rest::Low,
    };
    let sp = BtSpecial::read(first).ok_or_else(|| ctx.corrupted(first_blk, "no special area"))?;
    let mut next = check_page(ctx, first, first_blk, &sp, &k, key, heap, rel, own)?;
    let nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut hops = 0u32;
    while let PageResult::Right(blk) = next {
        hops += 1;
        if hops > nblocks {
            return Err(ctx.corrupted(blk, "sibling chain is cyclic"));
        }
        let pin = ctx.pool.read_buffer(ctx.tag(blk))?;
        let g = pin.read_tree()?;
        let sp = validate_page(ctx, &g, blk, Some(0))?;
        next = check_page(ctx, &g, blk, &sp, &k, key, heap, rel, own)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_page(
    ctx: &BtCtx<'_>,
    page: &Page,
    blk: BlockNumber,
    sp: &BtSpecial,
    k: &SearchKey<'_>,
    key: &[Datum],
    heap: &dyn TableStore,
    rel: &RelHandle,
    own: Xid,
) -> Result<PageResult> {
    let ncols = ctx.index.columns.len();
    let pos = find_first(ctx, page, blk, sp, k, false)?;
    for off in pos..=page.max_offset() {
        let bytes = item(ctx, page, blk, off)?;
        let t = IndexTuple(bytes);
        if t.is_pivot() {
            return Err(ctx.corrupted(blk, "pivot tuple on a leaf page"));
        }
        let item_key = decode_key(ctx.index, &t, ncols)?;
        if cmp_keys(ctx.index, &item_key, key) != Ordering::Equal {
            return Ok(PageResult::Done);
        }
        let tid = t
            .heap_tid()
            .ok_or_else(|| ctx.corrupted(blk, "leaf tuple without a heap TID"))?;
        match heap.fetch_dirty(rel, Some(own), tid)? {
            DirtyResult::Visible => return Err(unique_violation(ctx)),
            DirtyResult::Invisible => {}
            DirtyResult::WaitFor(x) => {
                return Err(Error::internal(format!(
                    "unique check would wait for transaction {}",
                    x.0
                )));
            }
        }
    }
    if sp.is_rightmost() {
        return Ok(PageResult::Done);
    }
    let hk = IndexTuple(item(ctx, page, blk, BT_P_HIKEY)?);
    let hk_key = decode_key(ctx.index, &hk, ncols)?;
    if cmp_keys(ctx.index, &hk_key, key) == Ordering::Greater {
        return Ok(PageResult::Done);
    }
    Ok(PageResult::Right(sp.next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::insert::insert;
    use crate::storage::btree::testing::{FakeHeap, TestIndex, check_tree, new_index, put, tid_of};
    use crate::storage::testing::test_rel;
    use crate::storage::{TupleDesc, UniqueCheck};
    use crate::types::SqlType;
    use std::sync::Arc;

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];

    fn rel() -> RelHandle {
        RelHandle {
            oid: 16384,
            locator: test_rel(16384),
            desc: Arc::new(TupleDesc { attrs: vec![] }),
            indexes: Arc::from(Vec::new()),
        }
    }

    fn ins(t: &TestIndex, heap: &FakeHeap, key: Datum, n: u32) -> Result<()> {
        let r = rel();
        insert(
            &t.ctx(),
            &t.write_ctx(),
            &[key],
            tid_of(n),
            UniqueCheck::Check {
                heap,
                rel: &r,
                own_xid: Xid(3),
            },
        )
    }

    #[test]
    fn visible_duplicate_is_23505_with_s_t_n_and_no_detail() {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new();
        heap.set(tid_of(1), DirtyResult::Visible);
        ins(&t, &heap, Datum::Int4(7), 1).unwrap();
        let before = t.snapshot_pages();
        let e = ins(&t, &heap, Datum::Int4(7), 2).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(
            e.message,
            "duplicate key value violates unique constraint \"t_idx\""
        );
        assert!(e.detail.is_none());
        let d = e.diag.as_deref().unwrap();
        assert_eq!(d.schema.as_deref(), Some("public"));
        assert_eq!(d.table.as_deref(), Some("t"));
        assert_eq!(d.constraint.as_deref(), Some("t_idx"));
        for (a, b) in before.iter().zip(t.snapshot_pages()) {
            assert!(a.0 == b.0, "a failed check must not change the page");
        }
        // 別のキーは入る。
        ins(&t, &heap, Datum::Int4(8), 3).unwrap();
        t.assert_clean();
    }

    #[test]
    fn invisible_duplicates_are_ignored() {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new(); // 表にない TID は Invisible（中断・削除済み）
        ins(&t, &heap, Datum::Int4(7), 1).unwrap();
        ins(&t, &heap, Datum::Int4(7), 2).unwrap();
        ins(&t, &heap, Datum::Int4(7), 3).unwrap();
        assert_eq!(check_tree(&t).entries.len(), 3);
        heap.set(tid_of(2), DirtyResult::Visible);
        assert!(ins(&t, &heap, Datum::Int4(7), 4).is_err());
    }

    #[test]
    fn null_keys_and_skip_are_not_checked() {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new();
        heap.set(tid_of(1), DirtyResult::Visible);
        ins(&t, &heap, Datum::Null, 1).unwrap();
        ins(&t, &heap, Datum::Null, 2).unwrap();
        put(&t, &[Datum::Int4(5)], tid_of(3)).unwrap();
        heap.set(tid_of(3), DirtyResult::Visible);
        put(&t, &[Datum::Int4(5)], tid_of(4)).unwrap(); // Skip
        assert_eq!(check_tree(&t).entries.len(), 4);
    }

    #[test]
    fn non_unique_index_does_not_check() {
        let t = new_index(&INT, false);
        let heap = FakeHeap::new();
        heap.set(tid_of(1), DirtyResult::Visible);
        ins(&t, &heap, Datum::Int4(7), 1).unwrap();
        ins(&t, &heap, Datum::Int4(7), 2).unwrap();
    }

    #[test]
    fn wait_for_is_an_internal_error() {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new();
        ins(&t, &heap, Datum::Int4(7), 1).unwrap();
        heap.set(tid_of(1), DirtyResult::WaitFor(Xid(9)));
        let e = ins(&t, &heap, Datum::Int4(7), 2).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert_eq!(e.message, "unique check would wait for transaction 9");
    }

    /// 等値の連続が複数の葉にまたがるとき、右の葉まで調べる（最初の葉・中間・最後の葉のどこでも検出）。
    #[test]
    fn duplicates_spanning_several_leaves_are_found_on_any_page() {
        let heap = FakeHeap::new();
        let t = new_index(&INT, true);
        let n = 1200u32; // 葉 3〜4 枚
        for i in 0..n {
            ins(&t, &heap, Datum::Int4(5), i).unwrap();
        }
        // 前後に別のキーも置く。
        ins(&t, &heap, Datum::Int4(1), 5000).unwrap();
        ins(&t, &heap, Datum::Int4(9), 5001).unwrap();
        assert!(check_tree(&t).pages_per_level[0] >= 3);
        for visible in [0u32, 400, 800, n - 1] {
            let heap2 = FakeHeap::new();
            heap2.set(tid_of(visible), DirtyResult::Visible);
            let e = ins(&t, &heap2, Datum::Int4(5), 9000).unwrap_err();
            assert_eq!(
                e.sqlstate,
                sqlstate::UNIQUE_VIOLATION,
                "visible entry {visible}"
            );
        }
        // 全部 Invisible なら入り、TID が最大なので最後の葉に入る。
        ins(&t, &heap, Datum::Int4(5), 9001).unwrap();
        check_tree(&t);
        t.assert_clean();
    }
}
