//! ストレスの対象への接続（10 章 §4.1）。プロセス内（`inproc.rs`）、wire（yuzhu-server、
//! 本物の PostgreSQL 17）の 3 つで実装する。wire 版は yuzhu-server 側の担当。

use std::fmt;

use crate::txn::Xid;
use crate::txn::lock::LockStatusRow;

#[derive(Clone, Debug)]
pub struct StressError {
    pub sqlstate: String,
    pub message: String,
    pub detail: Option<String>,
}

impl StressError {
    pub fn new(sqlstate: impl Into<String>, message: impl Into<String>) -> Self {
        StressError {
            sqlstate: sqlstate.into(),
            message: message.into(),
            detail: None,
        }
    }

    /// 40001 か 40P01。
    pub fn is_retryable(&self) -> bool {
        self.sqlstate == "40001" || self.sqlstate == "40P01"
    }

    /// クラス XX（内部エラー）。許容リストに入れても失敗にする。
    pub fn is_internal(&self) -> bool {
        self.sqlstate.starts_with("XX")
    }
}

impl fmt::Display for StressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.sqlstate, self.message)?;
        if let Some(d) = &self.detail {
            write!(f, " (DETAIL: {d})")?;
        }
        Ok(())
    }
}

impl std::error::Error for StressError {}

#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub rows: Vec<Vec<Option<String>>>,
    pub affected: u64,
}

impl QueryResult {
    /// 1 行 1 列の値（NULL と行なしは None）。
    pub fn scalar(&self) -> Option<&str> {
        self.rows.first()?.first()?.as_deref()
    }

    pub fn scalar_i64(&self) -> Option<i64> {
        self.scalar()?.parse().ok()
    }
}

/// 観測用の制御情報。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlSnapshot {
    pub oldest_xid: u64,
    pub next_xid: u64,
    pub next_multi: u64,
}

pub trait StressInternals: Send + Sync {
    fn lock_status(&self) -> Vec<LockStatusRow>;
    fn oldest_xmin(&self) -> Xid;
    fn control(&self) -> ControlSnapshot;
}

pub trait StressConn: Send {
    /// Simple Query（複数文可）。文ごとの結果を返す。
    fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError>;
    /// Extended Query（無名の文。パラメータはテキスト形式）。
    fn extended(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError>;
    fn prepared(&mut self, name: &str, sql: &str) -> Result<(), StressError>;
    fn execute_prepared(
        &mut self,
        name: &str,
        params: &[Option<&str>],
    ) -> Result<QueryResult, StressError>;
    fn backend_pid(&self) -> i32;
    /// CancelRequest 相当。
    fn cancel(&self) -> Result<(), StressError>;
}

pub trait StressTarget: Send + Sync {
    fn connect(&self, database: &str, user: &str) -> Result<Box<dyn StressConn>, StressError>;
    /// プロセス内だけが Some。
    fn internals(&self) -> Option<&dyn StressInternals>;
    /// `"inproc"` | `"yuzhu-wire"` | `"pg-wire"`
    fn label(&self) -> &'static str;
}

impl dyn StressConn + '_ {
    /// 1 文を実行して最後の結果を返す補助。
    ///
    /// # Errors
    ///
    /// 文が失敗した。
    pub fn query(&mut self, sql: &str) -> Result<QueryResult, StressError> {
        Ok(self.simple(sql)?.pop().unwrap_or_default())
    }
}
