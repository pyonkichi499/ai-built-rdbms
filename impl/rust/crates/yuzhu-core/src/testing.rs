//! Test scaffolding: `TestCluster` runs initdb on a `SimVfs` and starts a
//! `Cluster` (`m2.md` §2). Always built (the server's integration tests use
//! it).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::bootstrap::{InitdbOptions, initdb};
use crate::catalog::TableDef;
use crate::debug_knobs::DebugKnobs;
use crate::engine::{Cluster, ClusterOptions, DEFAULT_MAX_WAL_SIZE};
use crate::error::{Error, Result};
use crate::session::{ColumnDesc, Notice, ResultSink, Session, StartupParams};
use crate::storage::buffer::BufferPool;
use crate::storage::vfs::{CrashMode, SimVfs, Vfs};
use crate::storage::{DEFAULT_RELSEG_SIZE, IndexHandle};
use crate::types::Oid;
use crate::wal::{DEFAULT_WAL_SEGMENT_SIZE, MIN_WAL_SEGMENT_SIZE};

/// Settings of a [`TestCluster`].
#[derive(Debug, Clone)]
pub struct TestClusterOptions {
    pub seed: u64,
    pub superuser: String,
    pub nframes: usize,
    pub rel_seg_blocks: u32,
    pub checkpoint_timeout: Duration,
    /// WAL segment size in bytes (initdb).
    pub wal_segment_size: u32,
    /// Mutation-testing switches (`m3.md` §4.10).
    pub knobs: DebugKnobs,
}

