//! Test scaffolding: `TestCluster` runs initdb on a `SimVfs` and starts a
//! `Cluster` (`m2.md` §2). Always built (the server's integration tests use
//! it).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::bootstrap::{InitdbOptions, initdb};
use crate::engine::{Cluster, ClusterOptions};
use crate::error::Result;
use crate::session::{Session, StartupParams};
use crate::storage::DEFAULT_RELSEG_SIZE;
use crate::storage::vfs::{CrashMode, SimVfs, Vfs};

/// Settings of a [`TestCluster`].
#[derive(Debug, Clone)]
pub struct TestClusterOptions {
    pub seed: u64,
    pub superuser: String,
    pub nframes: usize,
    pub rel_seg_blocks: u32,
    pub checkpoint_timeout: Duration,
}

impl Default for TestClusterOptions {
    fn default() -> Self {
        TestClusterOptions {
            seed: 1,
            superuser: "postgres".into(),
            nframes: 256,
            rel_seg_blocks: DEFAULT_RELSEG_SIZE,
            // The checkpointer thread stays idle unless a test asks for it.
            checkpoint_timeout: Duration::from_secs(3600),
        }
    }
}

/// An initialized cluster on a simulated disk.
#[derive(Debug)]
pub struct TestCluster {
    /// The simulated disk (the same object the cluster uses).
    pub vfs: SimVfs,
    pub cluster: Arc<Cluster>,
    pub options: TestClusterOptions,
}

/// `ClusterOptions` for tests.
pub fn cluster_options(o: &TestClusterOptions, ignore_unclean_shutdown: bool) -> ClusterOptions {
    ClusterOptions {
        data_dir: PathBuf::from("/sim/data"),
        shared_buffers: o.nframes,
        max_connections: 16,
        checkpoint_timeout: o.checkpoint_timeout,
        ignore_unclean_shutdown,
    }
}

impl TestCluster {
    /// initdb (`-U postgres`) and start.
    pub fn new() -> TestCluster {
        TestCluster::with_options(TestClusterOptions::default())
            .expect("the default test cluster must start")
    }

    pub fn with_options(options: TestClusterOptions) -> Result<TestCluster> {
        let vfs = SimVfs::new(options.seed);
        initdb(
            Arc::new(vfs.clone()),
            &InitdbOptions {
                superuser: options.superuser.clone(),
                no_sync: false,
                rel_seg_blocks: options.rel_seg_blocks,
            },
        )?;
        TestCluster::start_on(vfs, options, false)
    }

    /// Starts a cluster on an existing (initialized) disk.
    pub fn start_on(
        vfs: SimVfs,
        options: TestClusterOptions,
        ignore_unclean_shutdown: bool,
    ) -> Result<TestCluster> {
        let dyn_vfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
        let cluster = Cluster::open(dyn_vfs, cluster_options(&options, ignore_unclean_shutdown))?;
        Ok(TestCluster {
            vfs,
            cluster,
            options,
        })
    }

    /// A session for `database` as the superuser.
    pub fn session(&self, database: &str) -> Result<Session> {
        self.session_as(database, &self.options.superuser)
    }

    pub fn session_as(&self, database: &str, user: &str) -> Result<Session> {
        Session::new(
            Arc::clone(&self.cluster),
            StartupParams {
                user: user.into(),
                database: database.into(),
                application_name: None,
                options: Vec::new(),
            },
        )
    }

    /// Clean stop, then start again on the same disk.
    pub fn restart(self) -> Result<TestCluster> {
        self.cluster.shutdown()?;
        let TestCluster {
            vfs,
            cluster,
            options,
        } = self;
        cluster.abandon();
        TestCluster::start_on(vfs, options, false)
    }

    /// Simulated crash (nothing is written), then start on what survived
    /// with `ignore_unclean_shutdown`.
    pub fn crash_and_restart(self, mode: CrashMode) -> Result<TestCluster> {
        let TestCluster {
            vfs,
            cluster,
            options,
        } = self;
        cluster.abandon();
        TestCluster::start_on(vfs.crash(mode), options, true)
    }

    /// Simulated crash; the caller starts the cluster itself.
    pub fn crash(self, mode: CrashMode) -> (SimVfs, TestClusterOptions) {
        let TestCluster {
            vfs,
            cluster,
            options,
        } = self;
        cluster.abandon();
        (vfs.crash(mode), options)
    }
}

impl Default for TestCluster {
    fn default() -> Self {
        TestCluster::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Severity, sqlstate};

