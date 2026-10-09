//! 待ちのループ（`acquire`）と `wait_for_xact`（01 章 §5.2・§5.5）。
//!
//! デッドロックの検査（`find_cycle`）とエラーの組み立て（`deadlock_error`）は、LK-2 が
//! `deadlock.rs` に移すまでの暫定の実装をこのファイルの末尾に置いている。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::table::{Hop, LockTable, Request};
use super::{LockManager, LockMode, LockScope, LockTag, WaitCtl, WaitSite, assert_may_wait};
use crate::backend::BackendId;
use crate::error::{Error, Result, sqlstate};
use crate::txn::Xid;
use crate::util::sync::{lock, wait_timeout};

/// 待ちの周期。キャンセル・各タイムアウト・`deadlock_timeout` はこの周期で確かめる。
const LOCK_WAIT_POLL: Duration = Duration::from_millis(20);

/// `55P03 canceling statement due to lock timeout`。
pub(super) fn lock_timeout_error() -> Error {
    Error::new(
        sqlstate::LOCK_NOT_AVAILABLE,
        "canceling statement due to lock timeout",
    )
}

impl LockManager {
    /// 取れるまで待つ。キャンセル → 57014、`lock_timeout` → 55P03、デッドロック → 40P01、
    /// 停止 → 57P01。成功したら `scope` のカウントが 1 増える。
    pub fn acquire(
        &self,
        id: BackendId,
        tag: LockTag,
        mode: LockMode,
        scope: LockScope,
        wait: &WaitCtl<'_>,
    ) -> Result<()> {
        assert_may_wait(WaitSite::Acquire);
        let mut t = lock(&self.inner)?;
        if !t.backends.contains_key(&id) {
            return Err(Error::internal(
                "backend not registered with the lock manager",
            ));
        }
        match t.request(id, tag, mode, scope, false) {
            Request::Granted => return Ok(()),
            Request::Deadlock(hops) => return Err(deadlock_error(&t, &hops)),
            Request::Queued => {}
            Request::NotAvailable => unreachable!("dont_wait is false"),
        }
        let Some(cv) = t.backends.get(&id).map(|b| b.cv.clone()) else {
            return Err(Error::internal("backend vanished while waiting"));
        };
        let started = Instant::now();
        let deadlock_at = started + wait.deadlock_timeout;
        let timeout_at = wait.lock_timeout.map(|d| started + d);
        let mut deadlock_checked = false;
        loop {
            let now = Instant::now();
            let mut slice = LOCK_WAIT_POLL;
            if !deadlock_checked {
                slice = slice.min(deadlock_at.saturating_duration_since(now));
            }
            if let Some(at) = timeout_at {
                slice = slice.min(at.saturating_duration_since(now));
            }
            let (g, _) = wait_timeout(&cv, t, slice.max(Duration::from_millis(1)))?;
            t = g;
            // (1) 付与が最優先（取れたロックを持ったままエラーを返さない）
            if t.take_granted(id) {
                return Ok(());
            }
            // (2) 停止 → キャンセル → statement_timeout
            if let Err(e) = wait.interrupts.check() {
                t.cancel_wait(id);
                return Err(e);
            }
            // (3) lock_timeout（1 回の acquire の待ち）
            if timeout_at.is_some_and(|at| Instant::now() >= at) {
                t.cancel_wait(id);
                return Err(lock_timeout_error());
            }
            // (4) deadlock_timeout が過ぎたら、1 回だけデッドロックを検査する
            if !deadlock_checked && Instant::now() >= deadlock_at {
                deadlock_checked = true;
                if !t.knobs.deadlock_detection_off
                    && let Some(hops) = find_cycle(&t, id)
                {
                    t.cancel_wait(id);
                    return Err(deadlock_error(&t, &hops));
                }
            }
        }
    }

    /// `XactLockTableWait`: `xid` の `Share` を取って即解放し、`still_running(xid)` が真なら
    /// 繰り返す。`still_running` は内部の `Mutex` を持たずに呼ぶ。
    pub fn wait_for_xact(
        &self,
        id: BackendId,
        xid: Xid,
        still_running: &dyn Fn(Xid) -> bool,
        wait: &WaitCtl<'_>,
    ) -> Result<()> {
        assert_may_wait(WaitSite::WaitForXact);
        let tag = LockTag::TransactionId(xid);
        if self.holds(id, tag, LockMode::Exclusive) {
            return Err(Error::internal(
                "wait_for_xact: waiting for own transaction",
            ));
        }
        let mut first = true;
        loop {
            self.acquire(id, tag, LockMode::Share, LockScope::Transaction, wait)?;
            self.release(id, tag, LockMode::Share);
            if !still_running(xid) {
                return Ok(());
            }
            if !first {
                wait.interrupts.check()?;
                std::thread::sleep(Duration::from_millis(1));
            }
            first = false;
        }
    }
}

// ---- 暫定: デッドロックの検査（LK-2 が deadlock.rs に移す） ----

/// `start` から出発して `start` に戻る循環を探す。見つかったら、循環上の各待ち手を `start` から順に返す。
/// 一度訪れたバックエンドは二度訪れない。辺は `LockTable::blockers`（hard → soft）の順に辿る。
pub(super) fn find_cycle(t: &LockTable, start: BackendId) -> Option<Vec<Hop>> {
    fn dfs(
        t: &LockTable,
        cur: BackendId,
        start: BackendId,
        visited: &mut HashSet<BackendId>,
        path: &mut Vec<Hop>,
    ) -> bool {
        if !visited.insert(cur) {
            return false;
        }
        let Some(w) = t.waiting_not_granted(cur) else {
            return false;
        };
        path.push(Hop {
            waiter: cur,
            tag: w.tag,
            mode: w.mode,
        });
        for b in t.blockers(cur) {
            if b == start || dfs(t, b, start, visited, path) {
                return true;
            }
        }
        path.pop();
        false
    }
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    dfs(t, start, start, &mut visited, &mut path).then_some(path)
}

/// `40P01 deadlock detected`。DETAIL は循環の各待ち手につき 1 行。
pub(super) fn deadlock_error(t: &LockTable, cycle: &[Hop]) -> Error {
    let detail = cycle
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let next = &cycle[(i + 1) % cycle.len()];
            format!(
                "Process {} waits for {} on {}; blocked by process {}.",
                t.pid_of(h.waiter),
                h.mode.pg_name(),
                h.tag.describe(),
                t.pid_of(next.waiter)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Error::new(sqlstate::DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(detail)
        .with_hint("See server log for query details.")
}
