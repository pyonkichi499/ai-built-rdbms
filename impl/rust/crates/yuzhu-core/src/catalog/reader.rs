//! `StatementCatalog`: the per-statement `CatalogReader` (`m2.md` §4.6,
//! §6.8.6).
//!
//! Name resolution: an unqualified name finds the 13 system catalogs first
//! (`pg_catalog` is implicitly first in the search path, and their
//! definitions are pinned), then each `search_path` namespace in order. A
//! qualified `pg_catalog.x` finds only the system catalogs. User tables come
//! from the per-database [`CatalogCache`](super::cache::CatalogCache) or, on
//! a miss, are built from the catalog tables with the statement's snapshot.
//! A transaction that changed the catalog (`bypass_cache`) never uses or
//! fills the shared cache: it must see its own uncommitted changes, and the
//! cache holds committed definitions only.

use std::sync::Arc;

use super::schema::{self, oids};
use super::{CatalogReader, TableDef};
use crate::engine::DatabaseHandle;
use crate::error::Result;
use crate::txn::Snapshot;
use crate::types::Oid;

pub struct StatementCatalog<'a> {
    pub db: &'a DatabaseHandle,
    pub snapshot: &'a Snapshot,
    pub gen_at_snapshot: u64,
    /// The transaction changed the catalog: skip the shared cache.
    pub bypass_cache: bool,
    pub search_path: &'a [String],
}

impl std::fmt::Debug for StatementCatalog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatementCatalog")
            .field("db", &self.db.name)
            .field("gen_at_snapshot", &self.gen_at_snapshot)
            .field("bypass_cache", &self.bypass_cache)
            .field("search_path", &self.search_path)
            .finish_non_exhaustive()
    }
}

impl StatementCatalog<'_> {
    /// The pinned definition of the system catalog named `name`.
    fn nailed_by_name(&self, name: &str) -> Option<Arc<TableDef>> {
        let def = schema::catalog_by_name(name)?;
        self.db.catalog.nailed_def(def.oid)
    }

    /// The table `name` in namespace `nsp`, through the cache.
    fn in_namespace(&self, nsp: Oid, name: &str) -> Result<Option<Arc<TableDef>>> {
        if !self.bypass_cache
            && let Some(oid) = self.db.cache.get_by_name(nsp, name)
            && let Some(def) = self.db.cache.get_by_oid(oid)
        {
            return Ok(Some(def));
        }
        match self.db.catalog.lookup_relation(self.snapshot, nsp, name)? {
            Some(oid) => self.load(oid),
            None => Ok(None),
        }
    }

    /// The definition of a relation by OID, through the cache.
    fn load(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
        if schema::catalog_def(oid).is_some() {
            return Ok(self.db.catalog.nailed_def(oid));
        }
        if !self.bypass_cache
            && let Some(def) = self.db.cache.get_by_oid(oid)
        {
            return Ok(Some(def));
        }
        let Some(def) = self.db.catalog.load_table_def(self.snapshot, oid)? else {
            // Absence is never cached.
            return Ok(None);
        };
        let def = Arc::new(def);
        if !self.bypass_cache {
            self.db.cache.insert(Arc::clone(&def), self.gen_at_snapshot);
        }
        Ok(Some(def))
    }

    fn namespace_by_name(&self, name: &str) -> Result<Option<Oid>> {
        if name == "pg_catalog" {
            return Ok(Some(oids::NAMESPACE_PG_CATALOG));
        }
        self.db.catalog.namespace_oid(self.snapshot, name)
    }
}

