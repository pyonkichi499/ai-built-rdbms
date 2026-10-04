//! Unit tests of the buffer pool (over `SimVfs`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::*;
use crate::error::sqlstate;
use crate::storage::BLCKSZ;
use crate::storage::page::Page;
use crate::storage::smgr::{RelFileNumber, StorageManager};
use crate::storage::testing::{TestStorage, test_rel, test_tag};
use crate::storage::vfs::{
    CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, OpenMode, SimVfs, Vfs as _,
};

fn pool() -> Arc<BufferPool> {
    let smgr = Arc::new(StorageManager::new(Arc::new(SimVfs::new(1)), 131_072));
    BufferPool::new(16, smgr, Arc::new(NoWal))
}

fn fault(op: FaultOp, nth: Option<u64>, effect: FaultEffect) -> FaultPlan {
    FaultPlan {
        rules: vec![FaultRule {
            op,
            path_prefix: None,
            nth,
            probability: None,
            effect,
        }],
    }
}

/// Extends `rel`, turns the new block into a heap page holding one 8-byte
/// item `value` and returns its block number.
fn add_page(ts: &TestStorage, rel: RelFileLocator, value: u64) -> BlockNumber {
    let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
    let mut g = buf.write().unwrap();
    g.page_mut().init_heap();
    g.page_mut().add_item(&value.to_le_bytes()).unwrap();
    buf.tag().block
}

fn value_of(ts: &TestStorage, rel: RelFileLocator, block: BlockNumber) -> u64 {
    let buf = ts.pool().read_buffer(test_tag(rel, block)).unwrap();
    let g = buf.read().unwrap();
    u64::from_le_bytes(g.item(1).unwrap().try_into().unwrap())
}

fn set_value(ts: &TestStorage, rel: RelFileLocator, block: BlockNumber, v: u64) {
    let buf = ts.pool().read_buffer(test_tag(rel, block)).unwrap();
    let mut g = buf.write().unwrap();
    g.page_mut()
        .item_mut(1)
        .unwrap()
        .copy_from_slice(&v.to_le_bytes());
}

fn disk_page(ts: &TestStorage, rel: RelFileLocator, block: BlockNumber) -> Page {
    let path = format!("base/{}/{}", rel.db_oid, rel.rel_number.0);
    let bytes = ts.vfs.file_contents(Path::new(&path)).unwrap();
    let off = block as usize * BLCKSZ;
    Page(bytes[off..off + BLCKSZ].try_into().unwrap())
}

#[test]
fn critical_section_escalates_errors_to_panic() {
    let p = pool();
    let cs = CriticalSection::enter(&p);
    assert!(!p.poison_flag().is_set());
    let e = cs.escalate(Error::internal("boom"));
    assert_eq!(e.severity, Severity::Panic);
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    assert!(p.poison_flag().is_set());
}

#[test]
fn critical_section_poisons_on_panic_only() {
    let p = pool();
    drop(CriticalSection::enter(&p));
    assert!(!p.poison_flag().is_set());
    let p2 = Arc::clone(&p);
    let r = std::thread::spawn(move || {
        let _cs = CriticalSection::enter(&p2);
        panic!("inside the critical section");
    })
    .join();
    assert!(r.is_err());
    assert!(p.poison_flag().is_set());
}

#[test]
fn nowal_is_a_no_op() {
    let w = NoWal;
    assert!(w.flush_to(100).is_ok());
    assert_eq!(w.redo_ptr(), 0);
}

#[test]
fn extend_gives_zero_pages_and_numbers_blocks() {
    let ts = TestStorage::new();
    let rel = test_rel(1000);
    ts.create_rel(rel).unwrap();
    for i in 0..3 {
        let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
        assert_eq!(buf.tag().block, i);
        assert!(buf.read().unwrap().is_all_zero());
    }
    assert_eq!(ts.pool().nblocks(rel, ForkNumber::Main).unwrap(), 3);
    assert_eq!(
        ts.pool().dirty_frames(),
        0,
        "a zero extension is already on disk"
    );
    ts.assert_clean();
}

