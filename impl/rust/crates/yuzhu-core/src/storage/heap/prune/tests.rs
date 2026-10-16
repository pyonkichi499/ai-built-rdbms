//! `prune.rs` と `wal2.rs` の単体テスト（`m5/03` §7.2、VC-1）。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use super::*;
use crate::control::{ControlData, ControlFileHandle};
use crate::debug_knobs::DebugKnobs;
use crate::error::Severity;
use crate::storage::heap::hio::tag;
use crate::storage::heap::tuple::{
    HEAP_KEYS_UPDATED, HEAP_XMAX_EXCL_LOCK, HEAP_XMAX_INVALID, HEAP_XMAX_IS_MULTI,
    HEAP_XMAX_LOCK_ONLY, TupleFlags, form_tuple,
};
use crate::storage::heap::wal2::{self, *};
use crate::storage::smgr::RelFileNumber;
use crate::storage::smgr_wal;
use crate::storage::testing::{TestStorage, TestStorageOptions};
use crate::storage::vfs::{CrashMode, SimVfs, Vfs};
use crate::storage::{AttrDesc, RelHandle, TableStore, TmResult, TupleDesc, WriteCtx};
use crate::txn::Snapshot;
use crate::txn::clog::XidStatus;
use crate::txn::multixact::{LockTupleMode, MultiMember};
use crate::txn::proc_array::ProcArray;
use crate::types::{Datum, Tid, oid};
use crate::wal::{DecodedRecord, SEG_HEADER_SIZE, WalReader};

const HORIZON: Xid = Xid(100);

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// `RunningXids` のフェイク（LK-3 の `ProcArray` が無くても進めるため）。
#[derive(Debug)]
struct FakeProcs {
    running: Mutex<BTreeSet<u64>>,
    next: AtomicU64,
    oldest: AtomicU64,
    hint: AtomicU64,
    oldest_calls: AtomicU64,
}

impl FakeProcs {
    fn new(next: u64, oldest: u64) -> Arc<FakeProcs> {
        Arc::new(FakeProcs {
            running: Mutex::new(BTreeSet::new()),
            next: AtomicU64::new(next),
            oldest: AtomicU64::new(oldest),
            hint: AtomicU64::new(oldest),
            oldest_calls: AtomicU64::new(0),
        })
    }

    fn set_horizon(&self, h: u64) {
        self.oldest.store(h, Ordering::SeqCst);
        self.hint.store(h, Ordering::SeqCst);
    }
}

impl RunningXids for FakeProcs {
    fn is_in_progress(&self, x: Xid) -> bool {
        self.running.lock().unwrap().contains(&x.0)
    }
    fn next_xid(&self) -> Xid {
        Xid(self.next.load(Ordering::SeqCst))
    }
    fn oldest_xmin(&self) -> Xid {
        self.oldest_calls.fetch_add(1, Ordering::SeqCst);
        Xid(self.oldest.load(Ordering::SeqCst))
    }
    fn oldest_xmin_hint(&self) -> Xid {
        Xid(self.hint.load(Ordering::SeqCst))
    }
}

struct World {
    ts: TestStorage,
    procs: Arc<FakeProcs>,
    multi: Arc<MultiXactTable>,
}

impl World {
    fn with_knobs(knobs: DebugKnobs) -> World {
        let ts = TestStorage::with_options(TestStorageOptions {
            nframes: 32,
            next_xid: Xid(3),
            knobs: DebugKnobs {
                assert_wal_before_data: true,
                ..knobs
            },
            ..TestStorageOptions::default()
        })
        .unwrap();
        ts.clog().ensure_page_for(Xid(3));
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(9));
        let control = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(1, 2, 131_072)).unwrap(),
        );
        let multi = Arc::new(MultiXactTable::new(control));
        multi.ensure_reserved().unwrap();
        World {
            ts,
            procs: FakeProcs::new(200, HORIZON.0),
            multi,
        }
    }

    /// A `MultiXact` of exclusive lockers `xids`.
    fn multixact(&self, xids: &[u64]) -> u64 {
        let members: Vec<MultiMember> = xids
            .iter()
            .map(|&x| MultiMember {
                xid: Xid(x),
                mode: LockTupleMode::Exclusive,
            })
            .collect();
        self.multi.create(&members).unwrap().0
    }

    fn wire(&self) {
        assert!(
            self.ts
                .heap()
                .set_vacuum_env(self.procs.clone(), self.multi.clone())
        );
    }

    fn new() -> World {
        World::with_knobs(DebugKnobs::default())
    }

    fn env(&self) -> HeapEnv<'_> {
        HeapEnv {
            clog: self.ts.clog().as_ref(),
            procs: &*self.procs,
            multixact: &self.multi,
        }
    }

    fn commit(&self, x: u64) {
        self.ts
            .clog()
            .set_status(Xid(x), XidStatus::Committed)
            .unwrap();
    }

    fn abort(&self, x: u64) {
        self.ts
            .clog()
            .set_status(Xid(x), XidStatus::Aborted)
            .unwrap();
    }

    /// Running: in the registry, clog `IN_PROGRESS`.
    fn run(&self, x: u64) {
        self.procs.running.lock().unwrap().insert(x);
    }

    /// The standard cast of XIDs.
    fn standard_xids(&self) -> &Self {
        for x in [10, 12, 150] {
            self.commit(x);
        }
        self.abort(11);
        self.run(120);
        self.run(121);
        // 130: a crash leftover (IN_PROGRESS, not in the registry). 77: never touched.
        self
    }
}

fn params(freeze: Option<u64>) -> PruneParams {
    PruneParams {
        horizon: HORIZON,
        freeze_cutoff: freeze.map(Xid),
        mark_unused_now: false,
        own_xid: None,
    }
}

fn header(xmin: u64, xmax: u64, infomask: u16, infomask2: u16) -> TupleHeader {
    TupleHeader {
        xmin: Xid(xmin),
        xmax: Xid(xmax),
        cmin: 0,
        cmax: 0,
        ctid: Tid {
            block: 0,
            offset: 1,
        },
        infomask2,
        infomask,
        hoff: 40,
    }
}

fn tuple(xmin: u64, xmax: u64, infomask: u16) -> Vec<u8> {
    tuple_h(&header(xmin, xmax, infomask, 0))
}

fn tuple_h(h: &TupleHeader) -> Vec<u8> {
    let mut b = vec![0u8; 51];
    h.write(&mut b);
    b
}

fn page_of(tuples: &[Vec<u8>]) -> Page {
    let mut p = Page::zeroed();
    p.init_heap();
    for t in tuples {
        p.add_item(t).unwrap();
    }
    p
}

fn hdr_at(p: &Page, off: u16) -> TupleHeader {
    TupleHeader::read(p.item(off).unwrap()).unwrap()
}