impl TestClusterOptions {
    /// Settings of the layer-1 crash tests (`m3.md` §7.5): 16 buffer frames,
    /// 2 MiB WAL segments, the WAL-before-data assertion on.
    pub fn crash_sim(seed: u64) -> Self {
        TestClusterOptions {
            seed,
            nframes: 16,
            wal_segment_size: MIN_WAL_SEGMENT_SIZE,
            knobs: DebugKnobs {
                assert_wal_before_data: true,
                ..DebugKnobs::default()
            },
            ..TestClusterOptions::default()
        }
    }
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
            wal_segment_size: DEFAULT_WAL_SEGMENT_SIZE,
            knobs: DebugKnobs::default(),
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
pub fn cluster_options(o: &TestClusterOptions) -> ClusterOptions {
    ClusterOptions {
        data_dir: PathBuf::from("/sim/data"),
        shared_buffers: o.nframes,
        max_connections: 16,
        checkpoint_timeout: o.checkpoint_timeout,
        max_wal_size: DEFAULT_MAX_WAL_SIZE,
        // Tests call `Cluster::checkpoint` themselves.
        background_checkpointer: false,
        knobs: o.knobs,
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
                wal_segment_size: options.wal_segment_size,
            },
        )?;
        TestCluster::start_on(vfs, options)
    }

    /// Starts a cluster on an existing (initialized) disk.
    pub fn start_on(vfs: SimVfs, options: TestClusterOptions) -> Result<TestCluster> {
        let dyn_vfs: Arc<dyn Vfs> = Arc::new(vfs.clone());
        let cluster = Cluster::open(dyn_vfs, cluster_options(&options))?;
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
        TestCluster::start_on(vfs, options)
    }

    /// Simulated crash (nothing is written), then start on what survived
    /// (crash recovery).
    pub fn crash_and_restart(self, mode: CrashMode) -> Result<TestCluster> {
        let TestCluster {
            vfs,
            cluster,
            options,
        } = self;
        cluster.abandon();
        TestCluster::start_on(vfs.crash(mode), options)
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

/// Everything one Simple Query message produced (a `ResultSink` that keeps it).
#[derive(Debug, Default)]
pub struct QueryOutput {
    /// Rows of the last result set (text format, `None` = NULL).
    pub rows: Vec<Vec<Option<String>>>,
    /// The command tag of every statement that completed.
    pub tags: Vec<String>,
    /// Every error reported (a statement stops the message at its first).
    pub errors: Vec<Error>,
    pub notices: Vec<Notice>,
}

impl QueryOutput {
    /// The rows as strings (NULL as `"NULL"`).
    pub fn text_rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.clone().unwrap_or_else(|| "NULL".into()))
                    .collect()
            })
            .collect()
    }

    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl ResultSink for QueryOutput {
    fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
        self.rows.clear();
        Ok(())
    }
    fn data_row(&mut self, values: &[Option<String>]) -> std::io::Result<()> {
        self.rows.push(values.to_vec());
        Ok(())
    }
    fn command_complete(&mut self, tag: &str) -> std::io::Result<()> {
        self.tags.push(tag.to_owned());
        Ok(())
    }
    fn empty_query(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn error(&mut self, err: &Error) -> std::io::Result<()> {
        self.errors.push(err.clone());
        Ok(())
    }
    fn notice(&mut self, notice: &Notice) -> std::io::Result<()> {
        self.notices.push(notice.clone());
        Ok(())
    }
    fn parameter_status(&mut self, _: &str, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs one Simple Query message and collects the result.
pub fn run_sql(session: &mut Session, sql: &str) -> QueryOutput {
    let mut out = QueryOutput::default();
    // The sink never fails, so `execute_simple` cannot either.
    let _ = session.execute_simple(sql, &mut out);
    out
}

// ----- Helpers for the layer-1 crash tests (`m4/11` §3.6.3) ----------------------

impl TestClusterOptions {
    /// Sets the number of buffer frames (`shared_buffers`). The crash workloads pick their own
    /// (6 uses 24, 7 uses 16, 8 uses 24).
    #[must_use]
    pub fn shared_buffers(mut self, frames: usize) -> Self {
        self.nframes = frames;
        self
    }
}

/// The tables (`r`) and sequences (`S`) of `database` that live in `public` (namespace 2200),
/// in OID order, as the committed catalog shows them. A table's `indexes` are its indexes.
///
/// # Errors
///
/// The connection or a catalog read fails.
pub fn user_relation_defs(cluster: &Arc<Cluster>, database: &str) -> Result<Vec<TableDef>> {
    let (db, _) = cluster.connect(database, "postgres")?;
    let mut session = Session::new(
        Arc::clone(cluster),
        StartupParams {
            user: "postgres".into(),
            database: database.into(),
            application_name: None,
            options: Vec::new(),
        },
    )?;
    let out = run_sql(
        &mut session,
        "SELECT oid FROM pg_class WHERE relnamespace = 2200 ORDER BY oid",
    );
    if let Some(e) = out.errors.first() {
        return Err(e.clone());
    }
    let snap = cluster.txn_manager().snapshot(None, 0);
    let mut defs = Vec::new();
    for row in out.text_rows() {
        let oid: Oid = row[0]
            .parse()
            .map_err(|_| Error::internal(format!("pg_class.oid is not a number: {:?}", row[0])))?;
        if let Some(def) = db.catalog.load_table_def(&snap, oid)? {
            defs.push(def);
        }
    }
    Ok(defs)
}

/// Every index of the user tables of `database`, with the owning table's definition.
///
/// # Errors
///
/// See [`user_relation_defs`].
pub fn user_index_handles(
    cluster: &Arc<Cluster>,
    database: &str,
) -> Result<Vec<(IndexHandle, TableDef)>> {
    let mut out = Vec::new();
    for def in user_relation_defs(cluster, database)? {
        for idx in &def.indexes {
            out.push((IndexHandle::from_def(idx, &def), def.clone()));
        }
    }
    Ok(out)
}

impl TestCluster {
    /// The buffer pool (the structure checkers take `&Arc<BufferPool>`).
    pub fn pool(&self) -> &Arc<BufferPool> {
        &self.cluster.stack().pool
    }

    /// Buffers still pinned. Zero when no statement is running.
    pub fn pinned_frames(&self) -> usize {
        self.cluster.stack().pool.pinned_frames()
    }

    /// See [`user_relation_defs`].
    ///
    /// # Errors
    ///
    /// See [`user_relation_defs`].
    pub fn relation_defs(&self, database: &str) -> Result<Vec<TableDef>> {
        user_relation_defs(&self.cluster, database)
    }

    /// The handle of the index called `name` in `public`, looked up in the catalog.
    ///
    /// # Errors
    ///
    /// See [`user_relation_defs`].
    pub fn index_handle(&self, database: &str, name: &str) -> Result<Option<IndexHandle>> {
        Ok(user_index_handles(&self.cluster, database)?
            .into_iter()
            .map(|(h, _)| h)
            .find(|h| h.name == name))
    }

    /// Opens a session on `database` and runs one Simple Query message.
    ///
    /// # Errors
    ///
    /// The session cannot start. Errors of the statements are in the output.
    pub fn sql(&self, database: &str, sql: &str) -> Result<QueryOutput> {
        let mut s = self.session(database)?;
        Ok(run_sql(&mut s, sql))
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
        let irq = crate::InterruptFlag::default();
        let wait = crate::txn::WaitCtl {
            lock_timeout: None,
            interrupts: &irq,
        };
        let (xid, guard) = t.begin_write(1, &wait).unwrap();
        let w = WriteCtx { xid, cid: 0 };
        let snap = t.snapshot(Some(xid), 0);
        let oid = db.catalog.get_new_relation_oid(c.oid_allocator()).unwrap();
        let columns = vec![ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
            identity: None,
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
                    indexes: vec![],
                    extra_depends: vec![],
                },
            )
            .unwrap();
        let snap = t.snapshot(Some(xid), 1);
        let def: TableDef = db.catalog.load_table_def(&snap, oid).unwrap().unwrap();
        c.storage()
            .create_storage(&WriteCtx { xid, cid: 1 }, def.locator)
            .unwrap();
        let rel = RelHandle::from_table(&def);
        c.storage()
            .insert(&rel, &WriteCtx { xid, cid: 1 }, &[Datum::Int4(42)])
            .unwrap();
        if commit {
            t.commit(xid, &[]).unwrap();
        } else {
            t.abort(xid, &[]).unwrap();
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

#[cfg(test)]
mod sql_helper_tests {
    use super::*;

    #[test]
    fn run_sql_collects_rows_tags_and_errors() {
        let tc = TestCluster::new();
        let mut s = tc.session("postgres").unwrap();
        let out = run_sql(
            &mut s,
            "CREATE TABLE t (a int, b text); INSERT INTO t VALUES (1, NULL)",
        );
        assert!(out.is_ok());
        assert_eq!(out.tags, vec!["CREATE TABLE", "INSERT 0 1"]);
        let out = run_sql(&mut s, "SELECT a, b FROM t");
        assert_eq!(
            out.text_rows(),
            vec![vec!["1".to_string(), "NULL".to_string()]]
        );
        assert_eq!(out.rows[0][1], None);
        let out = run_sql(&mut s, "SELECT * FROM missing");
        assert!(!out.is_ok());
        assert_eq!(
            out.errors[0].sqlstate,
            crate::error::sqlstate::UNDEFINED_TABLE
        );
    }

    #[test]
    fn crash_sim_options_follow_the_design() {
        let o = TestClusterOptions::crash_sim(9);
        assert_eq!(o.seed, 9);
        assert_eq!(o.nframes, 16);
        assert_eq!(o.wal_segment_size, 2 << 20);
        assert!(o.knobs.assert_wal_before_data);
        assert!(!o.knobs.skip_commit_flush);
        let c = cluster_options(&o);
        assert!(!c.background_checkpointer);
        assert_eq!(c.shared_buffers, 16);
    }

    #[test]
    fn crash_and_restart_recovers_committed_rows() {
        let tc = TestCluster::with_options(TestClusterOptions::crash_sim(3)).unwrap();
        {
            let mut s = tc.session("postgres").unwrap();
            assert!(run_sql(&mut s, "CREATE TABLE t (a int)").is_ok());
            assert!(run_sql(&mut s, "INSERT INTO t VALUES (5)").is_ok());
            run_sql(&mut s, "BEGIN");
            run_sql(&mut s, "INSERT INTO t VALUES (6)");
        }
        let tc = tc.crash_and_restart(CrashMode::DropUnsynced).unwrap();
        let mut s = tc.session("postgres").unwrap();
        let out = run_sql(&mut s, "SELECT a FROM t");
        assert_eq!(out.text_rows(), vec![vec!["5".to_string()]]);
    }
}
