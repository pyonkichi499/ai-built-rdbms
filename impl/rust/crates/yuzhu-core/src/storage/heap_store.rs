//! `HeapStore`: the `TableStore` implementation over the buffer pool and
//! the heap (`m2.md` §4.4, §6.5; WAL: `m3.md` §5.1).

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard};
use crate::storage::heap::hio::{self, InsertHints, page_error, tag};
use crate::storage::heap::scan::decode_if_visible;
use crate::storage::heap::tuple::{
    HEAP_KEYS_UPDATED, HEAP_XMAX_LOCK_BITS, TupleFlags, TupleHeader, form_tuple,
};
use crate::storage::heap::visibility::satisfies_update;
use crate::storage::heap::wal::{
    HEAP_DELETE, HEAP_INIT_PAGE, HEAP_UPDATE, HeapDeleteMain, HeapUpdateMain,
};
use crate::storage::page::LpFlags;
use crate::storage::smgr::{ForkNumber, RelFileLocator};
use crate::storage::smgr_wal::log_and_create;
use crate::storage::{
    HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
};
use crate::txn::Snapshot;
use crate::txn::clog::Clog;
use crate::types::{Datum, Tid};
use crate::wal::{Lsn, RecordBuilder, RegFlags, RmgrId, Wal};

#[derive(Debug)]
pub struct HeapStore {
    pool: Arc<BufferPool>,
    clog: Arc<Clog>,
    wal: Arc<Wal>,
    hints: InsertHints,
}

impl HeapStore {
    pub fn new(pool: Arc<BufferPool>, clog: Arc<Clog>, wal: Arc<Wal>) -> HeapStore {
        HeapStore {
            pool,
            clog,
            wal,
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

/// Writes xmax / cmax (and optionally the new ctid) into the old version and returns
/// the header as written. Must run inside a critical section.
fn stamp_xmax(
    guard: &mut PageWriteGuard<'_>,
    off: u16,
    mut hdr: TupleHeader,
    w: &WriteCtx,
    keys_updated: bool,
    new_ctid: Option<Tid>,
) -> Result<TupleHeader> {
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
        .map_err(|_| Error::internal("tuple vanished while being marked"))?;
    hdr.write(item);
    Ok(hdr)
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
        self.hints.forget(rel)?;
        self.pool.smgr().unlink(rel)
    }

    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid> {
        let data = form_tuple(&rel.desc, row, w, TupleFlags::default())?;
        hio::insert_tuple(
            &self.pool,
            &self.wal,
            &self.hints,
            rel.locator,
            w.xid,
            &data,
        )
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
        let stamped =
            stamp_xmax(&mut guard, tid.offset, hdr, w, true, None).map_err(|e| cs.escalate(e))?;
        let end = self
            .log_delete(&guard, buf.tag(), tid.offset, &stamped, w)
            .map_err(|e| cs.escalate(e))?;
        guard.set_lsn(end.0);
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
        let obuf = self.pool.read_buffer(tag(rel.locator, tid.block))?;
        let mut oguard = obuf.write()?;
        let Some(hdr) = Self::locate(&oguard, rel.locator, tid)? else {
            return Ok(outcome(TmResult::Invisible));
        };
        let res = satisfies_update(&self.clog, &hdr, snap, tid)?;
        if res != TmResult::Ok {
            return Ok(outcome(res));
        }
        let done = |new_tid| UpdateOutcome {
            result: TmResult::Ok,
            new_tid: Some(new_tid),
        };

        if hio::fits(oguard.page(), data.len()) {
            let cs = CriticalSection::enter(&self.pool);
            let (new_tid, stamped) = hio::place_in_page(&mut oguard, tid.block, &data)
                .and_then(|n| {
                    Ok((
                        n,
                        stamp_xmax(&mut oguard, tid.offset, hdr, w, false, Some(n))?,
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
            return Ok(done(new_tid));
        }

        // The new version goes to another page: latch both in block order (§5.9).
        drop(oguard);
        loop {
            let nbuf = hio::find_target(&self.pool, &self.hints, rel.locator, data.len())?;
            let nblk = nbuf.tag().block;
            if nblk == tid.block {
                // The old page has room after all; start over through the same-page path.
                return self.update(rel, w, snap, tid, new_row);
            }
            let (mut nguard, mut oguard);
            if nblk < tid.block {
                nguard = nbuf.write()?;
                oguard = obuf.write()?;
            } else {
                oguard = obuf.write()?;
                nguard = nbuf.write()?;
            }
            let Some(now) = Self::locate(&oguard, rel.locator, tid)? else {
                return Err(Error::internal("old tuple version vanished during UPDATE"));
            };
            if now != hdr {
                return Err(Error::internal("old tuple version changed during UPDATE"));
            }
            if !hio::fits(nguard.page(), data.len()) {
                continue;
            }
            let init = nguard.page().is_new();
            let cs = CriticalSection::enter(&self.pool);
            let (new_tid, stamped) = hio::place_in_page(&mut nguard, nblk, &data)
                .and_then(|n| {
                    Ok((
                        n,
                        stamp_xmax(&mut oguard, tid.offset, hdr, w, false, Some(n))?,
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
            self.hints.set(rel.locator, nblk)?;
            return Ok(done(new_tid));
        }
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
        let h = &f.ts.stack.heap;
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
        assert_eq!(f.ts.stack.pool.pinned_frames(), 0);
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
            h.delete(&f.rel, &w(5, 0), &s, tids[3]).unwrap(),
            TmResult::Ok
        );
        // Same page: a page is full of 1000-byte rows, so shrink the row.
        let out = h
            .update(&f.rel, &w(5, 0), &s, tids[4], &row(4, "s"))
            .unwrap();
        assert_eq!(out.new_tid.unwrap().block, tids[4].block);
        // Another page: grow the row past the free space of its page.
        let out = h
            .update(&f.rel, &w(5, 0), &s, tids[5], &row(5, &"y".repeat(2000)))
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
        h.hints.set(f.rel.locator, 0).unwrap();
        let s = Fixture::snap(Some(5), 6, 0);
        let out = h
            .update(&f.rel, &w(5, 0), &s, old, &row(2, &"n".repeat(5000)))
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
