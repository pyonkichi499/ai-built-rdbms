//! Session: the boundary with `yuzhu-server` (`m1.md` §3.5, §4, §5.4).
//!
//! `execute_simple` runs one Simple Query message: parse → per statement
//! (utility statements handled here; SELECT / INSERT go through analyzer →
//! planner → executor) → results to a `ResultSink`.
//!
//! Transaction semantics follow PostgreSQL's Simple Query protocol:
//! outside an explicit block, one Query message is one implicit
//! transaction; inside a block an error moves the session to the failed
//! state, where everything but COMMIT / ROLLBACK gives 25P02.
//!
//! M2 の手順（`m2.md` §5.2〜§5.4）: 書く文はライターロックを先に取り、
//! ストレージバリア（共有）の下でスナップショット → analyze → plan → 実行し、
//! 文が成功したら CCI する。コミット / アボートは §5.3。

use std::collections::HashMap;
use std::sync::Arc;

use crate::analyzer::{self, BoundCreateTable, BoundDropTable, BoundSelect, BoundStatement};
use crate::catalog::CatalogReader;
use crate::catalog::reader::StatementCatalog;
use crate::catalog::store::NewTable;
use crate::engine::{Cluster, DatabaseHandle};
use crate::error::{Error, Result, Severity, SqlState, sqlstate};
use crate::executor::eval::row_to_text;
use crate::executor::{self, ExecCtx, SessionInfo};
use crate::interrupt::InterruptFlag;
use crate::planner;
use crate::settings::Settings;
use crate::sql::{
    self,
    ast::{
        ParamTarget, SetArg, SetStmt, SetValue, ShowStmt, Statement, TransactionKind,
        TransactionMode,
    },
};
use crate::storage::buffer;
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::txn::Transaction;
use crate::types::{Datum, Oid, SqlType, io, oid};

/// Values from the `StartupMessage`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupParams {
    pub user: String,
    pub database: String,
    pub application_name: Option<String>,
    /// Other parameters (e.g. `client_encoding`, `DateStyle`, `options`).
    pub options: Vec<(String, String)>,
}

/// Transaction state reported in `ReadyForQuery` (`I` / `T` / `E`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransactionStatus {
    Idle,
    InBlock,
    Failed,
}

impl TransactionStatus {
    /// The `ReadyForQuery` status byte.
    pub fn as_byte(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InBlock => b'T',
            TransactionStatus::Failed => b'E',
        }
    }
}

/// One field of a `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDesc {
    pub name: String,
    /// Source table OID for plain column references, else 0.
    pub table_oid: Oid,
    /// Source attnum for plain column references, else 0.
    pub column_attnum: i16,
    pub type_oid: Oid,
    pub type_len: i16,
    pub type_modifier: i32,
}

/// A `NoticeResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub severity: Severity,
    pub sqlstate: SqlState,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl Notice {
    fn new(severity: Severity, sqlstate: SqlState, message: impl Into<String>) -> Self {
        Notice {
            severity,
            sqlstate,
            message: message.into(),
            detail: None,
            hint: None,
        }
    }
}

/// Receives the results of a query. Implemented by `yuzhu-server` (and by
/// test collectors).
pub trait ResultSink {
    fn row_description(&mut self, columns: &[ColumnDesc]) -> std::io::Result<()>;
    /// `values` are in `ColumnDesc` order, already converted to text with
    /// `types::io::output_text` (`None` = NULL).
    fn data_row(&mut self, values: &[Option<String>]) -> std::io::Result<()>;
    fn command_complete(&mut self, tag: &str) -> std::io::Result<()>;
    fn empty_query(&mut self) -> std::io::Result<()>;
    fn error(&mut self, err: &Error) -> std::io::Result<()>;
    fn notice(&mut self, notice: &Notice) -> std::io::Result<()>;
    fn parameter_status(&mut self, name: &str, value: &str) -> std::io::Result<()>;
}

/// Internal transaction state. `Implicit` is the transaction of a Query
/// message outside an explicit block; it never survives the message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TxState {
    Idle,
    Implicit,
    Block,
    Failed,
}

/// Output of one statement, collected while the database lock is held and
/// sent afterwards.
#[derive(Debug, Default)]
struct Output {
    notices: Vec<Notice>,
    columns: Option<Vec<ColumnDesc>>,
    rows: Vec<Vec<Option<String>>>,
}

const IN_FAILED_MSG: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

/// One client session.
#[derive(Debug)]
pub struct Session {
    /// `Some` for sessions made by [`Session::new`]. `None` only in unit
    /// tests of the statement state machine.
    cluster: Option<Arc<Cluster>>,
    /// The database this session is connected to (`Some` with `cluster`).
    db: Option<Arc<DatabaseHandle>>,
    /// OID of the connected role (the owner of created tables).
    role_oid: Oid,
    /// A FATAL error was sent: the server must close the connection.
    closing: bool,
    params: StartupParams,
    state: TxState,
    txn: Transaction,
    settings: Settings,
    /// Last value sent to the client for each reported parameter.
    reported: HashMap<&'static str, String>,
    /// Session ID (the writer lock owner passed to `begin_write`).
    id: u64,
    /// Whether the current Query message has more than one statement
    /// (PostgreSQL's implicit transaction *block*, where SET LOCAL works).
    multi_statement: bool,
    interrupt: Arc<InterruptFlag>,
}

impl Session {
    /// Creates a session. `Cluster::connect` rejects an unknown database
    /// (`3D000`), a database that does not accept connections (`55000`) and
    /// an unknown role (`28000`) as FATAL errors.
    pub fn new(cluster: Arc<Cluster>, params: StartupParams) -> Result<Session> {
        let (db, role) = cluster.connect(&params.database, &params.user)?;
        let id = cluster.next_session_id();
        let mut s = Session::build(Some(cluster), id, params);
        if let Some(c) = &s.cluster {
            for (name, value) in c.server_settings() {
                s.settings.set_server_value(name, value);
            }
        }
        s.db = Some(db);
        s.role_oid = role.oid;
        Ok(s)
    }

    fn build(cluster: Option<Arc<Cluster>>, id: u64, params: StartupParams) -> Session {
        let settings = Settings::new(&params.user, &startup_settings(&params));
        let reported = settings.reported_values().into_iter().collect();
        Session {
            cluster,
            db: None,
            role_oid: 0,
            closing: false,
            id,
            multi_statement: false,
            params,
            state: TxState::Idle,
            txn: Transaction::new(),
            settings,
            reported,
            interrupt: Arc::new(InterruptFlag::default()),
        }
    }

    /// `ParameterStatus` messages to send right after authentication.
    pub fn initial_parameter_status(&self) -> Vec<(String, String)> {
        self.settings
            .reported_values()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }

