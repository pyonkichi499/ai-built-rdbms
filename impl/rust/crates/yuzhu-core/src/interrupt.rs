//! Shutdown, cancel and statement-deadline flags shared by `ExecCtx`,
//! `Session` and the server's connection registry (`m2.md` §4.1,
//! `m3.md` §4.1).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::error::{Error, Result, Severity, sqlstate};

/// A request to terminate or cancel, plus the current statement's deadline.
/// Shared through `Arc`.
#[derive(Debug, Default)]
pub struct InterruptFlag {
    terminate: AtomicBool,
    cancel: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl InterruptFlag {
    pub fn request_terminate(&self) {
        self.terminate.store(true, Ordering::SeqCst);
    }

    pub fn is_terminate_requested(&self) -> bool {
        self.terminate.load(Ordering::SeqCst)
    }

    /// `CancelRequest` を受けたとき（yuzhu-server が別スレッドから呼ぶ）。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Query メッセージの処理を始めるときに session が呼ぶ（アイドル中に届いた
    /// キャンセルは捨てる）。
    pub fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// 文の開始時に `Some(now + statement_timeout)`、終了時に `None`。
    pub fn set_statement_deadline(&self, deadline: Option<Instant>) {
        if let Ok(mut g) = self.deadline.lock() {
            *g = deadline;
        }
    }

    /// 優先順: 停止（FATAL 57P01）→ キャンセル（57014 user request）→
    /// 期限（57014 statement timeout）。キャンセルと期限のエラーを返したら、その
    /// フラグを落とす（同じ文で二度報告しない）。
    pub fn check(&self) -> Result<()> {
        if self.is_terminate_requested() {
            return Err(Error::new(
                sqlstate::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            )
            .with_severity(Severity::Fatal));
        }
        if self.cancel.swap(false, Ordering::SeqCst) {
            return Err(Error::new(
                sqlstate::QUERY_CANCELED,
                "canceling statement due to user request",
            ));
        }
        if let Ok(mut g) = self.deadline.lock()
            && g.is_some_and(|d| Instant::now() >= d)
        {
            *g = None;
            return Err(Error::new(
                sqlstate::QUERY_CANCELED,
                "canceling statement due to statement timeout",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn flag_is_shared_and_sticky() {
        let f = Arc::new(InterruptFlag::default());
        assert!(!f.is_terminate_requested());
        let g = Arc::clone(&f);
        std::thread::spawn(move || g.request_terminate())
            .join()
            .unwrap();
        assert!(f.is_terminate_requested());
        // 停止は落ちない
        assert_eq!(f.check().unwrap_err().severity, Severity::Fatal);
        assert_eq!(f.check().unwrap_err().sqlstate, sqlstate::ADMIN_SHUTDOWN);
    }

    #[test]
    fn cancel_is_reported_once_and_cleared_at_query_start() {
        let f = InterruptFlag::default();
        assert!(f.check().is_ok());
        f.request_cancel();
        let e = f.check().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(e.message.contains("user request"));
        assert!(f.check().is_ok());

        f.request_cancel();
        f.clear_cancel();
        assert!(f.check().is_ok());
    }

    #[test]
    fn deadline_expires_and_is_reported_once() {
        let f = InterruptFlag::default();
        f.set_statement_deadline(Some(Instant::now() + Duration::from_secs(3600)));
        assert!(f.check().is_ok());
        f.set_statement_deadline(Some(Instant::now()));
        let e = f.check().unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(e.message.contains("statement timeout"));
        assert!(f.check().is_ok());
        f.set_statement_deadline(Some(Instant::now()));
        f.set_statement_deadline(None);
        assert!(f.check().is_ok());
    }

    #[test]
    fn priority_is_terminate_then_cancel_then_deadline() {
        let f = InterruptFlag::default();
        f.set_statement_deadline(Some(Instant::now()));
        f.request_cancel();
        assert!(f.check().unwrap_err().message.contains("user request"));
        assert!(f.check().unwrap_err().message.contains("statement timeout"));
        f.request_cancel();
        f.request_terminate();
        assert_eq!(f.check().unwrap_err().sqlstate, sqlstate::ADMIN_SHUTDOWN);
    }
}
