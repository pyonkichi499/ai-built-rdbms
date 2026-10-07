//! `StatementCatalog`: the per-statement `CatalogReader` (`m2.md` §4.6,
//! §6.8.6; `m4/07-catalog-ddl.md` §4.2).
//!
//! Name resolution: an unqualified name finds the 22 system catalogs first
//! (`pg_catalog` is implicitly first in the search path, and their
//! definitions are pinned), then each `search_path` namespace in order. A
//! qualified `pg_catalog.x` finds only the system catalogs. Tables, sequences
//! and indexes share one namespace (`relation_in_namespace`). User relations
//! come from the per-database [`CatalogCache`](super::cache::CatalogCache) or,
//! on a miss, are built from the catalog tables with the statement's snapshot.
//! A transaction that changed the catalog (`bypass_cache`) never uses or
//! fills the shared cache: it must see its own uncommitted changes, and the
//! cache holds committed definitions only.

use std::sync::Arc;

use super::schema::{self, oids};
use super::{CatalogReader, ConstraintDef, IndexDef, RelKind, TableDef};
use crate::deparse::ident::{quote_identifier, quote_qualified};
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
        // `pg_roles` はビューの代わりに `pg_authid` の別名で見せる（列名は PG の `pg_roles` と同じ
        // `oid` / `rolname` / ... を持つ。`rolpassword` が `********` にならないのは既知の差）。
        let name = if name == "pg_roles" {
            "pg_authid"
        } else {
            name
        };
        let def = schema::catalog_by_name(name)?;
        self.db.catalog.nailed_def(def.oid)
    }

    /// 名前空間 `nsp` の中の、この名前のリレーション（表・索引・シーケンスのどれでも）。
    /// 存在しないことはキャッシュしない（M2 §6.8.6）。
    fn relation_in_namespace(&self, nsp: Oid, name: &str) -> Result<Option<(Oid, RelKind)>> {
        if !self.bypass_cache
            && let Some(found) = self.db.cache.get_relation_by_name(nsp, name)
        {
            return Ok(Some(found));
        }
        Ok(self
            .db
            .catalog
            .lookup_relation_row(self.snapshot, nsp, name)?
            .map(|r| (r.oid, r.kind)))
    }

    /// 修飾なしの名前: 22 個のシステムカタログ（釘付け）→ `search_path` の名前空間を順に。
    /// `pg_catalog.x` と修飾されたらシステムカタログだけ。
    fn resolve_relation(
        &self,
        schema_name: Option<&str>,
        name: &str,
    ) -> Result<Option<(Oid, RelKind)>> {
        match schema_name {
            Some("pg_catalog") => Ok(self.nailed_by_name(name).map(|d| (d.oid, RelKind::Table))),
            Some(schema_name) => match self.namespace_by_name(schema_name)? {
                Some(nsp) => self.relation_in_namespace(nsp, name),
                None => Ok(None),
            },
            None => {
                if let Some(def) = self.nailed_by_name(name) {
                    return Ok(Some((def.oid, RelKind::Table)));
                }
                for entry in self.search_path {
                    if entry == "pg_catalog" {
                        continue;
                    }
                    if let Some(nsp) = self.namespace_by_name(entry)?
                        && let Some(found) = self.relation_in_namespace(nsp, name)?
                    {
                        return Ok(Some(found));
                    }
                }
                Ok(None)
            }
        }
    }

    /// The definition of a relation by OID, through the cache. An index gives `None`.
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
        match self.resolve_relation(schema_name, name)? {
            Some((oid, RelKind::Table | RelKind::Sequence)) => self.load(oid),
            // 索引の名前は SELECT できるリレーションではない（D07-18）。
            Some((_, RelKind::Index)) | None => Ok(None),
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

    fn relation_kind(
        &self,
        schema_name: Option<&str>,
        name: &str,
    ) -> Result<Option<(Oid, RelKind)>> {
        self.resolve_relation(schema_name, name)
    }

    fn index_by_name(
        &self,
        schema_name: Option<&str>,
        name: &str,
    ) -> Result<Option<Arc<IndexDef>>> {
        match self.resolve_relation(schema_name, name)? {
            Some((oid, RelKind::Index)) => self.index_by_oid(oid),
            _ => Ok(None),
        }
    }

    fn index_by_oid(&self, oid: Oid) -> Result<Option<Arc<IndexDef>>> {
        let owner = if !self.bypass_cache
            && let Some(t) = self.db.cache.get_index_owner(oid)
        {
            Some(t)
        } else {
            // pg_class の relkind は見ずに pg_index だけを引く。
            self.db.catalog.index_owner(self.snapshot, oid)?
        };
        let Some(table_oid) = owner else {
            return Ok(None);
        };
        Ok(self
            .load(table_oid)?
            .and_then(|t| t.index_by_oid(oid).cloned()))
    }

    /// `regclassout`: 検索パスで見える名前空間なら修飾しない。
    fn relation_name(&self, oid: Oid) -> Result<Option<String>> {
        let (nsp, name) = if let Some(t) = self.table_by_oid(oid)? {
            (t.namespace, t.name.clone())
        } else if let Some(i) = self.index_by_oid(oid)? {
            (i.namespace, i.name.clone())
        } else {
            return Ok(None);
        };
        if self.visible_namespaces()?.contains(&nsp) {
            return Ok(Some(quote_identifier(&name)));
        }
        let schema_name = self
            .db
            .catalog
            .namespace_name(self.snapshot, nsp)?
            .unwrap_or_else(|| nsp.to_string());
        Ok(Some(quote_qualified(&schema_name, &name)))
    }

    fn constraint_by_oid(&self, oid: Oid) -> Result<Option<ConstraintDef>> {
        self.db.catalog.constraint_by_oid(self.snapshot, oid)
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
                identity: None,
            }];
            let checks = vec![CheckDef {
                name: format!("{name}_a_check"),
                expr_sql: "a > 0".into(),
                no_inherit: false,
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
                        indexes: vec![],
                        extra_depends: vec![],
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

#[cfg(test)]
mod tests_m4 {
    use super::*;
    use crate::catalog::cache::CatalogCache;
    use crate::catalog::rows::InitParams;
    use crate::catalog::store::fake_store::FakeStore;
    use crate::catalog::store::test_env::{pk_spec, snap, wctx, write_table};
    use crate::catalog::store::{CatalogStore, SharedCatalogStore};
    use crate::control::{ControlData, ControlFileHandle};
    use crate::datadir::OidAllocator;
    use crate::storage::TableStore;
    use crate::storage::vfs::{SimVfs, Vfs};

    struct Env {
        db: DatabaseHandle,
        oids: OidAllocator,
        fake: Arc<FakeStore>,
    }

    fn env() -> Env {
        let fake = FakeStore::new();
        let storage: Arc<dyn TableStore> = fake.clone();
        let params = InitParams {
            superuser: "postgres".into(),
        };
        let shared = Arc::new(SharedCatalogStore::new(Arc::clone(&storage)));
        shared.bootstrap(&wctx(), &params).unwrap();
        let catalog = CatalogStore::new(5, storage);
        catalog.bootstrap(&wctx(), &params).unwrap();
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let control = Arc::new(
            ControlFileHandle::create(&vfs, &ControlData::initial(7, 9, 131_072)).unwrap(),
        );
        Env {
            db: DatabaseHandle {
                oid: 5,
                name: "postgres".into(),
                catalog,
                shared,
                cache: CatalogCache::default(),
            },
            oids: OidAllocator::new(control),
            fake,
        }
    }

    fn stmt<'a>(
        env: &'a Env,
        s: &'a Snapshot,
        p: &'a [String],
        bypass: bool,
    ) -> StatementCatalog<'a> {
        StatementCatalog {
            db: &env.db,
            snapshot: s,
            gen_at_snapshot: env.db.cache.generation(),
            bypass_cache: bypass,
            search_path: p,
        }
    }

    /// `p(id int primary key, name text unique, n int)` を `crate::catalog::store::test_env` と同じ形で書く。
    fn make(e: &Env) -> crate::catalog::store::NewTable {
        use crate::catalog::store::test_env::Env as StoreEnv;
        // test_env の補助は自前の Env を取るので、同じ手順をここで行う。
        let _ = std::marker::PhantomData::<StoreEnv>;
        let spec = {
            let oids_ =
                e.db.catalog
                    .allocate_table_oids(
                        &e.oids,
                        &crate::catalog::store::TableOidRequest {
                            n_attrdefs: 0,
                            n_checks: 0,
                            n_index_constraints: 1,
                        },
                    )
                    .unwrap();
            let (index, con) = oids_.index_constraints[0];
            let mut t = crate::catalog::store::NewTable::plain(
                oids_.table,
                2200,
                "p",
                10,
                vec![crate::catalog::ColumnDef {
                    name: "id".into(),
                    attnum: 1,
                    ty: crate::types::SqlType::INT4,
                    not_null: true,
                    default: None,
                    identity: None,
                }],
            );
            t.indexes.push(crate::catalog::store::NewIndex {
                oid: index,
                name: "p_pkey".into(),
                namespace: 2200,
                owner: 10,
                table_oid: oids_.table,
                relfilenode: index,
                columns: vec![crate::catalog::store::NewIndexColumn {
                    name: "id".into(),
                    column: crate::catalog::IndexColumn {
                        attnum: 1,
                        opclass: 1978,
                        opfamily: 1976,
                        descending: false,
                        nulls_first: false,
                    },
                    ty: crate::types::SqlType::INT4,
                }],
                unique: true,
                primary: true,
                constraint: Some(crate::catalog::store::NewConstraint {
                    oid: con,
                    name: "p_pkey".into(),
                }),
                stats: crate::catalog::store::EMPTY_INDEX_STATS,
            });
            t
        };
        for l in std::iter::once(spec.oid).chain(spec.indexes.iter().map(|i| i.relfilenode)) {
            e.fake
                .create_storage(
                    &wctx(),
                    crate::storage::smgr::RelFileLocator {
                        spc_oid: 1663,
                        db_oid: 5,
                        rel_number: crate::storage::smgr::RelFileNumber(l),
                    },
                )
                .unwrap();
        }
        e.db.catalog.create_table(&wctx(), &snap(), &spec).unwrap();
        spec
    }

    #[test]
    fn indexes_are_found_by_name_and_oid_and_cached() {
        let e = env();
        let spec = make(&e);
        let (s, p) = (snap(), vec!["public".to_string()]);
        let cat = stmt(&e, &s, &p, false);
        let idx_oid = spec.indexes[0].oid;
        assert_eq!(
            cat.relation_kind(None, "p").unwrap(),
            Some((spec.oid, RelKind::Table))
        );
        assert_eq!(
            cat.relation_kind(None, "p_pkey").unwrap(),
            Some((idx_oid, RelKind::Index))
        );
        assert_eq!(
            cat.relation_kind(None, "pg_class").unwrap(),
            Some((1259, RelKind::Table))
        );
        assert_eq!(cat.relation_kind(Some("pg_catalog"), "p").unwrap(), None);
        assert_eq!(cat.relation_kind(None, "nope").unwrap(), None);
        // table() does not return an index
        assert!(cat.table(None, "p_pkey").unwrap().is_none());
        assert!(cat.table_by_oid(idx_oid).unwrap().is_none());
        // the index by name, loaded through its table, then cached
        assert!(e.db.cache.is_empty());
        let i = cat.index_by_name(None, "p_pkey").unwrap().unwrap();
        assert_eq!(i.oid, idx_oid);
        assert_eq!(e.db.cache.get_index_owner(idx_oid), Some(spec.oid));
        assert_eq!(
            e.db.cache.get_relation_by_name(2200, "p_pkey"),
            Some((idx_oid, RelKind::Index))
        );
        assert!(cat.index_by_name(None, "p").unwrap().is_none());
        // same answer with and without the cache
        let bypass = stmt(&e, &s, &p, true);
        assert_eq!(
            bypass.index_by_oid(idx_oid).unwrap().unwrap().name,
            "p_pkey"
        );
        assert!(bypass.index_by_oid(spec.oid).unwrap().is_none());
        assert!(cat.index_by_oid(99_999).unwrap().is_none());
        assert_eq!(
            cat.constraint_by_oid(i.constraint.as_ref().unwrap().oid)
                .unwrap()
                .unwrap()
                .name,
            "p_pkey"
        );
    }

    #[test]
    fn relation_names_are_qualified_outside_the_search_path() {
        let e = env();
        let spec = make(&e);
        let s = snap();
        let p = vec!["public".to_string()];
        assert_eq!(
            stmt(&e, &s, &p, false)
                .relation_name(spec.oid)
                .unwrap()
                .as_deref(),
            Some("p")
        );
        assert_eq!(
            stmt(&e, &s, &p, false)
                .relation_name(spec.indexes[0].oid)
                .unwrap()
                .as_deref(),
            Some("p_pkey")
        );
        let other = vec!["pg_catalog".to_string()];
        assert_eq!(
            stmt(&e, &s, &other, false)
                .relation_name(spec.oid)
                .unwrap()
                .as_deref(),
            Some("public.p")
        );
        assert_eq!(stmt(&e, &s, &p, false).relation_name(99_999).unwrap(), None);
    }

    #[test]
    fn the_cache_is_replaced_after_an_index_is_created() {
        let e = env();
        let spec = make(&e);
        let (s, p) = (snap(), vec!["public".to_string()]);
        let cat = stmt(&e, &s, &p, false);
        assert_eq!(cat.table(None, "p").unwrap().unwrap().indexes.len(), 1);
        // a committed DDL invalidates everything
        e.db.cache.invalidate_all();
        assert!(e.db.cache.get_relation_by_name(2200, "p_pkey").is_none());
        assert!(e.db.cache.get_index_owner(spec.indexes[0].oid).is_none());
    }

    #[test]
    fn pg_pkey_is_a_known_fixture() {
        let _ = pk_spec;
        let _ = write_table;
    }
}
