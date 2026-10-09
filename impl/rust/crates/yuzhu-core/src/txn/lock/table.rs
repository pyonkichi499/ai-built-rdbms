//! ロック表（LK-1）。`LockManager` の `Mutex` の中身。
//!
//! PostgreSQL の `LockAcquireExtended` と `ProcSleep`・`ProcLockWakeup` を、ロック表 1 本の
//! 上に移植したもの（01 章 §5.1・§5.3・§6.1）。ここの関数はすべて `Mutex` を持ったまま呼ぶ。
//! I/O・他の `Mutex` の取得・待ちはしない。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar};
use std::time::Instant;

use super::{LockMode, LockScope, LockTag};
use crate::backend::BackendId;

/// 変異テスト用のスイッチ（01 章 §6.7）。`DebugKnobs` に項目が入るまでは、テストだけが立てる。
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct LockKnobs {
    pub(super) ignore_access_exclusive: bool,
    pub(super) ignore_conflicts: bool,
    pub(super) disable_fifo: bool,
    pub(super) disable_breakin: bool,
    pub(super) deadlock_detection_off: bool,
}

impl LockKnobs {
    /// `AccessExclusive` の要求・保持を衝突しないものとして扱うスイッチだけを反映したマスク。
    pub(super) fn mask_ae(self, m: LockMode) -> u8 {
        let base = m.conflict_mask();
        if !self.ignore_access_exclusive {
            base
        } else if m == LockMode::AccessExclusive {
            0
        } else {
            base & !LockMode::AccessExclusive.bit()
        }
    }

    /// 保持者との衝突の判定に使うマスク（`ignore_conflicts` も反映）。
    pub(super) fn conflict_mask(self, m: LockMode) -> u8 {
        if self.ignore_conflicts {
            0
        } else {
            self.mask_ae(m)
        }
    }
}

/// 待ちグラフの 1 辺の出発点（循環上の待ち手と、その待っているタグ・モード）。
#[derive(Clone, Copy, Debug)]
pub(super) struct Hop {
    pub(super) waiter: BackendId,
    pub(super) tag: LockTag,
    pub(super) mode: LockMode,
}

#[derive(Debug)]
pub(super) enum Request {
    Granted,
    Queued,
    NotAvailable,
    Deadlock(Vec<Hop>),
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HoldCount {
    pub(super) txn: u32,
    pub(super) session: u32,
}

impl HoldCount {
    fn total(self) -> u32 {
        self.txn + self.session
    }
}

#[derive(Debug)]
pub(super) struct Holder {
    pub(super) backend: BackendId,
    pub(super) counts: [HoldCount; 8],
}

impl Holder {
    /// 保持しているモードのビット集合。
    pub(super) fn mask(&self) -> u8 {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.total() > 0)
            .fold(0u8, |acc, (i, _)| acc | (1 << i))
    }

    pub(super) fn has(&self, mode: LockMode) -> bool {
        self.counts[mode as usize - 1].total() > 0
    }
}

#[derive(Debug, Default)]
pub(super) struct LockEntry {
    /// 登録順（`blocking_backends` の順序の元。決定的）。
    pub(super) holders: Vec<Holder>,
    /// モードごとの「保持しているバックエンド数」（`holders` から導ける冗長情報）。
    pub(super) granted: [u32; 8],
    /// 待ち手。先頭が最古（割り込みで途中に入ることがある）。
    pub(super) queue: VecDeque<BackendId>,
}

impl LockEntry {
    fn is_empty(&self) -> bool {
        self.holders.is_empty() && self.queue.is_empty()
    }

    pub(super) fn holder(&self, id: BackendId) -> Option<&Holder> {
        self.holders.iter().find(|h| h.backend == id)
    }

    pub(super) fn hold_mask(&self, id: BackendId) -> u8 {
        self.holder(id).map_or(0, Holder::mask)
    }

