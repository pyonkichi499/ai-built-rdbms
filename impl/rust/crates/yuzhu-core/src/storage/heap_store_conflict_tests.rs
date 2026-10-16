//! `HeapStore` の更新競合・行ロックの単体テスト（`m5/02` §5.2〜§5.5、RW-1c）。
//! 2 スレッドで、待つ側がラッチ・ピンを持たずにタプルロックと XID の終了を待つことを確かめる。

#![allow(clippy::many_single_char_names)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::heap::prune::RunningXids;
use super::heap::tuple::TupleHeader;
use super::heap::xmax::{XmaxState, decode_xmax};
use super::smgr::{RelFileLocator, RelFileNumber};
use super::testing::{TestStorage, TestStorageOptions};
use super::vfs::{SimVfs, Vfs};
use super::{
    AttrDesc, LockOutcome, RelHandle, RowWait, SimpleHeapOp, TableStore, TmResult, TupleDesc,
    WriteCtx,
};
use crate::backend::BackendId;
use crate::control::{ControlData, ControlFileHandle};
use crate::interrupt::InterruptFlag;
use crate::txn::clog::XidStatus;
use crate::txn::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtl, WaitCtx};
use crate::txn::multixact::MultiXactTable;
use crate::txn::{LockTupleMode, Snapshot, Xid};
use crate::types::{Datum, Tid, oid};

#[derive(Debug, Default)]
struct Procs(Mutex<BTreeSet<u64>>);

impl RunningXids for Procs {
    fn is_in_progress(&self, x: Xid) -> bool {
        self.0.lock().unwrap().contains(&x.0)
    }
    fn next_xid(&self) -> Xid {
        Xid(1000)
    }
    fn oldest_xmin(&self) -> Xid {
        Xid(3)
    }
    fn oldest_xmin_hint(&self) -> Xid {
        Xid(3)
    }
}

struct World {
    ts: TestStorage,
    rel: RelHandle,
    procs: Arc<Procs>,
    locks: Arc<LockManager>,
    multi: Arc<MultiXactTable>,
    flag: InterruptFlag,
}

fn row(i: i32, s: &str) -> Vec<Datum> {
    vec![Datum::Int4(i), Datum::Text(s.into())]
}

