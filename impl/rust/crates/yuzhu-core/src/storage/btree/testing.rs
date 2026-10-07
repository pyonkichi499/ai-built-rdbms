//! B+Tree の単体テストの足場（`#[cfg(test)]`。`m4/06-btree.md` §7.8、§8）。
//!
//! - `test_index`: 列の型・DESC・NULLS FIRST・`unique` から `cmp = cmp_datum` の `IndexHandle` を作る
//!   （`catalog::opclass` に依存しない）。
//! - `TestIndex`: `TestStorage` の上に索引のファイルを作った束。
//! - `FakeHeap`: `fetch_dirty` の結果を表で与える偽のヒープ。
//! - 割り込みフック: 降下・スキャンの「ラッチを外した隙間」でテストの関数を実行する。

#![allow(unreachable_pub)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;

use super::BtCtx;
use crate::error::{Error, Result};
use crate::storage::HeapScan;
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::{BlockNumber, RelFileLocator};
use crate::storage::testing::{TestStorage, test_rel};
use crate::storage::vfs::Vfs;
use crate::storage::{
    DirtyResult, HeapTuple, IndexHandle, IndexKeyColumn, RelHandle, TableStore, TmResult,
    UpdateOutcome, WriteCtx,
};
use crate::txn::{Snapshot, Xid};
use crate::types::{Datum, SqlType, Tid, cmp_datum};
use crate::wal::redo::InvalidPages;
use crate::wal::{DecodedRecord, Lsn, RedoCtx, Wal, WalReader};

/// 索引のリレーション番号（`test_index` が使う）。
pub(crate) const TEST_INDEX_REL: u32 = 16390;

/// `TestStorage` と、その上に作った索引のハンドル。
pub(crate) struct TestIndex {
    pub ts: TestStorage,
    pub handle: IndexHandle,
    /// 索引のファイルを作った直後の WAL の挿入位置（`read_wal_from` の起点）。
    pub start: Lsn,
}

impl std::fmt::Debug for TestIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestIndex")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// `(型, DESC, NULLS FIRST)` の並びから索引ハンドルを作る。表名は `t`、索引名は `t_idx`。
pub(crate) fn index_handle(cols: &[(SqlType, bool, bool)], unique: bool) -> IndexHandle {
    let columns: Vec<IndexKeyColumn> = cols
        .iter()
        .enumerate()
        .map(|(i, &(ty, descending, nulls_first))| IndexKeyColumn {
            attnum: i16::try_from(i + 1).unwrap_or(i16::MAX),
            name: format!("c{}", i + 1),
            ty,
            attr: crate::storage::AttrDesc::from_type(ty.oid),
            cmp: cmp_datum,
            descending,
            nulls_first,
        })
        .collect();
    IndexHandle {
        oid: TEST_INDEX_REL,
        locator: test_rel(TEST_INDEX_REL),
        schema: "public".to_string(),
        name: "t_idx".to_string(),
        table_name: "t".to_string(),
        unique,
        primary: false,
        columns: columns.into(),
    }
}

/// 64 フレームのプールの上に、空のファイルを持つ索引を作る（`init_index` はしない）。
pub(crate) fn test_index(cols: &[(SqlType, bool, bool)], unique: bool) -> TestIndex {
    test_index_with(TestStorage::new(), cols, unique)
}

pub(crate) fn test_index_with(
    ts: TestStorage,
    cols: &[(SqlType, bool, bool)],
    unique: bool,
) -> TestIndex {
    let handle = index_handle(cols, unique);
    ts.create_rel(handle.locator)
        .expect("create the index file");
    let start = ts.wal().insert_lsn();
    TestIndex { ts, handle, start }
}

impl TestIndex {
    pub fn pool(&self) -> &Arc<BufferPool> {
        self.ts.pool()
    }

    pub fn wal(&self) -> &Arc<Wal> {
        self.ts.wal()
    }

    pub fn locator(&self) -> RelFileLocator {
        self.handle.locator
    }

