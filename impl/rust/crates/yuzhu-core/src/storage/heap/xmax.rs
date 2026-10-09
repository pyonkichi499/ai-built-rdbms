//! xmax and infomask assembly (`02-row-lock-rr.md` §3, §4.1, §6.1; contract 3).
//!
//! Only the functions in this file build or interpret the xmax-related bits of
//! a tuple header. Callers (heap, FK, VACUUM) never touch those bits.

use crate::error::{Error, Result};
use crate::storage::heap::tuple::{
    HEAP_KEYS_UPDATED, HEAP_XMAX_COMMITTED, HEAP_XMAX_EXCL_LOCK, HEAP_XMAX_INVALID,
    HEAP_XMAX_IS_MULTI, HEAP_XMAX_KEYSHR_LOCK, HEAP_XMAX_LOCK_ONLY, TupleHeader,
};
use crate::txn::multixact::{
    LockTupleMode, MultiMember, MultiXactId, MultiXactState, MultiXactTable,
};
use crate::txn::{CommandId, Xid};
use crate::types::Datum;

/// `FOR SHARE`: `EXCL | KEYSHR`.
pub const HEAP_XMAX_SHR_LOCK: u16 = HEAP_XMAX_EXCL_LOCK | HEAP_XMAX_KEYSHR_LOCK;
/// Both `XMIN_COMMITTED` and `XMIN_INVALID` set.
pub const HEAP_XMIN_FROZEN: u16 = 0x0300;
/// `KEYSHR`, `EXCL`, `LOCK_ONLY`, `XMAX_COMMITTED`, `XMAX_INVALID` and `IS_MULTI` (§3.1).
pub const XMAX_STATE_BITS: u16 = HEAP_XMAX_KEYSHR_LOCK
    | HEAP_XMAX_EXCL_LOCK
    | HEAP_XMAX_LOCK_ONLY
    | HEAP_XMAX_COMMITTED
    | HEAP_XMAX_INVALID
    | HEAP_XMAX_IS_MULTI;

/// The strength bits of the infomask (`KEYSHR | EXCL`).
const STRENGTH_BITS: u16 = HEAP_XMAX_SHR_LOCK;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmaxState {
    None,
    Locker { xid: Xid, mode: LockTupleMode },
    LockerMulti(MultiXactId),
    Updater { xid: Xid, keys_updated: bool },
}

fn corrupt(h: &TupleHeader, what: &str) -> Error {
    Error::corrupted(format!(
        "invalid heap tuple xmax: {what} (xmax {}, infomask {:#06x}, infomask2 {:#06x})",
        h.xmax.0, h.infomask, h.infomask2
    ))
}

/// Strength rank (`KeyShare < Share < NoKeyExclusive < Exclusive`).
fn rank(mode: LockTupleMode) -> u8 {
    match mode {
        LockTupleMode::KeyShare => 0,
        LockTupleMode::Share => 1,
        LockTupleMode::NoKeyExclusive => 2,
        LockTupleMode::Exclusive => 3,
    }
}

fn stronger(a: LockTupleMode, b: LockTupleMode) -> LockTupleMode {
    if rank(b) > rank(a) { b } else { a }
}

/// Reads the xmax state (§3.2). Damaged combinations are `XX001`.
pub fn decode_xmax(h: &TupleHeader) -> Result<XmaxState> {
    if h.xmax_invalid() {
        return Ok(XmaxState::None);
    }
    let multi = h.infomask & HEAP_XMAX_IS_MULTI != 0;
    let lock_only = h.infomask & HEAP_XMAX_LOCK_ONLY != 0;
    if multi && !lock_only {
        return Err(corrupt(h, "IS_MULTI without LOCK_ONLY"));
    }
    if lock_only {
        let mode = mode_from_bits(h.infomask, h.infomask2)
            .map_err(|_| corrupt(h, "LOCK_ONLY without lock strength bits"))?;
        return Ok(if multi {
            XmaxState::LockerMulti(MultiXactId(h.xmax.0))
        } else {
            XmaxState::Locker { xid: h.xmax, mode }
        });
    }
    Ok(XmaxState::Updater {
        xid: h.xmax,
        keys_updated: h.infomask2 & HEAP_KEYS_UPDATED != 0,
    })
}

/// `infomask & 0x0300 == 0x0300`, or xmin is the bootstrap / frozen XID.
pub fn xmin_is_frozen(h: &TupleHeader) -> bool {
    h.infomask & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN
        || h.xmin == Xid::BOOTSTRAP
        || h.xmin == Xid::FROZEN
}

/// `LOCK_ONLY` is set (single or `MultiXact`). Visibility ignores xmax then.
pub fn xmax_is_lock_only(h: &TupleHeader) -> bool {
    h.infomask & HEAP_XMAX_LOCK_ONLY != 0
}

/// The raw xmax triple, to detect changes across a latch release (§5.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawXmax {
    pub xmax: Xid,
    pub infomask: u16,
    pub infomask2: u16,
}

impl RawXmax {
    pub fn of(h: &TupleHeader) -> RawXmax {
        RawXmax {
            xmax: h.xmax,
            infomask: h.infomask,
            infomask2: h.infomask2,
        }
    }
}

/// `(bits to add to infomask, bits to add to infomask2)` (§3.2).
pub fn lock_mode_bits(mode: LockTupleMode) -> (u16, u16) {
    match mode {
        LockTupleMode::KeyShare => (HEAP_XMAX_KEYSHR_LOCK, 0),
        LockTupleMode::Share => (HEAP_XMAX_SHR_LOCK, 0),
        LockTupleMode::NoKeyExclusive => (HEAP_XMAX_EXCL_LOCK, 0),
        LockTupleMode::Exclusive => (HEAP_XMAX_EXCL_LOCK, HEAP_KEYS_UPDATED),
    }
}

