//! Choosing the page for a new tuple (`m5/03` §5.7.2) and the insert itself.
//!
//! The order is: hint (last page used) -> free space map -> last page -> extend. The
//! FSM is derived data and may be missing or wrong; the caller checks the page it is
//! handed under the exclusive latch (`m3.md` §5.1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::prune::{HeapEnv, RunningXids, candidate, prune_opt};
use super::tuple::TupleHeader;
use super::wal::{HEAP_INIT_PAGE, HEAP_INSERT};
use crate::error::{Error, Result};
use crate::storage::MAXALIGN;
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard, PinnedBuffer};
use crate::storage::fsm::FreeSpaceMap;
use crate::storage::page::{Page, PageError};
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator};
use crate::txn::Xid;
use crate::txn::clog::Clog;
use crate::txn::multixact::MultiXactTable;
use crate::types::Tid;
use crate::util::sync;
use crate::wal::{Lsn, RecordBuilder, RegFlags, RmgrId, Wal};

pub fn tag(rel: RelFileLocator, block: BlockNumber) -> BufferTag {
    BufferTag {
        rel,
        fork: ForkNumber::Main,
        block,
    }
}

/// Converts page-level damage to `XX001`.
pub fn page_error(e: PageError, rel: RelFileLocator, block: BlockNumber) -> Error {
    Error::corrupted(format!(
        "invalid page in block {block} of relation {}/{}/{}",
        rel.spc_oid, rel.db_oid, rel.rel_number.0
    ))
    .with_detail(format!("{e:?}"))
}

/// In-memory "last page used for inserts" per relation.
#[derive(Debug, Default)]
pub struct InsertHints {
    map: Mutex<HashMap<RelFileLocator, BlockNumber>>,
}

impl InsertHints {
    pub fn get(&self, rel: RelFileLocator) -> Result<Option<BlockNumber>> {
        Ok(sync::lock(&self.map)?.get(&rel).copied())
    }

    pub fn set(&self, rel: RelFileLocator, block: BlockNumber) -> Result<()> {
        sync::lock(&self.map)?.insert(rel, block);
        Ok(())
    }

    pub fn forget(&self, rel: RelFileLocator) -> Result<()> {
        sync::lock(&self.map)?.remove(&rel);
        Ok(())
    }
}

/// Whether a tuple of `len` bytes fits on `page` (a new page counts as empty).
/// `Page::free_space` already accounts for `MAX_HEAP_TUPLES_PER_PAGE` and for
/// reusable `LP_UNUSED` line pointers.
pub fn fits(page: &Page, len: usize) -> bool {
    page.is_new() || page.free_space() >= len.div_ceil(MAXALIGN) * MAXALIGN
}

/// Puts `data` on the page and sets its `ctid` to its own TID. The caller
/// has checked [`fits`] and is inside a critical section; nothing before
/// the first `page_mut` can fail.
pub fn place_in_page(
    guard: &mut PageWriteGuard<'_>,
    block: BlockNumber,
    data: &[u8],
) -> Result<Tid> {
    let page = guard.page_mut();
    if page.is_new() {
        page.init_heap();
    }
    let off = page
        .add_item(data)
        .ok_or_else(|| Error::internal("tuple does not fit on the page chosen for it"))?;
    let tid = Tid { block, offset: off };
    let item = page
        .item_mut(off)
        .map_err(|_| Error::internal("freshly added tuple is unreadable"))?;
    let mut hdr = TupleHeader::read(item)?;
    hdr.ctid = tid;
    hdr.write(item);
    Ok(tid)
}

/// Writes the `HEAP_INSERT` record. The caller is in a critical section, holds the
/// latch and has already called [`place_in_page`] (which returned `tid`).
fn log_insert(
    wal: &Wal,
    guard: &PageWriteGuard<'_>,
    tag: BufferTag,
    tid: Tid,
    init: bool,
    xid: Xid,
) -> Result<Lsn> {
    let info = HEAP_INSERT | if init { HEAP_INIT_PAGE } else { 0 };
    let mut rec = RecordBuilder::new(RmgrId::Heap, info, xid);
    let flags = if init {
        RegFlags::STANDARD | RegFlags::WILL_INIT
    } else {
        RegFlags::STANDARD
    };
    let b = rec.register_block(tag, guard.page(), flags);
    let bytes = guard
        .page()
        .item(tid.offset)
        .map_err(|_| Error::internal("freshly added tuple is unreadable"))?;
    rec.block_data(b, bytes);
    let mut main = [0u8; 4];
    main[0..2].copy_from_slice(&tid.offset.to_le_bytes());
    rec.main_data(&main);
    Ok(wal.insert(rec)?.end)
}