impl World {
    fn new(key0: bool) -> World {
        let ts = TestStorage::with_options(TestStorageOptions {
            nframes: 32,
            next_xid: Xid(3),
            ..TestStorageOptions::default()
        })
        .unwrap();
        let locator = RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(16384),
        };
        ts.heap()
            .create_storage(
                &WriteCtx {
                    xid: Xid(3),
                    cid: 0,
                },
                locator,
            )
            .unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(9));
        let control = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(1, 2, 131_072)).unwrap(),
        );
        let multi = Arc::new(MultiXactTable::new(control));
        let procs = Arc::new(Procs::default());
        let locks = LockManager::new();
        assert!(ts.heap().set_vacuum_env(procs.clone(), multi.clone()));
        assert!(ts.heap().set_lock_env(locks.clone()));
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
            key_columns: Arc::from(vec![key0, false]),
            ri: Arc::default(),
        };
        let w = World {
            ts,
            rel,
            procs,
            locks,
            multi,
            flag: InterruptFlag::default(),
        };
        // xid 3 inserted the rows and committed.
        w.ts.clog().ensure_page_for(Xid(3));
        w.ts.clog()
            .set_status(Xid(3), XidStatus::Committed)
            .unwrap();
        w
    }

    /// A running transaction `xid` run by backend `xid`.
    fn begin(&self, xid: u64) -> u64 {
        self.ts.clog().ensure_page_for(Xid(xid));
        self.locks
            .register_backend(BackendId(xid), i32::try_from(xid).unwrap());
        self.procs.0.lock().unwrap().insert(xid);
        assert!(self.locks.try_acquire(
            BackendId(xid),
            LockTag::TransactionId(Xid(xid)),
            LockMode::Exclusive,
            LockScope::Transaction
        ));
        xid
    }

    fn finish(&self, xid: u64, st: XidStatus) {
        self.ts.clog().set_status(Xid(xid), st).unwrap();
        self.procs.0.lock().unwrap().remove(&xid);
        self.locks.release(
            BackendId(xid),
            LockTag::TransactionId(Xid(xid)),
            LockMode::Exclusive,
        );
        self.multi.on_xact_end(Xid(xid));
    }

    fn commit(&self, xid: u64) {
        self.finish(xid, XidStatus::Committed);
    }

    fn abort(&self, xid: u64) {
        self.finish(xid, XidStatus::Aborted);
    }

    fn snap(xid: u64) -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(1000),
            xip: vec![],
            curcid: 1,
            own_xid: Some(Xid(xid)),
        }
    }

    fn wctx(xid: u64) -> WriteCtx {
        WriteCtx {
            xid: Xid(xid),
            cid: 1,
        }
    }

    fn ctl(&self, timeout: Option<Duration>) -> WaitCtl<'_> {
        WaitCtl {
            lock_timeout: timeout.or(Some(Duration::from_secs(20))),
            deadlock_timeout: Duration::from_secs(10),
            interrupts: &self.flag,
        }
    }

    fn insert(&self, i: i32) -> Tid {
        self.ts
            .heap()
            .insert(&self.rel, &Self::wctx(3), &row(i, "v"))
            .unwrap()
    }

    fn header(&self, tid: Tid) -> TupleHeader {
        let buf = self
            .ts
            .pool()
            .read_buffer(super::heap::hio::tag(self.rel.locator, tid.block))
            .unwrap();
        let g = buf.read().unwrap();
        TupleHeader::read(g.item(tid.offset).unwrap()).unwrap()
    }

    fn lock(
        &self,
        xid: u64,
        tid: Tid,
        mode: LockTupleMode,
        policy: RowWait,
        follow: bool,
        timeout: Option<Duration>,
    ) -> crate::error::Result<LockOutcome> {
        let ctl = self.ctl(timeout);
        let wait = WaitCtx {
            backend: BackendId(xid),
            ctl: &ctl,
        };
        self.ts.heap().lock_tuple(
            &self.rel,
            &Self::wctx(xid),
            &Self::snap(xid),
            tid,
            mode,
            policy,
            follow,
            &wait,
        )
    }

    fn delete(
        &self,
        xid: u64,
        tid: Tid,
        timeout: Option<Duration>,
    ) -> crate::error::Result<TmResult> {
        let ctl = self.ctl(timeout);
        let wait = WaitCtx {
            backend: BackendId(xid),
            ctl: &ctl,
        };
        self.ts.heap().delete(
            &self.rel,
            &Self::wctx(xid),
            &Self::snap(xid),
            tid,
            Some(&wait),
            None,
        )
    }

    fn update(
        &self,
        xid: u64,
        tid: Tid,
        new: &[Datum],
    ) -> crate::error::Result<super::UpdateOutcome> {
        let ctl = self.ctl(None);
        let wait = WaitCtx {
            backend: BackendId(xid),
            ctl: &ctl,
        };
        self.ts.heap().update(
            &self.rel,
            &Self::wctx(xid),
            &Self::snap(xid),
            tid,
            new,
            Some(&wait),
            None,
        )
    }

    fn tuple_tag(tid: Tid) -> LockTag {
        LockTag::Tuple {
            db: 5,
            rel: 16384,
            block: tid.block,
            offset: tid.offset,
        }
    }

    /// Waits until `backend` is blocked in the lock manager.
    fn wait_blocked(&self, backend: u64) -> LockTag {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((tag, _, _)) = self.locks.waiting_on(BackendId(backend)) {
                return tag;
            }
            assert!(Instant::now() < end, "backend {backend} never blocked");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn lock_modes_are_written_and_strengthened_by_the_same_xid() {
    let w = World::new(false);
    let t = w.insert(1);
    let x = w.begin(10);
    for (mode, expect) in [
        (LockTupleMode::KeyShare, LockTupleMode::KeyShare),
        (LockTupleMode::Share, LockTupleMode::Share),
        (LockTupleMode::KeyShare, LockTupleMode::Share),
        (LockTupleMode::NoKeyExclusive, LockTupleMode::NoKeyExclusive),
        (LockTupleMode::Exclusive, LockTupleMode::Exclusive),
    ] {
        let out = w.lock(x, t, mode, RowWait::Block, false, None).unwrap();
        assert_eq!(out.result, TmResult::Ok);
        assert!(out.latest.is_none());
        assert_eq!(
            decode_xmax(&w.header(t)).unwrap(),
            XmaxState::Locker {
                xid: Xid(x),
                mode: expect
            }
        );
    }
    assert_eq!(w.ts.pool().pinned_frames(), 0);
}

#[test]
fn compatible_lockers_share_a_multixact() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.lock(a, t, LockTupleMode::KeyShare, RowWait::Block, false, None)
        .unwrap();
    let out = w
        .lock(
            b,
            t,
            LockTupleMode::NoKeyExclusive,
            RowWait::Block,
            false,
            None,
        )
        .unwrap();
    assert_eq!(out.result, TmResult::Ok);
    let XmaxState::LockerMulti(id) = decode_xmax(&w.header(t)).unwrap() else {
        panic!("expected a MultiXact");
    };
    let state = w.multi.expand(id, &|x| x == Xid(a) || x == Xid(b));
    match state {
        crate::txn::multixact::MultiXactState::Live(m) => assert_eq!(m.len(), 2),
        crate::txn::multixact::MultiXactState::Dead => panic!("members are running"),
    }
}