#[test]
fn read_write_roundtrip_hits_and_reads() {
    let ts = TestStorage::new();
    let rel = test_rel(1001);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 41);
    assert_eq!(ts.pool().dirty_frames(), 1);
    assert_eq!(value_of(&ts, rel, blk), 41);
    let s = ts.pool().stats();
    assert_eq!((s.hits, s.reads), (1, 0));
    // Flush, restart the cache, read from disk.
    ts.pool().flush_all_for_checkpoint().unwrap();
    let ts2 = TestStorage::over(ts.vfs.clone(), ts.options).unwrap();
    assert_eq!(value_of(&ts2, rel, blk), 41);
    assert_eq!(ts2.pool().stats().reads, 1);
    ts2.assert_clean();
}

#[test]
fn pins_and_clone() {
    let ts = TestStorage::new();
    let rel = test_rel(1002);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 1);
    assert_eq!(ts.pool().pinned_frames(), 0);
    let a = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    let b = a.clone();
    assert_eq!(ts.pool().pinned_frames(), 1);
    assert_eq!(track::pins_held(), 2);
    drop(a);
    assert_eq!(ts.pool().pinned_frames(), 1);
    drop(b);
    assert_eq!(ts.pool().pinned_frames(), 0);
    assert_eq!(track::pins_held(), 0);
}

#[test]
fn flush_puts_valid_checksums_on_disk_but_not_on_zero_pages() {
    let ts = TestStorage::new();
    let rel = test_rel(1003);
    ts.create_rel(rel).unwrap();
    let b0 = add_page(&ts, rel, 7);
    let b1 = ts.pool().extend(rel, ForkNumber::Main).unwrap().tag().block;
    // Dirty-but-all-zero cannot happen through page_mut with init, so mark
    // the second block dirty without changing it.
    {
        let buf = ts.pool().read_buffer(test_tag(rel, b1)).unwrap();
        let _ = buf.write().unwrap().page_mut();
    }
    let st = ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!(st.written, 2);
    let p0 = disk_page(&ts, rel, b0);
    assert!(p0.verify(b0).is_ok());
    assert_ne!(p0.checksum(), 0);
    let p1 = disk_page(&ts, rel, b1);
    assert!(p1.is_all_zero(), "zero pages get no checksum (D12)");
    assert!(p1.verify(b1).is_ok());
    assert_eq!(ts.pool().dirty_frames(), 0);
    // Flushing again has nothing to do.
    let st = ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!((st.written, st.skipped_clean), (0, 0));
}

#[test]
fn eviction_keeps_every_page() {
    let ts = TestStorage::small(4, 131_072);
    let rel = test_rel(1004);
    ts.create_rel(rel).unwrap();
    for i in 0..20 {
        assert_eq!(add_page(&ts, rel, 100 + i), u32::try_from(i).unwrap());
    }
    for i in 0..20u32 {
        assert_eq!(value_of(&ts, rel, i), 100 + u64::from(i), "block {i}");
    }
    let s = ts.pool().stats();
    assert!(s.evictions >= 16, "{s:?}");
    assert!(s.writes >= 16, "{s:?}");
    ts.assert_clean();
}

#[test]
fn hot_pages_survive_the_clock_sweep() {
    let ts = TestStorage::small(3, 131_072);
    let rel = test_rel(1005);
    ts.create_rel(rel).unwrap();
    for i in 0..4 {
        add_page(&ts, rel, i);
    }
    ts.pool().flush_all_for_checkpoint().unwrap();
    // Block 0 becomes hot (usage_count 5).
    for _ in 0..6 {
        value_of(&ts, rel, 0);
    }
    value_of(&ts, rel, 1);
    value_of(&ts, rel, 2);
    value_of(&ts, rel, 3);
    value_of(&ts, rel, 1);
    value_of(&ts, rel, 2);
    let before = ts.pool().stats();
    value_of(&ts, rel, 0);
    let after = ts.pool().stats();
    assert_eq!(after.hits, before.hits + 1, "the hot page was not evicted");
}

