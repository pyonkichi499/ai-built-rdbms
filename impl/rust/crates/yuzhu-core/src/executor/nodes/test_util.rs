//! ノードの単体テストの足場（`m4/02` §5.2 P0-c の 7、§6.2）。X1〜X3 が拡張する
//! （X3 が `FakeIndexStore` をここに足す）。
//!
//! - [`FakeStore`]: メモリ上の `TableStore`。
//! - [`Fixture`]: `ExecCtx` が借用するものをすべて持つ。`PhysicalQuery`（`subplans`・`ctes`・`n_params`）と
//!   `mem_limit` を指定できる。
//! - [`CountingExec`]: 子の読み込み回数・`rewind` 回数を数えるテスト用の入力。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

use crate::catalog::fake::FakeCatalog;
use crate::error::Result;
use crate::error::{Error, sqlstate};
use crate::executor::eval::tests::session;
use crate::executor::mem::DEFAULT_QUERY_MEM_LIMIT;
use crate::executor::{BoxedExecutor, ExecCtx, ExecEnv, Executor, NullRuntime};
use crate::interrupt::InterruptFlag;
use crate::planner::physical::PhysicalQuery;
use crate::storage::smgr::RelFileLocator;
use crate::storage::{
    BuildStats, BuildUnique, DirtyResult, HeapScan, HeapTuple, IndexHandle, IndexScan, IndexStore,
    RelHandle, ResolvedScanKeys, ScanDirection, TableStore, TmResult, TupleState, UniqueCheck,
    UpdateOutcome, WriteCtx,
};
use crate::txn::{Snapshot, Transaction, Xid};
use crate::types::cmp::cmp_with_nulls;
use crate::types::{Datum, Oid, Row, Tid, TypeEnv, cmp_datum};

/// A `TableStore` that keeps rows in memory (no MVCC). Deleted rows
/// leave a hole so TIDs stay stable. `force_result` makes the next
/// `delete` / `update` return a given `TmResult` without changing
/// anything. Stands in for `HeapStore` in executor and planner tests.
#[derive(Debug, Default)]
pub(crate) struct FakeStore {
    tables: Mutex<HashMap<Oid, Vec<Option<Row>>>>,
    forced: Mutex<std::collections::VecDeque<TmResult>>,
    /// `(xid, cid)` of every write, in order.
    writes: Mutex<Vec<(Xid, u32)>>,
}

impl FakeStore {
    /// Appends a row without going through `TableStore::insert`.
    pub(crate) fn add_row(&self, oid: Oid, row: Row) {
        self.tables
            .lock()
            .unwrap()
            .entry(oid)
            .or_default()
            .push(Some(row));
    }

    /// Live rows in TID order.
    pub(crate) fn rows(&self, oid: Oid) -> Vec<Row> {
        self.tables
            .lock()
            .unwrap()
            .get(&oid)
            .map(|v| v.iter().flatten().cloned().collect())
            .unwrap_or_default()
    }

    /// The next `delete` / `update` returns `r` and changes nothing.
    pub(crate) fn force_result(&self, r: TmResult) {
        self.forced.lock().unwrap().push_back(r);
    }

    pub(crate) fn writes(&self) -> Vec<(Xid, u32)> {
        self.writes.lock().unwrap().clone()
    }

    fn slot(tid: Tid) -> usize {
        usize::from(tid.offset).saturating_sub(1)
    }

    fn heap_tuple(i: usize, row: &Row) -> HeapTuple {
        HeapTuple {
            tid: tid_of(i),
            xmin: Xid::BOOTSTRAP,
            xmax: Xid::INVALID,
            cmin: 0,
            cmax: 0,
            row: row.clone(),
        }
    }

