//! プロセス内の `StressTarget`。`TestCluster`（`SimVfs` 上のクラスタ）に `Session` を直接つなぐ。

use std::sync::{Arc, mpsc};
use std::thread;

use super::target::{
    ControlSnapshot, QueryResult, StressConn, StressError, StressInternals, StressTarget,
};
use crate::error::Error;
use crate::interrupt::InterruptFlag;
use crate::session::{ColumnDesc, ExtOutcome, Format, Notice, ResultSink, Session, StartupParams};
use crate::testing::{TestCluster, TestClusterOptions};
use crate::txn::Xid;
use crate::txn::lock::LockStatusRow;

/// プロセス内のクラスタ。
#[derive(Debug)]
pub struct InprocTarget {
    tc: TestCluster,
}

impl InprocTarget {
    /// 既定の設定で initdb して起動する。
    pub fn new() -> Self {
        // 同時接続の余裕を持たせるため、バッファは多めにする。
        let opts = TestClusterOptions {
            nframes: 1024,
            ..TestClusterOptions::default()
        };
        InprocTarget::with_options(opts).expect("the in-process test cluster must start")
    }

    /// # Errors
    ///
    /// initdb か起動に失敗した。
    pub fn with_options(opts: TestClusterOptions) -> crate::error::Result<Self> {
        Ok(InprocTarget {
            tc: TestCluster::with_options(opts)?,
        })
    }

    pub fn cluster(&self) -> &TestCluster {
        &self.tc
    }
}

impl Default for InprocTarget {
    fn default() -> Self {
        InprocTarget::new()
    }
}

fn to_stress_error(e: &Error) -> StressError {
    StressError {
        sqlstate: e.sqlstate.code().to_owned(),
        message: e.message.clone(),
        detail: e.detail.clone(),
    }
}

impl StressTarget for InprocTarget {
    fn connect(&self, database: &str, user: &str) -> Result<Box<dyn StressConn>, StressError> {
        Ok(Box::new(InprocConn::open(&self.tc, database, user)?))
    }

    fn internals(&self) -> Option<&dyn StressInternals> {
        Some(self)
    }

    fn label(&self) -> &'static str {
        "inproc"
    }
}

impl StressInternals for InprocTarget {
    fn lock_status(&self) -> Vec<LockStatusRow> {
        self.tc.cluster.locks().lock_status()
    }

    fn oldest_xmin(&self) -> Xid {
        self.tc.cluster.txn_manager().oldest_xmin()
    }

    fn control(&self) -> ControlSnapshot {
        let d = self.tc.cluster.control().get();
        ControlSnapshot {
            oldest_xid: d.oldest_xid,
            next_xid: d.next_xid,
            next_multi: d.next_multi,
        }
    }
}

/// 文ごとの結果とエラーを集める。
#[derive(Default)]
struct Collector {
    rows: Vec<Vec<Option<String>>>,
    results: Vec<QueryResult>,
    error: Option<Error>,
}