#[test]
fn no_unpinned_buffers_available_when_everything_is_pinned() {
    let ts = TestStorage::small(4, 131_072);
    let rel = test_rel(1006);
    ts.create_rel(rel).unwrap();
    for i in 0..5 {
        add_page(&ts, rel, i);
    }
    let held: Vec<_> = (0..4)
        .map(|i| ts.pool().read_buffer(test_tag(rel, i)).unwrap())
        .collect();
    let e = ts.pool().read_buffer(test_tag(rel, 4)).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    assert_eq!(e.message, "no unpinned buffers available");
    assert_eq!(e.severity, Severity::Error);
    let e = ts.pool().extend(rel, ForkNumber::Main).unwrap_err();
    assert_eq!(e.message, "no unpinned buffers available");
    // The failed extend left the relation as it was.
    assert_eq!(ts.pool().nblocks(rel, ForkNumber::Main).unwrap(), 5);
    drop(held);
    assert_eq!(value_of(&ts, rel, 4), 4);
    ts.assert_clean();
}

#[test]
fn concurrent_reads_of_one_block_do_one_io() {
    let ts = TestStorage::new();
    let rel = test_rel(1007);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 9);
    ts.pool().flush_all_for_checkpoint().unwrap();
    let ts2 = Arc::new(TestStorage::over(ts.vfs.clone(), ts.options).unwrap());
    // Make the read slow so that the threads pile up.
    ts.vfs.set_faults(fault(
        FaultOp::Read,
        None,
        FaultEffect::Delay(Duration::from_millis(80)),
    ));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let t = Arc::clone(&ts2);
            std::thread::spawn(move || value_of(&t, rel, blk))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 9);
    }
    let s = ts2.pool().stats();
    assert_eq!(s.reads, 1, "{s:?}");
    assert_eq!(s.hits, 5, "{s:?}");
    assert_eq!(ts2.pool().pinned_frames(), 0);
}

#[test]
fn corrupt_pages_are_xx001_and_leave_the_pool_usable() {
    let ts = TestStorage::new();
    let rel = test_rel(1008);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    add_page(&ts, rel, 2);
    ts.pool().flush_all_for_checkpoint().unwrap();
    // Flip a byte of block 0 on "disk" behind the pool's back.
    let ts2 = TestStorage::over(ts.vfs.clone(), ts.options).unwrap();
    let f = ts
        .vfs
        .open(Path::new("base/5/1008"), OpenMode::ReadWrite)
        .unwrap();
    let _ = &f;
    let mut b = [0u8; 1];
    f.read_exact_at(&mut b, 5000).unwrap();
    f.write_all_at(&[b[0] ^ 0x40], 5000).unwrap();
    for _ in 0..2 {
        let e = ts2.pool().read_buffer(test_tag(rel, 0)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert_eq!(e.message, "invalid page in block 0 of relation base/5/1008");
        assert!(e.detail.unwrap().contains("checksum"));
        assert_eq!(ts2.pool().pinned_frames(), 0);
    }
    assert_eq!(value_of(&ts2, rel, 1), 2);
    // Both failures went to disk: nothing was cached for the bad block.
    assert_eq!(ts2.pool().stats().reads, 1);
}

#[test]
fn bit_flip_on_read_is_detected() {
    let ts = TestStorage::new();
    let rel = test_rel(1009);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    ts.pool().flush_all_for_checkpoint().unwrap();
    let ts2 = TestStorage::over(ts.vfs.clone(), ts.options).unwrap();
    ts.vfs
        .set_faults(fault(FaultOp::Read, Some(1), FaultEffect::BitFlipOnRead));
    let e = ts2.pool().read_buffer(test_tag(rel, 0)).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
    // The next read is clean.
    assert_eq!(value_of(&ts2, rel, 0), 1);
}

#[test]
fn torn_writes_are_never_returned_silently() {
    for seed in 0..20u64 {
        let ts = TestStorage::with_options(crate::storage::testing::TestStorageOptions {
            seed,
            ..Default::default()
        })
        .unwrap();
        let rel = test_rel(1010);
        ts.create_rel(rel).unwrap();
        for i in 0..4 {
            add_page(&ts, rel, i);
        }
        ts.pool().flush_all_for_checkpoint().unwrap();
        ts.smgr().sync_pending().unwrap();
        // Rewrite every block with new content, flush without fsync, crash.
        for i in 0..4 {
            set_value(&ts, rel, i, 1000 + u64::from(i));
        }
        ts.pool().flush_all_for_checkpoint().unwrap();
        let ts2 = ts
            .crash_and_reopen(CrashMode::TornSectors {
                sector: 512,
                keep_probability: 0.5,
            })
            .unwrap();
        for i in 0..4u32 {
            match ts2.pool().read_buffer(test_tag(rel, i)) {
                Ok(buf) => {
                    let g = buf.read().unwrap();
                    let v = u64::from_le_bytes(g.item(1).unwrap().try_into().unwrap());
                    assert!(
                        v == u64::from(i) || v == 1000 + u64::from(i),
                        "seed {seed} block {i}: silently wrong value {v}"
                    );
                }
                Err(e) => assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED, "seed {seed}"),
            }
        }
    }
}