    pub fn ctx(&self) -> BtCtx<'_> {
        BtCtx {
            pool: self.ts.pool(),
            wal: self.ts.wal(),
            index: &self.handle,
            knobs: crate::debug_knobs::DebugKnobs::default(),
        }
    }

    #[allow(clippy::unused_self)]
    pub fn write_ctx(&self) -> WriteCtx {
        WriteCtx {
            xid: Xid::FIRST_NORMAL,
            cid: 0,
        }
    }

    /// ピンの取りこぼしがないこと。
    pub fn assert_clean(&self) {
        self.ts.assert_clean();
    }

    /// `start` 以降の WAL レコードを全部読む（`wal.flush` してから読む）。
    pub fn read_wal_from(&self, start: Lsn) -> Vec<DecodedRecord> {
        let wal = self.ts.wal();
        wal.flush(wal.insert_lsn()).expect("flush the WAL");
        let vfs: Arc<dyn Vfs> = Arc::new(self.ts.vfs.clone());
        let mut reader = WalReader::open(vfs, wal.config(), start);
        let mut out = Vec::new();
        while let Some(r) = reader.next().expect("read the WAL") {
            out.push(r);
        }
        out
    }

    /// このテスト環境のプール・smgr に対する REDO の文脈（REDO を空のディスクに当てるテスト用。
    /// 呼び出し側が別の `TestIndex` を作って、そのプールへ REDO する）。
    pub fn redo_ctx(&self) -> RedoCtx {
        self.ts.smgr().set_recovery_mode(true);
        RedoCtx {
            pool: Arc::clone(self.ts.pool()),
            smgr: Arc::clone(self.ts.smgr()),
            ext: Box::new(()),
            invalid: Mutex::new(InvalidPages::default()),
            next_oid: AtomicU32::new(16384),
            knobs: self.ts.options.knobs,
        }
    }

    /// 索引の全ブロックの写し（プールを flush せず、プール経由で読む）。
    pub fn snapshot_pages(&self) -> Vec<Box<crate::storage::page::Page>> {
        let n = self
            .pool()
            .nblocks(self.locator(), crate::storage::smgr::ForkNumber::Main)
            .expect("nblocks");
        (0..n)
            .map(|b| {
                let pin = self.pool().read_buffer(self.ctx().tag(b)).expect("read");
                let g = pin.read().expect("latch");
                Box::new(crate::storage::page::Page(g.0))
            })
            .collect()
    }
}

// ----- FakeHeap ---------------------------------------------------------------

/// `fetch_dirty` の結果を表で与える偽のヒープ（一意性検査のテスト用）。表にない TID は `Invisible`。
#[derive(Debug, Default)]
pub(crate) struct FakeHeap {
    pub states: Mutex<HashMap<Tid, DirtyResult>>,
}

impl FakeHeap {
    pub fn new() -> FakeHeap {
        FakeHeap::default()
    }

    pub fn set(&self, tid: Tid, r: DirtyResult) {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tid, r);
    }
}

fn unsupported<T>() -> Result<T> {
    Err(Error::not_supported("FakeHeap supports fetch_dirty only"))
}

impl TableStore for FakeHeap {
    fn create_storage(&self, _w: &WriteCtx, _rel: RelFileLocator) -> Result<()> {
        unsupported()
    }
    fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
        unsupported()
    }
    fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
        unsupported()
    }
    fn insert(&self, _rel: &RelHandle, _w: &WriteCtx, _row: &[Datum]) -> Result<Tid> {
        unsupported()
    }
    fn delete(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
    ) -> Result<TmResult> {
        unsupported()
    }
    fn update(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
        _new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        unsupported()
    }
    fn begin_scan(&self, _rel: &RelHandle, _snap: &Snapshot) -> Result<HeapScan> {
        unsupported()
    }
    fn scan_next(&self, _scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        unsupported()
    }
    fn fetch(&self, _rel: &RelHandle, _snap: &Snapshot, _tid: Tid) -> Result<Option<HeapTuple>> {
        unsupported()
    }
    fn fetch_dirty(&self, _rel: &RelHandle, _own: Option<Xid>, tid: Tid) -> Result<DirtyResult> {
        Ok(self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&tid)
            .copied()
            .unwrap_or(DirtyResult::Invisible))
    }
}

