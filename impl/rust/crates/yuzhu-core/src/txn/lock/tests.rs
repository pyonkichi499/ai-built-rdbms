//! `LockManager` の単体・性質テスト（01 章 §7.1 のうちデッドロック以外。LK-1）。
//!
//! デッドロックは、暫定の `find_cycle` が正しく動くことの最小の確認（2 者・早期）だけ。
//! 3 者・偽陽性・soft は LK-2 が足す。

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::table::{LockKnobs, LockTable, Request};
use super::{LockManager, LockMode, LockScope, LockStatusRow, LockTag, WaitCtl};
use crate::backend::BackendId;
use crate::error::{Result, Severity, sqlstate};
use crate::interrupt::InterruptFlag;
use crate::txn::Xid;

use LockMode::{
    AccessExclusive, AccessShare, Exclusive, RowExclusive, Share, ShareRowExclusive,
    ShareUpdateExclusive,
};

// ---- 部品 ----

fn id(n: u64) -> BackendId {
    BackendId(n)
}

fn rel(n: u32) -> LockTag {
    LockTag::Relation { db: 5, rel: n }
}

fn xact(n: u64) -> LockTag {
    LockTag::TransactionId(Xid(n))
}

const LONG: Duration = Duration::from_secs(30);

fn manager(n: u64) -> Arc<LockManager> {
    manager_with(n, LockKnobs::default())
}

fn manager_with(n: u64, knobs: LockKnobs) -> Arc<LockManager> {
    let lm = LockManager::with_lock_knobs(knobs);
    for i in 1..=n {
        lm.register_backend(id(i), 1000 + i32::try_from(i).unwrap());
    }
    lm
}

fn ctl(flag: &InterruptFlag, deadlock: Duration, lock_timeout: Option<Duration>) -> WaitCtl<'_> {
    WaitCtl {
        lock_timeout,
        deadlock_timeout: deadlock,
        interrupts: flag,
    }
}

/// 取れる前提の `acquire`（Transaction スコープ）。
fn take(lm: &LockManager, b: u64, tag: LockTag, mode: LockMode) {
    let flag = InterruptFlag::default();
    lm.acquire(
        id(b),
        tag,
        mode,
        LockScope::Transaction,
        &ctl(&flag, LONG, None),
    )
    .unwrap();
}

struct Waiter {
    flag: Arc<InterruptFlag>,
    handle: JoinHandle<Result<()>>,
}

impl Waiter {
    fn join(self) -> Result<()> {
        self.handle.join().unwrap()
    }
}

fn spawn_acquire(
    lm: &Arc<LockManager>,
    b: u64,
    tag: LockTag,
    mode: LockMode,
    deadlock: Duration,
    lock_timeout: Option<Duration>,
) -> Waiter {
    let flag = Arc::new(InterruptFlag::default());
    let (lm2, flag2) = (Arc::clone(lm), Arc::clone(&flag));
    let handle = std::thread::spawn(move || {
        lm2.acquire(
            id(b),
            tag,
            mode,
            LockScope::Transaction,
            &ctl(&flag2, deadlock, lock_timeout),
        )
    });
    Waiter { flag, handle }
}

/// 長く待つ待ち手を起こして、待ち行列に入るまで待つ。
fn spawn_waiting(lm: &Arc<LockManager>, b: u64, tag: LockTag, mode: LockMode) -> Waiter {
    let w = spawn_acquire(lm, b, tag, mode, LONG, None);
    wait_until(|| lm.waiting_on(id(b)).is_some());
    w
}

fn wait_until(cond: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < end, "condition not reached in 5s");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_still_waiting(lm: &LockManager, b: u64) {
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        lm.waiting_on(id(b)).is_some(),
        "backend {b} should still wait"
    );
}

const EXPECTED_MASK: [u8; 8] = [0x80, 0xC0, 0xF0, 0xF8, 0xEC, 0xFC, 0xFE, 0xFF];

// ---- 衝突表 ----

#[test]
fn conflict_table_matches_fixed_masks() {
    for (i, req) in LockMode::ALL.into_iter().enumerate() {
        assert_eq!(req as usize, i + 1);
        assert_eq!(req.conflict_mask(), EXPECTED_MASK[i]);
        for (j, held) in LockMode::ALL.into_iter().enumerate() {
            let expect = EXPECTED_MASK[i] & (1 << j) != 0;
            assert_eq!(req.conflicts(held), expect, "{req:?} vs {held:?}");
            assert_eq!(req.conflicts(held), held.conflicts(req), "symmetry");
        }
    }
}

#[test]
fn conflict_table_shape() {
    for m in LockMode::ALL {
        assert_eq!(AccessShare.conflicts(m), m == AccessExclusive);
        assert!(AccessExclusive.conflicts(m));
    }
    assert!(ShareUpdateExclusive.conflicts(ShareUpdateExclusive));
    assert!(!Share.conflicts(Share));
    assert!(Exclusive.conflicts(Exclusive));
    assert!(!Exclusive.conflicts(AccessShare));
    assert!(ShareRowExclusive.conflicts(ShareRowExclusive));
    assert!(!RowExclusive.conflicts(RowExclusive));
}