#[test]
fn write_failure_keeps_the_page_dirty_and_eviction_moves_on() {
    let ts = TestStorage::small(2, 131_072);
    let rel = test_rel(1011);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    add_page(&ts, rel, 2);
    add_page(&ts, rel, 3); // evicts (and writes) one of the first two
    ts.pool().flush_all_for_checkpoint().unwrap();
    for i in 0..3 {
        set_value(&ts, rel, i, 10 + u64::from(i));
    }
    // From now on every write fails.
    ts.vfs.set_faults(fault(
        FaultOp::Write,
        None,
        FaultEffect::Error(std::io::ErrorKind::Other),
    ));
    let e = ts.pool().read_buffer(test_tag(rel, 0)).unwrap_err();
    // Whichever page was missing could not be brought in: the dirty
    // victims could not be written and were not discarded.
    assert_eq!(e.message, "no unpinned buffers available");
    assert!(ts.pool().stats().write_errors >= 1);
    assert_eq!(ts.pool().dirty_frames(), 2);
    // The checkpoint reports the failure too.
    let e = ts.pool().flush_all_for_checkpoint().unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
    // Writes work again: nothing was lost.
    ts.vfs.set_faults(FaultPlan::default());
    for i in 0..3u32 {
        assert_eq!(value_of(&ts, rel, i), 10 + u64::from(i));
    }
    ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!(ts.pool().dirty_frames(), 0);
    ts.assert_clean();
}

#[test]
fn short_write_is_handled_like_eio() {
    let ts = TestStorage::small(2, 131_072);
    let rel = test_rel(1012);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 5);
    ts.vfs
        .set_faults(fault(FaultOp::Write, Some(1), FaultEffect::ShortWrite));
    assert!(ts.pool().flush_all_for_checkpoint().is_err());
    assert_eq!(ts.pool().dirty_frames(), 1);
    // The whole block is rewritten next time.
    ts.pool().flush_all_for_checkpoint().unwrap();
    let p = disk_page(&ts, rel, 0);
    assert!(p.verify(0).is_ok());
    assert_eq!(p.item(1).unwrap(), &5u64.to_le_bytes());
}

/// A `WalFlush` that modifies a page while the flusher is between copying
/// the page and clearing the dirty flag (the `just_dirtied` race).
#[derive(Debug, Default)]
struct RacingWal {
    target: Mutex<Option<(Weak<BufferPool>, BufferTag)>>,
    fired: AtomicBool,
}

impl WalFlush for RacingWal {
    fn flush_to(&self, _lsn: u64) -> Result<()> {
        if self.fired.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let target = self.target.lock().unwrap().clone();
        if let Some((pool, tag)) = target {
            let pool = pool.upgrade().unwrap();
            let buf = pool.read_buffer(tag)?;
            let mut g = buf.write()?;
            g.page_mut()
                .item_mut(1)
                .unwrap()
                .copy_from_slice(&999u64.to_le_bytes());
        }
        Ok(())
    }

    fn redo_ptr(&self) -> u64 {
        0
    }
}