    /// Processes one Simple Query message. Errors are reported through
    /// `sink.error`; `Err` is returned only when writing to the sink fails
    /// (client gone).
    pub fn execute_simple(&mut self, sql: &str, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let parsed = sql::parse(sql);
        self.run(sql, parsed, sink)
    }

    /// Must be called when the connection closes (also after a panic):
    /// aborts the open transaction.
    pub fn terminate(&mut self) {
        if self.state != TxState::Idle {
            self.rollback_transaction();
        }
    }

    /// A FATAL error was reported: the server must close the connection
    /// after sending the pending messages.
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// The flag through which the server asks this session to stop.
    pub fn interrupt_flag(&self) -> Arc<InterruptFlag> {
        Arc::clone(&self.interrupt)
    }

    pub fn transaction_status(&self) -> TransactionStatus {
        match self.state {
            TxState::Idle | TxState::Implicit => TransactionStatus::Idle,
            TxState::Block => TransactionStatus::InBlock,
            TxState::Failed => TransactionStatus::Failed,
        }
    }

    /// The statement loop over already-parsed statements.
    fn run(
        &mut self,
        sql: &str,
        parsed: Result<Vec<Statement>>,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        let r = self.run_statements(sql, parsed, sink);
        if r.is_err() && self.state == TxState::Implicit {
            // The client is gone part-way through the message: the implicit
            // transaction must not survive.
            self.rollback_transaction();
        }
        r
    }

    fn run_statements(
        &mut self,
        sql: &str,
        parsed: Result<Vec<Statement>>,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        let stmts = match parsed {
            Ok(s) => s,
            Err(e) => return self.report_error(e, sql, sink),
        };
        if stmts.is_empty() {
            return sink.empty_query();
        }
        self.multi_statement = stmts.len() > 1;
        for stmt in &stmts {
            if self.state == TxState::Failed && !is_transaction_end(stmt) {
                let e = Error::new(sqlstate::IN_FAILED_SQL_TRANSACTION, IN_FAILED_MSG);
                return self.report_error(e, sql, sink);
            }
            if self.state == TxState::Idle {
                self.begin_transaction(TxState::Implicit);
            }
            let mut out = Output::default();
            let result = self.exec_statement(stmt, &mut out);
            self.send_output(&out, sink)?;
            match result {
                Ok(tag) => sink.command_complete(&tag)?,
                Err(e) => return self.report_error(e, sql, sink),
            }
        }
        if self.state == TxState::Implicit
            && let Err(e) = self.commit_transaction()
        {
            return self.report_error(e, sql, sink);
        }
        self.flush_parameter_status(sink)
    }

    /// Aborts the current transaction (implicit → idle, block → failed) and
    /// sends the error.
    fn report_error(
        &mut self,
        mut e: Error,
        sql: &str,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        if e.severity == Severity::Panic {
            // The shared state may be inconsistent: stop the cluster's
            // writes and drop this connection (`m2.md` §5.2 step 10).
            if let Some(c) = &self.cluster {
                c.poison();
            }
            e.severity = Severity::Fatal;
        }
        if e.severity == Severity::Fatal {
            self.closing = true;
        }
        match self.state {
            TxState::Implicit => {
                self.rollback_transaction();
            }
            TxState::Block => {
                self.rollback_transaction();
                self.state = TxState::Failed;
            }
            TxState::Idle | TxState::Failed => {}
        }
        e.resolve_position(sql);
        sink.error(&e)?;
        self.flush_parameter_status(sink)
    }

    fn send_output(&self, out: &Output, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        for n in &out.notices {
            self.send_notice(n, sink)?;
        }
        if let Some(cols) = &out.columns {
            sink.row_description(cols)?;
            for r in &out.rows {
                sink.data_row(r)?;
            }
        }
        Ok(())
    }

    fn send_notice(&self, n: &Notice, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let level = n.severity.as_str().to_ascii_lowercase();
        if self.settings.client_wants(&level) {
            sink.notice(n)?;
        }
        Ok(())
    }

    /// Sends `ParameterStatus` for every reported parameter whose value
    /// changed since it was last sent.
    fn flush_parameter_status(&mut self, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        for (name, value) in self.settings.reported_values() {
            if self.reported.get(name) != Some(&value) {
                sink.parameter_status(name, &value)?;
                self.reported.insert(name, value);
            }
        }
        Ok(())
    }

    // ----- transaction control ---------------------------------------------

    fn begin_transaction(&mut self, state: TxState) {
        self.txn = Transaction::new();
        self.settings.begin();
        self.state = state;
    }

    /// Commits (`m2.md` §5.3). The session is idle afterwards even when this
    /// fails: a failure to record the commit is `Severity::Panic`.
    fn commit_transaction(&mut self) -> Result<()> {
        let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
        self.settings.commit();
        self.state = TxState::Idle;
        let (Some(cluster), Some(xid)) = (self.cluster.as_ref(), txn.xid) else {
            return Ok(());
        };
        let mgr = cluster.txn_manager();
        // 1. clog + running list. The cache is invalidated only after this.
        mgr.commit(xid)?;
        // 2.
        if txn.catalog_dirty {
            cluster.invalidate_all_catalog_caches();
        }
        // 3. Remove the files of dropped tables once no statement can be
        // reading them. A failure does not undo the commit.
        if !txn.pending_unlinks.is_empty() {
            Self::unlink_files(cluster, &txn.pending_unlinks);
        }
        // 4. Release the writer lock.
        txn.writer = None;
        Ok(())
    }

    /// Aborts (`m2.md` §5.3). Never fails to reset the session; the error of
    /// `TxnManager::abort` (a `Panic`) is dropped here and poisons the
    /// cluster.
    fn rollback_transaction(&mut self) {
        let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
        self.settings.rollback();
        self.state = TxState::Idle;
        let (Some(cluster), Some(xid)) = (self.cluster.as_ref(), txn.xid) else {
            return;
        };
        if let Err(e) = cluster.txn_manager().abort(xid) {
            warn(&format!("could not record the abort: {}", e.message));
            cluster.poison();
        }
        if !txn.pending_creates.is_empty() {
            Self::unlink_files(cluster, &txn.pending_creates);
        }
        txn.writer = None;
    }

    /// Removes relation files under the exclusive storage barrier. Errors
    /// are only warned about: the transaction is already decided.
    fn unlink_files(cluster: &Cluster, rels: &[RelFileLocator]) {
        let guard = match cluster.txn_manager().exclusive_barrier() {
            Ok(g) => g,
            Err(e) => {
                warn(&format!("could not remove relation files: {}", e.message));
                return;
            }
        };
        for rel in rels {
            if let Err(e) = cluster.storage().unlink_storage(*rel) {
                warn(&format!(
                    "could not remove relation file {}: {}",
                    rel.rel_number.0, e.message
                ));
            }
        }
        drop(guard);
    }

    // ----- statements --------------------------------------------------------