#[test]
fn mode_names_and_sql_words() {
    let names = [
        "AccessShareLock",
        "RowShareLock",
        "RowExclusiveLock",
        "ShareUpdateExclusiveLock",
        "ShareLock",
        "ShareRowExclusiveLock",
        "ExclusiveLock",
        "AccessExclusiveLock",
    ];
    let words = [
        "access share",
        "row share",
        "row exclusive",
        "share update exclusive",
        "share",
        "share row exclusive",
        "exclusive",
        "access exclusive",
    ];
    for (i, m) in LockMode::ALL.into_iter().enumerate() {
        assert_eq!(m.pg_name(), names[i]);
        assert_eq!(LockMode::from_sql_words(words[i]), Some(m));
        assert_eq!(m.bit(), 1 << i);
    }
    assert_eq!(
        LockMode::from_sql_words("  ACCESS   Share "),
        Some(AccessShare)
    );
    assert_eq!(LockMode::from_sql_words("bogus"), None);
    assert_eq!(LockMode::from_sql_words(""), None);
}

/// `LockManager` 経由の全 64 組で、`try_acquire` の結果が表と一致する。
fn count_table_mismatches(knobs: LockKnobs) -> usize {
    let mut bad = 0;
    for a in LockMode::ALL {
        for b in LockMode::ALL {
            let lm = manager_with(2, knobs);
            take(&lm, 1, rel(1), a);
            let got = lm.try_acquire(id(2), rel(1), b, LockScope::Transaction);
            if got == b.conflicts(a) {
                bad += 1;
            }
        }
    }
    bad
}

#[test]
fn conflict_table_via_manager() {
    assert_eq!(count_table_mismatches(LockKnobs::default()), 0);
}

#[test]
fn mutation_ignore_conflicts_is_detected() {
    let knobs = LockKnobs {
        ignore_conflicts: true,
        ..LockKnobs::default()
    };
    assert!(count_table_mismatches(knobs) > 0);
}

#[test]
fn mutation_ignore_access_exclusive_is_detected() {
    let knobs = LockKnobs {
        ignore_access_exclusive: true,
        ..LockKnobs::default()
    };
    assert!(count_table_mismatches(knobs) > 0);
}

// ---- 参照カウント・スコープ ----

#[test]
fn refcount_three_acquires_need_three_releases() {
    let lm = manager(1);
    let t = rel(1);
    for _ in 0..3 {
        take(&lm, 1, t, AccessShare);
    }
    lm.release(id(1), t, AccessShare);
    lm.release(id(1), t, AccessShare);
    assert!(lm.holds(id(1), t, AccessShare));
    lm.release(id(1), t, AccessShare);
    assert!(!lm.holds(id(1), t, AccessShare));
    assert!(lm.held_locks(id(1)).is_empty());
    assert!(lm.lock_status().is_empty());
}

#[test]
fn scopes_are_counted_separately() {
    let lm = manager(1);
    let t = rel(1);
    let flag = InterruptFlag::default();
    let c = ctl(&flag, LONG, None);
    lm.acquire(id(1), t, RowExclusive, LockScope::Transaction, &c)
        .unwrap();
    lm.acquire(id(1), t, RowExclusive, LockScope::Session, &c)
        .unwrap();
    lm.acquire(id(1), t, Share, LockScope::Transaction, &c)
        .unwrap(); // 同じバックエンドなので衝突しない
    lm.release(id(1), t, Share);
    lm.release_all(id(1), LockScope::Transaction);
    assert!(lm.holds(id(1), t, RowExclusive), "session count remains");
    assert!(!lm.holds(id(1), t, Share));
    lm.release_all(id(1), LockScope::Session);
    assert!(lm.held_locks(id(1)).is_empty());
    assert!(lm.lock_status().is_empty());
}

#[test]
fn release_prefers_transaction_scope() {
    let lm = manager(1);
    let t = rel(1);
    let flag = InterruptFlag::default();
    let c = ctl(&flag, LONG, None);
    lm.acquire(id(1), t, AccessShare, LockScope::Session, &c)
        .unwrap();
    lm.acquire(id(1), t, AccessShare, LockScope::Transaction, &c)
        .unwrap();
    lm.release(id(1), t, AccessShare); // Transaction が先に減る
    lm.release_all(id(1), LockScope::Transaction);
    assert!(lm.holds(id(1), t, AccessShare), "session count survives");
    lm.release(id(1), t, AccessShare);
    assert!(!lm.holds(id(1), t, AccessShare));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "release of a lock not held")]
fn release_of_unheld_mode_asserts() {
    let lm = manager(1);
    take(&lm, 1, rel(1), AccessShare);
    lm.release(id(1), rel(1), AccessExclusive);
}

#[test]
fn unregistered_backend() {
    let lm = manager(1);
    let flag = InterruptFlag::default();
    let err = lm
        .acquire(
            id(9),
            rel(1),
            AccessShare,
            LockScope::Transaction,
            &ctl(&flag, LONG, None),
        )
        .unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
    assert!(!lm.try_acquire(id(9), rel(1), AccessShare, LockScope::Transaction));
    lm.release(id(9), rel(1), AccessShare); // 何もしない
    lm.release_all(id(9), LockScope::Session);
    assert!(lm.lock_status().is_empty());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "unregistered while holding")]
fn unregister_with_locks_panics() {
    let lm = manager(1);
    take(&lm, 1, rel(1), AccessShare);
    lm.unregister_backend(id(1));
}

#[test]
fn unregister_after_release_is_clean() {
    let lm = manager(1);
    take(&lm, 1, rel(1), AccessShare);
    lm.release_all(id(1), LockScope::Session);
    lm.unregister_backend(id(1));
    assert!(!lm.try_acquire(id(1), rel(1), AccessShare, LockScope::Transaction));
}

// ---- 同一バックエンド・即付与・待ち ----

