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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::analyzer::bound::{BoundExplain, BoundStatement};
use crate::analyzer::{self, OutputColumn};
use crate::catalog::CatalogReader;
use crate::catalog::names::CatalogNames;
use crate::catalog::reader::StatementCatalog;
use crate::copy::{self, CopyIn};
use crate::engine::{Cluster, DatabaseHandle};
use crate::error::{Error, Result, Severity, SqlState, sqlstate};
use crate::executor::instrument::Instrumentation;
use crate::executor::seq::{SeqRuntime, SeqSession};
use crate::executor::{self, ExecCtx, RuntimeInfo, SessionInfo};
use crate::interrupt::InterruptFlag;
use crate::planner::physical::PhysicalQuery;
use crate::planner::{self, PlannerSettings};
use crate::settings::{Isolation, Settings, TxnCharacteristics};
use crate::sql::{
    self,
    ast::{
        ParamTarget, SetArg, SetStmt, SetTransaction, SetValue, ShowStmt, Statement,
        TransactionKind, TransactionMode, TransactionStmt,
    },
};
use crate::storage::buffer;
use crate::storage::smgr::RelFileLocator;
use crate::storage::{SequenceStore, WriteCtx};
use crate::txn::{Snapshot, Transaction, TxnManager, WaitCtl, WriterGuard, Xid};
use crate::types::{Datum, Oid, SqlType, TypeEnv, io, oid};

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
    pub(crate) fn new(severity: Severity, sqlstate: SqlState, message: impl Into<String>) -> Self {
        Notice {
            severity,
            sqlstate,
            message: message.into(),
            detail: None,
            hint: None,
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
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
    /// `CopyInResponse`（`G`）。`format` は全体の形式（0 = text）、`column_formats` は列ごとの形式
    /// （`i16`。`m4/11` の C-29）。COPY FROM STDIN を扱えないシンクは既定のまま（エラーを返す）。
    fn copy_in_response(&mut self, format: u8, column_formats: &[i16]) -> std::io::Result<()> {
        let _ = (format, column_formats);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this sink does not support COPY FROM STDIN",
        ))
    }
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

/// `exec_statement` の結果。`Tag` はコマンドタグ、`CopyIn` は `COPY FROM STDIN` の開始
/// （`CopyInResponse` を送って `CopyData` を待つ。`m4/10` §5.3）。
enum ExecOutcome {
    Tag(String),
    CopyIn(Box<CopyIn>),
}

/// 同じ `Query` メッセージの、まだ実行していない文（COPY の `CopyDone` の後に再開する）。
#[derive(Debug)]
struct PendingQuery {
    /// 元の SQL（エラー位置の解決に使う）。
    sql: String,
    /// 同じ `Query` メッセージの文すべて。
    stmts: Vec<Statement>,
    /// `CopyDone` の後に実行する次の文の添字。
    next: usize,
}

/// 進行中の COPY FROM STDIN。
#[derive(Debug)]
struct CopyState {
    copy: CopyIn,
    pending: PendingQuery,
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
    /// Start of the statement being run (`statement_timestamp()`); 0 before the first one.
    stmt_started_at: i64,
    interrupt: Arc<InterruptFlag>,
    /// Characteristics of the current transaction (`m3.md` §6.11.1); the
    /// defaults come from `default_transaction_*` when the transaction starts.
    chars: TxnCharacteristics,
    /// Characteristics the current block started with (defaults, or those
    /// restored by AND CHAIN). A failed block's abort reverts SET TRANSACTION
    /// changes to these, and AND CHAIN carries them over.
    start_chars: TxnCharacteristics,
    /// A statement that references a table has run in this transaction
    /// (PostgreSQL's `FirstSnapshotSet`; `SELECT 1` does not count).
    txn_snapshot_taken: bool,
    /// シーケンスの状態（`currval` / `lastval` と先取りした値。`m4/08` §4.4）。
    seq_state: RefCell<SeqSession>,
    /// 進行中の COPY FROM STDIN（`m4/10` §5.3）。
    copy: Option<Box<CopyState>>,
}

