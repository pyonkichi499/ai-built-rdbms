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
//! 担当 I が M2 の手順（`m2.md` §5.2〜§5.4）で実装する。A が行ったのは
//! M1 の `Database` / undo ログへの依存を外すところまで: 文の実行
//! （`exec_statement` の `_` の枝）、CREATE / DROP の手順、ライターロック、
//! コミットとアボートの実処理は未実装。M1 の実装は git の 88d1bc0 の
//! `session.rs` を参照。

use std::collections::HashMap;
use std::sync::Arc;

use crate::analyzer::BoundSelect;
use crate::catalog::CatalogReader;
use crate::engine::Cluster;
use crate::error::{Error, Result, Severity, SqlState, sqlstate};
use crate::executor::SessionInfo;
use crate::interrupt::InterruptFlag;
use crate::settings::Settings;
use crate::sql::{
    self,
    ast::{
        ParamTarget, SetArg, SetStmt, SetValue, ShowStmt, Statement, TransactionKind,
        TransactionMode,
    },
};
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
    /// tests of the statement state machine, until `TestCluster` exists.
    #[allow(dead_code)]
    cluster: Option<Arc<Cluster>>,
    params: StartupParams,
    state: TxState,
    txn: Transaction,
    settings: Settings,
    /// Last value sent to the client for each reported parameter.
    reported: HashMap<&'static str, String>,
    /// Session ID (the writer lock owner passed to `begin_write`).
    #[allow(dead_code)]
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
        cluster.connect(&params.database, &params.user)?;
        let id = cluster.next_session_id();
        Ok(Session::build(Some(cluster), id, params))
    }

    fn build(cluster: Option<Arc<Cluster>>, id: u64, params: StartupParams) -> Session {
        let settings = Settings::new(&params.user, &startup_settings(&params));
        let reported = settings.reported_values().into_iter().collect();
        Session {
            cluster,
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
        if self.state == TxState::Implicit {
            self.commit_transaction();
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

    /// 担当 I が `m2.md` §5.3 の手順（`clog`、`pending_unlinks`）で実装する。
    fn commit_transaction(&mut self) {
        self.txn = Transaction::new();
        self.settings.commit();
        self.state = TxState::Idle;
    }

    /// 担当 I が `m2.md` §5.3 の手順（`clog`、`pending_creates`）で実装する。
    fn rollback_transaction(&mut self) {
        self.txn = Transaction::new();
        self.settings.rollback();
        self.state = TxState::Idle;
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
            // 担当 I が analyze → plan → execute と DDL の手順を実装する。
            _ => Err(Error::not_supported(
                "executing this statement is not implemented yet",
            )),
        }
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
                    self.commit_transaction();
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

    #[allow(dead_code)]
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

#[allow(dead_code)]
fn already_exists_notice(name: &str) -> Notice {
    Notice::new(
        Severity::Notice,
        sqlstate::DUPLICATE_TABLE,
        format!("relation \"{name}\" already exists, skipping"),
    )
}

#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Text output of a value, honouring `extra_float_digits`.
#[allow(dead_code)]
fn datum_text(d: &Datum, ty: SqlType, extra_float_digits: i32) -> Option<String> {
    if extra_float_digits <= 0 {
        match d {
            Datum::Float8(v) => return Some(format_g(*v, 15 + extra_float_digits)),
            Datum::Float4(v) => return Some(format_g(f64::from(*v), 6 + extra_float_digits)),
            _ => {}
        }
    }
    io::output_text(d, ty)
}

/// C's `%.*g` (what `float8out` / `float4out` use when
/// `extra_float_digits <= 0`), with PostgreSQL's spellings of the special
/// values. `precision` below 1 is treated as 1.
#[allow(dead_code)]
fn format_g(v: f64, precision: i32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let p = usize::try_from(precision.max(1)).unwrap_or(1);
    let e_form = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = e_form.split_once('e').unwrap_or((&e_form, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            s.to_owned()
        }
    };
    if exp < -4 || exp >= precision.max(1) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mantissa), exp.abs())
    } else {
        let decimals = usize::try_from(precision.max(1) - 1 - exp).unwrap_or(0);
        strip(&format!("{v:.decimals$}"))
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
    fn unimplemented_statements_are_rejected_for_now() {
        let mut s = session();
        let ev = sql(&mut s, "select 1");
        assert_eq!(ev, vec![Ev::Error("0A000", None)]);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
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

    #[test]
    fn float_output_with_extra_float_digits() {
        let t = |v: f64, efd| datum_text(&Datum::Float8(v), SqlType::FLOAT8, efd).unwrap();
        assert_eq!(t(0.1 + 0.2, 0), "0.3");
        assert_eq!(t(1.0 / 3.0, 0), "0.333333333333333");
        assert_eq!(t(0.1 + 0.2, 1), "0.30000000000000004");
        assert_eq!(t(3.25, -15), "3");
        assert_eq!(t(f64::from(0.1f32), 0), "0.100000001490116");
        assert_eq!(t(1e20, 0), "1e+20");
        assert_eq!(t(1e-5, 0), "1e-05");
        assert_eq!(t(123_456.0, 0), "123456");
        assert_eq!(t(-0.0, 0), "-0");
        assert_eq!(t(f64::INFINITY, 0), "Infinity");
        assert_eq!(t(99999.95, -10), "1e+05");
        let f = datum_text(&Datum::Float4(1.0 / 3.0), SqlType::FLOAT4, 0).unwrap();
        assert_eq!(f, "0.333333");
        assert_eq!(format_g(1e15, 15), "1e+15");
        assert_eq!(format_g(1e14, 15), "100000000000000");
    }
}
