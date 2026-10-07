//! `CatalogCache`: per-database shared cache of `TableDef`s (`m2.md` §4.6,
//! §6.8.6).
//!
//! The cache holds only committed definitions. A reader remembers the
//! generation it started with and inserts a definition it built only if the
//! generation is unchanged; a commit that changed the catalog calls
//! `invalidate_all` (clear, then bump) after the commit is visible, so a
//! definition built from the old catalog can never be inserted after the
//! new one became current.
//!
//! The accessors cannot return an error (their signatures are fixed), so a
//! poisoned lock is read through: a poisoned lock means a thread panicked,
//! and the cluster is stopped by the server's poison check
//! (`m2.md` §6.3.4) rather than by this cache.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{RelKind, TableDef};
use crate::types::Oid;

#[derive(Debug, Default)]
struct CacheInner {
    by_oid: HashMap<Oid, Arc<TableDef>>,
    /// namespace OID -> name -> (OID, 種別)。表・シーケンス・索引（M2 の `by_name` は表だけだった）。
    by_name: HashMap<Oid, HashMap<String, (Oid, RelKind)>>,
    /// 索引の OID -> 表の OID。
    index_owner: HashMap<Oid, Oid>,
    generation: u64,
}

/// `generation` lives in the same `RwLock` as the maps: `insert` (compare
/// generation, then insert) and `invalidate_all` (clear, then bump) both run
/// under the write lock.
#[derive(Debug, Default)]
pub struct CatalogCache {
    inner: RwLock<CacheInner>,
}

impl CatalogCache {
    fn read(&self) -> RwLockReadGuard<'_, CacheInner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, CacheInner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current generation. A statement reads it before it takes its
    /// snapshot.
    pub fn generation(&self) -> u64 {
        self.read().generation
    }

    pub fn get_by_oid(&self, oid: Oid) -> Option<Arc<TableDef>> {
        self.read().by_oid.get(&oid).cloned()
    }

    /// The `(OID, kind)` of the table / sequence / index `name` in namespace `nsp`, if it is cached.
    /// A miss says nothing about whether the relation exists.
    pub fn get_relation_by_name(&self, nsp: Oid, name: &str) -> Option<(Oid, RelKind)> {
        self.read().by_name.get(&nsp)?.get(name).copied()
    }

    /// 索引の OID から、その表の OID。
    pub fn get_index_owner(&self, index_oid: Oid) -> Option<Oid> {
        self.read().index_owner.get(&index_oid).copied()
    }

    /// Inserts only if `built_at_gen` equals the current generation. `def` goes into `by_oid`; `def` and
    /// every index of `def` go into `by_name` and `index_owner`.
    pub fn insert(&self, def: Arc<TableDef>, built_at_gen: u64) {
        let mut g = self.write();
        if g.generation != built_at_gen {
            return;
        }
        g.by_name
            .entry(def.namespace)
            .or_default()
            .insert(def.name.clone(), (def.oid, def.kind));
        for i in &def.indexes {
            g.by_name
                .entry(i.namespace)
                .or_default()
                .insert(i.name.clone(), (i.oid, RelKind::Index));
            g.index_owner.insert(i.oid, def.oid);
        }
        g.by_oid.insert(def.oid, def);
    }

    /// Drops every entry and bumps the generation.
    pub fn invalidate_all(&self) {
        let mut g = self.write();
        g.by_oid.clear();
        g.by_name.clear();
        g.index_owner.clear();
        g.generation += 1;
    }

    /// Number of cached definitions.
    pub fn len(&self) -> usize {
        self.read().by_oid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;

    fn def(oid: Oid, name: &str) -> Arc<TableDef> {
        Arc::new(table_def(oid, name, vec![], vec![]))
    }

    #[test]
    fn insert_and_lookup() {
        let c = CatalogCache::default();
        assert_eq!(c.generation(), 0);
        assert!(c.is_empty());
        assert!(c.get_by_oid(16384).is_none());
        c.insert(def(16384, "t"), 0);
        assert_eq!(c.get_by_oid(16384).unwrap().name, "t");
        assert_eq!(
            c.get_relation_by_name(2200, "t"),
            Some((16384, RelKind::Table))
        );
        assert_eq!(c.get_relation_by_name(2200, "u"), None);
        assert_eq!(c.get_relation_by_name(11, "t"), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn stale_generation_is_not_inserted() {
        let c = CatalogCache::default();
        let started_at = c.generation();
        // A commit invalidates while the reader is still building.
        c.invalidate_all();
        assert_eq!(c.generation(), 1);
        c.insert(def(16384, "t"), started_at);
        assert!(c.get_by_oid(16384).is_none());
        assert!(c.is_empty());
        c.insert(def(16384, "t"), c.generation());
        assert!(c.get_by_oid(16384).is_some());
    }

    #[test]
    fn invalidate_all_clears_and_bumps() {
        let c = CatalogCache::default();
        c.insert(def(16384, "a"), 0);
        c.insert(def(16385, "b"), 0);
        c.invalidate_all();
        assert!(c.is_empty());
        assert_eq!(c.get_relation_by_name(2200, "a"), None);
        assert_eq!(c.generation(), 1);
        c.invalidate_all();
        assert_eq!(c.generation(), 2);
    }

    #[test]
    fn insert_replaces_an_entry_of_the_same_oid() {
        let c = CatalogCache::default();
        c.insert(def(16384, "a"), 0);
        c.insert(def(16384, "a"), 0);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn concurrent_inserts_and_invalidations_stay_consistent() {
        let c = Arc::new(CatalogCache::default());
        let writers: Vec<_> = (0..4u32)
            .map(|i| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for n in 0..200u32 {
                        let gen_before = c.generation();
                        let oid = 16384 + i * 1000 + n;
                        c.insert(def(oid, &format!("t{oid}")), gen_before);
                        if n % 50 == 0 {
                            c.invalidate_all();
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        // Whatever survived is consistent between the two maps.
        for oid in 16384..16384 + 4000 {
            if let Some(d) = c.get_by_oid(oid) {
                assert_eq!(
                    c.get_relation_by_name(d.namespace, &d.name).map(|x| x.0),
                    Some(oid)
                );
            }
        }
        assert!(c.generation() >= 16);
    }

    #[test]
    fn poisoned_lock_is_read_through() {
        let c = Arc::new(CatalogCache::default());
        c.insert(def(16384, "t"), 0);
        let c2 = Arc::clone(&c);
        let _ = std::thread::spawn(move || {
            let _g = c2.inner.write().unwrap();
            panic!("poison");
        })
        .join();
        assert!(c.get_by_oid(16384).is_some());
        assert_eq!(c.generation(), 0);
    }
}
