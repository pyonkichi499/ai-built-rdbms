//! 葉への挿入（`BTREE_INSERT_LEAF`。`m4/06-btree.md` §5.3、`m5/02` §6.9）。入らなければ `split::split_and_insert`。
//!
//! 複数のライターが同時に挿入する（M5）。降下とラッチの間に葉が分割されうるので、葉を排他ラッチした後で
//! `page_covers` を確かめ、外れていれば右へ移る（B1。右隣のラッチを取ってから左を放す）。一意検査は最初の葉
//! `P0` の排他ラッチを持ったまま行い、挿入先が右の葉になるときも右隣を先にラッチしてから `P0` を放す（B6）。

use std::cmp::Ordering;

use super::page::{BtSpecial, item, validate_page};
use super::search::{Rest, SearchKey, StackEntry, cmp_key_item, descend, find_first, page_covers};
use super::split::split_and_insert;
use super::tuple::{IndexTuple, form_index_tuple};
use super::unique::{UniqueOutcome, check_unique};
use super::wal::log_insert_leaf;
use super::{BtCtx, INDEX_MAX_KEYS};
use crate::error::{Error, Result};
use crate::storage::buffer::{CriticalSection, PageWriteGuard, PinnedBuffer};
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::storage::{InsertOutcome, RelHandle, TableStore, UniqueCheck, WriteCtx};
use crate::txn::Xid;
use crate::types::{Datum, Tid};

pub(crate) fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// 挿入の間に持ち回る値。
struct Insertion<'a> {
    ctx: &'a BtCtx<'a>,
    w: &'a WriteCtx,
    key: &'a [Datum],
    tuple: Vec<u8>,
    k_first: SearchKey<'a>,
    k_new: SearchKey<'a>,
    stack: Vec<StackEntry>,
    /// 一意検査がまだなら `(heap, rel, own_xid)`。検査が済む（または不要）と `None`。
    checking: Option<(&'a dyn TableStore, &'a RelHandle, Xid)>,
    hops: u32,
    nblocks: u32,
}

/// `IndexStore::insert` の本体。`WaitFor` のときは何も挿入しておらず、ラッチもピンも外れている。
pub(crate) fn insert(
    ctx: &BtCtx<'_>,
    w: &WriteCtx,
    key: &[Datum],
    tid: Tid,
    check: UniqueCheck<'_>,
) -> Result<InsertOutcome> {
    let ncols = ctx.index.columns.len();
    if key.len() != ncols || ncols > INDEX_MAX_KEYS {
        return Err(Error::internal(format!(
            "index \"{}\" has {ncols} key columns but the key has {} values",
            ctx.index.name,
            key.len()
        )));
    }
    // ラッチを取る前。ページに触れない（54000）。
    let tuple = form_index_tuple(ctx.index, key, tid)?;
    let checking = match check {
        UniqueCheck::Check { heap, rel, own_xid }
            if ctx.index.unique && !key.iter().any(Datum::is_null) =>
        {
            Some((heap, rel, own_xid))
        }
        _ => None,
    };
    let k_first = SearchKey {
        cols: key,
        tid: None,
        rest: Rest::Low,
    };
    let k_new = SearchKey {
        cols: key,
        tid: Some(tid),
        rest: Rest::Low,
    };
    let d = descend(ctx, if checking.is_some() { &k_first } else { &k_new })?;
    let nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut st = Insertion {
        ctx,
        w,
        key,
        tuple,
        k_first,
        k_new,
        stack: d.stack,
        checking,
        hops: 0,
        nblocks,
    };
    let pin = ctx.pool.read_buffer(ctx.tag(d.leaf))?;
    let g = pin.write_tree()?;
    insert_at(&mut st, &pin, g, d.leaf)
}

