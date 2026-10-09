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

mod extended;
pub use self::extended::{ExecOutcome as ExtOutcome, StatementDescription};
mod locking;
#[allow(dead_code)] // XQ-5 が prepared.rs を作り直すまで ExtStore は extended.rs にある。
mod prepared;
mod simple;
mod txn_ctl;

use self::extended::ExtStore;
#[allow(clippy::wildcard_imports)]
use self::locking::*;
#[allow(clippy::wildcard_imports)]
use self::simple::*;
#[allow(clippy::wildcard_imports)]
use self::txn_ctl::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::analyzer::bound::{BoundExplain, BoundStatement};
use crate::analyzer::{self, OutputColumn};
use crate::backend::BackendId;
use crate::catalog::CatalogReader;
use crate::catalog::names::CatalogNames;
use crate::catalog::reader::StatementCatalog;
use crate::catalog::store::RoleRow;
use crate::copy::{self, CopyIn};
use crate::ddl::role::PasswordPolicy;
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
use crate::txn::lock;
use crate::txn::lock::LockScope;
use crate::txn::{Snapshot, Transaction, TxnManager, Xid, XidGuard};
use crate::types::{Datum, Oid, SqlType, TypeEnv, io, oid};

/// Values from the `StartupMessage`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupParams {
    pub user: String,
    pub database: String,
    pub application_name: Option<String>,
    /// Other parameters (e.g. `client_encoding`, `DateStyle`, `options`).
    pub options: Vec<(String, String)>,
    /// 接続元のアドレス（`pg_stat_activity.client_addr`・hba の照合に使う。Unix ソケットや不明なら `None`）。
    pub client_addr: Option<std::net::IpAddr>,
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
    /// 結果の形式（Extended Query の Bind で決まる。Simple Query は常に Text）。
    pub format: Format,
}

/// Format code of a parameter or a result column (`m5/00` §4.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Text = 0,
    Binary = 1,
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
    pub fn new(severity: Severity, sqlstate: SqlState, message: impl Into<String>) -> Self {
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
    /// 列ごとの形式に従ってエンコード済みの行（Extended Query。`m5/00` §4.7）。
    /// 既定の実装は全列 Text と見なして `data_row` に流す（不正な UTF-8 は置換する）。
    fn data_row_raw(&mut self, values: &[Option<Vec<u8>>]) -> std::io::Result<()> {
        let text: Vec<Option<String>> = values
            .iter()
            .map(|v| v.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()))
            .collect();
        self.data_row(&text)
    }
    /// `CopyOutResponse`（`H`。COPY TO。XQ-6）。既定の実装は `Unsupported`。
    fn copy_out_response(&mut self, format: u8, column_formats: &[u8]) -> std::io::Result<()> {
        let _ = (format, column_formats);
        Err(copy_out_unsupported())
    }
    /// `CopyData`（`d`。COPY TO）。
    fn copy_out_data(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        let _ = chunk;
        Err(copy_out_unsupported())
    }
    /// `CopyDone`（`c`。COPY TO）。
    fn copy_out_done(&mut self) -> std::io::Result<()> {
        Err(copy_out_unsupported())
    }
}

fn copy_out_unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this sink does not support COPY TO STDOUT",
    )
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
    /// 接続しているロールの行（`DdlCtx.role`）。
    role: RoleRow,
    /// A FATAL error was sent: the server must close the connection.
    closing: bool,
    params: StartupParams,
    state: TxState,
    txn: Transaction,
    settings: Settings,
    /// Last value sent to the client for each reported parameter.
    reported: HashMap<&'static str, String>,
    /// Session ID (`BackendId`; the pid shown to the client).
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
    /// Extended Query・準備済み文の状態（XQ）。
    ext: ExtStore,
    /// 始めたトランザクションの通し番号（準備済み文の再解析の判定に使う。XQ-5）。
    txn_serial: u64,
}

/// 移行用（LK-3b）: セッションスコープのロックを解放して、重量ロックの管理者から外す。
/// 09 章の `SessionLocks` が置き換える。
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cluster) = &self.cluster {
            let id = BackendId(self.id);
            cluster.locks().release_all(id, LockScope::Session);
            cluster.locks().unregister_backend(id);
        }
    }
}

impl Session {
    /// Creates a session. `Cluster::connect` rejects an unknown database
    /// (`3D000`), a database that does not accept connections (`55000`) and
    /// an unknown role (`28000`) as FATAL errors.
    pub fn new(cluster: Arc<Cluster>, params: StartupParams) -> Result<Session> {
        let (db, role) = cluster.connect_simple(&params.database, &params.user)?;
        let id = cluster.next_session_id();
        if i32::try_from(id).is_err() {
            return Err(Error::new(
                sqlstate::TOO_MANY_CONNECTIONS,
                "sorry, too many clients already",
            )
            .with_severity(Severity::Fatal));
        }
        // 移行用（LK-3b）: 重量ロックの管理者に登録する。09 章の `SessionLocks` が置き換える。
        cluster
            .locks()
            .register_backend(BackendId(id), i32::try_from(id).unwrap_or(i32::MAX));
        let mut s = Session::build(Some(cluster), id, params);
        if let Some(c) = &s.cluster {
            for (name, value) in c.server_settings() {
                s.settings.set_server_value(name, value);
            }
        }
        s.db = Some(db);
        s.role_oid = role.oid;
        s.role = role;
        Ok(s)
    }