#[test]
fn same_backend_never_conflicts_with_itself() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    take(&lm, 1, t, AccessExclusive);
    assert!(lm.holds(id(1), t, AccessShare));
    assert!(lm.holds(id(1), t, AccessExclusive));
    assert!(!lm.try_acquire(id(2), t, AccessShare, LockScope::Transaction));
}

#[test]
fn immediate_grant_and_wait_until_release() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, RowExclusive);
    take(&lm, 2, t, RowExclusive); // 衝突しない
    let w = spawn_waiting(&lm, 3, t, Share); // Share は RowExclusive と衝突
    assert_still_waiting(&lm, 3);
    lm.release(id(1), t, RowExclusive);
    assert_still_waiting(&lm, 3);
    lm.release(id(2), t, RowExclusive);
    w.join().unwrap();
    assert!(lm.holds(id(3), t, Share));
}

#[test]
fn try_acquire_changes_nothing_on_failure() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessExclusive);
    assert!(!lm.try_acquire(id(2), t, AccessShare, LockScope::Transaction));
    assert!(lm.held_locks(id(2)).is_empty());
    assert!(lm.waiting_on(id(2)).is_none());
    assert!(!lm.has_waiters(id(1), t));
    assert_eq!(lm.lock_status().len(), 1);
    assert_eq!(lm.lock_status().len(), 1, "no empty entry is left behind");
}

// ---- FIFO と割り込み（実機 E8・E7） ----

#[test]
fn fifo_later_reader_queues_behind_waiting_writer() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let wb = spawn_waiting(&lm, 2, t, AccessExclusive);
    // C の AccessShare は A と衝突しないが、B の後ろに並ぶ
    assert!(!lm.try_acquire(id(3), t, AccessShare, LockScope::Transaction));
    let wc = spawn_waiting(&lm, 3, t, AccessShare);
    lm.release(id(1), t, AccessShare);
    wb.join().unwrap();
    assert_still_waiting(&lm, 3);
    lm.release(id(2), t, AccessExclusive);
    wc.join().unwrap();
    assert!(lm.holds(id(3), t, AccessShare));
}

#[test]
fn mutation_disable_fifo_is_detected() {
    let lm = manager_with(
        3,
        LockKnobs {
            disable_fifo: true,
            ..LockKnobs::default()
        },
    );
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let _wb = spawn_waiting(&lm, 2, t, AccessExclusive);
    assert!(lm.try_acquire(id(3), t, AccessShare, LockScope::Transaction));
    lm.release_all(id(1), LockScope::Session);
    lm.release_all(id(3), LockScope::Session);
}

#[test]
fn breakin_holder_can_upgrade_past_waiter() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let wb = spawn_waiting(&lm, 2, t, AccessExclusive);
    // A の RowExclusive は B を待たずに付与される
    assert!(lm.try_acquire(id(1), t, RowExclusive, LockScope::Transaction));
    take(&lm, 1, t, RowExclusive);
    lm.release_all(id(1), LockScope::Transaction);
    wb.join().unwrap();
}

#[test]
fn mutation_disable_breakin_is_detected() {
    let lm = manager_with(
        2,
        LockKnobs {
            disable_breakin: true,
            ..LockKnobs::default()
        },
    );
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let _wb = spawn_waiting(&lm, 2, t, AccessExclusive);
    assert!(!lm.try_acquire(id(1), t, RowExclusive, LockScope::Transaction));
    lm.release_all(id(1), LockScope::Session);
}

#[test]
fn early_deadlock_is_reported_immediately() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    take(&lm, 2, t, AccessShare);
    let wb = spawn_waiting(&lm, 2, t, AccessExclusive);
    assert!(
        !lm.try_acquire(id(1), t, AccessExclusive, LockScope::Transaction),
        "early deadlock is not available either"
    );
    let flag = InterruptFlag::default();
    let started = Instant::now();
    let err = lm
        .acquire(
            id(1),
            t,
            AccessExclusive,
            LockScope::Transaction,
            &ctl(&flag, LONG, None),
        )
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(err.sqlstate, sqlstate::DEADLOCK_DETECTED);
    let detail = err.detail.unwrap();
    let lines: Vec<&str> = detail.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        "Process 1001 waits for AccessExclusiveLock on relation 1 of database 5; blocked by process 1002."
    );
    assert_eq!(
        lines[1],
        "Process 1002 waits for AccessExclusiveLock on relation 1 of database 5; blocked by process 1001."
    );
    // 何も取り下げられていない（A はまだ AccessShare を持ち、B はまだ待つ）
    assert!(lm.holds(id(1), t, AccessShare));
    assert!(lm.waiting_on(id(1)).is_none());
    assert!(lm.waiting_on(id(2)).is_some());
    lm.release_all(id(1), LockScope::Transaction);
    lm.release_all(id(2), LockScope::Transaction); // B の AccessShare
    wb.join().unwrap();
}

// ---- 起こし方 ----

#[test]
fn waiter_withdrawal_wakes_the_queue() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    // X: AccessExclusive（A と衝突）、Y: AccessShare（X の後ろ）
    let x = spawn_acquire(
        &lm,
        2,
        t,
        AccessExclusive,
        LONG,
        Some(Duration::from_millis(100)),
    );
    wait_until(|| lm.waiting_on(id(2)).is_some());
    let y = spawn_waiting(&lm, 3, t, AccessShare);
    let err = x.join().unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    assert_eq!(err.message, "canceling statement due to lock timeout");
    y.join().unwrap();
    assert!(lm.holds(id(3), t, AccessShare));
    assert!(lm.held_locks(id(2)).is_empty());
}