    /// 他のバックエンドが `mask(mode)` のモードを保持しているか。同じバックエンドの保持同士は衝突しない。
    pub(super) fn check_conflicts(&self, id: BackendId, mode: LockMode, knobs: LockKnobs) -> bool {
        let m = knobs.conflict_mask(mode);
        let mine = self.hold_mask(id);
        (0..8).any(|i| {
            let bit = 1u8 << i;
            m & bit != 0 && self.granted[i] > u32::from(mine & bit != 0)
        })
    }

    fn wait_mask(&self, backends: &HashMap<BackendId, BackendLockState>) -> u8 {
        self.queue
            .iter()
            .filter_map(|q| backends.get(q).and_then(|b| b.waiting.as_ref()))
            .fold(0u8, |acc, w| acc | w.mode.bit())
    }

    /// 保持者を作る（無ければ）か、そのモードのスコープのカウントを 1 増やす。
    fn grant(&mut self, id: BackendId, mode: LockMode, scope: LockScope) {
        let idx = mode as usize - 1;
        let pos = if let Some(p) = self.holders.iter().position(|h| h.backend == id) {
            p
        } else {
            self.holders.push(Holder {
                backend: id,
                counts: [HoldCount::default(); 8],
            });
            self.holders.len() - 1
        };
        let c = &mut self.holders[pos].counts[idx];
        let was_held = c.total() > 0;
        match scope {
            LockScope::Transaction => c.txn += 1,
            LockScope::Session => c.session += 1,
        }
        if !was_held {
            self.granted[idx] += 1;
        }
    }

    /// Transaction スコープのカウントを優先して 1 減らす。持っていなければ `false`。
    fn release_one(&mut self, id: BackendId, mode: LockMode) -> bool {
        let idx = mode as usize - 1;
        let Some(pos) = self.holders.iter().position(|h| h.backend == id) else {
            return false;
        };
        let c = &mut self.holders[pos].counts[idx];
        if c.txn > 0 {
            c.txn -= 1;
        } else if c.session > 0 {
            c.session -= 1;
        } else {
            return false;
        }
        if c.total() == 0 {
            self.granted[idx] -= 1;
            if self.holders[pos].mask() == 0 {
                self.holders.remove(pos);
            }
        }
        true
    }

    /// `scope` のカウントを 0 にする（Session は両方）。
    fn release_scope(&mut self, id: BackendId, scope: LockScope) {
        let Some(pos) = self.holders.iter().position(|h| h.backend == id) else {
            return;
        };
        for idx in 0..8 {
            let c = &mut self.holders[pos].counts[idx];
            let was_held = c.total() > 0;
            c.txn = 0;
            if scope == LockScope::Session {
                c.session = 0;
            }
            if was_held && c.total() == 0 {
                self.granted[idx] -= 1;
            }
        }
        if self.holders[pos].mask() == 0 {
            self.holders.remove(pos);
        }
    }
}

#[derive(Debug)]
pub(super) struct Waiting {
    pub(super) tag: LockTag,
    pub(super) mode: LockMode,
    pub(super) scope: LockScope,
    pub(super) since: Instant,
    /// 解放者が付与を済ませた（待ち手はまだ目覚めていない）。
    pub(super) granted: bool,
}

#[derive(Debug)]
pub(super) struct BackendLockState {
    pub(super) pid: i32,
    pub(super) cv: Arc<Condvar>,
    pub(super) waiting: Option<Waiting>,
    /// 保持しているタグ（`release_all` の走査用）。
    pub(super) held: HashSet<LockTag>,
}

#[derive(Debug, Default)]
pub(super) struct LockTable {
    pub(super) locks: HashMap<LockTag, LockEntry>,
    pub(super) backends: HashMap<BackendId, BackendLockState>,
    pub(super) knobs: LockKnobs,
}

impl LockTable {
    pub(super) fn new(knobs: LockKnobs) -> Self {
        LockTable {
            locks: HashMap::new(),
            backends: HashMap::new(),
            knobs,
        }
    }