#[test]
fn a_change_made_during_the_write_keeps_the_page_dirty() {
    let vfs = SimVfs::new(1);
    let smgr = Arc::new(StorageManager::new(Arc::new(vfs.clone()), 131_072));
    let wal = Arc::new(RacingWal::default());
    let pool = BufferPool::new(8, Arc::clone(&smgr), Arc::clone(&wal) as Arc<dyn WalFlush>);
    let rel = test_rel(1013);
    smgr.create(rel, ForkNumber::Main).unwrap();
    let tag = test_tag(rel, 0);
    {
        let buf = pool.extend(rel, ForkNumber::Main).unwrap();
        let mut g = buf.write().unwrap();
        g.page_mut().init_heap();
        g.page_mut().add_item(&1u64.to_le_bytes()).unwrap();
    }
    *wal.target.lock().unwrap() = Some((Arc::downgrade(&pool), tag));
    let st = pool.flush_all_for_checkpoint().unwrap();
    assert_eq!(st.written, 1);
    // The write carried the old value, but the page must still be dirty.
    assert_eq!(pool.dirty_frames(), 1, "the concurrent change was lost");
    let on_disk = |v: &SimVfs| {
        let bytes = v.file_contents(Path::new("base/5/1013")).unwrap();
        Page(bytes[..BLCKSZ].try_into().unwrap())
    };
    assert_eq!(on_disk(&vfs).item(1).unwrap(), &1u64.to_le_bytes());
    pool.flush_all_for_checkpoint().unwrap();
    assert_eq!(pool.dirty_frames(), 0);
    assert_eq!(on_disk(&vfs).item(1).unwrap(), &999u64.to_le_bytes());
}

#[test]
fn checkpoint_counts_written_pages() {
    let ts = TestStorage::new();
    let rel = test_rel(1014);
    ts.create_rel(rel).unwrap();
    for i in 0..5 {
        add_page(&ts, rel, i);
    }
    let st = ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!((st.written, st.skipped_clean), (5, 0));
    set_value(&ts, rel, 2, 77);
    let st = ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!(st.written, 1);
}

#[test]
fn poisoned_latch_is_a_panic_error_and_blocks_writes() {
    let ts = TestStorage::new();
    let rel = test_rel(1015);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    add_page(&ts, rel, 2);
    ts.pool().flush_all_for_checkpoint().unwrap();
    let pool = Arc::clone(ts.pool());
    let r = std::thread::spawn(move || {
        let buf = pool.read_buffer(test_tag(rel, 0)).unwrap();
        let mut g = buf.write().unwrap();
        g.page_mut().0[100] = 0xEE;
        panic!("while holding the write latch");
    })
    .join();
    assert!(r.is_err());
    assert_eq!(ts.pool().dirty_frames(), 0, "no dirty mark while panicking");
    assert_eq!(
        ts.pool().pinned_frames(),
        0,
        "the pin was released by unwinding"
    );

    let buf = ts.pool().read_buffer(test_tag(rel, 0)).unwrap();
    for e in [
        buf.read().err().unwrap(),
        buf.write().err().unwrap(),
        buf.try_write().err().unwrap(),
    ] {
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.message, "buffer content lock poisoned");
    }
    assert!(ts.pool().is_poisoned());
    drop(buf);
    // The cluster is poisoned: no more writes, not even of healthy pages.
    set_value(&ts, rel, 1, 55);
    let writes = ts.vfs.stats().writes;
    let e = ts.pool().flush_all_for_checkpoint().unwrap_err();
    assert_eq!(e.severity, Severity::Panic);
    assert_eq!(ts.vfs.stats().writes, writes);
    assert_eq!(disk_page(&ts, rel, 1).item(1).unwrap(), &2u64.to_le_bytes());
}

#[test]
fn drop_relation_buffers_discards_without_writing() {
    let ts = TestStorage::new();
    let (r1, r2) = (test_rel(1020), test_rel(1021));
    ts.create_rel(r1).unwrap();
    ts.create_rel(r2).unwrap();
    add_page(&ts, r1, 1);
    add_page(&ts, r2, 2);
    ts.pool().flush_all_for_checkpoint().unwrap();
    set_value(&ts, r1, 0, 100);
    set_value(&ts, r2, 0, 200);
    assert_eq!(ts.pool().dirty_frames(), 2);
    let writes = ts.pool().stats().writes;
    // A pinned buffer blocks the drop.
    let pin = ts.pool().read_buffer(test_tag(r1, 0)).unwrap();
    assert_eq!(
        ts.pool().drop_relation_buffers(r1).unwrap_err().sqlstate,
        sqlstate::INTERNAL_ERROR
    );
    drop(pin);
    ts.pool().drop_relation_buffers(r1).unwrap();
    assert_eq!(ts.pool().stats().writes, writes);
    assert_eq!(ts.pool().dirty_frames(), 1, "only r2 stays dirty");
    assert_eq!(value_of(&ts, r1, 0), 1, "the dirty change was discarded");
    assert_eq!(value_of(&ts, r2, 0), 200);
    ts.assert_clean();
}

