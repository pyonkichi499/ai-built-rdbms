//! In-memory `MultiXact` table (m5 02 §4.3 / §6.8, D6, D36).
//!
//! A `MultiXact` groups several row-lock holders under one ID stored in a tuple's
//! xmax. Members live only in memory; the control file's `next_multi` is an
//! upper bound of IDs handed out. IDs below `boundary` (the value at startup)
//! always expand to [`MultiXactState::Dead`]. Updaters are never members.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::Xid;
use crate::control::ControlFileHandle;
use crate::error::{Error, Result};

/// IDs reserved per control-file write.
pub const MULTI_PREFETCH: u64 = 1024;
/// `limit - next` below this triggers a new reservation.
pub const MULTI_RESERVE_LOW: u64 = 512;

/// Row lock strength, weakest first.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum LockTupleMode {
    KeyShare,
    Share,
    NoKeyExclusive,
    Exclusive,
}

/// 0 is invalid. Stored in a tuple's xmax together with `IS_MULTI`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MultiXactId(pub u64);

/// A lock holder. Updaters are never members (D6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MultiMember {
    pub xid: Xid,
    pub mode: LockTupleMode,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MultiXactState {
    /// Members still running.
    Live(Vec<MultiMember>),
    /// Everyone finished, or the ID predates this startup.
    Dead,
}

#[derive(Debug)]
struct Inner {
    next: u64,
    limit: u64,
    boundary: u64,
    map: HashMap<MultiXactId, Vec<MultiMember>>,
    by_set: HashMap<Vec<MultiMember>, MultiXactId>,
    by_xid: HashMap<Xid, Vec<MultiXactId>>,
    /// Members not yet reported finished via `on_xact_end`.
    live: HashMap<MultiXactId, u32>,
}

#[derive(Debug)]
pub struct MultiXactTable {
    control: Arc<ControlFileHandle>,
    inner: Mutex<Inner>,
}

