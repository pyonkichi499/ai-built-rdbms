//! Extended Query（Parse / Bind / Execute。XQ が持つ）。契約は `m5/00` §4.7、`m5/04` §4.1・§5。
//!
//! XQ-2a の範囲: 文とポータルの表、暗黙のトランザクション（Sync まで）、Sync、Describe、Close、
//! 失敗したブロック、エラーの後始末。次は仮実装で、後続の担当が置き換える。
//!
//! - 再解析はしない（XQ-5）。解析の鍵（世代・search_path・DateStyle・TimeZone）が Parse のときと
//!   変わっていたら Bind / Describe は `0A000` で止める。
//! - Execute の方式は文で決まる（XQ-D3）。ロックなしの SELECT は `OneSelect`: Bind で計画と実行器を作って
//!   スナップショットを登録したまま持ち、Execute ごとに `max_rows` 行ずつ進める（`PortalSuspended`）。
//!   RETURNING つき DML・SHOW・EXPLAIN は初回の Execute で完走して行を保存し（`Stored`）、
//!   それ以外は完走してタグだけ返す（`Multi`）。いずれも完走した後の Execute は 55000。
//! - `OneSelect` の実行は Execute ごとに共有ストレージバリアを取り直す（メッセージの間は持たない）。
//! - `$n`（XQ-3）とバイナリ形式（XQ-4）は未対応。宣言の型が 0 の引数は `42P18`、バイナリは `0A000`。
//! - Bind でのリレーションロックは取らない（Simple Query も `lock_statement_relations` をまだ呼ばない）。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use super::locking::StmtEnv;
use super::{
    ColumnDesc, Format, Output, ParamTarget, ResultSink, Session, Severity, Statement, TxState,
    allowed_when_failed, pg_now_micros, text_column,
};
use crate::analyzer::bound::BoundStatement;
use crate::error::{Error, Result, sqlstate};
use crate::executor::{BoxedExecutor, ExecCtx, ExecState};
use crate::planner::physical::PhysicalQuery;
use crate::sql;
use crate::storage::buffer;
use crate::txn::{RegisteredSnapshot, Snapshot, Transaction};
use crate::types::{Datum, Oid, SqlType, io, oid};

use super::ExecOutcome as RunOutcome;

/// Describe（文）の結果。`columns` が `None` なら `NoData`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementDescription {
    pub param_types: Vec<Oid>,
    pub columns: Option<Vec<ColumnDesc>>,
}

/// Execute の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    /// コマンドタグ。
    Complete(String),
    /// 行数制限で中断した（PortalSuspended）。
    Suspended,
    /// 空の問い合わせ。
    EmptyQuery,
    /// COPY FROM STDIN を始めた（CopyInResponse は送信済み。XQ-6）。
    CopyIn,
}

/// 文の種別（仮。XQ-5 の `StmtKind` が置き換える）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Empty,
    Query,
    Dml {
        returning: bool,
    },
    Explain,
    /// COMMIT / END / ROLLBACK / ABORT / ROLLBACK TO（失敗したブロックでも許される）。
    TxExit,
    Show,
    Other,
}

impl Kind {
    fn of(stmt: &Statement) -> Kind {
        match stmt {
            Statement::Query(_) => Kind::Query,
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                Kind::Dml { returning: false }
            }
            Statement::Explain(_) => Kind::Explain,
            Statement::Show(_) => Kind::Show,
            s if allowed_when_failed(s) => Kind::TxExit,
            _ => Kind::Other,
        }
    }

    /// Parse で解析する（SELECT・DML・EXPLAIN）。
    fn analyzed(self) -> bool {
        matches!(self, Kind::Query | Kind::Dml { .. } | Kind::Explain)
    }
}

/// 解析結果が古いかの判定に使う鍵（`m5/04` XQ-D7。`catalog_dirty` は仮実装では見ない）。
#[derive(Clone, PartialEq, Eq, Debug)]
struct AnalysisKey {
    generation: u64,
    search_path: Vec<String>,
    datestyle: String,
    timezone: String,
}

/// 準備済み文（XQ-5 が `prepared.rs` へ広げる）。
#[derive(Debug)]
pub(super) struct PreparedStmt {
    name: String,
    sql: Arc<str>,
    stmt: Option<Arc<Statement>>,
    kind: Kind,
    param_types: Vec<Oid>,
    columns: Option<Arc<[ColumnDesc]>>,
    key: Option<AnalysisKey>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strategy {
    Empty,
    /// Bind で計画と実行器を作り、Execute ごとに `max_rows` 行ずつ進める（スナップショットは Bind の時点）。
    OneSelect,
    /// 初回の Execute で完走して行を保存し、`max_rows` ごとに返す。
    Stored,
    /// 完走して `Complete(tag)`。以降は 55000。
    Multi,
}

#[derive(Debug)]
struct StoredRows {
    rows: VecDeque<Vec<Option<Vec<u8>>>>,
    /// `"SELECT "` `"INSERT 0 "` など（件数を後ろに付ける）。`counted` が false なら、そのままタグ。
    tag: String,
    counted: bool,
}

/// 中断中の `OneSelect` が持ち越すもの。
struct SelectRun {
    stmt: Arc<Statement>,
    /// Bind の時点のスナップショット。
    snapshot: Snapshot,
    /// Read Committed の登録。ポータルを閉じるまで地平線を押さえる（Repeatable Read は `xact_snapshot` が持つ）。
    _registered: Option<RegisteredSnapshot>,
    /// 最初の Execute で作る。
    planned: Option<Planned>,
}

/// 計画と実行器（`SelectRun` が最初の Execute で作る）。
struct Planned {
    query: Arc<PhysicalQuery>,
    exec: BoxedExecutor,
    state: ExecState,
    /// 出力列の型（`unknown` は text に直してある）。
    types: Vec<SqlType>,
}

impl std::fmt::Debug for SelectRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectRun")
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum PortalRun {
    Ready,
    Select(Box<SelectRun>),
    Stored(StoredRows),
    Done,
    Failed,
}

#[derive(Debug)]
pub(super) struct Portal {
    stmt: Arc<PreparedStmt>,
    txn_serial: u64,
    strategy: Strategy,
    /// 束縛済みの値（`$n` を読む XQ-3 が使う）。
    #[allow(dead_code)]
    params: Vec<Datum>,
    columns: Option<Vec<ColumnDesc>>,
    run: PortalRun,
}

#[derive(Debug, Default)]
struct BatchState {
    open: bool,
    executed: u32,
}

/// Extended Query の状態（`Session.ext`）。無名は名前 `""`。
#[derive(Debug, Default)]
pub(super) struct ExtStore {
    stmts: HashMap<String, Arc<PreparedStmt>>,
    portals: HashMap<String, Portal>,
    batch: BatchState,
}

impl ExtStore {
    /// トランザクションの終わり（コミットとロールバックの先頭）に呼ぶ。ポータルを全部捨てる。
    pub(super) fn end_of_transaction(&mut self) {
        self.portals.clear();
    }