#[test]
fn flush_relation_buffers_writes_only_that_relation() {
    let ts = TestStorage::new();
    let (r1, r2) = (test_rel(1022), test_rel(1023));
    ts.create_rel(r1).unwrap();
    ts.create_rel(r2).unwrap();
    add_page(&ts, r1, 1);
    add_page(&ts, r2, 2);
    ts.pool().flush_relation_buffers(r1).unwrap();
    assert_eq!(ts.pool().dirty_frames(), 1);
    assert!(disk_page(&ts, r1, 0).verify(0).is_ok());
    assert!(disk_page(&ts, r1, 0).item(1).is_ok());
    assert!(disk_page(&ts, r2, 0).is_all_zero());
}

#[test]
fn read_buffer_zeroed_extends_the_relation() {
    let ts = TestStorage::new();
    let rel = test_rel(1024);
    ts.create_rel(rel).unwrap();
    let buf = ts.pool().read_buffer_zeroed(test_tag(rel, 3)).unwrap();
    assert!(buf.read().unwrap().is_all_zero());
    assert_eq!(ts.pool().nblocks(rel, ForkNumber::Main).unwrap(), 4);
    drop(buf);
    ts.assert_clean();
}

#[test]
fn try_write_and_hint_writes() {
    let ts = TestStorage::new();
    let rel = test_rel(1025);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 3);
    ts.pool().flush_all_for_checkpoint().unwrap();
    let buf = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    {
        let r = buf.read().unwrap();
        assert!(buf.try_write().unwrap().is_none(), "busy latch");
        drop(r);
    }
    {
        let mut g = buf.try_write().unwrap().expect("free latch");
        assert!(!g.lsn_missing());
        let _ = g.page_mut_hint();
    }
    assert_eq!(
        ts.pool().dirty_frames(),
        1,
        "hint writes dirty the buffer in M2"
    );
    ts.pool().flush_all_for_checkpoint().unwrap();
    {
        let mut g = buf.write().unwrap();
        let _ = g.page();
        assert_eq!(
            ts.pool().dirty_frames(),
            0,
            "reading through a write latch is not a change"
        );
        g.page_mut();
        assert!(g.lsn_missing());
        g.set_lsn(77);
        assert!(!g.lsn_missing());
        assert_eq!(g.page().lsn(), 77);
    }
}

#[test]
fn usage_of_a_latch_is_tracked() {
    let ts = TestStorage::new();
    let rel = test_rel(1026);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 3);
    let buf = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    assert_eq!(track::latches_held(), 0);
    {
        let _g = buf.read().unwrap();
        assert_eq!(track::latches_held(), 1);
    }
    assert_eq!(track::latches_held(), 0);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "double latch")]
fn latching_a_frame_twice_panics_in_debug_builds() {
    let ts = TestStorage::new();
    let rel = test_rel(1027);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 3);
    let buf = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    let _a = buf.read().unwrap();
    let b = buf.clone();
    let _b = b.read();
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "latch order violation")]
fn latching_blocks_in_descending_order_panics_in_debug_builds() {
    let ts = TestStorage::new();
    let rel = test_rel(1028);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 0);
    add_page(&ts, rel, 1);
    let hi = ts.pool().read_buffer(test_tag(rel, 1)).unwrap();
    let lo = ts.pool().read_buffer(test_tag(rel, 0)).unwrap();
    let _g1 = hi.write().unwrap();
    let _g0 = lo.write();
}