impl MultiXactTable {
    /// `boundary = next = limit = control.next_multi` (1 when the field is 0).
    pub fn new(control: Arc<ControlFileHandle>) -> MultiXactTable {
        let start = control.get().next_multi.max(1);
        MultiXactTable {
            control,
            inner: Mutex::new(Inner {
                next: start,
                limit: start,
                boundary: start,
                map: HashMap::new(),
                by_set: HashMap::new(),
                by_xid: HashMap::new(),
                live: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reserves the next batch of IDs when fewer than `MULTI_RESERVE_LOW`
    /// remain, writing `next_multi` to the control file. Does I/O: call it
    /// without holding a page latch (RW-D15). The table mutex is not held
    /// during the write.
    pub fn ensure_reserved(&self) -> Result<()> {
        let new_limit = {
            let g = self.lock();
            if g.limit - g.next >= MULTI_RESERVE_LOW {
                return Ok(());
            }
            g.limit + MULTI_PREFETCH
        };
        self.control.update(|c| {
            c.next_multi = c.next_multi.max(new_limit);
        })?;
        let mut g = self.lock();
        g.limit = g.limit.max(new_limit);
        Ok(())
    }

    /// One member per xid (strongest mode), sorted by xid. The same set yields
    /// the same ID while any member is still unfinished. No I/O.
    pub fn create(&self, members: &[MultiMember]) -> Result<MultiXactId> {
        let mut by_xid: HashMap<Xid, LockTupleMode> = HashMap::new();
        for m in members {
            by_xid
                .entry(m.xid)
                .and_modify(|mode| *mode = (*mode).max(m.mode))
                .or_insert(m.mode);
        }
        let mut set: Vec<MultiMember> = by_xid
            .into_iter()
            .map(|(xid, mode)| MultiMember { xid, mode })
            .collect();
        set.sort_by_key(|m| m.xid);

        let mut g = self.lock();
        if let Some(&id) = g.by_set.get(&set) {
            return Ok(id);
        }
        if g.next >= g.limit {
            return Err(Error::internal("multixact id reservation exhausted"));
        }
        let id = MultiXactId(g.next);
        g.next += 1;
        for m in &set {
            g.by_xid.entry(m.xid).or_default().push(id);
        }
        g.live
            .insert(id, u32::try_from(set.len()).unwrap_or(u32::MAX));
        g.by_set.insert(set.clone(), id);
        g.map.insert(id, set);
        Ok(id)
    }

    /// `Dead` for IDs below `boundary` or absent from the table; otherwise the
    /// members for which `still_running` holds (`Dead` if none). Does not
    /// modify the table.
    pub fn expand(&self, id: MultiXactId, still_running: &dyn Fn(Xid) -> bool) -> MultiXactState {
        let members = {
            let g = self.lock();
            if id.0 < g.boundary {
                return MultiXactState::Dead;
            }
            match g.map.get(&id) {
                Some(m) => m.clone(),
                None => return MultiXactState::Dead,
            }
        };
        let alive: Vec<MultiMember> = members
            .into_iter()
            .filter(|m| still_running(m.xid))
            .collect();
        if alive.is_empty() {
            MultiXactState::Dead
        } else {
            MultiXactState::Live(alive)
        }
    }

    /// Call at commit/abort, after removal from the `ProcArray`. Drops entries
    /// whose members have all ended.
    pub fn on_xact_end(&self, xid: Xid) {
        let mut g = self.lock();
        let Some(ids) = g.by_xid.remove(&xid) else {
            return;
        };
        for id in ids {
            let Some(n) = g.live.get_mut(&id) else {
                continue;
            };
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.live.remove(&id);
                if let Some(set) = g.map.remove(&id) {
                    g.by_set.remove(&set);
                }
            }
        }
    }

    /// IDs below this are always `Dead`.
    pub fn boundary(&self) -> MultiXactId {
        MultiXactId(self.lock().boundary)
    }

    /// Number of table entries (for tests).
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlData;
    use crate::storage::vfs::{SimVfs, Vfs};
    use std::collections::HashSet;

    fn mk() -> (Arc<dyn Vfs>, Arc<ControlFileHandle>, MultiXactTable) {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(7));
        let d = ControlData::initial(1, 2, 131_072);
        let c = Arc::new(ControlFileHandle::create(&vfs, &d).unwrap());
        let t = MultiXactTable::new(Arc::clone(&c));
        (vfs, c, t)
    }

    fn m(x: u64, mode: LockTupleMode) -> MultiMember {
        MultiMember { xid: Xid(x), mode }
    }

    fn all(_: Xid) -> bool {
        true
    }

    #[test]
    fn same_set_same_id_and_normalization() {
        let (_v, _c, t) = mk();
        t.ensure_reserved().unwrap();
        let a = t
            .create(&[m(5, LockTupleMode::Share), m(4, LockTupleMode::KeyShare)])
            .unwrap();
        let b = t
            .create(&[
                m(4, LockTupleMode::KeyShare),
                m(5, LockTupleMode::KeyShare),
                m(5, LockTupleMode::Share),
            ])
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(t.len(), 1);
        let c = t
            .create(&[m(4, LockTupleMode::Share), m(5, LockTupleMode::Share)])
            .unwrap();
        assert_ne!(a, c);
        assert_eq!(
            t.expand(a, &all),
            MultiXactState::Live(vec![
                m(4, LockTupleMode::KeyShare),
                m(5, LockTupleMode::Share)
            ])
        );
    }

    #[test]
    fn create_without_reservation_fails() {
        let (_v, _c, t) = mk();
        let e = t.create(&[m(3, LockTupleMode::Share)]).unwrap_err();
        assert!(e.message.contains("exhausted"));
    }

    #[test]
    fn dead_via_three_paths() {
        let (_v, c, t) = mk();
        t.ensure_reserved().unwrap();
        // 1. below boundary (also after restart).
        assert_eq!(t.expand(MultiXactId(0), &all), MultiXactState::Dead);
        // 2. all members finished (still_running false) without cleanup.
        let id = t
            .create(&[m(3, LockTupleMode::Share), m(4, LockTupleMode::Share)])
            .unwrap();
        assert_eq!(t.expand(id, &|x| x == Xid(4)).clone(), {
            MultiXactState::Live(vec![m(4, LockTupleMode::Share)])
        });
        assert_eq!(t.expand(id, &|_| false), MultiXactState::Dead);
        // 3. cleaned up.
        t.on_xact_end(Xid(3));
        t.on_xact_end(Xid(4));
        assert_eq!(t.expand(id, &all), MultiXactState::Dead);
        // Restarted table: old IDs are below boundary.
        let t2 = MultiXactTable::new(Arc::clone(&c));
        assert_eq!(t2.expand(id, &all), MultiXactState::Dead);
        assert_eq!(t2.boundary().0, c.get().next_multi);
    }

    #[test]
    fn on_xact_end_cleans_only_when_all_ended() {
        let (_v, _c, t) = mk();
        t.ensure_reserved().unwrap();
        let a = t
            .create(&[m(3, LockTupleMode::Share), m(4, LockTupleMode::Share)])
            .unwrap();
        let b = t
            .create(&[m(3, LockTupleMode::KeyShare), m(5, LockTupleMode::Share)])
            .unwrap();
        assert_eq!(t.len(), 2);
        t.on_xact_end(Xid(3));
        assert_eq!(t.len(), 2);
        t.on_xact_end(Xid(3)); // idempotent
        assert_eq!(t.len(), 2);
        t.on_xact_end(Xid(4));
        assert_eq!(t.len(), 1);
        t.on_xact_end(Xid(5));
        assert!(t.is_empty());
        // A finished set gets a fresh ID.
        let a2 = t
            .create(&[m(3, LockTupleMode::Share), m(4, LockTupleMode::Share)])
            .unwrap();
        assert!(a2 > a && a2 > b);
    }

    #[test]
    fn ensure_reserved_writes_control_file() {
        let (_v, c, t) = mk();
        assert_eq!(c.get().next_multi, 1);
        let g0 = c.generation();
        t.ensure_reserved().unwrap();
        assert_eq!(c.get().next_multi, 1 + MULTI_PREFETCH);
        let g1 = c.generation();
        assert!(g1 > g0);
        // Plenty left: no write.
        t.ensure_reserved().unwrap();
        assert_eq!(c.generation(), g1);
        // Consume until below the low-water mark, then it reserves again.
        let mut n = 0u64;
        while 1 + MULTI_PREFETCH - (1 + n) >= MULTI_RESERVE_LOW {
            t.create(&[m(10 + n, LockTupleMode::Share)]).unwrap();
            n += 1;
        }
        t.ensure_reserved().unwrap();
        assert_eq!(c.get().next_multi, 1 + 2 * MULTI_PREFETCH);
        assert!(c.generation() > g1);
    }

    #[test]
    fn restart_does_not_collide_ids() {
        let (vfs, c, t) = mk();
        t.ensure_reserved().unwrap();
        let mut old = HashSet::new();
        for i in 0..10 {
            old.insert(t.create(&[m(10 + i, LockTupleMode::Share)]).unwrap());
        }
        drop((t, c));
        let c2 = Arc::new(ControlFileHandle::open(&vfs).unwrap());
        let t2 = MultiXactTable::new(c2);
        t2.ensure_reserved().unwrap();
        let fresh = t2.create(&[m(99, LockTupleMode::KeyShare)]).unwrap();
        assert!(!old.contains(&fresh));
        assert!(old.iter().all(|&o| o < fresh));
        for o in old {
            assert_eq!(t2.expand(o, &all), MultiXactState::Dead);
        }
        assert_eq!(
            t2.expand(fresh, &all),
            MultiXactState::Live(vec![m(99, LockTupleMode::KeyShare)])
        );
    }

    #[test]
    fn concurrent_creates_get_unique_ids() {
        let (_v, _c, t) = mk();
        let t = Arc::new(t);
        t.ensure_reserved().unwrap();
        let hs: Vec<_> = (0..4u64)
            .map(|k| {
                let t = Arc::clone(&t);
                std::thread::spawn(move || {
                    (0..100u64)
                        .map(|i| {
                            t.create(&[m(1000 + k * 100 + i, LockTupleMode::Share)])
                                .unwrap()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let ids: Vec<_> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        let set: HashSet<_> = ids.iter().collect();
        assert_eq!(set.len(), 400);
    }
}
