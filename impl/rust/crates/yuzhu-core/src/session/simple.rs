//! Simple Query の実行（`m1.md` §5.4）: 文の振り分け、エラー報告、SET / SHOW、結果の送出。

#![allow(clippy::wildcard_imports)]

use super::*;

impl Session {
    /// The statement loop over already-parsed statements.
    pub(super) fn run(
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

    pub(super) fn run_statements(
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
    pub(super) fn run_from(
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
            self.begin_implicit_if_idle();
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

    /// Aborts the current transaction (implicit → idle, block → failed) and
    /// sends the error.
    pub(super) fn report_error(
        &mut self,
        e: Error,
        sql: &str,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        let e = self.fail(e, Some(sql));
        sink.error(&e)?;
        self.flush_parameter_status(sink)
    }

    /// Turns `e` into the error to send: poisons the cluster for a PANIC, marks the
    /// session closing for a FATAL, aborts the transaction (implicit → idle,
    /// block → failed) and resolves the error position against `sql`. Shared by
    /// Simple Query (`report_error`) and Extended Query (`m5/04` §6.2.4, C8).
    pub(super) fn fail(&mut self, mut e: Error, sql: Option<&str>) -> Error {
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
        if let Some(sql) = sql {
            e.resolve_position(sql);
        }
        e
    }

    // ----- parts shared with Extended Query (`m5/04` §6.2.5, C8) -------------------------

    /// Opens the implicit transaction when the session is idle.
    pub(super) fn begin_implicit_if_idle(&mut self) {
        if self.state == TxState::Idle {
            self.begin_transaction(TxState::Implicit);
        }
    }

    #[allow(dead_code)] // XQ-2 が使う。
    /// A short-lived snapshot for catalog lookups (parse, describe).
    /// This is not registered in the `ProcArray` yet: LK-3 switches it to a
    /// `RegisteredSnapshot` (`m5/04` D12).
    pub(super) fn catalog_snapshot(&self) -> Result<Snapshot> {
        let cluster = self.cluster()?;
        Ok(cluster.txn_manager().snapshot(self.txn.xid, self.txn.cid))
    }

    #[allow(dead_code)] // XQ-2 が使う。
    /// Takes the writer lock and an XID for a statement that writes (step 1 of
    /// `m2.md` §5.2), unless this transaction has them already. `tag` names the
    /// statement in the error of a read-only transaction (`25006`).
    pub(super) fn assign_xid_for_write(&mut self, tag: &'static str) -> Result<()> {
        if self.chars.read_only {
            return Err(read_only_error(tag));
        }
        if self.txn.guard.is_some() {
            return Ok(());
        }
        let cluster = self.cluster()?;
        Self::check_not_poisoned(&cluster)?;
        let (xid, guard) = mgr_begin_write(cluster.txn_manager(), self.id)?;
        self.txn.xid = Some(xid);
        self.txn.guard = Some(guard);
        // The cluster may have been poisoned while we waited for the lock.
        Self::check_not_poisoned(&cluster)
    }

    #[allow(dead_code)] // XQ-2 が使う。
    /// The session-side inputs of a statement's execution context. The pieces that
    /// borrow the catalog (`TypeEnv`, `EvalCtx`) are built by the caller.
    pub(super) fn exec_ctx_parts(&mut self) -> ExecParts<'_> {
        ExecParts {
            info: self.session_info(),
            interrupts: Arc::clone(&self.interrupt),
            settings: &self.settings,
            backend: BackendId(self.id),
            role: &self.role,
            pid: self.backend_pid(),
            stmt_started_at: self.stmt_started_at,
            xid: self.txn.xid.map(|x| x.0),
            lock_timeout: self.settings.lock_timeout(),
            deadlock_timeout: self.settings.deadlock_timeout(),
            password: self.settings.password_policy(),
        }
    }

    /// Hands the UPDATE / DELETE counts of this thread to the statistics table
    /// (`m5/03` §5.9). The shared table is VC-6's; until it exists the counts are
    /// only drained so the thread-local map stays small.
    #[allow(clippy::unused_self)] // VC-6 が共有の件数表を持たせる。
    pub(super) fn flush_modstat(&self) {
        let _ = crate::storage::heap::modstat::drain();
    }

    pub(super) fn send_output(
        &self,
        out: &Output,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
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

    pub(super) fn send_notice(&self, n: &Notice, sink: &mut dyn ResultSink) -> std::io::Result<()> {
        let level = n.severity.as_str().to_ascii_lowercase();
        if self.settings.client_wants(&level) {
            sink.notice(n)?;
        }
        Ok(())
    }

    /// Sends `ParameterStatus` for every reported parameter whose value
    /// changed since it was last sent.
    pub(super) fn flush_parameter_status(
        &mut self,
        sink: &mut dyn ResultSink,
    ) -> std::io::Result<()> {
        for (name, value) in self.settings.reported_values() {
            if self.reported.get(name) != Some(&value) {
                sink.parameter_status(name, &value)?;
                self.reported.insert(name, value);
            }
        }
        Ok(())
    }

    // ----- transaction control ---------------------------------------------

    /// Executes one statement and returns its outcome (the command tag, or the
    /// start of a COPY).
    pub(super) fn exec_statement(
        &mut self,
        stmt: &Statement,
        out: &mut Output,
    ) -> Result<ExecOutcome> {
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

    /// SELECT / VALUES / INSERT / UPDATE / DELETE / DDL / COPY / EXPLAIN
    /// (`m2.md` §5.2, `m4/02` §4.1).
    pub(super) fn exec_data_statement(
        &mut self,
        stmt: &Statement,
        out: &mut Output,
    ) -> Result<ExecOutcome> {
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
        // Relation locks come before the XID and the snapshot (m5/01 §5.9, LK-D24).
        self.lock_for_statement(stmt)?;
        Self::check_not_poisoned(&cluster)?;
        let mgr = Arc::clone(cluster.txn_manager());
        if write_tag.is_some() && read_only_write.is_none() && self.txn.guard.is_none() {
            let (xid, guard) = mgr_begin_write(&mgr, self.id)?;
            self.txn.xid = Some(xid);
            self.txn.guard = Some(guard);
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
        self.flush_modstat();
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

    /// Step 6 of `m2.md` §5.2: analyze, then execute by the kind of statement
    /// (`m4/02` §4.1).
    pub(super) fn run_bound(
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
                    backend: env.backend,
                    wait: env.wait,
                    session: env.info,
                    role: env.role,
                    password: env.password,
                    outside_block: !env.in_transaction_block,
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
    pub(super) fn exec_explain(
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

    pub(super) fn exec_set(&mut self, s: &SetStmt, out: &mut Output) -> Result<String> {
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

    pub(super) fn exec_show(&mut self, s: &ShowStmt, out: &mut Output) -> Result<String> {
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
}

/// What a failed transaction still accepts: the statements that end it,
/// and `ROLLBACK TO` (`m3.md` §6.11.3).
pub(super) fn allowed_when_failed(stmt: &Statement) -> bool {
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

pub(super) fn read_only_error(tag: &str) -> Error {
    Error::new(
        sqlstate::READ_ONLY_SQL_TRANSACTION,
        format!("cannot execute {tag} in a read-only transaction"),
    )
}

pub(super) fn cannot_reset(name: &str) -> Error {
    Error::not_supported(format!(
        "parameter \"{}\" cannot be reset",
        name.to_ascii_lowercase()
    ))
}

pub(super) fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The command tag used in `25006` for a statement that writes, judged on
/// the raw parse tree (`m3.md` §5.2 d, `m4/07` §5.0). `VACUUM` / `ANALYZE`
/// are not writing statements; `EXPLAIN ANALYZE` of a DML statement is.
pub(super) fn write_statement_tag(stmt: &Statement) -> Option<&'static str> {
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
pub(super) fn explain_analyzes(e: &crate::sql::ast::Explain) -> bool {
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

/// Renders the `QUERY PLAN` lines (`Planning Time` / `Execution Time` included).
pub(super) fn render_explain(
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
pub(super) fn row_text(
    row: &[Datum],
    types: &[SqlType],
    env: &TypeEnv<'_>,
) -> Result<Vec<Option<String>>> {
    row.iter()
        .take(types.len())
        .zip(types)
        .map(|(d, ty)| io::try_output_text_env(d, *ty, env))
        .collect()
}

pub(super) fn no_transaction_warning() -> Notice {
    Notice::new(
        Severity::Warning,
        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
        "there is no transaction in progress",
    )
}
#[allow(clippy::print_stderr)]
pub(super) fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

pub(super) fn text_column(name: impl Into<String>) -> ColumnDesc {
    ColumnDesc {
        name: name.into(),
        table_oid: 0,
        column_attnum: 0,
        type_oid: oid::TEXT,
        type_len: -1,
        type_modifier: -1,
        format: Format::Text,
    }
}

/// `RowDescription` fields for a SELECT's visible columns.
pub(super) fn column_descs(
    columns: &[OutputColumn],
    catalog: &dyn CatalogReader,
) -> Vec<ColumnDesc> {
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
                format: Format::Text,
            }
        })
        .collect()
}

/// What `Session::exec_ctx_parts` hands out: the session-side inputs of a statement's
/// execution context (`m5/04` §6.2.5). The pieces that borrow the catalog (`TypeEnv`,
/// `EvalCtx`) are built by the caller from `settings`.
#[derive(Debug)]
#[allow(dead_code)] // XQ-2 が使う。
pub(super) struct ExecParts<'a> {
    pub(super) info: SessionInfo,
    pub(super) interrupts: Arc<InterruptFlag>,
    pub(super) settings: &'a Settings,
    pub(super) backend: BackendId,
    pub(super) role: &'a RoleRow,
    pub(super) pid: i32,
    pub(super) stmt_started_at: i64,
    pub(super) xid: Option<u64>,
    pub(super) lock_timeout: Option<Duration>,
    pub(super) deadlock_timeout: Duration,
    pub(super) password: PasswordPolicy,
}

impl ExecParts<'_> {
    #[allow(dead_code)] // XQ-2 が使う。
    /// The abort conditions of a wait (`lock_timeout`, `deadlock_timeout`, cancel).
    pub(super) fn wait_ctl(&self) -> lock::WaitCtl<'_> {
        lock::WaitCtl {
            lock_timeout: self.lock_timeout,
            deadlock_timeout: self.deadlock_timeout,
            interrupts: &self.interrupts,
        }
    }
}