impl CatalogReader for StatementCatalog<'_> {
    fn table(&self, schema_name: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>> {
        match schema_name {
            Some("pg_catalog") => Ok(self.nailed_by_name(name)),
            Some(schema_name) => match self.namespace_by_name(schema_name)? {
                Some(nsp) => self.in_namespace(nsp, name),
                None => Ok(None),
            },
            None => {
                if let Some(def) = self.nailed_by_name(name) {
                    return Ok(Some(def));
                }
                for entry in self.search_path {
                    if entry == "pg_catalog" {
                        continue;
                    }
                    if let Some(nsp) = self.namespace_by_name(entry)?
                        && let Some(def) = self.in_namespace(nsp, name)?
                    {
                        return Ok(Some(def));
                    }
                }
                Ok(None)
            }
        }
    }

    fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
        self.load(oid)
    }

    fn current_database(&self) -> &str {
        &self.db.name
    }

    fn search_path(&self) -> &[String] {
        self.search_path
    }

    fn role_name(&self, oid: Oid) -> Result<Option<String>> {
        self.db.shared.role_name(self.snapshot, oid)
    }

    fn visible_namespaces(&self) -> Result<Vec<Oid>> {
        let mut out = vec![oids::NAMESPACE_PG_CATALOG];
        for entry in self.search_path {
            if let Some(nsp) = self.namespace_by_name(entry)?
                && !out.contains(&nsp)
            {
                out.push(nsp);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::cache::CatalogCache;
    use crate::catalog::rows::InitParams;
    use crate::catalog::store::fake_store::FakeStore;
    use crate::catalog::store::{CatalogStore, NewTable, SharedCatalogStore};
    use crate::catalog::{BoundExprSource, CheckDef, ColumnDef};
    use crate::control::{ControlData, ControlFileHandle};
    use crate::datadir::OidAllocator;
    use crate::storage::vfs::{SimVfs, Vfs};
    use crate::storage::{TableStore, WriteCtx};
    use crate::txn::Xid;
    use crate::types::SqlType;

    const DB: Oid = 5;

    fn snap() -> Snapshot {
        Snapshot {
            xmin: Xid(3),
            xmax: Xid(1000),
            xip: vec![],
            curcid: 100,
            own_xid: None,
        }
    }

    struct Env {
        db: DatabaseHandle,
        alloc: OidAllocator,
        w: WriteCtx,
    }

    impl Env {
        fn new() -> Env {
            let fake = FakeStore::new();
            let storage: Arc<dyn TableStore> = fake.clone();
            let w = WriteCtx {
                xid: Xid::BOOTSTRAP,
                cid: 0,
            };
            let params = InitParams {
                superuser: "postgres".into(),
            };
            let shared = Arc::new(SharedCatalogStore::new(Arc::clone(&storage)));
            shared.bootstrap(&w, &params).unwrap();
            let catalog = CatalogStore::new(DB, storage);
            catalog.bootstrap(&w, &params).unwrap();
            let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
            let control = Arc::new(
                ControlFileHandle::create(&vfs, &ControlData::initial(7, 9, 131_072)).unwrap(),
            );
            Env {
                db: DatabaseHandle {
                    oid: DB,
                    name: "postgres".into(),
                    catalog,
                    shared,
                    cache: CatalogCache::default(),
                },
                alloc: OidAllocator::new(control),
                w,
            }
        }

        fn create(&self, name: &str) -> Oid {
            let oid = self.db.catalog.get_new_relation_oid(&self.alloc).unwrap();
            let columns = vec![ColumnDef {
                name: "a".into(),
                attnum: 1,
                ty: SqlType::INT4,
                not_null: false,
                default: Some(BoundExprSource {
                    expr_sql: "1".into(),
                }),
            }];
            let checks = vec![CheckDef {
                name: format!("{name}_a_check"),
                expr_sql: "a > 0".into(),
            }];
            let (attrdef_oids, constraint_oids) = self
                .db
                .catalog
                .allocate_child_oids(&self.alloc, &columns, &checks)
                .unwrap();
            self.db
                .catalog
                .create_table(
                    &self.w,
                    &snap(),
                    &NewTable {
                        oid,
                        namespace: 2200,
                        name: name.into(),
                        owner: 10,
                        columns,
                        checks,
                        attrdef_oids,
                        constraint_oids,
                    },
                )
                .unwrap();
            oid
        }
    }

    fn path() -> Vec<String> {
        vec!["$user".into(), "public".into()]
    }

    fn stmt<'a>(env: &'a Env, s: &'a Snapshot, path: &'a [String]) -> StatementCatalog<'a> {
        StatementCatalog {
            db: &env.db,
            snapshot: s,
            gen_at_snapshot: env.db.cache.generation(),
            bypass_cache: false,
            search_path: path,
        }
    }

    #[test]
    fn system_catalogs_are_pinned() {
        let env = Env::new();
        let (s, p) = (snap(), path());
        let cat = stmt(&env, &s, &p);
        let t = cat.table(None, "pg_class").unwrap().unwrap();
        assert_eq!(t.oid, 1259);
        assert_eq!(t.schema, "pg_catalog");
        assert_eq!(
            cat.table(Some("pg_catalog"), "pg_type")
                .unwrap()
                .unwrap()
                .oid,
            1247
        );
        assert!(cat.table(Some("public"), "pg_class").unwrap().is_none());
        assert!(cat.table(Some("pg_catalog"), "nope").unwrap().is_none());
        assert_eq!(cat.table_by_oid(1262).unwrap().unwrap().name, "pg_database");
        // Shared catalogs are found through the same path.
        assert_eq!(
            cat.table(None, "pg_authid")
                .unwrap()
                .unwrap()
                .locator
                .db_oid,
            0
        );
        assert!(env.db.cache.is_empty(), "pinned definitions are not cached");
    }

    #[test]
    fn user_tables_are_found_and_cached() {
        let env = Env::new();
        let oid = env.create("t");
        let (s, p) = (snap(), path());
        let cat = stmt(&env, &s, &p);
        assert!(env.db.cache.is_empty());
        let t = cat.table(None, "t").unwrap().unwrap();
        assert_eq!(t.oid, oid);
        assert_eq!(t.columns[0].default.as_ref().unwrap().expr_sql, "1");
        assert_eq!(t.checks[0].name, "t_a_check");
        assert_eq!(env.db.cache.len(), 1);
        // Served from the cache the second time: the same Arc.
        let again = cat.table(None, "t").unwrap().unwrap();
        assert!(Arc::ptr_eq(&t, &again));
        let by_oid = cat.table_by_oid(oid).unwrap().unwrap();
        assert!(Arc::ptr_eq(&t, &by_oid));
        assert!(Arc::ptr_eq(
            &t,
            &cat.table(Some("public"), "t").unwrap().unwrap()
        ));
        assert!(cat.table(Some("pg_catalog"), "t").unwrap().is_none());
        assert!(cat.table(Some("nosuch"), "t").unwrap().is_none());
        // Absence is not cached.
        assert!(cat.table(None, "missing").unwrap().is_none());
        assert!(cat.table_by_oid(99_999).unwrap().is_none());
        assert_eq!(env.db.cache.len(), 1);
    }

    #[test]
    fn a_stale_generation_does_not_fill_the_cache() {
        let env = Env::new();
        env.create("t");
        let (s, p) = (snap(), path());
        let started = env.db.cache.generation();
        let cat = StatementCatalog {
            db: &env.db,
            snapshot: &s,
            gen_at_snapshot: started,
            bypass_cache: false,
            search_path: &p,
        };
        // A commit invalidates the cache after the statement began.
        env.db.cache.invalidate_all();
        assert!(cat.table(None, "t").unwrap().is_some());
        assert!(env.db.cache.is_empty());
    }

    #[test]
    fn bypass_reads_the_snapshot_and_leaves_the_cache_alone() {
        let env = Env::new();
        let oid = env.create("t");
        let (s, p) = (snap(), path());
        // Something stale in the cache is ignored when bypassing.
        let mut stale = (*stmt(&env, &s, &p).table(None, "t").unwrap().unwrap()).clone();
        stale.columns.clear();
        env.db
            .cache
            .insert(Arc::new(stale), env.db.cache.generation());
        let cat = StatementCatalog {
            db: &env.db,
            snapshot: &s,
            gen_at_snapshot: env.db.cache.generation(),
            bypass_cache: true,
            search_path: &p,
        };
        let t = cat.table(None, "t").unwrap().unwrap();
        assert_eq!(t.oid, oid);
        assert_eq!(t.columns.len(), 1);
        assert_eq!(env.db.cache.get_by_oid(oid).unwrap().columns.len(), 0);
        // Nothing new is cached either.
        env.db.cache.invalidate_all();
        assert!(cat.table(None, "t").unwrap().is_some());
        assert!(env.db.cache.is_empty());
    }

    #[test]
    fn search_path_order_and_missing_namespaces() {
        let env = Env::new();
        let oid = env.create("t");
        let s = snap();
        let only_missing = vec!["$user".to_string(), "nosuch".to_string()];
        assert!(
            stmt(&env, &s, &only_missing)
                .table(None, "t")
                .unwrap()
                .is_none()
        );
        let with_public = vec!["nosuch".to_string(), "public".to_string()];
        assert_eq!(
            stmt(&env, &s, &with_public)
                .table(None, "t")
                .unwrap()
                .unwrap()
                .oid,
            oid
        );
        // pg_catalog in the path changes nothing: it is always first.
        let explicit = vec!["public".to_string(), "pg_catalog".to_string()];
        assert_eq!(
            stmt(&env, &s, &explicit)
                .table(None, "pg_class")
                .unwrap()
                .unwrap()
                .oid,
            1259
        );
    }

    #[test]
    fn a_user_table_named_like_a_catalog_does_not_shadow_it() {
        let env = Env::new();
        let oid = env.create("pg_class");
        let (s, p) = (snap(), path());
        let cat = stmt(&env, &s, &p);
        assert_eq!(cat.table(None, "pg_class").unwrap().unwrap().oid, 1259);
        assert_eq!(
            cat.table(Some("public"), "pg_class").unwrap().unwrap().oid,
            oid
        );
    }

    #[test]
    fn dropped_tables_disappear_for_new_snapshots() {
        let env = Env::new();
        let oid = env.create("t");
        let (s, p) = (snap(), path());
        let def = stmt(&env, &s, &p).table(None, "t").unwrap().unwrap();
        env.db.catalog.drop_table(&env.w, &s, &def).unwrap();
        env.db.cache.invalidate_all();
        let cat = stmt(&env, &s, &p);
        assert!(cat.table(None, "t").unwrap().is_none());
        assert!(cat.table_by_oid(oid).unwrap().is_none());
    }

    #[test]
    fn roles_namespaces_and_session_info() {
        let env = Env::new();
        let (s, p) = (snap(), path());
        let cat = stmt(&env, &s, &p);
        assert_eq!(cat.role_name(10).unwrap().as_deref(), Some("postgres"));
        assert_eq!(
            cat.role_name(6171).unwrap().as_deref(),
            Some("pg_database_owner")
        );
        assert_eq!(cat.role_name(99_999).unwrap(), None);
        assert_eq!(cat.current_database(), "postgres");
        assert_eq!(cat.search_path(), p.as_slice());
        // "$user" does not exist as a namespace in M2.
        assert_eq!(cat.visible_namespaces().unwrap(), vec![11, 2200]);
        let both = vec![
            "public".to_string(),
            "pg_catalog".to_string(),
            "public".to_string(),
        ];
        assert_eq!(
            stmt(&env, &s, &both).visible_namespaces().unwrap(),
            vec![11, 2200]
        );
        assert!(format!("{cat:?}").contains("postgres"));
    }

    #[test]
    fn built_in_lookups_use_the_static_tables() {
        let env = Env::new();
        let (s, p) = (snap(), path());
        let cat = stmt(&env, &s, &p);
        assert_eq!(cat.type_by_name("int4").unwrap().oid, 23);
        assert_eq!(cat.type_by_oid(26).unwrap().name, "oid");
        assert!(cat.find_cast(23, 26).is_some());
        assert!(!cat.operators_named("=").is_empty());
        assert!(!cat.functions_named("pg_get_userbyid").is_empty());
    }
}
