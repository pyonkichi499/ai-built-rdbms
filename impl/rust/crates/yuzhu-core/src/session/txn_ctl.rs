//! トランザクション制御（BEGIN / COMMIT / ROLLBACK / SAVEPOINT / 分離レベル。RW が持つ）。

#![allow(clippy::wildcard_imports)]

use super::*;

impl Session {
    pub(super) fn begin_transaction(&mut self, state: TxState) {
        self.txn_serial += 1;
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
    pub(super) fn commit_transaction(&mut self) -> Result<()> {
        self.ext.end_of_transaction();
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
            let result = if txn.wal_flush_upto == crate::wal::Lsn::default() {
                Ok(())
            } else {
                Self::check_not_poisoned(cluster)
                    .and_then(|()| cluster.txn_manager().finish_without_xid(txn.wal_flush_upto))
            };
            // Read-only transactions hold relation locks too (m5/01 §5.8).
            cluster
                .locks()
                .release_all(BackendId(self.id), LockScope::Transaction);
            return result;
        };
        if let Err(e) = Self::check_not_poisoned(cluster) {
            txn.guard = None;
            cluster
                .locks()
                .release_all(BackendId(self.id), LockScope::Transaction);
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
        if !txn.pending_unlinks.is_empty() {
            Self::unlink_files(cluster, &txn.pending_unlinks);
        }
        // 4. The transaction-scope locks go last (D38): the XID lock wakes the
        // sessions that waited for this transaction.
        txn.guard = None;
        cluster
            .locks()
            .release_all(BackendId(self.id), LockScope::Transaction);
        Ok(())
    }

    /// Aborts (`m2.md` §5.3). Never fails to reset the session; the error of
    /// `TxnManager::abort` (a `Panic`) is dropped here and poisons the
    /// cluster.
    pub(super) fn rollback_transaction(&mut self) {
        self.ext.end_of_transaction();
        let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
        self.settings.rollback();
        self.state = TxState::Idle;
        let Some(cluster) = self.cluster.as_ref() else {
            return;
        };
        let Some(xid) = txn.xid else {
            cluster
                .locks()
                .release_all(BackendId(self.id), LockScope::Transaction);
            return;
        };
        if let Err(e) = mgr_abort(cluster.txn_manager(), xid, &txn.pending_creates) {
            warn(&format!("could not record the abort: {}", e.message));
            cluster.poison();
        }
        if !txn.pending_creates.is_empty() {
            Self::unlink_files(cluster, &txn.pending_creates);
        }
        txn.guard = None;
        cluster
            .locks()
            .release_all(BackendId(self.id), LockScope::Transaction);
    }

    /// Removes relation files under the exclusive storage barrier. The wait
    /// is never blocking: while another session runs a statement the files
    /// stay queued and the next statement end (of any session) retries.
    /// Errors are only warned about: the transaction is already decided.
    pub(super) fn unlink_files(cluster: &Cluster, rels: &[RelFileLocator]) {
        cluster.txn_manager().defer_unlinks(rels);
        Self::flush_deferred_unlinks(cluster);
    }

    pub(super) fn flush_deferred_unlinks(cluster: &Cluster) {
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

    pub(super) fn exec_transaction(
        &mut self,
        t: &TransactionStmt,
        out: &mut Output,
    ) -> Result<String> {
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
    pub(super) fn end_transaction(
        &mut self,
        commit: bool,
        chain: bool,
        out: &mut Output,
    ) -> Result<String> {
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
    pub(super) fn in_block_for_set(&self) -> bool {
        self.state == TxState::Block || (self.state == TxState::Implicit && self.multi_statement)
    }

    pub(super) fn apply_modes(&mut self, modes: &[TransactionMode]) -> Result<()> {
        modes.iter().try_for_each(|m| self.apply_mode(m))
    }

    /// Changes one characteristic of the current transaction. The checks
    /// are PostgreSQL's `check_XactIsoLevel` and friends: they only apply
    /// when the value changes, and only after a statement has used a
    /// snapshot (`txn_snapshot_taken`).
    pub(super) fn apply_mode(&mut self, mode: &TransactionMode) -> Result<()> {
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
    pub(super) fn set_session_characteristics(&mut self, t: &SetTransaction) -> Result<String> {
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
    pub(super) fn set_characteristic(&mut self, name: &str, args: &[String]) -> Result<()> {
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
    pub(super) fn characteristic_value(&self, name: &str) -> Option<String> {
        match name.to_ascii_lowercase().as_str() {
            "transaction_isolation" => Some(self.chars.isolation.as_str().to_owned()),
            "transaction_read_only" => Some(on_off(self.chars.read_only).to_owned()),
            "transaction_deferrable" => Some(on_off(self.chars.deferrable).to_owned()),
            _ => None,
        }
    }
}

pub(super) fn not_in_block(what: &str) -> Error {
    Error::new(
        sqlstate::NO_ACTIVE_SQL_TRANSACTION,
        format!("{what} can only be used in transaction blocks"),
    )
}

pub(super) fn no_such_savepoint(name: &str) -> Error {
    Error::new(
        sqlstate::INVALID_SAVEPOINT_SPECIFICATION,
        format!("savepoint \"{name}\" does not exist"),
    )
}

/// `transaction_isolation` and friends: the parameters of the current
/// transaction, owned by the session.
pub(super) fn is_characteristic(name: &str) -> bool {
    [
        "transaction_isolation",
        "transaction_read_only",
        "transaction_deferrable",
    ]
    .iter()
    .any(|n| n.eq_ignore_ascii_case(name))
}

pub(super) fn mgr_commit(mgr: &TxnManager, xid: Xid, dropped: &[RelFileLocator]) -> Result<()> {
    mgr.commit(xid, dropped)
}

pub(super) fn mgr_abort(mgr: &TxnManager, xid: Xid, created: &[RelFileLocator]) -> Result<()> {
    mgr.abort(xid, created)
}

/// RW-D11: 文は「最初のスナップショットを取った」ことになるか。トランザクション制御、SET / RESET / SHOW、
/// LOCK TABLE、CHECKPOINT だけが false（`SELECT 1` や DDL、EXPLAIN、PREPARE も true）。
pub(super) fn statement_needs_snapshot(stmt: &Statement) -> bool {
    !matches!(
        stmt,
        Statement::Transaction(_)
            | Statement::Set(_)
            | Statement::Reset(_)
            | Statement::Show(_)
            | Statement::LockTable(_)
            | Statement::Checkpoint(_)
    )
}

impl Session {
    /// Extended Query の Parse が呼ぶ。スナップショットを要する文なら「最初のスナップショットを取った」印だけを立てる
    /// （RW-D11、RW-K7: 実際のスナップショットは Bind / Execute が取る）。
    pub(super) fn note_snapshot_use(&mut self, stmt: &Statement) {
        if statement_needs_snapshot(stmt) {
            self.txn_snapshot_taken = true;
        }
    }

    /// 文の実行の手順 0'（RW-D10、`m5/02` §5.8）: スナップショットを要する文なら印を立て、Repeatable Read の
    /// トランザクションでは最初のスナップショットを**リレーションロックの前に**取る。
    /// 分離レベルは `SET TRANSACTION` で最初のスナップショットまで変えられるので、ここで確定する。
    pub(super) fn begin_statement_snapshot(&mut self, stmt: &Statement) {
        if !statement_needs_snapshot(stmt) {
            return;
        }
        self.txn_snapshot_taken = true;
        self.txn.isolation = self.chars.isolation.level();
        if self.txn.isolation.uses_xact_snapshot()
            && self.txn.xact_snapshot.is_none()
            && let Some(cluster) = self.cluster.as_ref()
        {
            self.txn.xact_snapshot = Some(
                cluster
                    .txn_manager()
                    .take_snapshot(self.txn.xid, self.txn.cid),
            );
        }
    }
}