impl Session {
    /// Creates a session. `Cluster::connect` rejects an unknown database
    /// (`3D000`), a database that does not accept connections (`55000`) and
    /// an unknown role (`28000`) as FATAL errors.
    pub fn new(cluster: Arc<Cluster>, params: StartupParams) -> Result<Session> {
        let (db, role) = cluster.connect(&params.database, &params.user)?;
        let id = cluster.next_session_id();
        if i32::try_from(id).is_err() {
            return Err(Error::new(
                sqlstate::TOO_MANY_CONNECTIONS,
                "sorry, too many clients already",
            )
            .with_severity(Severity::Fatal));
        }
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
            stmt_started_at: 0,
            params,
            state: TxState::Idle,
            txn: Transaction::new(),
            settings,
            reported,
            interrupt: Arc::new(InterruptFlag::default()),
            chars: TxnCharacteristics::default(),
            start_chars: TxnCharacteristics::default(),
            txn_snapshot_taken: false,
            seq_state: RefCell::new(SeqSession::new()),
            copy: None,
        }
    }

    /// Whether `client_encoding` is currently LATIN1 (otherwise UTF8).
    pub fn client_encoding_is_latin1(&self) -> bool {
        self.settings.get("client_encoding") == "LATIN1"
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
        self.copy = None;
        self.interrupt.set_statement_deadline(None);
        if self.state != TxState::Idle {
            self.rollback_transaction();
        }
    }

    /// A FATAL error was reported: the server must close the connection
    /// after sending the pending messages.
    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// The process ID shown to the client (`BackendKeyData`,
    /// `pg_backend_pid()`): the session ID as an `i32`.
    pub fn backend_pid(&self) -> i32 {
        i32::try_from(self.id).unwrap_or(i32::MAX)
    }

    /// How long the connection may stay idle in the current state before the
    /// server must end it (`m3.md` §5.10): `idle_in_transaction_session_timeout`
    /// in a block, `idle_session_timeout` otherwise. `None` means no limit.
    pub fn idle_timeout(&self) -> Option<Duration> {
        // COPY の途中は「アイドル」ではない（`m4/10` §5.3）。
        if self.copy.is_some() {
            return None;
        }
        match self.state {
            TxState::Block | TxState::Failed => self.settings.idle_in_transaction_session_timeout(),
            TxState::Idle => self.settings.idle_session_timeout(),
            TxState::Implicit => None,
        }
    }

    /// The FATAL error to send when [`Session::idle_timeout`] expires. The
    /// caller then calls [`Session::terminate`] and closes the connection.
    pub fn idle_timeout_error(&self) -> Error {
        let (state, msg) = if self.state == TxState::Idle {
            (
                sqlstate::IDLE_SESSION_TIMEOUT,
                "terminating connection due to idle-session timeout",
            )
        } else {
            (
                sqlstate::IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
                "terminating connection due to idle-in-transaction timeout",
            )
        };
        Error::new(state, msg).with_severity(Severity::Fatal)
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
        // A cancel that arrived while idle is dropped (`m3.md` §5.2 step 0).
        self.interrupt.clear_cancel();
        let r = self.run_statements(sql, parsed, sink);
        if r.is_err() {
            // The client is gone part-way through the message: neither a COPY
            // nor the implicit transaction may survive.
            self.copy = None;
            self.interrupt.set_statement_deadline(None);
            if self.state == TxState::Implicit {
                self.rollback_transaction();
            }
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
        self.run_from(
            PendingQuery {
                sql: sql.to_owned(),
                stmts,
                next: 0,
            },
            sink,
        )
    }

    /// Runs `pending.stmts[pending.next..]`. A `COPY ... FROM STDIN` sends the
    /// `CopyInResponse`, stores the rest of the message in `self.copy` and
    /// returns without `CommandComplete`, the implicit commit or
    /// `ReadyForQuery` (`copy_done` resumes from here; `m4/10` §5.3).
    fn run_from(
        &mut self,
        pending: PendingQuery,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        let PendingQuery { sql, stmts, next } = pending;
        let mut i = next;
        while i < stmts.len() {
            let stmt = &stmts[i];
            if self.state == TxState::Failed && !allowed_when_failed(stmt) {
                let e = Error::new(sqlstate::IN_FAILED_SQL_TRANSACTION, IN_FAILED_MSG);
                return self.report_error(e, &sql, sink);
            }
            self.stmt_started_at = pg_now_micros();
            if self.state == TxState::Idle {
                self.begin_transaction(TxState::Implicit);
            }
            let mut out = Output::default();
            // `statement_timeout` is per statement (`m3.md` D31).
            self.interrupt.set_statement_deadline(
                self.settings
                    .statement_timeout()
                    .and_then(|d| Instant::now().checked_add(d)),
            );
            let result = self.exec_statement(stmt, &mut out);
            if let Ok(ExecOutcome::CopyIn(_)) = &result {
                // The deadline stays: it covers the whole COPY.
            } else {
                self.interrupt.set_statement_deadline(None);
            }
            self.send_output(&out, sink)?;
            match result {
                Ok(ExecOutcome::Tag(tag)) => sink.command_complete(&tag)?,
                Ok(ExecOutcome::CopyIn(copy)) => {
                    let formats = vec![0i16; copy.ncols()];
                    sink.copy_in_response(0, &formats)?;
                    // PostgreSQL checks FREEZE after it sent the `G`.
                    if let Err(e) = copy.check_freeze(&self.txn) {
                        self.interrupt.set_statement_deadline(None);
                        return self.report_error(e, &sql, sink);
                    }
                    self.copy = Some(Box::new(CopyState {
                        copy: *copy,
                        pending: PendingQuery {
                            sql,
                            stmts,
                            next: i + 1,
                        },
                    }));
                    return Ok(());
                }
                Err(e) => return self.report_error(e, &sql, sink),
            }
            i += 1;
        }
        if self.state == TxState::Implicit
            && let Err(e) = self.commit_transaction()
        {
            return self.report_error(e, &sql, sink);
        }
        self.flush_parameter_status(sink)
    }

    // ----- COPY FROM STDIN (`m4/10` §5.3) ---------------------------------------

    /// Whether the server must read `CopyData` / `CopyDone` / `CopyFail` next
    /// (after `execute_simple` or `copy_done`).
    pub fn is_copying_in(&self) -> bool {
        self.copy.is_some()
    }

    /// One `CopyData`: processes and inserts the rows it completes. On an
    /// error the session reports it and leaves the COPY (the server sends
    /// `ReadyForQuery`). Ignored when no COPY is running.
    pub fn copy_data(&mut self, chunk: &[u8], sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let Some(mut st) = self.copy.take() else {
            return Ok(());
        };
        match self.copy_message(|ctx, w| st.copy.push_data(chunk, ctx, w)) {
            Ok(()) => {
                self.copy = Some(st);
                Ok(())
            }
            Err(e) => {
                self.interrupt.set_statement_deadline(None);
                self.report_error(e, &st.pending.sql, sink)
            }
        }
    }

    /// `CopyDone`: finishes the last row, sends `COPY n` and runs the rest of
    /// the message.
    pub fn copy_done(&mut self, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let Some(mut st) = self.copy.take() else {
            return Ok(());
        };
        let rows = self
            .copy_message(|ctx, w| st.copy.finish(ctx, w))
            .and_then(|n| self.txn.command_counter_increment().map(|()| n));
        self.interrupt.set_statement_deadline(None);
        match rows {
            Ok(n) => {
                sink.command_complete(&format!("COPY {n}"))?;
                self.run_from(st.pending, sink)
            }
            Err(e) => self.report_error(e, &st.pending.sql, sink),
        }
    }

    /// `CopyFail`: `57014`.
    pub fn copy_fail(&mut self, message: &str, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let Some(st) = self.copy.take() else {
            return Ok(());
        };
        self.interrupt.set_statement_deadline(None);
        let e = st.copy.fail(message);
        self.report_error(e, &st.pending.sql, sink)
    }

    /// Called by the server whenever its read for the next message times out
    /// (100 ms): reports a cancel, a stop request or the `statement_timeout`
    /// that happened while the client sent nothing.
    pub fn copy_poll(&mut self, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        if self.copy.is_none() {
            return Ok(());
        }
        let Err(e) = self.interrupt.check() else {
            return Ok(());
        };
        let Some(st) = self.copy.take() else {
            return Ok(());
        };
        self.interrupt.set_statement_deadline(None);
        self.report_error(e, &st.pending.sql, sink)
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
        self.txn.started_at = if self.stmt_started_at == 0 {
            pg_now_micros()
        } else {
            self.stmt_started_at
        };
        self.settings.begin();
        self.chars = self.settings.default_characteristics();
        self.start_chars = self.chars;
        self.txn_snapshot_taken = false;
        self.state = state;
    }

    /// Commits (`m2.md` §5.3). The session is idle afterwards even when this
    /// fails: a failure to record the commit is `Severity::Panic`.
    fn commit_transaction(&mut self) -> Result<()> {
        let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
        self.settings.commit();
        self.state = TxState::Idle;
        let Some(cluster) = self.cluster.as_ref() else {
            return Ok(());
        };
        let Some(xid) = txn.xid else {
            // No XID: nothing was written to the heap. A `nextval` may have
            // logged `SEQ_LOG` records, which must be durable before the
            // client sees the value (`m4/08` §5.3).
            if txn.wal_flush_upto == crate::wal::Lsn::default() {
                return Ok(());
            }
            Self::check_not_poisoned(cluster)?;
            return cluster.txn_manager().finish_without_xid(txn.wal_flush_upto);
        };
        if let Err(e) = Self::check_not_poisoned(cluster) {
            txn.writer = None;
            return Err(e);
        }
        let mgr = cluster.txn_manager();
        // 1. clog + running list. The cache is invalidated only after this.
        mgr_commit(mgr, xid, &txn.pending_unlinks)?;
        // 2.
        if txn.catalog_dirty {
            cluster.invalidate_all_catalog_caches();
        }
        // 3. Remove the files of dropped tables once no statement can be
        // reading them. A failure does not undo the commit.
        // 4. Release the writer lock first: waiting for the barrier must not
        // hold up other writers.
        txn.writer = None;
        if !txn.pending_unlinks.is_empty() {
            Self::unlink_files(cluster, &txn.pending_unlinks);
        }
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
        if let Err(e) = mgr_abort(cluster.txn_manager(), xid, &txn.pending_creates) {
            warn(&format!("could not record the abort: {}", e.message));
            cluster.poison();
        }
        txn.writer = None;
        if !txn.pending_creates.is_empty() {
            Self::unlink_files(cluster, &txn.pending_creates);
        }
    }

    /// Removes relation files under the exclusive storage barrier. The wait
    /// is never blocking: while another session runs a statement the files
    /// stay queued and the next statement end (of any session) retries.
    /// Errors are only warned about: the transaction is already decided.
    fn unlink_files(cluster: &Cluster, rels: &[RelFileLocator]) {
        cluster.txn_manager().defer_unlinks(rels);
        Self::flush_deferred_unlinks(cluster);
    }

    fn flush_deferred_unlinks(cluster: &Cluster) {
        let mgr = cluster.txn_manager();
        if !mgr.has_deferred_unlinks() {
            return;
        }
        let guard = match mgr.try_exclusive_barrier() {
            Ok(Some(g)) => g,
            Ok(None) => return,
            Err(e) => {
                warn(&format!("could not remove relation files: {}", e.message));
                return;
            }
        };
        for rel in mgr.take_deferred_unlinks() {
            if let Err(e) = cluster.storage().unlink_storage(rel) {
                warn(&format!(
                    "could not remove relation file {}: {}",
                    rel.rel_number.0, e.message
                ));
            }
        }
        drop(guard);
    }

    // ----- statements --------------------------------------------------------

    /// Executes one statement and returns its outcome (the command tag, or the
    /// start of a COPY).
    fn exec_statement(&mut self, stmt: &Statement, out: &mut Output) -> Result<ExecOutcome> {
        let tag = match stmt {
            Statement::Transaction(t) => self.exec_transaction(t, out)?,
            Statement::Set(s) => self.exec_set(s, out)?,
            Statement::Reset(r) => {
                match &r.target {
                    ParamTarget::All => self.settings.reset_all(),
                    ParamTarget::Name(n) if is_characteristic(n) => {
                        return Err(cannot_reset(n));
                    }
                    ParamTarget::Name(n) => self.settings.reset(n)?,
                }
                "RESET".into()
            }
            Statement::Show(s) => self.exec_show(s, out)?,
            Statement::Checkpoint(_) => {
                // No storage barrier may be held (`checkpoint::run` takes it).
                let cluster = self.cluster()?;
                cluster.checkpoint()?;
                "CHECKPOINT".into()
            }
            _ => return self.exec_data_statement(stmt, out),
        };
        Ok(ExecOutcome::Tag(tag))
    }

    fn cluster(&self) -> Result<Arc<Cluster>> {
        self.cluster
            .clone()
            .ok_or_else(|| Error::not_supported("this session has no cluster"))
    }

    fn check_not_poisoned(cluster: &Cluster) -> Result<()> {
        if cluster.is_poisoned() {
            return Err(Error::new(
                sqlstate::ADMIN_SHUTDOWN,
                "the server is in a failed state; restart it",
            )
            .with_severity(Severity::Fatal));
        }
        Ok(())
    }

    /// SELECT / VALUES / INSERT / UPDATE / DELETE / DDL / COPY / EXPLAIN
    /// (`m2.md` §5.2, `m4/02` §4.1).
    fn exec_data_statement(&mut self, stmt: &Statement, out: &mut Output) -> Result<ExecOutcome> {
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        Self::check_not_poisoned(&cluster)?;
        // 1. The writer lock comes before the snapshot, so that no other
        // writer's commit lands between the snapshot and our first write.
        let write_tag = write_statement_tag(stmt);
        // A read-only transaction is rejected before the writer lock is
        // taken. PostgreSQL checks utility statements (CREATE / DROP TABLE,
        // COPY, ...) up front, but INSERT / UPDATE / DELETE (also under
        // EXPLAIN ANALYZE) only at executor start, i.e. after analysis and
        // planning (`run_bound`).
        let read_only_write = write_tag.filter(|_| self.chars.read_only);
        if let Some(tag) = read_only_write
            && !matches!(
                stmt,
                Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_)
                    | Statement::Explain(_)
            )
        {
            return Err(read_only_error(tag));
        }
        let mgr = Arc::clone(cluster.txn_manager());
        if write_tag.is_some() && read_only_write.is_none() && self.txn.writer.is_none() {
            let (xid, guard) =
                mgr_begin_write(&mgr, self.id, self.settings.lock_timeout(), &self.interrupt)?;
            self.txn.xid = Some(xid);
            self.txn.writer = Some(guard);
            // The cluster may have been poisoned while we waited for the lock.
            Self::check_not_poisoned(&cluster)?;
        }
        // 2. The shared storage barrier for the duration of the statement.
        let barrier = mgr.statement_barrier()?;
        if let Err(e) = Self::check_not_poisoned(&cluster) {
            drop(barrier);
            return Err(e);
        }
        buffer::track::barrier_acquired();
        let result = self.run_under_barrier(stmt, &cluster, &db, read_only_write, out);
        buffer::track::barrier_released();
        drop(barrier);
        // The values this statement handed out are covered by the `SEQ_LOG`
        // records it wrote, success or not (`m4/08` §5.3).
        let lsn = self.seq_state.borrow_mut().end_statement();
        self.txn.note_wal(lsn);
        Self::flush_deferred_unlinks(&cluster);
        // 8.
        buffer::assert_no_pins();
        let outcome = result?;
        // 9. A COPY is one command: `copy_done` increments the counter.
        if matches!(outcome, ExecOutcome::Tag(_)) {
            self.txn.command_counter_increment()?;
        }
        Ok(outcome)
    }

    /// Steps 3 to 6 of `m2.md` §5.2.
    fn run_under_barrier(
        &mut self,
        stmt: &Statement,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        read_only_write: Option<&'static str>,
        out: &mut Output,
    ) -> Result<ExecOutcome> {
        let span = stmt.span();
        self.with_stmt_env(cluster, db, |txn, env| {
            Self::run_bound(txn, env, stmt, span, read_only_write, out)
        })
    }

    /// Builds the per-statement environment (snapshot, catalog, `TypeEnv`,
    /// planner settings, the `RuntimeInfo`) and calls `f` with it and the
    /// transaction. What `set_config` changed in `f` is kept when it succeeds.
    fn with_stmt_env<R>(
        &mut self,
        cluster: &Cluster,
        db: &Arc<DatabaseHandle>,
        f: impl FnOnce(&mut Transaction, &StmtEnv<'_>) -> Result<R>,
    ) -> Result<R> {
        // 3. The generation is read before the snapshot.
        let generation = db.cache.generation();
        // 4.
        // Any statement but SET / SHOW / BEGIN / COMMIT / CHECKPOINT counts as
        // having used a snapshot (PostgreSQL 17, also for `SELECT 1`).
        self.txn_snapshot_taken = true;
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
        let info = self.session_info();
        let interrupts = Arc::clone(&self.interrupt);
        let zones = cluster.zones();
        let datetime = self.settings.datetime_settings(zones);
        let names = CatalogNames(&catalog);
        let type_env = self
            .settings
            .type_env(&datetime, zones, self.txn.started_at, Some(&names));
        let planner_settings = self.settings.planner_settings();
        let runtime = SessionRuntime {
            pid: self.backend_pid(),
            mgr: Some(Arc::clone(cluster.txn_manager())),
            interrupts: Arc::clone(&interrupts),
            xid: self.txn.xid.map(|x| x.0),
            stmt_started_at: self.stmt_started_at,
            base: &self.settings,
            changed: RefCell::new(None),
            chars: self.chars,
            seq: &self.seq_state,
            seq_store: &**cluster.sequences(),
            catalog: &catalog,
        };
        let env = StmtEnv {
            cluster,
            db,
            catalog: &catalog,
            snapshot: &snap,
            info: &info,
            runtime: &runtime,
            type_env: &type_env,
            interrupts: &interrupts,
            planner: &planner_settings,
            role_oid: self.role_oid,
            in_transaction_block: self.in_block_for_set(),
        };
        let r = f(&mut self.txn, &env)?;
        let changed = runtime.changed.into_inner();
        if let Some(s) = changed {
            // Keep what `set_config` changed.
            self.settings = s;
        }
        Ok(r)
    }

    /// Step 6 of `m2.md` §5.2: analyze, then execute by the kind of statement
    /// (`m4/02` §4.1).
    fn run_bound(
        txn: &mut Transaction,
        env: &StmtEnv<'_>,
        stmt: &Statement,
        span: crate::error::Span,
        read_only_write: Option<&'static str>,
        out: &mut Output,
    ) -> Result<ExecOutcome> {
        let bound = match stmt {
            Statement::Copy(c) => BoundStatement::Copy(copy::analyze_copy(env.catalog, c)?),
            other => analyzer::analyze(other, env.catalog)?,
        };
        match bound {
            BoundStatement::Checkpoint => Err(Error::internal(
                "CHECKPOINT must be handled before the storage barrier",
            )),
            BoundStatement::Ddl(ddl) => {
                let mut ctx = crate::ddl::DdlCtx {
                    cluster: env.cluster,
                    db: env.db,
                    snapshot: env.snapshot,
                    catalog: env.catalog,
                    txn,
                    role_oid: env.role_oid,
                    notices: &mut out.notices,
                    type_env: env.type_env,
                    in_transaction_block: env.in_transaction_block,
                    interrupts: env.interrupts,
                };
                crate::ddl::execute(&mut ctx, ddl).map(ExecOutcome::Tag)
            }
            BoundStatement::Copy(c) => {
                // The storage barrier is released by the caller: nothing is
                // held while the server waits for `CopyData`.
                copy::begin(&c).map(|c| ExecOutcome::CopyIn(Box::new(c)))
            }
            BoundStatement::Explain(e) => {
                Self::exec_explain(txn, env, *e, span, read_only_write, out).map(ExecOutcome::Tag)
            }
            mut bound => {
                env.fold_constants(&mut bound)?;
                let query = env.plan(&bound, false, false)?;
                // PostgreSQL rejects at executor start: after analysis and planning.
                if let Some(tag) = read_only_write {
                    return Err(read_only_error(tag));
                }
                let returns_rows = bound.returns_rows();
                out.columns = returns_rows.then(|| column_descs(&query.output, env.catalog));
                let run = env.run(txn, &query, returns_rows, None)?;
                out.rows = run.rows;
                let n = run.affected;
                Ok(ExecOutcome::Tag(match bound {
                    BoundStatement::Select(_) => format!("SELECT {}", out.rows.len()),
                    BoundStatement::Insert(_) => format!("INSERT 0 {n}"),
                    BoundStatement::Update(_) => format!("UPDATE {n}"),
                    BoundStatement::Delete(_) => format!("DELETE {n}"),
                    _ => unreachable!("handled above"),
                }))
            }
        }
    }

    /// `EXPLAIN [ANALYZE]` (`m4/10` §3.10).
    fn exec_explain(
        txn: &mut Transaction,
        env: &StmtEnv<'_>,
        e: BoundExplain,
        span: crate::error::Span,
        read_only_write: Option<&'static str>,
        out: &mut Output,
    ) -> Result<String> {
        let BoundExplain { options, mut inner } = e;
        // 1. Planning is timed for `Planning Time`.
        let planning_start = Instant::now();
        env.fold_constants(&mut inner)?;
        let query = env.plan(&inner, true, options.verbose)?;
        let planning = planning_start.elapsed();
        // 2. Without ANALYZE nothing runs and nothing is written (the writer
        // lock was not even taken: `write_statement_tag`).
        let analyzed = if options.analyze {
            // PostgreSQL rejects a write in a read-only transaction at
            // executor start.
            if let Some(tag) = read_only_write {
                return Err(read_only_error(tag));
            }
            // 3. Run it for real; the rows are thrown away.
            let ids = planner::physical::assign_exec_ids(&query);
            let instr = Rc::new(Instrumentation::with_timing(ids.total, options.timing));
            let start = Instant::now();
            env.run(txn, &query, false, Some(&instr))?;
            let elapsed = start.elapsed();
            instr.finish();
            Some((instr, elapsed))
        } else {
            None
        };
        // 4. Render.
        let lines = render_explain(options, &query, analyzed.as_ref(), planning, span)?;
        out.columns = Some(vec![text_column("QUERY PLAN")]);
        out.rows = lines.into_iter().map(|l| vec![Some(l)]).collect();
        Ok("EXPLAIN".into())
    }

    /// One `CopyData` / `CopyDone` for the running COPY: takes the shared
    /// storage barrier, the snapshot and the catalog for the message only
    /// (the writer lock is already held) and calls `f` with an `ExecCtx`
    /// whose plan is empty (`m4/10` §5.3).
    fn copy_message<R>(
        &mut self,
        f: impl FnOnce(&mut ExecCtx<'_>, &WriteCtx) -> Result<R>,
    ) -> Result<R> {
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        Self::check_not_poisoned(&cluster)?;
        let mgr = Arc::clone(cluster.txn_manager());
        let barrier = mgr.statement_barrier()?;
        if let Err(e) = Self::check_not_poisoned(&cluster) {
            drop(barrier);
            return Err(e);
        }
        buffer::track::barrier_acquired();
        let result = self.with_stmt_env(&cluster, &db, |txn, env| {
            let w = txn.write_ctx()?;
            let empty = PhysicalQuery::empty();
            let mut ctx = ExecCtx::new(env.exec_env(None), txn, &empty);
            f(&mut ctx, &w)
        });
        buffer::track::barrier_released();
        drop(barrier);
        let lsn = self.seq_state.borrow_mut().end_statement();
        self.txn.note_wal(lsn);
        Self::flush_deferred_unlinks(&cluster);
        buffer::assert_no_pins();
        result
    }

    fn exec_transaction(&mut self, t: &TransactionStmt, out: &mut Output) -> Result<String> {
        match &t.kind {
            TransactionKind::Begin | TransactionKind::StartTransaction => {
                // An unsupported level fails before anything changes.
                for m in &t.modes {
                    if let TransactionMode::IsolationLevel(l) = m {
                        Isolation::parse("transaction_isolation", l)?;
                    }
                }
                let was_block = self.state == TxState::Block;
                // The modes apply even to a block that was already open. A
                // failing mode leaves the state unchanged (the block has not
                // started yet).
                if was_block {
                    out.notices.push(Notice::new(
                        Severity::Warning,
                        sqlstate::ACTIVE_SQL_TRANSACTION,
                        "there is already a transaction in progress",
                    ));
                }
                self.apply_modes(&t.modes)?;
                if !was_block {
                    // Statements earlier in this Query message become part
                    // of the block, as in PostgreSQL.
                    self.state = TxState::Block;
                }
                Ok(if t.kind == TransactionKind::Begin {
                    "BEGIN"
                } else {
                    "START TRANSACTION"
                }
                .into())
            }
            TransactionKind::Commit | TransactionKind::End => {
                self.end_transaction(true, t.chain, out)
            }
            TransactionKind::Rollback | TransactionKind::Abort => {
                self.end_transaction(false, t.chain, out)
            }
            // Sub-transactions do not exist yet; the errors follow
            // PostgreSQL's order (`m3.md` §6.11.3). A failed block never
            // gets here for SAVEPOINT / RELEASE (25P02 comes first).
            TransactionKind::Savepoint(_) => {
                if self.state == TxState::Block {
                    Err(Error::not_supported("SAVEPOINT is not supported yet"))
                } else {
                    Err(not_in_block("SAVEPOINT"))
                }
            }
            TransactionKind::Release(name) => {
                if self.state == TxState::Block {
                    Err(no_such_savepoint(name))
                } else {
                    Err(not_in_block("RELEASE SAVEPOINT"))
                }
            }
            TransactionKind::RollbackTo(name) => {
                if matches!(self.state, TxState::Block | TxState::Failed) {
                    Err(no_such_savepoint(name))
                } else {
                    Err(not_in_block("ROLLBACK TO SAVEPOINT"))
                }
            }
        }
    }

    /// COMMIT / END / ROLLBACK / ABORT, with or without AND CHAIN
    /// (`m3.md` §6.11.2). A failed block ends with the tag `ROLLBACK`; the
    /// chained block inherits the characteristics the failed block started
    /// with: the abort reverts SET TRANSACTION changes made inside it, but not
    /// what AND CHAIN restored (PostgreSQL 17).
    fn end_transaction(&mut self, commit: bool, chain: bool, out: &mut Output) -> Result<String> {
        let tag = if commit { "COMMIT" } else { "ROLLBACK" };
        if self.state == TxState::Failed {
            let saved = self.start_chars;
            self.rollback_transaction();
            if chain {
                self.begin_transaction(TxState::Block);
                self.chars = saved;
                self.start_chars = saved;
            }
            return Ok("ROLLBACK".into());
        }
        if self.state != TxState::Block {
            if chain {
                return Err(not_in_block(&format!("{tag} AND CHAIN")));
            }
            out.notices.push(no_transaction_warning());
        }
        let saved = self.chars;
        if commit {
            self.commit_transaction()?;
        } else {
            self.rollback_transaction();
        }
        if chain {
            self.begin_transaction(TxState::Block);
            self.chars = saved;
            self.start_chars = saved;
        }
        Ok(tag.into())
    }

    /// Whether `SET LOCAL` / `SET TRANSACTION` are allowed: in an explicit
    /// block, or in the implicit block of a multi-statement Query
    /// (PostgreSQL's `TBLOCK_IMPLICIT_INPROGRESS`).
    fn in_block_for_set(&self) -> bool {
        self.state == TxState::Block || (self.state == TxState::Implicit && self.multi_statement)
    }

    fn apply_modes(&mut self, modes: &[TransactionMode]) -> Result<()> {
        modes.iter().try_for_each(|m| self.apply_mode(m))
    }

    /// Changes one characteristic of the current transaction. The checks
    /// are PostgreSQL's `check_XactIsoLevel` and friends: they only apply
    /// when the value changes, and only after a statement has used a
    /// snapshot (`txn_snapshot_taken`).
    fn apply_mode(&mut self, mode: &TransactionMode) -> Result<()> {
        let too_late = |what: &str| {
            Error::new(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                format!("{what} must be called before any query"),
            )
        };
        match mode {
            TransactionMode::IsolationLevel(level) => {
                let iso = match Isolation::parse("transaction_isolation", level) {
                    Ok(iso) => iso,
                    // Valid but not supported yet: it differs from the current
                    // level, so PostgreSQL's 25001 comes first.
                    Err(e)
                        if e.sqlstate == sqlstate::FEATURE_NOT_SUPPORTED
                            && self.txn_snapshot_taken =>
                    {
                        return Err(too_late("SET TRANSACTION ISOLATION LEVEL"));
                    }
                    Err(e) => return Err(e),
                };
                if iso != self.chars.isolation && self.txn_snapshot_taken {
                    return Err(too_late("SET TRANSACTION ISOLATION LEVEL"));
                }
                self.chars.isolation = iso;
            }
            TransactionMode::ReadOnly => self.chars.read_only = true,
            TransactionMode::ReadWrite => {
                if self.chars.read_only && self.txn_snapshot_taken {
                    return Err(Error::new(
                        sqlstate::ACTIVE_SQL_TRANSACTION,
                        "transaction read-write mode must be set before any query",
                    ));
                }
                self.chars.read_only = false;
            }
            TransactionMode::Deferrable => {
                if self.txn_snapshot_taken {
                    return Err(too_late("SET TRANSACTION [NOT] DEFERRABLE"));
                }
                self.chars.deferrable = true;
            }
            TransactionMode::NotDeferrable => {
                if self.txn_snapshot_taken {
                    return Err(too_late("SET TRANSACTION [NOT] DEFERRABLE"));
                }
                self.chars.deferrable = false;
            }
        }
        Ok(())
    }

    /// `SET SESSION CHARACTERISTICS AS TRANSACTION ...`: changes the
    /// `default_transaction_*` parameters.
    fn set_session_characteristics(&mut self, t: &SetTransaction) -> Result<String> {
        for mode in &t.modes {
            let (name, value) = match mode {
                TransactionMode::IsolationLevel(l) => ("default_transaction_isolation", l.as_str()),
                TransactionMode::ReadOnly => ("default_transaction_read_only", "on"),
                TransactionMode::ReadWrite => ("default_transaction_read_only", "off"),
                TransactionMode::Deferrable => ("default_transaction_deferrable", "on"),
                TransactionMode::NotDeferrable => ("default_transaction_deferrable", "off"),
            };
            self.settings.set(name, Some(&[value.to_owned()]), false)?;
        }
        Ok("SET".into())
    }

    /// `SET transaction_isolation / transaction_read_only /
    /// transaction_deferrable`: the same as the matching `SET TRANSACTION`
    /// mode. (`RESET` / `SET ... TO DEFAULT` are rejected, as in PostgreSQL.)
    fn set_characteristic(&mut self, name: &str, args: &[String]) -> Result<()> {
        let name = name.to_ascii_lowercase();
        let value = self.settings.validate(&name, args)?;
        let on = value == "on";
        let mode = match name.as_str() {
            "transaction_isolation" => TransactionMode::IsolationLevel(value),
            "transaction_read_only" if on => TransactionMode::ReadOnly,
            "transaction_read_only" => TransactionMode::ReadWrite,
            _ if on => TransactionMode::Deferrable,
            _ => TransactionMode::NotDeferrable,
        };
        self.apply_mode(&mode)
    }

    /// The current value of a `transaction_*` parameter, which the session
    /// (not `Settings`) owns.
    fn characteristic_value(&self, name: &str) -> Option<String> {
        match name.to_ascii_lowercase().as_str() {
            "transaction_isolation" => Some(self.chars.isolation.as_str().to_owned()),
            "transaction_read_only" => Some(on_off(self.chars.read_only).to_owned()),
            "transaction_deferrable" => Some(on_off(self.chars.deferrable).to_owned()),
            _ => None,
        }
    }

    fn exec_set(&mut self, s: &SetStmt, out: &mut Output) -> Result<String> {
        let in_block = self.in_block_for_set();
        if s.constraints {
            if !in_block {
                out.notices.push(Notice::new(
                    Severity::Warning,
                    sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                    "SET CONSTRAINTS can only be used in transaction blocks",
                ));
            }
            return Ok("SET CONSTRAINTS".into());
        }
        if let Some(t) = &s.transaction {
            if t.session_characteristics {
                return self.set_session_characteristics(t);
            }
            if !in_block {
                out.notices.push(Notice::new(
                    Severity::Warning,
                    sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                    "SET TRANSACTION can only be used in transaction blocks",
                ));
                return Ok("SET".into());
            }
            self.apply_modes(&t.modes)?;
            return Ok("SET".into());
        }
        if s.local && !in_block {
            out.notices.push(Notice::new(
                Severity::Warning,
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET LOCAL can only be used in transaction blocks",
            ));
            // Still validate the name, as PostgreSQL does.
            if !crate::settings::is_known(&s.name) && !s.name.contains('.') {
                return Err(Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("unrecognized configuration parameter \"{}\"", s.name),
                ));
            }
            self.settings.declare_custom(&s.name);
            if let SetValue::Values(args) = &s.value
                && crate::settings::is_known(&s.name)
            {
                let texts: Vec<String> = args
                    .iter()
                    .map(|a| match a {
                        SetArg::Word(w) | SetArg::String(w) | SetArg::Number(w) => w.clone(),
                    })
                    .collect();
                self.settings.validate(&s.name, &texts)?;
            }
            return Ok("SET".into());
        }
        let texts: Option<Vec<String>> = match &s.value {
            SetValue::Default => None,
            SetValue::Values(args) => Some(
                args.iter()
                    .map(|a| match a {
                        SetArg::Word(w) | SetArg::String(w) | SetArg::Number(w) => w.clone(),
                    })
                    .collect(),
            ),
        };
        if is_characteristic(&s.name) {
            let Some(texts) = texts.as_deref() else {
                return Err(cannot_reset(&s.name));
            };
            self.set_characteristic(&s.name, texts)?;
        } else {
            self.settings.set(&s.name, texts.as_deref(), s.local)?;
        }
        Ok("SET".into())
    }

    fn exec_show(&mut self, s: &ShowStmt, out: &mut Output) -> Result<String> {
        match &s.target {
            ParamTarget::Name(n) => {
                let (name, value) = match self.characteristic_value(n) {
                    Some(v) => (n.to_ascii_lowercase(), v),
                    None => self.settings.show(n)?,
                };
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
                    let v = self.characteristic_value(&n).unwrap_or(v);
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

/// What a failed transaction still accepts: the statements that end it,
/// and `ROLLBACK TO` (`m3.md` §6.11.3).
fn allowed_when_failed(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Transaction(t) if matches!(
            t.kind,
            TransactionKind::Commit
                | TransactionKind::End
                | TransactionKind::Rollback
                | TransactionKind::Abort
                | TransactionKind::RollbackTo(_)
        )
    )
}

fn read_only_error(tag: &str) -> Error {
    Error::new(
        sqlstate::READ_ONLY_SQL_TRANSACTION,
        format!("cannot execute {tag} in a read-only transaction"),
    )
}

fn cannot_reset(name: &str) -> Error {
    Error::not_supported(format!(
        "parameter \"{}\" cannot be reset",
        name.to_ascii_lowercase()
    ))
}

fn not_in_block(what: &str) -> Error {
    Error::new(
        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
        format!("{what} can only be used in transaction blocks"),
    )
}

fn no_such_savepoint(name: &str) -> Error {
    Error::new(
        sqlstate::INVALID_SAVEPOINT_SPECIFICATION,
        format!("savepoint \"{name}\" does not exist"),
    )
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// `transaction_isolation` and friends: the parameters of the current
/// transaction, owned by the session.
fn is_characteristic(name: &str) -> bool {
    [
        "transaction_isolation",
        "transaction_read_only",
        "transaction_deferrable",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
}

/// The command tag used in `25006` for a statement that writes, judged on
/// the raw parse tree (`m3.md` §5.2 d, `m4/07` §5.0). `VACUUM` / `ANALYZE`
/// are not writing statements; `EXPLAIN ANALYZE` of a DML statement is.
fn write_statement_tag(stmt: &Statement) -> Option<&'static str> {
    match stmt {
        Statement::Insert(_) => Some("INSERT"),
        Statement::Update(_) => Some("UPDATE"),
        Statement::Delete(_) => Some("DELETE"),
        Statement::CreateTable(_) => Some("CREATE TABLE"),
        Statement::DropTable(_) => Some("DROP TABLE"),
        Statement::CreateIndex(_) => Some("CREATE INDEX"),
        Statement::DropIndex(_) => Some("DROP INDEX"),
        Statement::AlterTable(_) => Some("ALTER TABLE"),
        Statement::Truncate(_) => Some("TRUNCATE TABLE"),
        Statement::CreateSequence(_) => Some("CREATE SEQUENCE"),
        Statement::AlterSequence(_) => Some("ALTER SEQUENCE"),
        Statement::DropSequence(_) => Some("DROP SEQUENCE"),
        Statement::Copy(c) if c.direction == crate::sql::ast::CopyDirection::From => {
            Some("COPY FROM")
        }
        Statement::Explain(e) if explain_analyzes(e) => match &*e.statement {
            Statement::Insert(_) => Some("INSERT"),
            Statement::Update(_) => Some("UPDATE"),
            Statement::Delete(_) => Some("DELETE"),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the raw `EXPLAIN` options turn `ANALYZE` on (values are checked by the analyzer).
fn explain_analyzes(e: &crate::sql::ast::Explain) -> bool {
    use crate::sql::ast::ExplainValue;
    e.options.iter().any(|o| {
        o.name.eq_ignore_ascii_case("analyze")
            && match &o.value {
                None => true,
                Some(ExplainValue::Word(w)) => crate::settings::parse_bool(w) == Some(true),
                Some(ExplainValue::Integer(n)) => *n == 1,
                Some(ExplainValue::Other(_)) => false,
            }
    })
}

/// Microseconds since 2000-01-01 (PostgreSQL's epoch), for `Transaction.started_at`.
fn pg_now_micros() -> i64 {
    const UNIX_TO_PG_SECS: i64 = 946_684_800;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .map_or(0, |us| us - UNIX_TO_PG_SECS * 1_000_000)
}

/// Everything a statement needs besides the transaction (`m4/02` §4.1).
struct StmtEnv<'a> {
    cluster: &'a Cluster,
    db: &'a Arc<DatabaseHandle>,
    catalog: &'a dyn CatalogReader,
    snapshot: &'a Snapshot,
    info: &'a SessionInfo,
    runtime: &'a SessionRuntime<'a>,
    type_env: &'a TypeEnv<'a>,
    interrupts: &'a InterruptFlag,
    planner: &'a PlannerSettings,
    role_oid: Oid,
    in_transaction_block: bool,
}

/// Rows (as text) and the affected-row count of one run.
struct RunResult {
    rows: Vec<Vec<Option<String>>>,
    affected: u64,
}

impl StmtEnv<'_> {
    fn fold_constants(&self, bound: &mut BoundStatement) -> Result<()> {
        planner::check_constant_exprs(
            bound,
            &executor::EvalCtx {
                session: self.info,
                catalog: self.catalog,
                runtime: self.runtime,
                type_env: self.type_env,
            },
        )
    }

    fn plan(
        &self,
        bound: &BoundStatement,
        want_explain: bool,
        explain_verbose: bool,
    ) -> Result<PhysicalQuery> {
        let env = planner::PlanEnv {
            catalog: self.catalog,
            storage: &**self.cluster.storage(),
            settings: self.planner,
            type_env: self.type_env,
            want_explain,
            explain_verbose,
        };
        planner::plan(bound, &env)
    }

    fn exec_env<'e>(&'e self, instr: Option<&'e Rc<Instrumentation>>) -> executor::ExecEnv<'e> {
        executor::ExecEnv {
            catalog: self.catalog,
            storage: &**self.cluster.storage(),
            indexes: &**self.cluster.indexes(),
            snapshot: self.snapshot,
            session: self.info,
            runtime: self.runtime,
            interrupts: self.interrupts,
            type_env: self.type_env,
            mem_limit: self.planner.query_mem_limit,
            instr,
        }
    }

    /// Runs `query` to the end. Rows are converted to text only when `returns_rows`.
    fn run(
        &self,
        txn: &mut Transaction,
        query: &PhysicalQuery,
        returns_rows: bool,
        instr: Option<&Rc<Instrumentation>>,
    ) -> Result<RunResult> {
        // A leftover `unknown` is reported (and rendered) as text (PostgreSQL ≥ 10).
        let types: Vec<SqlType> = query
            .output
            .iter()
            .map(|c| {
                if c.ty.oid == oid::UNKNOWN {
                    SqlType::TEXT
                } else {
                    c.ty
                }
            })
            .collect();
        let mut exec = build_executor(query, instr);
        let mut ctx = ExecCtx::new(self.exec_env(instr), txn, query);
        let mut rows = Vec::new();
        while let Some(row) = exec.next(&mut ctx)? {
            if returns_rows {
                rows.push(row_text(&row, &types, self.type_env)?);
            }
        }
        Ok(RunResult {
            rows,
            affected: exec.rows_affected(),
        })
    }
}

/// Builds the executor tree; EXPLAIN ANALYZE wraps every node with its counters.
fn build_executor(
    query: &PhysicalQuery,
    instr: Option<&Rc<Instrumentation>>,
) -> executor::BoxedExecutor {
    match instr {
        Some(i) => executor::instrument::build_instrumented(query, i),
        None => executor::build_query(query),
    }
}

/// Renders the `QUERY PLAN` lines (`Planning Time` / `Execution Time` included).
fn render_explain(
    options: analyzer::bound::ExplainOptions,
    query: &PhysicalQuery,
    analyzed: Option<&(Rc<Instrumentation>, Duration)>,
    planning: Duration,
    span: crate::error::Span,
) -> Result<Vec<String>> {
    let times = crate::explain::Timings {
        planning,
        execution: analyzed.as_ref().map(|(_, d)| *d),
    };
    crate::explain::run(
        &options,
        query,
        analyzed.as_ref().map(|(i, _)| &**i),
        &times,
        span,
    )
}

/// The visible columns of a row as text, following `DateStyle`, `TimeZone` and `extra_float_digits`.
fn row_text(row: &[Datum], types: &[SqlType], env: &TypeEnv<'_>) -> Result<Vec<Option<String>>> {
    row.iter()
        .take(types.len())
        .zip(types)
        .map(|(d, ty)| io::try_output_text_env(d, *ty, env))
        .collect()
}

// ----- TxnManager calls ----------------------------------------------------
// Every call into the transaction manager that `m3.md` §4.6 changes goes
// through these three functions.

/// Takes the writer lock and an XID. The lock wait honours `lock_timeout`.
fn mgr_begin_write(
    mgr: &Arc<TxnManager>,
    session_id: u64,
    lock_timeout: Option<Duration>,
    interrupts: &InterruptFlag,
) -> Result<(Xid, WriterGuard)> {
    mgr.begin_write(
        session_id,
        &WaitCtl {
            lock_timeout,
            interrupts,
        },
    )
}

fn mgr_commit(mgr: &TxnManager, xid: Xid, dropped: &[RelFileLocator]) -> Result<()> {
    mgr.commit(xid, dropped)
}

fn mgr_abort(mgr: &TxnManager, xid: Xid, created: &[RelFileLocator]) -> Result<()> {
    mgr.abort(xid, created)
}

fn mgr_is_blocked_by(mgr: &TxnManager, session_id: u64, among: &[u64]) -> bool {
    mgr.is_blocked_by(session_id, among)
}

/// The `RuntimeInfo` a statement's `EvalCtx` gets (`m3.md` §6.11.6).
struct SessionRuntime<'a> {
    pid: i32,
    mgr: Option<Arc<TxnManager>>,
    interrupts: Arc<InterruptFlag>,
    stmt_started_at: i64,
    xid: Option<u64>,
    /// The session's parameters. `set_config` copies them into `changed` and
    /// the session takes that back when the statement succeeds.
    base: &'a Settings,
    changed: RefCell<Option<Settings>>,
    chars: TxnCharacteristics,
    seq: &'a RefCell<SeqSession>,
    seq_store: &'a dyn SequenceStore,
    catalog: &'a dyn CatalogReader,
}

impl std::fmt::Debug for SessionRuntime<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuntime")
            .field("pid", &self.pid)
            .field("xid", &self.xid)
            .finish_non_exhaustive()
    }
}

impl SessionRuntime<'_> {
    fn seq_runtime(&self) -> SeqRuntime<'_> {
        SeqRuntime {
            state: self.seq,
            store: self.seq_store,
            catalog: self.catalog,
            read_only: self.chars.read_only,
        }
    }
}