    /// Executes one statement and returns its command tag.
    fn exec_statement(&mut self, stmt: &Statement, out: &mut Output) -> Result<String> {
        match stmt {
            Statement::Transaction(t) => self.exec_transaction(t.kind, &t.modes, out),
            Statement::Set(s) => self.exec_set(s, out),
            Statement::Reset(r) => {
                match &r.target {
                    ParamTarget::All => self.settings.reset_all(),
                    ParamTarget::Name(n) => self.settings.reset(n)?,
                }
                Ok("RESET".into())
            }
            Statement::Show(s) => self.exec_show(s, out),
            Statement::Checkpoint(_) => {
                // No storage barrier may be held (`checkpoint::run` takes it).
                let cluster = self.cluster()?;
                cluster.checkpoint()?;
                Ok("CHECKPOINT".into())
            }
            _ => self.exec_data_statement(stmt, out),
        }
    }

    fn cluster(&self) -> Result<Arc<Cluster>> {
        self.cluster
            .clone()
            .ok_or_else(|| Error::not_supported("this session has no cluster"))
    }

    /// SELECT / VALUES / INSERT / UPDATE / DELETE / CREATE TABLE / DROP TABLE
    /// (`m2.md` §5.2).
    fn exec_data_statement(&mut self, stmt: &Statement, out: &mut Output) -> Result<String> {
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        if cluster.is_poisoned() {
            return Err(Error::new(
                sqlstate::ADMIN_SHUTDOWN,
                "the server is in a failed state; restart it",
            )
            .with_severity(Severity::Fatal));
        }
        // 1. The writer lock comes before the snapshot, so that no other
        // writer's commit lands between the snapshot and our first write.
        let is_write = matches!(
            stmt,
            Statement::Insert(_)
                | Statement::Update(_)
                | Statement::Delete(_)
                | Statement::CreateTable(_)
                | Statement::DropTable(_)
        );
        let mgr = Arc::clone(cluster.txn_manager());
        if is_write && self.txn.writer.is_none() {
            let (xid, guard) = mgr.begin_write(self.id, self.settings.lock_timeout())?;
            self.txn.xid = Some(xid);
            self.txn.writer = Some(guard);
        }
        // 2. The shared storage barrier for the duration of the statement.
        let barrier = mgr.statement_barrier()?;
        buffer::track::barrier_acquired();
        let result = self.run_under_barrier(stmt, &cluster, &db, out);
        buffer::track::barrier_released();
        drop(barrier);
        // 8.
        buffer::assert_no_pins();
        let tag = result?;
        // 9.
        self.txn.command_counter_increment()?;
        Ok(tag)
    }

    /// Steps 3 to 6 of `m2.md` §5.2.
    fn run_under_barrier(
        &mut self,
        stmt: &Statement,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        out: &mut Output,
    ) -> Result<String> {
        // 3. The generation is read before the snapshot.
        let generation = db.cache.generation();
        // 4.
        let snap = cluster.txn_manager().snapshot(self.txn.xid, self.txn.cid);
        // 5.
        let search_path = self.settings.search_path();
        let catalog = StatementCatalog {
            db,
            snapshot: &snap,
            gen_at_snapshot: generation,
            bypass_cache: self.txn.catalog_dirty,
            search_path: &search_path,
        };
        // 6.
        let bound = analyzer::analyze(stmt, &catalog)?;
        match bound {
            BoundStatement::CreateTable(c) => {
                self.exec_create_table(&c, cluster, db, &snap, &catalog, out)
            }
            BoundStatement::DropTable(d) => self.exec_drop_table(&d, db, &snap, out),
            BoundStatement::Checkpoint => Err(Error::internal(
                "CHECKPOINT must be handled before the storage barrier",
            )),
            bound => {
                let plan = planner::plan(&bound)?;
                let (columns, types) = match &bound {
                    BoundStatement::Select(sel) => (Some(column_descs(sel, &catalog)), {
                        sel.columns
                            .iter()
                            .map(|c| {
                                if c.ty.oid == oid::UNKNOWN {
                                    SqlType::TEXT
                                } else {
                                    c.ty
                                }
                            })
                            .collect::<Vec<_>>()
                    }),
                    _ => (None, Vec::new()),
                };
                out.columns = columns;
                let ncols = types.len();
                let opts = io::OutputOpts {
                    extra_float_digits: self.settings.extra_float_digits(),
                };
                let info = self.session_info();
                let interrupts = Arc::clone(&self.interrupt);
                let mut exec = executor::build(&plan);
                let mut ctx = ExecCtx {
                    catalog: &catalog,
                    storage: &**cluster.storage(),
                    txn: &mut self.txn,
                    snapshot: &snap,
                    session: &info,
                    interrupts: &interrupts,
                };
                while let Some(row) = exec.next(&mut ctx)? {
                    if out.columns.is_some() {
                        let visible: Vec<Datum> = row.iter().take(ncols).cloned().collect();
                        out.rows.push(row_to_text(&visible, &types, &opts));
                    }
                }
                let n = exec.rows_affected();
                Ok(match bound {
                    BoundStatement::Select(_) => format!("SELECT {}", out.rows.len()),
                    BoundStatement::Insert(_) => format!("INSERT 0 {n}"),
                    BoundStatement::Update(_) => format!("UPDATE {n}"),
                    BoundStatement::Delete(_) => format!("DELETE {n}"),
                    _ => unreachable!("handled above"),
                })
            }
        }
    }