#[test]
fn latching_blocks_in_ascending_order_is_fine() {
    let ts = TestStorage::new();
    let rel = test_rel(1029);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 0);
    add_page(&ts, rel, 1);
    let lo = ts.pool().read_buffer(test_tag(rel, 0)).unwrap();
    let hi = ts.pool().read_buffer(test_tag(rel, 1)).unwrap();
    let _g0 = lo.write().unwrap();
    let _g1 = hi.write().unwrap();
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "while holding a page latch")]
fn extending_while_latched_panics_in_debug_builds() {
    let ts = TestStorage::new();
    let rel = test_rel(1030);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 3);
    let buf = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    let _g = buf.read().unwrap();
    let _ = ts.pool().extend(rel, ForkNumber::Main);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "buffer refcount leak")]
fn leaked_pins_are_caught_by_assert_no_pins() {
    let ts = TestStorage::new();
    let rel = test_rel(1031);
    ts.create_rel(rel).unwrap();
    let blk = add_page(&ts, rel, 3);
    let _leak = ts.pool().read_buffer(test_tag(rel, blk)).unwrap();
    assert_no_pins();
}

#[test]
fn assert_no_pins_passes_when_clean() {
    let ts = TestStorage::new();
    let rel = test_rel(1032);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 3);
    assert_no_pins();
}