#[test]
fn delete_waits_for_a_committed_updater_and_reports_updated() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    let out = w.update(a, t, &row(1, "new")).unwrap();
    assert_eq!(out.result, TmResult::Ok);
    let new_tid = out.new_tid.unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| w.delete(b, t, None).unwrap());
        let blocked_on = w.wait_blocked(b);
        assert_eq!(blocked_on, LockTag::TransactionId(Xid(a)));
        // The waiter holds the tuple lock, and holds no latch: the page is usable.
        assert!(
            w.locks
                .holds(BackendId(b), World::tuple_tag(t), LockMode::Exclusive)
        );
        w.insert(2);
        w.commit(a);
        assert_eq!(
            h.join().unwrap(),
            TmResult::Updated {
                ctid: new_tid,
                xmax: Xid(a)
            }
        );
    });
    assert!(
        !w.locks
            .holds(BackendId(b), World::tuple_tag(t), LockMode::Exclusive)
    );
    assert_eq!(w.ts.pool().pinned_frames(), 0);
}

#[test]
fn delete_proceeds_after_the_updater_aborts() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.delete(a, t, None).unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| w.delete(b, t, None).unwrap());
        w.wait_blocked(b);
        w.abort(a);
        assert_eq!(h.join().unwrap(), TmResult::Ok);
    });
    let hdr = w.header(t);
    assert_eq!(hdr.xmax, Xid(b));
}

#[test]
fn deleted_row_after_commit_is_deleted() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.delete(a, t, None).unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| w.delete(b, t, None).unwrap());
        w.wait_blocked(b);
        w.commit(a);
        assert_eq!(h.join().unwrap(), TmResult::Deleted { xmax: Xid(a) });
    });
}

#[test]
fn update_waits_for_a_key_share_locker_and_succeeds_after_commit() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.lock(a, t, LockTupleMode::KeyShare, RowWait::Block, false, None)
        .unwrap();
    std::thread::scope(|s| {
        let h = s.spawn(|| w.update(b, t, &row(1, "x")).unwrap());
        w.wait_blocked(b);
        w.commit(a);
        let out = h.join().unwrap();
        assert_eq!(out.result, TmResult::Ok);
        assert_eq!(out.lockmode, LockTupleMode::NoKeyExclusive);
    });
}

#[test]
fn update_lockmode_follows_key_columns() {
    let w = World::new(true);
    let t = w.insert(1);
    let a = w.begin(10);
    let out = w.update(a, t, &row(2, "v")).unwrap();
    assert_eq!(out.lockmode, LockTupleMode::Exclusive);
    let t2 = out.new_tid.unwrap();
    let b = w.update(a, t2, &row(2, "w"));
    // Own earlier version in the same command is invisible or self-modified; either way not an error.
    assert!(b.is_ok());
}

#[test]
fn skip_and_error_policies_do_not_wait() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.delete(a, t, None).unwrap();
    for policy in [RowWait::Skip, RowWait::Error] {
        let out = w
            .lock(b, t, LockTupleMode::Share, policy, false, None)
            .unwrap();
        assert_eq!(out.result, TmResult::WouldBlock);
        assert!(
            !w.locks
                .holds(BackendId(b), World::tuple_tag(t), LockMode::Exclusive)
        );
    }
}

#[test]
fn lock_timeout_releases_the_tuple_lock() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.delete(a, t, None).unwrap();
    let err = w
        .delete(b, t, Some(Duration::from_millis(100)))
        .unwrap_err();
    assert_eq!(err.sqlstate, crate::error::sqlstate::LOCK_NOT_AVAILABLE);
    assert!(
        !w.locks
            .holds(BackendId(b), World::tuple_tag(t), LockMode::Exclusive)
    );
    assert_eq!(w.ts.pool().pinned_frames(), 0);
}