    /// 生きている行を TID の順に。
    fn live_tuples(&self, oid: Oid) -> Vec<HeapTuple> {
        self.tables
            .lock()
            .unwrap()
            .get(&oid)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .filter_map(|(i, r)| r.as_ref().map(|row| Self::heap_tuple(i, row)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `tid` の行（削除済み・範囲外なら `None`）。
    fn tuple_at(&self, oid: Oid, tid: Tid) -> Option<HeapTuple> {
        let t = self.tables.lock().unwrap();
        let slot = Self::slot(tid);
        t.get(&oid)?
            .get(slot)?
            .as_ref()
            .map(|row| Self::heap_tuple(slot, row))
    }
}

fn tid_of(n: usize) -> Tid {
    Tid {
        block: 0,
        offset: u16::try_from(n + 1).unwrap(),
    }
}

impl TableStore for FakeStore {
    fn create_storage(&self, _w: &WriteCtx, _rel: RelFileLocator) -> Result<()> {
        Ok(())
    }
    fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
        Ok(false)
    }
    fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
        Ok(())
    }
    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
        assert_ne!(w.xid, Xid::INVALID);
        self.writes.lock().unwrap().push((w.xid, w.cid));
        let mut t = self.tables.lock().unwrap();
        let rows = t.entry(rel.oid).or_default();
        rows.push(Some(row.to_vec()));
        Ok(tid_of(rows.len() - 1))
    }
    fn delete(&self, rel: &RelHandle, w: &WriteCtx, _: &Snapshot, tid: Tid) -> Result<TmResult> {
        self.writes.lock().unwrap().push((w.xid, w.cid));
        if let Some(r) = self.forced.lock().unwrap().pop_front() {
            return Ok(r);
        }
        let mut t = self.tables.lock().unwrap();
        match t.get_mut(&rel.oid).and_then(|v| v.get_mut(Self::slot(tid))) {
            Some(s) if s.is_some() => {
                *s = None;
                Ok(TmResult::Ok)
            }
            _ => Ok(TmResult::Invisible),
        }
    }
    fn update(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        _: &Snapshot,
        tid: Tid,
        new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        self.writes.lock().unwrap().push((w.xid, w.cid));
        if let Some(r) = self.forced.lock().unwrap().pop_front() {
            return Ok(UpdateOutcome {
                result: r,
                new_tid: None,
            });
        }
        let mut t = self.tables.lock().unwrap();
        let rows = t.entry(rel.oid).or_default();
        match rows.get_mut(Self::slot(tid)) {
            Some(s) if s.is_some() => {
                *s = None;
                rows.push(Some(new_row.to_vec()));
                Ok(UpdateOutcome {
                    result: TmResult::Ok,
                    new_tid: Some(tid_of(rows.len() - 1)),
                })
            }
            _ => Ok(UpdateOutcome {
                result: TmResult::Invisible,
                new_tid: None,
            }),
        }
    }
    fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
        // The live rows at this moment: rows added later (the new
        // versions written by an UPDATE) are not seen, like the
        // command-ID rule of the real heap.
        let tuples = self.live_tuples(rel.oid);
        Ok(HeapScan::from_tuples(rel.clone(), snap.clone(), tuples))
    }
    fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        Ok(scan.pop_buffered())
    }
    fn fetch(&self, rel: &RelHandle, _: &Snapshot, tid: Tid) -> Result<Option<HeapTuple>> {
        Ok(self.tuple_at(rel.oid, tid))
    }
    fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan> {
        let snap = Snapshot {
            xmin: Xid(3),
            xmax: Xid(4),
            xip: vec![],
            curcid: 0,
            own_xid: None,
        };
        Ok(HeapScan::from_tuples(
            rel.clone(),
            snap,
            self.live_tuples(rel.oid),
        ))
    }
    fn tuple_state(&self, _t: &HeapTuple, _own: Option<Xid>) -> Result<TupleState> {
        Ok(TupleState::Live)
    }
    fn fetch_dirty(&self, rel: &RelHandle, _own: Option<Xid>, tid: Tid) -> Result<DirtyResult> {
        Ok(if self.tuple_at(rel.oid, tid).is_some() {
            DirtyResult::Visible
        } else {
            DirtyResult::Invisible
        })
    }
    fn nblocks(&self, rel: &RelHandle) -> Result<u32> {
        Ok(u32::from(!self.rows(rel.oid).is_empty()))
    }
}