    /// CREATE TABLE (`m2.md` §5.4).
    fn exec_create_table(
        &mut self,
        c: &BoundCreateTable,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        snap: &crate::txn::Snapshot,
        catalog: &dyn CatalogReader,
        out: &mut Output,
    ) -> Result<String> {
        if catalog.table(Some(&c.schema), &c.name)?.is_some() {
            if c.if_not_exists {
                out.notices.push(already_exists_notice(&c.name));
                return Ok("CREATE TABLE".into());
            }
            return Err(Error::new(
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists", c.name),
            ));
        }
        let Some(namespace) = db.catalog.namespace_oid(snap, &c.schema)? else {
            return Err(Error::new(
                sqlstate::INVALID_SCHEMA_NAME,
                format!("schema \"{}\" does not exist", c.schema),
            ));
        };
        let alloc = cluster.oid_allocator();
        let oid = db.catalog.get_new_relation_oid(alloc)?;
        let locator = RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: db.oid,
            rel_number: RelFileNumber(oid),
        };
        let (attrdef_oids, constraint_oids) = db
            .catalog
            .allocate_child_oids(alloc, &c.columns, &c.checks)?;
        // The file is created first and remembered, so an abort removes it.
        cluster.storage().create_storage(locator)?;
        self.txn.pending_creates.push(locator);
        let w = self.txn.write_ctx()?;
        db.catalog.create_table(
            &w,
            snap,
            &NewTable {
                oid,
                namespace,
                name: c.name.clone(),
                owner: self.role_oid,
                columns: c.columns.clone(),
                checks: c.checks.clone(),
                attrdef_oids,
                constraint_oids,
            },
        )?;
        self.txn.catalog_dirty = true;
        Ok("CREATE TABLE".into())
    }

    /// DROP TABLE (`m2.md` §5.4). The files go at commit.
    fn exec_drop_table(
        &mut self,
        d: &BoundDropTable,
        db: &Arc<DatabaseHandle>,
        snap: &crate::txn::Snapshot,
        out: &mut Output,
    ) -> Result<String> {
        for name in &d.missing {
            out.notices.push(Notice::new(
                Severity::Notice,
                sqlstate::SUCCESSFUL_COMPLETION,
                format!("table \"{name}\" does not exist, skipping"),
            ));
        }
        for def in &d.tables {
            if def.is_system_catalog() {
                return Err(Error::new(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied: \"{}\" is a system catalog", def.name),
                ));
            }
        }
        for def in &d.tables {
            let w = self.txn.write_ctx()?;
            db.catalog.drop_table(&w, snap, def)?;
            self.txn.pending_unlinks.push(def.locator);
            self.txn.catalog_dirty = true;
        }
        Ok("DROP TABLE".into())
    }

    fn exec_transaction(
        &mut self,
        kind: TransactionKind,
        modes: &[TransactionMode],
        out: &mut Output,
    ) -> Result<String> {
        match kind {
            TransactionKind::Begin | TransactionKind::StartTransaction => {
                check_transaction_modes(modes)?;
                if self.state == TxState::Block {
                    out.notices.push(Notice::new(
                        Severity::Warning,
                        sqlstate::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    ));
                } else {
                    // Statements earlier in this Query message become part
                    // of the block, as in PostgreSQL.
                    self.state = TxState::Block;
                }
                Ok(if kind == TransactionKind::Begin {
                    "BEGIN"
                } else {
                    "START TRANSACTION"
                }
                .into())
            }
            TransactionKind::Commit | TransactionKind::End => match self.state {
                TxState::Failed => {
                    self.rollback_transaction();
                    Ok("ROLLBACK".into())
                }
                state => {
                    if state != TxState::Block {
                        out.notices.push(no_transaction_warning());
                    }
                    self.commit_transaction()?;
                    Ok("COMMIT".into())
                }
            },
            TransactionKind::Rollback | TransactionKind::Abort => {
                if !matches!(self.state, TxState::Block | TxState::Failed) {
                    out.notices.push(no_transaction_warning());
                }
                self.rollback_transaction();
                Ok("ROLLBACK".into())
            }
        }
    }

    fn exec_set(&mut self, s: &SetStmt, out: &mut Output) -> Result<String> {
        // An implicit transaction of a multi-statement Query is a block for
        // SET LOCAL (PostgreSQL's TBLOCK_IMPLICIT_INPROGRESS).
        let in_block = self.state == TxState::Block
            || (self.state == TxState::Implicit && self.multi_statement);
        if s.local && !in_block {
            out.notices.push(Notice::new(
                Severity::Warning,
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET LOCAL can only be used in transaction blocks",
            ));
            // Still validate the name, as PostgreSQL does.
            if crate::settings::lookup(&s.name).is_none() && !s.name.contains('.') {
                return Err(Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("unrecognized configuration parameter \"{}\"", s.name),
                ));
            }
            return Ok("SET".into());
        }
        match &s.value {
            SetValue::Default => self.settings.set(&s.name, None, s.local)?,
            SetValue::Values(args) => {
                let texts: Vec<String> = args
                    .iter()
                    .map(|a| match a {
                        SetArg::Word(w) | SetArg::String(w) | SetArg::Number(w) => w.clone(),
                    })
                    .collect();
                self.settings.set(&s.name, Some(&texts), s.local)?;
            }
        }
        Ok("SET".into())
    }

    fn exec_show(&mut self, s: &ShowStmt, out: &mut Output) -> Result<String> {
        match &s.target {
            ParamTarget::Name(n) => {
                let (name, value) = self.settings.show(n)?;
                out.columns = Some(vec![text_column(name)]);
                out.rows.push(vec![Some(value)]);
            }
            ParamTarget::All => {
                out.columns = Some(vec![
                    text_column("name"),
                    text_column("setting"),
                    text_column("description"),
                ]);
                for (n, v, d) in self.settings.show_all() {
                    out.rows.push(vec![Some(n), Some(v), Some(d)]);
                }
            }
        }
        Ok("SHOW".into())
    }

    fn session_info(&self) -> SessionInfo {
        let current_schema = self
            .settings
            .search_path()
            .into_iter()
            .find(|s| s == "public" || s == "pg_catalog");
        SessionInfo {
            current_user: self.params.user.clone(),
            session_user: self.params.user.clone(),
            database: self.params.database.clone(),
            current_schema,
        }
    }
}

fn is_transaction_end(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Transaction(t) if matches!(
            t.kind,
            TransactionKind::Commit
                | TransactionKind::End
                | TransactionKind::Rollback
                | TransactionKind::Abort
        )
    )
}