// ----- 割り込みフック (§7.8) ------------------------------------------------------

/// 降下・スキャン・`walk_left` の「ラッチを外した隙間」。隙間ではラッチもピンも持たないので、
/// フックの中で `insert` などを呼べる（同じスレッドで再入できる）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookPoint {
    /// 親を外して子（レベル `level`）を取る前。
    DescendBetweenLevels { level: u32 },
    /// メタを読んだ後、ルートを読む前。
    DescendAfterMeta,
    /// 1 枚の葉をコピーして外した後、次の葉を読む前。
    ScanBetweenLeaves,
    /// 左リンクを覚えた後、そのページを読む前。
    WalkLeftBeforeLatch { target: BlockNumber },
}

type Hook = Box<dyn FnMut(HookPoint)>;

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

/// フックを設定する（スレッドローカル）。`None` で外す。
pub(crate) fn set_test_hook(f: Option<Hook>) {
    HOOK.with(|h| *h.borrow_mut() = f);
}

/// フックがあれば呼ぶ。フックの中の再入（フックの中の降下が `fire` を呼ぶ）では何もしない。
pub(crate) fn fire(p: HookPoint) {
    let taken = HOOK.with(|h| h.borrow_mut().take());
    if let Some(mut f) = taken {
        f(p);
        HOOK.with(|h| {
            let mut slot = h.borrow_mut();
            if slot.is_none() {
                *slot = Some(f);
            }
        });
    }
}

impl TestIndex {
    /// 空のファイルに `pages[i]` をブロック `i` として書く（`BTREE_PAGES` / `Build`。32 枚まで）。
    pub fn install_pages(&self, pages: &[Box<crate::storage::page::Page>]) {
        use crate::storage::btree::wal::{PagesReason, log_pages};
        use crate::storage::buffer::CriticalSection;
        use crate::storage::smgr::ForkNumber;
        assert!(pages.len() <= crate::wal::MAX_BLOCK_REFS);
        let pins: Vec<_> = pages
            .iter()
            .map(|_| {
                self.pool()
                    .extend(self.locator(), ForkNumber::Main)
                    .expect("extend")
            })
            .collect();
        let mut guards: Vec<_> = pins
            .iter()
            .map(|p| (p.tag(), p.write_tree().expect("latch")))
            .collect();
        let cs = CriticalSection::enter(self.pool());
        for ((_, g), page) in guards.iter_mut().zip(pages) {
            *g.page_mut() = (**page).clone();
        }
        log_pages(
            self.wal(),
            &self.write_ctx(),
            PagesReason::Build,
            &mut guards,
        )
        .map_err(|e| cs.escalate(e))
        .expect("log pages");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn hook_fires_once_without_reentry_and_can_be_removed() {
        let log: Rc<RefCell<Vec<HookPoint>>> = Rc::new(RefCell::new(Vec::new()));
        let l2 = Rc::clone(&log);
        set_test_hook(Some(Box::new(move |p| {
            l2.borrow_mut().push(p);
            fire(HookPoint::ScanBetweenLeaves); // 再入は無視される
        })));
        fire(HookPoint::DescendAfterMeta);
        fire(HookPoint::DescendBetweenLevels { level: 0 });
        assert_eq!(
            *log.borrow(),
            vec![
                HookPoint::DescendAfterMeta,
                HookPoint::DescendBetweenLevels { level: 0 }
            ]
        );
        set_test_hook(None);
        fire(HookPoint::DescendAfterMeta);
        assert_eq!(log.borrow().len(), 2);
    }

    #[test]
    fn fake_heap_returns_table_values() {
        let h = FakeHeap::new();
        let rel = RelHandle {
            oid: 1,
            locator: test_rel(1),
            desc: Arc::new(crate::storage::TupleDesc { attrs: vec![] }),
            indexes: Arc::from(Vec::new()),
        };
        let t = Tid {
            block: 1,
            offset: 2,
        };
        assert_eq!(
            h.fetch_dirty(&rel, None, t).unwrap(),
            DirtyResult::Invisible
        );
        h.set(t, DirtyResult::Visible);
        assert_eq!(h.fetch_dirty(&rel, None, t).unwrap(), DirtyResult::Visible);
    }
}

// ----- 最小の木の検査器（B2b の `check.rs` ができるまでの代用。B1b） ----------------------

/// `check_tree` の結果。
#[derive(Debug)]
pub(crate) struct TreeInfo {
    /// 葉を左から右へ読んだ全項目（キー、TID）。
    pub entries: Vec<(Vec<Datum>, Tid)>,
    /// 木の高さ（葉だけなら 1）。
    pub height: u32,
    /// レベルごとのページ数（葉から）。
    pub pages_per_level: Vec<usize>,
}

type Bound = Option<(Vec<Datum>, Tid)>;

struct Checker<'a> {
    t: &'a TestIndex,
    levels: Vec<Vec<u32>>,
    entries: Vec<(Vec<Datum>, Tid)>,
}