#[test]
fn concurrent_extends_get_distinct_blocks() {
    let ts = Arc::new(TestStorage::small(16, 131_072));
    let rel = test_rel(1040);
    ts.create_rel(rel).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let ts = Arc::clone(&ts);
            std::thread::spawn(move || {
                (0..30u64)
                    .map(|i| {
                        let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
                        let mut g = buf.write().unwrap();
                        g.page_mut().init_heap();
                        g.page_mut()
                            .add_item(&(t * 1000 + i).to_le_bytes())
                            .unwrap();
                        (buf.tag().block, t * 1000 + i)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seen = Vec::new();
    for h in handles {
        seen.extend(h.join().unwrap());
    }
    seen.sort_unstable();
    assert_eq!(seen.len(), 120);
    for (i, (blk, _)) in seen.iter().enumerate() {
        assert_eq!(*blk as usize, i);
    }
    for (blk, v) in seen {
        assert_eq!(value_of(&ts, rel, blk), v);
    }
    ts.assert_clean();
}

#[test]
fn many_threads_counting_through_a_tiny_pool() {
    const PAGES: u32 = 24;
    const THREADS: usize = 6;
    const OPS: usize = 400;
    let ts = Arc::new(TestStorage::small(6, 131_072));
    let rel = test_rel(1041);
    ts.create_rel(rel).unwrap();
    for _ in 0..PAGES {
        add_page(&ts, rel, 0);
    }
    let total = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let ts = Arc::clone(&ts);
            let total = Arc::clone(&total);
            std::thread::spawn(move || {
                let mut x = 0x9E37_79B9_u64.wrapping_mul(t as u64 + 1);
                for _ in 0..OPS {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let blk = u32::try_from(x % u64::from(PAGES)).unwrap();
                    // Retry when every frame happens to be pinned.
                    let buf = loop {
                        match ts.pool().read_buffer(test_tag(rel, blk)) {
                            Ok(b) => break b,
                            Err(e) if e.message == "no unpinned buffers available" => {
                                std::thread::yield_now();
                            }
                            Err(e) => panic!("{e:?}"),
                        }
                    };
                    let mut g = buf.write().unwrap();
                    let item = g.page_mut().item_mut(1).unwrap();
                    let v = u64::from_le_bytes((&*item).try_into().unwrap()) + 1;
                    item.copy_from_slice(&v.to_le_bytes());
                    total.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let sum: u64 = (0..PAGES).map(|b| value_of(&ts, rel, b)).sum();
    assert_eq!(sum, (THREADS * OPS) as u64, "lost updates");
    assert_eq!(total.load(Ordering::SeqCst), THREADS * OPS);
    assert!(ts.pool().stats().evictions > 0);
    ts.assert_clean();
    // And the same after everything went through the disk.
    ts.pool().flush_all_for_checkpoint().unwrap();
    let ts2 = TestStorage::over(ts.vfs.clone(), ts.options).unwrap();
    let sum2: u64 = (0..PAGES).map(|b| value_of(&ts2, rel, b)).sum();
    assert_eq!(sum2, sum);
}

#[test]
fn checkpointing_while_threads_write_loses_nothing() {
    const PAGES: u32 = 12;
    let ts = Arc::new(TestStorage::small(8, 131_072));
    let rel = test_rel(1042);
    ts.create_rel(rel).unwrap();
    for _ in 0..PAGES {
        add_page(&ts, rel, 0);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3)
        .map(|t| {
            let ts = Arc::clone(&ts);
            std::thread::spawn(move || {
                let mut n = 0u64;
                for i in 0..300u32 {
                    let blk = (i * 7 + t) % PAGES;
                    let Ok(buf) = ts.pool().read_buffer(test_tag(rel, blk)) else {
                        continue;
                    };
                    let mut g = buf.write().unwrap();
                    let item = g.page_mut().item_mut(1).unwrap();
                    let v = u64::from_le_bytes((&*item).try_into().unwrap()) + 1;
                    item.copy_from_slice(&v.to_le_bytes());
                    n += 1;
                }
                n
            })
        })
        .collect();
    let checkpointer = {
        let ts = Arc::clone(&ts);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                ts.pool().flush_all_for_checkpoint().unwrap();
            }
        })
    };
    let done: u64 = writers.into_iter().map(|h| h.join().unwrap()).sum();
    stop.store(true, Ordering::SeqCst);
    checkpointer.join().unwrap();
    ts.pool().flush_all_for_checkpoint().unwrap();
    assert_eq!(ts.pool().dirty_frames(), 0);
    let ts2 = TestStorage::over(ts.vfs.clone(), ts.options).unwrap();
    let sum: u64 = (0..PAGES).map(|b| value_of(&ts2, rel, b)).sum();
    assert_eq!(
        sum, done,
        "updates were lost between the writers and the checkpoint"
    );
}

#[test]
fn shared_relation_tags_are_one_buffer() {
    // Shared relations use db_oid 0 (§4.3), so every database sees one frame.
    let ts = TestStorage::new();
    let shared = RelFileLocator {
        spc_oid: crate::storage::smgr::GLOBALTABLESPACE_OID,
        db_oid: 0,
        rel_number: RelFileNumber(1262),
    };
    ts.smgr().create(shared, ForkNumber::Main).unwrap();
    {
        let buf = ts.pool().extend(shared, ForkNumber::Main).unwrap();
        let mut g = buf.write().unwrap();
        g.page_mut().init_heap();
    }
    let a = ts.pool().read_buffer(test_tag(shared, 0)).unwrap();
    let b = ts.pool().read_buffer(test_tag(shared, 0)).unwrap();
    assert_eq!(ts.pool().pinned_frames(), 1);
    drop((a, b));
    ts.pool().flush_all_for_checkpoint().unwrap();
    assert!(ts.vfs.file_contents(Path::new("global/1262")).is_some());
}

#[test]
fn failed_extend_leaves_no_trace() {
    let ts = TestStorage::small(3, 131_072);
    let rel = test_rel(1050);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    ts.pool().flush_all_for_checkpoint().unwrap();
    ts.vfs
        .set_faults(fault(FaultOp::Write, Some(1), FaultEffect::ShortWrite));
    let e = ts.pool().extend(rel, ForkNumber::Main).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::DISK_FULL);
    assert_eq!(ts.pool().nblocks(rel, ForkNumber::Main).unwrap(), 1);
    assert_eq!(ts.pool().pinned_frames(), 0);
    // The reserved tag is gone: the next extend gets the same block.
    let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
    assert_eq!(buf.tag().block, 1);
    drop(buf);
    ts.assert_clean();
}

#[test]
fn failed_reads_return_their_frames() {
    let ts = TestStorage::small(2, 131_072);
    let rel = test_rel(1051);
    ts.create_rel(rel).unwrap();
    add_page(&ts, rel, 1);
    for blk in 5..30 {
        let e = ts.pool().read_buffer(test_tag(rel, blk)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
        assert_eq!(ts.pool().pinned_frames(), 0);
    }
    assert_eq!(value_of(&ts, rel, 0), 1);
    // A relation without a file.
    let e = ts
        .pool()
        .read_buffer(test_tag(test_rel(9999), 0))
        .unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::UNDEFINED_FILE);
    ts.assert_clean();
}