    fn build(cluster: Option<Arc<Cluster>>, id: u64, params: StartupParams) -> Session {
        let settings = Settings::new(&params.user, &startup_settings(&params));
        let reported = settings.reported_values().into_iter().collect();
        Session {
            cluster,
            db: None,
            role_oid: 0,
            role: RoleRow {
                oid: 0,
                name: params.user.clone(),
                can_login: true,
                superuser: false,
            },
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
            ext: ExtStore::default(),
            txn_serial: 0,
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
        self.ext.discard_unnamed();
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

    /// The ID that owns this session's locks (`LockManager`, `BackendRegistry`).
    pub fn backend_id(&self) -> BackendId {
        BackendId(self.id)
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
        let result = self.with_stmt_env(&cluster, &db, |txn, env| {
            let w = txn.write_ctx()?;
            let empty = PhysicalQuery::empty();
            let mut ctx = ExecCtx::new(env.exec_env(None), txn, &empty);
            f(&mut ctx, &w)
        });
        let lsn = self.seq_state.borrow_mut().end_statement();
        self.txn.note_wal(lsn);
        Self::flush_deferred_unlinks(&cluster);
        buffer::assert_no_pins();
        result
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
            client_addr: None,
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
        // The transaction's locks are released too.
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
    fn writers_run_concurrently_and_readers_see_only_committed() {
        let (tc, mut s1) = cl();
        let mut s2 = tc.session("postgres").unwrap();
        sql(&mut s1, "create table w (a int)");
        sql(&mut s1, "begin");
        sql(&mut s1, "insert into w values (1)");
        // Readers neither wait nor see the uncommitted row.
        assert_eq!(count(&mut s2, "w"), 0);
        // M5 (D5): there is no writer lock. A second writer inserts at once
        // (two plain INSERTs do not conflict) with an XID of its own.
        assert_eq!(
            tags(&sql(&mut s2, "insert into w values (2)")),
            vec!["INSERT 0 1"]
        );
        assert_eq!(s2.transaction_status(), TransactionStatus::Idle);
        sql(&mut s1, "commit");
        assert_eq!(count(&mut s2, "w"), 2);
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

    #[test]
    fn backend_id_matches_the_pid_and_is_registered_with_the_lock_manager() {
        let (tc, s) = cl();
        let other = tc.session("postgres").unwrap();
        assert_eq!(
            i64::try_from(s.backend_id().0).unwrap(),
            i64::from(s.backend_pid())
        );
        assert_ne!(s.backend_id(), other.backend_id());
        // Registered: an XID lock can be taken for the session's backend.
        let locks = tc.cluster.locks();
        assert!(locks.held_locks(s.backend_id()).is_empty());
    }

    #[test]
    fn xid_lock_is_held_until_the_end_of_the_transaction_and_released_last() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table xl (a int)");
        let locks = tc.cluster.locks();
        sql(&mut s, "begin");
        sql(&mut s, "insert into xl values (1)");
        let held = locks.held_locks(s.backend_id());
        assert!(
            held.iter()
                .any(|(t, _)| matches!(t, crate::txn::lock::LockTag::TransactionId(_))),
            "{held:?}"
        );
        sql(&mut s, "commit");
        assert!(locks.held_locks(s.backend_id()).is_empty());
        // The same for a rollback.
        sql(&mut s, "begin");
        sql(&mut s, "insert into xl values (2)");
        assert!(!locks.held_locks(s.backend_id()).is_empty());
        sql(&mut s, "rollback");
        assert!(locks.held_locks(s.backend_id()).is_empty());
    }

    #[test]
    fn dropping_a_session_unregisters_the_backend_and_aborts_its_transaction() {
        let (tc, mut s) = cl();
        sql(&mut s, "create table dd (a int)");
        sql(&mut s, "begin");
        sql(&mut s, "insert into dd values (1)");
        let id = s.backend_id();
        let xid = tc.cluster.txn_manager().procs().running_xids();
        assert_eq!(xid.len(), 1);
        drop(s);
        assert!(tc.cluster.locks().held_locks(id).is_empty());
        assert!(tc.cluster.txn_manager().procs().running_xids().is_empty());
        let mut s2 = tc.session("postgres").unwrap();
        assert_eq!(count(&mut s2, "dd"), 0);
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