/// メモリ上の `IndexStore`（`m4/05` §10.1）。索引ごとに `(キー, TID)` を木の順序（列ごとの
/// `cmp_with_nulls` + TID）で持つ。`insert` は本物と同じく、一意索引で NULL を含まないキーが既存の項目と
/// 全列等しいとき `FakeStore::fetch_dirty` で生きている行かを調べ、23505（`s` / `t` / `n` つき、DETAIL なし）を返す。
/// `begin_scan` は `ResolvedScanKeys` で絞る（範囲の列は昇順の索引だけ扱う）。
type IndexEntry = (Vec<Datum>, Tid);

#[derive(Debug, Default)]
pub(crate) struct FakeIndexStore {
    entries: Mutex<HashMap<Oid, Vec<IndexEntry>>>,
}

impl FakeIndexStore {
    /// 索引 `oid` の項目数。
    pub(crate) fn len(&self, oid: Oid) -> usize {
        self.entries.lock().unwrap().get(&oid).map_or(0, Vec::len)
    }

    /// 索引 `oid` の項目（木の順序）。
    pub(crate) fn entries(&self, oid: Oid) -> Vec<IndexEntry> {
        self.entries
            .lock()
            .unwrap()
            .get(&oid)
            .cloned()
            .unwrap_or_default()
    }

    fn cmp_keys(index: &IndexHandle, a: &[Datum], b: &[Datum]) -> std::cmp::Ordering {
        index
            .columns
            .iter()
            .zip(a.iter().zip(b))
            .map(|(c, (x, y))| cmp_with_nulls(x, y, c.descending, c.nulls_first))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    }

    fn matches(keys: &ResolvedScanKeys, key: &[Datum]) -> bool {
        for (i, k) in keys.eq.iter().enumerate() {
            let ok = match k {
                None => key[i].is_null(),
                Some(v) => !key[i].is_null() && cmp_datum(&key[i], v).is_eq(),
            };
            if !ok {
                return false;
            }
        }
        let n = keys.eq.len();
        if keys.lower.is_none() && keys.upper.is_none() {
            return true;
        }
        let Some(d) = key.get(n) else {
            return false;
        };
        if d.is_null() {
            return false;
        }
        let lower_ok = keys.lower.as_ref().is_none_or(|(v, inclusive)| {
            let o = cmp_datum(d, v);
            o.is_gt() || (*inclusive && o.is_eq())
        });
        let upper_ok = keys.upper.as_ref().is_none_or(|(v, inclusive)| {
            let o = cmp_datum(d, v);
            o.is_lt() || (*inclusive && o.is_eq())
        });
        lower_ok && upper_ok
    }
}

impl IndexStore for FakeIndexStore {
    fn init_index(&self, _w: &WriteCtx, index: &IndexHandle) -> Result<()> {
        self.entries.lock().unwrap().entry(index.oid).or_default();
        Ok(())
    }

    fn insert(
        &self,
        _w: &WriteCtx,
        index: &IndexHandle,
        key: &[Datum],
        tid: Tid,
        check: UniqueCheck<'_>,
    ) -> Result<()> {
        let mut all = self.entries.lock().unwrap();
        let list = all.entry(index.oid).or_default();
        if let UniqueCheck::Check { heap, rel, own_xid } = check
            && index.unique
            && !key.iter().any(Datum::is_null)
        {
            for (k, t) in list.iter() {
                if Self::cmp_keys(index, k, key).is_ne() {
                    continue;
                }
                match heap.fetch_dirty(rel, Some(own_xid), *t)? {
                    DirtyResult::Visible => {
                        return Err(Error::new(
                            sqlstate::UNIQUE_VIOLATION,
                            format!(
                                "duplicate key value violates unique constraint \"{}\"",
                                index.name
                            ),
                        )
                        .with_table(index.schema.clone(), index.table_name.clone())
                        .with_constraint(index.name.clone()));
                    }
                    DirtyResult::Invisible => {}
                    DirtyResult::WaitFor(_) => {
                        return Err(Error::internal("unique check would have to wait"));
                    }
                }
            }
        }
        let pos = list.partition_point(|(k, t)| {
            Self::cmp_keys(index, k, key)
                .then_with(|| (t.block, t.offset).cmp(&(tid.block, tid.offset)))
                .is_lt()
        });
        list.insert(pos, (key.to_vec(), tid));
        Ok(())
    }