/// 葉 `blk`（排他ラッチ `g`）から挿入を進める。右へ移るときは右隣の排他ラッチを取ってから `g` を放す
/// （カップリング。順序 `O` は左 → 右）。
fn insert_at(
    st: &mut Insertion<'_>,
    pin: &PinnedBuffer,
    g: PageWriteGuard<'_>,
    blk: BlockNumber,
) -> Result<InsertOutcome> {
    let ctx = st.ctx;
    let sp = validate_page(ctx, g.page(), blk, Some(0))?;
    crate::sync_point!("btree.leaf.latched");
    let sk = if st.checking.is_some() {
        st.k_first
    } else {
        st.k_new
    };
    if !page_covers(ctx, g.page(), blk, &sp, &sk)? {
        return move_right(st, g, blk, sp.next);
    }
    if let Some((heap, rel, own)) = st.checking.take() {
        // 最初の葉 `P0` の排他ラッチを持ったまま検査する。
        match check_unique(ctx, g.page(), blk, st.key, heap, rel, own)? {
            UniqueOutcome::Clear => {}
            UniqueOutcome::Wait(x) => return Ok(InsertOutcome::WaitFor(x)),
        }
        crate::sync_point!("btree.unique.checked");
        if !page_covers(ctx, g.page(), blk, &sp, &st.k_new)? {
            // 等値の連続が右の葉へ続き、tid が high key 以上。右隣を先にラッチしてから `P0` を放す。
            return move_right(st, g, blk, sp.next);
        }
    }
    insert_into_leaf(ctx, st.w, pin, g, &st.stack, &st.tuple, &st.k_new, blk)?;
    Ok(InsertOutcome::Inserted)
}

/// `next` の排他ラッチを取り、`g` を放して `insert_at` を続ける。
fn move_right(
    st: &mut Insertion<'_>,
    g: PageWriteGuard<'_>,
    from: BlockNumber,
    next: BlockNumber,
) -> Result<InsertOutcome> {
    let ctx = st.ctx;
    if next == 0 {
        return Err(ctx.corrupted(from, "no right sibling for a key above the high key"));
    }
    st.hops += 1;
    if st.hops > st.nblocks {
        // 並行する分割でファイルが伸びたかもしれないので、読み直してから判定する。
        st.nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
        if st.hops > st.nblocks {
            return Err(ctx.corrupted(next, "sibling chain is cyclic"));
        }
    }
    let npin = ctx.pool.read_buffer(ctx.tag(next))?;
    let ng = npin.write_tree()?;
    drop(g);
    insert_at(st, &npin, ng, next)
}

