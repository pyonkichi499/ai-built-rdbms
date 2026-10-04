//! `HeapScan`: scan state that holds neither pins nor latches
//! (`m2.md` §4.4, §6.5.4).

use std::collections::VecDeque;
use std::sync::Arc;

use super::hio::{page_error, tag};
use super::tuple::{TupleHeader, deform_tuple};
use super::visibility::visible;
use crate::error::Result;
use crate::storage::buffer::BufferPool;
use crate::storage::page::{LpFlags, Page};
use crate::storage::smgr::BlockNumber;
use crate::storage::{HeapTuple, RelHandle};
use crate::txn::Snapshot;
use crate::txn::clog::Clog;
use crate::types::Tid;

#[derive(Debug)]
pub struct HeapScan {
    rel: RelHandle,
    snapshot: Snapshot,
    /// Fixed when the scan begins; blocks added later are not read.
    nblocks: u32,
    next_block: u32,
    /// Visible tuples of the current block, already decoded.
    buf: VecDeque<HeapTuple>,
}

impl HeapScan {
    /// The scan before the first block. `nblocks` is the block count at the
    /// time the scan begins.
    pub fn new(rel: RelHandle, snapshot: Snapshot, nblocks: u32) -> HeapScan {
        HeapScan {
            rel,
            snapshot,
            nblocks,
            next_block: 0,
            buf: VecDeque::new(),
        }
    }

    /// Returns the next visible tuple, reading one block at a time. Neither
    /// a pin nor a latch outlives the call.
    pub(crate) fn next_tuple(
        &mut self,
        pool: &Arc<BufferPool>,
        clog: &Clog,
    ) -> Result<Option<HeapTuple>> {
        loop {
            if let Some(t) = self.buf.pop_front() {
                return Ok(Some(t));
            }
            if self.next_block >= self.nblocks {
                return Ok(None);
            }
            let blk = self.next_block;
            self.next_block += 1;
            let pinned = pool.read_buffer(tag(self.rel.locator, blk))?;
            let guard = pinned.read()?;
            self.buf = collect_visible(&guard, &self.rel, blk, &self.snapshot, clog)?;
        }
    }
}

/// Decodes the visible tuples of one page.
pub(crate) fn collect_visible(
    page: &Page,
    rel: &RelHandle,
    blk: BlockNumber,
    snap: &Snapshot,
    clog: &Clog,
) -> Result<VecDeque<HeapTuple>> {
    let mut out = VecDeque::new();
    if page.is_new() {
        return Ok(out);
    }
    for off in 1..=page.max_offset() {
        let id = page
            .item_id(off)
            .map_err(|e| page_error(e, rel.locator, blk))?;
        if id.flags != LpFlags::Normal {
            continue;
        }
        let bytes = page
            .item(off)
            .map_err(|e| page_error(e, rel.locator, blk))?;
        let tid = Tid {
            block: blk,
            offset: off,
        };
        if let Some(t) = decode_if_visible(bytes, tid, rel, snap, clog)? {
            out.push_back(t);
        }
    }
    Ok(out)
}

/// Visibility check plus decoding of one stored tuple.
pub(crate) fn decode_if_visible(
    bytes: &[u8],
    tid: Tid,
    rel: &RelHandle,
    snap: &Snapshot,
    clog: &Clog,
) -> Result<Option<HeapTuple>> {
    let hdr = TupleHeader::read(bytes)?;
    if !visible(clog, &hdr, snap)? {
        return Ok(None);
    }
    Ok(Some(HeapTuple {
        tid,
        xmin: hdr.xmin,
        xmax: if hdr.xmax_invalid() {
            crate::txn::Xid::INVALID
        } else {
            hdr.xmax
        },
        cmin: hdr.cmin,
        cmax: hdr.cmax,
        row: deform_tuple(&rel.desc, bytes)?,
    }))
}

#[cfg(test)]
impl HeapScan {
    /// A scan preloaded with `tuples` (no blocks to read), for fake
    /// `TableStore`s in executor and planner unit tests.
    pub(crate) fn from_tuples(rel: RelHandle, snapshot: Snapshot, tuples: Vec<HeapTuple>) -> Self {
        let mut s = HeapScan::new(rel, snapshot, 0);
        s.buf = tuples.into();
        s
    }

    /// Takes the next preloaded tuple.
    pub(crate) fn pop_buffered(&mut self) -> Option<HeapTuple> {
        self.buf.pop_front()
    }
}