impl Checker<'_> {
    fn page(&self, blk: u32) -> Box<crate::storage::page::Page> {
        let pin = self
            .t
            .pool()
            .read_buffer(self.t.ctx().tag(blk))
            .expect("read");
        let g = pin.read_tree().expect("latch");
        Box::new(crate::storage::page::Page(g.0))
    }

    fn decode(&self, bytes: &[u8]) -> (Vec<Datum>, Tid) {
        use super::tuple::{IndexTuple, decode_key};
        let it = IndexTuple(bytes);
        let n = self.t.handle.columns.len();
        (
            decode_key(&self.t.handle, &it, n).expect("decode"),
            it.heap_tid().expect("tid"),
        )
    }

    fn cmp(&self, a: &(Vec<Datum>, Tid), b: &(Vec<Datum>, Tid)) -> std::cmp::Ordering {
        super::cmp_key_tid(&self.t.handle, &a.0, a.1, &b.0, b.1)
    }

    fn visit(&mut self, blk: u32, level: u32, lower: &Bound, upper: &Bound) {
        use super::tuple::IndexTuple;
        let ctx = self.t.ctx();
        let page = self.page(blk);
        let sp = super::page::validate_page(&ctx, &page, blk, Some(level)).expect("valid page");
        let lvl = level as usize;
        while self.levels.len() <= lvl {
            self.levels.push(Vec::new());
        }
        self.levels[lvl].push(blk);
        assert_eq!(
            sp.next == 0,
            upper.is_none(),
            "high key presence, block {blk}"
        );
        if sp.next != 0 {
            let hk = self.decode(page.item(super::BT_P_HIKEY).unwrap());
            assert_eq!(
                Some(&hk),
                upper.as_ref(),
                "high key == parent bound, block {blk}"
            );
        }
        let first = sp.first_data_offset();
        let max = page.max_offset();
        let mut prev: Option<(Vec<Datum>, Tid)> = None;
        let mut kids: Vec<(u32, Bound)> = Vec::new();
        for off in first..=max {
            let bytes = page.item(off).unwrap().to_vec();
            let it = IndexTuple(&bytes);
            if level == 0 {
                assert!(!it.is_pivot());
                let e = self.decode(&bytes);
                if let Some(p) = &prev {
                    assert_eq!(
                        self.cmp(p, &e),
                        std::cmp::Ordering::Less,
                        "leaf order, block {blk}"
                    );
                }
                if let Some(lo) = lower {
                    assert_ne!(self.cmp(&e, lo), std::cmp::Ordering::Less, "leaf >= lower");
                }
                if let Some(up) = upper {
                    assert_eq!(self.cmp(&e, up), std::cmp::Ordering::Less, "leaf < upper");
                }
                self.entries.push(e.clone());
                prev = Some(e);
            } else if off == first {
                assert!(it.is_minus_infinity(), "first internal item is -inf");
                kids.push((it.downlink(), lower.clone()));
            } else {
                assert!(it.is_pivot() && !it.is_minus_infinity());
                let e = self.decode(&bytes);
                if let Some(p) = &prev {
                    assert_eq!(self.cmp(p, &e), std::cmp::Ordering::Less, "pivot order");
                }
                if let Some(lo) = lower {
                    assert_ne!(self.cmp(&e, lo), std::cmp::Ordering::Less, "pivot >= lower");
                }
                if let Some(up) = upper {
                    assert_eq!(self.cmp(&e, up), std::cmp::Ordering::Less, "pivot < upper");
                }
                kids.push((it.downlink(), Some(e.clone())));
                prev = Some(e);
            }
        }
        for (i, (child, lo)) in kids.iter().enumerate() {
            let up = kids
                .get(i + 1)
                .map_or_else(|| upper.clone(), |k| k.1.clone());
            self.visit(*child, level - 1, lo, &up);
        }
    }
}

