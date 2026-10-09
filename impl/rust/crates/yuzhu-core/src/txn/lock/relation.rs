//! リレーションのロックの「解決 → ロック → 再解決」の輪と名前予約ロック
//! （`m5/01-lock-txn.md` §6.3。LK-4a）。
//!
//! `ddl::*` は `session` を使えないので、`catalog` にも `session` にも依存しない形でここに置く。
//! VC・DB・AU も使う。

use super::{LockManager, LockMode, LockScope, LockTag, WaitCtl};
use crate::backend::BackendId;
use crate::error::{Error, Result, sqlstate};
use crate::types::Oid;

/// `pg_class` のクラス OID（`Object` タグの `class`。名前予約ロック）。
const PG_CLASS_CLASS_OID: Oid = 1259;

#[derive(Clone, Copy, Debug)]
pub struct RelLockOpts<'a> {
    pub mode: LockMode,
    /// `Some` なら NOWAIT（待たない）。取れなければ 55P03 `could not obtain lock on relation "{label}"`。
    /// label は文に**書かれたとおりの名前**（スキーマを書いたなら `public.a`、書かなければ `a`。実機 E12）。
    pub nowait_label: Option<&'a str>,
}

/// PostgreSQL の `RangeVarGetRelidExtended` に倣う（`namespace.c`）。
///
/// `resolve` は最新のカタログ（カタログ用スナップショット + 自分の変更。D12）で名前を引き、
/// `(db, rel)`（db は共有カタログなら 0）を返す。見つからなければ `Ok(None)`（ロックしない。
/// 42P01 は呼び出し側）。`generation` はカタログのキャッシュの世代。
///
/// ロックを取った後に世代が変わっていたら（待っている間に何かがコミットされた）、もう一度解決して
/// 同じ OID か確かめる。違えばロックを外してやり直す。
pub fn lock_relation(
    locks: &LockManager,
    backend: BackendId,
    wait: &WaitCtl<'_>,
    opts: &RelLockOpts<'_>,
    generation: &dyn Fn() -> u64,
    resolve: &mut dyn FnMut() -> Result<Option<(Oid, Oid)>>,
) -> Result<Option<(Oid, Oid)>> {
    loop {
        wait.interrupts.check()?;
        let g0 = generation();
        let Some((db, rel)) = resolve()? else {
            return Ok(None);
        };
        let tag = LockTag::Relation { db, rel };
        match opts.nowait_label {
            None => locks.acquire(backend, tag, opts.mode, LockScope::Transaction, wait)?,
            Some(label) => {
                if !locks.try_acquire(backend, tag, opts.mode, LockScope::Transaction) {
                    return Err(Error::new(
                        sqlstate::LOCK_NOT_AVAILABLE,
                        format!("could not obtain lock on relation \"{label}\""),
                    ));
                }
            }
        }
        if generation() == g0 {
            return Ok(Some((db, rel)));
        }
        match resolve()? {
            Some(k) if k == (db, rel) => return Ok(Some((db, rel))),
            _ => locks.release(backend, tag, opts.mode),
        }
    }
}

/// 名前予約ロック（LK-D15）。`Object { db, class: 1259, obj: name_key(namespace, name) }` を
/// Exclusive、トランザクションスコープで取る。
pub fn lock_name(
    locks: &LockManager,
    backend: BackendId,
    wait: &WaitCtl<'_>,
    db: Oid,
    namespace: Oid,
    name: &str,
) -> Result<()> {
    let tag = LockTag::Object {
        db,
        class: PG_CLASS_CLASS_OID,
        obj: name_key(namespace, name),
    };
    locks.acquire(
        backend,
        tag,
        LockMode::Exclusive,
        LockScope::Transaction,
        wait,
    )
}

/// FNV-1a 64 ビットを、namespace の 4 バイト（リトルエンディアン）→ name の UTF-8 バイトの順に計算する。
pub fn name_key(namespace: Oid, name: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    namespace
        .to_le_bytes()
        .iter()
        .chain(name.as_bytes())
        .fold(OFFSET_BASIS, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME))
}