const LOCK_ONLY_EXCL: u16 = HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_EXCL_LOCK;
const FROZEN: u16 = 0x0300;

// ---------------------------------------------------------------------------
// satisfies_vacuum
// ---------------------------------------------------------------------------

fn vac(w: &World, t: &TupleHeader) -> Result<VacTuple> {
    satisfies_vacuum(&w.env(), t, HORIZON)
}

#[test]
fn satisfies_vacuum_truth_table() {
    use VacTuple::*;
    let w = World::new();
    w.standard_xids();
    let v = |xmin, xmax, mask| vac(&w, &header(xmin, xmax, mask, 0)).unwrap();

    // xmin aborted: Dead whatever xmax says. A crash leftover counts as aborted.
    assert_eq!(v(11, 0, 0), Dead);
    assert_eq!(v(11, 150, 0), Dead);
    assert_eq!(v(130, 0, 0), Dead);
    // xmin running, no xmax.
    assert_eq!(v(120, 0, 0), InsertInProgress);
    // xmin committed (also at or above the horizon), no xmax / lock only.
    assert_eq!(v(10, 0, 0), Live);
    assert_eq!(v(150, 0, 0), Live);
    assert_eq!(v(10, 10, LOCK_ONLY_EXCL), Live);
    assert_eq!(v(10, 120, LOCK_ONLY_EXCL | HEAP_XMAX_IS_MULTI), Live);
    assert_eq!(v(10, 120, HEAP_XMAX_INVALID), Live);
    // xmax updater.
    assert_eq!(v(10, 121, 0), DeleteInProgress);
    assert_eq!(v(10, 11, 0), Live, "an aborted updater");
    assert_eq!(v(10, 130, 0), Live, "a crash leftover updater");
    assert_eq!(v(10, 12, 0), Dead, "committed below the horizon");
    assert_eq!(
        v(10, 150, 0),
        RecentlyDead,
        "committed at or above the horizon"
    );
    // Frozen xmin: no clog lookup (77 is not in the clog), BOOTSTRAP / FROZEN likewise.
    assert_eq!(v(77, 12, FROZEN), Dead);
    assert_eq!(v(77, 150, FROZEN), RecentlyDead);
    assert_eq!(v(77, 0, FROZEN), Live);
    assert_eq!(v(1, 0, 0), Live);
    assert_eq!(v(1, 12, 0), Dead);
    assert_eq!(v(2, 0, 0), Live);
    // A single hint bit does not freeze (D13): 77 is looked up in the clog and
    // is not committed.
    assert_eq!(v(77, 0, 0x0100), Dead);
}

#[test]
fn satisfies_vacuum_checks_the_registry_before_the_clog() {
    let w = World::new();
    // 120 is running: clog IN_PROGRESS, in the registry. Looking at the clog
    // first would call it a crash leftover (Aborted -> Dead).
    w.run(120);
    assert_eq!(
        vac(&w, &header(120, 0, 0, 0)).unwrap(),
        VacTuple::InsertInProgress
    );
    // Commit order: the clog is written first, the registry entry is removed
    // later. At that moment the registry still says "running": conservative.
    w.commit(120);
    assert_eq!(
        vac(&w, &header(120, 0, 0, 0)).unwrap(),
        VacTuple::InsertInProgress
    );
    w.procs.running.lock().unwrap().remove(&120);
    assert_eq!(vac(&w, &header(120, 0, 0, 0)).unwrap(), VacTuple::Live);
}

#[test]
fn xids_below_the_horizon_are_not_looked_up_in_the_registry() {
    let w = World::new();
    w.commit(10);
    // A (stale) registry entry below the horizon is ignored: the XID has finished.
    w.run(10);
    assert_eq!(
        vac(&w, &header(10, 0, 0, 0)).unwrap(),
        VacTuple::Live,
        "x < horizon goes to the clog"
    );
}

#[test]
fn satisfies_vacuum_rejects_a_corrupt_xmax() {
    let w = World::new();
    w.standard_xids();
    // IS_MULTI without LOCK_ONLY: updaters never live in a MultiXact (D6).
    let e = vac(&w, &header(10, 150, HEAP_XMAX_IS_MULTI, 0)).unwrap_err();
    assert_eq!(e.sqlstate, crate::error::sqlstate::DATA_CORRUPTED);
    // LOCK_ONLY without a strength bit.
    assert!(vac(&w, &header(10, 150, HEAP_XMAX_LOCK_ONLY, 0)).is_err());
}

// ---------------------------------------------------------------------------
// scan_page
// ---------------------------------------------------------------------------

/// off1 live, off2 dead (xmax committed old), off3 dead (xmin aborted),
/// off4 recently dead (xmax 150), off5 delete in progress (121), off6 insert in
/// progress (120), off7 `LP_DEAD`, off8 `LP_UNUSED` (not the last).
fn mixed_page() -> Page {
    let mut p = page_of(&[
        tuple(10, 0, 0),
        tuple(10, 12, 0),
        tuple(11, 0, 0),
        tuple(10, 150, 0),
        tuple(10, 121, 0),
        tuple(120, 0, 0),
        tuple(10, 12, 0),
        tuple(10, 0, 0),
        tuple(10, 0, 0),
    ]);
    p.mark_dead(7);
    p.mark_unused(8);
    p.set_prune_xid(0);
    p
}

#[test]
fn scan_page_builds_a_plan_and_counts() {
    let w = World::new();
    w.standard_xids();
    let p = mixed_page();
    let before = p.clone();
    let s = scan_page(&w.env(), &p, &params(None)).unwrap();
    assert_eq!(p.0[..], before.0[..], "scan_page only reads");
    assert_eq!(s.plan.dead, vec![2, 3]);
    assert!(s.plan.unused.is_empty() && s.plan.frozen.is_empty());
    assert_eq!(s.lpdead_after, vec![2, 3, 7]);
    // min(150, 121): the oldest xmax that is not removable yet.
    assert_eq!(s.plan.prune_xid, 121);
    assert!(!s.plan.is_empty());
    assert_eq!(
        s.counts,
        PageCounts {
            live: 3, // off1, off5 (delete in progress), off9; off6 is not ours
            recently_dead: 1,
            insert_in_progress: 1,
            made_dead: 2,
            frozen: 0,
            lpdead_total: 3,
        }
    );
}

#[test]
fn scan_page_counts_own_inserts_as_live() {
    let w = World::new();
    w.standard_xids();
    let p = mixed_page();
    let mut pr = params(None);
    pr.own_xid = Some(Xid(120));
    let s = scan_page(&w.env(), &p, &pr).unwrap();
    assert_eq!((s.counts.live, s.counts.insert_in_progress), (4, 0));
    // Somebody else's insert is still not counted.
    pr.own_xid = Some(Xid(121));
    let s = scan_page(&w.env(), &p, &pr).unwrap();
    assert_eq!((s.counts.live, s.counts.insert_in_progress), (3, 1));
}