#[test]
fn release_grants_only_the_head_waiter() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let x = spawn_waiting(&lm, 2, t, AccessExclusive);
    let y = spawn_waiting(&lm, 3, t, AccessShare);
    lm.release(id(1), t, AccessShare);
    x.join().unwrap();
    assert_still_waiting(&lm, 3);
    assert!(!lm.holds(id(3), t, AccessShare));
    lm.release(id(2), t, AccessExclusive);
    y.join().unwrap();
}

#[test]
fn compatible_waiters_are_granted_together() {
    let lm = manager(4);
    let t = rel(1);
    take(&lm, 1, t, AccessExclusive);
    let w2 = spawn_waiting(&lm, 2, t, AccessShare);
    let w3 = spawn_waiting(&lm, 3, t, RowExclusive);
    let w4 = spawn_waiting(&lm, 4, t, AccessShare);
    lm.release_all(id(1), LockScope::Transaction);
    w2.join().unwrap();
    w3.join().unwrap();
    w4.join().unwrap();
}

// ---- 中断 ----

fn assert_clean_after_abort(lm: &LockManager, aborted: u64, t: LockTag) {
    assert!(lm.held_locks(id(aborted)).is_empty());
    assert!(lm.waiting_on(id(aborted)).is_none());
    assert!(!lm.has_waiters(id(1), t));
}

#[test]
fn cancel_returns_57014_promptly_and_wakes_followers() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let x = spawn_waiting(&lm, 2, t, AccessExclusive);
    let y = spawn_waiting(&lm, 3, t, AccessShare);
    let started = Instant::now();
    x.flag.request_cancel();
    let err = x.join().unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(err.sqlstate, sqlstate::QUERY_CANCELED);
    assert!(err.message.contains("user request"));
    y.join().unwrap();
    assert_clean_after_abort(&lm, 2, t);
    assert!(lm.holds(id(1), t, AccessShare));
}

#[test]
fn lock_timeout_fires_near_the_deadline() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessExclusive);
    let started = Instant::now();
    let w = spawn_acquire(
        &lm,
        2,
        t,
        AccessShare,
        LONG,
        Some(Duration::from_millis(100)),
    );
    let err = w.join().unwrap_err();
    let el = started.elapsed();
    assert!(el >= Duration::from_millis(95), "{el:?}");
    assert!(el < Duration::from_millis(250), "{el:?}");
    assert_eq!(err.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    assert_clean_after_abort(&lm, 2, t);
}

#[test]
fn statement_timeout_wins_and_is_57014() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessExclusive);
    let flag = Arc::new(InterruptFlag::default());
    flag.set_statement_deadline(Some(Instant::now() + Duration::from_millis(60)));
    let (lm2, f2) = (Arc::clone(&lm), Arc::clone(&flag));
    // lock_timeout も同じ時刻に切れるが、statement_timeout の側が勝つ
    let h = std::thread::spawn(move || {
        lm2.acquire(
            id(2),
            t,
            AccessShare,
            LockScope::Transaction,
            &ctl(&f2, LONG, Some(Duration::from_millis(60))),
        )
    });
    let err = h.join().unwrap().unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::QUERY_CANCELED);
    assert!(err.message.contains("statement timeout"));
    assert_clean_after_abort(&lm, 2, t);
}

#[test]
fn terminate_returns_57p01_fatal() {
    let lm = manager(3);
    let t = rel(1);
    take(&lm, 1, t, AccessShare);
    let x = spawn_waiting(&lm, 2, t, AccessExclusive);
    let y = spawn_waiting(&lm, 3, t, AccessShare);
    x.flag.request_terminate();
    let err = x.join().unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::ADMIN_SHUTDOWN);
    assert_eq!(err.severity, Severity::Fatal);
    y.join().unwrap();
    assert_clean_after_abort(&lm, 2, t);
}

#[test]
fn grant_and_timeout_race_never_leaks_or_loses_a_lock() {
    for i in 0..120u64 {
        let lm = manager(2);
        let t = rel(1);
        take(&lm, 1, t, AccessShare);
        let lock_timeout = Duration::from_millis(8);
        let started = Instant::now();
        let w = spawn_acquire(&lm, 2, t, AccessExclusive, LONG, Some(lock_timeout));
        // 期限のちょうど前後で解放する
        let release_at = started + Duration::from_micros(2000 + i * 150);
        while Instant::now() < release_at {
            std::hint::spin_loop();
        }
        lm.release(id(1), t, AccessShare);
        let r = w.join();
        let held = lm.holds(id(2), t, AccessExclusive);
        match r {
            Ok(()) => assert!(held, "iteration {i}: Ok but the lock is not held"),
            Err(e) => {
                assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
                assert!(!held, "iteration {i}: Err but the lock is held");
            }
        }
        assert!(lm.waiting_on(id(2)).is_none());
    }
}

// ---- デッドロック（最小の確認。残りは LK-2） ----

#[test]
fn deadlock_two_parties_on_xids_victim_is_first_waiter() {
    let lm = manager(2);
    take(&lm, 1, xact(1), Exclusive);
    take(&lm, 2, xact(2), Exclusive);
    let dl = Duration::from_millis(100);
    let a = spawn_acquire(&lm, 1, xact(2), Share, dl, None);
    wait_until(|| lm.waiting_on(id(1)).is_some());
    std::thread::sleep(Duration::from_millis(30));
    let b = spawn_acquire(&lm, 2, xact(1), Share, dl, None);
    wait_until(|| lm.waiting_on(id(2)).is_some());

    let started = Instant::now();
    let err = a.join().unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(err.sqlstate, sqlstate::DEADLOCK_DETECTED);
    assert_eq!(err.message, "deadlock detected");
    assert_eq!(
        err.hint.as_deref(),
        Some("See server log for query details.")
    );
    let detail = err.detail.unwrap();
    let lines: Vec<&str> = detail.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        "Process 1001 waits for ShareLock on transaction 2; blocked by process 1002."
    );
    assert_eq!(
        lines[1],
        "Process 1002 waits for ShareLock on transaction 1; blocked by process 1001."
    );
    // A のトランザクションが終わると B が進む
    lm.release_all(id(1), LockScope::Transaction);
    b.join().unwrap();
}

