//! RI のイベントキューと、イベントの積み方（`m5/07` §5.2）。
//!
//! 積むかどうかの判定は `events_after_*` の純粋関数にある。`RiQueue` はイベント・開いた関係・
//! 検査済みキャッシュ・メモリの課金を持つ。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use super::keys::{NullClass, extract_key, images_equal, keys_equal, null_class};
use super::rel::{FkPlan, RiEnv, RiRel};
use crate::catalog::fk::{FkMatch, ForeignKeyDef};
use crate::error::{Error, Result, sqlstate};
use crate::executor::mem::{MemBudget, ROW_OVERHEAD, estimate_datum_bytes};
use crate::storage::RiRelInfo;
use crate::txn::Snapshot;
use crate::txn::lock::LockMode;
use crate::types::hash::HashKey;
use crate::types::{Datum, Oid, Tid};

/// 1 文で処理できるイベント数の安全弁。
pub const RI_MAX_EVENTS: u64 = 100_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiOp {
    Delete,
    Update,
}

#[derive(Clone, Debug)]
pub enum RiEvent {
    /// 子の行（INSERT、またはキーが変わった UPDATE）に親があるか。
    CheckParent {
        fk: Arc<ForeignKeyDef>,
        child_tid: Tid,
        key: Vec<Datum>,
    },
    /// 親のキーが無くなった。`new_key` は UPDATE のときだけ。
    ParentKeyGone {
        fk: Arc<ForeignKeyDef>,
        op: RiOp,
        old_key: Vec<Datum>,
        new_key: Option<Vec<Datum>>,
    },
}

impl RiEvent {
    /// キューに置くときの概算のバイト数。
    pub fn estimate_bytes(&self) -> usize {
        let keys = |k: &[Datum]| k.iter().map(estimate_datum_bytes).sum::<usize>() + ROW_OVERHEAD;
        let base = std::mem::size_of::<RiEvent>();
        base + match self {
            RiEvent::CheckParent { key, .. } => keys(key),
            RiEvent::ParentKeyGone {
                old_key, new_key, ..
            } => keys(old_key) + new_key.as_deref().map_or(0, keys),
        }
    }
}

/// 子の挿入で積むもの（`after_insert` の outgoing 側）。`None` は積まない。
fn child_check(fk: &Arc<ForeignKeyDef>, tid: Tid, key: Vec<Datum>) -> Option<RiEvent> {
    match (null_class(&key), fk.match_type) {
        (NullClass::AllNull, _) | (NullClass::Mixed, FkMatch::Simple) => None,
        _ => Some(RiEvent::CheckParent {
            fk: Arc::clone(fk),
            child_tid: tid,
            key,
        }),
    }
}

/// 行の挿入で積むイベント。被参照側は何も積まない。
pub fn events_after_insert(ri: &RiRelInfo, tid: Tid, new_row: &[Datum]) -> Vec<RiEvent> {
    ri.outgoing
        .iter()
        .filter_map(|fk| child_check(fk, tid, extract_key(new_row, &fk.child_cols)))
        .collect()
}

/// 行の削除で積むイベント。NULL を含む親のキーは誰にも参照されない。
pub fn events_after_delete(ri: &RiRelInfo, old_row: &[Datum]) -> Vec<RiEvent> {
    ri.incoming
        .iter()
        .filter_map(|fk| {
            let old_key = extract_key(old_row, &fk.parent_cols);
            (null_class(&old_key) == NullClass::NoneNull).then(|| RiEvent::ParentKeyGone {
                fk: Arc::clone(fk),
                op: RiOp::Delete,
                old_key,
                new_key: None,
            })
        })
        .collect()
}

/// 行の更新で積むイベント。被参照側（OID 昇順）、続けて参照側（OID 昇順）。
///
/// `old_made_by_self` は、旧版を作ったのが自分のトランザクションか（読めなければ `None`）。
/// 子のキーが値で変わっておらず NULL も無いときだけ呼ばれ、`Some(false)` のときに限り積まない。
pub fn events_after_update(
    ri: &RiRelInfo,
    new_tid: Tid,
    old_row: &[Datum],
    new_row: &[Datum],
    old_made_by_self: &mut dyn FnMut() -> Option<bool>,
) -> Vec<RiEvent> {
    let mut out = Vec::new();
    for fk in ri.incoming.iter() {
        let old_key = extract_key(old_row, &fk.parent_cols);
        if null_class(&old_key) != NullClass::NoneNull {
            continue;
        }
        let new_key = extract_key(new_row, &fk.parent_cols);
        if images_equal(&old_key, &new_key) {
            continue;
        }
        out.push(RiEvent::ParentKeyGone {
            fk: Arc::clone(fk),
            op: RiOp::Update,
            old_key,
            new_key: Some(new_key),
        });
    }
    for fk in ri.outgoing.iter() {
        let new_key = extract_key(new_row, &fk.child_cols);
        if null_class(&new_key) == NullClass::NoneNull {
            let old_key = extract_key(old_row, &fk.child_cols);
            if keys_equal(&old_key, &new_key) && old_made_by_self() == Some(false) {
                continue;
            }
        }
        out.extend(child_check(fk, new_tid, new_key));
    }
    out
}