#[test]
fn scan_page_mark_unused_now_turns_existing_lp_dead_into_unused_too() {
    let w = World::new();
    w.standard_xids();
    let p = mixed_page();
    let mut pr = params(None);
    pr.mark_unused_now = true;
    let s = scan_page(&w.env(), &p, &pr).unwrap();
    assert!(s.plan.dead.is_empty());
    assert_eq!(s.plan.unused, vec![2, 3, 7]);
    assert!(s.lpdead_after.is_empty());
    assert_eq!(s.counts.lpdead_total, 0);
    assert_eq!(s.counts.made_dead, 2);

    let mut q = p;
    apply_plan(&mut q, &s.plan).unwrap();
    // Everything dead is now LP_UNUSED; 7 and 8 are not trailing (9 is live), so they stay.
    assert_eq!(q.max_offset(), 9);
    for off in [2, 3] {
        assert_eq!(q.item_id(off).unwrap().flags, LpFlags::Unused);
    }
    assert_eq!(q.flags() & 1, 1);
}

#[test]
fn scan_page_takes_the_smallest_prunable_xid() {
    let w = World::new();
    w.standard_xids();
    let p = page_of(&[tuple(10, 150, 0), tuple(10, 121, 0), tuple(10, 12, 0)]);
    let s = scan_page(&w.env(), &p, &params(None)).unwrap();
    assert_eq!(s.plan.prune_xid, 121);
    let q = page_of(&[tuple(10, 0, 0)]);
    assert_eq!(
        scan_page(&w.env(), &q, &params(None))
            .unwrap()
            .plan
            .prune_xid,
        0
    );
}

#[test]
fn an_unchanged_plan_is_empty_but_a_stale_hint_is_not() {
    let w = World::new();
    w.standard_xids();
    let mut p = page_of(&[tuple(10, 0, 0), tuple(10, 0, 0)]);
    let s = scan_page(&w.env(), &p, &params(None)).unwrap();
    assert!(s.plan.is_empty());
    assert!(PrunePlan::default().is_empty());
    // The page remembers a prunable XID, but nothing is prunable any more (the
    // deleter aborted): the plan clears the hint, otherwise every read would retry.
    p.set_prune_xid(77);
    let s = scan_page(&w.env(), &p, &params(None)).unwrap();
    assert!(s.plan.dead.is_empty() && !s.plan.is_empty());
    assert_eq!((s.plan.prune_xid, s.plan.base_prune_xid), (0, 77));
    apply_plan(&mut p, &s.plan).unwrap();
    assert_eq!(p.prune_xid(), 0);
    assert!(
        scan_page(&w.env(), &p, &params(None))
            .unwrap()
            .plan
            .is_empty()
    );
}

#[test]
fn scan_page_ignores_a_new_page_and_rejects_redirects() {
    let w = World::new();
    let z = Page::zeroed();
    let s = scan_page(&w.env(), &z, &params(None)).unwrap();
    assert!(s.plan.is_empty() && s.counts == PageCounts::default());
    let mut p = page_of(&[tuple(10, 0, 0)]);
    let pos = 24;
    p.0[pos..pos + 4].copy_from_slice(&((2u32 << 15) | 5).to_le_bytes());
    assert!(scan_page(&w.env(), &p, &params(None)).is_err());
}

// ---------------------------------------------------------------------------
// freezing
// ---------------------------------------------------------------------------

#[test]
fn freezing_covers_xmin_and_three_xmax_cases() {
    let w = World::new();
    w.standard_xids();
    let keys = |t: TupleHeader| TupleHeader {
        infomask2: HEAP_KEYS_UPDATED,
        ..t
    };
    let finished_multi = w.multixact(&[12, 10]);
    let running_multi = w.multixact(&[12, 120]);
    let p = page_of(&[
        // 1: xmin only.
        tuple(10, 0, 0),
        // 2: a finished single locker -> xmax invalidated (KEYS_UPDATED dropped too).
        tuple_h(&keys(header(10, 12, LOCK_ONLY_EXCL, 0))),
        // 3: an aborted updater -> xmax invalidated.
        tuple(10, 11, 0),
        // 4: a MultiXact locker whose members all finished -> xmax invalidated.
        tuple(10, finished_multi, LOCK_ONLY_EXCL | HEAP_XMAX_IS_MULTI),
        // 5: not old enough (xmin 150 >= cutoff): untouched.
        tuple(150, 0, 0),
        // 6: already frozen: no entry.
        tuple(77, 0, FROZEN),
        // 7: a running locker above the cutoff: xmin frozen, xmax kept.
        tuple(10, 120, LOCK_ONLY_EXCL),
        // 8: BOOTSTRAP xmin: not a freeze target.
        tuple(1, 0, 0),
        // 9: a dead tuple is never frozen.
        tuple(10, 12, 0),
        // 10: a running inserter is never frozen.
        tuple(120, 0, 0),
        // 11: a MultiXact locker with a running member: xmax kept, xmin frozen.
        tuple(10, running_multi, LOCK_ONLY_EXCL | HEAP_XMAX_IS_MULTI),
    ]);
    let s = scan_page(&w.env(), &p, &params(Some(100))).unwrap();
    let by_off = |o: u16| s.plan.frozen.iter().find(|e| e.offnum == o).copied();
    assert_eq!(s.plan.dead, vec![9]);
    assert_eq!(s.counts.frozen, 6);
    assert_eq!(
        by_off(1),
        Some(FreezeEntry {
            offnum: 1,
            infomask: FROZEN,
            infomask2: 0,
            clear_xmax: false
        })
    );
    assert_eq!(
        by_off(2),
        Some(FreezeEntry {
            offnum: 2,
            infomask: FROZEN | HEAP_XMAX_INVALID,
            infomask2: 0,
            clear_xmax: true
        })
    );
    assert_eq!(
        by_off(3).map(|e| (e.infomask, e.clear_xmax)),
        Some((FROZEN | HEAP_XMAX_INVALID, true))
    );
    assert_eq!(
        by_off(4).map(|e| (e.infomask, e.clear_xmax)),
        Some((FROZEN | HEAP_XMAX_INVALID, true))
    );
    assert_eq!(
        by_off(7).map(|e| (e.infomask, e.clear_xmax)),
        Some((FROZEN | LOCK_ONLY_EXCL, false))
    );
    assert_eq!(
        by_off(11).map(|e| (e.infomask, e.clear_xmax)),
        Some((FROZEN | LOCK_ONLY_EXCL | HEAP_XMAX_IS_MULTI, false))
    );
    for o in [5, 6, 8, 9, 10] {
        assert_eq!(by_off(o), None, "offset {o}");
    }
    assert!(s.plan.frozen.windows(2).all(|w| w[0].offnum < w[1].offnum));

    // Apply and look at the headers.
    let mut q = p.clone();
    apply_plan(&mut q, &s.plan).unwrap();
    let h2 = hdr_at(&q, 2);
    assert_eq!(h2.xmax, Xid::INVALID);
    assert_eq!(h2.infomask & FROZEN, FROZEN);
    assert_eq!(h2.infomask & HEAP_XMAX_INVALID, HEAP_XMAX_INVALID);
    assert_eq!(h2.infomask & LOCK_ONLY_EXCL, 0);
    assert_eq!(h2.infomask2 & HEAP_KEYS_UPDATED, 0);
    assert_eq!(h2.xmin, Xid(10), "the xmin value stays");
    assert_eq!(hdr_at(&q, 7).xmax, Xid(120));
    assert_eq!(q.item_id(9).unwrap().flags, LpFlags::Dead);
    // Frozen tuples read as committed for every later horizon.
    let env = w.env();
    assert_eq!(
        satisfies_vacuum(&env, &hdr_at(&q, 1), Xid(200)).unwrap(),
        VacTuple::Live
    );
}

