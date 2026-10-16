//! `HeapStore`: the `TableStore` implementation over the buffer pool and
//! the heap (`m2.md` §4.4, §6.5; WAL: `m3.md` §5.1).
//!
//! M5 (`m5/02-row-lock-rr.md` RW-1c): `delete` / `update` / `lock_tuple` follow
//! PostgreSQL's `l1:` / `l2:` / `l3:` loops. Whenever a conflicting holder must
//! be waited for, the page latch and the pin are released first (convention 1),
//! the wait happens in [`wait_for_holders`], and the whole decision is taken
//! again from the top (RW-D5).
//!
//! The environment for "who is running" and `MultiXact` lookups comes from
//! [`HeapStore::set_vacuum_env`], the lock manager from
//! [`HeapStore::set_lock_env`]. Until the cluster has wired them (the assembly in
//! `m5/01` §5.12), a store behaves like M4: "running" is derived from the caller's
//! snapshot and a write that would have to wait returns `BeingModified`.

use std::sync::{Arc, OnceLock};

use crate::control::{ControlData, ControlFileHandle};
use crate::error::{Error, Result};
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard};
use crate::storage::fsm::FreeSpaceMap;
use crate::storage::heap::hio::{self, HioCtx, page_error, tag};
use crate::storage::heap::lock::{TupleLockGuard, WaitEnv, WaitResult, wait_for_holders};
use crate::storage::heap::modstat;
use crate::storage::heap::prune::{HeapEnv, PruneCtx, RunningXids, prune_before_read};
use crate::storage::heap::scan::decode_if_visible;
use crate::storage::heap::tuple::{TupleFlags, TupleHeader, deform_tuple, form_tuple};
use crate::storage::heap::visibility::{
    VisEnv, classify_env, satisfies_dirty_env, satisfies_update_env, visible,
};
use crate::storage::heap::wal::{
    HEAP_DELETE, HEAP_INIT_PAGE, HEAP_LOCK, HEAP_UPDATE, HeapDeleteMain, HeapLockMain,
    HeapUpdateMain,
};
use crate::storage::heap::xmax::{
    LockDecision, RawXmax, UpdateDecision, XmaxEnv, XmaxState, XmaxWrite, decode_xmax,
    keys_changed, mode_from_bits, plan_lock, plan_update, stamp_updater, xmax_is_lock_only,
    xmin_is_frozen,
};
use crate::storage::page::LpFlags;
use crate::storage::smgr::{BlockNumber, ForkNumber, RelFileLocator};
use crate::storage::smgr_wal::log_and_create;
use crate::storage::vfs::{SimVfs, Vfs};
use crate::storage::{
    DirtyResult, HeapScan, HeapTuple, LockOutcome, RelHandle, RowWait, TableStore, TmResult,
    TupleState, UpdateOutcome, WriteCtx,
};
use crate::txn::clog::{Clog, XidStatus};
use crate::txn::lock::{LockManager, LockTag, WaitCtx};
use crate::txn::multixact::MultiXactTable;
use crate::txn::{LockTupleMode, Snapshot, Xid};
use crate::types::{Datum, Tid};
use crate::wal::{Lsn, RecordBuilder, RegFlags, RmgrId, Wal};

#[derive(Debug)]
pub struct HeapStore {
    pool: Arc<BufferPool>,
    clog: Arc<Clog>,
    wal: Arc<Wal>,
    hio: HioCtx,
    /// 実行中 XID の窓口と `MultiXact` 表。機会的 pruning（`m5/03` §5.2.3）、`env()`、行の更新競合の判定
    /// （`m5/02` §5.1）に使う。`Cluster::open` が `set_vacuum_env` で渡す。
    /// 未設定の間は pruning せず、競合の判定は呼び出しの snapshot から導く（M4 の動作）。
    vacuum_env: OnceLock<(Arc<dyn RunningXids>, Arc<MultiXactTable>)>,
    /// タプルロックと XID の終了待ちに使う。`set_lock_env` で渡す。未設定なら待たない。
    locks: OnceLock<Arc<LockManager>>,
    /// `vacuum_env` が未設定の間だけ使う空の `MultiXact` 表（ロックだけの xmax が無い前提）。
    fallback_multi: OnceLock<Arc<MultiXactTable>>,
}

/// `vacuum_env` が未設定のとき、呼び出しの snapshot から「実行中」を導く（M4 の `satisfies_update` と同じ）:
/// clog が `IN_PROGRESS` で、snapshot の `xmax` 以上か `xip` にある XID。
#[derive(Debug)]
struct SnapRunning<'a> {
    clog: &'a Clog,
    snap: &'a Snapshot,
}

impl RunningXids for SnapRunning<'_> {
    fn is_in_progress(&self, x: Xid) -> bool {
        (x >= self.snap.xmax || self.snap.xip.binary_search(&x).is_ok())
            && matches!(self.clog.status(x), Ok(XidStatus::InProgress))
    }

    fn next_xid(&self) -> Xid {
        self.snap.xmax
    }

    fn oldest_xmin(&self) -> Xid {
        self.snap.xmin
    }

    fn oldest_xmin_hint(&self) -> Xid {
        self.snap.xmin
    }
}

/// `vacuum_env` が未設定のとき、snapshot の無い呼び出し（`tuple_state` / `fetch_dirty`）が使う:
/// 実行中の XID は無い（clog が `IN_PROGRESS` のものは中断扱い。M4 の単一ライター）。
#[derive(Debug)]
struct NoneRunning;

impl RunningXids for NoneRunning {
    fn is_in_progress(&self, _: Xid) -> bool {
        false
    }

    fn next_xid(&self) -> Xid {
        Xid::INVALID
    }

    fn oldest_xmin(&self) -> Xid {
        Xid::INVALID
    }

    fn oldest_xmin_hint(&self) -> Xid {
        Xid::INVALID
    }
}

/// 1 回の呼び出しが使う「誰が実行中か」と `MultiXact` 表。
#[derive(Clone, Copy)]
struct RunEnv<'a> {
    procs: &'a dyn RunningXids,
    multi: &'a MultiXactTable,
}

/// `satisfies_update_env` の結果を、書いてよいか・返すか・待つかに分けたもの。
#[derive(Debug)]
enum Gate {
    /// xmax を書いてよい（xmax なし、終了したロックだけ、自分のロックだけ、中断した更新者）。
    Go,
    /// そのまま呼び出し側に返す（`Invisible` / `SelfModified` / `Updated` / `Deleted`）。
    Done(TmResult),
    /// 衝突する保持者がいる。
    Wait { xids: Vec<Xid>, i_am_member: bool },
}

impl HeapStore {
    pub fn new(pool: Arc<BufferPool>, clog: Arc<Clog>, wal: Arc<Wal>) -> HeapStore {
        let hio = HioCtx::new(
            Arc::clone(&pool),
            Arc::new(FreeSpaceMap::new(Arc::clone(&pool))),
            Arc::clone(&clog),
            Arc::clone(&wal),
        );
        HeapStore {
            pool,
            clog,
            wal,
            hio,
            vacuum_env: OnceLock::new(),
            locks: OnceLock::new(),
            fallback_multi: OnceLock::new(),
        }
    }

    /// The free space map (derived data; VACUUM rebuilds it with `record_batch`).
    pub fn fsm(&self) -> &Arc<FreeSpaceMap> {
        self.hio.fsm()
    }

