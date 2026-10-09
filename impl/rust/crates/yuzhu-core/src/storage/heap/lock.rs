//! 行を待つ処理: タプルロックと XID の終了待ち（`m5/02-row-lock-rr.md` §5.5、§6.2、RW-1c）。
//!
//! README.tuplock のとおり 2 段で待つ。同じ行を待つ者が複数いるとき、タプルロックで先着順に並べ、
//! 先頭の者だけが保持者の XID の終了を待つ（飢餓の防止）。呼び出し時点でページのラッチもピンも
//! 持っていないこと（M5 規約 1）。

use std::sync::Arc;
use std::time::Duration;

use super::prune::RunningXids;
use crate::backend::BackendId;
use crate::error::Result;
use crate::storage::RowWait;
use crate::txn::Xid;
use crate::txn::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtx};

/// `wait_for_holders` の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitResult {
    /// 待った、または条件付きの待ちが取れた。呼び出し側は取り直して最初からやり直す。
    Waited,
    /// `Skip` / `Error` で、タプルロックか XID の終了が条件付きで取れなかった。
    WouldBlock,
}

/// タプルロック（`LockTag::Tuple` の `Exclusive`）の持ち主。`Drop` で解放する（エラー・中断でも）。
/// 解放はメモリ上の操作だけで、I/O をしない。各バックエンドが同時に持つのは高々 1 つ。
#[derive(Debug, Default)]
pub struct TupleLockGuard {
    held: Option<(Arc<LockManager>, BackendId, LockTag)>,
}

impl TupleLockGuard {
    pub fn none() -> TupleLockGuard {
        TupleLockGuard { held: None }
    }

    pub fn held(&self) -> bool {
        self.held.is_some()
    }

    /// 取ったタプルロックを預ける。すでに預かっていれば内部エラー相当（デバッグビルドで panic）。
    pub fn set(&mut self, locks: &Arc<LockManager>, backend: BackendId, tag: LockTag) {
        debug_assert!(
            self.held.is_none(),
            "a backend holds one tuple lock at a time"
        );
        self.release();
        self.held = Some((Arc::clone(locks), backend, tag));
    }

    pub fn tag(&self) -> Option<LockTag> {
        self.held.as_ref().map(|(_, _, t)| *t)
    }

    /// 解放する。持っていなければ何もしない。
    pub fn release(&mut self) {
        if let Some((locks, backend, tag)) = self.held.take() {
            locks.release(backend, tag, LockMode::Exclusive);
        }
    }
}

impl Drop for TupleLockGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// 待ちに使う共有の状態。
#[allow(missing_debug_implementations)]
#[derive(Clone, Copy)]
pub struct WaitEnv<'a> {
    pub locks: &'a Arc<LockManager>,
    pub procs: &'a dyn RunningXids,
}

/// `PG: ConditionalXactLockTableWait`。`x` がすでに終了していれば `true`、実行中なら待たずに `false`。
pub fn conditional_wait_for_xact(env: &WaitEnv<'_>, backend: BackendId, x: Xid) -> bool {
    let tag = LockTag::TransactionId(x);
    loop {
        if !env.procs.is_in_progress(x) {
            return true;
        }
        if env
            .locks
            .try_acquire(backend, tag, LockMode::Share, LockScope::Transaction)
        {
            // 取れた = 保持者が終了した（または終了の最中）。もう一度 ProcArray で確かめる。
            env.locks.release(backend, tag, LockMode::Share);
            std::thread::sleep(Duration::from_millis(1));
        } else {
            return false;
        }
    }
}

/// 衝突する保持者 `xids` の終了を待つ（必要ならタプルロックを先に取る）。
///
/// - `i_am_member`: 自分がすでにこの行のロック保持者（昇格）のときはタプルロックを取らない
///   （README.tuplock。デッドロックの回避）。
/// - `tuplock`: すでに取っていれば取り直さない。取ったら預ける。
/// - `Block` はキャンセル・`lock_timeout`・デッドロックを `Err` で返す。`tuplock` は呼び出し側の Drop で解放される。
pub fn wait_for_holders(
    env: &WaitEnv<'_>,
    wait: &WaitCtx<'_>,
    tuplock: &mut TupleLockGuard,
    tuple_tag: LockTag,
    xids: &[Xid],
    policy: RowWait,
    i_am_member: bool,
) -> Result<WaitResult> {
    #[cfg(debug_assertions)]
    crate::storage::buffer::track::assert_may_wait(crate::txn::lock::WaitSite::Acquire);

    if !i_am_member && !tuplock.held() {
        match policy {
            RowWait::Block => env.locks.acquire(
                wait.backend,
                tuple_tag,
                LockMode::Exclusive,
                LockScope::Transaction,
                wait.ctl,
            )?,
            RowWait::Skip | RowWait::Error => {
                if !env.locks.try_acquire(
                    wait.backend,
                    tuple_tag,
                    LockMode::Exclusive,
                    LockScope::Transaction,
                ) {
                    return Ok(WaitResult::WouldBlock);
                }
            }
        }
        tuplock.set(env.locks, wait.backend, tuple_tag);
    }
    for &x in xids {
        match policy {
            RowWait::Block => {
                env.locks.wait_for_xact(
                    wait.backend,
                    x,
                    &|x| env.procs.is_in_progress(x),
                    wait.ctl,
                )?;
            }
            RowWait::Skip | RowWait::Error => {
                if !conditional_wait_for_xact(env, wait.backend, x) {
                    return Ok(WaitResult::WouldBlock);
                }
            }
        }
    }
    Ok(WaitResult::Waited)
}