#[test]
fn freeze_cutoff_below_the_horizon_leaves_newer_xids_alone() {
    let w = World::new();
    w.standard_xids();
    // cutoff 11: xmin 12 is committed but not below the cutoff.
    let p = page_of(&[tuple(10, 0, 0), tuple(12, 0, 0)]);
    let s = scan_page(&w.env(), &p, &params(Some(11))).unwrap();
    assert_eq!(
        s.plan.frozen.iter().map(|e| e.offnum).collect::<Vec<_>>(),
        vec![1]
    );
}

// ---------------------------------------------------------------------------
// apply_plan
// ---------------------------------------------------------------------------

#[test]
fn apply_plan_is_idempotent_and_checks_states() {
    let w = World::new();
    w.standard_xids();
    let p = mixed_page();
    let mut pr = params(Some(100));
    pr.mark_unused_now = false;
    let s = scan_page(&w.env(), &p, &pr).unwrap();
    let mut once = p.clone();
    apply_plan(&mut once, &s.plan).unwrap();
    let mut twice = once.clone();
    apply_plan(&mut twice, &s.plan).unwrap();
    assert_eq!(
        once.0[..],
        twice.0[..],
        "REDO of the same plan twice is a no-op"
    );

    // A dead entry on an LP_UNUSED is not allowed; frozen must be LP_NORMAL.
    let mut q = page_of(&[tuple(10, 0, 0), tuple(10, 0, 0)]);
    q.mark_unused(1);
    let bad = PrunePlan {
        dead: vec![1],
        ..PrunePlan::default()
    };
    assert!(apply_plan(&mut q, &bad).is_err());
    let mut q = page_of(&[tuple(10, 0, 0)]);
    q.mark_dead(1);
    let bad = PrunePlan {
        frozen: vec![FreezeEntry {
            offnum: 1,
            infomask: FROZEN,
            infomask2: 0,
            clear_xmax: false,
        }],
        ..PrunePlan::default()
    };
    assert!(apply_plan(&mut q, &bad).is_err());
    let mut q = page_of(&[tuple(10, 0, 0)]);
    let bad = PrunePlan {
        dead: vec![9],
        ..PrunePlan::default()
    };
    assert!(apply_plan(&mut q, &bad).is_err(), "out of range");
}

#[test]
fn apply_plan_packs_the_page_like_the_documented_example() {
    let w = World::new();
    w.standard_xids();
    // m5/03 §3.1: three 51-byte tuples; the second one is dead.
    let mut p = page_of(&[tuple(10, 0, 0), tuple(10, 12, 0), tuple(10, 0, 0)]);
    assert_eq!((p.lower(), p.upper()), (36, 8024));
    let s = scan_page(&w.env(), &p, &params(None)).unwrap();
    apply_plan(&mut p, &s.plan).unwrap();
    assert_eq!((p.lower(), p.upper()), (36, 8080));
    assert_eq!(
        &p.0[24..36],
        &[
            0xC8, 0x9F, 0x66, 0, 0x00, 0x80, 0x01, 0, 0x90, 0x9F, 0x66, 0
        ]
    );
    assert_eq!(p.prune_xid(), 0);
}

// ---------------------------------------------------------------------------
// WAL formats
// ---------------------------------------------------------------------------

fn sample_plan() -> PrunePlan {
    PrunePlan {
        dead: vec![2, 5],
        unused: vec![],
        frozen: vec![FreezeEntry {
            offnum: 3,
            infomask: 0x0B03,
            infomask2: 4,
            clear_xmax: false,
        }],
        prune_xid: 756,
        base_prune_xid: 0,
    }
}