/// 葉 `g`（排他ラッチ）に `tuple` を入れる。入らなければ分割する（§5.5）。
#[allow(clippy::too_many_arguments)]
fn insert_into_leaf(
    ctx: &BtCtx<'_>,
    w: &WriteCtx,
    pin: &PinnedBuffer,
    mut g: PageWriteGuard<'_>,
    stack: &[StackEntry],
    tuple: &[u8],
    k_new: &SearchKey<'_>,
    leaf: BlockNumber,
) -> Result<()> {
    let sp = BtSpecial::read(g.page()).ok_or_else(|| ctx.corrupted(leaf, "no special area"))?;
    let pos = find_first(ctx, g.page(), leaf, &sp, k_new, false)?;
    if pos <= g.page().max_offset() {
        let bytes = item(ctx, g.page(), leaf, pos)?;
        if cmp_key_item(ctx.index, k_new, &IndexTuple(bytes))? == Ordering::Equal {
            return Err(Error::internal("duplicate index entry"));
        }
    }
    if g.page().free_space_unbounded() >= maxalign(tuple.len()) {
        let cs = CriticalSection::enter(ctx.pool);
        if g.page_mut().insert_item_at(pos, tuple).is_none() {
            // page_mut の後は set_lsn が必要（ガードの検査）。エラーは Panic になる。
            let lsn = g.page().lsn();
            g.set_lsn(lsn);
            return Err(cs.escalate(Error::internal("btree leaf has no room for the tuple")));
        }
        log_insert_leaf(ctx.wal, w, pin.tag(), &mut g, pos, tuple).map_err(|e| cs.escalate(e))?;
        return Ok(());
    }
    split_and_insert(ctx, w, pin, g, stack, tuple, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::sqlstate;
    use crate::storage::btree::testing::{
        HookPoint, TestIndex, check_tree, new_index, put, set_test_hook, tid_of,
    };
    use crate::storage::btree::wal::BTREE_INSERT_LEAF;
    use crate::types::SqlType;

    use std::cell::RefCell;
    use std::rc::Rc;

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];

    fn text(n: usize) -> Datum {
        Datum::Text("a".repeat(n))
    }

    fn nblocks(t: &TestIndex) -> u32 {
        t.pool().nblocks(t.locator(), ForkNumber::Main).unwrap()
    }

    /// 境界の大きさは入り、1 つ大きいと `54000`（06 §7.2。実測の PostgreSQL 17 と同じ値）。
    fn boundary(cols: &[(SqlType, bool, bool)], key: impl Fn(usize) -> Vec<Datum>, ok: usize) {
        let t = new_index(cols, false);
        put(&t, &key(ok), tid_of(1)).unwrap();
        let before = t.snapshot_pages();
        let n0 = nblocks(&t);
        let e = put(&t, &key(ok + 1), tid_of(2)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            e.message
                .starts_with("index row size 2712 exceeds btree version 4 maximum 2704")
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Index row references tuple (0,3) in relation \"t\".")
        );
        let diag = e.diag.as_deref().unwrap();
        assert_eq!(diag.schema.as_deref(), Some("public"));
        assert_eq!(diag.table.as_deref(), Some("t"));
        assert_eq!(diag.constraint.as_deref(), Some("t_idx"));
        assert_eq!(nblocks(&t), n0);
        for (a, b) in before.iter().zip(t.snapshot_pages()) {
            assert_eq!(a.0, b.0);
        }
        check_tree(&t);
        t.assert_clean();
    }

    #[test]
    fn row_size_limit_single_text() {
        boundary(&[(SqlType::TEXT, false, false)], |n| vec![text(n)], 2692);
    }

    #[test]
    fn row_size_limit_int_and_text() {
        boundary(
            &[(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            |n| vec![Datum::Int4(1), text(n)],
            2688,
        );
    }

    #[test]
    fn row_size_limit_null_and_text() {
        boundary(
            &[(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
            |n| vec![Datum::Null, text(n)],
            2684,
        );
    }

    #[test]
    fn boundary_size_keys_in_random_ascending_and_descending_order() {
        for order in 0..3 {
            let t = new_index(&[(SqlType::TEXT, false, false)], false);
            let n = if order == 2 { 80 } else { 200 }; // 切り詰めのないピボットは 2.7KB で、ランダム順は木が高くなる
            let mut perm: Vec<usize> = (0..n).collect();
            match order {
                1 => perm.reverse(),
                2 => perm = crate::storage::btree::testing::Lcg(3).permutation(n),
                _ => {}
            }
            for (i, p) in perm.iter().enumerate() {
                let mut s = format!("{p:05}");
                s.push_str(&"b".repeat(2692 - 5));
                put(&t, &[Datum::Text(s)], tid_of(u32::try_from(i).unwrap())).unwrap();
            }
            let info = check_tree(&t);
            assert_eq!(info.entries.len(), n);
            assert!(info.height >= 4, "height {}", info.height);
            t.assert_clean();
        }
    }

    #[test]
    fn insert_leaf_record_matches_example_7() {
        let t = new_index(&INT, false);
        for (i, k) in [10, 20, 30].into_iter().enumerate() {
            put(
                &t,
                &[Datum::Int4(k)],
                crate::types::Tid {
                    block: 0,
                    offset: u16::try_from(i + 1).unwrap(),
                },
            )
            .unwrap();
        }
        let before = t.wal().insert_lsn();
        put(
            &t,
            &[Datum::Int4(40)],
            crate::types::Tid {
                block: 0,
                offset: 4,
            },
        )
        .unwrap();
        let recs = t.read_wal_from(before);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.info, BTREE_INSERT_LEAF);
        assert_eq!(r.main, vec![4, 0, 0, 0]);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].tag.block, 1);
        assert_eq!(r.blocks[0].data.len(), 16);
        assert_eq!(r.end.0 - r.start.0, 80, "align8(tot_len = 76)");
        assert!(r.blocks[0].image.is_none());
        assert_eq!(
            crate::storage::btree::wal::describe(r),
            "BTREE_INSERT_LEAF rel 1663/5/16390 blk 1 off 4 size 16"
        );
    }

    #[test]
    fn duplicate_key_and_tid_is_an_internal_error_and_leaves_the_page_alone() {
        let t = new_index(&INT, false);
        put(&t, &[Datum::Int4(1)], tid_of(1)).unwrap();
        let before = t.snapshot_pages();
        let e = put(&t, &[Datum::Int4(1)], tid_of(1)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        for (a, b) in before.iter().zip(t.snapshot_pages()) {
            assert_eq!(a.0, b.0);
        }
        // 同じキーで TID が違えば入る（TID の昇順に並ぶ）。
        put(&t, &[Datum::Int4(1)], tid_of(0)).unwrap();
        assert_eq!(check_tree(&t).entries.len(), 2);
    }

    #[test]
    fn wrong_key_length_is_an_internal_error() {
        let t = new_index(&INT, false);
        let e = put(&t, &[Datum::Int4(1), Datum::Int4(2)], tid_of(1)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    // ----- 並行する分割の一部（§7.8。単一スレッドで手順を刻む） -----

    fn fill(t: &TestIndex, upto: i32) {
        for i in 1..=upto {
            put(t, &[Datum::Int4(i * 2)], tid_of(u32::try_from(i).unwrap())).unwrap();
        }
    }

    #[test]
    fn insert_follows_the_right_link_when_its_leaf_splits_after_the_descent() {
        let t = Rc::new(new_index(&INT, false));
        // 2 枚の葉（左 365 件）にしてから、左の葉をほぼ満杯にして、降下の後で左の葉を分割する。
        fill(&t, 408);
        for i in 0..39 {
            put(
                &t,
                &[Datum::Int4(2 * i + 1)],
                tid_of(5000 + u32::try_from(i).unwrap()),
            )
            .unwrap();
        }
        let fired = Rc::new(RefCell::new(0));
        let (t2, f2) = (Rc::clone(&t), Rc::clone(&fired));
        set_test_hook(Some(Box::new(move |p| {
            if p == (HookPoint::DescendBetweenLevels { level: 0 }) {
                *f2.borrow_mut() += 1;
                // 目的の葉（左の葉）を分割する挿入をフックの中で行う。
                for i in 0..60 {
                    put(
                        &t2,
                        &[Datum::Int4(2 * i + 41)],
                        tid_of(6000 + u32::try_from(i).unwrap()),
                    )
                    .unwrap();
                }
            }
        })));
        put(&t, &[Datum::Int4(700)], tid_of(9000)).unwrap();
        set_test_hook(None);
        assert_eq!(*fired.borrow(), 1);
        let info = check_tree(&t);
        assert_eq!(info.entries.len(), 408 + 39 + 60 + 1);
        assert!(
            info.entries
                .iter()
                .any(|(k, tid)| k[0] == Datum::Int4(700) && *tid == tid_of(9000))
        );
        t.assert_clean();
    }
}

#[cfg(test)]
mod prop {
    //! 性質テスト（06 §7.3 のうち挿入。モデルの順序はテストの中で独立に書く）。

    use super::*;
    use crate::storage::btree::testing::{check_tree, new_index, put, tid_of};
    use crate::types::SqlType;
    use proptest::prelude::*;
    use std::cmp::Ordering;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum V {
        I(i32),
        S(String),
    }

    type ModelKey = Vec<Option<V>>;

    /// 列ごとの (DESC, NULLS FIRST)。
    fn cmp_model(spec: &[(bool, bool)], a: &ModelKey, b: &ModelKey) -> Ordering {
        for ((&(desc, nulls_first), x), y) in spec.iter().zip(a).zip(b) {
            let o = match (x, y) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (Some(V::I(p)), Some(V::I(q))) => {
                    let o = p.cmp(q);
                    if desc { o.reverse() } else { o }
                }
                (Some(V::S(p)), Some(V::S(q))) => {
                    let o = p.cmp(q);
                    if desc { o.reverse() } else { o }
                }
                _ => unreachable!("mixed column types"),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }

    #[allow(clippy::ref_option)]
    fn datum(v: &Option<V>) -> Datum {
        match v {
            None => Datum::Null,
            Some(V::I(i)) => Datum::Int4(*i),
            Some(V::S(s)) => Datum::Text(s.clone()),
        }
    }

    /// (列の定義, 生の値から `ModelKey` を作る関数)。
    #[allow(clippy::type_complexity)]
    fn schema(
        i: usize,
    ) -> (
        Vec<(SqlType, bool, bool)>,
        fn(&Option<i32>, &Option<String>) -> ModelKey,
    ) {
        match i {
            0 => (vec![(SqlType::INT4, false, false)], |a, _| {
                vec![Some(V::I(a.unwrap_or(0)))]
            }),
            1 => (vec![(SqlType::TEXT, true, false)], |_, s| {
                vec![s.clone().map(V::S)]
            }),
            2 => (
                vec![(SqlType::INT4, false, false), (SqlType::TEXT, false, false)],
                |a, s| vec![a.map(V::I), s.clone().map(V::S)],
            ),
            3 => (vec![(SqlType::INT4, true, true)], |a, _| vec![a.map(V::I)]),
            _ => (
                vec![(SqlType::INT4, false, false), (SqlType::INT4, false, false)],
                |a, _| vec![Some(V::I(a.unwrap_or(0) % 3)), Some(V::I(a.unwrap_or(0)))],
            ),
        }
    }

    fn run(i: usize, raw: &[(Option<i32>, Option<String>)], pad: usize, mode: u8) {
        let (cols, mk) = schema(i);
        let spec: Vec<(bool, bool)> = cols.iter().map(|c| (c.1, c.2)).collect();
        let t = new_index(&cols, false);
        let padding = "z".repeat(pad);
        let mut model: Vec<(ModelKey, Tid)> = Vec::new();
        let mut order: Vec<usize> = (0..raw.len()).collect();
        match mode {
            1 => order.reverse(),
            2 => order.sort_by_key(|&j| raw[j].0.unwrap_or(0)),
            _ => {}
        }
        for (n, &j) in order.iter().enumerate() {
            let (a, s) = &raw[j];
            let s = s.as_ref().map(|s| format!("{s}{padding}"));
            let key = mk(a, &s);
            let tid = tid_of(u32::try_from(n).unwrap());
            let d: Vec<Datum> = key.iter().map(datum).collect();
            put(&t, &d, tid).unwrap();
            model.push((key, tid));
        }
        model.sort_by(|x, y| {
            cmp_model(&spec, &x.0, &y.0).then((x.1.block, x.1.offset).cmp(&(y.1.block, y.1.offset)))
        });
        let info = check_tree(&t);
        assert_eq!(info.entries.len(), model.len());
        for ((k, tid), (mk, mt)) in info.entries.iter().zip(&model) {
            let want: Vec<Datum> = mk.iter().map(datum).collect();
            assert!(k == &want && tid == mt, "entry differs: {k:?} vs {want:?}");
        }
        t.assert_clean();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn inserts_match_the_model(
            i in 0usize..5,
            raw in proptest::collection::vec(
                (proptest::option::of(0i32..40), proptest::option::of("[a-d]{0,3}")),
                0..500,
            ),
            pad in prop_oneof![Just(0usize), Just(300usize), Just(900usize)],
            mode in 0u8..3,
        ) {
            run(i, &raw, pad, mode);
        }
    }
}

#[cfg(test)]
mod concurrent {
    //! 複数ライターの試験（`m5/02` §7.5。B1〜B4・B6・B3）。止める場所は `util::sync_point` の同期点。

    use super::*;
    use crate::storage::btree::page::BtSpecial;
    use crate::storage::btree::testing::{
        FakeHeap, Lcg, TestIndex, check_tree, new_index, new_index_with, put, tid_of, tree_level,
    };
    use crate::storage::testing::{TestStorage, TestStorageOptions, test_rel};
    use crate::storage::{DirtyResult, TupleDesc};
    use crate::types::SqlType;
    use crate::util::sync_point::Scenario;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Duration;

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];
    const TEXT: [(SqlType, bool, bool); 1] = [(SqlType::TEXT, false, false)];

    fn int(k: i32) -> Vec<Datum> {
        vec![Datum::Int4(k)]
    }

    /// 約 1KB のキー（葉に 7 件ほどしか入らず、少ない件数で木が高くなる）。辞書順は `n` の順。
    fn wide(n: u32) -> Vec<Datum> {
        vec![Datum::Text(format!("{n:08}{}", "k".repeat(1000)))]
    }

    fn rel() -> RelHandle {
        RelHandle {
            oid: 16384,
            locator: test_rel(16384),
            desc: Arc::new(TupleDesc { attrs: vec![] }),
            indexes: Arc::from(Vec::new()),
            key_columns: Arc::from(Vec::new()),
            ri: Arc::default(),
        }
    }

    fn check_with<'a>(h: &'a FakeHeap, r: &'a RelHandle) -> UniqueCheck<'a> {
        UniqueCheck::Check {
            heap: h,
            rel: r,
            own_xid: Xid(3),
        }
    }

    /// `wide(n)` の `n`。
    fn key_no(k: &[Datum]) -> u32 {
        match &k[0] {
            Datum::Text(s) => s[..8].parse().unwrap(),
            other => panic!("unexpected key {other:?}"),
        }
    }

    /// ブロック `blk` が、`need` バイトの項目を入れられないほど詰まっているか。
    fn is_full(t: &TestIndex, blk: u32, need: usize) -> bool {
        let pages = t.snapshot_pages();
        pages[blk as usize].free_space_unbounded() < maxalign(need)
    }

    /// 最も右の葉のブロック番号。
    fn rightmost_leaf(t: &TestIndex) -> u32 {
        let pages = t.snapshot_pages();
        let (blk, _) = pages
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, p)| BtSpecial::read(p).is_some_and(|sp| sp.is_leaf() && sp.next == 0))
            .expect("a rightmost leaf");
        u32::try_from(blk).unwrap()
    }

    /// 別スレッドで `f` を走らせ、`secs` 秒以内に終わらなければ失敗にする（デッドロックの検出）。
    fn run_with_watchdog<T: Send + 'static>(
        secs: u64,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs))
            .expect("the threads did not finish in time (deadlock?)")
    }

    /// B1: 降下の後、葉をラッチする前に別のライターがその葉を分割する。A は右へ移って正しいページに入れる。
    #[test]
    fn b1_insert_moves_right_when_the_leaf_splits_after_the_descent() {
        let t = new_index(&INT, false);
        for i in 1..=408 {
            put(&t, &int(i * 2), tid_of(u32::try_from(i).unwrap())).unwrap();
        }
        for i in 0..39 {
            put(
                &t,
                &int(2 * i + 1),
                tid_of(5000 + u32::try_from(i).unwrap()),
            )
            .unwrap();
        }
        let sc = Scenario::new();
        // 1 回目 = メタを放した後、2 回目 = ルートを放して葉を取る前。
        let gate = sc.block_at_nth("btree.descend.parent_released", 2);
        std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                put(&t, &int(700), tid_of(9000)).unwrap();
            });
            gate.wait_arrived();
            for i in 0..60 {
                put(
                    &t,
                    &int(2 * i + 41),
                    tid_of(6000 + u32::try_from(i).unwrap()),
                )
                .unwrap();
            }
            gate.release();
            a.join().unwrap();
        });
        let info = check_tree(&t);
        assert_eq!(info.entries.len(), 408 + 39 + 60 + 1);
        assert!(
            info.entries
                .iter()
                .any(|(k, tid)| k[0] == Datum::Int4(700) && *tid == tid_of(9000))
        );
        t.assert_clean();
    }

    /// 葉の排他ラッチを持つ A が止まっている間、同じ葉への挿入は待たされる。放すと両方入る。
    #[test]
    fn a_writer_waits_for_the_leaf_latch_held_by_another() {
        let t = new_index(&INT, false);
        put(&t, &int(10), tid_of(1)).unwrap();
        let sc = Scenario::new();
        let gate = sc.block_at("btree.leaf.latched");
        let b_done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                put(&t, &int(20), tid_of(2)).unwrap();
            });
            gate.wait_arrived();
            let b = s.spawn(|| {
                put(&t, &int(30), tid_of(3)).unwrap();
                b_done.store(true, AtomicOrdering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(150));
            assert!(
                !b_done.load(AtomicOrdering::SeqCst),
                "B must wait for A's latch"
            );
            gate.release();
            a.join().unwrap();
            b.join().unwrap();
        });
        assert_eq!(check_tree(&t).entries.len(), 3);
        t.assert_clean();
    }

    /// 2 段の木（ルート + 4 枚以上の葉）。最も右の葉を満杯にして、次の追記が分割になる状態で返す。
    /// 左端の葉の分割が右端の葉（A が持つ）の隣のラッチを要らないよう、葉は 4 枚以上にする。
    fn two_level_tree_with_full_rightmost_leaf() -> (TestIndex, u32) {
        let t = new_index(&TEXT, false);
        let mut n = 1_000_000u32;
        loop {
            if tree_level(&t) == 1
                && check_tree(&t).pages_per_level[0] >= 4
                && is_full(&t, rightmost_leaf(&t), 1012)
            {
                return (t, n);
            }
            put(&t, &wide(n), tid_of(n)).unwrap();
            n += 1;
            assert!(n < 1_000_100, "could not build the tree");
        }
    }

    /// B2・B4: A が葉を分割して親を探す前に、B が左端へ挿入を続けてルートを分割する。A のスタックの親は
    /// もうルートではなく、ダウンリンクは右へ移っている。`find_parent` が右へたどって見つける。
    #[test]
    fn b2_b4_parent_and_root_split_between_the_descent_and_the_parent_lookup() {
        let (t, mut n) = two_level_tree_with_full_rightmost_leaf();
        let level0 = tree_level(&t);
        let a_key = n;
        let sc = Scenario::new();
        let gate = sc.block_at("btree.split.before_parent_latch");
        std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                put(&t, &wide(a_key), tid_of(a_key)).unwrap();
            });
            gate.wait_arrived();
            // 左端へ（A の葉には触れない）、ルートが分割されるまで挿入する。
            let mut low = 900_000u32;
            while tree_level(&t) == level0 {
                put(&t, &wide(low), tid_of(low)).unwrap();
                low -= 1;
                assert!(low > 899_000);
            }
            gate.release();
            a.join().unwrap();
        });
        n += 1;
        let _ = n;
        assert!(tree_level(&t) > level0);
        let info = check_tree(&t);
        assert!(
            info.entries
                .iter()
                .any(|(k, tid)| k == &wide(a_key) && *tid == tid_of(a_key)),
            "A's entry must be in the tree"
        );
        t.assert_clean();
    }

    /// B4（スタックが尽きる）: A はルートが葉の木を降りた（スタックは空）。そのあと B がルートを分割し、A の
    /// 行き先の葉は詰まっている。A の分割は親を最も左のページから探す。
    #[test]
    fn b4_the_root_leaf_is_split_after_the_descent_and_the_stack_is_empty() {
        let t = new_index(&TEXT, false);
        let sc = Scenario::new();
        for i in 0..3u32 {
            put(&t, &wide(100 + i), tid_of(100 + i)).unwrap();
        }
        assert_eq!(tree_level(&t), 0);
        let gate = sc.block_at("btree.descend.parent_released");
        let a_key = 150u32;
        std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                put(&t, &wide(a_key), tid_of(a_key)).unwrap();
            });
            gate.wait_arrived();
            // ルート（葉）を分割させる。
            let mut k = 200u32;
            while tree_level(&t) == 0 {
                put(&t, &wide(k), tid_of(k)).unwrap();
                k += 1;
            }
            // A の行き先（左の葉。キー 150 は左の葉の範囲）を詰める。次の挿入で分割になる手前まで。
            let mut j = 0u32;
            while !is_full(&t, 1, 1012) {
                put(&t, &wide(110 + (j % 30)), tid_of(300 + j)).unwrap();
                j += 1;
                assert!(j < 100);
            }
            gate.release();
            a.join().unwrap();
        });
        assert!(tree_level(&t) >= 1);
        let info = check_tree(&t);
        assert!(
            info.entries
                .iter()
                .any(|(k, tid)| k == &wide(a_key) && *tid == tid_of(a_key))
        );
        t.assert_clean();
    }

    /// B6: A が一意検査を終えて（`P0` の排他ラッチを持ったまま）止まっている間、同じキーを入れる B は待たされる。
    /// A の項目が生きていれば B は 23505、中断していれば B が入る。
    fn same_key_race(a_visible: bool) {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new();
        heap.set(
            tid_of(1),
            if a_visible {
                DirtyResult::Visible
            } else {
                DirtyResult::Invisible
            },
        );
        let r = rel();
        let check = || UniqueCheck::Check {
            heap: &heap,
            rel: &r,
            own_xid: Xid(3),
        };
        let sc = Scenario::new();
        let gate = sc.block_at("btree.unique.checked");
        let b_done = AtomicBool::new(false);
        let b_result = std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                insert(&t.ctx(), &t.write_ctx(), &int(7), tid_of(1), check()).unwrap()
            });
            gate.wait_arrived();
            let b = s.spawn(|| {
                let res = insert(&t.ctx(), &t.write_ctx(), &int(7), tid_of(2), check());
                b_done.store(true, AtomicOrdering::SeqCst);
                res
            });
            std::thread::sleep(Duration::from_millis(150));
            assert!(
                !b_done.load(AtomicOrdering::SeqCst),
                "B must wait at P0 until A has inserted"
            );
            gate.release();
            assert_eq!(a.join().unwrap(), InsertOutcome::Inserted);
            b.join().unwrap()
        });
        if a_visible {
            assert_eq!(
                b_result.unwrap_err().sqlstate,
                crate::error::sqlstate::UNIQUE_VIOLATION
            );
            assert_eq!(check_tree(&t).entries.len(), 1);
        } else {
            assert_eq!(b_result.unwrap(), InsertOutcome::Inserted);
            assert_eq!(check_tree(&t).entries.len(), 2);
        }
        t.assert_clean();
    }

    #[test]
    fn b6_same_key_waits_at_p0_and_then_fails_when_the_first_is_alive() {
        same_key_race(true);
    }

    #[test]
    fn b6_same_key_waits_at_p0_and_then_inserts_when_the_first_aborted() {
        same_key_race(false);
    }

    /// B6: 等値のキーが複数の葉にまたがる。A は最後の葉へ入る（右へ移動するときにカップリングする）。
    /// その間に始めた B は最初の葉で待たされ、A の項目を見つけて 23505 になる。
    #[test]
    fn b6_equal_keys_spanning_leaves_are_still_seen_by_the_second_writer() {
        let t = new_index(&INT, true);
        let heap = FakeHeap::new(); // 既存の項目はすべて Invisible
        let r = rel();
        for i in 0..1200u32 {
            let o = insert(
                &t.ctx(),
                &t.write_ctx(),
                &int(5),
                tid_of(i),
                check_with(&heap, &r),
            )
            .unwrap();
            assert_eq!(o, InsertOutcome::Inserted);
        }
        assert!(check_tree(&t).pages_per_level[0] >= 3);
        heap.set(tid_of(9000), DirtyResult::Visible);
        let sc = Scenario::new();
        let gate = sc.block_at("btree.unique.checked");
        let b_done = AtomicBool::new(false);
        let res = std::thread::scope(|s| {
            let a = s.spawn(|| {
                let _at = sc.attach();
                insert(
                    &t.ctx(),
                    &t.write_ctx(),
                    &int(5),
                    tid_of(9000),
                    check_with(&heap, &r),
                )
                .unwrap()
            });
            gate.wait_arrived();
            let b = s.spawn(|| {
                let res = insert(
                    &t.ctx(),
                    &t.write_ctx(),
                    &int(5),
                    tid_of(9001),
                    check_with(&heap, &r),
                );
                b_done.store(true, AtomicOrdering::SeqCst);
                res
            });
            std::thread::sleep(Duration::from_millis(150));
            assert!(!b_done.load(AtomicOrdering::SeqCst));
            gate.release();
            assert_eq!(a.join().unwrap(), InsertOutcome::Inserted);
            b.join().unwrap()
        });
        assert_eq!(
            res.unwrap_err().sqlstate,
            crate::error::sqlstate::UNIQUE_VIOLATION
        );
        assert_eq!(check_tree(&t).entries.len(), 1201);
        t.assert_clean();
    }

    /// B3・モデル: 幅の広いキーで分割を頻発させ、複数のスレッドがランダムなキーを同時に挿入する。
    /// デッドロックせず、検査器が通り、挿入した (キー, TID) の集合が木の中身と一致する。
    #[test]
    fn b3_many_writers_do_not_deadlock_and_the_tree_stays_valid() {
        const THREADS: u32 = 8;
        const PER_THREAD: u32 = 150;
        let ts = TestStorage::with_options(TestStorageOptions {
            nframes: 512,
            ..TestStorageOptions::default()
        })
        .unwrap();
        let t = Arc::new(new_index_with(ts, &TEXT, false));
        let t2 = Arc::clone(&t);
        run_with_watchdog(120, move || {
            std::thread::scope(|s| {
                for th in 0..THREADS {
                    let t = &t2;
                    s.spawn(move || {
                        let mut rng = Lcg(u64::from(th) * 7919 + 1);
                        for i in 0..PER_THREAD {
                            // 重複するキーも混ぜる（TID で区別される）。
                            let k = u32::try_from(rng.below(400)).unwrap();
                            put(t, &wide(k), tid_of(th * 10_000 + i)).unwrap();
                        }
                    });
                }
            });
        });
        let info = check_tree(&t);
        let mut model = BTreeSet::new();
        for th in 0..THREADS {
            let mut rng = Lcg(u64::from(th) * 7919 + 1);
            for i in 0..PER_THREAD {
                let k = u32::try_from(rng.below(400)).unwrap();
                let tid = tid_of(th * 10_000 + i);
                model.insert((k, tid.block, tid.offset));
            }
        }
        assert_eq!(info.entries.len(), model.len());
        let got: BTreeSet<_> = info
            .entries
            .iter()
            .map(|(k, tid)| (key_no(k), tid.block, tid.offset))
            .collect();
        assert_eq!(
            got, model,
            "the tree must hold exactly the inserted entries"
        );
        assert!(info.height >= 3, "height {}", info.height);
        t.assert_clean();
    }

    /// B3: 昇順の追記と左端への挿入が同時に走る（分割が木の両端で連鎖する）。
    #[test]
    fn b3_appenders_and_prependers_do_not_deadlock() {
        let ts = TestStorage::with_options(TestStorageOptions {
            nframes: 512,
            ..TestStorageOptions::default()
        })
        .unwrap();
        let t = Arc::new(new_index_with(ts, &TEXT, false));
        let t2 = Arc::clone(&t);
        run_with_watchdog(120, move || {
            std::thread::scope(|s| {
                for th in 0..4u32 {
                    let t = &t2;
                    s.spawn(move || {
                        for i in 0..120u32 {
                            let k = if th % 2 == 0 {
                                500_000 + th * 1000 + i
                            } else {
                                500_000 - th * 1000 - i
                            };
                            put(t, &wide(k), tid_of(th * 10_000 + i)).unwrap();
                        }
                    });
                }
            });
        });
        assert_eq!(check_tree(&t).entries.len(), 480);
        t.assert_clean();
    }
}
