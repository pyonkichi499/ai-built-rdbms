//! 葉への挿入（`BTREE_INSERT_LEAF`。`m4/06-btree.md` §5.3）。入らなければ `split::split_and_insert`。

use std::cmp::Ordering;

use super::page::{BtSpecial, item, validate_page};
use super::search::{Rest, SearchKey, StackEntry, cmp_key_item, descend, find_first, page_covers};
use super::split::split_and_insert;
use super::tuple::{IndexTuple, form_index_tuple};
use super::unique::check_unique;
use super::wal::log_insert_leaf;
use super::{BtCtx, INDEX_MAX_KEYS};
use crate::error::{Error, Result};
use crate::storage::buffer::{CriticalSection, PageWriteGuard, PinnedBuffer};
use crate::storage::smgr::{BlockNumber, ForkNumber};
use crate::storage::{UniqueCheck, WriteCtx};
use crate::types::{Datum, Tid};

pub(crate) fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// `IndexStore::insert` の本体。
pub(crate) fn insert(
    ctx: &BtCtx<'_>,
    w: &WriteCtx,
    key: &[Datum],
    tid: Tid,
    check: UniqueCheck<'_>,
) -> Result<()> {
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
    let unique_check = match check {
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
    let d = descend(
        ctx,
        if unique_check.is_some() {
            &k_first
        } else {
            &k_new
        },
    )?;

    let nblocks = ctx.pool.nblocks(ctx.index.locator, ForkNumber::Main)?;
    let mut leaf: BlockNumber = d.leaf;
    let mut checking = unique_check;
    let mut hops = 0u32;
    loop {
        let pin = ctx.pool.read_buffer(ctx.tag(leaf))?;
        let g = pin.write_tree()?;
        let sp = validate_page(ctx, g.page(), leaf, Some(0))?;
        let sk = if checking.is_some() { &k_first } else { &k_new };
        if !page_covers(ctx, g.page(), leaf, &sp, sk)? {
            leaf = sp.next;
            hops += 1;
            if hops > nblocks {
                return Err(ctx.corrupted(leaf, "sibling chain is cyclic"));
            }
            continue;
        }
        if let Some((heap, rel, own)) = checking {
            // 最初の葉の排他ラッチを持ったまま検査する。
            check_unique(ctx, g.page(), leaf, key, heap, rel, own)?;
            checking = None;
            if !page_covers(ctx, g.page(), leaf, &sp, &k_new)? {
                // 等値の連続が右の葉へ続き、tid が high key 以上。右へ移って挿入先を決める。
                leaf = sp.next;
                continue;
            }
        }
        return insert_into_leaf(ctx, w, &pin, g, &d.stack, &tuple, &k_new, leaf);
    }
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