    /// 無名の文と無名のポータルを捨てる（Simple Query の `Q` の先頭）。
    pub(super) fn discard_unnamed(&mut self) {
        self.stmts.remove("");
        self.portals.remove("");
    }
}

fn stmt_missing(name: &str) -> Error {
    let msg = if name.is_empty() {
        "unnamed prepared statement does not exist".to_owned()
    } else {
        format!("prepared statement \"{name}\" does not exist")
    };
    Error::new(sqlstate::INVALID_SQL_STATEMENT_NAME, msg)
}

fn portal_missing(name: &str) -> Error {
    Error::new(
        sqlstate::INVALID_CURSOR_NAME,
        format!("portal \"{name}\" does not exist"),
    )
}

fn in_failed() -> Error {
    Error::new(sqlstate::IN_FAILED_SQL_TRANSACTION, super::IN_FAILED_MSG)
}

fn not_yet(what: &str) -> Error {
    Error::not_supported(format!(
        "extended query protocol does not support {what} yet"
    ))
}

fn io_failure(e: &std::io::Error) -> Error {
    Error::new(
        sqlstate::CONNECTION_FAILURE,
        format!("could not send data to client: {e}"),
    )
    .with_severity(Severity::Fatal)
}

/// `invalid byte sequence for encoding "UTF8": 0x..`（`m5/04` §6.2.3）。
fn invalid_utf8(raw: &[u8], e: &std::str::Utf8Error) -> Error {
    let from = e.valid_up_to();
    let to = e.error_len().map_or(raw.len(), |n| from + n);
    let bytes: Vec<String> = raw[from..to].iter().map(|b| format!("0x{b:02x}")).collect();
    Error::new(
        sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"UTF8\": {}",
            bytes.join(" ")
        ),
    )
}

fn portal_label(name: &str) -> String {
    if name.is_empty() {
        "unnamed portal".to_owned()
    } else {
        format!("portal \"{name}\"")
    }
}

impl Session {
    // ----- 共通の部品 -------------------------------------------------------------------

    /// バッチの最初のメッセージの先頭（アイドル中に届いたキャンセルを捨てる。`m5/04` §5.3）。
    fn open_batch(&mut self) {
        if !self.ext.batch.open {
            self.interrupt.clear_cancel();
            self.ext.batch.open = true;
        }
    }

    fn arm_timeout(&self) {
        self.interrupt.set_statement_deadline(
            self.settings
                .statement_timeout()
                .and_then(|d| std::time::Instant::now().checked_add(d)),
        );
    }

    fn disarm_timeout(&self) {
        self.interrupt.set_statement_deadline(None);
    }

    /// `msg_*` のエラーの出口（`m5/04` §6.2.4）。トランザクションを中断する。失敗したブロックでは
    /// ポータルの表を残す（Failed の後の Execute は 34000 ではなく 25P02）。
    fn fail_ext(&mut self, e: Error, sql: Option<&str>) -> Error {
        let kept = (self.state == TxState::Block).then(|| std::mem::take(&mut self.ext.portals));
        let e = self.fail(e, sql);
        if let Some(p) = kept
            && self.state == TxState::Failed
        {
            self.ext.portals = p;
        }
        e
    }

    fn finish<T>(&mut self, r: Result<T>, sql: Option<&str>) -> Result<T> {
        r.map_err(|e| self.fail_ext(e, sql))
    }