    #[test]
    fn initdb_start_stop_restart() {
        let tc = TestCluster::new();
        for db in ["template1", "postgres"] {
            let (h, role) = tc.cluster.connect(db, "postgres").unwrap();
            assert_eq!(h.name, db);
            assert_eq!(role.name, "postgres");
        }
        let tc = tc.restart().unwrap();
        tc.cluster.connect("postgres", "postgres").unwrap();
        tc.cluster.shutdown().unwrap();
    }

    #[test]
    fn connect_errors() {
        let tc = TestCluster::new();
        let e = tc.cluster.connect("nope", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_CATALOG_NAME);
        assert_eq!(e.severity, Severity::Fatal);
        let e = tc.cluster.connect("template0", "postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        assert!(e.message.contains("not currently accepting connections"));
        let e = tc.cluster.connect("postgres", "nobody").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        let e = tc
            .cluster
            .connect("postgres", "pg_database_owner")
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_AUTHORIZATION_SPECIFICATION);
        assert!(e.message.contains("not permitted to log in"));
    }

    #[test]
    fn custom_superuser() {
        let tc = TestCluster::with_options(TestClusterOptions {
            superuser: "alice".into(),
            ..TestClusterOptions::default()
        })
        .unwrap();
        let (_, role) = tc.cluster.connect("postgres", "alice").unwrap();
        assert!(role.superuser);
        assert!(tc.cluster.connect("postgres", "postgres").is_err());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::catalog::schema::oids;
    use crate::catalog::store::NewTable;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::storage::{RelHandle, WriteCtx};
    use crate::types::{Datum, SqlType};

    fn create_t(tc: &TestCluster, commit: bool) -> u32 {
        let c = &tc.cluster;
        let (db, _) = c.connect("postgres", "postgres").unwrap();
        let t = c.txn_manager();
        let (xid, guard) = t.begin_write(1, None).unwrap();
        let w = WriteCtx { xid, cid: 0 };
        let snap = t.snapshot(Some(xid), 0);
        let oid = db.catalog.get_new_relation_oid(c.oid_allocator()).unwrap();
        let columns = vec![ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
        }];
        db.catalog
            .create_table(
                &w,
                &snap,
                &NewTable {
                    oid,
                    namespace: oids::NAMESPACE_PUBLIC,
                    name: "t".into(),
                    owner: oids::BOOTSTRAP_SUPERUSER,
                    columns,
                    checks: vec![],
                    attrdef_oids: vec![],
                    constraint_oids: vec![],
                },
            )
            .unwrap();
        let snap = t.snapshot(Some(xid), 1);
        let def: TableDef = db.catalog.load_table_def(&snap, oid).unwrap().unwrap();
        c.storage().create_storage(def.locator).unwrap();
        let rel = RelHandle::from_table(&def);
        c.storage()
            .insert(&rel, &WriteCtx { xid, cid: 1 }, &[Datum::Int4(42)])
            .unwrap();
        if commit {
            t.commit(xid).unwrap();
        } else {
            t.abort(xid).unwrap();
        }
        drop(guard);
        oid
    }

    fn read_t(tc: &TestCluster, oid: u32) -> Option<Vec<Datum>> {
        let c = &tc.cluster;
        let (db, _) = c.connect("postgres", "postgres").unwrap();
        let snap = c.txn_manager().snapshot(None, 0);
        let def = db.catalog.load_table_def(&snap, oid).unwrap()?;
        let rel = RelHandle::from_table(&def);
        let mut scan = c.storage().begin_scan(&rel, &snap).unwrap();
        let mut out = Vec::new();
        while let Some(t) = c.storage().scan_next(&mut scan).unwrap() {
            out.push(t.row[0].clone());
        }
        drop(scan);
        assert_eq!(c.stack().pool.pinned_frames(), 0);
        Some(out)
    }

    #[test]
    fn committed_table_survives_a_clean_restart() {
        let tc = TestCluster::new();
        let oid = create_t(&tc, true);
        assert_eq!(read_t(&tc, oid), Some(vec![Datum::Int4(42)]));
        let tc = tc.restart().unwrap();
        assert_eq!(read_t(&tc, oid), Some(vec![Datum::Int4(42)]));
    }

    #[test]
    fn aborted_table_is_gone_after_restart() {
        let tc = TestCluster::new();
        let oid = create_t(&tc, false);
        assert_eq!(read_t(&tc, oid), None);
        let tc = tc.restart().unwrap();
        assert_eq!(read_t(&tc, oid), None);
    }

    #[test]
    fn checkpointed_data_survives_a_crash() {
        let tc = TestCluster::new();
        let oid = create_t(&tc, true);
        tc.cluster.checkpoint().unwrap();
        let tc = tc.crash_and_restart(CrashMode::DropUnsynced).unwrap();
        assert_eq!(read_t(&tc, oid), Some(vec![Datum::Int4(42)]));
    }
}