/// The inverse of [`lock_mode_bits`]. No strength bits is `XX001`.
pub fn mode_from_bits(infomask: u16, infomask2: u16) -> Result<LockTupleMode> {
    match infomask & STRENGTH_BITS {
        HEAP_XMAX_KEYSHR_LOCK => Ok(LockTupleMode::KeyShare),
        HEAP_XMAX_SHR_LOCK => Ok(LockTupleMode::Share),
        HEAP_XMAX_EXCL_LOCK => Ok(if infomask2 & HEAP_KEYS_UPDATED != 0 {
            LockTupleMode::Exclusive
        } else {
            LockTupleMode::NoKeyExclusive
        }),
        _ => Err(Error::corrupted(format!(
            "invalid heap tuple: lock-only xmax without strength bits (infomask {infomask:#06x})"
        ))),
    }
}

/// Row lock conflicts among lockers (§3.4, upper-left 4x4).
pub fn tuple_lock_conflicts(held: LockTupleMode, req: LockTupleMode) -> bool {
    use LockTupleMode::{Exclusive, KeyShare, NoKeyExclusive, Share};
    match (held, req) {
        (Exclusive, _)
        | (_, Exclusive)
        | (Share, NoKeyExclusive)
        | (NoKeyExclusive, Share | NoKeyExclusive) => true,
        (KeyShare, _) | (_, KeyShare) | (Share, Share) => false,
    }
}

/// External state needed to decide, passed as closures so the functions stay pure.
#[allow(missing_debug_implementations)]
pub struct XmaxEnv<'a> {
    /// Our own XID (`WriteCtx.xid`). Comparisons with it happen only here.
    pub me: Xid,
    /// `xid_state(x) == Running`.
    pub is_running: &'a dyn Fn(Xid) -> bool,
    pub multi: &'a MultiXactTable,
}

impl XmaxEnv<'_> {
    fn alive(&self, x: Xid) -> bool {
        x == self.me || (self.is_running)(x)
    }

    fn expand(&self, id: MultiXactId) -> MultiXactState {
        self.multi.expand(id, &|x| self.alive(x))
    }
}

/// The complete xmax values to write. `infomask` / `infomask2` are final values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct XmaxWrite {
    pub xmax: Xid,
    pub cmax: Option<CommandId>,
    pub infomask: u16,
    pub infomask2: u16,
}

