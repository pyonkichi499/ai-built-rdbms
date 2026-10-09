//! ロックマネージャ（M5 LK。契約 §4.2、01 章 §4.1〜§4.3・§5・§6.1）。
//!
//! PostgreSQL の 8 モードの重量ロックを、ロック表 1 本（`Mutex`）とバックエンドごとの
//! `Condvar` で実装する。`txn::lock` は `storage` を使わない。
//!
//! - 判定と付与・解放: `table.rs`
//! - 待ちのループ（`acquire`・`wait_for_xact`）: `wait.rs`
//! - デッドロック検出: `deadlock.rs`（LK-2。今は `wait.rs` に暫定の実装がある）

pub mod deadlock;
pub mod relation;
pub mod table;
pub mod wait;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::backend::BackendId;
use crate::debug_knobs::DebugKnobs;
use crate::interrupt::InterruptFlag;
use crate::txn::Xid;
use crate::types::Oid;
use crate::util::sync::lock_ignore_poison;

use table::{LockKnobs, LockTable, Request};

/// 要求するモード（添字 = モード − 1）が衝突する保持モードのビット集合（ビット (m − 1) が m）。
const CONFLICT_MASK: [u8; 8] = [0x80, 0xC0, 0xF0, 0xF8, 0xEC, 0xFC, 0xFE, 0xFF];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum LockTag {
    Relation {
        db: Oid,
        rel: Oid,
    },
    Tuple {
        db: Oid,
        rel: Oid,
        block: u32,
        offset: u16,
    },
    TransactionId(Xid),
    Database(Oid),
    Object {
        db: Oid,
        class: Oid,
        obj: u64,
    },
}

#[allow(clippy::cast_possible_truncation)]
fn low32(v: u64) -> u32 {
    (v & 0xFFFF_FFFF) as u32
}