    /// Hands over the running-XID registry and the `MultiXact` table (once). Until then
    /// no opportunistic pruning happens and [`env`](Self::env) fails. `false` if
    /// already set.
    pub fn set_vacuum_env(
        &self,
        procs: Arc<dyn RunningXids>,
        multixact: Arc<MultiXactTable>,
    ) -> bool {
        self.hio.set_env(Arc::clone(&procs), Arc::clone(&multixact));
        self.vacuum_env.set((procs, multixact)).is_ok()
    }

    /// Hands over the lock manager (once): the tuple locks and the waits for XIDs
    /// (`m5/02` §5.5). Until then a write that has to wait fails with an internal
    /// error, and `lock_tuple` is unavailable. `false` if already set.
    pub fn set_lock_env(&self, locks: Arc<LockManager>) -> bool {
        self.locks.set(locks).is_ok()
    }

    /// The environment of `satisfies_vacuum` (`m5/03` §4.1).
    pub fn env(&self) -> Result<HeapEnv<'_>> {
        let (procs, multixact) = self
            .vacuum_env
            .get()
            .ok_or_else(|| Error::internal("the heap store has no running-XID registry yet"))?;
        Ok(HeapEnv {
            clog: &self.clog,
            procs: procs.as_ref(),
            multixact,
        })
    }

    fn prune_ctx(&self) -> Option<PruneCtx<'_>> {
        Some(PruneCtx {
            env: self.env().ok()?,
            wal: &self.wal,
        })
    }

    /// The environment of one call. `fallback` is the stand-in for the running-XID
    /// registry when `set_vacuum_env` has not been called.
    fn run_env<'a>(&'a self, fallback: &'a dyn RunningXids) -> Result<RunEnv<'a>> {
        if let Some((procs, multi)) = self.vacuum_env.get() {
            return Ok(RunEnv {
                procs: procs.as_ref(),
                multi,
            });
        }
        let multi = if let Some(m) = self.fallback_multi.get() {
            m
        } else {
            let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(0));
            let control = Arc::new(ControlFileHandle::create(
                &vfs,
                &ControlData::initial(1, 2, 131_072),
            )?);
            let _ = self
                .fallback_multi
                .set(Arc::new(MultiXactTable::new(control)));
            self.fallback_multi
                .get()
                .ok_or_else(|| Error::internal("MultiXact stand-in vanished"))?
        };
        Ok(RunEnv {
            procs: fallback,
            multi,
        })
    }

    fn lock_manager(&self) -> Result<&Arc<LockManager>> {
        self.locks.get().ok_or_else(|| {
            Error::internal("the heap store has no lock manager (cannot wait for a row)")
        })
    }

    /// `satisfies_update` followed by `plan_update` (`m5/02` §5.2): may the caller put its
    /// XID in xmax now?
    fn gate_update(
        &self,
        renv: RunEnv<'_>,
        hdr: &TupleHeader,
        snap: &Snapshot,
        tid: Tid,
        w: &WriteCtx,
    ) -> Result<Gate> {
        let venv = VisEnv {
            clog: &self.clog,
            procs: renv.procs,
            multi: renv.multi,
        };
        match satisfies_update_env(&venv, hdr, snap, tid)? {
            TmResult::Ok => Ok(Gate::Go),
            TmResult::BeingModified { .. } => {
                // 更新者（ロックだけでない xmax）が実行中なら、必ず終了を待つ。判定のあとで更新者が
                // コミットしても `plan_update` に任せない（`alive` が偽になり、コミット済みの更新者の xmax を
                // 上書きしてしまう）。待ったあとは `satisfies_update` からやり直す。
                if let XmaxState::Updater { xid, .. } = decode_xmax(hdr)? {
                    return Ok(Gate::Wait {
                        xids: vec![xid],
                        i_am_member: false,
                    });
                }
                let running = |x: Xid| renv.procs.is_in_progress(x);
                let xenv = XmaxEnv {
                    me: w.xid,
                    is_running: &running,
                    multi: renv.multi,
                };
                Ok(match plan_update(hdr, &xenv)? {
                    UpdateDecision::Proceed => Gate::Go,
                    UpdateDecision::Wait { xids, i_am_member } => Gate::Wait { xids, i_am_member },
                })
            }
            other => Ok(Gate::Done(other)),
        }
    }

    /// Waits for `xids` with the latch and the pin already released. `Block` never
    /// returns `WouldBlock`.
    #[allow(clippy::too_many_arguments)]
    fn wait_holders(
        &self,
        renv: RunEnv<'_>,
        wait: &WaitCtx<'_>,
        tuplock: &mut TupleLockGuard,
        rel: &RelHandle,
        tid: Tid,
        xids: &[Xid],
        policy: RowWait,
        i_am_member: bool,
    ) -> Result<WaitResult> {
        let env = WaitEnv {
            locks: self.lock_manager()?,
            procs: renv.procs,
        };
        wait_for_holders(
            &env,
            wait,
            tuplock,
            tuple_tag(rel, tid),
            xids,
            policy,
            i_am_member,
        )
    }

    /// Reads the header of a live tuple under the latch, or `None` if the
    /// TID does not point to a normal line pointer.
    fn locate(
        guard: &PageWriteGuard<'_>,
        rel: RelFileLocator,
        tid: Tid,
    ) -> Result<Option<TupleHeader>> {
        let page = guard.page();
        if page.is_new() || tid.offset == 0 || tid.offset > page.max_offset() {
            return Ok(None);
        }
        let id = page
            .item_id(tid.offset)
            .map_err(|e| page_error(e, rel, tid.block))?;
        if id.flags != LpFlags::Normal {
            return Ok(None);
        }
        let bytes = page
            .item(tid.offset)
            .map_err(|e| page_error(e, rel, tid.block))?;
        Ok(Some(TupleHeader::read(bytes)?))
    }

    fn block_exists(&self, rel: RelFileLocator, tid: Tid) -> Result<bool> {
        Ok(tid.offset != 0 && tid.block < self.pool.nblocks(rel, ForkNumber::Main)?)
    }

    /// The lock strength an UPDATE of the tuple at `tid` to `new_row` needs: `Exclusive`
    /// when a key column changes, else `NoKeyExclusive` (`m5/02` §5.3).
    fn update_lockmode(
        rel: &RelHandle,
        guard: &PageWriteGuard<'_>,
        tid: Tid,
        new_row: &[Datum],
    ) -> Result<LockTupleMode> {
        if !rel.key_columns.iter().any(|k| *k) {
            return Ok(LockTupleMode::NoKeyExclusive);
        }
        let bytes = guard
            .page()
            .item(tid.offset)
            .map_err(|e| page_error(e, rel.locator, tid.block))?;
        let old = deform_tuple(&rel.desc, bytes)?;
        Ok(if keys_changed(&rel.key_columns, &old, new_row) {
            LockTupleMode::Exclusive
        } else {
            LockTupleMode::NoKeyExclusive
        })
    }

    /// Writes `HEAP_DELETE` for the header just stamped on `guard`.
    fn log_delete(
        &self,
        guard: &PageWriteGuard<'_>,
        tag: crate::storage::smgr::BufferTag,
        off: u16,
        stamped: &TupleHeader,
        w: &WriteCtx,
    ) -> Result<Lsn> {
        let mut rec = RecordBuilder::new(RmgrId::Heap, HEAP_DELETE, w.xid);
        rec.register_block(tag, guard.page(), RegFlags::STANDARD);
        rec.main_data(&HeapDeleteMain::from_header(off, stamped).encode());
        Ok(self.wal.insert(rec)?.end)
    }

    /// Writes `HEAP_LOCK` for the header just stamped on `guard` (`m5/02` §3.6).
    fn log_lock(
        &self,
        guard: &PageWriteGuard<'_>,
        tag: crate::storage::smgr::BufferTag,
        off: u16,
        stamped: &TupleHeader,
        lock_mode: LockTupleMode,
        w: &WriteCtx,
    ) -> Result<Lsn> {
        let mut rec = RecordBuilder::new(RmgrId::Heap, HEAP_LOCK, w.xid);
        rec.register_block(tag, guard.page(), RegFlags::STANDARD);
        rec.main_data(&HeapLockMain::from_header(off, stamped, lock_mode as u8).encode());
        Ok(self.wal.insert(rec)?.end)
    }

    /// Writes `HEAP_UPDATE`. `new` is the page holding the new version; `old` is the
    /// page of the old version, or `None` when they are the same page.
    #[allow(clippy::too_many_arguments)]
    fn log_update(
        &self,
        new: (&PageWriteGuard<'_>, crate::storage::smgr::BufferTag, bool),
        old: Option<(&PageWriteGuard<'_>, crate::storage::smgr::BufferTag)>,
        new_off: u16,
        old_off: u16,
        stamped: &TupleHeader,
        w: &WriteCtx,
    ) -> Result<Lsn> {
        let (new_guard, new_tag, init) = new;
        let info = HEAP_UPDATE | if init { HEAP_INIT_PAGE } else { 0 };
        let mut rec = RecordBuilder::new(RmgrId::Heap, info, w.xid);
        let flags = if init {
            RegFlags::STANDARD | RegFlags::WILL_INIT
        } else {
            RegFlags::STANDARD
        };
        let b = rec.register_block(new_tag, new_guard.page(), flags);
        let bytes = new_guard
            .page()
            .item(new_off)
            .map_err(|_| Error::internal("freshly added tuple is unreadable"))?;
        rec.block_data(b, bytes);
        if let Some((old_guard, old_tag)) = old {
            rec.register_block(old_tag, old_guard.page(), RegFlags::STANDARD);
        }
        rec.main_data(
            &HeapUpdateMain::from_header(old_off, new_off, stamped, old.is_none()).encode(),
        );
        Ok(self.wal.insert(rec)?.end)
    }
}

/// The tuple lock's tag for the row at `tid` (`m5/02` §5.5).
fn tuple_tag(rel: &RelHandle, tid: Tid) -> LockTag {
    LockTag::Tuple {
        db: rel.locator.db_oid,
        rel: rel.oid,
        block: tid.block,
        offset: tid.offset,
    }
}

/// Writes the updater's / deleter's xmax and cmax (and optionally the new `ctid`) into the
/// old version, marks the page prunable and returns the header as written. Must run inside
/// a critical section.
fn stamp_xmax(
    guard: &mut PageWriteGuard<'_>,
    off: u16,
    mut hdr: TupleHeader,
    w: &WriteCtx,
    keys_updated: bool,
    new_ctid: Option<Tid>,
) -> Result<TupleHeader> {
    stamp_updater(&hdr, w.xid, w.cid, keys_updated).apply(&mut hdr);
    if let Some(c) = new_ctid {
        hdr.ctid = c;
    }
    let item = guard
        .page_mut()
        .item_mut(off)
        .map_err(|_| Error::internal("tuple vanished while being marked"))?;
    hdr.write(item);
    guard.page_mut().set_prunable(w.xid);
    Ok(hdr)
}

/// Writes a row lock (`XmaxWrite` from `plan_lock`) into the tuple and returns the header as
/// written. `cmax` and `ctid` stay. Must run inside a critical section.
fn stamp_lock(
    guard: &mut PageWriteGuard<'_>,
    off: u16,
    mut hdr: TupleHeader,
    write: &XmaxWrite,
) -> Result<TupleHeader> {
    write.apply(&mut hdr);
    let item = guard
        .page_mut()
        .item_mut(off)
        .map_err(|_| Error::internal("tuple vanished while being locked"))?;
    hdr.write(item);
    Ok(hdr)
}

/// `HeapTuple.xmax` is the deleter / updater; a lock holder is not one.
fn deleter_xmax(h: &TupleHeader) -> Xid {
    if h.xmax_invalid() || xmax_is_lock_only(h) {
        Xid::INVALID
    } else {
        h.xmax
    }
}

impl TableStore for HeapStore {
    fn create_storage(&self, w: &WriteCtx, rel: RelFileLocator) -> Result<()> {
        log_and_create(&self.wal, self.pool.smgr(), w.xid, rel, ForkNumber::Main)
    }

    fn storage_exists(&self, rel: RelFileLocator) -> Result<bool> {
        self.pool.smgr().exists(rel, ForkNumber::Main)
    }

    fn unlink_storage(&self, rel: RelFileLocator) -> Result<()> {
        self.pool.drop_relation_buffers(rel)?;
        self.hio.forget(rel)?;
        self.pool.smgr().unlink(rel)
    }

    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
        let data = form_tuple(&rel.desc, row, w, TupleFlags::default())?;
        self.hio.insert_tuple(rel.locator, w.xid, &data)
    }

    fn delete(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        wait: Option<&WaitCtx<'_>>,
        crosscheck: Option<&Snapshot>,
    ) -> Result<TmResult> {
        if !self.block_exists(rel.locator, tid)? {
            return Ok(TmResult::Invisible);
        }
        let snap_procs = SnapRunning {
            clog: &self.clog,
            snap,
        };
        let renv = self.run_env(&snap_procs)?;
        // Dropped last: the tuple lock is released after the latch and the pin (§5.2).
        let mut tuplock = TupleLockGuard::none();
        loop {
            // l1:
            let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
            let mut guard = buf.write()?;
            let Some(hdr) = Self::locate(&guard, rel.locator, tid)? else {
                return Ok(TmResult::Invisible);
            };
            match self.gate_update(renv, &hdr, snap, tid, w)? {
                Gate::Go => {}
                Gate::Done(res) => return Ok(res),
                Gate::Wait { xids, i_am_member } => {
                    let Some(wait) = wait else {
                        return Ok(TmResult::BeingModified { xmax: xids[0] });
                    };
                    drop(guard);
                    drop(buf);
                    self.wait_holders(
                        renv,
                        wait,
                        &mut tuplock,
                        rel,
                        tid,
                        &xids,
                        RowWait::Block,
                        i_am_member,
                    )?;
                    continue;
                }
            }
            if let Some(cc) = crosscheck
                && !visible(&self.clog, &hdr, cc)?
            {
                return Ok(TmResult::Updated {
                    ctid: tid,
                    xmax: Xid::INVALID,
                });
            }
            let cs = CriticalSection::enter(&self.pool);
            let stamped = stamp_xmax(&mut guard, tid.offset, hdr, w, true, None)
                .map_err(|e| cs.escalate(e))?;
            let end = self
                .log_delete(&guard, buf.tag(), tid.offset, &stamped, w)
                .map_err(|e| cs.escalate(e))?;
            guard.set_lsn(end.0);
            modstat::note_modified(rel.locator, 1);
            return Ok(TmResult::Ok);
        }
    }

    #[allow(clippy::too_many_lines)] // PostgreSQL heap_update の分岐を 1 関数で追えるようにしている。
    fn update(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        new_row: &[Datum],
        wait: Option<&WaitCtx<'_>>,
        crosscheck: Option<&Snapshot>,
    ) -> Result<UpdateOutcome> {
        let data = form_tuple(&rel.desc, new_row, w, TupleFlags { updated: true })?;
        let outcome = |result, lockmode| UpdateOutcome {
            result,
            new_tid: None,
            lockmode,
        };
        let mut lockmode = LockTupleMode::NoKeyExclusive;
        if !self.block_exists(rel.locator, tid)? {
            return Ok(outcome(TmResult::Invisible, lockmode));
        }
        let snap_procs = SnapRunning {
            clog: &self.clog,
            snap,
        };
        let renv = self.run_env(&snap_procs)?;
        let mut tuplock = TupleLockGuard::none();
        // Pages that turned out to have no room for the new version (the old page is excluded).
        let mut tried: Vec<BlockNumber> = vec![tid.block];
        loop {
            // l2:
            let obuf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
            let mut oguard = obuf.write()?;
            let Some(hdr) = Self::locate(&oguard, rel.locator, tid)? else {
                return Ok(outcome(TmResult::Invisible, lockmode));
            };
            lockmode = Self::update_lockmode(rel, &oguard, tid, new_row)?;
            let keys_updated = lockmode == LockTupleMode::Exclusive;
            match self.gate_update(renv, &hdr, snap, tid, w)? {
                Gate::Go => {}
                Gate::Done(res) => return Ok(outcome(res, lockmode)),
                Gate::Wait { xids, i_am_member } => {
                    let Some(wait) = wait else {
                        return Ok(outcome(TmResult::BeingModified { xmax: xids[0] }, lockmode));
                    };
                    drop(oguard);
                    drop(obuf);
                    self.wait_holders(
                        renv,
                        wait,
                        &mut tuplock,
                        rel,
                        tid,
                        &xids,
                        RowWait::Block,
                        i_am_member,
                    )?;
                    continue;
                }
            }
            if let Some(cc) = crosscheck
                && !visible(&self.clog, &hdr, cc)?
            {
                return Ok(outcome(
                    TmResult::Updated {
                        ctid: tid,
                        xmax: Xid::INVALID,
                    },
                    lockmode,
                ));
            }
            let done = |new_tid| UpdateOutcome {
                result: TmResult::Ok,
                new_tid: Some(new_tid),
                lockmode,
            };

            let mut room = hio::fits(oguard.page(), data.len());
            if !room
                && self
                    .hio
                    .try_prune_for_space(rel.locator, tid.block, &mut oguard)?
            {
                room = hio::fits(oguard.page(), data.len());
            }
            if room {
                let cs = CriticalSection::enter(&self.pool);
                let (new_tid, stamped) = hio::place_in_page(&mut oguard, tid.block, &data)
                    .and_then(|n| {
                        Ok((
                            n,
                            stamp_xmax(&mut oguard, tid.offset, hdr, w, keys_updated, Some(n))?,
                        ))
                    })
                    .map_err(|e| cs.escalate(e))?;
                let end = self
                    .log_update(
                        (&oguard, obuf.tag(), false),
                        None,
                        new_tid.offset,
                        tid.offset,
                        &stamped,
                        w,
                    )
                    .map_err(|e| cs.escalate(e))?;
                oguard.set_lsn(end.0);
                modstat::note_modified(rel.locator, 1);
                return Ok(done(new_tid));
            }

            // The new version goes to another page: latch both in block order (§5.9). While
            // the old page is unlatched somebody may change the old version's xmax; that is
            // detected below and handled by starting over (no wait needed for the comparison).
            let seen = RawXmax::of(&hdr);
            drop(oguard);
            let nbuf = self.hio.find_target(rel.locator, data.len(), &tried)?;
            let nblk = nbuf.tag().block;
            let (mut nguard, mut oguard);
            if nblk < tid.block {
                nguard = nbuf.write()?;
                oguard = obuf.write()?;
            } else {
                oguard = obuf.write()?;
                nguard = nbuf.write()?;
            }
            let Some(now) = Self::locate(&oguard, rel.locator, tid)? else {
                continue;
            };
            if RawXmax::of(&now) != seen {
                continue;
            }
            let mut room = hio::fits(nguard.page(), data.len());
            if !room
                && self
                    .hio
                    .try_prune_for_space(rel.locator, nblk, &mut nguard)?
            {
                room = hio::fits(nguard.page(), data.len());
            }
            if !room {
                let free = nguard.page().free_space();
                drop(nguard);
                drop(oguard);
                drop(nbuf);
                self.hio.note_no_fit(rel.locator, nblk, free);
                tried.push(nblk);
                continue;
            }
            let init = nguard.page().is_new();
            let cs = CriticalSection::enter(&self.pool);
            let (new_tid, stamped) = hio::place_in_page(&mut nguard, nblk, &data)
                .and_then(|n| {
                    Ok((
                        n,
                        stamp_xmax(&mut oguard, tid.offset, now, w, keys_updated, Some(n))?,
                    ))
                })
                .map_err(|e| cs.escalate(e))?;
            let end = self
                .log_update(
                    (&nguard, nbuf.tag(), init),
                    Some((&oguard, obuf.tag())),
                    new_tid.offset,
                    tid.offset,
                    &stamped,
                    w,
                )
                .map_err(|e| cs.escalate(e))?;
            nguard.set_lsn(end.0);
            oguard.set_lsn(end.0);
            drop(nguard);
            drop(oguard);
            drop(cs);
            self.hio.note_used(rel.locator, nblk)?;
            modstat::note_modified(rel.locator, 1);
            return Ok(done(new_tid));
        }
    }

    #[allow(clippy::too_many_lines)] // PostgreSQL heap_lock_tuple の分岐を 1 関数で追えるようにしている。
    fn lock_tuple(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        mode: LockTupleMode,
        policy: RowWait,
        follow_updates: bool,
        wait: &WaitCtx<'_>,
    ) -> Result<LockOutcome> {
        let done = |result| {
            Ok(LockOutcome {
                result,
                latest: None,
            })
        };
        let snap_procs = SnapRunning {
            clog: &self.clog,
            snap,
        };
        let renv = self.run_env(&snap_procs)?;
        let venv = VisEnv {
            clog: &self.clog,
            procs: renv.procs,
            multi: renv.multi,
        };
        let running = |x: Xid| renv.procs.is_in_progress(x);
        let xenv = XmaxEnv {
            me: w.xid,
            is_running: &running,
            multi: renv.multi,
        };
        // Dropped last, after the latch and the pin.
        let mut tuplock = TupleLockGuard::none();
        let mut cur = tid;
        let mut prior_xmax: Option<Xid> = None;
        let mut all_nonkey = true;
        loop {
            // l3:
            renv.multi.ensure_reserved()?;
            let traversed = prior_xmax.is_some();
            let gone = || {
                if let Some(px) = prior_xmax {
                    TmResult::Deleted { xmax: px }
                } else {
                    TmResult::Invisible
                }
            };
            if !self.block_exists(rel.locator, cur)? {
                return done(gone());
            }
            let buf = self.pool.read_buffer(tag(rel.locator, cur.block))?;
            let mut guard = buf.write()?;
            let Some(hdr) = Self::locate(&guard, rel.locator, cur)? else {
                return done(gone());
            };
            if let Some(px) = prior_xmax
                && !xmin_is_frozen(&hdr)
                && hdr.xmin != px
            {
                // The line pointer was reused: the chain broke.
                return done(TmResult::Deleted { xmax: px });
            }
            let mut being_modified = false;
            match satisfies_update_env(&venv, &hdr, snap, cur)? {
                TmResult::Invisible => return done(gone()),
                res @ (TmResult::SelfModified { .. } | TmResult::Deleted { .. }) => {
                    return done(res);
                }
                TmResult::Updated { ctid, xmax } => {
                    let keyshare_nonkey = mode == LockTupleMode::KeyShare
                        && hdr.infomask2 & crate::storage::heap::tuple::HEAP_KEYS_UPDATED == 0;
                    if !(follow_updates || keyshare_nonkey) {
                        return done(TmResult::Updated { ctid, xmax });
                    }
                    drop(guard);
                    drop(buf);
                    prior_xmax = Some(xmax);
                    cur = ctid;
                    all_nonkey &= keyshare_nonkey;
                    continue;
                }
                TmResult::Ok => {}
                TmResult::BeingModified { .. } => {
                    being_modified = true;
                }
                TmResult::WouldBlock => {
                    return Err(Error::internal("satisfies_update returned WouldBlock"));
                }
            }
            // 実行中だった更新者は、判定のあとでコミットしても `plan_lock` に任せず、必ず待つ
            // （任せるとコミット済みの更新者の xmax を上書きする）。待ったあとは最初からやり直す。
            let decision = match decode_xmax(&hdr)? {
                XmaxState::Updater { xid, .. } if being_modified => LockDecision::Wait {
                    xids: vec![xid],
                    i_am_member: false,
                },
                _ => plan_lock(&hdr, mode, &xenv)?,
            };
            let stamped = match decision {
                LockDecision::AlreadyHeld => hdr,
                LockDecision::Write(write) => {
                    let held = mode_from_bits(write.infomask, write.infomask2)?;
                    let cs = CriticalSection::enter(&self.pool);
                    let stamped = stamp_lock(&mut guard, cur.offset, hdr, &write)
                        .map_err(|e| cs.escalate(e))?;
                    let end = self
                        .log_lock(&guard, buf.tag(), cur.offset, &stamped, held, w)
                        .map_err(|e| cs.escalate(e))?;
                    guard.set_lsn(end.0);
                    stamped
                }
                LockDecision::Wait { xids, i_am_member } => {
                    drop(guard);
                    drop(buf);
                    match self.wait_holders(
                        renv,
                        wait,
                        &mut tuplock,
                        rel,
                        cur,
                        &xids,
                        policy,
                        i_am_member,
                    )? {
                        WaitResult::WouldBlock => return done(TmResult::WouldBlock),
                        WaitResult::Waited => continue,
                    }
                }
            };
            // Done. Decode the locked version while the latch is held.
            let latest = if traversed && !(mode == LockTupleMode::KeyShare && all_nonkey) {
                let bytes = guard
                    .page()
                    .item(cur.offset)
                    .map_err(|e| page_error(e, rel.locator, cur.block))?;
                Some(HeapTuple {
                    tid: cur,
                    xmin: stamped.xmin,
                    xmax: deleter_xmax(&stamped),
                    cmin: stamped.cmin,
                    cmax: stamped.cmax,
                    row: deform_tuple(&rel.desc, bytes)?,
                })
            } else {
                None
            };
            drop(guard);
            drop(buf);
            drop(tuplock);
            return Ok(LockOutcome {
                result: TmResult::Ok,
                latest,
            });
        }
    }

    fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
        let nblocks = self.pool.nblocks(rel.locator, ForkNumber::Main)?;
        Ok(HeapScan::new(rel.clone(), snap.clone(), nblocks))
    }

    fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        scan.next_tuple(&self.pool, &self.clog, self.prune_ctx().as_ref())
    }

    fn fetch(&self, rel: &RelHandle, snap: &Snapshot, tid: Tid) -> Result<Option<HeapTuple>> {
        if !self.block_exists(rel.locator, tid)? {
            return Ok(None);
        }
        let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
        let mut guard = buf.read()?;
        if let Some(ctx) = self.prune_ctx() {
            guard = prune_before_read(&ctx, &self.pool, &buf, guard)?;
        }
        let page = &*guard;
        if page.is_new() || tid.offset > page.max_offset() {
            return Ok(None);
        }
        let id = page
            .item_id(tid.offset)
            .map_err(|e| page_error(e, rel.locator, tid.block))?;
        if id.flags != LpFlags::Normal {
            return Ok(None);
        }
        let bytes = page
            .item(tid.offset)
            .map_err(|e| page_error(e, rel.locator, tid.block))?;
        decode_if_visible(bytes, tid, rel, snap, &self.clog)
    }

    fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan> {
        let nblocks = self.pool.nblocks(rel.locator, ForkNumber::Main)?;
        // SnapshotAny: `visible` returns true for every LP_NORMAL tuple.
        let any = Snapshot {
            xmin: Xid::INVALID,
            xmax: Xid::INVALID,
            xip: Vec::new(),
            curcid: u32::MAX,
            own_xid: None,
        };
        Ok(HeapScan::new(rel.clone(), any, nblocks))
    }

    fn tuple_state(&self, t: &HeapTuple, own: Option<Xid>) -> Result<TupleState> {
        let none = NoneRunning;
        let renv = self.run_env(&none)?;
        let venv = VisEnv {
            clog: &self.clog,
            procs: renv.procs,
            multi: renv.multi,
        };
        // `HeapTuple.xmax` is the deleter / updater only (a lock holder is `INVALID`).
        classify_env(&venv, t.xmin, t.xmax, false, own)
    }

    fn fetch_dirty(&self, rel: &RelHandle, own: Option<Xid>, tid: Tid) -> Result<DirtyResult> {
        if !self.block_exists(rel.locator, tid)? {
            return Ok(DirtyResult::Invisible);
        }
        let hdr = {
            let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
            let guard = buf.read()?;
            let page = &*guard;
            if page.is_new() || tid.offset > page.max_offset() {
                return Ok(DirtyResult::Invisible);
            }
            let id = page
                .item_id(tid.offset)
                .map_err(|e| page_error(e, rel.locator, tid.block))?;
            if id.flags != LpFlags::Normal {
                return Ok(DirtyResult::Invisible);
            }
            let bytes = page
                .item(tid.offset)
                .map_err(|e| page_error(e, rel.locator, tid.block))?;
            TupleHeader::read(bytes)?
        };
        // Latch and pin are released; only now consult the clog and the registry.
        let none = NoneRunning;
        let renv = self.run_env(&none)?;
        let venv = VisEnv {
            clog: &self.clog,
            procs: renv.procs,
            multi: renv.multi,
        };
        satisfies_dirty_env(&venv, &hdr, own)
    }

    fn nblocks(&self, rel: &RelHandle) -> Result<u32> {
        self.pool.nblocks(rel.locator, ForkNumber::Main)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::RelFileNumber;
    use crate::storage::testing::{TestStorage, TestStorageOptions};
    use crate::storage::{AttrDesc, TupleDesc};
    use crate::txn::Xid;
    use crate::txn::clog::XidStatus;
    use crate::types::oid;
    use crate::wal::Lsn;

    struct Fixture {
        ts: TestStorage,
        rel: RelHandle,
    }

    fn fixture() -> Fixture {
        let ts = TestStorage::with_options(TestStorageOptions {
            nframes: 32,
            next_xid: Xid(3),
            ..TestStorageOptions::default()
        })
        .unwrap();
        let stack = &ts.stack;
        let locator = RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(16384),
        };
        stack
            .heap
            .create_storage(
                &WriteCtx {
                    xid: Xid(3),
                    cid: 0,
                },
                locator,
            )
            .unwrap();
        let rel = RelHandle {
            oid: 16384,
            locator,
            desc: Arc::new(TupleDesc {
                attrs: vec![
                    AttrDesc::from_type(oid::INT4),
                    AttrDesc::from_type(oid::TEXT),
                ],
            }),
            indexes: Arc::from([]),
            key_columns: Arc::from([]),
            ri: Arc::default(),
        };
        Fixture { ts, rel }
    }

    impl Fixture {
        fn xid(&self, x: u64, s: Option<XidStatus>) {
            self.ts.stack.clog.ensure_page_for(Xid(x));
            if let Some(s) = s {
                self.ts.stack.clog.set_status(Xid(x), s).unwrap();
            }
        }

        fn snap(own: Option<u64>, xmax: u64, curcid: u32) -> Snapshot {
            Snapshot {
                xmin: Xid(3),
                xmax: Xid(xmax),
                xip: vec![],
                curcid,
                own_xid: own.map(Xid),
            }
        }

        fn all(&self, snap: &Snapshot) -> Vec<HeapTuple> {
            let h = &self.ts.stack.heap;
            let mut s = h.begin_scan(&self.rel, snap).unwrap();
            let mut v = vec![];
            while let Some(t) = h.scan_next(&mut s).unwrap() {
                v.push(t);
            }
            v
        }
    }

    fn row(i: i32, s: &str) -> Vec<Datum> {
        vec![Datum::Int4(i), Datum::Text(s.into())]
    }

    fn w(x: u64, cid: u32) -> WriteCtx {
        WriteCtx { xid: Xid(x), cid }
    }

    #[test]
    fn insert_scan_and_visibility() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.ts.stack.heap;
        let t1 = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let t2 = h.insert(&f.rel, &w(4, 0), &row(2, "b")).unwrap();
        assert_eq!(
            t1,
            Tid {
                block: 0,
                offset: 1
            }
        );
        assert_eq!(
            t2,
            Tid {
                block: 0,
                offset: 2
            }
        );
        h.insert(&f.rel, &w(5, 0), &row(3, "c")).unwrap();

        let got = f.all(&Fixture::snap(None, 6, 0));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].row, row(1, "a"));
        assert_eq!(got[0].xmin, Xid(4));
        // The writer sees its own row.
        assert_eq!(f.all(&Fixture::snap(Some(5), 6, 1)).len(), 3);
        // ... but not in the same command.
        assert_eq!(f.all(&Fixture::snap(Some(5), 6, 0)).len(), 2);
        assert_eq!(f.ts.stack.pool.pinned_frames(), 0);
    }

    #[test]
    fn delete_and_double_delete() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.ts.stack.heap;
        let t = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        assert_eq!(
            h.delete(&f.rel, &w(5, 0), &s, t, None, None).unwrap(),
            TmResult::Ok
        );
        let s1 = Fixture::snap(Some(5), 6, 1);
        assert!(f.all(&s1).is_empty());
        assert_eq!(f.all(&Fixture::snap(None, 6, 0)).len(), 1);
        assert_eq!(
            h.delete(&f.rel, &w(5, 0), &s, t, None, None).unwrap(),
            TmResult::SelfModified { cmax: 0 }
        );
        assert_eq!(
            h.delete(&f.rel, &w(5, 1), &s1, t, None, None).unwrap(),
            TmResult::Invisible
        );
        let bogus = Tid {
            block: 9,
            offset: 1,
        };
        assert_eq!(
            h.delete(&f.rel, &w(5, 1), &s1, bogus, None, None).unwrap(),
            TmResult::Invisible
        );
    }

    #[test]
    fn update_same_page_chains_ctid() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.ts.stack.heap;
        let t = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        let out = h
            .update(&f.rel, &w(5, 0), &s, t, &row(1, "z"), None, None)
            .unwrap();
        assert_eq!(out.result, TmResult::Ok);
        let nt = out.new_tid.unwrap();
        assert_eq!(
            nt,
            Tid {
                block: 0,
                offset: 2
            }
        );
        let s1 = Fixture::snap(Some(5), 6, 1);
        let rows = f.all(&s1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row, row(1, "z"));
        assert_eq!(rows[0].tid, nt);
        // Old version is gone for the writer, present for others.
        assert!(h.fetch(&f.rel, &s1, t).unwrap().is_none());
        let other = Fixture::snap(None, 6, 0);
        assert_eq!(
            h.fetch(&f.rel, &other, t).unwrap().unwrap().row,
            row(1, "a")
        );
        assert!(h.fetch(&f.rel, &other, nt).unwrap().is_none());
    }

    #[test]
    fn many_rows_span_blocks_and_update_moves_to_new_page() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.ts.stack.heap;
        let big = "x".repeat(1000);
        let mut tids = vec![];
        for i in 0..40 {
            tids.push(h.insert(&f.rel, &w(4, 0), &row(i, &big)).unwrap());
        }
        assert!(tids.last().unwrap().block >= 4);
        let snap = Fixture::snap(None, 6, 0);
        assert_eq!(f.all(&snap).len(), 40);
        // Last page is full-ish; update the first row to a bigger value and
        // make sure it lands somewhere valid.
        let s = Fixture::snap(Some(5), 6, 0);
        let out = h
            .update(
                &f.rel,
                &w(5, 0),
                &s,
                tids[0],
                &row(0, &"y".repeat(5000)),
                None,
                None,
            )
            .unwrap();
        assert_eq!(out.result, TmResult::Ok);
        let s1 = Fixture::snap(Some(5), 6, 1);
        let rows = f.all(&s1);
        assert_eq!(rows.len(), 40);
        assert!(
            rows.iter()
                .any(|r| r.row[1] == Datum::Text("y".repeat(5000)))
        );
        assert_eq!(f.ts.stack.pool.pinned_frames(), 0);
    }

    #[test]
    fn scan_all_returns_dead_versions_and_ignores_new_blocks() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, Some(XidStatus::Aborted));
        f.xid(6, None);
        let h = &f.ts.stack.heap;
        let t1 = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        h.insert(&f.rel, &w(5, 0), &row(2, "aborted")).unwrap();
        h.insert(&f.rel, &w(6, 0), &row(3, "own")).unwrap();
        let snap = Fixture::snap(Some(6), 7, 5);
        h.delete(&f.rel, &w(6, 1), &snap, t1, None, None).unwrap();
        let mut s = h.begin_scan_all(&f.rel).unwrap();
        for i in 0..30 {
            h.insert(&f.rel, &w(6, 1), &row(i, &"q".repeat(900)))
                .unwrap();
        }
        let mut got = vec![];
        while let Some(t) = h.scan_next(&mut s).unwrap() {
            got.push(t);
        }
        assert!(h.nblocks(&f.rel).unwrap() > 1);
        assert!(got.len() >= 3);
        assert_eq!(got[0].xmax, Xid(6));
        assert_eq!(got[1].xmin, Xid(5));
        assert_eq!(got[2].xmax, Xid::INVALID);
        let own = Some(Xid(6));
        assert_eq!(
            h.tuple_state(&got[0], own).unwrap(),
            TupleState::DeletedBySelf
        );
        assert_eq!(
            h.tuple_state(&got[1], own).unwrap(),
            TupleState::InsertAborted
        );
        assert_eq!(h.tuple_state(&got[2], own).unwrap(), TupleState::Live);
        // blocks added after begin_scan_all are not read
        assert!(got.len() < 33);
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn fetch_dirty_ignores_command_ids_and_bad_tids() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, Some(XidStatus::Aborted));
        f.xid(6, None);
        f.xid(7, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        let own = Some(Xid(6));
        let a = h.insert(&f.rel, &w(4, 0), &row(1, "c")).unwrap();
        let b = h.insert(&f.rel, &w(5, 0), &row(2, "ab")).unwrap();
        let c = h.insert(&f.rel, &w(6, 9), &row(3, "own")).unwrap();
        let d = h.insert(&f.rel, &w(4, 0), &row(4, "del")).unwrap();
        let e = h.insert(&f.rel, &w(4, 0), &row(5, "delc")).unwrap();
        h.delete(
            &f.rel,
            &w(6, 0),
            &Fixture::snap(Some(6), 8, 0),
            d,
            None,
            None,
        )
        .unwrap();
        h.delete(
            &f.rel,
            &w(7, 0),
            &Fixture::snap(Some(7), 8, 0),
            e,
            None,
            None,
        )
        .unwrap();
        let fd = |tid| h.fetch_dirty(&f.rel, own, tid).unwrap();
        assert_eq!(fd(a), DirtyResult::Visible);
        assert_eq!(fd(b), DirtyResult::Invisible);
        assert_eq!(fd(c), DirtyResult::Visible);
        assert_eq!(fd(d), DirtyResult::Invisible);
        assert_eq!(fd(e), DirtyResult::Invisible);
        for tid in [
            Tid {
                block: 9,
                offset: 1,
            },
            Tid {
                block: 0,
                offset: 0,
            },
            Tid {
                block: 0,
                offset: 200,
            },
        ] {
            assert_eq!(fd(tid), DirtyResult::Invisible);
        }
    }

    #[test]
    fn scan_ignores_blocks_added_after_begin() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let snap = Fixture::snap(None, 6, 0);
        let mut s = h.begin_scan(&f.rel, &snap).unwrap();
        for i in 0..30 {
            h.insert(&f.rel, &w(4, 0), &row(i, &"q".repeat(900)))
                .unwrap();
        }
        let mut n = 0;
        while h.scan_next(&mut s).unwrap().is_some() {
            n += 1;
        }
        assert!(n <= 31);
        assert!(n >= 1);
    }

    #[test]
    fn too_big_row_leaves_page_untouched() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        let mut seed: u32 = 12345;
        let noisy: String = (0..9000)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from(b'a' + ((seed >> 16) % 26) as u8)
            })
            .collect();
        let err = h.insert(&f.rel, &w(4, 0), &row(1, &noisy)).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            f.ts.stack
                .pool
                .nblocks(f.rel.locator, ForkNumber::Main)
                .unwrap(),
            0
        );
    }

    #[test]
    fn unlink_forgets_hint_and_keeps_leftover() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        assert!(h.storage_exists(f.rel.locator).unwrap());
        h.unlink_storage(f.rel.locator).unwrap();
        assert!(h.storage_exists(f.rel.locator).unwrap());
        assert_eq!(
            f.ts.stack
                .pool
                .nblocks(f.rel.locator, ForkNumber::Main)
                .unwrap(),
            0
        );
    }

    // ----- WAL and REDO -----------------------------------------------------

    use crate::debug_knobs::DebugKnobs;
    use crate::error::Severity;
    use crate::storage::heap::wal as hwal;
    use crate::storage::smgr_wal;
    use crate::storage::vfs::CrashMode;
    use crate::wal::{DecodedRecord, SEG_HEADER_SIZE, WalReader};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    fn wal_records(ts: &TestStorage) -> Vec<DecodedRecord> {
        ts.wal().flush(ts.wal().insert_lsn()).unwrap();
        let cfg = *ts.wal().config();
        let start = Lsn(u64::from(cfg.segment_size) + SEG_HEADER_SIZE);
        let mut r = WalReader::open(Arc::new(ts.vfs.clone()), &cfg, start);
        let mut out = Vec::new();
        while let Some(rec) = r.next().unwrap() {
            out.push(rec);
        }
        out
    }

    fn redo_ctx(ts: &TestStorage, knobs: DebugKnobs) -> crate::wal::RedoCtx {
        crate::wal::RedoCtx {
            pool: Arc::clone(ts.pool()),
            smgr: Arc::clone(ts.smgr()),
            ext: Box::new(()),
            invalid: Mutex::new(crate::wal::InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs,
        }
    }

    fn replay(ts: &TestStorage, recs: &[DecodedRecord], knobs: DebugKnobs) {
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(ts, knobs);
        for rec in recs {
            match rec.rmgr {
                RmgrId::Smgr => smgr_wal::redo(&ctx, rec).unwrap(),
                RmgrId::Heap => hwal::redo(&ctx, rec).unwrap(),
                _ => {}
            }
        }
        ts.assert_clean();
    }

    fn page_images(ts: &TestStorage, rel: RelFileLocator) -> Vec<Vec<u8>> {
        let n = ts.pool().nblocks(rel, ForkNumber::Main).unwrap();
        (0..n)
            .map(|b| {
                let buf = ts.pool().read_buffer(tag(rel, b)).unwrap();
                let g = buf.read().unwrap();
                g.0.to_vec()
            })
            .collect()
    }

    /// Inserts across several pages, then deletes, updates in place and updates to
    /// another page. Returns the TIDs of the inserted rows.
    fn workload(f: &Fixture) -> Vec<Tid> {
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        let big = "x".repeat(1000);
        let tids: Vec<Tid> = (0..20)
            .map(|i| h.insert(&f.rel, &w(4, 0), &row(i, &big)).unwrap())
            .collect();
        let s = Fixture::snap(Some(5), 6, 0);
        assert_eq!(
            h.delete(&f.rel, &w(5, 0), &s, tids[3], None, None).unwrap(),
            TmResult::Ok
        );
        // Same page: a page is full of 1000-byte rows, so shrink the row.
        let out = h
            .update(&f.rel, &w(5, 0), &s, tids[4], &row(4, "s"), None, None)
            .unwrap();
        assert_eq!(out.new_tid.unwrap().block, tids[4].block);
        // Another page: grow the row past the free space of its page.
        let out = h
            .update(
                &f.rel,
                &w(5, 0),
                &s,
                tids[5],
                &row(5, &"y".repeat(2000)),
                None,
                None,
            )
            .unwrap();
        assert_ne!(out.new_tid.unwrap().block, tids[5].block);
        tids
    }

    fn visible(ts: &TestStorage, rel: &RelHandle) -> Vec<(Tid, Vec<Datum>)> {
        let snap = Fixture::snap(None, 6, 0);
        let h = ts.heap();
        let mut s = h.begin_scan(rel, &snap).unwrap();
        let mut v = vec![];
        while let Some(t) = h.scan_next(&mut s).unwrap() {
            v.push((t.tid, t.row));
        }
        v
    }

    #[test]
    fn operations_write_one_heap_record_each() {
        let f = fixture();
        workload(&f);
        let heap: Vec<_> = wal_records(&f.ts)
            .into_iter()
            .filter(|r| r.rmgr == RmgrId::Heap)
            .collect();
        let kind = |r: &DecodedRecord| r.info & 0x70;
        let n = |k| heap.iter().filter(|r| kind(r) == k).count();
        assert_eq!(n(hwal::HEAP_INSERT), 20);
        assert_eq!(n(hwal::HEAP_DELETE), 1);
        assert_eq!(n(hwal::HEAP_UPDATE), 2);
        // Pages are created by INSERT with INIT_PAGE: one per block.
        let inits = heap
            .iter()
            .filter(|r| r.info & hwal::HEAP_INIT_PAGE != 0)
            .count();
        let nblocks =
            f.ts.pool()
                .nblocks(f.rel.locator, ForkNumber::Main)
                .unwrap();
        assert!(inits >= nblocks as usize);
        for r in &heap {
            assert_eq!(
                r.xid,
                if r.info & 0x70 == hwal::HEAP_INSERT {
                    Xid(4)
                } else {
                    Xid(5)
                }
            );
        }
        let ins = heap.iter().find(|r| kind(r) == hwal::HEAP_INSERT).unwrap();
        assert_eq!(ins.blocks.len(), 1);
        assert_eq!(ins.main.len(), 4);
        assert_eq!(u16::from_le_bytes([ins.main[0], ins.main[1]]), 1);
        // The tuple in the record carries its own TID.
        let hdr = TupleHeader::read(&ins.blocks[0].data).unwrap();
        assert_eq!(
            hdr.ctid,
            Tid {
                block: 0,
                offset: 1
            }
        );
        // The cross-page UPDATE has two block references, the in-page one has one.
        let ups: Vec<_> = heap
            .iter()
            .filter(|r| kind(r) == hwal::HEAP_UPDATE)
            .collect();
        let nblks: Vec<_> = ups.iter().map(|r| r.blocks.len()).collect();
        assert_eq!(nblks, vec![1, 2]);
        assert!(
            hwal::HeapUpdateMain::decode(&ups[0].main)
                .unwrap()
                .same_page
        );
        assert!(
            !hwal::HeapUpdateMain::decode(&ups[1].main)
                .unwrap()
                .same_page
        );
        f.ts.assert_clean();
    }

    #[test]
    fn pages_carry_the_lsn_of_the_last_record_and_wal_is_flushed_before_data() {
        let f = fixture();
        workload(&f);
        let recs = wal_records(&f.ts);
        let last = recs.iter().rfind(|r| r.rmgr == RmgrId::Heap).unwrap();
        let buf =
            f.ts.pool()
                .read_buffer(tag(f.rel.locator, last.blocks[0].tag.block))
                .unwrap();
        assert_eq!(buf.read().unwrap().lsn(), last.end.0);
        // Writing every page out goes through flush_to (assert_wal_before_data is on).
        f.ts.pool().flush_all_for_checkpoint().unwrap();
    }

    #[test]
    fn redo_rebuilds_identical_pages_after_a_crash() {
        let f = fixture();
        workload(&f);
        let recs = wal_records(&f.ts);
        let want = page_images(&f.ts, f.rel.locator);
        let want_rows = visible(&f.ts, &f.rel);
        assert!(want.len() >= 3);

        let ts2 = f.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        ts2.clog().ensure_page_for(Xid(5));
        ts2.clog().set_status(Xid(4), XidStatus::Committed).unwrap();
        ts2.clog().set_status(Xid(5), XidStatus::Committed).unwrap();
        replay(&ts2, &recs, DebugKnobs::default());
        assert_eq!(page_images(&ts2, f.rel.locator), want);
        assert_eq!(visible(&ts2, &f.rel), want_rows);

        // A second replay is a no-op: every page is already at the record's LSN.
        replay(&ts2, &recs, DebugKnobs::default());
        assert_eq!(page_images(&ts2, f.rel.locator), want);
    }

    #[test]
    fn redo_skips_pages_that_are_already_ahead() {
        let f = fixture();
        workload(&f);
        let recs = wal_records(&f.ts);
        f.ts.pool().flush_all_for_checkpoint().unwrap();
        f.ts.smgr().sync_pending().unwrap();
        let want = page_images(&f.ts, f.rel.locator);
        let ts2 = f.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        // The pages are on disk with their final LSNs; replay must not touch them
        // (re-adding the tuples would otherwise hit the offset check and panic).
        replay(&ts2, &recs, DebugKnobs::default());
        assert_eq!(page_images(&ts2, f.rel.locator).len(), want.len());
    }

    #[test]
    fn redo_rejects_records_that_do_not_match_the_page() {
        let f = fixture();
        workload(&f);
        let recs = wal_records(&f.ts);
        let ts2 = f.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        ts2.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts2, DebugKnobs::default());
        for r in recs.iter().filter(|r| r.rmgr == RmgrId::Smgr) {
            smgr_wal::redo(&ctx, r).unwrap();
        }
        let ins: Vec<_> = recs
            .iter()
            .filter(|r| r.rmgr == RmgrId::Heap && r.info & 0x70 == hwal::HEAP_INSERT)
            .collect();
        hwal::redo(&ctx, ins[0]).unwrap();
        // Replaying the second row at a wrong offset number.
        let mut bad = ins[1].clone();
        bad.main[0] = 9;
        let e = hwal::redo(&ctx, &bad).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        // Wrong shapes.
        let mut bad = ins[1].clone();
        bad.main.push(0);
        assert_eq!(
            hwal::redo(&ctx, &bad).unwrap_err().severity,
            Severity::Panic
        );
        let mut bad = ins[1].clone();
        bad.info |= hwal::HEAP_INIT_PAGE;
        assert_eq!(
            hwal::redo(&ctx, &bad).unwrap_err().severity,
            Severity::Panic
        );
        // A DELETE of a line pointer that does not exist.
        let del = recs
            .iter()
            .find(|r| r.rmgr == RmgrId::Heap && r.info & 0x70 == hwal::HEAP_DELETE)
            .unwrap();
        let mut m = hwal::HeapDeleteMain::decode(&del.main).unwrap();
        m.offnum = 99;
        let mut bad = del.clone();
        bad.main = m.encode().to_vec();
        bad.blocks[0].tag.block = 0;
        bad.end = Lsn(bad.end.0 + 1_000_000);
        assert_eq!(
            hwal::redo(&ctx, &bad).unwrap_err().severity,
            Severity::Panic
        );
        ts2.assert_clean();
    }

    #[test]
    fn update_to_a_lower_block_latches_in_block_order() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, Some(XidStatus::Committed));
        let h = &f.ts.stack.heap;
        let big = "x".repeat(1000);
        // Page 0 holds two small rows (~6000 bytes free); the big row goes to page 1.
        for i in 0..2 {
            h.insert(&f.rel, &w(4, 0), &row(i, &big)).unwrap();
        }
        let old = h
            .insert(&f.rel, &w(4, 0), &row(2, &"m".repeat(6200)))
            .unwrap();
        assert_eq!(old.block, 1);
        h.hio.note_used(f.rel.locator, 0).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        let out = h
            .update(
                &f.rel,
                &w(5, 0),
                &s,
                old,
                &row(2, &"n".repeat(5000)),
                None,
                None,
            )
            .unwrap();
        assert_eq!(out.new_tid.unwrap().block, 0);
        let recs = wal_records(&f.ts);
        let up = recs
            .iter()
            .find(|r| r.rmgr == RmgrId::Heap && r.info & 0x70 == hwal::HEAP_UPDATE)
            .unwrap();
        assert_eq!(up.blocks[0].tag.block, 0);
        assert_eq!(up.blocks[1].tag.block, old.block);
        // And it replays.
        let want = page_images(&f.ts, f.rel.locator);
        let ts2 = f.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        replay(&ts2, &recs, DebugKnobs::default());
        assert_eq!(page_images(&ts2, f.rel.locator), want);
        f.ts.assert_clean();
    }

    #[test]
    fn create_storage_is_logged_with_the_callers_xid() {
        let f = fixture();
        let recs = wal_records(&f.ts);
        let c = recs.iter().find(|r| r.rmgr == RmgrId::Smgr).unwrap();
        assert_eq!(c.xid, Xid(3));
    }
}