impl XmaxWrite {
    pub fn apply(&self, h: &mut TupleHeader) {
        h.xmax = self.xmax;
        h.infomask = self.infomask;
        h.infomask2 = self.infomask2;
        if let Some(c) = self.cmax {
            h.cmax = c;
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LockDecision {
    /// We already hold at least the requested strength; write nothing.
    AlreadyHeld,
    Write(XmaxWrite),
    /// Conflicting holders (ascending, unique, non-empty, never ourselves).
    /// `i_am_member`: we are already a locker of this row.
    Wait {
        xids: Vec<Xid>,
        i_am_member: bool,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UpdateDecision {
    Proceed,
    Wait { xids: Vec<Xid>, i_am_member: bool },
}

fn lock_write(h: &TupleHeader, xmax: Xid, extra: u16, strongest: LockTupleMode) -> XmaxWrite {
    let (bits, bits2) = lock_mode_bits(strongest);
    XmaxWrite {
        xmax,
        cmax: None,
        infomask: (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_LOCK_ONLY | extra | bits,
        infomask2: (h.infomask2 & !HEAP_KEYS_UPDATED) | bits2,
    }
}

fn write_single(h: &TupleHeader, me: Xid, mode: LockTupleMode) -> XmaxWrite {
    lock_write(h, me, 0, mode)
}

fn write_multi(
    h: &TupleHeader,
    env: &XmaxEnv<'_>,
    members: &[MultiMember],
) -> Result<LockDecision> {
    let strongest = members
        .iter()
        .map(|m| m.mode)
        .reduce(stronger)
        .ok_or_else(|| Error::internal("empty MultiXact member list"))?;
    let id = env.multi.create(members)?;
    Ok(LockDecision::Write(lock_write(
        h,
        Xid(id.0),
        HEAP_XMAX_IS_MULTI,
        strongest,
    )))
}

fn sorted_unique(mut xids: Vec<Xid>) -> Vec<Xid> {
    xids.sort_unstable();
    xids.dedup();
    xids
}

/// New xmax for a row lock (PostgreSQL's `compute_new_xmax_infomask` without
/// updaters). Decision table: §6.1.
pub fn plan_lock(h: &TupleHeader, req: LockTupleMode, env: &XmaxEnv<'_>) -> Result<LockDecision> {
    let me = env.me;
    match decode_xmax(h)? {
        XmaxState::None => Ok(LockDecision::Write(write_single(h, me, req))),
        XmaxState::Locker { xid, mode } if xid == me => Ok(if rank(mode) >= rank(req) {
            LockDecision::AlreadyHeld
        } else {
            LockDecision::Write(write_single(h, me, req))
        }),
        XmaxState::Locker { xid, mode } => {
            if !env.alive(xid) {
                Ok(LockDecision::Write(write_single(h, me, req)))
            } else if tuple_lock_conflicts(mode, req) {
                Ok(LockDecision::Wait {
                    xids: vec![xid],
                    i_am_member: false,
                })
            } else {
                write_multi(
                    h,
                    env,
                    &[
                        MultiMember { xid, mode },
                        MultiMember { xid: me, mode: req },
                    ],
                )
            }
        }
        XmaxState::LockerMulti(id) => match env.expand(id) {
            MultiXactState::Dead => Ok(LockDecision::Write(write_single(h, me, req))),
            MultiXactState::Live(members) if members.is_empty() => {
                Ok(LockDecision::Write(write_single(h, me, req)))
            }
            MultiXactState::Live(members) => {
                let mine = members.iter().find(|m| m.xid == me).map(|m| m.mode);
                let others: Vec<MultiMember> =
                    members.iter().copied().filter(|m| m.xid != me).collect();
                let conflicting: Vec<Xid> = others
                    .iter()
                    .filter(|m| tuple_lock_conflicts(m.mode, req))
                    .map(|m| m.xid)
                    .collect();
                if !conflicting.is_empty() {
                    return Ok(LockDecision::Wait {
                        xids: sorted_unique(conflicting),
                        i_am_member: mine.is_some(),
                    });
                }
                if mine.is_some_and(|m| rank(m) >= rank(req)) {
                    return Ok(LockDecision::AlreadyHeld);
                }
                let new_mode = mine.map_or(req, |m| stronger(m, req));
                if others.is_empty() {
                    return Ok(LockDecision::Write(write_single(h, me, new_mode)));
                }
                let mut new_members = others;
                new_members.push(MultiMember {
                    xid: me,
                    mode: new_mode,
                });
                write_multi(h, env, &new_members)
            }
        },
        XmaxState::Updater { xid, .. } if xid == me => Err(Error::internal(
            "plan_lock: row was updated by the current transaction",
        )),
        // The caller (`satisfies_update`) has already turned a committed
        // updater into Updated / Deleted, so a finished updater here aborted.
        XmaxState::Updater { xid, .. } => {
            if env.alive(xid) {
                Ok(LockDecision::Wait {
                    xids: vec![xid],
                    i_am_member: false,
                })
            } else {
                Ok(LockDecision::Write(write_single(h, me, req)))
            }
        }
    }
}

/// Whether UPDATE / DELETE may overwrite xmax (conflicts with every other
/// running holder, D6). `Proceed` means the caller may write while holding the latch.
pub fn plan_update(h: &TupleHeader, env: &XmaxEnv<'_>) -> Result<UpdateDecision> {
    let me = env.me;
    let wait = |xids: Vec<Xid>, i_am_member| UpdateDecision::Wait { xids, i_am_member };
    match decode_xmax(h)? {
        XmaxState::None => Ok(UpdateDecision::Proceed),
        XmaxState::Locker { xid, .. } if xid == me => Ok(UpdateDecision::Proceed),
        XmaxState::Locker { xid, .. } | XmaxState::Updater { xid, .. } if xid == me => {
            // Unreachable for Locker (handled above); an own updater is a caller bug.
            Err(Error::internal(
                "plan_update: row was already updated by the current transaction",
            ))
        }
        XmaxState::Locker { xid, .. } | XmaxState::Updater { xid, .. } => {
            if env.alive(xid) {
                Ok(wait(vec![xid], false))
            } else {
                Ok(UpdateDecision::Proceed)
            }
        }
        XmaxState::LockerMulti(id) => match env.expand(id) {
            MultiXactState::Dead => Ok(UpdateDecision::Proceed),
            MultiXactState::Live(members) => {
                let i_am_member = members.iter().any(|m| m.xid == me);
                let others: Vec<Xid> = members.iter().map(|m| m.xid).filter(|x| *x != me).collect();
                if others.is_empty() {
                    Ok(UpdateDecision::Proceed)
                } else {
                    Ok(wait(sorted_unique(others), i_am_member))
                }
            }
        },
    }
}

/// xmax of an updater / deleter: lock bits dropped, `KEYS_UPDATED` as asked, cmax = `cid`.
pub fn stamp_updater(h: &TupleHeader, me: Xid, cid: CommandId, keys_updated: bool) -> XmaxWrite {
    XmaxWrite {
        xmax: me,
        cmax: Some(cid),
        infomask: h.infomask & !XMAX_STATE_BITS,
        infomask2: (h.infomask2 & !HEAP_KEYS_UPDATED)
            | if keys_updated { HEAP_KEYS_UPDATED } else { 0 },
    }
}

/// VACUUM: all lockers of a lock-only xmax have finished. `None` / `Updater` give false.
pub fn lockers_all_finished(h: &TupleHeader, env: &XmaxEnv<'_>) -> Result<bool> {
    Ok(match decode_xmax(h)? {
        XmaxState::None | XmaxState::Updater { .. } => false,
        XmaxState::Locker { xid, .. } => !env.alive(xid),
        XmaxState::LockerMulti(id) => match env.expand(id) {
            MultiXactState::Dead => true,
            MultiXactState::Live(members) => members.is_empty(),
        },
    })
}

/// VACUUM (freezing / pruning): xmax cleared to "none". The caller has decided
/// that the xmax is a finished locker or an aborted updater.
pub fn invalidate_xmax(h: &TupleHeader) -> XmaxWrite {
    XmaxWrite {
        xmax: Xid::INVALID,
        cmax: None,
        infomask: (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_INVALID,
        infomask2: h.infomask2 & !HEAP_KEYS_UPDATED,
    }
}

/// VACUUM (freezing): `infomask` with both `XMIN_FROZEN` bits set.
pub fn with_xmin_frozen(infomask: u16) -> u16 {
    infomask | HEAP_XMIN_FROZEN
}

/// Whether an UPDATE changes a key column (sets `KEYS_UPDATED`). Conservative:
/// never a false negative; floats compare by bit pattern.
pub fn keys_changed(key_columns: &[bool], old: &[Datum], new: &[Datum]) -> bool {
    key_columns
        .iter()
        .enumerate()
        .filter(|(_, is_key)| **is_key)
        .any(|(i, _)| match (old.get(i), new.get(i)) {
            (Some(Datum::Float4(a)), Some(Datum::Float4(b))) => a.to_bits() != b.to_bits(),
            (Some(Datum::Float8(a)), Some(Datum::Float8(b))) => a.to_bits() != b.to_bits(),
            (Some(a), Some(b)) => a != b,
            _ => true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlData, ControlFileHandle};
    use crate::storage::vfs::{SimVfs, Vfs};
    use crate::types::Tid;
    use LockTupleMode::{Exclusive, KeyShare, NoKeyExclusive, Share};
    use std::collections::HashSet;
    use std::sync::Arc;

    const ALL: [LockTupleMode; 4] = [KeyShare, Share, NoKeyExclusive, Exclusive];
    const ME: Xid = Xid(0x0102);
    const OTHER: Xid = Xid(0x0200);
    const THIRD: Xid = Xid(0x0300);

    /// M2 §3.7 tuple: `(1, 'hello', NULL, true)`.
    fn base() -> TupleHeader {
        TupleHeader {
            xmin: Xid(0x0100),
            xmax: Xid::INVALID,
            cmin: 0,
            cmax: 0,
            ctid: Tid {
                block: 0,
                offset: 1,
            },
            infomask2: 4,
            infomask: 0x0803,
            hoff: 40,
        }
    }

    fn table() -> MultiXactTable {
        let sim = SimVfs::new(1);
        let vfs: Arc<dyn Vfs> = Arc::new(sim);
        let ctl = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(1, 2, 131_072)).unwrap(),
        );
        let t = MultiXactTable::new(ctl);
        t.ensure_reserved().unwrap();
        t
    }

    struct Fx {
        multi: MultiXactTable,
        running: HashSet<Xid>,
    }

    impl Fx {
        fn new(running: &[Xid]) -> Fx {
            Fx {
                multi: table(),
                running: running.iter().copied().collect(),
            }
        }

        fn with<R>(&self, f: impl FnOnce(&XmaxEnv<'_>) -> R) -> R {
            let is_running = |x: Xid| self.running.contains(&x);
            f(&XmaxEnv {
                me: ME,
                is_running: &is_running,
                multi: &self.multi,
            })
        }
    }

    fn bytes_of(h: &TupleHeader) -> [u8; 35] {
        let mut b = [0u8; 35];
        h.write(&mut b);
        b
    }

    fn locked(h: &TupleHeader, xid: Xid, mode: LockTupleMode) -> TupleHeader {
        let mut out = *h;
        let (b1, b2) = lock_mode_bits(mode);
        out.xmax = xid;
        out.infomask = (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_LOCK_ONLY | b1;
        out.infomask2 = (h.infomask2 & !HEAP_KEYS_UPDATED) | b2;
        out
    }

    fn updated(h: &TupleHeader, xid: Xid, keys: bool) -> TupleHeader {
        let mut out = *h;
        stamp_updater(h, xid, 7, keys).apply(&mut out);
        out
    }

    fn multi_of(fx: &Fx, h: &TupleHeader, members: &[(Xid, LockTupleMode)]) -> TupleHeader {
        let ms: Vec<MultiMember> = members
            .iter()
            .map(|&(xid, mode)| MultiMember { xid, mode })
            .collect();
        let id = fx.multi.create(&ms).unwrap();
        let strongest = ms.iter().map(|m| m.mode).reduce(stronger).unwrap();
        let (b1, b2) = lock_mode_bits(strongest);
        let mut out = *h;
        out.xmax = Xid(id.0);
        out.infomask =
            (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_IS_MULTI | b1;
        out.infomask2 = (h.infomask2 & !HEAP_KEYS_UPDATED) | b2;
        out
    }

    fn wait(xids: &[Xid], i_am_member: bool) -> LockDecision {
        LockDecision::Wait {
            xids: xids.to_vec(),
            i_am_member,
        }
    }

    // --- constants and bit mapping ---------------------------------------

    #[test]
    fn constants() {
        assert_eq!(XMAX_STATE_BITS, 0x1CD0);
        assert_eq!(HEAP_XMAX_SHR_LOCK, 0x0050);
        assert_eq!(HEAP_XMIN_FROZEN, 0x0300);
    }

    #[test]
    fn mode_bits_roundtrip() {
        for m in ALL {
            let (b1, b2) = lock_mode_bits(m);
            assert_eq!(mode_from_bits(b1 | HEAP_XMAX_LOCK_ONLY, b2).unwrap(), m);
        }
        assert_eq!(lock_mode_bits(KeyShare), (0x0010, 0));
        assert_eq!(lock_mode_bits(Share), (0x0050, 0));
        assert_eq!(lock_mode_bits(NoKeyExclusive), (0x0040, 0));
        assert_eq!(lock_mode_bits(Exclusive), (0x0040, 0x2000));
        assert_eq!(
            mode_from_bits(0x0080, 0).unwrap_err().sqlstate.code(),
            "XX001"
        );
    }

    /// §3.4: the 4x4 lock matrix measured on PostgreSQL 17 (true = WAIT).
    #[test]
    fn conflict_matrix_4x4() {
        // rows: held, columns: req = KeyShare, Share, NoKeyUpdate, Update
        let expect = [
            [false, false, false, true],
            [false, false, true, true],
            [false, true, true, true],
            [true, true, true, true],
        ];
        for (i, h) in ALL.iter().enumerate() {
            for (j, r) in ALL.iter().enumerate() {
                assert_eq!(tuple_lock_conflicts(*h, *r), expect[i][j], "{h:?} vs {r:?}");
            }
        }
    }

    // --- §3.5 fixed bytes -------------------------------------------------

    fn check(h: &TupleHeader, xmax: [u8; 8], cmax: [u8; 4], infomask2: [u8; 2], infomask: [u8; 2]) {
        let b = bytes_of(h);
        assert_eq!(b[8..16], xmax, "xmax");
        assert_eq!(b[20..24], cmax, "cmax");
        assert_eq!(b[30..32], infomask2, "infomask2");
        assert_eq!(b[32..34], infomask, "infomask");
    }

    #[test]
    fn spec_3_5_header_bytes() {
        let fx = Fx::new(&[ME]);
        let x = [0x02, 0x01, 0, 0, 0, 0, 0, 0];
        let zero4 = [0u8; 4];
        let b = base();
        check(&b, [0; 8], zero4, [4, 0], [0x03, 0x08]);

        for (mode, im2, im) in [
            (KeyShare, [4, 0], [0x93, 0]),
            (Share, [4, 0], [0xD3, 0]),
            (NoKeyExclusive, [4, 0], [0xC3, 0]),
            (Exclusive, [4, 0x20], [0xC3, 0]),
        ] {
            let LockDecision::Write(w) = fx.with(|e| plan_lock(&b, mode, e)).unwrap() else {
                panic!("expected write");
            };
            let mut h = b;
            w.apply(&mut h);
            check(&h, x, zero4, im2, im);
            assert_eq!(
                decode_xmax(&h).unwrap(),
                XmaxState::Locker { xid: ME, mode }
            );
        }

        // MultiXactId 7 (strongest Share / Exclusive): bits only.
        let mut h = b;
        let w = lock_write(&b, Xid(7), HEAP_XMAX_IS_MULTI, Share);
        w.apply(&mut h);
        check(&h, [7, 0, 0, 0, 0, 0, 0, 0], zero4, [4, 0], [0xD3, 0x10]);
        let w = lock_write(&b, Xid(7), HEAP_XMAX_IS_MULTI, Exclusive);
        let mut h = b;
        w.apply(&mut h);
        check(&h, [7, 0, 0, 0, 0, 0, 0, 0], zero4, [4, 0x20], [0xC3, 0x10]);
        assert_eq!(
            decode_xmax(&h).unwrap(),
            XmaxState::LockerMulti(MultiXactId(7))
        );

        // non-key UPDATE (cid 7)
        let mut h = b;
        stamp_updater(&b, ME, 7, false).apply(&mut h);
        check(&h, x, [7, 0, 0, 0], [4, 0], [0x03, 0x00]);
        assert_eq!(
            decode_xmax(&h).unwrap(),
            XmaxState::Updater {
                xid: ME,
                keys_updated: false
            }
        );
        // key UPDATE / DELETE
        let mut h = b;
        stamp_updater(&b, ME, 7, true).apply(&mut h);
        check(&h, x, [7, 0, 0, 0], [4, 0x20], [0x03, 0x00]);
        assert_eq!(
            decode_xmax(&h).unwrap(),
            XmaxState::Updater {
                xid: ME,
                keys_updated: true
            }
        );
    }

    // --- decode -------------------------------------------------------------

    #[test]
    fn decode_none_and_corrupt() {
        let mut h = base();
        assert_eq!(decode_xmax(&h).unwrap(), XmaxState::None);
        // xmax set but XMAX_INVALID: none
        h.xmax = OTHER;
        assert_eq!(decode_xmax(&h).unwrap(), XmaxState::None);
        // xmax == 0 without the flag: none
        let mut h = base();
        h.infomask &= !HEAP_XMAX_INVALID;
        assert_eq!(decode_xmax(&h).unwrap(), XmaxState::None);
        // IS_MULTI without LOCK_ONLY
        let mut h = base();
        h.xmax = OTHER;
        h.infomask = (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_IS_MULTI;
        assert_eq!(decode_xmax(&h).unwrap_err().sqlstate.code(), "XX001");
        // LOCK_ONLY without strength
        let mut h = base();
        h.xmax = OTHER;
        h.infomask = (h.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_LOCK_ONLY;
        assert_eq!(decode_xmax(&h).unwrap_err().sqlstate.code(), "XX001");
    }

    #[test]
    fn frozen_and_lock_only_predicates() {
        let mut h = base();
        assert!(!xmin_is_frozen(&h));
        h.infomask |= 0x0100;
        assert!(!xmin_is_frozen(&h));
        h.infomask = with_xmin_frozen(h.infomask);
        assert!(xmin_is_frozen(&h));
        let mut h = base();
        h.xmin = Xid::BOOTSTRAP;
        assert!(xmin_is_frozen(&h));
        h.xmin = Xid::FROZEN;
        assert!(xmin_is_frozen(&h));
        assert!(!xmax_is_lock_only(&base()));
        assert!(xmax_is_lock_only(&locked(&base(), OTHER, Share)));
    }

    #[test]
    fn raw_xmax_detects_change() {
        let a = RawXmax::of(&base());
        let b = RawXmax::of(&locked(&base(), ME, Share));
        assert_ne!(a, b);
        assert_eq!(a, RawXmax::of(&base()));
    }

    // --- plan_lock ----------------------------------------------------------

    #[allow(clippy::needless_pass_by_value)]
    fn expect_single(d: LockDecision, h: &TupleHeader, mode: LockTupleMode) {
        let LockDecision::Write(w) = d else {
            panic!("expected Write, got {d:?}");
        };
        let mut out = *h;
        w.apply(&mut out);
        assert_eq!(
            decode_xmax(&out).unwrap(),
            XmaxState::Locker { xid: ME, mode }
        );
        assert_eq!(w.cmax, None);
    }

    #[test]
    fn lock_on_none_all_modes() {
        let fx = Fx::new(&[ME]);
        for m in ALL {
            expect_single(fx.with(|e| plan_lock(&base(), m, e)).unwrap(), &base(), m);
        }
        // a stale XMAX_INVALID xmax is overwritten and the flag cleared
        let mut h = base();
        h.xmax = OTHER;
        let LockDecision::Write(w) = fx.with(|e| plan_lock(&h, Share, e)).unwrap() else {
            panic!()
        };
        assert_eq!(w.infomask & HEAP_XMAX_INVALID, 0);
        assert_eq!(w.infomask & 0x0003, 0x0003, "unrelated bits kept");
    }

    #[test]
    fn lock_own_locker() {
        let fx = Fx::new(&[ME]);
        for held in ALL {
            let h = locked(&base(), ME, held);
            for req in ALL {
                let d = fx.with(|e| plan_lock(&h, req, e)).unwrap();
                if rank(held) >= rank(req) {
                    assert_eq!(d, LockDecision::AlreadyHeld, "{held:?}/{req:?}");
                } else {
                    expect_single(d, &h, req);
                }
            }
        }
    }

    /// Other running locker x requester: the full 4x4 table of single lockers.
    #[test]
    fn lock_other_running_locker_matrix() {
        let fx = Fx::new(&[ME, OTHER]);
        for held in ALL {
            let h = locked(&base(), OTHER, held);
            for req in ALL {
                let d = fx.with(|e| plan_lock(&h, req, e)).unwrap();
                if tuple_lock_conflicts(held, req) {
                    assert_eq!(d, wait(&[OTHER], false), "{held:?}/{req:?}");
                } else {
                    let LockDecision::Write(w) = d else {
                        panic!("{held:?}/{req:?}: {d:?}")
                    };
                    let mut out = h;
                    w.apply(&mut out);
                    assert_eq!(
                        out.infomask & XMAX_STATE_BITS & HEAP_XMAX_IS_MULTI,
                        HEAP_XMAX_IS_MULTI
                    );
                    let XmaxState::LockerMulti(id) = decode_xmax(&out).unwrap() else {
                        panic!()
                    };
                    let MultiXactState::Live(ms) = fx.multi.expand(id, &|_| true) else {
                        panic!()
                    };
                    assert_eq!(
                        ms,
                        vec![
                            MultiMember { xid: ME, mode: req },
                            MultiMember {
                                xid: OTHER,
                                mode: held
                            }
                        ]
                    );
                    // the infomask mirrors the strongest member
                    assert_eq!(
                        mode_from_bits(out.infomask, out.infomask2).unwrap(),
                        stronger(held, req)
                    );
                }
            }
        }
    }

    #[test]
    fn lock_multi_infomask_values() {
        // 2 x KEY SHARE -> 0x1090; 2 x SHARE -> 0x10D0; KEY SHARE + NO KEY UPDATE -> 0x10C0
        for (a, b, bits) in [
            (KeyShare, KeyShare, 0x1090u16),
            (Share, Share, 0x10D0),
            (KeyShare, NoKeyExclusive, 0x10C0),
        ] {
            let fx = Fx::new(&[ME, OTHER]);
            let h = locked(&base(), OTHER, a);
            let LockDecision::Write(w) = fx.with(|e| plan_lock(&h, b, e)).unwrap() else {
                panic!()
            };
            assert_eq!(w.infomask & XMAX_STATE_BITS, bits);
        }
    }

    #[test]
    fn lock_over_finished_locker_overwrites() {
        let fx = Fx::new(&[ME]); // OTHER is not running
        for held in ALL {
            let h = locked(&base(), OTHER, held);
            for req in ALL {
                expect_single(fx.with(|e| plan_lock(&h, req, e)).unwrap(), &h, req);
            }
        }
    }

    #[test]
    fn lock_multi_dead_and_boundary() {
        let fx = Fx::new(&[ME]);
        // all members finished
        let h = multi_of(&fx, &base(), &[(OTHER, Share), (THIRD, KeyShare)]);
        for req in ALL {
            expect_single(fx.with(|e| plan_lock(&h, req, e)).unwrap(), &h, req);
        }
        // id never created (boundary / swept)
        let mut h = base();
        h.xmax = Xid(999_999);
        h.infomask |= HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_IS_MULTI | HEAP_XMAX_KEYSHR_LOCK;
        h.infomask &= !HEAP_XMAX_INVALID;
        expect_single(fx.with(|e| plan_lock(&h, Share, e)).unwrap(), &h, Share);
    }

    #[test]
    fn lock_multi_waits_on_conflicting_members_only() {
        let fx = Fx::new(&[ME, OTHER, THIRD]);
        let h = multi_of(&fx, &base(), &[(OTHER, KeyShare), (THIRD, Share)]);
        // KeyShare req: no conflicts with either; I join as a third member.
        let d = fx.with(|e| plan_lock(&h, KeyShare, e)).unwrap();
        let LockDecision::Write(w) = d else {
            panic!("{d:?}")
        };
        let mut out = h;
        w.apply(&mut out);
        let XmaxState::LockerMulti(id) = decode_xmax(&out).unwrap() else {
            panic!()
        };
        let MultiXactState::Live(ms) = fx.multi.expand(id, &|_| true) else {
            panic!()
        };
        assert_eq!(ms.len(), 3);
        assert_eq!(mode_from_bits(out.infomask, out.infomask2).unwrap(), Share);
        // NoKeyExclusive conflicts with Share only
        assert_eq!(
            fx.with(|e| plan_lock(&h, NoKeyExclusive, e)).unwrap(),
            wait(&[THIRD], false)
        );
        // Exclusive conflicts with everyone, ascending xids
        assert_eq!(
            fx.with(|e| plan_lock(&h, Exclusive, e)).unwrap(),
            wait(&[OTHER, THIRD], false)
        );
    }

    #[test]
    fn lock_multi_with_me() {
        let fx = Fx::new(&[ME, OTHER]);
        // I hold Share, OTHER holds Share.
        let h = multi_of(&fx, &base(), &[(OTHER, Share), (ME, Share)]);
        assert_eq!(
            fx.with(|e| plan_lock(&h, KeyShare, e)).unwrap(),
            LockDecision::AlreadyHeld
        );
        assert_eq!(
            fx.with(|e| plan_lock(&h, Share, e)).unwrap(),
            LockDecision::AlreadyHeld
        );
        // upgrade conflicting with OTHER's Share: wait, and I am a member
        assert_eq!(
            fx.with(|e| plan_lock(&h, NoKeyExclusive, e)).unwrap(),
            wait(&[OTHER], true)
        );
        assert_eq!(
            fx.with(|e| plan_lock(&h, Exclusive, e)).unwrap(),
            wait(&[OTHER], true)
        );
        // I hold KeyShare, OTHER holds KeyShare: upgrading to Share replaces my member
        let h = multi_of(&fx, &base(), &[(OTHER, KeyShare), (ME, KeyShare)]);
        let LockDecision::Write(w) = fx.with(|e| plan_lock(&h, Share, e)).unwrap() else {
            panic!()
        };
        let mut out = h;
        w.apply(&mut out);
        let XmaxState::LockerMulti(id) = decode_xmax(&out).unwrap() else {
            panic!()
        };
        let MultiXactState::Live(ms) = fx.multi.expand(id, &|_| true) else {
            panic!()
        };
        assert_eq!(
            ms,
            vec![
                MultiMember {
                    xid: ME,
                    mode: Share
                },
                MultiMember {
                    xid: OTHER,
                    mode: KeyShare
                }
            ]
        );
    }

    #[test]
    fn lock_multi_only_me_alive() {
        // OTHER finished; only I remain. Upgrade collapses to a single locker.
        let fx = Fx::new(&[ME]);
        let h = multi_of(&fx, &base(), &[(OTHER, Share), (ME, KeyShare)]);
        assert_eq!(
            fx.with(|e| plan_lock(&h, KeyShare, e)).unwrap(),
            LockDecision::AlreadyHeld
        );
        expect_single(
            fx.with(|e| plan_lock(&h, Exclusive, e)).unwrap(),
            &h,
            Exclusive,
        );
    }

    #[test]
    fn lock_with_updater() {
        let fx = Fx::new(&[ME, OTHER]);
        let h = updated(&base(), OTHER, false);
        for req in ALL {
            assert_eq!(
                fx.with(|e| plan_lock(&h, req, e)).unwrap(),
                wait(&[OTHER], false)
            );
        }
        // aborted updater
        let fx2 = Fx::new(&[ME]);
        for req in ALL {
            expect_single(fx2.with(|e| plan_lock(&h, req, e)).unwrap(), &h, req);
        }
        // own updater is a caller bug
        let own = updated(&base(), ME, true);
        let e = fx.with(|e| plan_lock(&own, Share, e)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX000");
    }

    // --- plan_update --------------------------------------------------------

    #[test]
    fn update_plan_all_rows() {
        let fx = Fx::new(&[ME, OTHER, THIRD]);
        let p = |h: &TupleHeader| fx.with(|e| plan_update(h, e)).unwrap();
        let w = |xs: &[Xid], m| UpdateDecision::Wait {
            xids: xs.to_vec(),
            i_am_member: m,
        };

        assert_eq!(p(&base()), UpdateDecision::Proceed);
        for m in ALL {
            // own lock: proceed; other's running lock: wait (every strength, D6)
            assert_eq!(p(&locked(&base(), ME, m)), UpdateDecision::Proceed);
            assert_eq!(p(&locked(&base(), OTHER, m)), w(&[OTHER], false));
        }
        // running / finished updater
        assert_eq!(p(&updated(&base(), OTHER, false)), w(&[OTHER], false));
        assert_eq!(p(&updated(&base(), OTHER, true)), w(&[OTHER], false));
        let fx_done = Fx::new(&[ME]);
        let pd = |h: &TupleHeader| fx_done.with(|e| plan_update(h, e)).unwrap();
        assert_eq!(pd(&updated(&base(), OTHER, false)), UpdateDecision::Proceed);
        for m in ALL {
            assert_eq!(pd(&locked(&base(), OTHER, m)), UpdateDecision::Proceed);
        }
        // own updater: bug
        let own = updated(&base(), ME, false);
        assert_eq!(
            fx.with(|e| plan_update(&own, e))
                .unwrap_err()
                .sqlstate
                .code(),
            "XX000"
        );

        // MultiXact
        let h = multi_of(&fx, &base(), &[(OTHER, KeyShare), (THIRD, Share)]);
        assert_eq!(p(&h), w(&[OTHER, THIRD], false));
        let h = multi_of(&fx, &base(), &[(OTHER, KeyShare), (ME, Share)]);
        assert_eq!(p(&h), w(&[OTHER], true));
        let h = multi_of(&fx, &base(), &[(ME, KeyShare), (THIRD, Share)]);
        assert_eq!(p(&h), w(&[THIRD], true));
        // only me alive
        let h = multi_of(&fx_done, &base(), &[(OTHER, KeyShare), (ME, Share)]);
        assert_eq!(pd(&h), UpdateDecision::Proceed);
        // all dead
        let h = multi_of(&fx_done, &base(), &[(OTHER, KeyShare), (THIRD, Share)]);
        assert_eq!(pd(&h), UpdateDecision::Proceed);
    }

    // --- stamp / invalidate / lockers_all_finished --------------------------

    #[test]
    fn stamp_updater_clears_lock_bits() {
        let fx = Fx::new(&[ME]);
        // my own FOR UPDATE then UPDATE of a non-key column
        let h = locked(&base(), ME, Exclusive);
        let w = stamp_updater(&h, ME, 5, false);
        assert_eq!(w.infomask & XMAX_STATE_BITS, 0);
        assert_eq!(w.infomask2 & HEAP_KEYS_UPDATED, 0);
        assert_eq!(w.cmax, Some(5));
        assert_eq!(w.xmax, ME);
        // a MultiXact becomes a plain updater too
        let h = multi_of(&fx, &base(), &[(ME, Share), (OTHER, KeyShare)]);
        let w = stamp_updater(&h, ME, 1, true);
        assert_eq!(w.infomask & XMAX_STATE_BITS, 0);
        assert_eq!(w.infomask2 & HEAP_KEYS_UPDATED, HEAP_KEYS_UPDATED);
        assert_eq!(w.infomask & 0x0003, 0x0003);
        assert_eq!(w.infomask2 & 0x07FF, 4);
    }

    #[test]
    fn invalidate_xmax_values() {
        let fx = Fx::new(&[ME]);
        let mut h = multi_of(&fx, &base(), &[(OTHER, Share), (THIRD, Exclusive)]);
        h.cmax = 9;
        let w = invalidate_xmax(&h);
        assert_eq!(w.xmax, Xid::INVALID);
        assert_eq!(w.cmax, None);
        assert_eq!(w.infomask, 0x0803);
        assert_eq!(w.infomask2, 4);
        let mut out = h;
        w.apply(&mut out);
        assert_eq!(out.cmax, 9);
        assert_eq!(decode_xmax(&out).unwrap(), XmaxState::None);
        // an aborted updater's bits go too
        let h = updated(&base(), OTHER, true);
        let w = invalidate_xmax(&h);
        assert_eq!((w.infomask, w.infomask2), (0x0803, 4));
    }

    #[test]
    fn lockers_all_finished_cases() {
        let fx = Fx::new(&[ME, OTHER]);
        let q = |h: &TupleHeader| fx.with(|e| lockers_all_finished(h, e)).unwrap();
        assert!(!q(&base()));
        assert!(!q(&updated(&base(), THIRD, false)));
        assert!(!q(&updated(&base(), OTHER, false)));
        assert!(q(&locked(&base(), THIRD, Share)));
        assert!(!q(&locked(&base(), OTHER, Share)));
        assert!(!q(&locked(&base(), ME, KeyShare)));
        assert!(q(&multi_of(
            &fx,
            &base(),
            &[(THIRD, Share), (Xid(0x400), KeyShare)]
        )));
        assert!(!q(&multi_of(
            &fx,
            &base(),
            &[(THIRD, Share), (OTHER, KeyShare)]
        )));
        let mut bad = base();
        bad.xmax = OTHER;
        bad.infomask = (bad.infomask & !XMAX_STATE_BITS) | HEAP_XMAX_IS_MULTI;
        assert!(fx.with(|e| lockers_all_finished(&bad, e)).is_err());
    }

    // --- keys_changed -------------------------------------------------------

    #[test]
    fn keys_changed_rules() {
        let key = [true, false];
        let row = |a: Datum, b: Datum| vec![a, b];
        // only the non-key column changes
        assert!(!keys_changed(
            &key,
            &row(Datum::Int4(1), Datum::Int4(1)),
            &row(Datum::Int4(1), Datum::Int4(2))
        ));
        // key changes
        assert!(keys_changed(
            &key,
            &row(Datum::Int4(1), Datum::Null),
            &row(Datum::Int4(2), Datum::Null)
        ));
        assert!(keys_changed(
            &key,
            &row(Datum::Null, Datum::Null),
            &row(Datum::Int4(2), Datum::Null)
        ));
        assert!(!keys_changed(
            &key,
            &row(Datum::Null, Datum::Null),
            &row(Datum::Null, Datum::Null)
        ));
        // no key columns at all
        assert!(!keys_changed(
            &[false, false],
            &row(Datum::Int4(1), Datum::Null),
            &row(Datum::Int4(2), Datum::Int4(3))
        ));
        // floats: -0.0 vs +0.0 differ by bits; NaN is equal to itself by bits
        let k = [true];
        assert!(keys_changed(
            &k,
            &[Datum::Float8(0.0)],
            &[Datum::Float8(-0.0)]
        ));
        assert!(keys_changed(
            &k,
            &[Datum::Float4(0.0)],
            &[Datum::Float4(-0.0)]
        ));
        assert!(!keys_changed(
            &k,
            &[Datum::Float8(f64::NAN)],
            &[Datum::Float8(f64::NAN)]
        ));
        assert!(!keys_changed(
            &k,
            &[Datum::Float8(1.5)],
            &[Datum::Float8(1.5)]
        ));
        assert!(keys_changed(
            &k,
            &[Datum::Float8(1.5)],
            &[Datum::Float8(2.5)]
        ));
        // text
        assert!(keys_changed(
            &k,
            &[Datum::Text("a".into())],
            &[Datum::Text("b".into())]
        ));
        assert!(!keys_changed(
            &k,
            &[Datum::Text("a".into())],
            &[Datum::Text("a".into())]
        ));
    }
}