    /// 登録する。既に登録されていれば `false`（何も変えない）。
    pub(super) fn register(&mut self, id: BackendId, pid: i32) -> bool {
        if self.backends.contains_key(&id) {
            return false;
        }
        self.backends.insert(
            id,
            BackendLockState {
                pid,
                cv: Arc::new(Condvar::new()),
                waiting: None,
                held: HashSet::new(),
            },
        );
        true
    }

    /// 登録を外す。保持も待ちも残っていなければ `true`。
    pub(super) fn unregister(&mut self, id: BackendId) -> bool {
        match self.backends.remove(&id) {
            Some(b) => b.held.is_empty() && b.waiting.is_none(),
            None => true,
        }
    }

    pub(super) fn pid_of(&self, id: BackendId) -> i32 {
        self.backends.get(&id).map_or(0, |b| b.pid)
    }

    /// 待っている（付与されていない）なら、その情報。
    pub(super) fn waiting_not_granted(&self, id: BackendId) -> Option<&Waiting> {
        self.backends
            .get(&id)
            .and_then(|b| b.waiting.as_ref())
            .filter(|w| !w.granted)
    }

    fn put_back(&mut self, tag: LockTag, e: LockEntry) {
        if !e.is_empty() {
            self.locks.insert(tag, e);
        }
    }

    fn grant_in(
        &mut self,
        e: &mut LockEntry,
        tag: LockTag,
        id: BackendId,
        mode: LockMode,
        scope: LockScope,
    ) {
        e.grant(id, mode, scope);
        if let Some(b) = self.backends.get_mut(&id) {
            b.held.insert(tag);
        }
    }

    /// 要求の判定（01 章 §5.1）。`id` は登録済みであること。
    pub(super) fn request(
        &mut self,
        id: BackendId,
        tag: LockTag,
        mode: LockMode,
        scope: LockScope,
        dont_wait: bool,
    ) -> Request {
        let mut e = self.locks.remove(&tag).unwrap_or_default();
        let r = self.request_in(&mut e, tag, id, mode, scope, dont_wait);
        self.put_back(tag, e);
        r
    }

    fn request_in(
        &mut self,
        e: &mut LockEntry,
        tag: LockTag,
        id: BackendId,
        mode: LockMode,
        scope: LockScope,
        dont_wait: bool,
    ) -> Request {
        // (1) 同じ (タグ, モード) を既に持っている: 待ち行列を見ずにカウントを増やす
        if e.holder(id).is_some_and(|h| h.has(mode)) {
            self.grant_in(e, tag, id, mode, scope);
            return Request::Granted;
        }

        // (2) 待ち手のモードと衝突するなら、保持者と衝突しなくても待ち行列に入る（FIFO）
        let queued_conflict =
            !self.knobs.disable_fifo && self.knobs.mask_ae(mode) & e.wait_mask(&self.backends) != 0;
        let conflict = queued_conflict || e.check_conflicts(id, mode, self.knobs);
        if !conflict {
            self.grant_in(e, tag, id, mode, scope);
            return Request::Granted;
        }

        // (3) ProcSleep: 自分がこのロックを何か持っているときだけ、割り込む位置を探す
        let my_held = e.hold_mask(id);
        let mut insert_at = None;
        if my_held != 0 && !e.queue.is_empty() && !self.knobs.disable_breakin {
            let cm = self.knobs.conflict_mask(mode);
            let queue: Vec<BackendId> = e.queue.iter().copied().collect();
            let mut ahead = 0u8;
            for (i, q) in queue.into_iter().enumerate() {
                let Some(qmode) = self
                    .backends
                    .get(&q)
                    .and_then(|b| b.waiting.as_ref())
                    .map(|w| w.mode)
                else {
                    continue;
                };
                if self.knobs.conflict_mask(qmode) & my_held != 0 {
                    // q は私を待っている
                    if cm & e.hold_mask(q) != 0 {
                        // 私も q を待つ: 早期デッドロック
                        return Request::Deadlock(vec![
                            Hop {
                                waiter: id,
                                tag,
                                mode,
                            },
                            Hop {
                                waiter: q,
                                tag,
                                mode: qmode,
                            },
                        ]);
                    }
                    if cm & ahead == 0 && !e.check_conflicts(id, mode, self.knobs) {
                        self.grant_in(e, tag, id, mode, scope);
                        return Request::Granted;
                    }
                    insert_at = Some(i);
                    break;
                }
                ahead |= qmode.bit();
            }
        }
        if dont_wait {
            return Request::NotAvailable;
        }
        match insert_at {
            Some(i) => e.queue.insert(i, id),
            None => e.queue.push_back(id),
        }
        if let Some(b) = self.backends.get_mut(&id) {
            debug_assert!(b.waiting.is_none(), "backend already waiting");
            b.waiting = Some(Waiting {
                tag,
                mode,
                scope,
                since: Instant::now(),
                granted: false,
            });
        }
        Request::Queued
    }