#[test]
fn prune_freeze_encoding_matches_the_documented_example() {
    let plan = sample_plan();
    let main = PruneMain::of(&plan);
    assert_eq!(
        main.encode(),
        [0x02, 0, 0, 0, 0x01, 0, 0, 0, 0xF4, 0x02, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        encode_plan_data(&plan),
        [0x02, 0, 0x05, 0, 0x03, 0, 0x03, 0x0B, 0x04, 0, 0, 0]
    );
    let back = decode_plan(
        &PruneMain::decode(&main.encode()).unwrap(),
        &encode_plan_data(&plan),
    )
    .unwrap();
    assert_eq!(
        (
            back.dead.clone(),
            back.unused.clone(),
            back.frozen.clone(),
            back.prune_xid
        ),
        (
            plan.dead.clone(),
            plan.unused.clone(),
            plan.frozen.clone(),
            plan.prune_xid
        )
    );
    // unused and clear_xmax travel too.
    let plan2 = PrunePlan {
        dead: vec![1],
        unused: vec![4, 9],
        frozen: vec![FreezeEntry {
            offnum: 2,
            infomask: 0x0B00,
            infomask2: 0,
            clear_xmax: true,
        }],
        prune_xid: 0,
        base_prune_xid: 0,
    };
    let m = PruneMain::of(&plan2);
    let back = decode_plan(&m, &encode_plan_data(&plan2)).unwrap();
    assert_eq!(
        (back.dead, back.unused, back.frozen),
        (plan2.dead, plan2.unused, plan2.frozen)
    );
}

#[test]
fn prune_freeze_length_mismatches_are_panic_level() {
    let plan = sample_plan();
    let main = PruneMain::of(&plan);
    let data = encode_plan_data(&plan);
    for bad in [
        &data[..data.len() - 1],
        &[data.clone(), vec![0, 0]].concat()[..],
    ] {
        let e = decode_plan(&main, bad).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
    }
    assert!(PruneMain::decode(&[0; 15]).is_err());
    assert!(PruneMain::decode(&[0; 17]).is_err());
    // Not ascending / overlapping / unknown flags.
    let mut unsorted = plan.clone();
    unsorted.dead = vec![5, 2];
    assert!(decode_plan(&PruneMain::of(&unsorted), &encode_plan_data(&unsorted)).is_err());
    let mut overlap = plan.clone();
    overlap.unused = vec![5];
    assert!(decode_plan(&PruneMain::of(&overlap), &encode_plan_data(&overlap)).is_err());
    let mut flagged = PruneMain::of(&plan);
    flagged.flags = 1;
    assert!(decode_plan(&flagged, &data).is_err());
    let mut d = data.clone();
    d[10] = 2;
    assert!(decode_plan(&main, &d).is_err(), "unknown freeze flags");
}

#[test]
fn vacuum_unused_and_inplace_match_the_documented_examples() {
    assert_eq!(encode_vacuum_unused_main(2), [2, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(encode_vacuum_unused_data(&[2, 5]), [2, 0, 5, 0]);
    // m5/03 §3.5: relpages = 1 at byte 136, reltuples = 50.0 at byte 140 of tuple 7.
    let patches = vec![
        InplaceBytes {
            byte_off: 136,
            bytes: 1i32.to_le_bytes().to_vec(),
        },
        InplaceBytes {
            byte_off: 140,
            bytes: 50.0f32.to_le_bytes().to_vec(),
        },
    ];
    let main = encode_inplace_main(7, &patches);
    assert_eq!(
        main,
        [
            0x07, 0, 0x02, 0, 0, 0, 0, 0, 0x88, 0, 0x04, 0, 0x01, 0, 0, 0, 0x8C, 0, 0x04, 0, 0, 0,
            0x48, 0x42
        ]
    );
    assert_eq!(decode_inplace_main(&main).unwrap(), (7, patches));
    assert!(decode_inplace_main(&main[..main.len() - 1]).is_err());
    assert!(decode_inplace_main(&[main.clone(), vec![0]].concat()).is_err());
    assert!(decode_inplace_main(&[0; 4]).is_err());
}

#[test]
fn apply_vacuum_unused_requires_lp_dead_and_is_idempotent() {
    let mut p = page_of(&[
        tuple(10, 0, 0),
        tuple(10, 0, 0),
        tuple(10, 0, 0),
        tuple(10, 0, 0),
    ]);
    p.mark_dead(2);
    p.mark_dead(4);
    p.set_prune_xid(33);
    apply_vacuum_unused(&mut p, &[2, 4]).unwrap();
    // 4 was last, so the array shrank to the 3rd entry; 2 stays as LP_UNUSED.
    assert_eq!(p.max_offset(), 3);
    assert_eq!(p.item_id(2).unwrap().flags, LpFlags::Unused);
    assert_eq!(p.flags() & 1, 1);
    assert_eq!(p.prune_xid(), 33, "pd_prune_xid is not touched");
    assert!(p.item(1).is_ok() && p.item(3).is_ok());
    let again = {
        let mut q = p.clone();
        apply_vacuum_unused(&mut q, &[2, 4]).unwrap();
        q
    };
    assert_eq!(again.0[..], p.0[..]);
    // An LP_NORMAL one is a mismatch between record and page.
    assert!(apply_vacuum_unused(&mut p, &[1]).is_err());
}

#[test]
fn apply_inplace_checks_bounds_and_line_pointer_state() {
    let mut p = page_of(&[tuple(10, 0, 0)]);
    let patch = |off, bytes: &[u8]| InplaceBytes {
        byte_off: off,
        bytes: bytes.to_vec(),
    };
    apply_inplace(&mut p, 1, &[patch(44, &[1, 2, 3, 4])]).unwrap();
    assert_eq!(&p.item(1).unwrap()[44..48], &[1, 2, 3, 4]);
    assert!(
        apply_inplace(&mut p, 1, &[patch(49, &[0; 4])]).is_err(),
        "past the tuple"
    );
    assert!(
        apply_inplace(&mut p, 2, &[]).is_err(),
        "no such line pointer"
    );
    p.mark_dead(1);
    assert!(apply_inplace(&mut p, 1, &[]).is_err());
}

// ---------------------------------------------------------------------------
// prune_and_log and REDO
// ---------------------------------------------------------------------------

fn locator() -> RelFileLocator {
    RelFileLocator {
        spc_oid: 1663,
        db_oid: 5,
        rel_number: RelFileNumber(16384),
    }
}

fn rel_handle() -> RelHandle {
    RelHandle {
        oid: 16384,
        locator: locator(),
        desc: Arc::new(TupleDesc {
            attrs: vec![
                AttrDesc::from_type(oid::INT4),
                AttrDesc::from_type(oid::TEXT),
            ],
        }),
        indexes: Arc::from([]),
        key_columns: Arc::from([]),
        ri: Arc::default(),
    }
}

fn wctx(x: u64) -> WriteCtx {
    WriteCtx {
        xid: Xid(x),
        cid: 0,
    }
}

fn row(i: i32, n: usize) -> Vec<Datum> {
    vec![Datum::Int4(i), Datum::Text("x".repeat(n))]
}

fn snap_of(own: u64) -> Snapshot {
    Snapshot {
        xmin: Xid(3),
        xmax: Xid(own + 1),
        xip: vec![],
        curcid: 0,
        own_xid: Some(Xid(own)),
    }
}

fn wal_records(ts: &TestStorage) -> Vec<DecodedRecord> {
    ts.wal().flush(ts.wal().insert_lsn()).unwrap();
    let cfg = *ts.wal().config();
    let start = crate::wal::Lsn(u64::from(cfg.segment_size) + SEG_HEADER_SIZE);
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

fn replay_one(ctx: &crate::wal::RedoCtx, rec: &DecodedRecord) {
    match rec.rmgr {
        RmgrId::Smgr => smgr_wal::redo(ctx, rec).unwrap(),
        RmgrId::Heap => crate::storage::heap::wal::redo(ctx, rec).unwrap(),
        RmgrId::Heap2 => wal2::redo(ctx, rec).unwrap(),
        _ => {}
    }
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

/// Prunes (and freezes) block `blk` the way VACUUM's first pass does.
fn prune_block(w: &World, blk: BlockNumber, freeze: Option<u64>) -> Option<Lsn> {
    let buf = w.ts.pool().read_buffer(tag(locator(), blk)).unwrap();
    let mut g = buf.write().unwrap();
    let scan = scan_page(&w.env(), g.page(), &params(freeze)).unwrap();
    prune_and_log(w.ts.pool(), w.ts.wal(), locator(), blk, &mut g, &scan.plan).unwrap()
}

fn unused_block(w: &World, blk: BlockNumber) -> Vec<u16> {
    let buf = w.ts.pool().read_buffer(tag(locator(), blk)).unwrap();
    let mut g = buf.write().unwrap();
    let dead: Vec<u16> = (1..=g.page().max_offset())
        .filter(|&o| g.page().item_id(o).unwrap().flags == LpFlags::Dead)
        .collect();
    if !dead.is_empty() {
        log_vacuum_unused(w.ts.pool(), w.ts.wal(), locator(), blk, &mut g, &dead).unwrap();
    }
    dead
}

/// Rows across two pages, deletes by committed xid 5, then pruning + freezing,
/// the third stage, and an insert that reuses a line pointer.
fn run_workload(w: &World, checkpoint_before_unused: bool) {
    let h = w.ts.heap();
    let rel = rel_handle();
    h.create_storage(&wctx(3), locator()).unwrap();
    w.commit(3);
    w.ts.clog().ensure_page_for(Xid(5));
    let tids: Vec<Tid> = (0..24)
        .map(|i| h.insert(&rel, &wctx(4), &row(i, 600)).unwrap())
        .collect();
    w.commit(4);
    let s = snap_of(5);
    for i in [2, 3, 6, 15, 16, 20] {
        assert_eq!(
            h.delete(&rel, &wctx(5), &s, tids[i], None, None).unwrap(),
            TmResult::Ok
        );
    }
    w.commit(5);
    let nblocks = w.ts.pool().nblocks(locator(), ForkNumber::Main).unwrap();
    assert!(nblocks >= 2);
    for blk in 0..nblocks {
        assert!(prune_block(w, blk, Some(100)).is_some());
        // A second pass finds nothing to do and writes nothing.
        assert!(prune_block(w, blk, Some(100)).is_none());
    }
    if checkpoint_before_unused {
        w.ts.wal().begin_checkpoint_quiet().unwrap();
    }
    for blk in 0..nblocks {
        assert!(!unused_block(w, blk).is_empty());
    }
    // The reused line pointer goes through add_item_at in REDO.
    let t = h.insert(&rel, &wctx(6), &row(99, 600)).unwrap();
    assert!(tids.iter().all(|x| *x != t) || t.offset > 0);
    w.commit(6);
}

fn check_redo_matches(checkpoint: bool) {
    let w = World::new();
    run_workload(&w, checkpoint);
    let recs = wal_records(&w.ts);
    let want = page_images(&w.ts, locator());
    let heap2: Vec<&DecodedRecord> = recs.iter().filter(|r| r.rmgr == RmgrId::Heap2).collect();
    assert!(heap2.iter().any(|r| r.info == HEAP2_PRUNE_FREEZE));
    assert!(heap2.iter().any(|r| r.info == HEAP2_VACUUM_UNUSED));
    assert_eq!(
        heap2
            .iter()
            .any(|r| r.info == HEAP2_VACUUM_UNUSED && r.blocks[0].image.is_some()),
        checkpoint,
        "the first change after a checkpoint carries a full-page image"
    );
    assert!(heap2.iter().all(|r| r.xid == Xid::INVALID));

    let ts2 = w.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
    ts2.smgr().set_recovery_mode(true);
    let ctx = redo_ctx(&ts2, DebugKnobs::default());
    let again = redo_ctx(
        &ts2,
        DebugKnobs {
            redo_ignore_page_lsn: true,
            ..DebugKnobs::default()
        },
    );
    for rec in &recs {
        replay_one(&ctx, rec);
        if rec.rmgr == RmgrId::Heap2 {
            // Replaying a HEAP2 record a second time (ignoring the page LSN)
            // changes nothing: REDO is idempotent.
            let now = page_images(&ts2, locator());
            replay_one(&again, rec);
            assert_eq!(page_images(&ts2, locator()), now, "info {:#x}", rec.info);
        }
    }
    ts2.assert_clean();
    assert_eq!(
        page_images(&ts2, locator()),
        want,
        "REDO rebuilds identical bytes"
    );
}

#[test]
fn redo_rebuilds_the_same_bytes_without_images() {
    check_redo_matches(false);
}

#[test]
fn redo_rebuilds_the_same_bytes_with_images() {
    check_redo_matches(true);
}

#[test]
fn the_workload_really_reuses_line_pointers_and_frees_space() {
    let w = World::new();
    run_workload(&w, false);
    let imgs = page_images(&w.ts, locator());
    let mut p0 = Page::zeroed();
    p0.0.copy_from_slice(&imgs[0]);
    // Deleted rows 2 and 3 were on page 0: after the third stage the slots are
    // LP_UNUSED (or reused by the final insert), never LP_DEAD.
    assert!((1..=p0.max_offset()).all(|o| p0.item_id(o).unwrap().flags != LpFlags::Dead));
    assert!(p0.max_offset() >= 4);
    // Frozen: remaining tuples carry the frozen bits (xmin 4 < cutoff 100)
    // except the one inserted last by xid 6.
    let frozen = (1..=p0.max_offset())
        .filter(|&o| p0.item_id(o).unwrap().flags == LpFlags::Normal)
        .filter(|&o| hdr_at(&p0, o).infomask & FROZEN == FROZEN)
        .count();
    assert!(frozen >= 3);
}

#[test]
fn freeze_skip_wal_knob_drops_the_freezing_from_the_record_only() {
    let w = World::with_knobs(DebugKnobs {
        freeze_skip_wal: true,
        ..DebugKnobs::default()
    });
    w.standard_xids();
    let h = w.ts.heap();
    let rel = rel_handle();
    h.create_storage(&wctx(3), locator()).unwrap();
    h.insert(&rel, &wctx(10), &row(1, 10)).unwrap();
    assert!(prune_block(&w, 0, Some(100)).is_some());
    let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
    let on_page = hdr_at(&buf.read().unwrap(), 1).infomask & FROZEN;
    assert_eq!(on_page, FROZEN, "the page was frozen");
    let recs = wal_records(&w.ts);
    let rec = recs
        .iter()
        .find(|r| r.rmgr == RmgrId::Heap2)
        .expect("a HEAP2 record");
    assert_eq!(PruneMain::decode(&rec.main).unwrap().nfrozen, 0);
}

#[test]
fn prune_and_log_writes_nothing_for_an_empty_plan() {
    let w = World::new();
    w.standard_xids();
    let h = w.ts.heap();
    h.create_storage(&wctx(3), locator()).unwrap();
    h.insert(&rel_handle(), &wctx(10), &row(1, 10)).unwrap();
    let before = w.ts.wal().insert_lsn();
    assert!(prune_block(&w, 0, None).is_none());
    assert_eq!(w.ts.wal().insert_lsn(), before);
}

#[test]
fn describe_summarises_each_record() {
    let w = World::new();
    run_workload(&w, false);
    let recs = wal_records(&w.ts);
    let d: Vec<String> = recs
        .iter()
        .filter(|r| r.rmgr == RmgrId::Heap2)
        .map(wal2::describe)
        .collect();
    assert!(d.iter().any(|s| s.starts_with("PRUNE_FREEZE ndead:")));
    assert!(d.iter().any(|s| s.starts_with("VACUUM_UNUSED nunused:")));
}

#[test]
fn redo_rejects_malformed_heap2_records() {
    let w = World::new();
    run_workload(&w, false);
    let recs = wal_records(&w.ts);
    let rec = recs
        .iter()
        .find(|r| r.rmgr == RmgrId::Heap2 && r.info == HEAP2_PRUNE_FREEZE)
        .unwrap()
        .clone();
    let ts2 = w.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
    ts2.smgr().set_recovery_mode(true);
    let ctx = redo_ctx(&ts2, DebugKnobs::default());
    for r in &recs {
        if r.start < rec.start {
            replay_one(&ctx, r);
        }
    }
    let mut bad = rec.clone();
    bad.main.pop();
    assert_eq!(
        wal2::redo(&ctx, &bad).unwrap_err().severity,
        Severity::Panic
    );
    let mut bad = rec.clone();
    bad.info = HEAP2_MULTI_INSERT;
    assert!(wal2::redo(&ctx, &bad).is_err());
    let mut bad = rec.clone();
    bad.blocks.clear();
    assert!(wal2::redo(&ctx, &bad).is_err());
    let mut bad = rec;
    bad.blocks[0].image = None;
    bad.blocks[0].data.push(0);
    assert!(
        wal2::redo(&ctx, &bad).is_err(),
        "block data longer than the main data says"
    );
    ts2.assert_clean();
}

// ---------------------------------------------------------------------------
// opportunistic pruning
// ---------------------------------------------------------------------------

/// A page whose free space is below `PRUNE_MIN_FREE` and whose hint is `prune_xid`.
fn full_page_with_hint(prune_xid: u32) -> Page {
    let mut p = Page::zeroed();
    p.init_heap();
    while p.free_space() >= PRUNE_MIN_FREE {
        p.add_item(&tuple(10, 0, 0)).unwrap();
    }
    p.set_prune_xid(prune_xid);
    assert!(p.free_space() < PRUNE_MIN_FREE);
    p
}

#[test]
fn candidate_conditions() {
    let procs = FakeProcs::new(200, 100);
    let next = Xid(200);
    // A new page, no hint, plenty of room: no.
    assert_eq!(candidate(&Page::zeroed(), &*procs, next), None);
    let mut room = Page::zeroed();
    room.init_heap();
    room.set_prune_xid(50);
    assert!(room.free_space() >= PRUNE_MIN_FREE);
    assert_eq!(candidate(&room, &*procs, next), None, "room enough");
    let nohint = full_page_with_hint(0);
    assert_eq!(candidate(&nohint, &*procs, next), None, "no hint");

    // The hint below the lock-free lower bound: yes, without taking the lock.
    let old = full_page_with_hint(50);
    assert_eq!(candidate(&old, &*procs, next), Some(Xid(100)));
    assert_eq!(procs.oldest_calls.load(Ordering::SeqCst), 0);
    // Not below the hint's lower bound and not below the real horizon: no.
    let young = full_page_with_hint(150);
    assert_eq!(candidate(&young, &*procs, next), None);
    // The real horizon moved on past the stale lower bound: yes.
    procs.oldest.store(300, Ordering::SeqCst);
    assert_eq!(candidate(&young, &*procs, next), Some(Xid(300)));
    // A hint of exactly the horizon is not removable.
    let edge = full_page_with_hint(100);
    procs.set_horizon(100);
    assert_eq!(candidate(&edge, &*procs, next), None);
}

#[test]
fn candidate_restores_the_high_bits_of_the_hint() {
    let procs = FakeProcs::new(0x1_0000_0010, 0x1_0000_0000);
    // Hint 0xFFFF_FFF0 belongs to the previous epoch: older than the horizon.
    let p = full_page_with_hint(0xFFFF_FFF0);
    assert_eq!(
        candidate(&p, &*procs, Xid(0x1_0000_0010)),
        Some(Xid(0x1_0000_0000))
    );
    // Hint 5 belongs to this epoch (0x1_0000_0005): older than 0x1_0000_0010 but
    // not older than a horizon of 0x1_0000_0000.
    let q = full_page_with_hint(5);
    assert_eq!(candidate(&q, &*procs, Xid(0x1_0000_0010)), None);
}

struct HookWorld {
    w: World,
    rel: RelHandle,
    tids: Vec<Tid>,
}

/// One full page of rows by xid 4, rows `deleted` deleted by xid 5, both committed,
/// `pd_prune_xid` = 5 (RW's `delete` sets it; set here by hand), and the heap
/// store wired to the fake registry.
fn hook_world(deleted: &[usize]) -> HookWorld {
    let w = World::new();
    let h = w.ts.heap();
    let rel = rel_handle();
    h.create_storage(&wctx(3), locator()).unwrap();
    w.ts.clog().ensure_page_for(Xid(5));
    let mut tids = Vec::new();
    loop {
        let t = h
            .insert(
                &rel,
                &wctx(4),
                &row(i32::try_from(tids.len()).unwrap(), 400),
            )
            .unwrap();
        if t.block != 0 {
            break;
        }
        tids.push(t);
    }
    w.commit(4);
    let s = snap_of(5);
    for &i in deleted {
        assert_eq!(
            h.delete(&rel, &wctx(5), &s, tids[i], None, None).unwrap(),
            TmResult::Ok
        );
    }
    w.commit(5);
    {
        let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
        let mut g = buf.write().unwrap();
        assert!(g.page().free_space() < PRUNE_MIN_FREE);
        g.page_mut().set_prunable(Xid(5));
        w.ts.log_change(&buf, &mut g).unwrap();
    }
    HookWorld { w, rel, tids }
}

fn lp_flags(w: &World, blk: BlockNumber) -> Vec<LpFlags> {
    let buf = w.ts.pool().read_buffer(tag(locator(), blk)).unwrap();
    let g = buf.read().unwrap();
    (1..=g.max_offset())
        .map(|o| g.item_id(o).unwrap().flags)
        .collect()
}

#[allow(clippy::many_single_char_names)]
fn scan_all(w: &World, rel: &RelHandle, snap: &Snapshot) -> Vec<i32> {
    let h = w.ts.heap();
    let mut s = h.begin_scan(rel, snap).unwrap();
    let mut v = Vec::new();
    while let Some(t) = h.scan_next(&mut s).unwrap() {
        let Datum::Int4(i) = t.row[0] else { panic!() };
        v.push(i);
    }
    v
}

fn read_snap() -> Snapshot {
    Snapshot {
        xmin: Xid(150),
        xmax: Xid(200),
        xip: vec![],
        curcid: 0,
        own_xid: None,
    }
}

#[test]
fn a_scan_prunes_the_page_and_still_returns_the_visible_rows() {
    let hw = hook_world(&[1, 4]);
    let w = &hw.w;
    // Not wired yet: the scan reads and does not prune.
    let n = i32::try_from(hw.tids.len()).unwrap();
    let want: Vec<i32> = (0..=n).filter(|i| ![1, 4].contains(i)).collect();
    assert_eq!(scan_all(w, &hw.rel, &read_snap()), want);
    assert!(lp_flags(w, 0).iter().all(|f| *f == LpFlags::Normal));

    w.wire();
    assert!(
        !w.ts.heap().set_vacuum_env(w.procs.clone(), w.multi.clone()),
        "set once"
    );
    assert_eq!(scan_all(w, &hw.rel, &read_snap()), want);
    let f = lp_flags(w, 0);
    assert_eq!(f[1], LpFlags::Dead);
    assert_eq!(f[4], LpFlags::Dead);
    assert_eq!(f.iter().filter(|x| **x == LpFlags::Dead).count(), 2);
    // The hint is cleared (nothing left that is prunable): the next scan does not retry.
    let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
    assert_eq!(buf.read().unwrap().prune_xid(), 0);
    drop(buf);
    let before = w.ts.wal().insert_lsn();
    assert_eq!(scan_all(w, &hw.rel, &read_snap()), want);
    assert_eq!(w.ts.wal().insert_lsn(), before);
    w.ts.assert_clean();
}

#[test]
fn fetch_prunes_too() {
    let hw = hook_world(&[2]);
    let w = &hw.w;
    w.wire();
    let t =
        w.ts.heap()
            .fetch(&hw.rel, &read_snap(), hw.tids[0])
            .unwrap()
            .unwrap();
    assert_eq!(t.tid, hw.tids[0]);
    assert_eq!(lp_flags(w, 0)[2], LpFlags::Dead);
    // The deleted row is gone for fetch (LP_DEAD), not an error.
    assert!(
        w.ts.heap()
            .fetch(&hw.rel, &read_snap(), hw.tids[2])
            .unwrap()
            .is_none()
    );
    w.ts.assert_clean();
}

#[test]
fn pruning_waits_for_the_horizon_and_never_removes_what_a_snapshot_can_see() {
    // P0: xid 5 deleted rows 1 and 4 and committed, but an old registered snapshot
    // (xmin 3) can still see them: the horizon is 3, so they are RecentlyDead.
    let hw = hook_world(&[1, 4]);
    let w = &hw.w;
    w.wire();
    w.procs.set_horizon(3);
    let old = Snapshot {
        xmin: Xid(3),
        xmax: Xid(5),
        xip: vec![],
        curcid: 0,
        own_xid: None,
    };
    let n = i32::try_from(hw.tids.len()).unwrap();
    let all: Vec<i32> = (0..=n).collect();
    assert_eq!(scan_all(w, &hw.rel, &old), all);
    assert_eq!(scan_all(w, &hw.rel, &read_snap()).len(), all.len() - 2);
    assert!(
        lp_flags(w, 0).iter().all(|f| *f == LpFlags::Normal),
        "nothing removed"
    );
    // The old snapshot still sees both rows through fetch as well.
    assert!(
        w.ts.heap()
            .fetch(&hw.rel, &old, hw.tids[1])
            .unwrap()
            .is_some()
    );
    // Once the snapshot is gone the horizon moves and the rows are removed.
    w.procs.set_horizon(100);
    scan_all(w, &hw.rel, &read_snap());
    assert_eq!(lp_flags(w, 0)[1], LpFlags::Dead);
}

#[test]
fn a_busy_page_is_left_alone() {
    let hw = hook_world(&[1]);
    let w = &hw.w;
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let pool = Arc::clone(w.ts.pool());
    let holder = std::thread::spawn(move || {
        // Another backend holds a shared latch on the page.
        let buf = pool.read_buffer(tag(locator(), 0)).unwrap();
        let g = buf.read().unwrap();
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(g);
    });
    started_rx.recv().unwrap();

    let ctx = PruneCtx {
        env: w.env(),
        wal: w.ts.wal(),
    };
    let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
    let g = buf.read().unwrap();
    let g = prune_before_read(&ctx, w.ts.pool(), &buf, g).unwrap();
    assert_eq!(
        g.prune_xid(),
        5,
        "try_write failed: given up, hint untouched"
    );
    drop(g);
    assert!(lp_flags(w, 0).iter().all(|f| *f == LpFlags::Normal));
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    let g = buf.read().unwrap();
    let g = prune_before_read(&ctx, w.ts.pool(), &buf, g).unwrap();
    assert_eq!(g.prune_xid(), 0);
    drop(g);
    assert_eq!(lp_flags(w, 0)[1], LpFlags::Dead);
    drop(buf);
    w.ts.assert_clean();
}

#[test]
fn prune_opt_does_not_freeze() {
    let hw = hook_world(&[1]);
    let w = &hw.w;
    let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
    let mut g = buf.write().unwrap();
    assert!(
        prune_opt(
            &w.env(),
            w.ts.pool(),
            w.ts.wal(),
            locator(),
            0,
            &mut g,
            HORIZON,
            Xid(200)
        )
        .unwrap()
    );
    let page = g.page();
    assert!(
        (1..=page.max_offset())
            .filter(|&o| page.item_id(o).unwrap().flags == LpFlags::Normal)
            .all(|o| hdr_at(page, o).infomask & FROZEN == 0)
    );
}

#[test]
fn nothing_is_pruned_while_the_page_has_room() {
    let w = World::new();
    w.standard_xids();
    let h = w.ts.heap();
    h.create_storage(&wctx(3), locator()).unwrap();
    h.insert(&rel_handle(), &wctx(4), &row(1, 10)).unwrap();
    w.wire();
    let buf = w.ts.pool().read_buffer(tag(locator(), 0)).unwrap();
    {
        let mut g = buf.write().unwrap();
        g.page_mut().set_prunable(Xid(5));
        w.ts.log_change(&buf, &mut g).unwrap();
    }
    let before = w.ts.wal().insert_lsn();
    let rel = rel_handle();
    let _ = scan_all(&w, &rel, &read_snap());
    assert_eq!(
        w.ts.wal().insert_lsn(),
        before,
        "free space >= PRUNE_MIN_FREE: no pruning"
    );
    assert_eq!(buf.read().unwrap().prune_xid(), 5);
    let _ = form_tuple(&rel.desc, &row(1, 1), &wctx(4), TupleFlags::default()).unwrap();
}

#[test]
fn proc_array_is_a_running_xids() {
    let pa = ProcArray::new(Xid(77));
    let procs: &dyn RunningXids = &*pa;
    assert_eq!(procs.next_xid(), Xid(77));
    assert!(!procs.is_in_progress(Xid(50)));
    assert!(procs.oldest_xmin() <= Xid(77));
    assert!(procs.oldest_xmin_hint() <= procs.oldest_xmin());
}