/// 木全体を検査して、全項目と形を返す（順序、境界、high key、兄弟リンク、レベル）。壊れていれば panic。
pub(crate) fn check_tree(t: &TestIndex) -> TreeInfo {
    use super::meta::BtMeta;
    use super::page::BtSpecial;
    let ctx = t.ctx();
    let meta = {
        let pin = t.pool().read_buffer(ctx.tag(0)).expect("meta");
        let g = pin.read_tree().expect("latch");
        BtMeta::read(&ctx, &g).expect("meta")
    };
    let mut c = Checker {
        t,
        levels: Vec::new(),
        entries: Vec::new(),
    };
    c.visit(meta.root, meta.level, &None, &None);
    for (lvl, blocks) in c.levels.iter().enumerate() {
        for (i, &b) in blocks.iter().enumerate() {
            let sp = BtSpecial::read(&c.page(b)).unwrap();
            let want_prev = if i == 0 { 0 } else { blocks[i - 1] };
            let want_next = blocks.get(i + 1).copied().unwrap_or(0);
            assert_eq!(
                (sp.prev, sp.next),
                (want_prev, want_next),
                "siblings, level {lvl} block {b}"
            );
            assert_eq!(
                sp.is_root(),
                u32::try_from(lvl).unwrap() == meta.level,
                "ROOT flag, block {b}"
            );
        }
    }
    TreeInfo {
        entries: c.entries,
        height: meta.level + 1,
        pages_per_level: c.levels.iter().map(Vec::len).collect(),
    }
}

// ----- 挿入テストの補助（B1b） ---------------------------------------------------------

/// `init_index` 済みの索引。
pub(crate) fn new_index(cols: &[(SqlType, bool, bool)], unique: bool) -> TestIndex {
    new_index_with(TestStorage::new(), cols, unique)
}

pub(crate) fn new_index_with(
    ts: TestStorage,
    cols: &[(SqlType, bool, bool)],
    unique: bool,
) -> TestIndex {
    let t = test_index_with(ts, cols, unique);
    super::meta::init_index(t.pool(), t.wal(), &t.write_ctx(), &t.handle).expect("init_index");
    t
}

/// 通し番号 `n` から一意なヒープ TID を作る。
pub(crate) fn tid_of(n: u32) -> Tid {
    Tid {
        block: n / 1000,
        offset: u16::try_from(n % 1000 + 1).expect("offset"),
    }
}

/// 一意性検査なしで 1 件入れる。
pub(crate) fn put(t: &TestIndex, key: &[Datum], tid: Tid) -> Result<()> {
    super::insert::insert(
        &t.ctx(),
        &t.write_ctx(),
        key,
        tid,
        crate::storage::UniqueCheck::Skip,
    )
}

/// 決定的な擬似乱数（テスト用の LCG）。
pub(crate) struct Lcg(pub u64);

impl Lcg {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// `0..n` の並べ替え。
    pub fn permutation(&mut self, n: usize) -> Vec<usize> {
        let mut v: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = usize::try_from(self.below(i as u64 + 1)).unwrap_or(0);
            v.swap(i, j);
        }
        v
    }
}

/// メタが指す木の最上位のレベル（高さ - 1）。
pub(crate) fn tree_level(t: &TestIndex) -> u32 {
    let ctx = t.ctx();
    let pin = t.pool().read_buffer(ctx.tag(0)).expect("meta");
    let g = pin.read_tree().expect("latch");
    super::meta::BtMeta::read(&ctx, &g).expect("meta").level
}