    /// 付与済みなら `waiting` を下ろして `true`。
    pub(super) fn take_granted(&mut self, id: BackendId) -> bool {
        let Some(b) = self.backends.get_mut(&id) else {
            return false;
        };
        if b.waiting.as_ref().is_some_and(|w| w.granted) {
            b.waiting = None;
            true
        } else {
            false
        }
    }

    /// 待ち行列から自分を外し、そのタグの待ち手を起こす。付与済みなら何もしない。
    pub(super) fn cancel_wait(&mut self, id: BackendId) {
        let Some(tag) = self.waiting_not_granted(id).map(|w| w.tag) else {
            return;
        };
        if let Some(mut e) = self.locks.remove(&tag) {
            e.queue.retain(|x| *x != id);
            self.wake_in(&mut e, tag);
            self.put_back(tag, e);
        }
        if let Some(b) = self.backends.get_mut(&id) {
            b.waiting = None;
        }
    }

    /// 持っていないモードの解放は `false`（何も変えない）。
    pub(super) fn release_one(&mut self, id: BackendId, tag: LockTag, mode: LockMode) -> bool {
        let Some(mut e) = self.locks.remove(&tag) else {
            return false;
        };
        let ok = e.release_one(id, mode);
        if ok {
            if e.holder(id).is_none()
                && let Some(b) = self.backends.get_mut(&id)
            {
                b.held.remove(&tag);
            }
            self.wake_in(&mut e, tag);
        }
        self.put_back(tag, e);
        ok
    }

    /// `release_all` の本体。影響を受けた各タグで待ち手を起こす。
    pub(super) fn release_scope(&mut self, id: BackendId, scope: LockScope) {
        let tags: Vec<LockTag> = match self.backends.get(&id) {
            Some(b) => b.held.iter().copied().collect(),
            None => return,
        };
        for tag in tags {
            let Some(mut e) = self.locks.remove(&tag) else {
                if let Some(b) = self.backends.get_mut(&id) {
                    b.held.remove(&tag);
                }
                continue;
            };
            e.release_scope(id, scope);
            if e.holder(id).is_none()
                && let Some(b) = self.backends.get_mut(&id)
            {
                b.held.remove(&tag);
            }
            self.wake_in(&mut e, tag);
            self.put_back(tag, e);
        }
    }

    /// PG の `ProcLockWakeup`: 待ち行列を先頭から見て、先行する未付与の待ち手のモードと
    /// 衝突せず、保持者とも衝突しない待ち手を付与する。
    #[cfg(test)]
    pub(super) fn wake_waiters(&mut self, tag: LockTag) {
        if let Some(mut e) = self.locks.remove(&tag) {
            self.wake_in(&mut e, tag);
            self.put_back(tag, e);
        }
    }

