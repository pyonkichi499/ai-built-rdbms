//! `find_cycle`・`deadlock_error` の単体テスト（01 章 §7.1 のデッドロック。LK-2）。
//!
//! `LockTable` を直接操作して決定的に確かめる（スレッドなし）。

use super::super::table::{LockKnobs, LockTable, Request};
use super::super::{LockMode, LockScope, LockTag};
use super::{deadlock_error, find_cycle};
use crate::backend::BackendId;
use crate::error::sqlstate;
use crate::txn::Xid;

use LockMode::{AccessExclusive, AccessShare, Exclusive, RowExclusive, Share};

fn id(n: u64) -> BackendId {
    BackendId(n)
}

fn rel(n: u32) -> LockTag {
    LockTag::Relation { db: 5, rel: n }
}

fn xact(n: u64) -> LockTag {
    LockTag::TransactionId(Xid(n))
}

fn table(n: u64) -> LockTable {
    let mut t = LockTable::new(LockKnobs::default());
    for i in 1..=n {
        assert!(t.register(id(i), 1000 + i32::try_from(i).unwrap()));
    }
    t
}

fn grant(t: &mut LockTable, b: u64, tag: LockTag, mode: LockMode) {
    assert!(matches!(
        t.request(id(b), tag, mode, LockScope::Transaction, false),
        Request::Granted
    ));
}

fn wait(t: &mut LockTable, b: u64, tag: LockTag, mode: LockMode) {
    assert!(matches!(
        t.request(id(b), tag, mode, LockScope::Transaction, false),
        Request::Queued
    ));
}

fn waiters(cycle: &[super::super::table::Hop]) -> Vec<u64> {
    cycle.iter().map(|h| h.waiter.0).collect()
}

#[test]
fn two_party_cycle_on_xids() {
    let mut t = table(2);
    grant(&mut t, 1, xact(1), Exclusive);
    grant(&mut t, 2, xact(2), Exclusive);
    wait(&mut t, 1, xact(2), Share);
    wait(&mut t, 2, xact(1), Share);

    let c = find_cycle(&t, id(1)).unwrap();
    assert_eq!(waiters(&c), [1, 2]);
    let err = deadlock_error(&t, &c);
    assert_eq!(err.sqlstate, sqlstate::DEADLOCK_DETECTED);
    assert_eq!(err.message, "deadlock detected");
    assert_eq!(
        err.hint.as_deref(),
        Some("See server log for query details.")
    );
    assert_eq!(
        err.detail.unwrap(),
        "Process 1001 waits for ShareLock on transaction 2; blocked by process 1002.\n\
         Process 1002 waits for ShareLock on transaction 1; blocked by process 1001."
    );
    // 後から待った側から検査しても循環は見つかる（start が違えば並びも start 起点）
    assert_eq!(waiters(&find_cycle(&t, id(2)).unwrap()), [2, 1]);
}

#[test]
fn two_party_cycle_on_relations() {
    let mut t = table(2);
    grant(&mut t, 1, rel(16403), AccessExclusive);
    grant(&mut t, 2, rel(16408), AccessExclusive);
    wait(&mut t, 1, rel(16408), AccessShare);
    wait(&mut t, 2, rel(16403), AccessShare);
    let c = find_cycle(&t, id(1)).unwrap();
    assert_eq!(
        deadlock_error(&t, &c).detail.unwrap(),
        "Process 1001 waits for AccessShareLock on relation 16408 of database 5; blocked by process 1002.\n\
         Process 1002 waits for AccessShareLock on relation 16403 of database 5; blocked by process 1001."
    );
}

#[test]
fn three_party_cycle() {
    let mut t = table(3);
    for b in 1..=3 {
        grant(&mut t, b, xact(b), Exclusive);
    }
    wait(&mut t, 1, xact(2), Share);
    wait(&mut t, 2, xact(3), Share);
    wait(&mut t, 3, xact(1), Share);

    let c = find_cycle(&t, id(1)).unwrap();
    assert_eq!(waiters(&c), [1, 2, 3]);
    let detail = deadlock_error(&t, &c).detail.unwrap();
    let lines: Vec<&str> = detail.lines().collect();
    assert_eq!(
        lines,
        [
            "Process 1001 waits for ShareLock on transaction 2; blocked by process 1002.",
            "Process 1002 waits for ShareLock on transaction 3; blocked by process 1003.",
            "Process 1003 waits for ShareLock on transaction 1; blocked by process 1001.",
        ]
    );
}

#[test]
fn chain_without_cycle_is_not_a_deadlock() {
    let mut t = table(4);
    grant(&mut t, 4, rel(3), AccessExclusive);
    grant(&mut t, 3, rel(2), AccessExclusive);
    grant(&mut t, 2, rel(1), AccessExclusive);
    wait(&mut t, 3, rel(3), AccessShare);
    wait(&mut t, 2, rel(2), AccessShare);
    wait(&mut t, 1, rel(1), AccessShare);
    for b in 1..=4 {
        assert!(find_cycle(&t, id(b)).is_none(), "backend {b}");
    }
}

#[test]
fn non_waiting_backend_has_no_cycle() {
    let t = table(1);
    assert!(find_cycle(&t, id(1)).is_none());
    assert!(find_cycle(&t, id(99)).is_none());
}

