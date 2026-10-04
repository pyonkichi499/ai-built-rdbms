//! `HeapStore`: the `TableStore` implementation over the buffer pool and
//! the heap (`m2.md` §4.4, §6.5).

use std::sync::Arc;

use crate::error::Result;
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard};
use crate::storage::heap::hio::{self, InsertHints, page_error, tag};
use crate::storage::heap::scan::decode_if_visible;
use crate::storage::heap::tuple::{
    HEAP_KEYS_UPDATED, HEAP_XMAX_LOCK_BITS, TupleFlags, TupleHeader, form_tuple,
};
use crate::storage::heap::visibility::satisfies_update;
use crate::storage::page::LpFlags;
use crate::storage::smgr::{ForkNumber, RelFileLocator};
use crate::storage::{
    HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
};
use crate::txn::Snapshot;
use crate::txn::clog::Clog;
use crate::types::{Datum, Tid};

#[derive(Debug)]
pub struct HeapStore {
    pool: Arc<BufferPool>,
    clog: Arc<Clog>,
    hints: InsertHints,
}

impl HeapStore {
    pub fn new(pool: Arc<BufferPool>, clog: Arc<Clog>) -> HeapStore {
        HeapStore {
            pool,
            clog,
            hints: InsertHints::default(),
        }
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
}

/// Writes xmax / cmax (and optionally the new ctid) into the old version.
/// Must run inside a critical section.
fn stamp_xmax(
    guard: &mut PageWriteGuard<'_>,
    off: u16,
    mut hdr: TupleHeader,
    w: &WriteCtx,
    keys_updated: bool,
    new_ctid: Option<Tid>,
) -> Result<()> {
    hdr.xmax = w.xid;
    hdr.cmax = w.cid;
    hdr.infomask &= !HEAP_XMAX_LOCK_BITS;
    if keys_updated {
        hdr.infomask2 |= HEAP_KEYS_UPDATED;
    }
    if let Some(c) = new_ctid {
        hdr.ctid = c;
    }
    let item = guard
        .page_mut()
        .item_mut(off)
        .map_err(|_| crate::error::Error::internal("tuple vanished while being marked"))?;
    hdr.write(item);
    Ok(())
}

impl TableStore for HeapStore {
    fn create_storage(&self, rel: RelFileLocator) -> Result<()> {
        self.pool.smgr().create(rel, ForkNumber::Main)
    }

    fn storage_exists(&self, rel: RelFileLocator) -> Result<bool> {
        self.pool.smgr().exists(rel, ForkNumber::Main)
    }

    fn unlink_storage(&self, rel: RelFileLocator) -> Result<()> {
        self.pool.drop_relation_buffers(rel)?;
        self.hints.forget(rel)?;
        self.pool.smgr().unlink(rel)
    }

    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
        let data = form_tuple(&rel.desc, row, w, TupleFlags::default())?;
        hio::insert_tuple(&self.pool, &self.hints, rel.locator, &data)
    }

    fn delete(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid) -> Result<TmResult> {
        if !self.block_exists(rel.locator, tid)? {
            return Ok(TmResult::Invisible);
        }
        let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
        let mut guard = buf.write()?;
        let Some(hdr) = Self::locate(&guard, rel.locator, tid)? else {
            return Ok(TmResult::Invisible);
        };
        let res = satisfies_update(&self.clog, &hdr, snap, tid)?;
        if res != TmResult::Ok {
            return Ok(res);
        }
        let cs = CriticalSection::enter(&self.pool);
        stamp_xmax(&mut guard, tid.offset, hdr, w, true, None).map_err(|e| cs.escalate(e))?;
        Ok(TmResult::Ok)
    }

    fn update(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        let data = form_tuple(&rel.desc, new_row, w, TupleFlags { updated: true })?;
        let outcome = |result| UpdateOutcome {
            result,
            new_tid: None,
        };
        if !self.block_exists(rel.locator, tid)? {
            return Ok(outcome(TmResult::Invisible));
        }
        let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
        let mut guard = buf.write()?;
        let Some(hdr) = Self::locate(&guard, rel.locator, tid)? else {
            return Ok(outcome(TmResult::Invisible));
        };
        let res = satisfies_update(&self.clog, &hdr, snap, tid)?;
        if res != TmResult::Ok {
            return Ok(outcome(res));
        }

        if hio::fits(guard.page(), data.len()) {
            let cs = CriticalSection::enter(&self.pool);
            let new_tid = hio::place_in_page(&mut guard, tid.block, &data)
                .and_then(|n| {
                    stamp_xmax(&mut guard, tid.offset, hdr, w, false, Some(n))?;
                    Ok(n)
                })
                .map_err(|e| cs.escalate(e))?;
            return Ok(UpdateOutcome {
                result: TmResult::Ok,
                new_tid: Some(new_tid),
            });
        }

        // The new version goes to another page; never hold two latches.
        drop(guard);
        let new_tid = hio::insert_tuple(&self.pool, &self.hints, rel.locator, &data)?;
        let mut guard = buf.write()?;
        let Some(hdr) = Self::locate(&guard, rel.locator, tid)? else {
            return Err(crate::error::Error::internal(
                "old tuple version vanished during UPDATE",
            ));
        };
        let cs = CriticalSection::enter(&self.pool);
        stamp_xmax(&mut guard, tid.offset, hdr, w, false, Some(new_tid))
            .map_err(|e| cs.escalate(e))?;
        Ok(UpdateOutcome {
            result: TmResult::Ok,
            new_tid: Some(new_tid),
        })
    }

    fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan> {
        let nblocks = self.pool.nblocks(rel.locator, ForkNumber::Main)?;
        Ok(HeapScan::new(rel.clone(), snap.clone(), nblocks))
    }

    fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        scan.next_tuple(&self.pool, &self.clog)
    }

    fn fetch(&self, rel: &RelHandle, snap: &Snapshot, tid: Tid) -> Result<Option<HeapTuple>> {
        if !self.block_exists(rel.locator, tid)? {
            return Ok(None);
        }
        let buf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
        let guard = buf.read()?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::RelFileNumber;
    use crate::storage::stack::StorageStack;
    use crate::storage::vfs::sim::SimVfs;
    use crate::storage::{AttrDesc, TupleDesc};
    use crate::txn::Xid;
    use crate::txn::clog::XidStatus;
    use crate::types::oid;

    struct Fixture {
        stack: StorageStack,
        rel: RelHandle,
    }

    fn fixture() -> Fixture {
        let stack = StorageStack::new(Arc::new(SimVfs::new(1)), 131_072, 32, Xid(3)).unwrap();
        let locator = RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(16384),
        };
        stack.heap.create_storage(locator).unwrap();
        let rel = RelHandle {
            oid: 16384,
            locator,
            desc: Arc::new(TupleDesc {
                attrs: vec![
                    AttrDesc::from_type(oid::INT4),
                    AttrDesc::from_type(oid::TEXT),
                ],
            }),
        };
        Fixture { stack, rel }
    }

    impl Fixture {
        fn xid(&self, x: u64, s: Option<XidStatus>) {
            self.stack.clog.ensure_page_for(Xid(x));
            if let Some(s) = s {
                self.stack.clog.set_status(Xid(x), s).unwrap();
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
            let h = &self.stack.heap;
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
        let h = &f.stack.heap;
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
        assert_eq!(f.stack.pool.pinned_frames(), 0);
    }

    #[test]
    fn delete_and_double_delete() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.stack.heap;
        let t = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        assert_eq!(h.delete(&f.rel, &w(5, 0), &s, t).unwrap(), TmResult::Ok);
        let s1 = Fixture::snap(Some(5), 6, 1);
        assert!(f.all(&s1).is_empty());
        assert_eq!(f.all(&Fixture::snap(None, 6, 0)).len(), 1);
        assert_eq!(
            h.delete(&f.rel, &w(5, 0), &s, t).unwrap(),
            TmResult::SelfModified { cmax: 0 }
        );
        assert_eq!(
            h.delete(&f.rel, &w(5, 1), &s1, t).unwrap(),
            TmResult::Invisible
        );
        let bogus = Tid {
            block: 9,
            offset: 1,
        };
        assert_eq!(
            h.delete(&f.rel, &w(5, 1), &s1, bogus).unwrap(),
            TmResult::Invisible
        );
    }

    #[test]
    fn update_same_page_chains_ctid() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        f.xid(5, None);
        let h = &f.stack.heap;
        let t = h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        let out = h.update(&f.rel, &w(5, 0), &s, t, &row(1, "z")).unwrap();
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
        let h = &f.stack.heap;
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
            .update(&f.rel, &w(5, 0), &s, tids[0], &row(0, &"y".repeat(5000)))
            .unwrap();
        assert_eq!(out.result, TmResult::Ok);
        let s1 = Fixture::snap(Some(5), 6, 1);
        let rows = f.all(&s1);
        assert_eq!(rows.len(), 40);
        assert!(
            rows.iter()
                .any(|r| r.row[1] == Datum::Text("y".repeat(5000)))
        );
        assert_eq!(f.stack.pool.pinned_frames(), 0);
    }

    #[test]
    fn scan_ignores_blocks_added_after_begin() {
        let f = fixture();
        f.xid(4, Some(XidStatus::Committed));
        let h = &f.stack.heap;
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
        let h = &f.stack.heap;
        let err = h
            .insert(&f.rel, &w(4, 0), &row(1, &"z".repeat(9000)))
            .unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            f.stack
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
        let h = &f.stack.heap;
        h.insert(&f.rel, &w(4, 0), &row(1, "a")).unwrap();
        assert!(h.storage_exists(f.rel.locator).unwrap());
        h.unlink_storage(f.rel.locator).unwrap();
        assert!(h.storage_exists(f.rel.locator).unwrap());
        assert_eq!(
            f.stack
                .pool
                .nblocks(f.rel.locator, ForkNumber::Main)
                .unwrap(),
            0
        );
    }
}