impl LockTag {
    /// DETAIL の記述（PostgreSQL 17 と同じ書式）。XID は外部表現（下位 32 ビット）で出す。
    pub fn describe(&self) -> String {
        match *self {
            LockTag::Relation { db, rel } => format!("relation {rel} of database {db}"),
            LockTag::Tuple {
                db,
                rel,
                block,
                offset,
            } => format!("tuple ({block},{offset}) of relation {rel} of database {db}"),
            LockTag::TransactionId(xid) => format!("transaction {}", xid.to_external()),
            LockTag::Database(oid) => format!("object {oid} of class 1262 of database 0"),
            LockTag::Object { db, class, obj } => {
                format!("object {} of class {class} of database {db}", low32(obj))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum LockMode {
    AccessShare = 1,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

impl LockMode {
    pub const ALL: [LockMode; 8] = [
        LockMode::AccessShare,
        LockMode::RowShare,
        LockMode::RowExclusive,
        LockMode::ShareUpdateExclusive,
        LockMode::Share,
        LockMode::ShareRowExclusive,
        LockMode::Exclusive,
        LockMode::AccessExclusive,
    ];

    pub fn bit(self) -> u8 {
        1 << (self as u8 - 1)
    }

    /// このモードを要求したときに衝突する保持モードのビット集合。
    pub(crate) fn conflict_mask(self) -> u8 {
        CONFLICT_MASK[self as usize - 1]
    }

    pub fn conflicts(self, other: LockMode) -> bool {
        self.conflict_mask() & other.bit() != 0
    }

    /// `"AccessShareLock"` … `"AccessExclusiveLock"`。DETAIL にも `pg_locks.mode` にもそのまま使う。
    pub fn pg_name(self) -> &'static str {
        match self {
            LockMode::AccessShare => "AccessShareLock",
            LockMode::RowShare => "RowShareLock",
            LockMode::RowExclusive => "RowExclusiveLock",
            LockMode::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
            LockMode::Share => "ShareLock",
            LockMode::ShareRowExclusive => "ShareRowExclusiveLock",
            LockMode::Exclusive => "ExclusiveLock",
            LockMode::AccessExclusive => "AccessExclusiveLock",
        }
    }

    /// `LOCK TABLE` の `IN <words> MODE`（例 `"access share"`）。大文字小文字と空白は正規化する。
    pub fn from_sql_words(words: &str) -> Option<LockMode> {
        let normalized = words
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join(" ");
        Some(match normalized.as_str() {
            "access share" => LockMode::AccessShare,
            "row share" => LockMode::RowShare,
            "row exclusive" => LockMode::RowExclusive,
            "share update exclusive" => LockMode::ShareUpdateExclusive,
            "share" => LockMode::Share,
            "share row exclusive" => LockMode::ShareRowExclusive,
            "exclusive" => LockMode::Exclusive,
            "access exclusive" => LockMode::AccessExclusive,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockScope {
    Transaction,
    Session,
}

/// 待ちの中断条件。待つ関数はすべてこれを受け取る（M5 規約 2）。
#[derive(Clone, Copy, Debug)]
pub struct WaitCtl<'a> {
    /// 1 回の待ちに対する時間。
    pub lock_timeout: Option<Duration>,
    pub deadlock_timeout: Duration,
    /// キャンセル・`statement_timeout`・停止。
    pub interrupts: &'a InterruptFlag,
}

/// heap / index が待つときに渡す（誰が待つか + 中断条件）。
#[derive(Clone, Copy, Debug)]
pub struct WaitCtx<'a> {
    pub backend: BackendId,
    pub ctl: &'a WaitCtl<'a>,
}

/// `pg_locks` の 1 行。
#[derive(Clone, Debug)]
pub struct LockStatusRow {
    /// `"relation"` | `"tuple"` | `"transactionid"` | `"object"`
    pub locktype: &'static str,
    pub database: Option<Oid>,
    pub relation: Option<Oid>,
    pub page: Option<u32>,
    pub tuple: Option<u16>,
    pub transactionid: Option<Xid>,
    pub backend: BackendId,
    pub pid: i32,
    pub mode: &'static str,
    pub granted: bool,
    pub classid: Option<Oid>,
    pub objid: Option<Oid>,
    pub objsubid: Option<i16>,
    /// 待ち手（`granted = false`）の、待ち始めからの経過。
    pub waited: Option<Duration>,
}

/// 待ちの場所（規約 1 の検出フック用。LK-3 が `storage::buffer::track` に実装する）。
#[derive(Clone, Copy, Debug)]
pub enum WaitSite {
    Acquire,
    WaitForXact,
}

#[cfg(debug_assertions)]
static ASSERT_MAY_WAIT: std::sync::OnceLock<fn(WaitSite)> = std::sync::OnceLock::new();

/// `Cluster::open` が 1 回呼ぶ。2 回目以降は無視する。リリースビルドでは何もしない。
pub fn set_wait_assertion(f: fn(WaitSite)) {
    #[cfg(debug_assertions)]
    {
        let _ = ASSERT_MAY_WAIT.set(f);
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = f;
    }
}

/// `acquire` / `wait_for_xact` の入口（`Mutex` を取る前）で、待ってよい状態か検査する。
fn assert_may_wait(site: WaitSite) {
    #[cfg(debug_assertions)]
    if let Some(f) = ASSERT_MAY_WAIT.get() {
        f(site);
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = site;
    }
}

#[derive(Debug)]
pub struct LockManager {
    inner: Mutex<LockTable>,
}

impl LockManager {
    pub fn new() -> Arc<Self> {
        Self::with_knobs(DebugKnobs::default())
    }

    /// 変異テスト用（01 章 §6.7）。`DebugKnobs` のロックの項目を写す。
    pub fn with_knobs(knobs: DebugKnobs) -> Arc<Self> {
        Self::build(LockKnobs {
            ignore_access_exclusive: knobs.lock_ignore_access_exclusive,
            ignore_conflicts: knobs.lock_ignore_conflicts,
            disable_fifo: knobs.lock_disable_fifo,
            disable_breakin: knobs.lock_disable_breakin,
            deadlock_detection_off: knobs.deadlock_detection_off,
        })
    }

    #[cfg(test)]
    fn with_lock_knobs(knobs: LockKnobs) -> Arc<Self> {
        Self::build(knobs)
    }

    fn build(knobs: LockKnobs) -> Arc<Self> {
        Arc::new(LockManager {
            inner: Mutex::new(LockTable::new(knobs)),
        })
    }

    /// 失敗できない関数が使う（poison は無視）。
    fn table(&self) -> MutexGuard<'_, LockTable> {
        lock_ignore_poison(&self.inner)
    }

    /// 接続ごとに 1 回。同じ `id` の二重登録はデバッグビルドで panic（リリースでは無視）。
    pub fn register_backend(&self, id: BackendId, pid: i32) {
        let fresh = self.table().register(id, pid);
        debug_assert!(
            fresh,
            "backend {id:?} registered twice with the lock manager"
        );
    }

    /// `release_all(id, Session)` の後に 1 回。保持・待ちが残っていればデバッグビルドで panic
    /// （`panicking()` のときは検査しない）。
    pub fn unregister_backend(&self, id: BackendId) {
        let clean = self.table().unregister(id);
        debug_assert!(
            clean || std::thread::panicking(),
            "backend {id:?} unregistered while holding or waiting for locks"
        );
    }

    /// 待たずに試す。取れたら `true`。待たなければならない場合・早期デッドロックの場合は
    /// 何も変えずに `false`。未登録の `id` も `false`。
    pub fn try_acquire(
        &self,
        id: BackendId,
        tag: LockTag,
        mode: LockMode,
        scope: LockScope,
    ) -> bool {
        let mut t = self.table();
        if !t.backends.contains_key(&id) {
            return false;
        }
        matches!(t.request(id, tag, mode, scope, true), Request::Granted)
    }

    /// 1 つ解放する（Transaction スコープのカウントを優先）。持っていないモードは、
    /// デバッグビルドで panic、リリースビルドでは何もしない。
    pub fn release(&self, id: BackendId, tag: LockTag, mode: LockMode) {
        let ok = {
            let mut t = self.table();
            !t.backends.contains_key(&id) || t.release_one(id, tag, mode)
        };
        debug_assert!(ok, "release of a lock not held: {id:?} {tag:?} {mode:?}");
    }

    /// `Transaction`: トランザクションスコープのカウントだけ 0 にする。
    /// `Session`: すべて（接続を閉じるとき）。
    pub fn release_all(&self, id: BackendId, scope: LockScope) {
        self.table().release_scope(id, scope);
    }

    /// どちらかのスコープのカウントが 1 以上か（待ち手としての要求は含まない）。
    pub fn holds(&self, id: BackendId, tag: LockTag, mode: LockMode) -> bool {
        self.table()
            .locks
            .get(&tag)
            .and_then(|e| e.holder(id))
            .is_some_and(|h| h.has(mode))
    }

    /// `id` を止めているバックエンド（`pg_blocking_pids`）。hard（保持者）、soft（先行する
    /// 衝突する待ち手）の順で、重複なし。待っていなければ空。
    pub fn blocking_backends(&self, id: BackendId) -> Vec<BackendId> {
        self.table().blockers(id)
    }

    /// `blocking_backends(id)` と `among` が交わるか（`among` 以外を介した推移的な待ちは含めない）。
    pub fn is_blocked_by(&self, id: BackendId, among: &[BackendId]) -> bool {
        self.table().blockers(id).iter().any(|b| among.contains(b))
    }

    /// ロック表の一貫したスナップショット（`pg_locks`）。行の並びは
    /// `(database, relation, page, tuple, transactionid, backend, mode, granted の降順)`。
    pub fn lock_status(&self) -> Vec<LockStatusRow> {
        let t = self.table();
        let mut rows: Vec<(LockMode, LockStatusRow)> = Vec::new();
        for (tag, e) in &t.locks {
            for h in &e.holders {
                for mode in LockMode::ALL {
                    if h.has(mode) {
                        rows.push((mode, status_row(&t, *tag, h.backend, mode, true, None)));
                    }
                }
            }
            for q in &e.queue {
                if let Some(w) = t.waiting_not_granted(*q) {
                    rows.push((
                        w.mode,
                        status_row(&t, *tag, *q, w.mode, false, Some(w.since.elapsed())),
                    ));
                }
            }
        }
        rows.sort_by(|(ma, a), (mb, b)| {
            (
                a.database,
                a.relation,
                a.page,
                a.tuple,
                a.transactionid,
                a.backend,
                *ma,
            )
                .cmp(&(
                    b.database,
                    b.relation,
                    b.page,
                    b.tuple,
                    b.transactionid,
                    b.backend,
                    *mb,
                ))
                .then(b.granted.cmp(&a.granted))
        });
        rows.into_iter().map(|(_, r)| r).collect()
    }

    /// バックエンドが保持している `(タグ, モード)`（診断・テスト用。待ち手は含まない）。
    pub fn held_locks(&self, id: BackendId) -> Vec<(LockTag, LockMode)> {
        let t = self.table();
        let Some(b) = t.backends.get(&id) else {
            return Vec::new();
        };
        let mut out: Vec<(LockTag, LockMode)> = b
            .held
            .iter()
            .filter_map(|tag| {
                t.locks
                    .get(tag)
                    .and_then(|e| e.holder(id))
                    .map(|h| (*tag, h))
            })
            .flat_map(|(tag, h)| {
                LockMode::ALL
                    .into_iter()
                    .filter(|m| h.has(*m))
                    .map(move |m| (tag, m))
            })
            .collect();
        out.sort();
        out
    }

    /// 待っていれば `(タグ, モード, 待ち始めからの経過)`。付与済みで未起床の待ち手は `None`。
    pub fn waiting_on(&self, id: BackendId) -> Option<(LockTag, LockMode, Duration)> {
        self.table()
            .waiting_not_granted(id)
            .map(|w| (w.tag, w.mode, w.since.elapsed()))
    }

    /// `id` 以外のバックエンドが `tag` を待っているか（03 章の autovacuum の中断が使う）。
    pub fn has_waiters(&self, id: BackendId, tag: LockTag) -> bool {
        self.table()
            .locks
            .get(&tag)
            .is_some_and(|e| e.queue.iter().any(|q| *q != id))
    }
}

fn status_row(
    t: &LockTable,
    tag: LockTag,
    backend: BackendId,
    mode: LockMode,
    granted: bool,
    waited: Option<Duration>,
) -> LockStatusRow {
    let mut row = LockStatusRow {
        locktype: "relation",
        database: None,
        relation: None,
        page: None,
        tuple: None,
        transactionid: None,
        backend,
        pid: t.pid_of(backend),
        mode: mode.pg_name(),
        granted,
        classid: None,
        objid: None,
        objsubid: None,
        waited,
    };
    match tag {
        LockTag::Relation { db, rel } => {
            row.database = Some(db);
            row.relation = Some(rel);
        }
        LockTag::Tuple {
            db,
            rel,
            block,
            offset,
        } => {
            row.locktype = "tuple";
            row.database = Some(db);
            row.relation = Some(rel);
            row.page = Some(block);
            row.tuple = Some(offset);
        }
        LockTag::TransactionId(xid) => {
            row.locktype = "transactionid";
            row.transactionid = Some(xid);
        }
        LockTag::Database(oid) => {
            row.locktype = "object";
            row.database = Some(0);
            row.classid = Some(1262);
            row.objid = Some(oid);
            row.objsubid = Some(0);
        }
        LockTag::Object { db, class, obj } => {
            row.locktype = "object";
            row.database = Some(db);
            row.classid = Some(class);
            row.objid = Some(low32(obj));
            row.objsubid = Some(0);
        }
    }
    row
}