/// How many times `find_target` asks the FSM before giving up and trying the last page.
const FSM_ATTEMPTS: usize = 8;
/// With this many pages already tried, `find_target` extends the relation.
const MAX_TRIED: usize = 8;

type VacuumEnv = (Arc<dyn RunningXids>, Arc<MultiXactTable>);

/// Insert-target selection for the heap (`m5/03` §4.1, §5.7.2): the hint of the last
/// page used, the FSM, opportunistic pruning when a page lacks room.
#[derive(Debug)]
pub struct HioCtx {
    pool: Arc<BufferPool>,
    wal: Arc<Wal>,
    clog: Arc<Clog>,
    fsm: Arc<FreeSpaceMap>,
    hints: InsertHints,
    /// The running-XID registry and MultiXact table, set once by `HeapStore::set_vacuum_env`.
    /// Until then no pruning is attempted.
    env: OnceLock<VacuumEnv>,
}

impl HioCtx {
    pub fn new(
        pool: Arc<BufferPool>,
        fsm: Arc<FreeSpaceMap>,
        clog: Arc<Clog>,
        wal: Arc<Wal>,
    ) -> HioCtx {
        HioCtx {
            pool,
            wal,
            clog,
            fsm,
            hints: InsertHints::default(),
            env: OnceLock::new(),
        }
    }

    /// Hands over the pruning environment (once). `false` if already set.
    pub fn set_env(&self, procs: Arc<dyn RunningXids>, multixact: Arc<MultiXactTable>) -> bool {
        self.env.set((procs, multixact)).is_ok()
    }

    pub fn fsm(&self) -> &Arc<FreeSpaceMap> {
        &self.fsm
    }

    /// Returns a candidate page pinned, without a latch. `tried` are the blocks that
    /// turned out not to have room. The answer may be stale by the time the caller
    /// latches the page; the caller re-checks [`fits`].
    pub fn find_target(
        &self,
        rel: RelFileLocator,
        tuple_len: usize,
        tried: &[BlockNumber],
    ) -> Result<PinnedBuffer> {
        let need = tuple_len.div_ceil(MAXALIGN) * MAXALIGN;
        let nblocks = self.pool.nblocks(rel, ForkNumber::Main)?;
        if tried.len() >= MAX_TRIED {
            return self.pool.extend(rel, ForkNumber::Main);
        }
        if let Some(b) = self.hints.get(rel)?
            && b < nblocks
            && !tried.contains(&b)
        {
            return self.pool.read_buffer(tag(rel, b));
        }
        for _ in 0..FSM_ATTEMPTS {
            let Some(b) = self.fsm.search(rel, need)? else {
                break;
            };
            if b >= nblocks {
                // Left over from a truncation or damage: drop the entry.
                self.fsm.record(rel, b, 0)?;
                continue;
            }
            if tried.contains(&b) {
                continue;
            }
            return self.pool.read_buffer(tag(rel, b));
        }
        if let Some(last) = nblocks.checked_sub(1)
            && !tried.contains(&last)
        {
            return self.pool.read_buffer(tag(rel, last));
        }
        self.pool.extend(rel, ForkNumber::Main)
    }

    /// The caller found that the page has no room (and has released its latch and pin):
    /// records the page's real free space in the FSM.
    pub fn note_no_fit(&self, rel: RelFileLocator, blk: BlockNumber, free_space: usize) {
        if let Err(e) = self.fsm.record(rel, blk, free_space) {
            // Only a Severity::Panic gets here; the insert goes on and the poisoned
            // pool stops the next operation.
            eprintln!("WARNING:  could not record free space of block {blk}: {e}");
        }
    }

    /// The tuple went into `blk`: it is the next insert target.
    pub fn note_used(&self, rel: RelFileLocator, blk: BlockNumber) -> Result<()> {
        self.hints.set(rel, blk)
    }

