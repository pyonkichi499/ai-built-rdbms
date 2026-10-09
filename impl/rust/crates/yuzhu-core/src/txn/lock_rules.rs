//! 規約 1 の検出のテスト（01 章 §6.6、§7.2「規約 1 の検出」）。
//!
//! ラッチ・許容を超えるピン・共有バリア・コミットゲート・`checkpoint_lock` のどれかを
//! 持ったまま `LockManager::acquire` / `wait_for_xact` を呼ぶと、待たない状況でも panic する。
//! 検査はスレッドごとの `track` の状態を見るので、テストは自分のスレッドで足して必ず戻す。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use super::Xid;
use super::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtl, set_wait_assertion};
use crate::backend::BackendId;
use crate::interrupt::InterruptFlag;
use crate::storage::buffer::track;

fn setup() -> Arc<LockManager> {
    set_wait_assertion(track::assert_may_wait);
    let lm = LockManager::new();
    lm.register_backend(BackendId(1), 1001);
    lm
}

fn rel() -> LockTag {
    LockTag::Relation { db: 5, rel: 1 }
}

fn acquire(lm: &LockManager) {
    let flag = InterruptFlag::default();
    lm.acquire(
        BackendId(1),
        rel(),
        LockMode::AccessShare,
        LockScope::Transaction,
        &WaitCtl {
            lock_timeout: None,
            deadlock_timeout: Duration::from_millis(50),
            interrupts: &flag,
        },
    )
    .unwrap();
    lm.release_all(BackendId(1), LockScope::Transaction);
}

fn wait_for_xact(lm: &LockManager) {
    let flag = InterruptFlag::default();
    // 他人の XID ロックはどこにも無いので、待たずに返る（検査は入口で行われる）。
    let _ = lm.wait_for_xact(
        BackendId(1),
        Xid(999),
        &|_| false,
        &WaitCtl {
            lock_timeout: None,
            deadlock_timeout: Duration::from_millis(50),
            interrupts: &flag,
        },
    );
}

/// `f` が panic し、メッセージに `needle` が入っていること。
fn panics_with(needle: &str, f: impl FnOnce()) {
    let err = catch_unwind(AssertUnwindSafe(f)).expect_err("must panic");
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(msg.contains(needle), "unexpected panic message: {msg}");
}

#[test]
fn nothing_held_is_fine() {
    let lm = setup();
    acquire(&lm);
    wait_for_xact(&lm);
}

#[test]
fn commit_gate_forbids_waiting() {
    let lm = setup();
    track::gate_acquired();
    panics_with("commit gate", || acquire(&lm));
    panics_with("commit gate", || wait_for_xact(&lm));
    track::gate_released();
    acquire(&lm);
}

#[test]
fn checkpoint_lock_forbids_waiting() {
    let lm = setup();
    track::checkpoint_lock_acquired();
    panics_with("checkpoint lock", || acquire(&lm));
    panics_with("checkpoint lock", || wait_for_xact(&lm));
    track::checkpoint_lock_released();
    acquire(&lm);
}

#[test]
fn shared_barrier_forbids_waiting() {
    let lm = setup();
    track::barrier_acquired();
    panics_with("shared storage barrier", || acquire(&lm));
    panics_with("shared storage barrier", || wait_for_xact(&lm));
    track::barrier_released();
    acquire(&lm);
}

#[test]
fn page_latch_forbids_waiting() {
    use crate::storage::smgr::ForkNumber;
    use crate::storage::testing::{TestStorage, test_rel};
    let lm = setup();
    let ts = TestStorage::new();
    let rel = test_rel(1);
    ts.create_rel(rel).unwrap();
    let buf = ts.pool().extend(rel, ForkNumber::Main).unwrap();
    let latch = buf.read().unwrap();
    panics_with("page latch", || acquire(&lm));
    panics_with("page latch", || wait_for_xact(&lm));
    drop(latch);
    // the pin of `buf` is still held; the page the wait is about is allowed
    let _ok = track::allow_pins(1);
    acquire(&lm);
}

#[test]
fn pins_beyond_the_allowance_forbid_waiting() {
    use crate::storage::smgr::ForkNumber;
    use crate::storage::testing::{TestStorage, test_rel};
    let lm = setup();
    let ts = TestStorage::new();
    let rel = test_rel(1);
    ts.create_rel(rel).unwrap();
    let p1 = ts.pool().extend(rel, ForkNumber::Main).unwrap();
    // default allowance 0: one pin is already too many
    panics_with("pins x1 (allowed 0)", || acquire(&lm));
    {
        let _allow = track::allow_pins(1);
        acquire(&lm);
        wait_for_xact(&lm);
        let p2 = ts.pool().extend(rel, ForkNumber::Main).unwrap();
        panics_with("pins x2 (allowed 1)", || acquire(&lm));
        drop(p2);
        acquire(&lm);
    }
    panics_with("pins x1 (allowed 0)", || wait_for_xact(&lm));
    drop(p1);
    acquire(&lm);
}
