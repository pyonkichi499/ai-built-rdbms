//! Transaction control, SET / SHOW / RESET, EXPLAIN.

use super::Parser;
use super::expr::is_query_start_kw;
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{
    Explain, ParamTarget, ResetStmt, SetArg, SetStmt, SetTransaction, SetValue, ShowStmt,
    Statement, TransactionKind, TransactionMode, TransactionStmt,
};
use crate::sql::token::{KeywordCategory, TokenKind, keyword_category};

impl Parser<'_> {
    /// Eats the optional `SAVEPOINT` of `RELEASE` / `ROLLBACK TO`. As in
    /// PostgreSQL it is also a valid name, so a `SAVEPOINT` that is not
    /// followed by a word is the name itself.
    fn eat_savepoint_kw(&mut self) {
        if self.is_kw("savepoint") && matches!(self.peek_nth(1).kind, TokenKind::Word { .. }) {
            self.advance();
        }
    }

    pub(super) fn parse_transaction(&mut self) -> Result<TransactionStmt> {
        let tok = self.advance();
        let start = tok.span.start;
        let kw = tok.keyword().unwrap_or_default().to_string();
        let mut kind = match kw.as_str() {
            "begin" => TransactionKind::Begin,
            "start" => {
                self.expect_kw("transaction")?;
                TransactionKind::StartTransaction
            }
            "commit" => TransactionKind::Commit,
            "end" => TransactionKind::End,
            "rollback" => TransactionKind::Rollback,
            "savepoint" => TransactionKind::Savepoint(self.parse_col_id()?.value),
            "release" => {
                self.eat_savepoint_kw();
                TransactionKind::Release(self.parse_col_id()?.value)
            }
            _ => TransactionKind::Abort,
        };
        if matches!(
            kind,
            TransactionKind::Savepoint(_) | TransactionKind::Release(_)
        ) {
            return Ok(TransactionStmt {
                kind,
                modes: Vec::new(),
                chain: false,
                span: self.span_from(start),
            });
        }
        if matches!(kw.as_str(), "commit" | "rollback") && self.is_kw("prepared") {
            return Err(self.not_supported(&format!("{} PREPARED", kw.to_ascii_uppercase())));
        }
        if kind != TransactionKind::StartTransaction && !self.eat_kw("work") {
            self.eat_kw("transaction");
        }
        let mut modes = Vec::new();
        let mut chain = false;
        match kind {
            TransactionKind::Begin | TransactionKind::StartTransaction => {
                modes = self.parse_transaction_modes()?;
            }
            _ => {
                if kind == TransactionKind::Rollback && self.eat_kw("to") {
                    self.eat_savepoint_kw();
                    kind = TransactionKind::RollbackTo(self.parse_col_id()?.value);
                } else if self.eat_kw("and") {
                    chain = !self.eat_kw("no");
                    self.expect_kw("chain")?;
                }
            }
        }
        Ok(TransactionStmt {
            kind,
            modes,
            chain,
            span: self.span_from(start),
        })
    }

    /// `transaction_mode_list_or_empty` (items separated by commas or just
    /// juxtaposed).
    fn parse_transaction_modes(&mut self) -> Result<Vec<TransactionMode>> {
        let mut modes = Vec::new();
        loop {
            let mode = if self.eat_kw("isolation") {
                self.expect_kw("level")?;
                let level = if self.eat_kw("serializable") {
                    "serializable"
                } else if self.eat_kw("repeatable") {
                    self.expect_kw("read")?;
                    "repeatable read"
                } else {
                    self.expect_kw("read")?;
                    if self.eat_kw("committed") {
                        "read committed"
                    } else {
                        self.expect_kw("uncommitted")?;
                        "read uncommitted"
                    }
                };
                TransactionMode::IsolationLevel(level.to_string())
            } else if self.is_kw("read") {
                self.advance();
                if self.eat_kw("only") {
                    TransactionMode::ReadOnly
                } else {
                    self.expect_kw("write")?;
                    TransactionMode::ReadWrite
                }
            } else if self.eat_kw("deferrable") {
                TransactionMode::Deferrable
            } else if self.is_kw("not") && self.nth_is_kw(1, "deferrable") {
                self.advance();
                self.advance();
                TransactionMode::NotDeferrable
            } else if modes.is_empty() {
                return Ok(modes);
            } else {
                // After a comma a mode is required.
                return Err(self.unexpected());
            };
            modes.push(mode);
            if !self.eat(&TokenKind::Comma) && !self.at_transaction_mode() {
                return Ok(modes);
            }
        }
    }

    fn at_transaction_mode(&self) -> bool {
        self.is_kw("isolation")
            || self.is_kw("read")
            || self.is_kw("deferrable")
            || (self.is_kw("not") && self.nth_is_kw(1, "deferrable"))
    }

    fn next_is_set_assign(&self) -> bool {
        self.nth_is_kw(1, "to") || self.peek_nth(1).is_op("=")
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn parse_set(&mut self) -> Result<SetStmt> {
        let start = self.advance().span.start;
        let mut local = false;
        if self.is_kw("local") && !self.next_is_set_assign() {
            self.advance();
            local = true;
        } else if self.is_kw("session")
            && !self.next_is_set_assign()
            && !self.nth_is_kw(1, "authorization")
            && !self.nth_is_kw(1, "characteristics")
        {
            self.advance();
        }
        let set = |p: &Self, name: &str, value: SetValue| SetStmt {
            local,
            name: name.to_string(),
            value,
            transaction: None,
            span: p.span_from(start),
        };
        if self.is_kw("transaction") && !self.next_is_set_assign() {
            if self.nth_is_kw(1, "snapshot") {
                self.advance();
                return Err(self.not_supported("SET TRANSACTION SNAPSHOT"));
            }
            self.advance();
            let (name, value, modes) = self.modes_to_param("")?;
            let mut stmt = set(self, &name, value);
            stmt.transaction = Some(SetTransaction {
                session_characteristics: false,
                modes,
            });
            return Ok(stmt);
        }
        if self.is_kw("session") && self.nth_is_kw(1, "characteristics") {
            self.advance();
            self.advance();
            self.expect_kw("as")?;
            self.expect_kw("transaction")?;
            let (name, value, modes) = self.modes_to_param("default_")?;
            let mut stmt = set(self, &name, value);
            stmt.transaction = Some(SetTransaction {
                session_characteristics: true,
                modes,
            });
            return Ok(stmt);
        }
        if self.is_kw("session") && self.nth_is_kw(1, "authorization") {
            self.advance();
            self.advance();
            let value = if self.eat_kw("default") {
                SetValue::Default
            } else {
                SetValue::Values(vec![self.parse_word_or_string()?])
            };
            return Ok(set(self, "session_authorization", value));
        }
        if self.is_kw("time") && self.nth_is_kw(1, "zone") {
            self.advance();
            self.advance();
            let value = self.parse_zone_value()?;
            return Ok(set(self, "timezone", value));
        }
        if self.is_kw("names") && !self.next_is_set_assign() {
            self.advance();
            let value = if self.eat_kw("default")
                || matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::Eof)
            {
                SetValue::Default
            } else {
                SetValue::Values(vec![self.parse_word_or_string()?])
            };
            return Ok(set(self, "client_encoding", value));
        }
        if self.is_kw("schema") && matches!(self.peek_nth(1).kind, TokenKind::String(_)) {
            self.advance();
            let TokenKind::String(s) = self.advance().kind else {
                return Err(Error::internal("SET SCHEMA: expected a string"));
            };
            return Ok(set(
                self,
                "search_path",
                SetValue::Values(vec![SetArg::String(s)]),
            ));
        }
        if self.is_kw("role") && !self.next_is_set_assign() {
            self.advance();
            let value = SetValue::Values(vec![self.parse_word_or_string()?]);
            return Ok(set(self, "role", value));
        }
        if (self.is_kw("catalog") && matches!(self.peek_nth(1).kind, TokenKind::String(_)))
            || (self.is_kw("xml") && self.nth_is_kw(1, "option"))
        {
            let what = format!(
                "SET {}",
                self.peek()
                    .keyword()
                    .unwrap_or_default()
                    .to_ascii_uppercase()
            );
            return Err(self.not_supported(&what));
        }
        let name = self.parse_var_name()?;
        if self.is_kw("from") && self.nth_is_kw(1, "current") {
            return Err(self.not_supported("SET ... FROM CURRENT"));
        }
        if !self.eat_kw("to") && !self.eat_op("=") {
            return Err(self.unexpected());
        }
        let value = if self.eat_kw("default") {
            SetValue::Default
        } else {
            let mut values = vec![self.parse_var_value()?];
            while self.eat(&TokenKind::Comma) {
                values.push(self.parse_var_value()?);
            }
            SetValue::Values(values)
        };
        Ok(set(self, &name, value))
    }

    /// Maps `SET [SESSION CHARACTERISTICS AS] TRANSACTION mode` to the
    /// equivalent parameter assignment (the first mode; all modes are returned too).
    fn modes_to_param(&mut self, prefix: &str) -> Result<(String, SetValue, Vec<TransactionMode>)> {
        let modes = self.parse_transaction_modes()?;
        let Some(mode) = modes.first().cloned() else {
            return Err(self.unexpected());
        };
        let (name, arg) = match mode {
            TransactionMode::IsolationLevel(l) => ("transaction_isolation", SetArg::String(l)),
            TransactionMode::ReadOnly => ("transaction_read_only", SetArg::Word("on".into())),
            TransactionMode::ReadWrite => ("transaction_read_only", SetArg::Word("off".into())),
            TransactionMode::Deferrable => ("transaction_deferrable", SetArg::Word("on".into())),
            TransactionMode::NotDeferrable => {
                ("transaction_deferrable", SetArg::Word("off".into()))
            }
        };
        Ok((
            format!("{prefix}{name}"),
            SetValue::Values(vec![arg]),
            modes,
        ))
    }

    /// `NonReservedWord_or_Sconst`.
    fn parse_word_or_string(&mut self) -> Result<SetArg> {
        if let TokenKind::String(s) = self.peek_kind().clone() {
            self.advance();
            return Ok(SetArg::String(s));
        }
        Ok(SetArg::Word(self.parse_non_reserved_word()?.value))
    }

    /// `NumericOnly` if at a (signed) number.
    fn parse_numeric_only(&mut self) -> Option<SetArg> {
        let (sign, n) = match self.peek_kind() {
            TokenKind::Op(op) if op == "-" || op == "+" => (op.clone(), 1),
            _ => (String::new(), 0),
        };
        let text = match &self.peek_nth(n).kind {
            TokenKind::Integer(s) | TokenKind::Decimal(s) => s.clone(),
            _ => return None,
        };
        for _ in 0..=n {
            self.advance();
        }
        Some(SetArg::Number(if sign == "-" {
            format!("-{text}")
        } else {
            text
        }))
    }

    /// `var_value`: `TRUE | FALSE | ON | NonReservedWord | Sconst | NumericOnly`.
    fn parse_var_value(&mut self) -> Result<SetArg> {
        if let Some(n) = self.parse_numeric_only() {
            return Ok(n);
        }
        if let Some(kw) = self.peek().keyword()
            && matches!(kw, "true" | "false" | "on")
        {
            let w = kw.to_string();
            self.advance();
            return Ok(SetArg::Word(w));
        }
        self.parse_word_or_string()
    }

    /// `zone_value` for `SET TIME ZONE`.
    fn parse_zone_value(&mut self) -> Result<SetValue> {
        if self.eat_kw("default") || self.eat_kw("local") {
            return Ok(SetValue::Default);
        }
        if self.is_kw("interval") {
            return Err(self.not_supported("SET TIME ZONE INTERVAL"));
        }
        if let Some(n) = self.parse_numeric_only() {
            return Ok(SetValue::Values(vec![n]));
        }
        match self.peek_kind().clone() {
            TokenKind::String(s) => {
                self.advance();
                Ok(SetValue::Values(vec![SetArg::String(s)]))
            }
            TokenKind::Word { value, quoted }
                if quoted || keyword_category(&value) == KeywordCategory::Unreserved =>
            {
                self.advance();
                Ok(SetValue::Values(vec![SetArg::Word(value)]))
            }
            _ => Err(self.unexpected()),
        }
    }

    /// The parameter named after SHOW / RESET.
    fn parse_param_target(&mut self) -> Result<ParamTarget> {
        if self.eat_kw("all") {
            return Ok(ParamTarget::All);
        }
        if self.is_kw("time") && self.nth_is_kw(1, "zone") {
            self.advance();
            self.advance();
            return Ok(ParamTarget::Name("timezone".into()));
        }
        if self.is_kw("transaction") && self.nth_is_kw(1, "isolation") {
            self.advance();
            self.advance();
            self.expect_kw("level")?;
            return Ok(ParamTarget::Name("transaction_isolation".into()));
        }
        if self.is_kw("session") && self.nth_is_kw(1, "authorization") {
            self.advance();
            self.advance();
            return Ok(ParamTarget::Name("session_authorization".into()));
        }
        Ok(ParamTarget::Name(self.parse_var_name()?))
    }

    pub(super) fn parse_show(&mut self) -> Result<ShowStmt> {
        let start = self.advance().span.start;
        let target = self.parse_param_target()?;
        Ok(ShowStmt {
            target,
            span: self.span_from(start),
        })
    }

    pub(super) fn parse_reset(&mut self) -> Result<ResetStmt> {
        let start = self.advance().span.start;
        let target = self.parse_param_target()?;
        Ok(ResetStmt {
            target,
            span: self.span_from(start),
        })
    }

    pub(super) fn parse_explain(&mut self) -> Result<Explain> {
        let start = self.advance().span.start;
        let mut analyze = false;
        let mut verbose = false;
        if self.peek_kind() == &TokenKind::LParen {
            self.advance();
            loop {
                let name_tok = self.peek().clone();
                let name = match &name_tok.kind {
                    TokenKind::Word { value, .. } => value.clone(),
                    _ => return Err(self.unexpected()),
                };
                self.advance();
                let value = self.parse_explain_option_value()?;
                match name.as_str() {
                    "analyze" | "analyse" => analyze = value,
                    "verbose" => verbose = value,
                    "costs" | "buffers" | "timing" | "summary" | "format" | "settings" | "wal"
                    | "generic_plan" | "serialize" | "memory" => {}
                    _ => {
                        return Err(Error::new(
                            sqlstate::SYNTAX_ERROR,
                            format!("unrecognized EXPLAIN option \"{name}\""),
                        )
                        .with_span(name_tok.span));
                    }
                }
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
        } else {
            if self.eat_kw("analyze") || self.eat_kw("analyse") {
                analyze = true;
            }
            if self.eat_kw("verbose") {
                verbose = true;
            }
        }
        let statement = match self.peek().keyword() {
            Some("insert") => Statement::Insert(self.parse_insert()?),
            Some("update") => Statement::Update(self.parse_update()?),
            Some("delete") => Statement::Delete(self.parse_delete()?),
            kw if is_query_start_kw(kw) || self.peek_kind() == &TokenKind::LParen => {
                Statement::Query(Box::new(self.parse_query()?))
            }
            _ => return Err(self.unexpected()),
        };
        Ok(Explain {
            analyze,
            verbose,
            statement: Box::new(statement),
            span: self.span_from(start),
        })
    }

    /// The optional value of an EXPLAIN option, as a boolean (`true` when
    /// absent; non-boolean values such as `FORMAT JSON` also give `true`).
    fn parse_explain_option_value(&mut self) -> Result<bool> {
        let t = self.peek().clone();
        let value = match &t.kind {
            TokenKind::Comma | TokenKind::RParen => return Ok(true),
            TokenKind::Word { value, .. } => !matches!(value.as_str(), "false" | "off"),
            TokenKind::String(s) => {
                !matches!(s.to_ascii_lowercase().as_str(), "false" | "off" | "0")
            }
            TokenKind::Integer(s) | TokenKind::Decimal(s) => s != "0",
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(value)
    }
}