/// 1 文の間のイベントキューと、RI が開いた関係の覚え。
#[derive(Debug)]
pub struct RiQueue {
    events: VecDeque<RiEvent>,
    env: Arc<dyn RiEnv>,
    rels: HashMap<Oid, Arc<RiRel>>,
    plans: HashMap<Oid, Arc<FkPlan>>,
    locked: HashSet<(Oid, LockMode)>,
    verified: HashSet<(Oid, HashKey)>,
    draining: bool,
    crosscheck: Option<Snapshot>,
    mem_charged: usize,
    processed: u64,
}

impl RiQueue {
    pub fn new(env: Arc<dyn RiEnv>) -> RiQueue {
        RiQueue {
            events: VecDeque::new(),
            env,
            rels: HashMap::new(),
            plans: HashMap::new(),
            locked: HashSet::new(),
            verified: HashSet::new(),
            draining: false,
            crosscheck: None,
            mem_charged: 0,
            processed: 0,
        }
    }

    pub fn env(&self) -> &Arc<dyn RiEnv> {
        &self.env
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn pending(&self) -> usize {
        self.events.len()
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }

    pub fn set_draining(&mut self, on: bool) {
        self.draining = on;
    }

    pub fn processed(&self) -> u64 {
        self.processed
    }

    /// action の書き込みが `delete` / `update` に渡す crosscheck。RR の action の間だけ `Some`。
    pub fn crosscheck(&self) -> Option<&Snapshot> {
        self.crosscheck.as_ref()
    }

    pub fn set_crosscheck(&mut self, snap: Option<Snapshot>) {
        self.crosscheck = snap;
    }

    /// 積む。メモリを課金し（超えたら 53200）、イベント数の安全弁（54001）を見る。
    pub fn push(&mut self, ev: RiEvent, mem: &MemBudget) -> Result<()> {
        if self.processed.saturating_add(self.events.len() as u64) >= RI_MAX_EVENTS {
            return Err(too_many_events());
        }
        let bytes = ev.estimate_bytes();
        mem.charge(bytes)?;
        self.mem_charged += bytes;
        self.events.push_back(ev);
        Ok(())
    }

    pub fn push_all(&mut self, evs: Vec<RiEvent>, mem: &MemBudget) -> Result<()> {
        evs.into_iter().try_for_each(|e| self.push(e, mem))
    }

    /// 先頭を取り出して、そのメモリを返す。処理した数を 1 増やし、安全弁を超えたら 54001。
    pub fn pop(&mut self, mem: &MemBudget) -> Result<Option<RiEvent>> {
        let Some(ev) = self.events.pop_front() else {
            return Ok(None);
        };
        let bytes = ev.estimate_bytes().min(self.mem_charged);
        self.mem_charged -= bytes;
        mem.release(bytes);
        self.processed += 1;
        if self.processed > RI_MAX_EVENTS {
            return Err(too_many_events());
        }
        Ok(Some(ev))
    }

    /// 残りのイベントを捨てて、課金を全部返す。開いた関係の覚えは残す（`reset` で消す）。
    pub fn clear(&mut self, mem: &MemBudget) {
        self.events.clear();
        mem.release(self.mem_charged);
        self.mem_charged = 0;
    }

    /// 文が終わったときの片付け。イベント・関係・計画・ロックの覚え・検査済みを全部消す。
    pub fn reset(&mut self, mem: &MemBudget) {
        self.clear(mem);
        self.rels.clear();
        self.plans.clear();
        self.locked.clear();
        self.verified.clear();
        self.draining = false;
        self.crosscheck = None;
        self.processed = 0;
    }

    pub fn rel(&self, oid: Oid) -> Option<&Arc<RiRel>> {
        self.rels.get(&oid)
    }

    pub fn insert_rel(&mut self, oid: Oid, rel: Arc<RiRel>) {
        self.rels.insert(oid, rel);
    }

    pub fn plan_cached(&self, fk_oid: Oid) -> Option<&Arc<FkPlan>> {
        self.plans.get(&fk_oid)
    }

    pub fn insert_plan(&mut self, plan: Arc<FkPlan>) {
        self.plans.insert(plan.fk.oid, plan);
    }

    /// 初めてなら `true`（呼び出し側がロックを取る）。
    pub fn mark_locked(&mut self, oid: Oid, mode: LockMode) -> bool {
        self.locked.insert((oid, mode))
    }

    pub fn is_verified(&self, fk_oid: Oid, key: &[Datum]) -> bool {
        self.verified.contains(&(fk_oid, HashKey(key.to_vec())))
    }

    pub fn mark_verified(&mut self, fk_oid: Oid, key: Vec<Datum>) {
        self.verified.insert((fk_oid, HashKey(key)));
    }

    /// action が行を書いたら空にする。
    pub fn clear_verified(&mut self) {
        self.verified.clear();
    }
}

fn too_many_events() -> Error {
    Error::new(
        sqlstate::STATEMENT_TOO_COMPLEX,
        "too many foreign key events in one statement",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fk::{FkAction, FkEqOps};
    use crate::txn::{CommandId, RegisteredSnapshot, Xid};
    use crate::types::Tid;

    #[derive(Debug)]
    struct NoEnv;
    impl RiEnv for NoEnv {
        fn open_relation(&self, _oid: Oid) -> Result<RiRel> {
            Err(Error::internal("unused"))
        }
        fn latest_snapshot(&self, _own: Option<Xid>, _curcid: CommandId) -> RegisteredSnapshot {
            panic!("not used in queue tests")
        }
    }

    fn fk(
        oid: u32,
        child: u32,
        parent: u32,
        ccols: &[i16],
        pcols: &[i16],
        m: FkMatch,
    ) -> Arc<ForeignKeyDef> {
        Arc::new(ForeignKeyDef {
            oid: Oid::from(oid),
            name: format!("fk{oid}"),
            child: Oid::from(child),
            parent: Oid::from(parent),
            parent_index: Oid::from(0u32),
            child_cols: ccols.to_vec(),
            parent_cols: pcols.to_vec(),
            match_type: m,
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            set_cols: vec![],
            eq_ops: vec![
                FkEqOps {
                    pf: Oid::from(0u32),
                    pp: Oid::from(0u32),
                    ff: Oid::from(0u32)
                };
                ccols.len()
            ],
        })
    }

    fn info(outgoing: Vec<Arc<ForeignKeyDef>>, incoming: Vec<Arc<ForeignKeyDef>>) -> RiRelInfo {
        RiRelInfo {
            outgoing: Arc::from(outgoing),
            incoming: Arc::from(incoming),
        }
    }

    fn tid() -> Tid {
        Tid {
            block: 0,
            offset: 1,
        }
    }

    fn i(v: i32) -> Datum {
        Datum::Int4(v)
    }

    #[test]
    fn insert_null_handling() {
        let simple = fk(1, 10, 20, &[1, 2], &[1, 2], FkMatch::Simple);
        let full = fk(2, 10, 20, &[1, 2], &[1, 2], FkMatch::Full);
        let ri = info(vec![simple, full], vec![]);
        // 全部 NULL: 積まない。
        assert!(events_after_insert(&ri, tid(), &[Datum::Null, Datum::Null]).is_empty());
        // 混在: FULL だけ積む。
        let evs = events_after_insert(&ri, tid(), &[i(1), Datum::Null]);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], RiEvent::CheckParent { fk, .. } if fk.oid == Oid::from(2u32)));
        // NULL なし: 両方、OID 昇順。
        assert_eq!(events_after_insert(&ri, tid(), &[i(1), i(2)]).len(), 2);
    }

    #[test]
    fn delete_skips_null_parent_key() {
        let f = fk(1, 10, 20, &[1], &[2], FkMatch::Simple);
        let ri = info(vec![], vec![f]);
        assert!(events_after_delete(&ri, &[i(1), Datum::Null]).is_empty());
        let evs = events_after_delete(&ri, &[i(1), i(7)]);
        match &evs[..] {
            [
                RiEvent::ParentKeyGone {
                    op,
                    old_key,
                    new_key,
                    ..
                },
            ] => {
                assert_eq!(*op, RiOp::Delete);
                assert_eq!(old_key, &vec![i(7)]);
                assert!(new_key.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn update_parent_side_uses_image_equality() {
        let f = fk(1, 10, 20, &[1], &[1], FkMatch::Simple);
        let ri = info(vec![], vec![f]);
        let mut never = || -> Option<bool> { panic!("not needed") };
        assert!(events_after_update(&ri, tid(), &[i(1)], &[i(1)], &mut never).is_empty());
        let evs = events_after_update(&ri, tid(), &[i(1)], &[i(2)], &mut never);
        assert!(
            matches!(&evs[..], [RiEvent::ParentKeyGone { op: RiOp::Update, new_key: Some(k), .. }] if k == &vec![i(2)])
        );
        // 古いキーが NULL なら積まない。
        assert!(events_after_update(&ri, tid(), &[Datum::Null], &[i(2)], &mut never).is_empty());
    }

    #[test]
    fn update_child_side_skips_unchanged_key_only_for_foreign_old_version() {
        let f = fk(1, 10, 20, &[1], &[1], FkMatch::Simple);
        let ri = info(vec![f], vec![]);
        // キーが変わった: 旧版を見ずに積む。
        let mut never = || -> Option<bool> { panic!("not needed") };
        assert_eq!(
            events_after_update(&ri, tid(), &[i(1)], &[i(2)], &mut never).len(),
            1
        );
        // 変わらず、旧版は他人のもの: 積まない。
        let mut other = || Some(false);
        assert!(events_after_update(&ri, tid(), &[i(1)], &[i(1)], &mut other).is_empty());
        // 変わらず、旧版は自分のもの: 積む。
        let mut mine = || Some(true);
        assert_eq!(
            events_after_update(&ri, tid(), &[i(1)], &[i(1)], &mut mine).len(),
            1
        );
        // 読めない: 省かない側に倒す。
        let mut unknown = || None;
        assert_eq!(
            events_after_update(&ri, tid(), &[i(1)], &[i(1)], &mut unknown).len(),
            1
        );
        // 新しいキーが NULL: 旧版を読まない。
        assert!(events_after_update(&ri, tid(), &[i(1)], &[Datum::Null], &mut never).is_empty());
    }

    #[test]
    fn update_orders_incoming_before_outgoing() {
        let inc = fk(1, 10, 20, &[1], &[1], FkMatch::Simple);
        let out = fk(2, 20, 30, &[1], &[1], FkMatch::Simple);
        let ri = info(vec![out], vec![inc]);
        let mut none = || None;
        let evs = events_after_update(&ri, tid(), &[i(1)], &[i(2)], &mut none);
        assert!(matches!(&evs[0], RiEvent::ParentKeyGone { .. }));
        assert!(matches!(&evs[1], RiEvent::CheckParent { .. }));
    }

    #[test]
    fn queue_charges_and_releases_memory() {
        let mem = MemBudget::new(1 << 20);
        let mut q = RiQueue::new(Arc::new(NoEnv));
        let f = fk(1, 10, 20, &[1], &[1], FkMatch::Simple);
        let ev = RiEvent::CheckParent {
            fk: f,
            child_tid: tid(),
            key: vec![i(1)],
        };
        q.push(ev.clone(), &mem).unwrap();
        q.push(ev, &mem).unwrap();
        assert_eq!(q.pending(), 2);
        assert!(mem.used() > 0);
        assert!(q.pop(&mem).unwrap().is_some());
        let one = mem.used();
        assert!(one > 0);
        q.clear(&mem);
        assert_eq!(mem.used(), 0);
        assert!(q.is_empty());
        assert!(q.pop(&mem).unwrap().is_none());
        assert_eq!(q.processed(), 1);
    }

    #[test]
    fn queue_memory_limit_is_53200() {
        let mem = MemBudget::new(10);
        let mut q = RiQueue::new(Arc::new(NoEnv));
        let f = fk(1, 10, 20, &[1], &[1], FkMatch::Simple);
        let ev = RiEvent::CheckParent {
            fk: f,
            child_tid: tid(),
            key: vec![i(1)],
        };
        let e = q.push(ev, &mem).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OUT_OF_MEMORY);
        assert!(q.is_empty());
    }

    #[test]
    fn verified_cache_and_reset() {
        let mem = MemBudget::new(1 << 20);
        let mut q = RiQueue::new(Arc::new(NoEnv));
        let fo = Oid::from(1u32);
        assert!(!q.is_verified(fo, &[i(1)]));
        q.mark_verified(fo, vec![i(1)]);
        assert!(q.is_verified(fo, &[i(1)]));
        assert!(!q.is_verified(Oid::from(2u32), &[i(1)]));
        assert!(q.mark_locked(fo, LockMode::RowShare));
        assert!(!q.mark_locked(fo, LockMode::RowShare));
        q.set_draining(true);
        q.clear_verified();
        assert!(!q.is_verified(fo, &[i(1)]));
        q.reset(&mem);
        assert!(!q.is_draining());
        assert!(q.mark_locked(fo, LockMode::RowShare));
    }
}
