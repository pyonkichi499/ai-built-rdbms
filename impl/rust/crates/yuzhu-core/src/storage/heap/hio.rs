//! Choosing the page for an insert: per-relation hint of the last page, no
//! FSM (`m2.md` §6.5.2).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::tuple::TupleHeader;
use super::wal::{HEAP_INIT_PAGE, HEAP_INSERT};
use crate::error::{Error, Result};
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard, PinnedBuffer};
use crate::storage::page::{Page, PageError};
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator};
use crate::storage::{MAX_HEAP_TUPLES_PER_PAGE, MAXALIGN};
use crate::txn::Xid;
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
pub fn fits(page: &Page, len: usize) -> bool {
    if page.is_new() {
        return true;
    }
    let need = len.div_ceil(MAXALIGN) * MAXALIGN;
    usize::from(page.max_offset()) < MAX_HEAP_TUPLES_PER_PAGE && page.free_space() >= need
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

/// Picks the page for a new tuple of `len` bytes and returns it pinned, without a
/// latch (`m3.md` §6.7). The hinted (or last) page is used if it has room; otherwise
/// the relation is extended (no latch is held during that). The caller latches the
/// page and re-checks [`fits`]; the answer here may be stale by then.
pub fn find_target(
    pool: &Arc<BufferPool>,
    hints: &InsertHints,
    rel: RelFileLocator,
    len: usize,
) -> Result<PinnedBuffer> {
    let nblocks = pool.nblocks(rel, ForkNumber::Main)?;
    let candidate = hints
        .get(rel)?
        .filter(|b| *b < nblocks)
        .or(nblocks.checked_sub(1));
    if let Some(blk) = candidate {
        let buf = pool.read_buffer(tag(rel, blk))?;
        let room = fits(&*buf.read()?, len);
        if room {
            return Ok(buf);
        }
    }
    pool.extend(rel, ForkNumber::Main)
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

/// Inserts `data` on a page with room and logs it (`m3.md` §5.1). No latch is held
/// while the relation is extended.
pub fn insert_tuple(
    pool: &Arc<BufferPool>,
    wal: &Wal,
    hints: &InsertHints,
    rel: RelFileLocator,
    xid: Xid,
    data: &[u8],
) -> Result<Tid> {
    loop {
        let buf = find_target(pool, hints, rel, data.len())?;
        let blk = buf.tag().block;
        let mut guard = buf.write()?;
        if !fits(guard.page(), data.len()) {
            continue;
        }
        let init = guard.page().is_new();
        let cs = CriticalSection::enter(pool);
        let tid = place_in_page(&mut guard, blk, data).map_err(|e| cs.escalate(e))?;
        let end = log_insert(wal, &guard, buf.tag(), tid, init, xid).map_err(|e| cs.escalate(e))?;
        guard.set_lsn(end.0);
        drop(guard);
        drop(cs);
        hints.set(rel, blk)?;
        return Ok(tid);
    }
}