#[test]
fn mutation_deadlock_detection_off_is_detected() {
    let lm = manager_with(
        2,
        LockKnobs {
            deadlock_detection_off: true,
            ..LockKnobs::default()
        },
    );
    take(&lm, 1, xact(1), Exclusive);
    take(&lm, 2, xact(2), Exclusive);
    let dl = Duration::from_millis(30);
    let lt = Some(Duration::from_millis(250));
    let a = spawn_acquire(&lm, 1, xact(2), Share, dl, lt);
    let b = spawn_acquire(&lm, 2, xact(1), Share, dl, lt);
    // 検出がないので、どちらも lock_timeout で終わる
    assert_eq!(a.join().unwrap_err().sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    assert_eq!(b.join().unwrap_err().sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
}

#[test]
fn long_chain_without_cycle_is_not_a_deadlock() {
    let lm = manager(4);
    let dl = Duration::from_millis(60);
    // D が t3 を持ち、C が t3 を待ちつつ t2 を持ち、B が t2 を待ちつつ t1 を持ち、A が t1 を待つ
    take(&lm, 4, rel(3), AccessExclusive);
    take(&lm, 3, rel(2), AccessExclusive);
    take(&lm, 2, rel(1), AccessExclusive);
    let c = spawn_acquire(&lm, 3, rel(3), AccessShare, dl, None);
    let b = spawn_acquire(&lm, 2, rel(2), AccessShare, dl, None);
    let a = spawn_acquire(&lm, 1, rel(1), AccessShare, dl, None);
    std::thread::sleep(Duration::from_millis(250));
    assert!(lm.waiting_on(id(1)).is_some());
    assert!(lm.waiting_on(id(2)).is_some());
    assert!(lm.waiting_on(id(3)).is_some());
    lm.release_all(id(4), LockScope::Transaction);
    c.join().unwrap();
    lm.release_all(id(3), LockScope::Transaction);
    b.join().unwrap();
    lm.release_all(id(2), LockScope::Transaction);
    a.join().unwrap();
}

// ---- wait_for_xact ----

#[test]
fn wait_for_xact_waits_for_the_owner() {
    let lm = manager(2);
    take(&lm, 1, xact(7), Exclusive);
    let (lm2, flag) = (Arc::clone(&lm), Arc::new(InterruptFlag::default()));
    let h = std::thread::spawn(move || {
        lm2.wait_for_xact(id(2), Xid(7), &|_| false, &ctl(&flag, LONG, None))
    });
    wait_until(|| lm.waiting_on(id(2)).is_some());
    let (tag, mode, _) = lm.waiting_on(id(2)).unwrap();
    assert_eq!(tag, xact(7));
    assert_eq!(mode, Share);
    assert_still_waiting(&lm, 2);
    lm.release_all(id(1), LockScope::Transaction);
    h.join().unwrap().unwrap();
    // Share は取ってすぐ解放されている
    assert!(lm.held_locks(id(2)).is_empty());
}

#[test]
fn wait_for_xact_returns_after_one_round_when_not_running() {
    let lm = manager(1);
    let flag = InterruptFlag::default();
    let calls = std::cell::Cell::new(0);
    lm.wait_for_xact(
        id(1),
        Xid(9),
        &|_| {
            calls.set(calls.get() + 1);
            false
        },
        &ctl(&flag, LONG, None),
    )
    .unwrap();
    assert_eq!(calls.get(), 1);
}

#[test]
fn wait_for_xact_loops_while_still_running() {
    let lm = manager(1);
    let flag = InterruptFlag::default();
    let calls = std::cell::Cell::new(0);
    let started = Instant::now();
    lm.wait_for_xact(
        id(1),
        Xid(9),
        &|_| {
            calls.set(calls.get() + 1);
            calls.get() < 3
        },
        &ctl(&flag, LONG, None),
    )
    .unwrap();
    assert_eq!(calls.get(), 3);
    assert!(
        started.elapsed() >= Duration::from_millis(1),
        "sleeps 1ms from the second round"
    );
}

#[test]
fn wait_for_xact_own_xid_is_internal_error() {
    let lm = manager(1);
    take(&lm, 1, xact(3), Exclusive);
    let flag = InterruptFlag::default();
    let err = lm
        .wait_for_xact(id(1), Xid(3), &|_| true, &ctl(&flag, LONG, None))
        .unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
}

#[test]
fn wait_for_xact_callback_runs_without_the_table_mutex() {
    let lm = manager(2);
    take(&lm, 2, rel(1), AccessShare);
    let flag = InterruptFlag::default();
    let lm2 = Arc::clone(&lm);
    lm.wait_for_xact(
        id(1),
        Xid(4),
        &move |_| {
            // Mutex を持っていれば、ここでデッドロックする
            assert_eq!(lm2.lock_status().len(), 1);
            false
        },
        &ctl(&flag, LONG, None),
    )
    .unwrap();
}

#[test]
fn wait_for_xact_honours_cancel() {
    let lm = manager(2);
    take(&lm, 1, xact(7), Exclusive);
    let flag = Arc::new(InterruptFlag::default());
    let (lm2, f2) = (Arc::clone(&lm), Arc::clone(&flag));
    let h = std::thread::spawn(move || {
        lm2.wait_for_xact(id(2), Xid(7), &|_| true, &ctl(&f2, LONG, None))
    });
    wait_until(|| lm.waiting_on(id(2)).is_some());
    flag.request_cancel();
    assert_eq!(
        h.join().unwrap().unwrap_err().sqlstate,
        sqlstate::QUERY_CANCELED
    );
}

// ---- blocking_backends / is_blocked_by ----

#[test]
fn blocking_backends_hard_soft_and_both() {
    let lm = manager(4);
    let t = rel(1);
    take(&lm, 1, t, AccessShare); // A
    assert!(lm.blocking_backends(id(2)).is_empty(), "not waiting");
    let _b = spawn_waiting(&lm, 2, t, AccessExclusive);
    let _c = spawn_waiting(&lm, 3, t, AccessShare);
    assert_eq!(lm.blocking_backends(id(2)), vec![id(1)], "hard only");
    assert_eq!(lm.blocking_backends(id(3)), vec![id(2)], "soft only (E8)");
    let _d = spawn_waiting(&lm, 4, t, AccessExclusive);
    assert_eq!(
        lm.blocking_backends(id(4)),
        vec![id(1), id(2), id(3)],
        "hard then soft"
    );
    assert!(lm.is_blocked_by(id(3), &[id(2)]));
    assert!(
        !lm.is_blocked_by(id(3), &[id(1)]),
        "A is not blocking C directly"
    );
    assert!(lm.is_blocked_by(id(4), &[id(9), id(3)]));
    assert!(!lm.is_blocked_by(id(2), &[id(3), id(4)]));
    assert!(!lm.is_blocked_by(id(1), &[id(2)]));
    lm.release_all(id(1), LockScope::Session);
}

#[test]
fn granted_but_not_awake_waiter_is_not_blocked() {
    let mut t = LockTable::new(LockKnobs::default());
    t.register(id(1), 1);
    t.register(id(2), 2);
    let tag = rel(1);
    assert!(matches!(
        t.request(id(1), tag, AccessExclusive, LockScope::Transaction, false),
        Request::Granted
    ));
    assert!(matches!(
        t.request(id(2), tag, AccessShare, LockScope::Transaction, false),
        Request::Queued
    ));
    assert_eq!(t.blockers(id(2)), vec![id(1)]);
    t.release_one(id(1), tag, AccessExclusive);
    assert!(t.blockers(id(2)).is_empty());
    assert!(t.waiting_not_granted(id(2)).is_none());
    t.check_invariants().unwrap();
    assert!(t.take_granted(id(2)));
}

// ---- lock_status ----

fn row_key(r: &LockStatusRow) -> (&'static str, u64, bool) {
    (r.locktype, r.backend.0, r.granted)
}

#[test]
fn lock_status_columns_and_order() {
    let lm = manager(3);
    take(&lm, 1, rel(20), RowExclusive);
    take(&lm, 1, rel(10), AccessShare);
    take(&lm, 1, rel(10), Share);
    take(&lm, 1, xact(0x1_0000_0007), Exclusive);
    take(
        &lm,
        1,
        LockTag::Tuple {
            db: 5,
            rel: 10,
            block: 3,
            offset: 4,
        },
        Exclusive,
    );
    take(&lm, 1, LockTag::Database(5), AccessShare);
    take(
        &lm,
        1,
        LockTag::Object {
            db: 5,
            class: 1259,
            obj: 0x2_0000_00FF,
        },
        Exclusive,
    );
    let _w = spawn_waiting(&lm, 2, rel(10), AccessExclusive);

    let rows = lm.lock_status();
    // 並びは決定的
    let again = lm.lock_status();
    assert_eq!(
        rows.iter().map(row_key).collect::<Vec<_>>(),
        again.iter().map(row_key).collect::<Vec<_>>()
    );
    assert_eq!(rows.len(), 8);

    // (database, relation, page, tuple, transactionid, backend, mode, granted desc) の昇順
    let rel10: Vec<_> = rows
        .iter()
        .filter(|r| r.locktype == "relation" && r.relation == Some(10))
        .collect();
    assert_eq!(rel10.len(), 3);
    assert_eq!(rel10[0].mode, "AccessShareLock");
    assert!(rel10[0].granted && rel10[0].waited.is_none());
    assert_eq!(rel10[1].mode, "ShareLock");
    assert_eq!(rel10[2].mode, "AccessExclusiveLock");
    assert!(!rel10[2].granted);
    assert!(rel10[2].waited.is_some());
    assert_eq!(rel10[2].backend, id(2));
    assert_eq!(rel10[2].pid, 1002);
    assert_eq!(rel10[2].database, Some(5));

    let tuple = rows.iter().find(|r| r.locktype == "tuple").unwrap();
    assert_eq!(
        (tuple.database, tuple.relation, tuple.page, tuple.tuple),
        (Some(5), Some(10), Some(3), Some(4))
    );
    let x = rows.iter().find(|r| r.locktype == "transactionid").unwrap();
    assert_eq!(x.transactionid, Some(Xid(0x1_0000_0007)));
    assert!(x.database.is_none() && x.relation.is_none());
    let objs: Vec<_> = rows.iter().filter(|r| r.locktype == "object").collect();
    assert_eq!(objs.len(), 2);
    let dbrow = objs.iter().find(|r| r.classid == Some(1262)).unwrap();
    assert_eq!(
        (dbrow.database, dbrow.objid, dbrow.objsubid),
        (Some(0), Some(5), Some(0))
    );
    let orow = objs.iter().find(|r| r.classid == Some(1259)).unwrap();
    assert_eq!(
        (orow.database, orow.objid, orow.objsubid),
        (Some(5), Some(0xFF), Some(0))
    );
    assert!(
        rows.iter()
            .filter(|r| r.locktype != "object")
            .all(|r| r.classid.is_none())
    );

    lm.release_all(id(1), LockScope::Session);
}

#[test]
fn describe_formats() {
    assert_eq!(rel(7).describe(), "relation 7 of database 5");
    assert_eq!(
        LockTag::Tuple {
            db: 5,
            rel: 7,
            block: 1,
            offset: 2
        }
        .describe(),
        "tuple (1,2) of relation 7 of database 5"
    );
    assert_eq!(xact(0x1_0000_0009).describe(), "transaction 9");
    assert_eq!(
        LockTag::Database(3).describe(),
        "object 3 of class 1262 of database 0"
    );
    assert_eq!(
        LockTag::Object {
            db: 5,
            class: 1260,
            obj: 0x1_0000_0002
        }
        .describe(),
        "object 2 of class 1260 of database 5"
    );
}

#[test]
fn has_waiters_ignores_self() {
    let lm = manager(2);
    let t = rel(1);
    take(&lm, 1, t, AccessExclusive);
    assert!(!lm.has_waiters(id(1), t));
    let w = spawn_waiting(&lm, 2, t, AccessShare);
    assert!(lm.has_waiters(id(1), t));
    assert!(!lm.has_waiters(id(2), t));
    lm.release_all(id(1), LockScope::Transaction);
    w.join().unwrap();
    assert!(!lm.has_waiters(id(1), t));
}

#[test]
fn held_locks_lists_modes() {
    let lm = manager(1);
    take(&lm, 1, rel(2), Share);
    take(&lm, 1, rel(1), AccessShare);
    take(&lm, 1, rel(1), RowExclusive);
    assert_eq!(
        lm.held_locks(id(1)),
        vec![
            (rel(1), AccessShare),
            (rel(1), RowExclusive),
            (rel(2), Share)
        ]
    );
}

// ---- 性質テスト（LockTable を 1 スレッドで直接） ----

#[derive(Clone, Debug)]
enum Op {
    Request {
        b: u8,
        tag: u8,
        mode: u8,
        session: bool,
        dont_wait: bool,
    },
    Release {
        b: u8,
        tag: u8,
        mode: u8,
    },
    Cancel {
        b: u8,
    },
    Take {
        b: u8,
    },
    ReleaseAll {
        b: u8,
        session: bool,
    },
    Wake {
        tag: u8,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..4, 0u8..3, 0u8..8, any::<bool>(), any::<bool>())
            .prop_map(|(b, tag, mode, session, dont_wait)| Op::Request { b, tag, mode, session, dont_wait }),
        2 => (0u8..4, 0u8..3, 0u8..8).prop_map(|(b, tag, mode)| Op::Release { b, tag, mode }),
        1 => (0u8..4).prop_map(|b| Op::Cancel { b }),
        2 => (0u8..4).prop_map(|b| Op::Take { b }),
        1 => (0u8..4, any::<bool>()).prop_map(|(b, session)| Op::ReleaseAll { b, session }),
        1 => (0u8..3).prop_map(|tag| Op::Wake { tag }),
    ]
}

fn tag_of(n: u8) -> LockTag {
    rel(u32::from(n) + 1)
}

fn mode_of(n: u8) -> LockMode {
    LockMode::ALL[usize::from(n)]
}

/// 待ち手の状態の写し（付与済みかどうかを含む）。
fn waiting_snapshot(t: &LockTable) -> BTreeMap<BackendId, (LockTag, bool)> {
    t.backends
        .iter()
        .filter_map(|(k, b)| b.waiting.as_ref().map(|w| (*k, (w.tag, w.granted))))
        .collect()
}

#[allow(clippy::too_many_lines)]
fn run_ops(ops: &[Op]) -> std::result::Result<(), String> {
    let mut t = LockTable::new(LockKnobs::default());
    for i in 1..=4 {
        t.register(id(i), i32::try_from(i).unwrap());
    }
    let mut seq = 0u64;
    // 待ち手の到着順と、要求時にそのタグを既に持っていたか（割り込みの対象）
    let mut arrival: BTreeMap<BackendId, (u64, bool)> = BTreeMap::new();

    for (step, op) in ops.iter().enumerate() {
        let before = waiting_snapshot(&t);
        match *op {
            Op::Request {
                b,
                tag,
                mode,
                session,
                dont_wait,
            } => {
                let who = id(u64::from(b) + 1);
                if t.backends[&who].waiting.is_some() {
                    continue;
                }
                let (tg, md) = (tag_of(tag), mode_of(mode));
                let scope = if session {
                    LockScope::Session
                } else {
                    LockScope::Transaction
                };
                let held_before = t.locks.get(&tg).map_or(0, |e| e.hold_mask(who));
                let queued: Vec<_> = t.locks.get(&tg).map_or(Vec::new(), |e| {
                    e.queue
                        .iter()
                        .filter_map(|q| t.waiting_not_granted(*q).map(|w| w.mode))
                        .collect()
                });
                match t.request(who, tg, md, scope, dont_wait) {
                    Request::Granted => {
                        if held_before == 0 && queued.iter().any(|q| md.conflicts(*q)) {
                            return Err(format!("step {step}: granted past a conflicting waiter"));
                        }
                    }
                    Request::Queued => {
                        seq += 1;
                        arrival.insert(who, (seq, held_before != 0));
                    }
                    Request::NotAvailable | Request::Deadlock(_) => {}
                }
            }
            Op::Release { b, tag, mode } => {
                t.release_one(id(u64::from(b) + 1), tag_of(tag), mode_of(mode));
            }
            Op::Cancel { b } => t.cancel_wait(id(u64::from(b) + 1)),
            Op::Take { b } => {
                t.take_granted(id(u64::from(b) + 1));
            }
            Op::ReleaseAll { b, session } => {
                let scope = if session {
                    LockScope::Session
                } else {
                    LockScope::Transaction
                };
                t.release_scope(id(u64::from(b) + 1), scope);
            }
            Op::Wake { tag } => t.wake_waiters(tag_of(tag)),
        }
        t.check_invariants()
            .map_err(|e| format!("step {step} {op:?}: {e}"))?;

        // 新しく付与された待ち手について、衝突する先着の待ち手を追い越していない
        let after = waiting_snapshot(&t);
        for (g, (gtag, granted)) in &after {
            let was_granted = before.get(g).is_some_and(|(_, gr)| *gr);
            if !*granted || was_granted {
                continue;
            }
            let (gseq, breakin) = arrival.get(g).copied().unwrap_or((0, true));
            if breakin {
                continue;
            }
            let gmode = t.backends[g].waiting.as_ref().map(|w| w.mode).unwrap();
            for (q, (qtag, qgranted)) in &after {
                if qtag != gtag || *qgranted {
                    continue;
                }
                let qmode = t.backends[q].waiting.as_ref().map(|w| w.mode).unwrap();
                let (qseq, _) = arrival.get(q).copied().unwrap_or((u64::MAX, true));
                if qseq < gseq && gmode.conflicts(qmode) {
                    return Err(format!("step {step}: {g:?} overtook {q:?} on {gtag:?}"));
                }
            }
        }

        // I4: 静止状態で全タグを起こしても、何も新しく付与されない
        let snapshot = waiting_snapshot(&t);
        let tags: HashSet<LockTag> = t.locks.keys().copied().collect();
        for tg in tags {
            t.wake_waiters(tg);
        }
        if waiting_snapshot(&t) != snapshot {
            return Err(format!(
                "step {step} {op:?}: wake_waiters granted something at rest"
            ));
        }
        t.check_invariants()
            .map_err(|e| format!("step {step} after wake: {e}"))?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn lock_table_invariants_hold(ops in proptest::collection::vec(op_strategy(), 1..80)) {
        if let Err(e) = run_ops(&ops) {
            return Err(TestCaseError::fail(e));
        }
    }
}

#[test]
fn property_runner_detects_a_broken_invariant() {
    // ignore_conflicts は I1 を壊す: 検査が働いていること自体を確かめる
    let mut t = LockTable::new(LockKnobs {
        ignore_conflicts: true,
        ..LockKnobs::default()
    });
    t.register(id(1), 1);
    t.register(id(2), 2);
    t.request(
        id(1),
        rel(1),
        AccessExclusive,
        LockScope::Transaction,
        false,
    );
    t.request(
        id(2),
        rel(1),
        AccessExclusive,
        LockScope::Transaction,
        false,
    );
    assert!(t.check_invariants().is_err());
}

// ---- DebugKnobs の写し（LK-3c） ----

#[test]
fn debug_knobs_reach_the_lock_table() {
    use crate::debug_knobs::DebugKnobs;
    let cases: [(&str, DebugKnobs); 2] = [
        (
            "lock_ignore_access_exclusive",
            DebugKnobs {
                lock_ignore_access_exclusive: true,
                ..DebugKnobs::default()
            },
        ),
        (
            "lock_ignore_conflicts",
            DebugKnobs {
                lock_ignore_conflicts: true,
                ..DebugKnobs::default()
            },
        ),
    ];
    for (name, knobs) in cases {
        let lm = LockManager::with_knobs(knobs);
        lm.register_backend(id(1), 1001);
        lm.register_backend(id(2), 1002);
        take(&lm, 1, rel(1), AccessExclusive);
        assert!(
            lm.try_acquire(id(2), rel(1), AccessShare, LockScope::Transaction),
            "{name}: AccessExclusive no longer conflicts"
        );
    }
    let lm = LockManager::with_knobs(DebugKnobs::default());
    lm.register_backend(id(1), 1001);
    lm.register_backend(id(2), 1002);
    take(&lm, 1, rel(1), AccessExclusive);
    assert!(!lm.try_acquire(id(2), rel(1), AccessShare, LockScope::Transaction));
}

#[test]
fn debug_knob_deadlock_detection_off_reaches_the_wait_loop() {
    use crate::debug_knobs::DebugKnobs;
    let lm = LockManager::with_knobs(DebugKnobs {
        deadlock_detection_off: true,
        ..DebugKnobs::default()
    });
    lm.register_backend(id(1), 1001);
    lm.register_backend(id(2), 1002);
    take(&lm, 1, xact(1), Exclusive);
    take(&lm, 2, xact(2), Exclusive);
    let dl = Duration::from_millis(30);
    let lt = Some(Duration::from_millis(250));
    let a = spawn_acquire(&lm, 1, xact(2), Share, dl, lt);
    let b = spawn_acquire(&lm, 2, xact(1), Share, dl, lt);
    assert_eq!(a.join().unwrap_err().sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    assert_eq!(b.join().unwrap_err().sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
}