impl RuntimeInfo for SessionRuntime<'_> {
    fn backend_pid(&self) -> i32 {
        self.pid
    }

    fn is_blocked_by(&self, pid: i32, among: &[i32]) -> bool {
        let Some(mgr) = &self.mgr else {
            return false;
        };
        let to_id = |p: &i32| u64::try_from(*p).ok();
        let (Some(id), among) = (
            to_id(&pid),
            among.iter().filter_map(to_id).collect::<Vec<_>>(),
        ) else {
            return false;
        };
        mgr_is_blocked_by(mgr, id, &among)
    }

    fn check_interrupts(&self) -> Result<()> {
        self.interrupts.check()
    }

    fn current_xid(&self) -> Option<u64> {
        self.xid
    }

    fn statement_timestamp(&self) -> Option<i64> {
        (self.stmt_started_at != 0).then_some(self.stmt_started_at)
    }

    fn get_setting(&self, name: &str) -> Result<Option<String>> {
        match name.to_ascii_lowercase().as_str() {
            "transaction_isolation" => return Ok(Some(self.chars.isolation.as_str().to_owned())),
            "transaction_read_only" => return Ok(Some(on_off(self.chars.read_only).to_owned())),
            "transaction_deferrable" => return Ok(Some(on_off(self.chars.deferrable).to_owned())),
            _ => {}
        }
        let changed = self.changed.borrow();
        let settings = changed.as_ref().unwrap_or(self.base);
        match settings.show(name) {
            Ok((_, v)) => Ok(Some(v)),
            Err(e) if e.sqlstate == sqlstate::UNDEFINED_OBJECT => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String> {
        if is_characteristic(name) {
            return Err(Error::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format!("set_config of \"{name}\" is not supported; use SET TRANSACTION"),
            ));
        }
        let mut changed = self.changed.borrow_mut();
        let settings = changed.get_or_insert_with(|| self.base.clone());
        let args = value.map(|v| [v.to_owned()]);
        // is_local lasts until the end of the (implicit) transaction, which
        // `Settings::commit` / `rollback` take care of.
        settings.set(name, args.as_ref().map(<[String; 1]>::as_slice), local)?;
        Ok(settings.get(name).to_owned())
    }

    fn nextval(&self, seq: Oid) -> Result<i64> {
        self.seq_runtime().nextval(seq)
    }

    fn currval(&self, seq: Oid) -> Result<i64> {
        self.seq_runtime().currval(seq)
    }

    fn lastval(&self) -> Result<i64> {
        self.seq_runtime().lastval()
    }

    fn setval(&self, seq: Oid, value: i64, is_called: bool) -> Result<i64> {
        self.seq_runtime().setval(seq, value, is_called)
    }
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
fn column_descs(columns: &[OutputColumn], catalog: &dyn CatalogReader) -> Vec<ColumnDesc> {
    columns
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
        CopyIn(u8, Vec<i16>),
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
        fn copy_in_response(&mut self, format: u8, formats: &[i16]) -> std::io::Result<()> {
            self.0.push(Ev::CopyIn(format, formats.to_vec()));
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
            chain: false,
            span: Span::default(),
        })
    }
    fn set(name: &str, v: &str, local: bool) -> Statement {
        Statement::Set(SetStmt {
            local,
            name: name.into(),
            value: SetValue::Values(vec![SetArg::String(v.into())]),
            transaction: None,
            constraints: false,
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
            chain: false,
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
        let columns = vec![
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
        ];
        let cat = FakeCatalog::new("postgres");
        let d = column_descs(&columns, &cat);
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

    // ----- M3: transaction characteristics, AND CHAIN, savepoints ----------

    fn val(s: &mut Session, q: &str) -> String {
        one(s, q)
    }

    #[test]
    fn savepoint_errors_follow_postgresql() {
        let (_tc, mut s) = cl();
        for (q, state) in [
            ("savepoint a", "25P01"),
            ("release a", "25P01"),
            ("rollback to a", "25P01"),
            ("select 1; savepoint a", "25P01"),
        ] {
            assert_eq!(errors(&sql(&mut s, q)), vec![state], "{q}");
            assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        }
        sql(&mut s, "begin");
        assert_eq!(errors(&sql(&mut s, "savepoint a")), vec!["0A000"]);
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        sql(&mut s, "rollback");
        for q in ["release savepoint a", "rollback to savepoint a"] {
            sql(&mut s, "begin");
            assert_eq!(errors(&sql(&mut s, q)), vec!["3B001"], "{q}");
            assert_eq!(s.transaction_status(), TransactionStatus::Failed);
            sql(&mut s, "rollback");
        }
        // Failed block: SAVEPOINT / RELEASE give 25P02, ROLLBACK TO 3B001
        // and the block stays failed.
        sql(&mut s, "begin");
        sql(&mut s, "select 1 / 0");
        assert_eq!(errors(&sql(&mut s, "savepoint a")), vec!["25P02"]);
        assert_eq!(errors(&sql(&mut s, "release a")), vec!["25P02"]);
        assert_eq!(errors(&sql(&mut s, "rollback to a")), vec!["3B001"]);
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        sql(&mut s, "rollback");
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn and_chain_keeps_characteristics() {
        let (_tc, mut s) = cl();
        for q in [
            "commit and chain",
            "rollback and chain",
            "end and chain",
            "abort and chain",
        ] {
            assert_eq!(errors(&sql(&mut s, q)), vec!["25P01"], "{q}");
        }
        // The implicit block of a multi-statement Query is not a block.
        assert_eq!(
            errors(&sql(&mut s, "select 1; commit and chain")),
            vec!["25P01"]
        );
        sql(&mut s, "begin read only isolation level read uncommitted");
        let ev = sql(&mut s, "commit and chain");
        assert_eq!(tags(&ev), vec!["COMMIT"]);
        assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        assert_eq!(
            val(&mut s, "show transaction_isolation"),
            "read uncommitted"
        );
        let ev = sql(&mut s, "rollback and chain");
        assert_eq!(tags(&ev), vec!["ROLLBACK"]);
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        sql(&mut s, "commit");
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        assert_eq!(val(&mut s, "show transaction_isolation"), "read committed");
        // AND NO CHAIN is a plain COMMIT.
        sql(&mut s, "begin");
        sql(&mut s, "commit and no chain");
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn and_chain_from_a_failed_block_starts_from_the_defaults() {
        let mut s = session();
        for q in ["rollback and chain", "commit and chain"] {
            sql(&mut s, "begin read only");
            sql(&mut s, "select 1 / 0");
            assert_eq!(s.transaction_status(), TransactionStatus::Failed);
            let ev = sql(&mut s, q);
            assert_eq!(tags(&ev), vec!["ROLLBACK"], "{q}");
            assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
            // PostgreSQL 17: READ ONLY set by BEGIN is reverted by the abort.
            assert_eq!(val(&mut s, "show transaction_read_only"), "off", "{q}");
            sql(&mut s, "rollback");
            // ... but what AND CHAIN restored survives the next failure.
            sql(&mut s, "begin read only");
            sql(&mut s, "commit and chain");
            sql(&mut s, "select 1 / 0");
            sql(&mut s, q);
            assert_eq!(val(&mut s, "show transaction_read_only"), "on", "{q}");
            sql(&mut s, "rollback");
        }
    }

    #[test]
    fn transaction_characteristics_and_defaults() {
        let mut s = session();
        assert_eq!(val(&mut s, "show transaction_isolation"), "read committed");
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        assert_eq!(val(&mut s, "show transaction_deferrable"), "off");
        sql(
            &mut s,
            "begin isolation level read uncommitted, read only deferrable",
        );
        assert_eq!(
            val(&mut s, "show transaction_isolation"),
            "read uncommitted"
        );
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        assert_eq!(val(&mut s, "show transaction_deferrable"), "on");
        sql(&mut s, "commit");
        // The next transaction starts from the defaults again.
        assert_eq!(val(&mut s, "show transaction_isolation"), "read committed");
        // SET TRANSACTION in a block, with several modes.
        sql(&mut s, "begin");
        sql(
            &mut s,
            "set transaction isolation level read uncommitted, read only",
        );
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        assert_eq!(
            val(&mut s, "show transaction_isolation"),
            "read uncommitted"
        );
        sql(&mut s, "set transaction read write");
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        sql(&mut s, "rollback");
        // SET TRANSACTION outside a block: WARNING only.
        let ev = sql(&mut s, "set transaction read only");
        assert_eq!(
            ev,
            vec![
                Ev::Notice(Severity::Warning, "25P01"),
                Ev::Complete("SET".into())
            ]
        );
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        // ... but works in the implicit block of a multi-statement Query.
        let ev = sql(
            &mut s,
            "set transaction read only; show transaction_read_only",
        );
        assert_eq!(data(&ev), vec![vec![Some("on".to_owned())]]);
        // Defaults.
        sql(
            &mut s,
            "set session characteristics as transaction read only, isolation level read uncommitted",
        );
        assert_eq!(val(&mut s, "show default_transaction_read_only"), "on");
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        assert_eq!(
            val(&mut s, "show transaction_isolation"),
            "read uncommitted"
        );
        sql(&mut s, "begin read write");
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        sql(&mut s, "commit");
        sql(&mut s, "reset default_transaction_read_only");
        sql(
            &mut s,
            "set default_transaction_isolation = 'read committed'",
        );
        assert_eq!(val(&mut s, "show transaction_read_only"), "off");
        assert_eq!(val(&mut s, "show transaction_isolation"), "read committed");
        // SET / RESET of the current value.
        sql(&mut s, "begin");
        sql(&mut s, "set transaction_read_only = on");
        assert_eq!(val(&mut s, "show transaction_read_only"), "on");
        for q in [
            "reset transaction_read_only",
            "reset transaction_isolation",
            "reset transaction_deferrable",
            "set transaction_read_only = default",
        ] {
            assert_eq!(errors(&sql(&mut s, q)), vec!["0A000"], "{q}");
            sql(&mut s, "rollback");
            sql(&mut s, "begin");
        }
        sql(&mut s, "commit");
    }

    #[test]
    fn unsupported_isolation_levels_are_0a000() {
        let mut s = session();
        for q in [
            "begin isolation level repeatable read",
            "start transaction isolation level serializable",
            "set default_transaction_isolation = 'serializable'",
            "set session characteristics as transaction isolation level repeatable read",
        ] {
            assert_eq!(errors(&sql(&mut s, q)), vec!["0A000"], "{q}");
            assert_eq!(s.transaction_status(), TransactionStatus::Idle, "{q}");
        }
        sql(&mut s, "begin");
        assert_eq!(
            errors(&sql(&mut s, "set transaction isolation level serializable")),
            vec!["0A000"]
        );
        sql(&mut s, "rollback");
        sql(&mut s, "begin");
        assert_eq!(
            errors(&sql(&mut s, "set transaction_isolation = 'nonsense'")),
            vec!["22023"]
        );
        sql(&mut s, "rollback");
        assert_eq!(
            val(&mut s, "show default_transaction_isolation"),
            "read committed"
        );
    }

    #[test]
    fn read_only_rejects_writes_before_the_writer_lock() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table ro (a int)");
        sql(&mut s, "begin read only");
        for (q, tag) in [
            ("insert into ro values (1)", "INSERT"),
            ("update ro set a = 1", "UPDATE"),
            ("delete from ro", "DELETE"),
            ("create table ro2 (a int)", "CREATE TABLE"),
            ("drop table ro", "DROP TABLE"),
        ] {
            sql(&mut s, "begin read only");
            let mut sink = ErrSink::default();
            s.execute_simple(q, &mut sink).unwrap();
            assert_eq!(
                sink.0.as_deref(),
                Some(&format!("25006 cannot execute {tag} in a read-only transaction")[..]),
                "{q}"
            );
            // No writer lock or XID was taken: another session can write now.
            let mut other = tc.session("postgres").unwrap();
            assert_eq!(
                errors(&sql(&mut other, "insert into ro values (9)")),
                Vec::<&str>::new()
            );
            sql(&mut s, "rollback");
        }
        // Analysis errors win over 25006 for INSERT / UPDATE / DELETE.
        for (q, code) in [
            ("insert into nonexist values (1)", "42P01"),
            ("update ro set a = 'x'", "22P02"),
            ("update ro set b = 1", "42703"),
            ("delete from nonexist", "42P01"),
            ("insert into ro values (1, 2)", "42601"),
        ] {
            sql(&mut s, "begin read only");
            assert_eq!(errors(&sql(&mut s, q)), vec![code], "{q}");
            sql(&mut s, "rollback");
        }
        // Reads and CHECKPOINT are fine.
        sql(&mut s, "begin read only");
        assert_eq!(val(&mut s, "select a from ro"), "9");
        assert!(errors(&sql(&mut s, "checkpoint")).is_empty());
        sql(&mut s, "commit");
        // default_transaction_read_only; BEGIN READ WRITE overrides it.
        sql(&mut s, "set default_transaction_read_only = on");
        assert_eq!(
            errors(&sql(&mut s, "insert into ro values (2)")),
            vec!["25006"]
        );
        sql(&mut s, "begin read write");
        assert!(errors(&sql(&mut s, "insert into ro values (2)")).is_empty());
        sql(&mut s, "commit");
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    /// Records the SQLSTATE and message of the last error.
    #[derive(Default)]
    struct ErrSink(Option<String>);

    impl ResultSink for ErrSink {
        fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
            Ok(())
        }
        fn data_row(&mut self, _: &[Option<String>]) -> std::io::Result<()> {
            Ok(())
        }
        fn command_complete(&mut self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn empty_query(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn error(&mut self, e: &Error) -> std::io::Result<()> {
            self.0 = Some(format!("{} {}", e.sqlstate.0, e.message));
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
    fn isolation_and_read_write_changes_after_a_query() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table iso (a int)");
        // Any query counts, also SELECT without FROM (PostgreSQL 17).
        sql(&mut s, "begin");
        sql(&mut s, "select 1");
        assert_eq!(
            errors(&sql(
                &mut s,
                "set transaction isolation level read uncommitted"
            )),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
        sql(&mut s, "begin");
        sql(&mut s, "create table iso_x (a int)");
        assert_eq!(
            errors(&sql(&mut s, "set transaction not deferrable")),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
        sql(&mut s, "begin read only");
        sql(&mut s, "select 1");
        assert_eq!(
            errors(&sql(&mut s, "set transaction read write")),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
        // A failing BEGIN mode from idle leaves the session idle.
        assert_eq!(
            errors(&sql(
                &mut s,
                "select 1; begin isolation level read uncommitted"
            )),
            vec!["25001"]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        // A table reference does: a different level is 25001, the same is fine.
        sql(&mut s, "begin");
        sql(&mut s, "select * from iso");
        assert!(
            errors(&sql(
                &mut s,
                "set transaction isolation level read committed"
            ))
            .is_empty()
        );
        assert_eq!(
            errors(&sql(
                &mut s,
                "set transaction isolation level read uncommitted"
            )),
            vec!["25001"]
        );
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        sql(&mut s, "rollback");
        // READ ONLY is allowed after a query; READ WRITE is not.
        sql(&mut s, "begin read only");
        sql(&mut s, "select * from iso");
        assert!(errors(&sql(&mut s, "set transaction read only")).is_empty());
        assert_eq!(
            errors(&sql(&mut s, "set transaction read write")),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
        // A write also counts.
        sql(&mut s, "begin");
        sql(&mut s, "insert into iso values (1)");
        assert_eq!(
            errors(&sql(
                &mut s,
                "set transaction isolation level read uncommitted"
            )),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
        // BEGIN after a table reference in the same Query message.
        assert_eq!(
            errors(&sql(
                &mut s,
                "select * from iso; begin isolation level read uncommitted"
            )),
            vec!["25001"]
        );
        sql(&mut s, "rollback");
    }

    #[test]
    fn set_local_works_in_the_implicit_block_and_reverts() {
        let (_tc, mut s) = cl();
        let ev = sql(&mut s, "select 1; set local application_name = 'ib'");
        assert!(errors(&ev).is_empty());
        assert_eq!(val(&mut s, "show application_name"), "");
    }

    #[test]
    fn timeouts_and_cancel() {
        let (_tc, mut s) = cl();
        let flag = s.interrupt_flag();
        // A cancel that arrived while idle is dropped at the next Query.
        flag.request_cancel();
        assert!(errors(&sql(&mut s, "select 1")).is_empty());
        // The statement deadline is set per statement and cleared after it.
        sql(&mut s, "set statement_timeout = '1h'");
        assert!(errors(&sql(&mut s, "select 1")).is_empty());
        assert!(flag.check().is_ok());
        assert_eq!(val(&mut s, "show statement_timeout"), "1h");
        // A deadline already in the past is reported (57014) by the next
        // check, which a statement over rows makes.
        sql(&mut s, "create table tm (a int)");
        sql(&mut s, "insert into tm values (1), (2)");
        sql(&mut s, "set statement_timeout = '1ms'");
        std::thread::sleep(Duration::from_millis(0));
        flag.set_statement_deadline(Some(Instant::now()));
        assert_eq!(flag.check().unwrap_err().sqlstate, sqlstate::QUERY_CANCELED);
        flag.set_statement_deadline(None);
    }

    #[test]
    fn idle_timeouts_by_state() {
        let mut s = session();
        assert_eq!(s.idle_timeout(), None);
        sql(&mut s, "set idle_session_timeout = '5s'");
        sql(&mut s, "set idle_in_transaction_session_timeout = '2s'");
        assert_eq!(s.idle_timeout(), Some(Duration::from_secs(5)));
        assert_eq!(
            s.idle_timeout_error().sqlstate,
            sqlstate::IDLE_SESSION_TIMEOUT
        );
        assert_eq!(s.idle_timeout_error().severity, Severity::Fatal);
        sql(&mut s, "begin");
        assert_eq!(s.idle_timeout(), Some(Duration::from_secs(2)));
        assert_eq!(
            s.idle_timeout_error().sqlstate,
            sqlstate::IDLE_IN_TRANSACTION_SESSION_TIMEOUT
        );
        sql(&mut s, "select 1 / 0");
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        assert_eq!(s.idle_timeout(), Some(Duration::from_secs(2)));
        sql(&mut s, "rollback");
        assert_eq!(s.idle_timeout(), Some(Duration::from_secs(5)));
        sql(&mut s, "reset idle_session_timeout");
        assert_eq!(s.idle_timeout(), None);
    }

    #[test]
    fn m3_settings_are_registered() {
        let mut s = session();
        assert_eq!(val(&mut s, "show deadlock_timeout"), "1s");
        assert_eq!(val(&mut s, "show synchronous_commit"), "on");
        sql(&mut s, "set synchronous_commit = 'local'");
        assert_eq!(val(&mut s, "show synchronous_commit"), "local");
        assert_eq!(val(&mut s, "show full_page_writes"), "on");
        assert_eq!(val(&mut s, "show wal_sync_method"), "fdatasync");
        assert_eq!(
            errors(&sql(&mut s, "set full_page_writes = off")),
            vec!["55P02"]
        );
        // `transaction_timeout` is accepted and stored only (D10-8).
        assert!(errors(&sql(&mut s, "set transaction_timeout = 1")).is_empty());
        assert_eq!(val(&mut s, "show transaction_timeout"), "1ms");
        assert_eq!(
            errors(&sql(&mut s, "set max_connections = 1")),
            vec!["55P02"]
        );
        assert_eq!(val(&mut s, "show work_mem"), "4MB");
        assert_eq!(val(&mut s, "show default_transaction_deferrable"), "off");
    }

    #[test]
    fn backend_pid_is_the_session_id() {
        let (tc, s) = cl();
        let other = tc.session("postgres").unwrap();
        assert!(s.backend_pid() > 0);
        assert_ne!(s.backend_pid(), other.backend_pid());
    }
    // ----- COPY FROM STDIN (`m4/10` §5.3) ------------------------------------

    fn copy_sql(s: &mut Session, q: &str) -> Vec<Ev> {
        sql(s, q)
    }

    fn feed(s: &mut Session, f: impl FnOnce(&mut Session, &mut Sink)) -> Vec<Ev> {
        let mut sink = Sink::default();
        f(s, &mut sink);
        sink.0
    }

    #[test]
    fn copy_from_stdin_state_machine() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table cp (a int, b text)");
        let ev = copy_sql(&mut s, "copy cp from stdin");
        assert_eq!(ev, vec![Ev::CopyIn(0, vec![0, 0])]);
        assert!(s.is_copying_in());
        assert_eq!(s.idle_timeout(), None);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        // A row split across CopyData messages.
        let ev = feed(&mut s, |s, k| {
            s.copy_data(b"1\ta", k).unwrap();
            s.copy_data(b"\n2\t", k).unwrap();
            s.copy_data(b"b\n", k).unwrap();
            s.copy_data(b"", k).unwrap();
        });
        assert!(ev.is_empty(), "{ev:?}");
        assert!(s.is_copying_in());
        let ev = feed(&mut s, |s, k| s.copy_done(k).unwrap());
        assert_eq!(ev, vec![Ev::Complete("COPY 2".into())]);
        assert!(!s.is_copying_in());
        assert_eq!(count(&mut s, "cp"), 2);
        // Stray messages in the idle state are ignored.
        let ev = feed(&mut s, |s, k| {
            s.copy_data(b"x", k).unwrap();
            s.copy_done(k).unwrap();
            s.copy_fail("x", k).unwrap();
            s.copy_poll(k).unwrap();
        });
        assert!(ev.is_empty());
    }

    #[test]
    fn copy_resumes_the_rest_of_the_message() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table cp (a int)");
        let ev = copy_sql(&mut s, "copy cp from stdin; select 'after'; select 2");
        assert_eq!(ev, vec![Ev::CopyIn(0, vec![0])]);
        let ev = feed(&mut s, |s, k| {
            s.copy_data(b"7\n", k).unwrap();
            s.copy_done(k).unwrap();
        });
        assert_eq!(
            ev,
            vec![
                Ev::Complete("COPY 1".into()),
                Ev::RowDesc(vec!["?column?".into()]),
                Ev::Row(vec![Some("after".into())]),
                Ev::Complete("SELECT 1".into()),
                Ev::RowDesc(vec!["?column?".into()]),
                Ev::Row(vec![Some("2".into())]),
                Ev::Complete("SELECT 1".into()),
            ]
        );
        assert_eq!(count(&mut s, "cp"), 1);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn copy_fail_and_data_errors_abort_the_command() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table cp (a int not null)");
        copy_sql(&mut s, "copy cp from stdin");
        let ev = feed(&mut s, |s, k| {
            s.copy_data(b"1\n", k).unwrap();
            s.copy_fail("boom", k).unwrap();
        });
        assert_eq!(errors(&ev), vec!["57014"]);
        assert!(!s.is_copying_in());
        assert_eq!(count(&mut s, "cp"), 0);
        // A bad row: one error, then everything else is ignored.
        copy_sql(&mut s, "copy cp from stdin");
        let ev = feed(&mut s, |s, k| {
            s.copy_data(b"1\nabc\n", k).unwrap();
            s.copy_data(b"2\n", k).unwrap();
            s.copy_done(k).unwrap();
        });
        assert_eq!(errors(&ev), vec!["22P02"]);
        assert!(!s.is_copying_in());
        assert_eq!(count(&mut s, "cp"), 0);
        // The failure inside a block leaves it failed.
        sql(&mut s, "begin");
        copy_sql(&mut s, "copy cp from stdin");
        let ev = feed(&mut s, |s, k| s.copy_data(b"\\N\n", k).unwrap());
        assert_eq!(errors(&ev), vec!["23502"]);
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        sql(&mut s, "rollback");
        // A COPY inside a block commits with it.
        sql(&mut s, "begin");
        copy_sql(&mut s, "copy cp from stdin");
        assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
        feed(&mut s, |s, k| {
            s.copy_data(b"5\n", k).unwrap();
            s.copy_done(k).unwrap();
        });
        assert_eq!(count(&mut s, "cp"), 1);
        sql(&mut s, "rollback");
        assert_eq!(count(&mut s, "cp"), 0);
    }

    #[test]
    fn copy_errors_before_the_response() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table cp (a int)");
        assert_eq!(
            errors(&copy_sql(&mut s, "copy nosuch from stdin")),
            vec!["42P01"]
        );
        assert_eq!(
            errors(&copy_sql(&mut s, "copy cp (zz) from stdin")),
            vec!["42703"]
        );
        assert!(!s.is_copying_in());
        sql(&mut s, "begin read only");
        let ev = copy_sql(&mut s, "copy cp from stdin");
        assert_eq!(errors(&ev), vec!["25006"]);
        assert!(!ev.iter().any(|e| matches!(e, Ev::CopyIn(..))));
        sql(&mut s, "rollback");
        // FREEZE is checked after the response.
        let ev = copy_sql(&mut s, "copy cp from stdin with (freeze on)");
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert!(matches!(ev[0], Ev::CopyIn(..)));
        assert_eq!(errors(&ev), vec!["55000"]);
        assert!(!s.is_copying_in());
    }

    #[test]
    fn copy_poll_reports_cancel_and_terminate_drops_the_copy() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table cp (a int)");
        copy_sql(&mut s, "copy cp from stdin");
        let flag = s.interrupt_flag();
        flag.request_cancel();
        let ev = feed(&mut s, |s, k| s.copy_poll(k).unwrap());
        assert_eq!(errors(&ev), vec!["57014"]);
        assert!(!s.is_copying_in());
        copy_sql(&mut s, "copy cp from stdin");
        s.terminate();
        assert!(!s.is_copying_in());
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn write_statement_tags() {
        let tag = |q: &str| write_statement_tag(&sql::parse(q).unwrap()[0]);
        assert_eq!(tag("insert into t values (1)"), Some("INSERT"));
        assert_eq!(tag("create index i on t (a)"), Some("CREATE INDEX"));
        assert_eq!(tag("drop index i"), Some("DROP INDEX"));
        assert_eq!(tag("alter table t owner to x"), Some("ALTER TABLE"));
        assert_eq!(tag("truncate t"), Some("TRUNCATE TABLE"));
        assert_eq!(tag("create sequence s"), Some("CREATE SEQUENCE"));
        assert_eq!(tag("copy t from stdin"), Some("COPY FROM"));
        assert_eq!(tag("vacuum t"), None);
        assert_eq!(tag("select 1"), None);
        assert_eq!(tag("explain select 1"), None);
        assert_eq!(tag("explain (analyze) select 1"), None);
        assert_eq!(
            tag("explain analyze insert into t values (1)"),
            Some("INSERT")
        );
        assert_eq!(tag("explain (analyze false) delete from t"), None);
        assert_eq!(
            tag("explain (analyze on, costs off) update t set a = 1"),
            Some("UPDATE")
        );
    }

    #[test]
    fn planner_settings_reach_the_planner() {
        let (_tc, mut s) = cl();
        sql(&mut s, "create table pt (a int)");
        sql(&mut s, "set enable_seqscan = off");
        sql(&mut s, "set yuzhu.validate_plans = on");
        assert!(errors(&sql(&mut s, "select * from pt")).is_empty());
        assert_eq!(one(&mut s, "show enable_seqscan"), "off");
    }

    #[test]
    fn ddl_goes_through_ddl_execute() {
        let (_tc, mut s) = cl();
        let ev = sql(&mut s, "create table dd (a int primary key, b int)");
        assert_eq!(errors(&ev), Vec::<&str>::new(), "{ev:?}");
        assert_eq!(tags(&ev), vec!["CREATE TABLE"]);
        // Writing DDL is refused in a read-only transaction before anything else.
        sql(&mut s, "begin read only");
        for q in [
            "truncate dd",
            "create index ii on dd (b)",
            "drop index ii",
            "alter table dd owner to postgres",
            "create sequence qq",
        ] {
            assert_eq!(errors(&sql(&mut s, q)), vec!["25006"], "{q}");
            sql(&mut s, "rollback");
            sql(&mut s, "begin read only");
        }
        sql(&mut s, "rollback");
    }
}