    fn wake_in(&mut self, e: &mut LockEntry, tag: LockTag) {
        let queue: Vec<BackendId> = e.queue.iter().copied().collect();
        let mut ahead = 0u8;
        for q in queue {
            let Some((qmode, qscope)) = self
                .backends
                .get(&q)
                .and_then(|b| b.waiting.as_ref())
                .map(|w| (w.mode, w.scope))
            else {
                e.queue.retain(|x| *x != q);
                continue;
            };
            if self.knobs.conflict_mask(qmode) & ahead == 0
                && !e.check_conflicts(q, qmode, self.knobs)
            {
                e.grant(q, qmode, qscope);
                e.queue.retain(|x| *x != q);
                if let Some(b) = self.backends.get_mut(&q) {
                    b.held.insert(tag);
                    if let Some(w) = b.waiting.as_mut() {
                        w.granted = true;
                    }
                    b.cv.notify_one();
                }
            } else {
                ahead |= qmode.bit();
            }
        }
    }

    /// `id` を止めているバックエンド（`pg_blocking_pids` と同じ意味）。
    /// (1) 待っているモードと衝突するモードを保持している他のバックエンド（hard。保持の登録順）、
    /// (2) 待ち行列で先にいて、待っているモードが衝突するバックエンド（soft。待ち行列順）。
    pub(super) fn blockers(&self, id: BackendId) -> Vec<BackendId> {
        let Some(w) = self.waiting_not_granted(id) else {
            return Vec::new();
        };
        let Some(e) = self.locks.get(&w.tag) else {
            return Vec::new();
        };
        let m = self.knobs.conflict_mask(w.mode);
        let mut out: Vec<BackendId> = e
            .holders
            .iter()
            .filter(|h| h.backend != id && h.mask() & m != 0)
            .map(|h| h.backend)
            .collect();
        for &q in &e.queue {
            if q == id {
                break;
            }
            let conflicts = self
                .backends
                .get(&q)
                .and_then(|b| b.waiting.as_ref())
                .is_some_and(|qw| m & qw.mode.bit() != 0);
            if conflicts && !out.contains(&q) {
                out.push(q);
            }
        }
        out
    }

    /// 不変条件 I1〜I5（01 章 §6.1）の検査。テスト用。
    #[cfg(test)]
    pub(super) fn check_invariants(&self) -> Result<(), String> {
        for (tag, e) in &self.locks {
            if e.holders.is_empty() && e.queue.is_empty() {
                return Err(format!("I5: empty entry {tag:?}"));
            }
            for (i, a) in e.holders.iter().enumerate() {
                for b in &e.holders[i + 1..] {
                    for ma in LockMode::ALL {
                        for mb in LockMode::ALL {
                            if a.has(ma) && b.has(mb) && ma.conflicts(mb) {
                                return Err(format!(
                                    "I1: {:?} {ma:?} vs {:?} {mb:?} on {tag:?}",
                                    a.backend, b.backend
                                ));
                            }
                        }
                    }
                }
            }
            for m in LockMode::ALL {
                let n = e.holders.iter().filter(|h| h.has(m)).count();
                if n != e.granted[m as usize - 1] as usize {
                    return Err(format!(
                        "I2: granted[{m:?}] = {} but {n} on {tag:?}",
                        e.granted[m as usize - 1]
                    ));
                }
            }
            for q in &e.queue {
                let ok = self.waiting_not_granted(*q).is_some_and(|w| w.tag == *tag);
                if !ok {
                    return Err(format!("I3: queued {q:?} is not waiting on {tag:?}"));
                }
            }
        }
        for (id, b) in &self.backends {
            if let Some(w) = b.waiting.as_ref()
                && !w.granted
                && !self.locks.get(&w.tag).is_some_and(|e| e.queue.contains(id))
            {
                return Err(format!("I3: waiting {id:?} missing from queue"));
            }
            for tag in &b.held {
                if self.locks.get(tag).is_none_or(|e| e.holder(*id).is_none()) {
                    return Err(format!("I5: {id:?} held lists {tag:?} but is not a holder"));
                }
            }
        }
        for (tag, e) in &self.locks {
            for h in &e.holders {
                if !self
                    .backends
                    .get(&h.backend)
                    .is_some_and(|b| b.held.contains(tag))
                {
                    return Err(format!("I5: holder {:?} of {tag:?} not in held", h.backend));
                }
            }
        }
        Ok(())
    }
}