#[test]
fn cycle_not_through_start_is_ignored() {
    // 2 と 3 が循環し、1 はそれに巻き込まれて待つだけ。1 から見て start に戻る循環はない
    let mut t = table(3);
    grant(&mut t, 2, xact(2), Exclusive);
    grant(&mut t, 3, xact(3), Exclusive);
    wait(&mut t, 2, xact(3), Share);
    wait(&mut t, 3, xact(2), Share);
    wait(&mut t, 1, xact(2), Share);
    assert!(find_cycle(&t, id(1)).is_none());
    // 循環上の当事者からは見つかる
    assert_eq!(waiters(&find_cycle(&t, id(2)).unwrap()), [2, 3]);
}

#[test]
fn compatible_holders_are_not_false_positive() {
    // 1 と 2 は同じ表の AccessShare を共有し、互いに待たない。3 が AccessExclusive を待つだけ
    let mut t = table(3);
    grant(&mut t, 1, rel(1), AccessShare);
    grant(&mut t, 2, rel(1), AccessShare);
    wait(&mut t, 3, rel(1), AccessExclusive);
    for b in 1..=3 {
        assert!(find_cycle(&t, id(b)).is_none());
    }
}

#[test]
fn soft_edge_cycle_is_reported() {
    // 1 は rel1 に AccessShare を保持し、rel2 を待つ（hard で 3 を待つ）。
    // 2 は rel1 の AccessExclusive を待つ（hard で 1 を待つ）。
    // 3 は rel2 を保持し、rel1 の AccessShare を待つ。2 の後ろに並ぶので 2 に soft 辺を持つ。
    // 循環 1 -> 3 (hard) -> 2 (soft) -> 1 (hard)。PG なら並べ替えで解消する形（LK-D1）
    let mut t = table(3);
    grant(&mut t, 1, rel(1), AccessShare);
    grant(&mut t, 3, rel(2), AccessExclusive);
    wait(&mut t, 2, rel(1), AccessExclusive);
    wait(&mut t, 3, rel(1), AccessShare);
    wait(&mut t, 1, rel(2), AccessShare);

    assert_eq!(t.blockers(id(3)), [id(2)], "3 has only a soft edge to 2");
    let c = find_cycle(&t, id(1)).unwrap();
    assert_eq!(waiters(&c), [1, 3, 2]);
    assert_eq!(
        deadlock_error(&t, &c).detail.unwrap(),
        "Process 1001 waits for AccessShareLock on relation 2 of database 5; blocked by process 1003.\n\
         Process 1003 waits for AccessShareLock on relation 1 of database 5; blocked by process 1002.\n\
         Process 1002 waits for AccessExclusiveLock on relation 1 of database 5; blocked by process 1001."
    );
}

#[test]
fn queue_only_wait_without_cycle_is_not_a_deadlock() {
    // 1 が保持、2 が待ち、3 が 2 の後ろで待つ（soft）。3 を待つ者はいない
    let mut t = table(3);
    grant(&mut t, 1, rel(1), AccessShare);
    wait(&mut t, 2, rel(1), AccessExclusive);
    wait(&mut t, 3, rel(1), AccessShare);
    for b in 1..=3 {
        assert!(find_cycle(&t, id(b)).is_none());
    }
}

#[test]
fn early_deadlock_returns_two_party_cycle() {
    // 1 と 2 が rel1 に RowExclusive を保持。2 が AccessExclusive を待つ。
    // 1 が AccessExclusive を要求すると、2 は 1 を待ち 1 も 2 を待つので待たずに失敗する
    let mut t = table(2);
    grant(&mut t, 1, rel(1), RowExclusive);
    grant(&mut t, 2, rel(1), RowExclusive);
    wait(&mut t, 2, rel(1), AccessExclusive);
    let Request::Deadlock(hops) = t.request(
        id(1),
        rel(1),
        AccessExclusive,
        LockScope::Transaction,
        false,
    ) else {
        panic!("early deadlock expected");
    };
    assert_eq!(waiters(&hops), [1, 2]);
    assert_eq!(
        deadlock_error(&t, &hops).detail.unwrap(),
        "Process 1001 waits for AccessExclusiveLock on relation 1 of database 5; blocked by process 1002.\n\
         Process 1002 waits for AccessExclusiveLock on relation 1 of database 5; blocked by process 1001."
    );
}

#[test]
fn cycle_is_deterministic() {
    let build = || {
        let mut t = table(3);
        for b in 1..=3 {
            grant(&mut t, b, xact(b), Exclusive);
        }
        wait(&mut t, 1, xact(2), Share);
        wait(&mut t, 2, xact(3), Share);
        wait(&mut t, 3, xact(1), Share);
        let c = find_cycle(&t, id(2)).unwrap();
        deadlock_error(&t, &c).detail.unwrap()
    };
    assert_eq!(build(), build());
}

#[test]
fn cancelled_waiter_breaks_cycle() {
    let mut t = table(2);
    grant(&mut t, 1, xact(1), Exclusive);
    grant(&mut t, 2, xact(2), Exclusive);
    wait(&mut t, 1, xact(2), Share);
    wait(&mut t, 2, xact(1), Share);
    t.cancel_wait(id(1));
    assert!(find_cycle(&t, id(2)).is_none());
    assert!(find_cycle(&t, id(1)).is_none());
}
