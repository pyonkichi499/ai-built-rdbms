//! psql を子プロセスで起動し、1 シナリオ = 1 セッションで文を流す。
//!
//! psql は `-A -t`（整列なし・ヘッダなし）、`VERBOSITY=verbose`（SQLSTATE 付きのエラー）で動かす。
//! 標準エラーを標準出力に合流させ（sh 経由の `2>&1`）、文ごとに `\warn @@END@@ n` を流して区切りにする。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const MARK: &str = "@@END@@";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Res {
    Ok { rows: Vec<String>, tag: String },
    Err { sqlstate: String, message: String },
    Timeout,
    Disconnected(String),
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    n: u64,
}

impl Session {
    pub fn open(psql: &str, conninfo: &str) -> std::io::Result<Self> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exec \"$0\" \"$@\" 2>&1")
            .arg(psql)
            .args(["-X", "-A", "-t", "-F", "|", "-P", "null=<NULL>"])
            .args(["-v", "VERBOSITY=verbose", "-v", "ON_ERROR_STOP=0"])
            .arg(conninfo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin");
        let out = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let s = String::from_utf8_lossy(&buf);
                        if tx
                            .send(s.trim_end_matches(['\n', '\r']).to_string())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Session {
            child,
            stdin,
            rx,
            n: 0,
        })
    }

    /// 1 文（1 行、`;` で終わる）を流して正規化した結果を返す。
    pub fn exec(&mut self, sql: &str, timeout: Duration) -> Res {
        let sent = self.send(sql);
        self.finish(sql, sent, timeout)
    }

    /// 文を流すだけ（結果は `finish` で受ける）。2 つのサーバへ同時に流して待ち時間を重ねるのに使う。
    pub fn send(&mut self, sql: &str) -> bool {
        self.n += 1;
        let marker = format!("{MARK} {}", self.n);
        write!(self.stdin, "{sql}\n\\warn {marker}\n")
            .and_then(|_| self.stdin.flush())
            .is_ok()
    }

    /// `send` した文の結果を待って正規化する。
    pub fn finish(&mut self, sql: &str, sent: bool, timeout: Duration) -> Res {
        if !sent {
            return self.drain_disconnected(Vec::new());
        }
        let marker = format!("{MARK} {}", self.n);
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(l) if l == marker => break,
                Ok(l) => lines.push(l),
                Err(RecvTimeoutError::Timeout) => return Res::Timeout,
                Err(RecvTimeoutError::Disconnected) => return self.drain_disconnected(lines),
            }
        }
        normalize(sql, &lines)
    }

    fn drain_disconnected(&mut self, lines: Vec<String>) -> Res {
        let _ = self.child.wait();
        Res::Disconnected(lines.join(" / "))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn first_word(sql: &str) -> String {
    sql.trim_start()
        .trim_start_matches('(')
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

const NOISE: [&str; 6] = [
    "NOTICE:",
    "WARNING:",
    "DETAIL:",
    "HINT:",
    "LOCATION:",
    "CONTEXT:",
];

/// psql の出力を (行, コマンドタグ) か (SQLSTATE, メッセージ) に正規化する。
/// 通知・DETAIL・HINT・LOCATION・LINE/キャレットは比較対象外。
pub fn normalize(sql: &str, lines: &[String]) -> Res {
    for l in lines {
        let l = l.strip_prefix("psql:").map_or(l.as_str(), |r| {
            // "psql:<stdin>:3: ERROR: ..." のような接頭辞
            r.find(": ").map_or(l.as_str(), |i| &r[i + 2..])
        });
        let body = l
            .strip_prefix("ERROR:  ")
            .or_else(|| l.strip_prefix("FATAL:  "));
        if let Some(body) = body {
            return match body.split_once(": ") {
                Some((st, msg))
                    if st.len() == 5 && st.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    Res::Err {
                        sqlstate: st.to_string(),
                        message: msg.to_string(),
                    }
                }
                _ => Res::Err {
                    sqlstate: "?????".into(),
                    message: body.to_string(),
                },
            };
        }
        if l.starts_with("connection to server") || l.contains("server closed the connection") {
            return Res::Disconnected(l.to_string());
        }
    }
    let mut kept: Vec<String> = lines
        .iter()
        .filter(|l| !NOISE.iter().any(|p| l.starts_with(p)))
        .cloned()
        .collect();
    let kw = first_word(sql);
    let has_returning = sql.to_ascii_uppercase().contains(" RETURNING ");
    let rows_then_tag = matches!(kw.as_str(), "INSERT" | "UPDATE" | "DELETE") && has_returning;
    let rows_only = matches!(kw.as_str(), "SELECT" | "VALUES" | "WITH" | "TABLE" | "SHOW");
    if rows_only {
        Res::Ok {
            rows: kept,
            tag: String::new(),
        }
    } else if rows_then_tag {
        let tag = kept.pop().unwrap_or_default();
        Res::Ok { rows: kept, tag }
    } else {
        Res::Ok {
            rows: Vec::new(),
            tag: kept.join(" / "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parenthesised_select_is_a_row_result() {
        let r = normalize(
            "(SELECT 1 UNION SELECT 2) ORDER BY 1;",
            &["1".to_string(), "2".to_string()],
        );
        assert_eq!(
            r,
            Res::Ok {
                rows: vec!["1".into(), "2".into()],
                tag: String::new()
            }
        );
    }

    #[test]
    fn error_lines_carry_sqlstate_and_message() {
        let r = normalize(
            "SELECT 1/0;",
            &["ERROR:  22012: division by zero".to_string()],
        );
        assert_eq!(
            r,
            Res::Err {
                sqlstate: "22012".into(),
                message: "division by zero".into()
            }
        );
    }

    #[test]
    fn ddl_returns_the_tag() {
        let r = normalize("CREATE TABLE t (a int);", &["CREATE TABLE".to_string()]);
        assert_eq!(
            r,
            Res::Ok {
                rows: vec![],
                tag: "CREATE TABLE".into()
            }
        );
    }
}
