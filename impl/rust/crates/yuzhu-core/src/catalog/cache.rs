//! `CatalogCache`: per-database shared cache of `TableDef`s (`m2.md` §4.6,
//! §6.8.6).
//!
//! 担当 F が実装する。

#![allow(clippy::unimplemented)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::TableDef;
use crate::types::Oid;

#[derive(Debug, Default)]
#[allow(dead_code)]
struct CacheInner {
    by_oid: HashMap<Oid, Arc<TableDef>>,
    /// namespace OID -> name -> table OID.
    by_name: HashMap<Oid, HashMap<String, Oid>>,
    generation: u64,
}

/// `generation` lives in the same `RwLock` as the maps: `insert` (compare
/// generation, then insert) and `invalidate_all` (clear, then bump) both run
/// under the write lock.
#[derive(Debug, Default)]
pub struct CatalogCache {
    #[allow(dead_code)]
    inner: RwLock<CacheInner>,
}

impl CatalogCache {
    pub fn generation(&self) -> u64 {
        unimplemented!("担当 F が実装")
    }

    pub fn get_by_oid(&self, _oid: Oid) -> Option<Arc<TableDef>> {
        unimplemented!("担当 F が実装")
    }

    pub fn get_by_name(&self, _nsp: Oid, _name: &str) -> Option<Oid> {
        unimplemented!("担当 F が実装")
    }

    /// Inserts only if `built_at_gen` equals the current generation.
    pub fn insert(&self, _def: Arc<TableDef>, _built_at_gen: u64) {
        unimplemented!("担当 F が実装")
    }

    /// Drops every entry and bumps the generation.
    pub fn invalidate_all(&self) {
        unimplemented!("担当 F が実装")
    }
}