fn check_transaction_modes(modes: &[TransactionMode]) -> Result<()> {
    for m in modes {
        match m {
            TransactionMode::IsolationLevel(l)
                if l != "read committed" && l != "read uncommitted" =>
            {
                return Err(Error::not_supported(format!(
                    "transaction isolation level \"{l}\" is not supported yet"
                )));
            }
            TransactionMode::ReadOnly => {
                return Err(Error::not_supported(
                    "read-only transactions are not supported yet",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn no_transaction_warning() -> Notice {
    Notice::new(
        Severity::Warning,
        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
        "there is no transaction in progress",
    )
}
#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

fn already_exists_notice(name: &str) -> Notice {
    Notice::new(
        Severity::Notice,
        sqlstate::DUPLICATE_TABLE,
        format!("relation \"{name}\" already exists, skipping"),
    )
}

fn text_column(name: impl Into<String>) -> ColumnDesc {
    ColumnDesc {
        name: name.into(),
        table_oid: 0,
        column_attnum: 0,
        type_oid: oid::TEXT,
        type_len: -1,
        type_modifier: -1,
    }
}

/// `RowDescription` fields for a SELECT's visible columns.
fn column_descs(sel: &BoundSelect, catalog: &dyn CatalogReader) -> Vec<ColumnDesc> {
    sel.columns
        .iter()
        .map(|c| {
            // A leftover `unknown` is reported as text (PostgreSQL ≥ 10).
            let ty = if c.ty.oid == oid::UNKNOWN {
                SqlType::TEXT
            } else {
                c.ty
            };
            ColumnDesc {
                name: c.name.clone(),
                table_oid: c.table_oid,
                column_attnum: c.attnum,
                type_oid: ty.oid,
                type_len: catalog.type_by_oid(ty.oid).map_or(-1, |t| t.typlen),
                type_modifier: ty.typmod,
            }
        })
        .collect()
}

/// Settings given at connection start: `application_name`, other startup
/// parameters, and `-c name=value` / `--name=value` items of `options`.
fn startup_settings(params: &StartupParams) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(app) = &params.application_name {
        v.push(("application_name".to_owned(), app.clone()));
    }
    for (k, val) in &params.options {
        if k == "options" {
            let mut words = val.split_whitespace();
            while let Some(w) = words.next() {
                let item = if w == "-c" {
                    words.next().unwrap_or("")
                } else if let Some(rest) = w.strip_prefix("-c").or_else(|| w.strip_prefix("--")) {
                    rest
                } else {
                    continue;
                };
                if let Some((n, x)) = item.split_once('=') {
                    v.push((n.replace('-', "_"), x.to_owned()));
                }
            }
        } else if !matches!(k.as_str(), "user" | "database" | "replication") {
            v.push((k.clone(), val.clone()));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{BoundFrom, OutputColumn};
    use crate::catalog::fake::FakeCatalog;
    use crate::error::Span;
    use crate::sql::ast::{ResetStmt, TransactionStmt};

    fn params(db: &str) -> StartupParams {
        StartupParams {
            user: "alice".into(),
            database: db.into(),
            application_name: Some("psql".into()),
            options: vec![],
        }
    }

    #[derive(Debug, PartialEq)]
    enum Ev {
        RowDesc(Vec<String>),
        Row(Vec<Option<String>>),
        Complete(String),
        Empty,
        Error(&'static str, Option<u32>),
        Notice(Severity, &'static str),
        Param(String, String),
    }

    #[derive(Default)]
    struct Sink(Vec<Ev>);

    impl ResultSink for Sink {
        fn row_description(&mut self, c: &[ColumnDesc]) -> std::io::Result<()> {
            self.0
                .push(Ev::RowDesc(c.iter().map(|c| c.name.clone()).collect()));
            Ok(())
        }
        fn data_row(&mut self, v: &[Option<String>]) -> std::io::Result<()> {
            self.0.push(Ev::Row(v.to_vec()));
            Ok(())
        }
        fn command_complete(&mut self, tag: &str) -> std::io::Result<()> {
            self.0.push(Ev::Complete(tag.into()));
            Ok(())
        }
        fn empty_query(&mut self) -> std::io::Result<()> {
            self.0.push(Ev::Empty);
            Ok(())
        }
        fn error(&mut self, e: &Error) -> std::io::Result<()> {
            self.0.push(Ev::Error(e.sqlstate.0, e.position));
            Ok(())
        }
        fn notice(&mut self, n: &Notice) -> std::io::Result<()> {
            self.0.push(Ev::Notice(n.severity, n.sqlstate.0));
            Ok(())
        }
        fn parameter_status(&mut self, name: &str, value: &str) -> std::io::Result<()> {
            self.0.push(Ev::Param(name.into(), value.into()));
            Ok(())
        }
    }

    fn session() -> Session {
        Session::build(None, 1, params("postgres"))
    }

    fn tx(kind: TransactionKind) -> Statement {
        Statement::Transaction(TransactionStmt {
            kind,
            modes: vec![],
            span: Span::default(),
        })
    }
    fn set(name: &str, v: &str, local: bool) -> Statement {
        Statement::Set(SetStmt {
            local,
            name: name.into(),
            value: SetValue::Values(vec![SetArg::String(v.into())]),
            span: Span::default(),
        })
    }
    fn show(name: &str) -> Statement {
        Statement::Show(ShowStmt {
            target: ParamTarget::Name(name.into()),
            span: Span::default(),
        })
    }

    /// Runs statements as one Query message and returns the events.
    fn run(s: &mut Session, stmts: Vec<Statement>) -> Vec<Ev> {
        let mut sink = Sink::default();
        s.run("", Ok(stmts), &mut sink).unwrap();
        sink.0
    }

    fn show_value(s: &mut Session, name: &str) -> String {
        let ev = run(s, vec![show(name)]);
        match &ev[1] {
            Ev::Row(r) => r[0].clone().unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn initial_parameter_status_lists_reported_parameters() {
        let s = session();
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        let ps = s.initial_parameter_status();
        assert!(ps.contains(&("session_authorization".into(), "alice".into())));
        assert!(ps.contains(&("application_name".into(), "psql".into())));
        assert!(ps.contains(&("DateStyle".into(), "ISO, MDY".into())));
        assert!(ps.contains(&("TimeZone".into(), "UTC".into())));
        assert_eq!(ps.len(), 13);
    }

    #[test]
    fn startup_options_apply() {
        let mut p = params("postgres");
        p.options = vec![
            ("DateStyle".into(), "ISO".into()),
            (
                "options".into(),
                "-c search_path=foo --extra-float-digits=2".into(),
            ),
            ("bogus".into(), "x".into()),
        ];
        let mut s = Session::build(None, 1, p);
        assert_eq!(show_value(&mut s, "search_path"), "foo");
        assert_eq!(show_value(&mut s, "extra_float_digits"), "2");
    }

    #[test]
    fn terminate_ends_the_open_transaction() {
        let mut s = session();
        run(&mut s, vec![tx(TransactionKind::Begin)]);
        assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
        s.terminate();
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        assert!(!s.interrupt_flag().is_terminate_requested());
    }

    #[test]
    fn empty_query_and_parse_error_position() {
        let mut s = session();
        assert_eq!(run(&mut s, vec![]), vec![Ev::Empty]);
        let sql = "select 'é' x";
        let off = u32::try_from(sql.find('x').unwrap()).unwrap();
        let mut sink = Sink::default();
        let err = Error::syntax_at(Span::new(off, off + 1), "syntax error at or near \"x\"");
        s.run(sql, Err(err), &mut sink).unwrap();
        assert_eq!(sink.0, vec![Ev::Error("42601", Some(12))]);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn transaction_tags_and_warnings() {
        let mut s = session();
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::Commit)]),
            vec![
                Ev::Notice(Severity::Warning, "25P01"),
                Ev::Complete("COMMIT".into())
            ]
        );
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::Abort)]),
            vec![
                Ev::Notice(Severity::Warning, "25P01"),
                Ev::Complete("ROLLBACK".into())
            ]
        );
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::StartTransaction)]),
            vec![Ev::Complete("START TRANSACTION".into())]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::Begin)]),
            vec![
                Ev::Notice(Severity::Warning, "25001"),
                Ev::Complete("BEGIN".into())
            ]
        );
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::End)]),
            vec![Ev::Complete("COMMIT".into())]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        // Unsupported modes.
        let mut sink = Sink::default();
        let st = Statement::Transaction(TransactionStmt {
            kind: TransactionKind::Begin,
            modes: vec![TransactionMode::IsolationLevel("serializable".into())],
            span: Span::default(),
        });
        s.run("", Ok(vec![st]), &mut sink).unwrap();
        assert_eq!(sink.0, vec![Ev::Error("0A000", None)]);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn failed_block() {
        let mut s = session();
        run(&mut s, vec![tx(TransactionKind::Begin)]);
        let ev = run(&mut s, vec![set("no_such", "1", false)]);
        assert_eq!(ev, vec![Ev::Error("42704", None)]);
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        for st in [
            show("search_path"),
            set("search_path", "x", false),
            tx(TransactionKind::Begin),
        ] {
            assert_eq!(run(&mut s, vec![st]), vec![Ev::Error("25P02", None)]);
        }
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::Commit)]),
            vec![Ev::Complete("ROLLBACK".into())]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        // An error after BEGIN in the same message leaves the block failed.
        let ev = run(
            &mut s,
            vec![
                tx(TransactionKind::Begin),
                set("no_such", "1", false),
                show("timezone"),
            ],
        );
        assert_eq!(
            ev,
            vec![Ev::Complete("BEGIN".into()), Ev::Error("42704", None)]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        run(&mut s, vec![tx(TransactionKind::Rollback)]);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn set_show_reset_and_parameter_status() {
        let mut s = session();
        let ev = run(
            &mut s,
            vec![
                set("application_name", "x", false),
                show("Application_Name"),
            ],
        );
        assert_eq!(
            ev,
            vec![
                Ev::Complete("SET".into()),
                Ev::RowDesc(vec!["application_name".into()]),
                Ev::Row(vec![Some("x".into())]),
                Ev::Complete("SHOW".into()),
                // Only at the end of the Query message (C-16).
                Ev::Param("application_name".into(), "x".into()),
            ]
        );
        // Unreported parameter: no ParameterStatus.
        let ev = run(&mut s, vec![set("extra_float_digits", "0", false)]);
        assert_eq!(ev, vec![Ev::Complete("SET".into())]);
        let ev = run(
            &mut s,
            vec![Statement::Reset(ResetStmt {
                target: ParamTarget::All,
                span: Span::default(),
            })],
        );
        assert_eq!(
            ev,
            vec![
                Ev::Complete("RESET".into()),
                Ev::Param("application_name".into(), "psql".into()),
            ]
        );
        assert_eq!(show_value(&mut s, "extra_float_digits"), "1");
        let ev = run(
            &mut s,
            vec![Statement::Show(ShowStmt {
                target: ParamTarget::All,
                span: Span::default(),
            })],
        );
        assert_eq!(
            ev[0],
            Ev::RowDesc(vec!["name".into(), "setting".into(), "description".into()])
        );
        assert!(ev.len() > 20);
    }

    #[test]
    fn set_is_transactional() {
        let mut s = session();
        run(&mut s, vec![tx(TransactionKind::Begin)]);
        run(&mut s, vec![set("application_name", "in_txn", false)]);
        let ev = run(&mut s, vec![tx(TransactionKind::Rollback)]);
        assert_eq!(
            ev,
            vec![
                Ev::Complete("ROLLBACK".into()),
                Ev::Param("application_name".into(), "psql".into()),
            ]
        );
        // SET LOCAL reverts at COMMIT.
        run(&mut s, vec![tx(TransactionKind::Begin)]);
        run(&mut s, vec![set("extra_float_digits", "0", true)]);
        assert_eq!(show_value(&mut s, "extra_float_digits"), "0");
        run(&mut s, vec![tx(TransactionKind::Commit)]);
        assert_eq!(show_value(&mut s, "extra_float_digits"), "1");
        // SET LOCAL outside a block: warning, no effect.
        let ev = run(&mut s, vec![set("extra_float_digits", "0", true)]);
        assert_eq!(
            ev,
            vec![
                Ev::Notice(Severity::Warning, "25P01"),
                Ev::Complete("SET".into())
            ]
        );
        assert_eq!(show_value(&mut s, "extra_float_digits"), "1");
        // An error in an implicit transaction undoes earlier SETs.
        let ev = run(
            &mut s,
            vec![
                set("application_name", "a", false),
                set("no_such", "1", false),
            ],
        );
        assert_eq!(
            ev,
            // The final value did not change: no ParameterStatus at all.
            vec![Ev::Complete("SET".into()), Ev::Error("42704", None)]
        );
        // BEGIN; SET; ROLLBACK in one message: nothing to report.
        let ev = run(
            &mut s,
            vec![
                tx(TransactionKind::Begin),
                set("application_name", "a", false),
                tx(TransactionKind::Rollback),
            ],
        );
        assert!(!ev.iter().any(|e| matches!(e, Ev::Param(..))), "{ev:?}");
        // SET LOCAL in a multi-statement Query applies until its end.
        let ev = run(
            &mut s,
            vec![
                set("extra_float_digits", "0", true),
                show("extra_float_digits"),
            ],
        );
        assert_eq!(
            ev,
            vec![
                Ev::Complete("SET".into()),
                Ev::RowDesc(vec!["extra_float_digits".into()]),
                Ev::Row(vec![Some("0".into())]),
                Ev::Complete("SHOW".into()),
            ]
        );
        assert_eq!(show_value(&mut s, "extra_float_digits"), "1");
        // COMMIT in the middle of a message commits what came before.
        run(
            &mut s,
            vec![
                set("application_name", "kept", false),
                tx(TransactionKind::Commit),
                set("no_such", "1", false),
            ],
        );
        assert_eq!(show_value(&mut s, "application_name"), "kept");
    }

    fn sql(s: &mut Session, q: &str) -> Vec<Ev> {
        let mut sink = Sink::default();
        s.execute_simple(q, &mut sink).unwrap();
        sink.0
    }

    /// A sink whose writes fail after `ok` successful command completions.
    struct FailingSink {
        ok: usize,
    }

    impl ResultSink for FailingSink {
        fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
            Ok(())
        }
        fn data_row(&mut self, _: &[Option<String>]) -> std::io::Result<()> {
            Ok(())
        }
        fn command_complete(&mut self, _: &str) -> std::io::Result<()> {
            if self.ok == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            self.ok -= 1;
            Ok(())
        }
        fn empty_query(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn error(&mut self, _: &Error) -> std::io::Result<()> {
            Ok(())
        }
        fn notice(&mut self, _: &Notice) -> std::io::Result<()> {
            Ok(())
        }
        fn parameter_status(&mut self, _: &str, _: &str) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sink_failure_rolls_back_implicit_transaction() {
        let mut s = session();
        let r = s.run(
            "",
            Ok(vec![
                set("application_name", "a", false),
                set("application_name", "b", false),
            ]),
            &mut FailingSink { ok: 1 },
        );
        assert!(r.is_err());
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        assert_eq!(show_value(&mut s, "application_name"), "psql");
    }

    #[test]
    fn client_min_messages_filters_notices() {
        let mut s = session();
        run(&mut s, vec![set("client_min_messages", "error", false)]);
        assert_eq!(
            run(&mut s, vec![tx(TransactionKind::Commit)]),
            vec![Ev::Complete("COMMIT".into())]
        );
    }

    #[test]
    fn column_descriptions() {
        let sel = BoundSelect {
            from: BoundFrom::None,
            filter: None,
            targets: vec![],
            columns: vec![
                OutputColumn {
                    name: "a".into(),
                    ty: SqlType::INT4,
                    table_oid: 16384,
                    attnum: 1,
                },
                OutputColumn {
                    name: "v".into(),
                    ty: SqlType::varchar(3),
                    table_oid: 0,
                    attnum: 0,
                },
                OutputColumn {
                    name: "?column?".into(),
                    ty: SqlType::UNKNOWN,
                    table_oid: 0,
                    attnum: 0,
                },
            ],
            distinct: false,
            order_by: vec![],
            limit: None,
            offset: None,
        };
        let cat = FakeCatalog::new("postgres");
        let d = column_descs(&sel, &cat);
        assert_eq!(
            (
                d[0].table_oid,
                d[0].column_attnum,
                d[0].type_oid,
                d[0].type_len
            ),
            (16384, 1, 23, 4)
        );
        assert_eq!(
            (d[1].type_oid, d[1].type_len, d[1].type_modifier),
            (1043, -1, 7)
        );
        assert_eq!(d[2].type_oid, oid::TEXT);
    }

    #[test]
    fn session_info_current_schema() {
        let mut s = session();
        assert_eq!(s.session_info().current_schema.as_deref(), Some("public"));
        assert_eq!(s.session_info().database, "postgres");
        run(&mut s, vec![set("search_path", "nope", false)]);
        assert_eq!(s.session_info().current_schema, None);
    }

    // ----- with a real cluster --------------------------------------------

    use crate::testing::TestCluster;

    fn cl() -> (TestCluster, Session) {
        let tc = TestCluster::new();
        let s = tc.session("postgres").unwrap();
        (tc, s)
    }

    fn errors(ev: &[Ev]) -> Vec<&'static str> {
        ev.iter()
            .filter_map(|e| match e {
                Ev::Error(c, _) => Some(*c),
                _ => None,
            })
            .collect()
    }

    fn data(ev: &[Ev]) -> Vec<Vec<Option<String>>> {
        ev.iter()
            .filter_map(|e| match e {
                Ev::Row(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }

    fn one(s: &mut Session, q: &str) -> String {
        let ev = sql(s, q);
        assert!(errors(&ev).is_empty(), "{q}: {ev:?}");
        data(&ev)[0][0].clone().unwrap()
    }

    fn count(s: &mut Session, table: &str) -> usize {
        let ev = sql(s, &format!("select * from {table}"));
        assert!(errors(&ev).is_empty(), "{ev:?}");
        data(&ev).len()
    }

    fn tags(ev: &[Ev]) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                Ev::Complete(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn select_without_table_and_show_server_values() {
        let (_tc, mut s) = cl();
        assert_eq!(one(&mut s, "select 1 + 1"), "2");
        assert_eq!(one(&mut s, "show server_version"), "17.0");
        assert_eq!(one(&mut s, "show server_version_num"), "170000");
        assert_eq!(one(&mut s, "show data_checksums"), "on");
        assert_eq!(one(&mut s, "show block_size"), "8192");
        assert_eq!(one(&mut s, "show segment_size"), "1GB");
        assert_eq!(one(&mut s, "show checkpoint_timeout"), "1h");
        assert_eq!(one(&mut s, "show data_directory"), "/sim/data");
        assert_eq!(one(&mut s, "show shared_buffers"), "2MB");
        assert!(one(&mut s, "select version()").starts_with("PostgreSQL 17.0"));
    }

    #[test]
    fn create_insert_select_update_delete() {
        let (_tc, mut s) = cl();
        let ev = sql(&mut s, "create table t (a int primary_key_free, b text)");
        assert_eq!(errors(&ev), vec!["42601"]);
        let ev = sql(
            &mut s,
            "create table t (a int not null, b text default 'x')",
        );
        assert_eq!(tags(&ev), vec!["CREATE TABLE"]);
        let ev = sql(
            &mut s,
            "insert into t values (1, 'one'), (2, null), (3, default)",
        );
        assert_eq!(tags(&ev), vec!["INSERT 0 3"]);
        assert_eq!(count(&mut s, "t"), 3);
        let ev = sql(&mut s, "select a, b from t order by a");
        assert_eq!(
            data(&ev),
            vec![
                vec![Some("1".into()), Some("one".into())],
                vec![Some("2".into()), None],
                vec![Some("3".into()), Some("x".into())],
            ]
        );
        assert_eq!(
            tags(&sql(&mut s, "update t set a = a + 10 where a >= 2")),
            vec!["UPDATE 2"]
        );
        assert_eq!(
            tags(&sql(&mut s, "delete from t where a = 1")),
            vec!["DELETE 1"]
        );
        let ev = sql(&mut s, "select a from t order by a");
        assert_eq!(
            data(&ev),
            vec![vec![Some("12".into())], vec![Some("13".into())]]
        );
        // Constraint violations abort the statement.
        let ev = sql(&mut s, "insert into t values (null, 'z')");
        assert_eq!(errors(&ev), vec!["23502"]);
        assert_eq!(count(&mut s, "t"), 2);
        // pg_catalog tables are visible and protected.
        assert_eq!(
            one(&mut s, "select relname from pg_class where relname = 't'"),
            "t"
        );
        assert_eq!(errors(&sql(&mut s, "drop table pg_class")), vec!["42501"]);
        assert_eq!(
            errors(&sql(&mut s, "insert into pg_class (relname) values ('x')")),
            vec!["42501"]
        );
    }

    #[test]
    fn rollback_undoes_data_and_ddl() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table keep (a int)");
        sql(&mut s, "insert into keep values (1)");
        sql(&mut s, "begin");
        sql(&mut s, "create table gone (a int)");
        sql(&mut s, "insert into gone values (1)");
        sql(&mut s, "insert into keep values (2)");
        assert_eq!(count(&mut s, "gone"), 1);
        assert_eq!(count(&mut s, "keep"), 2);
        let files_during = rel_files(&tc);
        sql(&mut s, "rollback");
        sql(&mut s, "checkpoint");
        assert_eq!(count(&mut s, "keep"), 1);
        assert_eq!(errors(&sql(&mut s, "select * from gone")), vec!["42P01"]);
        assert!(rel_files(&tc) < files_during, "created file removed");
        // The writer lock is free again.
        let mut s2 = tc.session("postgres").unwrap();
        assert_eq!(
            tags(&sql(&mut s2, "insert into keep values (3)")),
            vec!["INSERT 0 1"]
        );
    }

    #[test]
    fn drop_table_unlinks_at_commit_only() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table d (a int)");
        sql(&mut s, "insert into d values (1)");
        let before = rel_files(&tc);
        sql(&mut s, "begin");
        assert_eq!(tags(&sql(&mut s, "drop table d")), vec!["DROP TABLE"]);
        assert_eq!(errors(&sql(&mut s, "select * from d")), vec!["42P01"]);
        sql(&mut s, "rollback");
        assert_eq!(count(&mut s, "d"), 1);
        assert_eq!(rel_files(&tc), before);
        sql(&mut s, "drop table d");
        sql(&mut s, "checkpoint");
        assert!(rel_files(&tc) < before);
        let ev = sql(&mut s, "drop table if exists d");
        assert_eq!(tags(&ev), vec!["DROP TABLE"]);
        assert!(ev.contains(&Ev::Notice(Severity::Notice, "00000")));
        assert_eq!(errors(&sql(&mut s, "drop table d")), vec!["42P01"]);
    }

    #[test]
    fn if_not_exists_and_duplicates() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table x (a int)");
        assert_eq!(
            errors(&sql(&mut s, "create table x (a int)")),
            vec!["42P07"]
        );
        let ev = sql(&mut s, "create table if not exists x (a int)");
        assert_eq!(tags(&ev), vec!["CREATE TABLE"]);
        assert!(ev.contains(&Ev::Notice(Severity::Notice, "42P07")));
        let ev = sql(&mut s, "create table pg_catalog.zz (a int)");
        assert_eq!(errors(&ev), vec!["42501"]);
    }

    #[test]
    fn failed_block_aborts_and_releases_writer() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table f (a int not null)");
        sql(&mut s, "begin");
        sql(&mut s, "insert into f values (1)");
        assert_eq!(
            errors(&sql(&mut s, "insert into f values (null)")),
            vec!["23502"]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        assert_eq!(errors(&sql(&mut s, "select 1")), vec!["25P02"]);
        sql(&mut s, "commit");
        assert_eq!(count(&mut s, "f"), 0);
        let mut s2 = tc.session("postgres").unwrap();
        assert_eq!(
            tags(&sql(&mut s2, "insert into f values (5)")),
            vec!["INSERT 0 1"]
        );
    }

    #[test]
    fn writer_lock_is_exclusive_and_readers_see_only_committed() {
        let (tc, mut s1) = cl();
        let mut s2 = tc.session("postgres").unwrap();
        sql(&mut s1, "create table w (a int)");
        sql(&mut s1, "begin");
        sql(&mut s1, "insert into w values (1)");
        // Readers neither wait nor see the uncommitted row.
        assert_eq!(count(&mut s2, "w"), 0);
        // A second writer times out.
        sql(&mut s2, "set lock_timeout = '50ms'");
        assert_eq!(
            errors(&sql(&mut s2, "insert into w values (2)")),
            vec!["55P03"]
        );
        assert_eq!(s2.transaction_status(), TransactionStatus::Idle);
        sql(&mut s1, "commit");
        assert_eq!(count(&mut s2, "w"), 1);
        assert_eq!(
            tags(&sql(&mut s2, "insert into w values (2)")),
            vec!["INSERT 0 1"]
        );
    }

    #[test]
    fn ddl_is_visible_to_other_sessions_after_commit() {
        let (tc, mut s1) = cl();
        let mut s2 = tc.session("postgres").unwrap();
        // Fill s2's view of the cache with the absence and presence.
        assert_eq!(errors(&sql(&mut s2, "select * from n")), vec!["42P01"]);
        sql(&mut s1, "create table n (a int)");
        sql(&mut s1, "insert into n values (7)");
        assert_eq!(one(&mut s2, "select a from n"), "7");
        sql(&mut s1, "drop table n");
        assert_eq!(errors(&sql(&mut s2, "select * from n")), vec!["42P01"]);
    }

    #[test]
    fn same_transaction_ddl_then_dml_and_update_halloween() {
        let (_tc, mut s) = cl();
        let ev = sql(
            &mut s,
            "begin; create table h (a int); insert into h values (1), (2); \
             update h set a = a + 1; insert into h select a from h; commit",
        );
        assert!(errors(&ev).is_empty(), "{ev:?}");
        assert_eq!(count(&mut s, "h"), 4);
        let ev = sql(&mut s, "select a from h order by a");
        let vals: Vec<_> = data(&ev)
            .into_iter()
            .map(|r| r[0].clone().unwrap())
            .collect();
        assert_eq!(vals, ["2", "2", "3", "3"]);
    }

    #[test]
    fn data_survives_clean_restart_and_checkpoint_crash() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table p (a int, b text)");
        sql(&mut s, "insert into p values (1, 'x'), (2, 'y')");
        assert_eq!(tags(&sql(&mut s, "checkpoint")), vec!["CHECKPOINT"]);
        s.terminate();
        drop(s);
        let tc = tc.restart().unwrap();
        let mut s = tc.session("postgres").unwrap();
        assert_eq!(count(&mut s, "p"), 2);
        sql(&mut s, "insert into p values (3, 'z')");
        sql(&mut s, "checkpoint");
        s.terminate();
        drop(s);
        let tc = tc
            .crash_and_restart(crate::storage::vfs::CrashMode::DropUnsynced)
            .unwrap();
        let mut s = tc.session("postgres").unwrap();
        assert_eq!(count(&mut s, "p"), 3);
        assert_eq!(one(&mut s, "select b from p where a = 3"), "z");
    }

    #[test]
    fn terminate_in_a_block_releases_everything() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table r (a int)");
        sql(&mut s, "begin");
        sql(&mut s, "insert into r values (1)");
        s.terminate();
        let mut s2 = tc.session("postgres").unwrap();
        assert_eq!(
            tags(&sql(&mut s2, "insert into r values (2)")),
            vec!["INSERT 0 1"]
        );
        assert_eq!(count(&mut s2, "r"), 1);
    }

    #[test]
    fn interrupt_stops_a_statement_with_fatal() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table i (a int)");
        sql(&mut s, "insert into i values (1), (2)");
        s.interrupt_flag().request_terminate();
        let ev = sql(&mut s, "select * from i");
        assert_eq!(errors(&ev), vec!["57P01"]);
        assert!(s.is_closing());
    }

    #[test]
    fn user_function_error_and_system_columns() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table sc (a int)");
        sql(&mut s, "insert into sc values (1)");
        assert_eq!(one(&mut s, "select ctid from sc"), "(0,1)");
        assert_eq!(
            one(
                &mut s,
                "select 1 from pg_database where datname = current_database()"
            ),
            "1"
        );
    }

    /// Number of files in the `postgres` database directory.
    fn rel_files(tc: &TestCluster) -> usize {
        let vfs: &dyn crate::storage::vfs::Vfs = &tc.vfs;
        vfs.read_dir(std::path::Path::new("base/5")).unwrap().len()
    }
}