/// `INSERT 0 5` / `UPDATE 3` / `SELECT 2` の最後の数。
fn affected_of(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

impl Collector {
    fn finish(&mut self, tag: &str) {
        let rows = std::mem::take(&mut self.rows);
        let affected = if rows.is_empty() {
            affected_of(tag)
        } else {
            rows.len() as u64
        };
        self.results.push(QueryResult { rows, affected });
    }
}

impl ResultSink for Collector {
    fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
        self.rows.clear();
        Ok(())
    }
    fn data_row(&mut self, values: &[Option<String>]) -> std::io::Result<()> {
        self.rows.push(values.to_vec());
        Ok(())
    }
    fn command_complete(&mut self, tag: &str) -> std::io::Result<()> {
        self.finish(tag);
        Ok(())
    }
    fn empty_query(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn error(&mut self, err: &Error) -> std::io::Result<()> {
        if self.error.is_none() {
            self.error = Some(err.clone());
        }
        Ok(())
    }
    fn notice(&mut self, _: &Notice) -> std::io::Result<()> {
        Ok(())
    }
    fn parameter_status(&mut self, _: &str, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}

/// セッションを持つ側。`Session` は Send でない（中断中のポータルが実行器を持つ）ので、専用スレッドの中だけで使う。
struct LocalConn {
    session: Session,
}

impl LocalConn {
    fn io(e: &std::io::Error) -> StressError {
        StressError::new("08006", format!("connection failure: {e}"))
    }

    /// Bind + Execute + Sync。`stmt` は文の名前。
    fn run_bound(
        &mut self,
        stmt: &str,
        params: &[Option<&str>],
    ) -> Result<QueryResult, StressError> {
        let raw: Vec<Option<&[u8]>> = params.iter().map(|p| p.map(str::as_bytes)).collect();
        let formats = vec![Format::Text; params.len()];
        let mut sink = Collector::default();
        let outcome = self
            .session
            .msg_bind("", stmt, &formats, &raw, &[])
            .and_then(|()| self.session.msg_execute("", 0, &mut sink));
        match outcome {
            Ok(out) => {
                if let ExtOutcome::Complete(tag) = &out {
                    // Execute が command_complete をシンクに流す実装でも、流さない実装でも 1 件にする。
                    if sink.results.is_empty() {
                        sink.finish(tag);
                    }
                }
                self.session.msg_sync().map_err(|e| to_stress_error(&e))?;
                Ok(sink.results.pop().unwrap_or_default())
            }
            Err(e) => {
                self.session.msg_abort_implicit();
                // 失敗後の Sync は、明示トランザクションの中断状態を変えない。
                let _ = self.session.msg_sync();
                Err(to_stress_error(&e))
            }
        }
    }
}

impl LocalConn {
    fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError> {
        let mut sink = Collector::default();
        self.session
            .execute_simple(sql, &mut sink)
            .map_err(|e| Self::io(&e))?;
        match sink.error {
            Some(e) => Err(to_stress_error(&e)),
            None => Ok(sink.results),
        }
    }

    fn extended(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError> {
        if let Err(e) = self.session.msg_parse("", sql, &[]) {
            self.session.msg_abort_implicit();
            let _ = self.session.msg_sync();
            return Err(to_stress_error(&e));
        }
        self.run_bound("", params)
    }

    fn prepared(&mut self, name: &str, sql: &str) -> Result<(), StressError> {
        let r = self.session.msg_parse(name, sql, &[]);
        match r {
            Ok(()) => self.session.msg_sync().map_err(|e| to_stress_error(&e)),
            Err(e) => {
                self.session.msg_abort_implicit();
                let _ = self.session.msg_sync();
                Err(to_stress_error(&e))
            }
        }
    }

    fn execute_prepared(
        &mut self,
        name: &str,
        params: &[Option<&str>],
    ) -> Result<QueryResult, StressError> {
        self.run_bound(name, params)
    }
}

impl Drop for LocalConn {
    fn drop(&mut self) {
        self.session.terminate();
    }
}

type Job = Box<dyn FnOnce(&mut LocalConn) + Send>;

/// 接続ごとに専用のスレッドを持ち、その中で `Session` を使う（本番と同じ「接続ごと 1 スレッド」）。
/// `Session` は Send でないので、スレッドの中で作る。
struct InprocConn {
    jobs: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
    interrupt: Arc<InterruptFlag>,
    pid: i32,
}

impl InprocConn {
    fn open(tc: &TestCluster, database: &str, user: &str) -> Result<InprocConn, StressError> {
        let cluster = Arc::clone(&tc.cluster);
        let params = StartupParams {
            user: user.into(),
            database: database.into(),
            application_name: None,
            options: Vec::new(),
            client_addr: None,
        };
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let session = match Session::new(cluster, params) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(to_stress_error(&e)));
                    return;
                }
            };
            let _ = ready_tx.send(Ok((session.interrupt_flag(), session.backend_pid())));
            let mut conn = LocalConn { session };
            while let Ok(job) = rx.recv() {
                job(&mut conn);
            }
        });
        let (interrupt, pid) = ready_rx
            .recv()
            .map_err(|_| StressError::new("08006", "session thread exited"))??;
        Ok(InprocConn {
            jobs: Some(jobs),
            worker: Some(worker),
            interrupt,
            pid,
        })
    }

    /// 接続のスレッドで `f` を実行して結果を待つ。
    fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut LocalConn) -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        let job: Job = Box::new(move |c| {
            let _ = tx.send(f(c));
        });
        self.jobs
            .as_ref()
            .expect("jobs are present until drop")
            .send(job)
            .expect("the session thread must be alive");
        rx.recv().expect("the session thread must reply")
    }
}

fn own_params(params: &[Option<&str>]) -> Vec<Option<String>> {
    params.iter().map(|p| p.map(str::to_owned)).collect()
}

fn borrow_params(params: &[Option<String>]) -> Vec<Option<&str>> {
    params.iter().map(Option::as_deref).collect()
}

impl StressConn for InprocConn {
    fn simple(&mut self, sql: &str) -> Result<Vec<QueryResult>, StressError> {
        let sql = sql.to_owned();
        self.call(move |c| c.simple(&sql))
    }

    fn extended(&mut self, sql: &str, params: &[Option<&str>]) -> Result<QueryResult, StressError> {
        let (sql, params) = (sql.to_owned(), own_params(params));
        self.call(move |c| c.extended(&sql, &borrow_params(&params)))
    }

    fn prepared(&mut self, name: &str, sql: &str) -> Result<(), StressError> {
        let (name, sql) = (name.to_owned(), sql.to_owned());
        self.call(move |c| c.prepared(&name, &sql))
    }

    fn execute_prepared(
        &mut self,
        name: &str,
        params: &[Option<&str>],
    ) -> Result<QueryResult, StressError> {
        let (name, params) = (name.to_owned(), own_params(params));
        self.call(move |c| c.execute_prepared(&name, &borrow_params(&params)))
    }

    fn backend_pid(&self) -> i32 {
        self.pid
    }

    fn cancel(&self) -> Result<(), StressError> {
        self.interrupt.request_cancel();
        Ok(())
    }
}

impl Drop for InprocConn {
    fn drop(&mut self) {
        // チャネルを閉じるとスレッドのループが終わり、`LocalConn` の Drop が `terminate` する。
        self.jobs = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