/// 比較 `held` ⊇ `wanted`: `held` を保持していれば `wanted` の要求は満たされる（衝突する相手の集合が
/// `wanted` のそれを含む。例: `RowExclusive` の保持は `AccessShare` の要求を満たす）。
pub fn mode_covers(held: LockMode, wanted: LockMode) -> bool {
    held.conflict_mask() & wanted.conflict_mask() == wanted.conflict_mask()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use super::*;
    use crate::interrupt::InterruptFlag;

    fn setup(n: u64) -> std::sync::Arc<LockManager> {
        let lm = LockManager::new();
        for i in 1..=n {
            lm.register_backend(BackendId(i), 100 + i32::try_from(i).unwrap());
        }
        lm
    }

    fn ctl(flag: &InterruptFlag) -> WaitCtl<'_> {
        WaitCtl {
            lock_timeout: Some(Duration::from_millis(200)),
            deadlock_timeout: Duration::from_secs(30),
            interrupts: flag,
        }
    }

    fn opts(mode: LockMode) -> RelLockOpts<'static> {
        RelLockOpts {
            mode,
            nowait_label: None,
        }
    }

    #[test]
    fn name_key_matches_the_fixed_values() {
        assert_eq!(name_key(2200, "t"), 0x6c68_3d56_3e00_69e3);
        assert_eq!(name_key(2200, "orders"), 0x85d5_fa88_9e7f_428c);
        assert_eq!(name_key(0, ""), 0x4d25_767f_9dce_13f5);
    }

    #[test]
    fn mode_covers_follows_the_conflict_sets() {
        use LockMode::*;
        assert!(mode_covers(RowExclusive, AccessShare));
        assert!(mode_covers(AccessExclusive, Share));
        assert!(mode_covers(Share, Share));
        assert!(!mode_covers(AccessShare, RowShare));
        assert!(!mode_covers(RowExclusive, Share));
        for m in LockMode::ALL {
            assert!(mode_covers(m, m));
            assert!(mode_covers(AccessExclusive, m));
        }
    }

    #[test]
    fn a_found_relation_is_locked_and_kept() {
        let lm = setup(1);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        let got = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::RowExclusive),
            &|| 7,
            &mut || Ok(Some((5, 42))),
        )
        .unwrap();
        assert_eq!(got, Some((5, 42)));
        assert!(lm.holds(
            BackendId(1),
            LockTag::Relation { db: 5, rel: 42 },
            LockMode::RowExclusive
        ));
    }

    #[test]
    fn a_missing_relation_takes_no_lock() {
        let lm = setup(1);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        let got = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::AccessShare),
            &|| 0,
            &mut || Ok(None),
        )
        .unwrap();
        assert_eq!(got, None);
        assert!(lm.held_locks(BackendId(1)).is_empty());
    }

    #[test]
    fn a_changed_generation_resolves_again_and_releases_a_stale_lock() {
        let lm = setup(1);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        // 世代は 1 回目の解決の後で変わる（待っている間に DDL がコミットされた）。
        let gen_calls = Cell::new(0u64);
        let resolves = Cell::new(0);
        let got = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::AccessShare),
            &|| {
                // g0（1 回目）、取った直後の比較（2 回目）で値が変わる。以降は安定。
                let n = gen_calls.get();
                gen_calls.set(n + 1);
                if n == 0 { 1 } else { 2 }
            },
            &mut || {
                let n = resolves.get();
                resolves.set(n + 1);
                // 1 回目は旧い OID 10、再解決は新しい OID 11（RENAME / DROP & CREATE された）。
                Ok(Some((5, if n == 0 { 10 } else { 11 })))
            },
        )
        .unwrap();
        assert_eq!(got, Some((5, 11)));
        assert!(!lm.holds(
            BackendId(1),
            LockTag::Relation { db: 5, rel: 10 },
            LockMode::AccessShare
        ));
        assert!(lm.holds(
            BackendId(1),
            LockTag::Relation { db: 5, rel: 11 },
            LockMode::AccessShare
        ));
    }

    #[test]
    fn the_same_oid_after_a_generation_change_is_kept() {
        let lm = setup(1);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        let n = Cell::new(0u64);
        let got = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::Share),
            &|| {
                n.set(n.get() + 1);
                n.get()
            },
            &mut || Ok(Some((5, 3))),
        )
        .unwrap();
        assert_eq!(got, Some((5, 3)));
        assert!(lm.holds(
            BackendId(1),
            LockTag::Relation { db: 5, rel: 3 },
            LockMode::Share
        ));
    }

    #[test]
    fn nowait_reports_the_written_name_with_55p03() {
        let lm = setup(2);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        lm.acquire(
            BackendId(2),
            LockTag::Relation { db: 5, rel: 9 },
            LockMode::AccessExclusive,
            LockScope::Transaction,
            &w,
        )
        .unwrap();
        let o = RelLockOpts {
            mode: LockMode::AccessShare,
            nowait_label: Some("public.a"),
        };
        let e =
            lock_relation(&lm, BackendId(1), &w, &o, &|| 0, &mut || Ok(Some((5, 9)))).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
        assert_eq!(e.message, "could not obtain lock on relation \"public.a\"");
    }

    #[test]
    fn a_blocked_wait_ends_with_the_lock_timeout() {
        let lm = setup(2);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        lm.acquire(
            BackendId(2),
            LockTag::Relation { db: 5, rel: 9 },
            LockMode::AccessExclusive,
            LockScope::Transaction,
            &w,
        )
        .unwrap();
        let e = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::AccessShare),
            &|| 0,
            &mut || Ok(Some((5, 9))),
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
    }

    #[test]
    fn a_pending_cancel_stops_the_loop_before_resolving() {
        let lm = setup(1);
        let flag = InterruptFlag::default();
        flag.request_cancel();
        let w = ctl(&flag);
        let called = Cell::new(false);
        let e = lock_relation(
            &lm,
            BackendId(1),
            &w,
            &opts(LockMode::AccessShare),
            &|| 0,
            &mut || {
                called.set(true);
                Ok(Some((5, 1)))
            },
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(!called.get());
    }

    #[test]
    fn name_locks_serialize_the_same_name_only() {
        let lm = setup(2);
        let flag = InterruptFlag::default();
        let w = ctl(&flag);
        lock_name(&lm, BackendId(1), &w, 5, 2200, "t").unwrap();
        // 別の名前は待たない。
        lock_name(&lm, BackendId(2), &w, 5, 2200, "u").unwrap();
        // 同じ名前は待ち、lock_timeout で 55P03。
        let e = lock_name(&lm, BackendId(2), &w, 5, 2200, "t").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::LOCK_NOT_AVAILABLE);
        // 解放すれば取れる。
        lm.release_all(BackendId(1), LockScope::Transaction);
        lock_name(&lm, BackendId(2), &w, 5, 2200, "t").unwrap();
    }
}
