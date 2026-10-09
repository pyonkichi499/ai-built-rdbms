//! `BackendId`・`BackendRegistry`・`BackendGuard`（M5 LK。契約 §4.1、01 章 §4.5）。
//!
//! 型だけを持つ。`LockManager` への登録とセッションスコープのロックの解放は
//! 09 章の `SessionLocks` が行い、`BackendGuard` の Drop は登録簿から外すだけ（LK-D21）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::interrupt::InterruptFlag;
use crate::types::Oid;
use crate::util::sync::{lock, lock_ignore_poison};

/// セッションごとの ID（`Cluster::next_session_id()` の値）。ロックの持ち主を表す。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BackendId(pub u64);

/// 登録簿の 1 行。
#[derive(Clone, Debug)]
pub struct BackendInfo {
    pub id: BackendId,
    /// `BackendKeyData` の pid。
    pub pid: i32,
    pub db_oid: Oid,
    pub user_oid: Oid,
    pub client_addr: Option<IpAddr>,
    pub interrupts: Arc<InterruptFlag>,
}

/// 接続中のバックエンドの登録簿。`LockManager` は知らない。
#[derive(Debug, Default)]
pub struct BackendRegistry {
    backends: Mutex<HashMap<BackendId, BackendInfo>>,
}

impl BackendRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(BackendRegistry::default())
    }

    /// 登録する。返したガードの Drop で外れる。
    pub fn register(self: &Arc<Self>, info: BackendInfo) -> BackendGuard {
        let guard = BackendGuard {
            registry: Arc::clone(self),
            info: info.clone(),
        };
        lock_ignore_poison(&self.backends).insert(info.id, info);
        guard
    }

    /// 接続数の検査と登録を 1 つの `Mutex` の中で行う。`admit` が `Err` なら登録しない。
    pub fn register_if(
        self: &Arc<Self>,
        info: BackendInfo,
        admit: &dyn Fn(&[BackendInfo]) -> Result<()>,
    ) -> Result<BackendGuard> {
        let mut map = lock(&self.backends)?;
        let current = sorted(&map);
        admit(&current)?;
        let guard = BackendGuard {
            registry: Arc::clone(self),
            info: info.clone(),
        };
        map.insert(info.id, info);
        Ok(guard)
    }

    pub fn count_in_db(&self, db: Oid) -> usize {
        lock_ignore_poison(&self.backends)
            .values()
            .filter(|b| b.db_oid == db)
            .count()
    }

    pub fn count_of_role(&self, role: Oid) -> usize {
        lock_ignore_poison(&self.backends)
            .values()
            .filter(|b| b.user_oid == role)
            .count()
    }

    pub fn by_pid(&self, pid: i32) -> Option<BackendInfo> {
        lock_ignore_poison(&self.backends)
            .values()
            .find(|b| b.pid == pid)
            .cloned()
    }

    pub fn by_id(&self, id: BackendId) -> Option<BackendInfo> {
        lock_ignore_poison(&self.backends).get(&id).cloned()
    }

    /// `BackendId` の昇順。
    pub fn all(&self) -> Vec<BackendInfo> {
        sorted(&lock_ignore_poison(&self.backends))
    }
}

fn sorted(map: &HashMap<BackendId, BackendInfo>) -> Vec<BackendInfo> {
    let mut v: Vec<BackendInfo> = map.values().cloned().collect();
    v.sort_by_key(|b| b.id);
    v
}

/// 登録の持ち主。Drop で登録簿から外れる（`LockManager` には触れない）。
#[derive(Debug)]
pub struct BackendGuard {
    registry: Arc<BackendRegistry>,
    info: BackendInfo,
}

impl BackendGuard {
    pub fn id(&self) -> BackendId {
        self.info.id
    }

    pub fn info(&self) -> BackendInfo {
        self.info.clone()
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        lock_ignore_poison(&self.registry.backends).remove(&self.info.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, sqlstate};

    fn info(id: u64, pid: i32, db: Oid, user: Oid) -> BackendInfo {
        BackendInfo {
            id: BackendId(id),
            pid,
            db_oid: db,
            user_oid: user,
            client_addr: None,
            interrupts: Arc::new(InterruptFlag::default()),
        }
    }

    #[test]
    fn register_lookup_and_drop() {
        let reg = BackendRegistry::new();
        let g1 = reg.register(info(1, 100, 5, 10));
        let g2 = reg.register(info(2, 200, 5, 11));
        let g3 = reg.register(info(3, 300, 6, 10));
        assert_eq!(g1.id(), BackendId(1));
        assert_eq!(g2.info().pid, 200);
        assert_eq!(reg.count_in_db(5), 2);
        assert_eq!(reg.count_in_db(6), 1);
        assert_eq!(reg.count_of_role(10), 2);
        assert_eq!(reg.by_pid(300).unwrap().id, BackendId(3));
        assert!(reg.by_pid(999).is_none());
        assert_eq!(reg.by_id(BackendId(2)).unwrap().pid, 200);
        let ids: Vec<_> = reg.all().iter().map(|b| b.id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);

        drop(g2);
        assert!(reg.by_id(BackendId(2)).is_none());
        assert_eq!(reg.count_in_db(5), 1);
        drop(g1);
        drop(g3);
        assert!(reg.all().is_empty());
    }

    #[test]
    fn register_if_checks_and_registers_atomically() {
        let reg = BackendRegistry::new();
        let admit = |cur: &[BackendInfo]| -> Result<()> {
            if cur.len() >= 2 {
                Err(Error::new(sqlstate::TOO_MANY_CONNECTIONS, "too many"))
            } else {
                Ok(())
            }
        };
        let _g1 = reg.register_if(info(1, 1, 5, 10), &admit).unwrap();
        let _g2 = reg.register_if(info(2, 2, 5, 10), &admit).unwrap();
        let err = reg.register_if(info(3, 3, 5, 10), &admit).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::TOO_MANY_CONNECTIONS);
        assert_eq!(reg.all().len(), 2);
    }
}