    fn build(
        &self,
        w: &WriteCtx,
        index: &IndexHandle,
        entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>,
        _unique: BuildUnique,
    ) -> Result<BuildStats> {
        let mut n = 0;
        for (k, t) in entries {
            self.insert(w, index, &k, t, UniqueCheck::Skip)?;
            n += 1;
        }
        Ok(BuildStats {
            tuples: n,
            pages: 1,
            levels: 1,
        })
    }

    fn begin_scan(
        &self,
        index: &IndexHandle,
        keys: &ResolvedScanKeys,
        dir: ScanDirection,
    ) -> Result<IndexScan> {
        let mut tids: Vec<Tid> = self
            .entries(index.oid)
            .into_iter()
            .filter(|(k, _)| Self::matches(keys, k))
            .map(|(_, t)| t)
            .collect();
        if dir == ScanDirection::Backward {
            tids.reverse();
        }
        Ok(IndexScan::for_test(tids))
    }

    fn scan_next(&self, scan: &mut IndexScan) -> Result<Option<Tid>> {
        Ok(scan.fake_tids.pop_front())
    }

    fn nblocks(&self, index: &IndexHandle) -> Result<u32> {
        Ok(u32::from(self.len(index.oid) > 0))
    }

    fn unlink_storage(&self, index: &IndexHandle) -> Result<()> {
        self.entries.lock().unwrap().remove(&index.oid);
        Ok(())
    }
}

/// Test fixture owning everything an `ExecCtx` borrows.
pub(crate) struct Fixture {
    pub(crate) catalog: FakeCatalog,
    pub(crate) storage: FakeStore,
    pub(crate) indexes: FakeIndexStore,
    pub(crate) txn: Transaction,
    pub(crate) snapshot: Snapshot,
    pub(crate) session: crate::executor::SessionInfo,
    pub(crate) interrupts: InterruptFlag,
    /// `ExecCtx::query`。`n_params` / `subplans` / `ctes` を指定するテストが差し替える。
    pub(crate) query: PhysicalQuery,
    pub(crate) mem_limit: usize,
    type_env: TypeEnv<'static>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let mut txn = Transaction::new();
        txn.xid = Some(Xid(3));
        Fixture {
            catalog: FakeCatalog::new("postgres"),
            storage: FakeStore::default(),
            indexes: FakeIndexStore::default(),
            txn,
            snapshot: Snapshot {
                xmin: Xid(3),
                xmax: Xid(4),
                xip: vec![],
                curcid: 0,
                own_xid: Some(Xid(3)),
            },
            session: session(),
            interrupts: InterruptFlag::default(),
            query: PhysicalQuery::empty(),
            mem_limit: DEFAULT_QUERY_MEM_LIMIT,
            type_env: TypeEnv::default(),
        }
    }

    /// `n` 個のパラメータ（`ParamId` 0..n、初期値 NULL）を持つ。
    pub(crate) fn with_params(n: usize) -> Self {
        let mut f = Fixture::new();
        f.query.n_params = n;
        f
    }

    /// `subplans` / `ctes` / `n_params` を持つ問い合わせを使う。
    pub(crate) fn with_query(query: PhysicalQuery) -> Self {
        let mut f = Fixture::new();
        f.query = query;
        f
    }

    /// `ExecCtx` を作る（`next` / `rewind` を複数回呼ぶテスト用。`params` は呼び出し側が書き換えられる）。
    pub(crate) fn ctx(&mut self) -> ExecCtx<'_> {
        let mut env = ExecEnv::without_indexes(
            &self.catalog,
            &self.storage,
            &self.snapshot,
            &self.session,
            &NullRuntime,
            &self.interrupts,
            &self.type_env,
        );
        env.indexes = &self.indexes;
        env.mem_limit = self.mem_limit;
        ExecCtx::new(env, &mut self.txn, &self.query)
    }

    /// Runs an executor to completion, returning all rows.
    pub(crate) fn run(&mut self, exec: &mut BoxedExecutor) -> Result<Vec<Row>> {
        let mut ctx = self.ctx();
        drain(exec, &mut ctx)
    }
}