    fn analysis_key(&self) -> Result<AnalysisKey> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| Error::internal("session has no database"))?;
        Ok(AnalysisKey {
            generation: db.cache.generation(),
            search_path: self.settings.search_path(),
            datestyle: self.settings.get("DateStyle").to_owned(),
            timezone: self.settings.get("TimeZone").to_owned(),
        })
    }

    fn lookup_stmt(&self, name: &str) -> Result<Arc<PreparedStmt>> {
        self.ext
            .stmts
            .get(name)
            .cloned()
            .ok_or_else(|| stmt_missing(name))
    }

    /// 鍵が変わっていたら止める（仮。再解析は XQ-5）。
    fn check_fresh(&self, ps: &PreparedStmt) -> Result<()> {
        match &ps.key {
            Some(k) if *k != self.analysis_key()? => {
                Err(not_yet("re-analysis of a stale prepared statement"))
            }
            _ => Ok(()),
        }
    }

    /// SELECT・DML・EXPLAIN を解析して、結果の列と（DML の）RETURNING の有無を返す。
    fn analyze_for_parse(
        &mut self,
        stmt: &Statement,
        declared: &[Oid],
    ) -> Result<(Kind, Option<Vec<ColumnDesc>>)> {
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        Self::check_not_poisoned(&cluster)?;
        self.with_stmt_env(&cluster, &db, |_txn, env| {
            for &o in declared {
                if o != 0 && env.catalog.type_by_oid(o).is_none() {
                    return Err(Error::new(
                        sqlstate::INTERNAL_ERROR,
                        format!("cache lookup failed for type {o}"),
                    ));
                }
            }
            let bound = crate::analyzer::analyze(stmt, env.catalog)?;
            let mut kind = Kind::of(stmt);
            if let Kind::Dml { .. } = kind {
                kind = Kind::Dml {
                    returning: bound.returns_rows(),
                };
            }
            let cols = match &bound {
                BoundStatement::Select(q) => Some(super::column_descs(&q.columns, env.catalog)),
                BoundStatement::Insert(i) => i
                    .returning
                    .as_ref()
                    .map(|r| super::column_descs(&r.columns, env.catalog)),
                BoundStatement::Update(u) => u
                    .returning
                    .as_ref()
                    .map(|r| super::column_descs(&r.columns, env.catalog)),
                BoundStatement::Delete(d) => d
                    .returning
                    .as_ref()
                    .map(|r| super::column_descs(&r.columns, env.catalog)),
                BoundStatement::Explain(_) => Some(vec![text_column("QUERY PLAN")]),
                _ => None,
            };
            Ok((kind, cols))
        })
    }

    fn show_columns(s: &sql::ast::ShowStmt) -> Vec<ColumnDesc> {
        match &s.target {
            ParamTarget::Name(n) => vec![text_column(n.to_ascii_lowercase())],
            ParamTarget::All => vec![
                text_column("name"),
                text_column("setting"),
                text_column("description"),
            ],
        }
    }

    // ----- Parse ----------------------------------------------------------------------

    pub fn msg_parse(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<()> {
        self.open_batch();
        self.arm_timeout();
        let r = self.parse_inner(name, sql, param_types);
        self.disarm_timeout();
        self.finish(r, Some(sql))
    }

    fn parse_inner(&mut self, name: &str, sql: &str, declared: &[Oid]) -> Result<()> {
        self.begin_implicit_if_idle();
        if name.is_empty() {
            self.ext.stmts.remove("");
        } else if self.ext.stmts.contains_key(name) {
            return Err(Error::new(
                sqlstate::DUPLICATE_PREPARED_STATEMENT,
                format!("prepared statement \"{name}\" already exists"),
            ));
        }
        let mut parsed = sql::parse(sql)?;
        if parsed.len() > 1 {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        let stmt = parsed.pop();
        if let Some(s) = &stmt {
            self.note_snapshot_use(s);
        }
        let mut kind = stmt.as_ref().map_or(Kind::Empty, Kind::of);
        if self.state == TxState::Failed && !matches!(kind, Kind::TxExit | Kind::Empty) {
            return Err(in_failed());
        }
        let mut columns = None;
        let mut key = None;
        if let Some(s) = &stmt {
            if kind.analyzed() {
                // 世代は解析の前に読む（解析中に進めば、次の Bind が古さを見つける）。
                key = Some(self.analysis_key()?);
                let (k, cols) = self.analyze_for_parse(s, declared)?;
                kind = k;
                columns = cols;
            } else if let Statement::Show(sh) = s {
                columns = Some(Self::show_columns(sh));
            }
        }
        // `$n` の推論は XQ-3。それまでは宣言の型だけを使い、0（未指定）は決められない。
        if let Some(i) = declared.iter().position(|&o| o == 0) {
            return Err(Error::new(
                sqlstate::INDETERMINATE_DATATYPE,
                format!("could not determine data type of parameter ${}", i + 1),
            ));
        }
        let ps = PreparedStmt {
            name: name.to_owned(),
            sql: Arc::from(sql),
            stmt: stmt.map(Arc::new),
            kind,
            param_types: declared.to_vec(),
            columns: columns.map(Arc::from),
            key,
        };
        self.ext.stmts.insert(name.to_owned(), Arc::new(ps));
        Ok(())
    }

    // ----- Bind -----------------------------------------------------------------------

    pub fn msg_bind(
        &mut self,
        portal: &str,
        stmt: &str,
        param_formats: &[Format],
        params: &[Option<&[u8]>],
        result_formats: &[Format],
    ) -> Result<()> {
        self.open_batch();
        let ps = match self.lookup_stmt(stmt) {
            Ok(p) => p,
            Err(e) => {
                self.begin_implicit_if_idle();
                return Err(self.fail_ext(e, None));
            }
        };
        let r = self.bind_inner(portal, &ps, param_formats, params, result_formats);
        self.finish(r, Some(&ps.sql))
    }

    fn bind_inner(
        &mut self,
        portal: &str,
        ps: &Arc<PreparedStmt>,
        param_formats: &[Format],
        params: &[Option<&[u8]>],
        result_formats: &[Format],
    ) -> Result<()> {
        self.begin_implicit_if_idle();
        if self.state == TxState::Failed && !(ps.kind == Kind::TxExit && params.is_empty()) {
            return Err(in_failed());
        }
        let protocol = |m: String| Error::new(sqlstate::PROTOCOL_VIOLATION, m);
        if param_formats.len() > 1 && param_formats.len() != params.len() {
            return Err(protocol(format!(
                "bind message has {} parameter formats but {} parameters",
                param_formats.len(),
                params.len()
            )));
        }
        if params.len() != ps.param_types.len() {
            return Err(protocol(format!(
                "bind message supplies {} parameters, but prepared statement \"{}\" requires {}",
                params.len(),
                ps.name,
                ps.param_types.len()
            )));
        }
        if !portal.is_empty() && self.ext.portals.contains_key(portal) {
            return Err(Error::new(
                sqlstate::DUPLICATE_CURSOR,
                format!("cursor \"{portal}\" already exists"),
            ));
        }
        self.ext.portals.remove(portal);
        self.check_fresh(ps)?;
        let values = self.bind_params(portal, ps, param_formats, params)?;
        let ncols = ps.columns.as_ref().map_or(0, |c| c.len());
        if result_formats.len() > 1 && result_formats.len() != ncols {
            return Err(protocol(format!(
                "bind message has {} result formats but query has {} columns",
                result_formats.len(),
                ncols
            )));
        }
        let columns = ps.columns.as_ref().map(|cols| {
            cols.iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut c = c.clone();
                    c.format = match result_formats.len() {
                        0 => Format::Text,
                        1 => result_formats[0],
                        _ => result_formats[i],
                    };
                    c
                })
                .collect::<Vec<_>>()
        });
        let strategy = match ps.kind {
            Kind::Empty => Strategy::Empty,
            Kind::Query if Self::streams(ps) => Strategy::OneSelect,
            Kind::Query | Kind::Explain | Kind::Show | Kind::Dml { returning: true } => {
                Strategy::Stored
            }
            _ => Strategy::Multi,
        };
        let run = match (strategy, &ps.stmt) {
            (Strategy::OneSelect, Some(stmt)) => {
                PortalRun::Select(Box::new(self.open_select(stmt)?))
            }
            _ => PortalRun::Ready,
        };
        self.ext.portals.insert(
            portal.to_owned(),
            Portal {
                stmt: Arc::clone(ps),
                txn_serial: self.txn_serial,
                strategy,
                params: values,
                columns,
                run,
            },
        );
        Ok(())
    }

    fn bind_params(
        &self,
        portal: &str,
        ps: &PreparedStmt,
        formats: &[Format],
        params: &[Option<&[u8]>],
    ) -> Result<Vec<Datum>> {
        if params.iter().all(Option::is_none) {
            return Ok(vec![Datum::Null; params.len()]);
        }
        let cluster = self.cluster()?;
        let zones = cluster.zones();
        let dt = self.settings.datetime_settings(zones);
        let env = self
            .settings
            .type_env(&dt, zones, self.txn.started_at, None);
        let mut out = Vec::with_capacity(params.len());
        for (i, raw) in params.iter().enumerate() {
            let Some(raw) = raw else {
                out.push(Datum::Null);
                continue;
            };
            let fmt = match formats.len() {
                0 => Format::Text,
                1 => formats[0],
                _ => formats[i],
            };
            let ctx = |e: Error| {
                e.with_context(format!(
                    "{} parameter ${} = '...'",
                    portal_label(portal),
                    i + 1
                ))
            };
            if fmt == Format::Binary {
                return Err(ctx(not_yet("binary parameters")));
            }
            let s = std::str::from_utf8(raw).map_err(|e| ctx(invalid_utf8(raw, &e)))?;
            if raw.contains(&0) {
                return Err(ctx(Error::new(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\": 0x00",
                )));
            }
            let d = io::input_text_env(s, SqlType::of(ps.param_types[i]), &env).map_err(ctx)?;
            out.push(d);
        }
        Ok(out)
    }

    // ----- Describe / Close -----------------------------------------------------------

    pub fn msg_describe_statement(&mut self, name: &str) -> Result<StatementDescription> {
        self.open_batch();
        self.begin_implicit_if_idle();
        let ps = match self.lookup_stmt(name) {
            Ok(p) => p,
            Err(e) => return Err(self.fail_ext(e, None)),
        };
        let r = self.describe_stmt_inner(&ps);
        self.finish(r, Some(&ps.sql))
    }

    fn describe_stmt_inner(&self, ps: &PreparedStmt) -> Result<StatementDescription> {
        if self.state == TxState::Failed && ps.columns.is_some() && ps.kind != Kind::TxExit {
            return Err(in_failed());
        }
        self.check_fresh(ps)?;
        Ok(StatementDescription {
            param_types: ps.param_types.clone(),
            columns: ps.columns.as_ref().map(|c| c.to_vec()),
        })
    }

    pub fn msg_describe_portal(&mut self, name: &str) -> Result<Option<Vec<ColumnDesc>>> {
        self.open_batch();
        self.begin_implicit_if_idle();
        let r = match self.ext.portals.get(name) {
            None => Err(portal_missing(name)),
            Some(p) if self.state == TxState::Failed && p.columns.is_some() => Err(in_failed()),
            Some(p) => Ok(p.columns.clone()),
        };
        self.finish(r, None)
    }

    /// 存在しない名前でも成功する。
    pub fn msg_close_statement(&mut self, name: &str) {
        self.ext.stmts.remove(name);
    }

    pub fn msg_close_portal(&mut self, name: &str) {
        self.ext.portals.remove(name);
    }

    // ----- Execute --------------------------------------------------------------------

    /// `max_rows` が 0 なら全行。
    pub fn msg_execute(
        &mut self,
        portal: &str,
        max_rows: u32,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecOutcome> {
        self.open_batch();
        self.begin_implicit_if_idle();
        let Some(mut p) = self.ext.portals.remove(portal) else {
            return Err(self.fail_ext(portal_missing(portal), None));
        };
        let stmt = Arc::clone(&p.stmt);
        if p.txn_serial != self.txn_serial {
            return Err(self.fail_ext(portal_missing(portal), None));
        }
        let r = if self.state == TxState::Failed && stmt.kind != Kind::TxExit {
            Err(in_failed())
        } else {
            self.arm_timeout();
            let r = self.execute_portal(portal, &mut p, max_rows, sink);
            self.disarm_timeout();
            r
        };
        if r.is_err() && !matches!(p.run, PortalRun::Done) {
            p.run = PortalRun::Failed;
        }
        let done = matches!(r, Ok(ExecOutcome::Complete(_) | ExecOutcome::EmptyQuery));
        // COMMIT などでトランザクションが終わったポータルは戻さない。
        if self.state != TxState::Idle && p.txn_serial == self.txn_serial {
            self.ext.portals.insert(portal.to_owned(), p);
        }
        if done {
            self.ext.batch.executed += 1;
        }
        self.finish(r, Some(&stmt.sql))
    }

    fn execute_portal(
        &mut self,
        name: &str,
        p: &mut Portal,
        max_rows: u32,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecOutcome> {
        if matches!(p.run, PortalRun::Failed | PortalRun::Done) {
            return Err(Error::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("portal \"{name}\" cannot be run"),
            ));
        }
        let Some(stmt) = p.stmt.stmt.clone() else {
            return Ok(ExecOutcome::EmptyQuery);
        };
        if matches!(p.run, PortalRun::Select(_)) {
            let PortalRun::Select(mut run) = std::mem::replace(&mut p.run, PortalRun::Failed)
            else {
                unreachable!("checked above");
            };
            if p.columns
                .as_ref()
                .is_some_and(|c| c.iter().any(|c| c.format == Format::Binary))
            {
                return Err(not_yet("binary result formats"));
            }
            let outcome = self.run_select(&mut run, max_rows, sink)?;
            p.run = if matches!(outcome, ExecOutcome::Suspended) {
                PortalRun::Select(run)
            } else {
                // 尽きた後の再実行は `SELECT 0`（XQ-D4）。
                PortalRun::Stored(StoredRows {
                    rows: VecDeque::new(),
                    tag: "SELECT ".to_owned(),
                    counted: true,
                })
            };
            return Ok(outcome);
        }
        if matches!(p.run, PortalRun::Ready) {
            self.check_fresh(&p.stmt)?;
            self.stmt_started_at = pg_now_micros();
            let mut out = Output::default();
            let outcome = self.exec_statement(&stmt, &mut out)?;
            let RunOutcome::Tag(tag) = outcome else {
                return Err(not_yet("COPY"));
            };
            for n in &out.notices {
                self.send_notice(n, sink).map_err(|e| io_failure(&e))?;
            }
            if p.strategy == Strategy::Stored {
                let (prefix, counted) = match tag.rfind(' ') {
                    Some(i) if tag[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
                        (tag[..=i].to_owned(), true)
                    }
                    _ => (tag, false),
                };
                let rows = out
                    .rows
                    .into_iter()
                    .map(|r| r.into_iter().map(|v| v.map(String::into_bytes)).collect())
                    .collect();
                p.run = PortalRun::Stored(StoredRows {
                    rows,
                    tag: prefix,
                    counted,
                });
            } else {
                p.run = PortalRun::Done;
                return Ok(ExecOutcome::Complete(tag));
            }
        }
        let PortalRun::Stored(stored) = &mut p.run else {
            return Err(Error::internal("portal in an unexpected state"));
        };
        let binary = p
            .columns
            .as_ref()
            .is_some_and(|c| c.iter().any(|c| c.format == Format::Binary));
        if binary && !stored.rows.is_empty() {
            return Err(not_yet("binary result formats"));
        }
        let mut sent: u64 = 0;
        while max_rows == 0 || sent < u64::from(max_rows) {
            let Some(row) = stored.rows.pop_front() else {
                break;
            };
            sink.data_row_raw(&row).map_err(|e| io_failure(&e))?;
            sent += 1;
        }
        if max_rows != 0 && sent == u64::from(max_rows) {
            return Ok(ExecOutcome::Suspended);
        }
        Ok(ExecOutcome::Complete(if stored.counted {
            format!("{}{sent}", stored.tag)
        } else {
            stored.tag.clone()
        }))
    }

    // ----- OneSelect（Portal の部分実行。`m5/04` §5.9）---------------------------------

    /// Bind で計画と実行器を作って持ち越せる文か（ロック句のない SELECT）。
    fn streams(ps: &PreparedStmt) -> bool {
        matches!(ps.stmt.as_deref(), Some(Statement::Query(q)) if q.locking.is_empty())
    }

    /// 共有ストレージバリアを取って `f` を実行し、文の後始末（シーケンス・WAL・ピン）をする。
    /// バリアは `f` の間だけ持つ（メッセージの間は持たない）。
    fn under_barrier<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let cluster = self.cluster()?;
        Self::check_not_poisoned(&cluster)?;
        let result = f(self);
        let lsn = self.seq_state.borrow_mut().end_statement();
        self.txn.note_wal(lsn);
        self.flush_modstat();
        Self::flush_deferred_unlinks(&cluster);
        buffer::assert_no_pins();
        result
    }

    /// Bind の `OneSelect`: リレーションロック → スナップショット。計画と実行器は最初の Execute で作る
    /// （定数式のエラーなどを PostgreSQL と同じく Execute で返すため。`m5/04` §5.8）。
    fn open_select(&mut self, stmt: &Arc<Statement>) -> Result<SelectRun> {
        let cluster = self.cluster()?;
        Self::check_not_poisoned(&cluster)?;
        // Repeatable Read の最初のスナップショットはロックの前（RW-D10）。RC の文のスナップショットはロックの後。
        self.begin_statement_snapshot(stmt);
        self.lock_for_statement(stmt)?;
        Self::check_not_poisoned(&cluster)?;
        let procs = Arc::clone(cluster.txn_manager().procs());
        let (snapshot, registered) = if let Some(first) = &self.txn.xact_snapshot {
            (first.for_statement(self.txn.xid, self.txn.cid), None)
        } else {
            let reg = procs.take_snapshot(self.txn.xid, self.txn.cid);
            ((*reg).clone(), Some(reg))
        };
        Ok(SelectRun {
            stmt: Arc::clone(stmt),
            snapshot,
            _registered: registered,
            planned: None,
        })
    }

    /// 解析・計画して実行器を作る（最初の Execute）。
    fn plan_select(txn: &mut Transaction, env: &StmtEnv<'_>, stmt: &Statement) -> Result<Planned> {
        let mut bound = crate::analyzer::analyze(stmt, env.catalog)?;
        if !matches!(bound, BoundStatement::Select(_)) {
            return Err(Error::internal("portal statement is not a SELECT"));
        }
        env.fold_constants(&mut bound)?;
        let query = env.plan(&bound, false, false)?;
        let exec = super::build_executor(&query, None);
        let state = ExecCtx::new(env.exec_env(None), txn, &query).take_state();
        // `unknown` は text として扱う。
        let types = query
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
        Ok(Planned {
            query: Arc::new(query),
            exec,
            state,
            types,
        })
    }

    /// Execute の `OneSelect`: `max_rows` 行（0 なら残り全部）を `sink` へ流す。
    /// 尽きたら `Complete("SELECT n")`（n はこの Execute で返した行数）、数に達したら `Suspended`。
    fn run_select(
        &mut self,
        run: &mut SelectRun,
        max_rows: u32,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecOutcome> {
        self.stmt_started_at = pg_now_micros();
        let cluster = self.cluster()?;
        let Some(db) = self.db.clone() else {
            return Err(Error::internal("session has a cluster but no database"));
        };
        let outcome = self.under_barrier(|this| {
            this.with_stmt_env(&cluster, &db, |txn, env| {
                let SelectRun {
                    stmt,
                    snapshot,
                    planned,
                    ..
                } = run;
                if planned.is_none() {
                    *planned = Some(Self::plan_select(txn, env, stmt)?);
                }
                let Some(Planned {
                    query,
                    exec,
                    state,
                    types,
                }) = planned.as_mut()
                else {
                    return Err(Error::internal("select was just planned"));
                };
                let mut exec_env = env.exec_env(None);
                exec_env.snapshot = snapshot;
                let mut ctx = ExecCtx::new(exec_env, txn, query);
                ctx.restore_state(std::mem::take(state));
                let r = Self::pump_select(&mut ctx, exec.as_mut(), types, env, max_rows, sink);
                *state = ctx.take_state();
                r
            })
        })?;
        if matches!(outcome, ExecOutcome::Complete(_)) {
            self.txn.command_counter_increment()?;
        }
        Ok(outcome)
    }

    fn pump_select(
        ctx: &mut ExecCtx<'_>,
        exec: &mut dyn crate::executor::Executor,
        types: &[SqlType],
        env: &StmtEnv<'_>,
        max_rows: u32,
        sink: &mut dyn ResultSink,
    ) -> Result<ExecOutcome> {
        let mut sent: u64 = 0;
        loop {
            if max_rows != 0 && sent == u64::from(max_rows) {
                return Ok(ExecOutcome::Suspended);
            }
            ctx.check_interrupts()?;
            let Some(row) = exec.next(ctx)? else {
                return Ok(ExecOutcome::Complete(format!("SELECT {sent}")));
            };
            let raw: Vec<Option<Vec<u8>>> = super::row_text(&row, types, env.type_env)?
                .into_iter()
                .map(|v| v.map(String::into_bytes))
                .collect();
            sink.data_row_raw(&raw).map_err(|e| io_failure(&e))?;
            sent += 1;
        }
    }

    // ----- Sync と後始末 ----------------------------------------------------------------

    /// Sync: 暗黙のトランザクションを閉じる（コミットの失敗は `Err`）。
    pub fn msg_sync(&mut self) -> Result<()> {
        self.ext.batch = BatchState::default();
        self.disarm_timeout();
        if self.state == TxState::Implicit
            && let Err(e) = self.commit_transaction()
        {
            return Err(self.fail(e, None));
        }
        Ok(())
    }

    /// `ErrorResponse` を送った直後に呼ぶ。暗黙のトランザクションを中断し、ポータルを破棄する。
    /// ブロックの中なら Failed にする（冪等）。
    pub fn msg_abort_implicit(&mut self) {
        self.disarm_timeout();
        match self.state {
            TxState::Implicit => self.rollback_transaction(),
            TxState::Block => {
                // The XID and locks stay until ROLLBACK (as in PostgreSQL).
                self.state = TxState::Failed;
                // 実行器とスナップショットは手放す（ポータル自体はトランザクションの終わりまで残る）。
                for p in self.ext.portals.values_mut() {
                    if matches!(p.run, PortalRun::Select(_)) {
                        p.run = PortalRun::Failed;
                    }
                }
            }
            TxState::Idle | TxState::Failed => {}
        }
    }

    /// Sync の処理の最後（ReadyForQuery の直前）に server が呼ぶ。前回送った値から変わった
    /// `ParameterStatus` を返し、送ったものとして記録する。
    pub fn pending_parameter_status(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, value) in self.settings.reported_values() {
            if self.reported.get(name) != Some(&value) {
                self.reported.insert(name, value.clone());
                out.push((name.to_owned(), value));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::session::{Notice, TransactionStatus};
    use crate::testing::TestCluster;

    #[derive(Default)]
    struct Rows(Vec<Vec<Option<String>>>);

    impl ResultSink for Rows {
        fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
            Ok(())
        }
        fn data_row(&mut self, v: &[Option<String>]) -> std::io::Result<()> {
            self.0.push(v.to_vec());
            Ok(())
        }
        fn command_complete(&mut self, _: &str) -> std::io::Result<()> {
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

    fn setup() -> (TestCluster, Session) {
        let tc = TestCluster::new();
        let s = tc.session("postgres").unwrap();
        (tc, s)
    }

    fn simple(s: &mut Session, q: &str) {
        s.execute_simple(q, &mut Rows::default()).unwrap();
    }

    fn code<T: std::fmt::Debug>(r: Result<T>) -> &'static str {
        r.unwrap_err().sqlstate.0
    }

    fn pb(s: &mut Session, name: &str, sql: &str) {
        s.msg_parse(name, sql, &[]).unwrap();
        s.msg_bind(name, name, &[], &[], &[]).unwrap();
    }

    fn exec(s: &mut Session, p: &str, n: u32) -> (ExecOutcome, Vec<String>) {
        let mut sink = Rows::default();
        let o = s.msg_execute(p, n, &mut sink).unwrap();
        let rows = sink.0.iter().map(|r| r[0].clone().unwrap()).collect();
        (o, rows)
    }

    #[test]
    fn basic_flow_with_describe_and_sync() {
        let (_tc, mut s) = setup();
        s.msg_parse("", "SELECT 1 AS a, 'x' AS b", &[]).unwrap();
        let d = s.msg_describe_statement("").unwrap();
        assert!(d.param_types.is_empty());
        let names: Vec<_> = d.columns.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["a", "b"]);
        s.msg_bind("", "", &[], &[], &[Format::Text]).unwrap();
        assert_eq!(s.msg_describe_portal("").unwrap().unwrap().len(), 2);
        let (o, rows) = exec(&mut s, "", 0);
        assert_eq!(o, ExecOutcome::Complete("SELECT 1".into()));
        assert_eq!(rows, ["1"]);
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        s.msg_sync().unwrap();
        // 無名の文は Sync の後も残る。ポータルは消える。
        s.msg_bind("", "", &[], &[], &[]).unwrap();
        s.msg_sync().unwrap();
        assert_eq!(code(s.msg_describe_portal("")), "34000");
    }

    #[test]
    fn result_format_counts_and_suspension() {
        let (_tc, mut s) = setup();
        s.msg_parse("q", "SELECT g FROM generate_series(1,4) g", &[])
            .unwrap();
        let e = s
            .msg_bind("p", "q", &[], &[], &[Format::Text; 3])
            .unwrap_err();
        assert_eq!(e.sqlstate.0, "08P01");
        assert!(
            e.message
                .contains("3 result formats but query has 1 columns")
        );
        s.msg_sync().unwrap();
        s.msg_parse("q2", "SELECT g FROM generate_series(1,4) g", &[])
            .unwrap();
        s.msg_bind("p", "q2", &[], &[], &[]).unwrap();
        let (o, r) = exec(&mut s, "p", 2);
        assert_eq!(
            (o, r),
            (ExecOutcome::Suspended, vec!["1".into(), "2".into()])
        );
        let (o, r) = exec(&mut s, "p", 2);
        assert_eq!(
            (o, r),
            (ExecOutcome::Suspended, vec!["3".into(), "4".into()])
        );
        let (o, r) = exec(&mut s, "p", 2);
        assert_eq!((o, r), (ExecOutcome::Complete("SELECT 0".into()), vec![]));
    }

    #[test]
    fn portal_lifetime() {
        let (_tc, mut s) = setup();
        pb(&mut s, "a", "SELECT 1");
        assert_eq!(code(s.msg_bind("a", "a", &[], &[], &[])), "42P03");
        s.msg_sync().unwrap();
        assert_eq!(code(s.msg_describe_portal("a")), "34000");
        s.msg_sync().unwrap();
        // ブロックの中では Sync の後も残る。COMMIT で消える。
        simple(&mut s, "BEGIN");
        pb(&mut s, "b", "SELECT 1");
        s.msg_sync().unwrap();
        assert!(s.msg_describe_portal("b").is_ok());
        s.msg_parse("c", "COMMIT", &[]).unwrap();
        s.msg_bind("pc", "c", &[], &[], &[]).unwrap();
        let (o, _) = exec(&mut s, "pc", 0);
        assert_eq!(o, ExecOutcome::Complete("COMMIT".into()));
        s.msg_sync().unwrap();
        assert_eq!(code(s.msg_describe_portal("b")), "34000");
        assert_eq!(code(s.msg_describe_portal("pc")), "34000");
        // 名前付きの文の重複。Close の後は作れる。
        s.msg_parse("dup", "SELECT 1", &[]).unwrap();
        assert_eq!(code(s.msg_parse("dup", "SELECT 2", &[])), "42P05");
        s.msg_sync().unwrap();
        s.msg_close_statement("dup");
        s.msg_close_statement("nope");
        s.msg_parse("dup", "SELECT 2", &[]).unwrap();
        // 無名の文は Q で消える。
        s.msg_parse("", "SELECT 1", &[]).unwrap();
        simple(&mut s, "SELECT 1");
        assert_eq!(code(s.msg_describe_statement("")), "26000");
    }

    #[test]
    fn pipeline_error_rolls_back_the_whole_batch() {
        let (_tc, mut s) = setup();
        simple(&mut s, "CREATE TABLE t (a int PRIMARY KEY)");
        pb(&mut s, "i1", "INSERT INTO t VALUES (1)");
        exec(&mut s, "i1", 0);
        pb(&mut s, "i2", "INSERT INTO t VALUES (1)");
        assert_eq!(code(s.msg_execute("i2", 0, &mut Rows::default())), "23505");
        s.msg_abort_implicit();
        s.msg_sync().unwrap();
        let mut sink = Rows::default();
        s.execute_simple("SELECT count(*) FROM t", &mut sink)
            .unwrap();
        assert_eq!(sink.0[0][0].as_deref(), Some("0"));
        // Sync を挟めば最初の INSERT は残る。
        pb(&mut s, "i3", "INSERT INTO t VALUES (1)");
        let (o, _) = exec(&mut s, "i3", 0);
        assert_eq!(o, ExecOutcome::Complete("INSERT 0 1".into()));
        s.msg_sync().unwrap();
        let mut sink = Rows::default();
        s.execute_simple("SELECT count(*) FROM t", &mut sink)
            .unwrap();
        assert_eq!(sink.0[0][0].as_deref(), Some("1"));
    }

    #[test]
    fn implicit_transaction_spans_messages_and_q_takes_it_over() {
        let (_tc, mut s) = setup();
        simple(&mut s, "CREATE TABLE t (a int)");
        pb(&mut s, "i", "INSERT INTO t VALUES (7)");
        exec(&mut s, "i", 0);
        let mut sink = Rows::default();
        s.execute_simple("SELECT count(*) FROM t", &mut sink)
            .unwrap();
        assert_eq!(sink.0[0][0].as_deref(), Some("1"));
        s.msg_sync().unwrap();
        // BEGIN を Execute するとブロックになり、Sync でコミットされない。
        pb(&mut s, "b", "BEGIN");
        exec(&mut s, "b", 0);
        s.msg_sync().unwrap();
        assert_eq!(s.transaction_status(), TransactionStatus::InBlock);
        simple(&mut s, "ROLLBACK");
    }

    #[test]
    fn failed_block_rules() {
        let (_tc, mut s) = setup();
        simple(&mut s, "BEGIN");
        pb(&mut s, "sel", "SELECT 1");
        s.msg_parse("ins", "SELECT 1/0", &[]).unwrap();
        s.msg_bind("bad", "ins", &[], &[], &[]).unwrap();
        assert_eq!(code(s.msg_execute("bad", 0, &mut Rows::default())), "22012");
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        // ポータルは残るので 34000 ではなく 25P02。
        assert_eq!(code(s.msg_execute("sel", 0, &mut Rows::default())), "25P02");
        assert_eq!(code(s.msg_execute("zzz", 0, &mut Rows::default())), "34000");
        assert_eq!(code(s.msg_parse("x", "SELECT 2", &[])), "25P02");
        assert_eq!(code(s.msg_parse("x", "SELEC", &[])), "42601");
        assert_eq!(code(s.msg_describe_portal("sel")), "25P02");
        assert_eq!(code(s.msg_bind("p2", "sel", &[], &[], &[])), "25P02");
        s.msg_close_portal("sel");
        s.msg_close_statement("sel");
        s.msg_sync().unwrap();
        assert_eq!(s.transaction_status(), TransactionStatus::Failed);
        s.msg_parse("rb", "ROLLBACK", &[]).unwrap();
        s.msg_bind("prb", "rb", &[], &[], &[]).unwrap();
        let (o, _) = exec(&mut s, "prb", 0);
        assert_eq!(o, ExecOutcome::Complete("ROLLBACK".into()));
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
    }

    #[test]
    fn empty_query() {
        let (_tc, mut s) = setup();
        for q in ["", ";", "-- c"] {
            s.msg_parse("e", q, &[]).unwrap();
            let d = s.msg_describe_statement("e").unwrap();
            assert_eq!(d.columns, None);
            s.msg_bind("p", "e", &[], &[], &[]).unwrap();
            assert_eq!(exec(&mut s, "p", 0).0, ExecOutcome::EmptyQuery);
            s.msg_close_statement("e");
            s.msg_sync().unwrap();
        }
    }

    #[test]
    fn bind_errors() {
        let (_tc, mut s) = setup();
        s.msg_parse("s", "SELECT 1", &[]).unwrap();
        let e = s.msg_bind("p", "s", &[], &[Some(b"1")], &[]).unwrap_err();
        assert_eq!(e.sqlstate.0, "08P01");
        assert_eq!(
            e.message,
            "bind message supplies 1 parameters, but prepared statement \"s\" requires 0"
        );
        s.msg_sync().unwrap();
        assert_eq!(code(s.msg_bind("p", "nope", &[], &[], &[])), "26000");
        s.msg_sync().unwrap();
        s.msg_parse("i", "SELECT 1", &[23, 25]).unwrap();
        let e = s
            .msg_bind("p", "i", &[Format::Text; 3], &[None, None], &[])
            .unwrap_err();
        assert_eq!(
            e.message,
            "bind message has 3 parameter formats but 2 parameters"
        );
        s.msg_sync().unwrap();
        let e = s
            .msg_bind("p", "i", &[], &[Some(b"x"), None], &[])
            .unwrap_err();
        assert_eq!(e.sqlstate.0, "22P02");
        assert!(e.context().unwrap().contains("portal \"p\" parameter $1"));
        s.msg_sync().unwrap();
        let e = s
            .msg_bind("", "i", &[], &[Some(b"1"), Some(&[0xff])], &[])
            .unwrap_err();
        assert_eq!(e.sqlstate.0, "22021");
        assert!(e.message.ends_with("0xff"));
        s.msg_sync().unwrap();
        let e = s
            .msg_bind("", "i", &[], &[Some(b"1"), Some(b"a\0b")], &[])
            .unwrap_err();
        assert!(e.message.ends_with("0x00"));
        s.msg_sync().unwrap();
        s.msg_bind("p", "i", &[], &[Some(b"1"), None], &[]).unwrap();
        // 未指定の型・複数の文
        assert_eq!(code(s.msg_parse("u", "SELECT 1", &[0])), "42P18");
        s.msg_sync().unwrap();
        assert_eq!(code(s.msg_parse("m", "SELECT 1; SELECT 2", &[])), "42601");
    }

    #[test]
    fn error_resets_and_stale_key_stops() {
        let (_tc, mut s) = setup();
        s.msg_parse("q", "SELECT 1", &[]).unwrap();
        simple(&mut s, "CREATE TABLE gen_bump (a int)");
        assert_eq!(code(s.msg_bind("p", "q", &[], &[], &[])), "0A000");
        s.msg_sync().unwrap();
        // 構文エラーは位置つき。暗黙のトランザクションは中断される。
        let e = s.msg_parse("x", "SELECT FROM FROM", &[]).unwrap_err();
        assert_eq!(e.sqlstate.0, "42601");
        assert!(e.position.is_some());
        assert_eq!(s.transaction_status(), TransactionStatus::Idle);
        s.msg_abort_implicit();
        s.msg_sync().unwrap();
    }

    #[test]
    fn show_and_parameter_status() {
        let (_tc, mut s) = setup();
        s.msg_parse("", "SHOW application_name", &[]).unwrap();
        assert_eq!(
            s.msg_describe_statement("").unwrap().columns.unwrap().len(),
            1
        );
        s.msg_bind("", "", &[], &[], &[]).unwrap();
        let (o, _) = exec(&mut s, "", 0);
        assert_eq!(o, ExecOutcome::Complete("SHOW".into()));
        s.msg_sync().unwrap();
        let _ = s.pending_parameter_status();
        pb(&mut s, "set", "SET application_name = 'zz'");
        exec(&mut s, "set", 0);
        s.msg_sync().unwrap();
        let p = s.pending_parameter_status();
        assert!(p.contains(&("application_name".into(), "zz".into())));
        assert!(s.pending_parameter_status().is_empty());
    }

    #[test]
    fn suspended_select_keeps_its_snapshot_and_portals_interleave() {
        let (_tc, mut s) = setup();
        simple(&mut s, "CREATE TABLE t (a int)");
        simple(&mut s, "INSERT INTO t VALUES (1), (2), (3)");
        simple(&mut s, "BEGIN");
        pb(&mut s, "p1", "SELECT a FROM t ORDER BY a");
        pb(&mut s, "p2", "SELECT a * 10 FROM t ORDER BY a");
        // Bind の後の書き込みは、中断中のポータルから見えない（XQ-D3）。
        simple(&mut s, "INSERT INTO t VALUES (4)");
        let (o, r) = exec(&mut s, "p1", 2);
        assert_eq!(
            (o, r),
            (ExecOutcome::Suspended, vec!["1".into(), "2".into()])
        );
        let (o, r) = exec(&mut s, "p2", 1);
        assert_eq!((o, r), (ExecOutcome::Suspended, vec!["10".into()]));
        let (o, r) = exec(&mut s, "p1", 5);
        assert_eq!(
            (o, r),
            (ExecOutcome::Complete("SELECT 1".into()), vec!["3".into()])
        );
        let (o, r) = exec(&mut s, "p2", 0);
        assert_eq!(
            (o, r),
            (
                ExecOutcome::Complete("SELECT 2".into()),
                vec!["20".into(), "30".into()]
            )
        );
        // 完了後の再実行は 0 行。
        let (o, r) = exec(&mut s, "p1", 0);
        assert_eq!((o, r), (ExecOutcome::Complete("SELECT 0".into()), vec![]));
        simple(&mut s, "COMMIT");
        assert_eq!(code(s.msg_describe_portal("p1")), "34000");
    }

    #[test]
    fn select_error_mid_stream_fails_the_portal() {
        let (_tc, mut s) = setup();
        pb(
            &mut s,
            "p",
            "SELECT 10 / (3 - g) FROM generate_series(1, 5) g",
        );
        let mut sink = Rows::default();
        assert_eq!(code(s.msg_execute("p", 0, &mut sink)), "22012");
        // 途中までの行はシンクに届いている。
        assert_eq!(sink.0.len(), 2);
    }
}