    /// Holding the exclusive latch of a page without room, tries opportunistic pruning
    /// (`m5/03` §5.2.3, VC-D7). `true` if the page changed.
    pub fn try_prune_for_space(
        &self,
        rel: RelFileLocator,
        blk: BlockNumber,
        guard: &mut PageWriteGuard<'_>,
    ) -> Result<bool> {
        let Some((procs, multixact)) = self.env.get() else {
            return Ok(false);
        };
        if self.pool.is_poisoned() {
            return Ok(false);
        }
        let next_xid = procs.next_xid();
        let Some(horizon) = candidate(guard.page(), procs.as_ref(), next_xid) else {
            return Ok(false);
        };
        let env = HeapEnv {
            clog: &self.clog,
            procs: procs.as_ref(),
            multixact,
        };
        prune_opt(
            &env, &self.pool, &self.wal, rel, blk, guard, horizon, next_xid,
        )
    }

    /// `unlink_storage`: forgets the relation's hint.
    pub fn forget(&self, rel: RelFileLocator) -> Result<()> {
        self.hints.forget(rel)
    }

    /// Inserts `data` on a page with room and logs it (`m3.md` §5.1, `m5/03` §5.7.2).
    /// No latch is held while the relation is extended or the FSM is updated.
    pub fn insert_tuple(&self, rel: RelFileLocator, xid: Xid, data: &[u8]) -> Result<Tid> {
        let mut tried: Vec<BlockNumber> = Vec::new();
        loop {
            let buf = self.find_target(rel, data.len(), &tried)?;
            let blk = buf.tag().block;
            let mut guard = buf.write()?;
            let mut room = fits(guard.page(), data.len());
            if !room && self.try_prune_for_space(rel, blk, &mut guard)? {
                room = fits(guard.page(), data.len());
            }
            if !room {
                let free = guard.page().free_space();
                drop(guard);
                drop(buf);
                self.note_no_fit(rel, blk, free);
                tried.push(blk);
                continue;
            }
            let init = guard.page().is_new();
            let cs = CriticalSection::enter(&self.pool);
            let tid = place_in_page(&mut guard, blk, data).map_err(|e| cs.escalate(e))?;
            let end = log_insert(&self.wal, &guard, buf.tag(), tid, init, xid)
                .map_err(|e| cs.escalate(e))?;
            guard.set_lsn(end.0);
            drop(guard);
            drop(cs);
            self.note_used(rel, blk)?;
            return Ok(tid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::testing::{TestStorage, test_rel};
    use crate::storage::vfs::Vfs as _;

    fn tuple(len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        TupleHeader {
            xmin: Xid(3),
            xmax: Xid::INVALID,
            cmin: 0,
            cmax: 0,
            ctid: Tid {
                block: 0,
                offset: 1,
            },
            infomask2: 0,
            infomask: 0,
            hoff: 40,
        }
        .write(&mut b);
        b
    }

    fn ctx(ts: &TestStorage) -> HioCtx {
        HioCtx::new(
            Arc::clone(ts.pool()),
            Arc::new(FreeSpaceMap::new(Arc::clone(ts.pool()))),
            Arc::clone(ts.clog()),
            Arc::clone(ts.wal()),
        )
    }

    fn setup() -> (TestStorage, RelFileLocator) {
        let ts = TestStorage::new();
        let rel = test_rel(7100);
        ts.create_rel(rel).unwrap();
        (ts, rel)
    }

    fn nblocks(ts: &TestStorage, rel: RelFileLocator) -> BlockNumber {
        ts.pool().nblocks(rel, ForkNumber::Main).unwrap()
    }

    #[test]
    fn inserts_fill_pages_then_extend() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        for _ in 0..20 {
            h.insert_tuple(rel, Xid(3), &tuple(1000)).unwrap();
        }
        // 8 tuples of 1000 bytes (+ line pointers) fit on a page.
        assert_eq!(nblocks(&ts, rel), 3);
        ts.assert_clean();
    }

    #[test]
    fn missing_or_destroyed_fsm_still_inserts_without_bloat() {
        let (ts, rel) = setup();
        let a = ctx(&ts);
        for _ in 0..4 {
            a.insert_tuple(rel, Xid(3), &tuple(1000)).unwrap();
        }
        assert_eq!(nblocks(&ts, rel), 1);
        // A new context has no hint and there is no FSM file: the last page is used.
        let b = ctx(&ts);
        for _ in 0..3 {
            b.insert_tuple(rel, Xid(3), &tuple(1000)).unwrap();
        }
        assert_eq!(nblocks(&ts, rel), 1);
        // A damaged FSM does not matter either.
        b.fsm().record(rel, 0, 8000).unwrap();
        ts.pool().flush_relation_buffers(rel).unwrap();
        ts.pool()
            .drop_relation_buffers_from(rel, ForkNumber::Fsm, 0)
            .unwrap();
        let path = crate::storage::smgr::relpath(rel, ForkNumber::Fsm, 0);
        let f = ts
            .vfs
            .open(&path, crate::storage::vfs::OpenMode::ReadWrite)
            .unwrap();
        f.write_all_at(&[0x5A; 64], 200).unwrap();
        let c = ctx(&ts);
        // 7 tuples are on block 0; 8 more make 15 of the 16 that fit on two pages.
        for _ in 0..8 {
            c.insert_tuple(rel, Xid(3), &tuple(1000)).unwrap();
        }
        assert_eq!(nblocks(&ts, rel), 2);
        ts.assert_clean();
    }

    #[test]
    fn stale_fsm_entries_beyond_the_end_are_dropped() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        h.fsm().record(rel, 50, 8000).unwrap();
        let g = ctx(&ts);
        // Same FSM file, no hint: block 50 is past the end and is discarded.
        g.fsm().record(rel, 50, 8000).unwrap();
        g.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        assert_eq!(g.fsm().category(rel, 50).unwrap(), 0);
        assert_eq!(nblocks(&ts, rel), 2);
        ts.assert_clean();
    }

    #[test]
    fn find_target_order_and_tried() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        for _ in 0..3 {
            h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        }
        assert_eq!(nblocks(&ts, rel), 3);
        h.fsm().record(rel, 1, 6000).unwrap();
        let blk = |tried: &[BlockNumber]| h.find_target(rel, 4000, tried).unwrap().tag().block;
        assert_eq!(blk(&[]), 2, "the hint comes first");
        h.forget(rel).unwrap();
        assert_eq!(blk(&[]), 1, "then the FSM");
        assert_eq!(blk(&[1]), 2, "then the last page");
        assert_eq!(blk(&[1, 2]), 3, "then an extension");
        let many: Vec<BlockNumber> = (0..8).collect();
        assert_eq!(blk(&many), 4, "eight misses extend");
        ts.assert_clean();
    }