#[test]
fn third_waiter_queues_on_the_tuple_lock() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    let c = w.begin(12);
    w.delete(a, t, None).unwrap();
    std::thread::scope(|s| {
        let hb = s.spawn(|| w.delete(b, t, None).unwrap());
        w.wait_blocked(b);
        let hc = s.spawn(|| w.delete(c, t, None).unwrap());
        assert_eq!(w.wait_blocked(c), World::tuple_tag(t));
        w.commit(a);
        assert_eq!(hb.join().unwrap(), TmResult::Deleted { xmax: Xid(a) });
        assert_eq!(hc.join().unwrap(), TmResult::Deleted { xmax: Xid(a) });
    });
}

#[test]
fn lock_follows_the_update_chain() {
    let w = World::new(true);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    let out = w.update(a, t, &row(1, "nonkey")).unwrap();
    let nt = out.new_tid.unwrap();
    w.commit(a);
    // FOR UPDATE follows and returns the latest version.
    let r = w
        .lock(b, t, LockTupleMode::Exclusive, RowWait::Block, true, None)
        .unwrap();
    assert_eq!(r.result, TmResult::Ok);
    assert_eq!(r.latest.unwrap().tid, nt);
    // KEY SHARE after a non-key update: lock goes to the new version, no latest.
    let c = w.begin(12);
    let t2 = w.insert(5);
    let d = w.begin(13);
    w.update(c, t2, &row(5, "nk")).unwrap();
    w.commit(c);
    let r = w
        .lock(d, t2, LockTupleMode::KeyShare, RowWait::Block, false, None)
        .unwrap();
    assert_eq!(r.result, TmResult::Ok);
    assert!(r.latest.is_none());
    // After a key update, KEY SHARE without follow_updates gets Updated.
    let e = w.begin(14);
    let t3 = w.insert(7);
    let f = w.begin(15);
    w.update(e, t3, &row(8, "key")).unwrap();
    w.commit(e);
    let r = w
        .lock(f, t3, LockTupleMode::KeyShare, RowWait::Block, false, None)
        .unwrap();
    assert!(matches!(r.result, TmResult::Updated { .. }));
}

#[test]
fn crosscheck_failure_reports_updated_without_xmax() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    // A crosscheck snapshot from before xid 3 committed cannot see the row.
    let cc = Snapshot {
        xmin: Xid(3),
        xmax: Xid(3),
        xip: vec![],
        curcid: 0,
        own_xid: None,
    };
    let r =
        w.ts.heap()
            .delete(&w.rel, &World::wctx(a), &World::snap(a), t, None, Some(&cc))
            .unwrap();
    assert_eq!(
        r,
        TmResult::Updated {
            ctid: t,
            xmax: Xid::INVALID
        }
    );
    assert!(w.header(t).xmax_invalid());
}

#[test]
fn without_wait_a_conflict_returns_being_modified() {
    let w = World::new(false);
    let t = w.insert(1);
    let a = w.begin(10);
    let b = w.begin(11);
    w.delete(a, t, None).unwrap();
    let r =
        w.ts.heap()
            .delete(&w.rel, &World::wctx(b), &World::snap(b), t, None, None)
            .unwrap();
    assert_eq!(r, TmResult::BeingModified { xmax: Xid(a) });
}

#[test]
fn expect_simple_messages() {
    let m = |r: TmResult, op| r.expect_simple(op).unwrap_err().message;
    assert!(TmResult::Ok.expect_simple(SimpleHeapOp::Update).is_ok());
    assert_eq!(
        m(
            TmResult::Updated {
                ctid: Tid {
                    block: 0,
                    offset: 1
                },
                xmax: Xid(5)
            },
            SimpleHeapOp::Update
        ),
        "tuple concurrently updated"
    );
    assert_eq!(
        m(TmResult::Deleted { xmax: Xid(5) }, SimpleHeapOp::Delete),
        "tuple concurrently deleted"
    );
    assert_eq!(
        m(TmResult::SelfModified { cmax: 0 }, SimpleHeapOp::Update),
        "tuple already updated by self"
    );
    assert_eq!(
        m(TmResult::Invisible, SimpleHeapOp::Delete),
        "attempted to delete invisible tuple"
    );
}
