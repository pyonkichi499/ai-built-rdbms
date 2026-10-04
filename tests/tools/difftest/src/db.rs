//! Connections (simple query protocol only) and outcome comparison.

use std::fmt::Write as _;

use std::sync::mpsc;
use std::time::Duration;

use postgres::{Client, Config, NoTls, SimpleQueryMessage};

/// What a server answered to one statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Rows {
        cols: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// A command without a result set (row count from `CommandComplete`).
    Done(u64),
    Error {
        code: String,
        message: String,
    },
    /// The connection broke (crash, protocol error, ...).
    Lost(String),
}

impl Outcome {
    pub(crate) fn is_error(&self) -> bool {
        matches!(self, Outcome::Error { .. } | Outcome::Lost(_))
    }

    pub(crate) fn summary(&self, max_rows: usize) -> String {
        match self {
            Outcome::Rows { cols, rows } => {
                let mut s = format!("{} row(s), columns [{}]", rows.len(), cols.join(", "));
                for r in rows.iter().take(max_rows) {
                    let cells: Vec<String> = r
                        .iter()
                        .map(|c| {
                            c.as_ref()
                                .map_or_else(|| "NULL".to_string(), |v| format!("{v:?}"))
                        })
                        .collect();
                    let _ = write!(s, "\n    ({})", cells.join(", "));
                }
                if rows.len() > max_rows {
                    let _ = write!(s, "\n    ... ({} more)", rows.len() - max_rows);
                }
                s
            }
            Outcome::Done(n) => format!("OK ({n})"),
            Outcome::Error { code, message } => format!("ERROR {code}: {message}"),
            Outcome::Lost(m) => format!("CONNECTION LOST: {m}"),
        }
    }
}

type Reply = Result<Vec<SimpleQueryMessage>, postgres::Error>;

/// A connection driven by its own thread, so that a server that never
/// answers can be abandoned after a timeout instead of hanging difftest.
struct Worker {
    jobs: mpsc::Sender<String>,
    replies: mpsc::Receiver<Reply>,
}

impl Worker {
    fn spawn(mut client: Client) -> Worker {
        let (jobs, job_rx) = mpsc::channel::<String>();
        let (reply_tx, replies) = mpsc::channel::<Reply>();
        std::thread::spawn(move || {
            for sql in job_rx {
                if reply_tx.send(client.simple_query(&sql)).is_err() {
                    break;
                }
            }
        });
        Worker { jobs, replies }
    }
}