    #[test]
    fn space_freed_by_vacuum_is_reused_without_growth() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        for _ in 0..3 {
            h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        }
        assert_eq!(nblocks(&ts, rel), 3);
        // "VACUUM" empties block 0 and records it.
        {
            let buf = ts.pool().read_buffer(tag(rel, 0)).unwrap();
            let mut g = buf.write().unwrap();
            g.set_lsn(0);
            g.page_mut().init_heap();
            let free = g.page().free_space();
            drop(g);
            h.fsm().record_batch(rel, &[(0, free)]).unwrap();
        }
        h.forget(rel).unwrap();
        let t = h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        assert_eq!(t.block, 0);
        assert_eq!(nblocks(&ts, rel), 3);
        ts.assert_clean();
    }

    #[test]
    fn a_page_without_room_gets_its_real_free_space_recorded() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        h.fsm().record(rel, 0, 8000).unwrap();
        h.forget(rel).unwrap();
        h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        assert_eq!(nblocks(&ts, rel), 2);
        assert!(h.fsm().category(rel, 0).unwrap() < 100);
        ts.assert_clean();
    }

    #[test]
    fn pruning_for_space_needs_the_environment() {
        let (ts, rel) = setup();
        let h = ctx(&ts);
        h.insert_tuple(rel, Xid(3), &tuple(5000)).unwrap();
        let buf = ts.pool().read_buffer(tag(rel, 0)).unwrap();
        let mut g = buf.write().unwrap();
        assert!(!h.try_prune_for_space(rel, 0, &mut g).unwrap());
        drop(g);
        drop(buf);
        ts.assert_clean();
    }
}