/// `exec` を尽きるまで読む。
pub(crate) fn drain(exec: &mut BoxedExecutor, ctx: &mut ExecCtx<'_>) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    while let Some(r) = exec.next(ctx)? {
        out.push(r);
    }
    Ok(out)
}

/// 行を返すだけの入力で、読み込み（`next` が行を返した回数）と `rewind` の回数を数える
/// （`rewind_reuses_or_rebuilds_by_uses_params` の `CountingScan`）。
#[derive(Debug)]
pub(crate) struct CountingExec {
    rows: Vec<Row>,
    pos: usize,
    /// `next` が行を返した回数の合計。
    pub(crate) reads: Rc<Cell<usize>>,
    pub(crate) rewinds: Rc<Cell<usize>>,
}

impl CountingExec {
    pub(crate) fn new(rows: Vec<Row>) -> (Self, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        let rewinds = Rc::new(Cell::new(0));
        (
            CountingExec {
                rows,
                pos: 0,
                reads: Rc::clone(&reads),
                rewinds: Rc::clone(&rewinds),
            },
            reads,
            rewinds,
        )
    }

    /// `n` 行（`Int4(0..n)` の 1 列）。
    pub(crate) fn ints(n: i32) -> (Self, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        CountingExec::new((0..n).map(|i| vec![Datum::Int4(i)]).collect())
    }
}

impl Executor for CountingExec {
    fn next(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        let Some(r) = self.rows.get(self.pos) else {
            return Ok(None);
        };
        self.pos += 1;
        self.reads.set(self.reads.get() + 1);
        Ok(Some(r.clone()))
    }

    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.pos = 0;
        self.rewinds.set(self.rewinds.get() + 1);
        Ok(())
    }
}

/// 索引のテスト用の `IndexHandle`。`cols` は `(attnum, 列名, 型)`（すべて昇順・NULLS LAST）。
pub(crate) fn index_on(
    oid: Oid,
    name: &str,
    table: &str,
    unique: bool,
    cols: &[(i16, &str, crate::types::SqlType)],
) -> IndexHandle {
    use crate::storage::smgr::{RelFileLocator, RelFileNumber};
    use crate::storage::{AttrDesc, IndexKeyColumn};
    IndexHandle {
        oid,
        locator: RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(oid),
        },
        schema: "public".into(),
        name: name.into(),
        table_name: table.into(),
        unique,
        primary: false,
        columns: cols
            .iter()
            .map(|(attnum, n, ty)| IndexKeyColumn {
                attnum: *attnum,
                name: (*n).into(),
                ty: *ty,
                attr: AttrDesc::from_type(ty.oid),
                cmp: cmp_datum,
                descending: false,
                nulls_first: false,
            })
            .collect(),
    }
}

impl Fixture {
    /// `row` を `rel` に挿入し、`rel.indexes` のすべてに項目を入れる（一意性検査なし）。
    pub(crate) fn insert_indexed(&self, rel: &RelHandle, row: &Row) -> Tid {
        let w = WriteCtx {
            xid: Xid(3),
            cid: 0,
        };
        let tid = self.storage.insert(rel, &w, row).unwrap();
        for idx in rel.indexes.iter() {
            let key: Vec<Datum> = idx
                .columns
                .iter()
                .map(|c| row[usize::try_from(c.attnum).unwrap() - 1].clone())
                .collect();
            self.indexes
                .insert(&w, idx, &key, tid, UniqueCheck::Skip)
                .unwrap();
        }
        tid
    }
}