pub(crate) struct Server {
    pub(crate) label: &'static str,
    config: Config,
    timeout: Duration,
    worker: Option<Worker>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("label", &self.label)
            .field("connected", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Connects; every statement must be answered within `timeout`.
    pub(crate) fn connect(
        label: &'static str,
        conninfo: &str,
        timeout: Duration,
    ) -> Result<Server, String> {
        let mut config: Config = conninfo
            .parse()
            .map_err(|e| format!("bad connection string for {label} server ({conninfo}): {e}"))?;
        config.connect_timeout(timeout);
        let mut s = Server {
            label,
            config,
            timeout,
            worker: None,
        };
        s.ensure()
            .map_err(|e| format!("{e} (connection string: {conninfo})"))?;
        Ok(s)
    }

    /// Reconnects if the previous connection was lost.
    pub(crate) fn ensure(&mut self) -> Result<(), String> {
        if self.worker.is_none() {
            let c = self
                .config
                .connect(NoTls)
                .map_err(|e| format!("cannot connect to {} server: {e}", self.label))?;
            self.worker = Some(Worker::spawn(c));
        }
        Ok(())
    }

    fn roundtrip(&mut self, sql: &str) -> Result<Reply, String> {
        self.ensure()?;
        let w = self.worker.as_ref().expect("connected");
        let sent = w.jobs.send(sql.to_string()).is_ok();
        let reply = if sent {
            w.replies.recv_timeout(self.timeout)
        } else {
            Err(mpsc::RecvTimeoutError::Disconnected)
        };
        reply.map_err(|e| {
            // The worker thread stays blocked on the dead connection; it ends
            // when the server finally answers or closes the socket.
            self.worker = None;
            match e {
                mpsc::RecvTimeoutError::Timeout => format!(
                    "no response within {}s (server hung?)",
                    self.timeout.as_secs_f64()
                ),
                mpsc::RecvTimeoutError::Disconnected => "connection worker ended".to_string(),
            }
        })
    }

    pub(crate) fn exec(&mut self, sql: &str) -> Outcome {
        match self.roundtrip(sql) {
            Err(e) => Outcome::Lost(e),
            Ok(Ok(msgs)) => {
                let mut cols: Option<Vec<String>> = None;
                let mut rows = Vec::new();
                let mut count = 0;
                for m in msgs {
                    match m {
                        SimpleQueryMessage::RowDescription(desc) => {
                            cols = Some(desc.iter().map(|c| c.name().to_string()).collect());
                        }
                        SimpleQueryMessage::Row(r) => {
                            if cols.is_none() {
                                cols = Some(
                                    r.columns().iter().map(|c| c.name().to_string()).collect(),
                                );
                            }
                            rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
                        }
                        SimpleQueryMessage::CommandComplete(n) => count = n,
                        _ => {}
                    }
                }
                match cols {
                    Some(cols) => Outcome::Rows { cols, rows },
                    None => Outcome::Done(count),
                }
            }
            Ok(Err(e)) => {
                if let Some(db) = e.as_db_error() {
                    Outcome::Error {
                        code: db.code().code().to_string(),
                        message: db.message().to_string(),
                    }
                } else {
                    self.worker = None;
                    Outcome::Lost(e.to_string())
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum ErrorMatch {
    /// SQLSTATE must be identical.
    Exact,
    /// Only the SQLSTATE class (first two characters) must match.
    Class,
    /// Both sides must fail; the code is not compared.
    Any,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum EvalOrder {
    /// Treat plan-dependent error outcomes as inconclusive (counted, not reported).
    Skip,
    /// Report them like any other difference.
    Report,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CmpOpts {
    pub(crate) errors: ErrorMatch,
    pub(crate) names: bool,
    pub(crate) eval_order: EvalOrder,
}

/// Properties of a statement that relax the comparison.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Traits {
    /// The statement has an ORDER BY over all output columns.
    pub(crate) ordered: bool,
    /// Which of `-0` and `0` (equal values) is output is not determined:
    /// DISTINCT / GROUP BY / min / max / LIMIT keep an arbitrary one.
    pub(crate) zero_ambiguous: bool,
    /// Whether a data exception (SQLSTATE class 22) is raised may depend on
    /// the evaluation order chosen by the planner: PostgreSQL reorders
    /// top-level AND conditions by cost, folds `x AND FALSE`, and picks join
    /// orders and when to run subqueries freely.
    pub(crate) eval_sensitive: bool,
}

fn is_data_exception(o: &Outcome) -> bool {
    matches!(o, Outcome::Error { code, .. } if code.starts_with("22"))
}

/// True when the outcomes differ only in a way the evaluation order can
/// explain (data exception on one side and rows or a different data
/// exception on the other) and the statement is evaluation-order sensitive.
pub(crate) fn eval_order_inconclusive(r: &Outcome, t: &Outcome, tr: Traits, opts: CmpOpts) -> bool {
    if !tr.eval_sensitive || opts.eval_order == EvalOrder::Report {
        return false;
    }
    let rows = |o: &Outcome| matches!(o, Outcome::Rows { .. });
    (is_data_exception(r) && (rows(t) || is_data_exception(t)))
        || (is_data_exception(t) && rows(r))
}

fn sorted(rows: &[Vec<Option<String>>]) -> Vec<Vec<Option<String>>> {
    let mut v = rows.to_vec();
    v.sort();
    v
}

/// `-0` and `0` are equal values whose textual forms differ; normalize them
/// where the choice between them is not determined.
fn norm_zero(rows: &[Vec<Option<String>>]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c.as_deref() {
                    Some("-0") => Some("0".to_string()),
                    _ => c.clone(),
                })
                .collect()
        })
        .collect()
}

/// `None` if the outcomes agree; otherwise a short reason.
pub(crate) fn compare(r: &Outcome, t: &Outcome, tr: Traits, opts: CmpOpts) -> Option<String> {
    let ordered = tr.ordered;
    match (r, t) {
        (Outcome::Error { code: a, .. }, Outcome::Error { code: b, .. }) => {
            let same = match opts.errors {
                ErrorMatch::Exact => a == b,
                ErrorMatch::Class => a.get(..2) == b.get(..2),
                ErrorMatch::Any => true,
            };
            (!same).then(|| format!("SQLSTATE differs: reference {a}, test {b}"))
        }
        (Outcome::Done(a), Outcome::Done(b)) => {
            (a != b).then(|| format!("row count differs: {a} vs {b}"))
        }
        (Outcome::Rows { cols: ca, rows: ra }, Outcome::Rows { cols: cb, rows: rb }) => {
            if ca.len() != cb.len() {
                return Some(format!(
                    "column count differs: {} vs {}",
                    ca.len(),
                    cb.len()
                ));
            }
            if opts.names && ca != cb {
                return Some(format!("column names differ: {ca:?} vs {cb:?}"));
            }
            if ra.len() != rb.len() {
                return Some(format!("row count differs: {} vs {}", ra.len(), rb.len()));
            }
            let (na, nb);
            let (ra, rb) = if tr.zero_ambiguous {
                na = norm_zero(ra);
                nb = norm_zero(rb);
                (&na, &nb)
            } else {
                (ra, rb)
            };
            if ra == rb {
                return None;
            }
            let same_set = sorted(ra) == sorted(rb);
            if !ordered {
                return (!same_set).then(|| "result rows differ".to_string());
            }
            if same_set && norm_zero(ra) == norm_zero(rb) {
                None
            } else if same_set {
                Some("row order differs".to_string())
            } else {
                Some("result rows differ".to_string())
            }
        }
        (Outcome::Lost(_), _) | (_, Outcome::Lost(_)) => Some("connection lost".to_string()),
        _ => Some("outcome kind differs".to_string()),
    }
}

/// Ternary logic partitioning: rows(Q) must equal the bag union of
/// rows(Q WHERE p), rows(Q WHERE NOT p) and rows(Q WHERE p IS NULL).
/// Returns `None` when the identity holds or cannot be checked (errors).
pub(crate) fn tlp_violation(base: &Outcome, parts: &[Outcome], distinct: bool) -> Option<String> {
    let Outcome::Rows { rows: b, .. } = base else {
        return None;
    };
    let mut union = Vec::new();
    for p in parts {
        let Outcome::Rows { rows, .. } = p else {
            return None;
        };
        union.extend(rows.iter().cloned());
    }
    let mut b = sorted(b);
    if distinct {
        // DISTINCT keeps an arbitrary one of `-0` / `0`, so a partition may
        // keep the other one.
        b = sorted(&norm_zero(&b));
        union = norm_zero(&union);
    }
    union.sort();
    if distinct {
        b.dedup();
        union.dedup();
    }
    (b != union).then(|| {
        format!(
            "TLP violated: base query returned {} row(s), partitions returned {} row(s) in total",
            b.len(),
            union.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&[&str]]) -> Outcome {
        Outcome::Rows {
            cols: vec!["a".into(); v.first().map_or(1, |r| r.len())],
            rows: v
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| (*c != "NULL").then(|| (*c).to_string()))
                        .collect()
                })
                .collect(),
        }
    }

    const OPTS: CmpOpts = CmpOpts {
        errors: ErrorMatch::Exact,
        names: false,
    };

    #[test]
    fn compare_rules() {
        let a = rows(&[&["1"], &["2"]]);
        let b = rows(&[&["2"], &["1"]]);
        assert!(compare(&a, &b, false, OPTS).is_none());
        assert!(compare(&a, &b, true, OPTS).is_some());
        let z1 = rows(&[&["-0"], &["0"]]);
        let z2 = rows(&[&["0"], &["-0"]]);
        assert!(compare(&z1, &z2, true, OPTS).is_none());
        let e1 = Outcome::Error {
            code: "22012".into(),
            message: String::new(),
        };
        let e2 = Outcome::Error {
            code: "22003".into(),
            message: String::new(),
        };
        assert!(compare(&e1, &e2, false, OPTS).is_some());
        let class = CmpOpts {
            errors: ErrorMatch::Class,
            names: false,
        };
        assert!(compare(&e1, &e2, false, class).is_none());
        assert!(compare(&e1, &a, false, OPTS).is_some());
    }

    #[test]
    fn tlp_rules() {
        let base = rows(&[&["1"], &["2"], &["NULL"]]);
        let parts = [rows(&[&["1"]]), rows(&[&["2"]]), rows(&[&["NULL"]])];
        assert!(tlp_violation(&base, &parts, false).is_none());
        let parts = [rows(&[&["1"]]), rows(&[&["2"]]), rows(&[])];
        assert!(tlp_violation(&base, &parts, false).is_some());
        let base = rows(&[&["1"]]);
        let parts = [rows(&[&["1"]]), rows(&[&["1"]]), rows(&[])];
        assert!(tlp_violation(&base, &parts, true).is_none());
    }
}
