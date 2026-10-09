//! Thread-local tracking of pins, latches and the storage barrier, used to
//! detect leaks and re-entrancy (`m2.md` §6.3 item 7, §2 convention 9).
//!
//! Pin counting is always on (it is cheap). The checks that panic
//! (double latch on one frame, latch-order violations, latching around
//! `extend`, re-entering the storage barrier) are compiled in with
//! `cfg(debug_assertions)`, not `cfg(test)`, so that the server's
//! integration tests get them too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;

use super::frame::FrameId;
use crate::storage::smgr::BufferTag;
use crate::txn::lock::WaitSite;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LatchMode {
    Read,
    Write,
}

#[derive(Debug)]
struct LatchRec {
    pool: usize,
    frame: FrameId,
    // Read only by the debug_assertions checks.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    tag: BufferTag,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    mode: LatchMode,
}

#[derive(Debug, Default)]
struct State {
    /// (pool id, frame) -> (pins held by this thread, tag).
    pins: HashMap<(usize, FrameId), (u32, BufferTag)>,
    /// Blocking latches in acquisition order.
    latches: Vec<LatchRec>,
    /// Depth of the shared storage barrier held by this thread.
    barrier: u32,
    /// Depth of the commit gate (shared or exclusive) held by this thread.
    gate: u32,
    /// Depth of `checkpoint_lock` held by this thread.
    checkpoint_lock: u32,
    /// Pins that may be held while waiting (see [`allow_pins`]).
    allowed_pins: usize,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R: Default>(f: impl FnOnce(&mut State) -> R) -> R {
    // `try_with` so that a Drop during thread teardown does not panic.
    STATE
        .try_with(|s| f(&mut s.borrow_mut()))
        .unwrap_or_default()
}

pub(super) fn pin_added(pool: usize, frame: FrameId, tag: BufferTag) {
    with(|s| {
        s.pins.entry((pool, frame)).or_insert((0, tag)).0 += 1;
    });
}

pub(super) fn pin_removed(pool: usize, frame: FrameId) {
    with(|s| {
        if let Some(e) = s.pins.get_mut(&(pool, frame)) {
            e.0 -= 1;
            if e.0 == 0 {
                s.pins.remove(&(pool, frame));
            }
        }
    });
}

/// Records a blocking latch about to be taken. In debug builds panics on a
/// second latch on the same frame (std's `RwLock` would hang or deadlock)
/// and on taking a latch on a block that is not greater than a latch this
/// thread already holds in the same relation fork.
pub(super) fn latch_acquired(pool: usize, frame: FrameId, tag: BufferTag, mode: LatchMode) {
    latch_acquired_inner(pool, frame, tag, mode, true);
}

/// Like [`latch_acquired`] for B+Tree pages: the double-latch check stays, the
/// ascending-block-order check is skipped (`06-btree.md` §4.3).
pub(super) fn latch_acquired_tree(pool: usize, frame: FrameId, tag: BufferTag, mode: LatchMode) {
    latch_acquired_inner(pool, frame, tag, mode, false);
}

fn latch_acquired_inner(
    pool: usize,
    frame: FrameId,
    tag: BufferTag,
    mode: LatchMode,
    ordered: bool,
) {
    #[cfg(not(debug_assertions))]
    let _ = ordered;
    #[cfg(debug_assertions)]
    {
        let violation = with(|s| {
            for h in &s.latches {
                if h.pool != pool {
                    continue;
                }
                if h.frame == frame {
                    return Some(format!(
                        "double latch on {tag:?}: already held ({:?}), requested {mode:?}",
                        h.mode
                    ));
                }
                if ordered
                    && h.tag.rel == tag.rel
                    && h.tag.fork == tag.fork
                    && tag.block < h.tag.block
                {
                    return Some(format!(
                        "latch order violation: holding block {} of the relation, requested block {}",
                        h.tag.block, tag.block
                    ));
                }
            }
            None
        });
        if let Some(msg) = violation {
            panic!("{msg}");
        }
    }
    with(|s| {
        s.latches.push(LatchRec {
            pool,
            frame,
            tag,
            mode,
        });
    });
}

/// Records a latch obtained without blocking (`try_write`); never panics.
pub(super) fn latch_acquired_nowait(pool: usize, frame: FrameId, tag: BufferTag, mode: LatchMode) {
    with(|s| {
        s.latches.push(LatchRec {
            pool,
            frame,
            tag,
            mode,
        });
    });
}

pub(super) fn latch_released(pool: usize, frame: FrameId) {
    with(|s| {
        if let Some(i) = s
            .latches
            .iter()
            .rposition(|l| l.pool == pool && l.frame == frame)
        {
            s.latches.remove(i);
        }
    });
}

/// `BufferPool::extend` must be called without holding any page latch
/// (`m2.md` §5.9). Debug builds panic.
pub(super) fn assert_no_latches_for_extend(pool: usize) {
    #[cfg(debug_assertions)]
    {
        let held = with(|s| s.latches.iter().any(|l| l.pool == pool));
        assert!(
            !held,
            "BufferPool::extend called while holding a page latch"
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = pool;
}

/// Call when this thread takes the shared storage barrier. Debug builds
/// panic if it already holds it (a writer waiting on the std `RwLock` would
/// make the second acquisition deadlock).
pub fn barrier_acquired() {
    let depth = with(|s| {
        s.barrier += 1;
        s.barrier
    });
    #[cfg(debug_assertions)]
    assert!(depth == 1, "storage barrier re-entered by the same thread");
    #[cfg(not(debug_assertions))]
    let _ = depth;
}

/// Call when this thread releases the shared storage barrier.
pub fn barrier_released() {
    with(|s| s.barrier = s.barrier.saturating_sub(1));
}

/// Whether this thread holds the shared storage barrier.
pub fn barrier_held() -> bool {
    with(|s| s.barrier > 0)
}

/// Total pins this thread holds.
pub fn pins_held() -> usize {
    with(|s| s.pins.values().map(|(n, _)| *n as usize).sum())
}

/// Page latches this thread holds.
pub fn latches_held() -> usize {
    with(|s| s.latches.len())
}

/// A description of the pins this thread holds, or `None`.
pub(super) fn describe_pins() -> Option<String> {
    with(|s| {
        if s.pins.is_empty() {
            return None;
        }
        let mut items: Vec<_> = s.pins.values().collect();
        items.sort_by_key(|(_, t)| *t);
        let mut out = String::new();
        for (n, tag) in items {
            let _ = write!(
                out,
                "[{}/{}/{} fork {:?} block {} x{n}] ",
                tag.rel.spc_oid, tag.rel.db_oid, tag.rel.rel_number.0, tag.fork, tag.block
            );
        }
        Some(out.trim_end().to_string())
    })
}

// ---- Convention 1: no waiting for a heavy lock while holding other things ----
// (`m5/01` §6.6, LK-D22 / LK-D23). The hook `assert_may_wait` is registered
// with `txn::lock::set_wait_assertion` by `Cluster::open`.

/// The commit gate (shared or exclusive) was taken by this thread.
pub fn gate_acquired() {
    with(|s| s.gate += 1);
}

pub fn gate_released() {
    with(|s| s.gate = s.gate.saturating_sub(1));
}

/// Whether this thread holds the commit gate.
pub fn gate_held() -> bool {
    with(|s| s.gate > 0)
}

/// `checkpoint::run` took `checkpoint_lock`.
pub fn checkpoint_lock_acquired() {
    with(|s| s.checkpoint_lock += 1);
}

pub fn checkpoint_lock_released() {
    with(|s| s.checkpoint_lock = s.checkpoint_lock.saturating_sub(1));
}

/// Whether this thread holds `checkpoint_lock`.
pub fn checkpoint_lock_held() -> bool {
    with(|s| s.checkpoint_lock > 0)
}

/// While the guard lives, up to `n` pins may be held when waiting (the page
/// the wait is about). Nested guards use the largest value. The heap uses it
/// right before it waits (`02`).
pub fn allow_pins(n: usize) -> AllowPinsGuard {
    let prev = with(|s| {
        let prev = s.allowed_pins;
        s.allowed_pins = prev.max(n);
        prev
    });
    AllowPinsGuard { prev }
}

#[derive(Debug)]
#[must_use = "the pins are allowed only while the guard is alive"]
pub struct AllowPinsGuard {
    prev: usize,
}

impl Drop for AllowPinsGuard {
    fn drop(&mut self) {
        with(|s| s.allowed_pins = self.prev);
    }
}

/// What this thread holds that forbids waiting for a heavy lock, or `None`.
fn held_while_waiting() -> Option<String> {
    with(|s| {
        let mut items: Vec<String> = Vec::new();
        if !s.latches.is_empty() {
            items.push(format!("page latch x{}", s.latches.len()));
        }
        let pins: usize = s.pins.values().map(|(n, _)| *n as usize).sum();
        if pins > s.allowed_pins {
            items.push(format!("pins x{pins} (allowed {})", s.allowed_pins));
        }
        if s.barrier > 0 {
            items.push("shared storage barrier".to_owned());
        }
        if s.gate > 0 {
            items.push("commit gate".to_owned());
        }
        if s.checkpoint_lock > 0 {
            items.push("checkpoint lock".to_owned());
        }
        if items.is_empty() {
            None
        } else {
            Some(items.join(", "))
        }
    })
}

/// The check behind `LockManager::acquire` / `wait_for_xact` (called at their
/// entry even when no wait is needed). Panics when this thread holds a page
/// latch, more pins than allowed, the shared storage barrier, the commit gate
/// or `checkpoint_lock`. Plain mutexes cannot be tracked; they are covered by
/// the leaf-lock rules (`00` §5.3) and review.
pub fn assert_may_wait(site: WaitSite) {
    if let Some(held) = held_while_waiting() {
        panic!("may not wait for a lock ({site:?}) while holding: {held}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::{ForkNumber, RelFileLocator, RelFileNumber};

    fn tag(rel: u32, block: u32) -> BufferTag {
        BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 1,
                rel_number: RelFileNumber(rel),
            },
            fork: ForkNumber::Main,
            block,
        }
    }

    #[test]
    fn pins_are_counted_per_frame() {
        assert_eq!(pins_held(), 0);
        pin_added(1, FrameId(0), tag(1, 0));
        pin_added(1, FrameId(0), tag(1, 0));
        pin_added(1, FrameId(3), tag(1, 7));
        assert_eq!(pins_held(), 3);
        assert!(describe_pins().unwrap().contains("block 7"));
        pin_removed(1, FrameId(0));
        pin_removed(1, FrameId(0));
        pin_removed(1, FrameId(3));
        assert_eq!(pins_held(), 0);
        assert!(describe_pins().is_none());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "double latch")]
    fn double_latch_panics() {
        latch_acquired(9, FrameId(1), tag(1, 1), LatchMode::Read);
        latch_acquired(9, FrameId(1), tag(1, 1), LatchMode::Read);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "double latch")]
    fn tree_latch_still_detects_double_latch() {
        latch_acquired_tree(6, FrameId(1), tag(1, 1), LatchMode::Read);
        latch_acquired_tree(6, FrameId(1), tag(1, 1), LatchMode::Write);
    }

    #[test]
    fn tree_latch_allows_descending_blocks() {
        latch_acquired_tree(5, FrameId(1), tag(1, 9), LatchMode::Write);
        latch_acquired_tree(5, FrameId(2), tag(1, 3), LatchMode::Write);
        latch_released(5, FrameId(2));
        latch_released(5, FrameId(1));
        assert_eq!(latches_held(), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "latch order violation")]
    fn latch_order_violation_panics() {
        latch_acquired(8, FrameId(1), tag(1, 5), LatchMode::Write);
        latch_acquired(8, FrameId(2), tag(1, 4), LatchMode::Write);
    }

    #[test]
    fn latch_order_allows_ascending_and_other_relations() {
        latch_acquired(7, FrameId(1), tag(1, 4), LatchMode::Write);
        latch_acquired(7, FrameId(2), tag(1, 5), LatchMode::Write);
        latch_acquired(7, FrameId(3), tag(2, 0), LatchMode::Write);
        assert_eq!(latches_held(), 3);
        latch_released(7, FrameId(2));
        latch_released(7, FrameId(1));
        latch_released(7, FrameId(3));
        assert_eq!(latches_held(), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "barrier re-entered")]
    fn barrier_reentry_panics() {
        barrier_acquired();
        barrier_acquired();
    }

    #[test]
    fn barrier_balanced() {
        barrier_acquired();
        assert!(barrier_held());
        barrier_released();
        assert!(!barrier_held());
    }

    #[test]
    fn may_wait_when_nothing_is_held() {
        assert_may_wait(WaitSite::Acquire);
        assert_may_wait(WaitSite::WaitForXact);
    }

    #[test]
    #[should_panic(expected = "page latch x1")]
    fn waiting_with_a_latch_panics() {
        latch_acquired(21, FrameId(1), tag(1, 1), LatchMode::Read);
        assert_may_wait(WaitSite::Acquire);
    }

    #[test]
    #[should_panic(expected = "pins x2 (allowed 0)")]
    fn waiting_with_pins_panics() {
        pin_added(22, FrameId(0), tag(1, 0));
        pin_added(22, FrameId(1), tag(1, 1));
        assert_may_wait(WaitSite::WaitForXact);
    }

    #[test]
    fn allow_pins_permits_that_many_and_restores() {
        pin_added(23, FrameId(0), tag(1, 0));
        {
            let _g = allow_pins(1);
            assert_may_wait(WaitSite::Acquire);
            {
                // Nested: the larger value wins and the outer one is restored.
                let _inner = allow_pins(0);
                assert_may_wait(WaitSite::Acquire);
            }
            assert_may_wait(WaitSite::Acquire);
        }
        let r = std::panic::catch_unwind(|| assert_may_wait(WaitSite::Acquire));
        assert!(r.is_err(), "the allowance ended with the guard");
        pin_removed(23, FrameId(0));
    }

    #[test]
    #[should_panic(expected = "shared storage barrier")]
    fn waiting_with_the_barrier_panics() {
        barrier_acquired();
        assert_may_wait(WaitSite::Acquire);
    }

    #[test]
    #[should_panic(expected = "commit gate")]
    fn waiting_with_the_gate_panics() {
        gate_acquired();
        assert_may_wait(WaitSite::Acquire);
    }

    #[test]
    #[should_panic(expected = "checkpoint lock")]
    fn waiting_with_the_checkpoint_lock_panics() {
        checkpoint_lock_acquired();
        assert_may_wait(WaitSite::WaitForXact);
    }

    #[test]
    fn gate_and_checkpoint_lock_are_balanced() {
        gate_acquired();
        gate_acquired();
        assert!(gate_held());
        gate_released();
        assert!(gate_held());
        gate_released();
        assert!(!gate_held());
        gate_released(); // saturates
        assert!(!gate_held());
        checkpoint_lock_acquired();
        assert!(checkpoint_lock_held());
        checkpoint_lock_released();
        assert!(!checkpoint_lock_held());
    }

    #[test]
    #[should_panic(expected = "Acquire")]
    fn the_message_names_the_wait_site() {
        gate_acquired();
        assert_may_wait(WaitSite::Acquire);
    }
}
