//! 単体テスト用の疑似サーバ。Simple Query と Extended Query（Parse / Bind / Describe /
//! Execute / Sync）を受けて固定の応答を返す小さな TCP サーバで、受けたメッセージの種類を記録する。
//!
//! 文の意味（テスト用の約束）:
//! - 空の文: `EmptyQueryResponse`
//! - `SELECT <n>`: 1 列 1 行（値 n）
//! - `pg_isolation_test_session_is_blocked` を含む文: `LOCKED` が待っているあいだ `t`、それ以外は `f`
//! - `LOCKED`: `RELEASE` が来るまで応答しない（ロック待ちの代わり）
//! - `RELEASE`: `LOCKED` を解く
//! - `ERR`: エラー
//! - それ以外: `CommandComplete`

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::conn::ConnParams;
use crate::runner::{BlockingDetection, Options, Protocol, run_spec};
use crate::spec;

/// 受信したメッセージの記録（接続番号, タグ）。接続番号は接続順で、制御接続が 0。
#[derive(Debug, Default)]
pub(crate) struct Shared {
    pub(crate) log: Mutex<Vec<(usize, u8)>>,
    locked: AtomicBool,
    conns: AtomicUsize,
}

impl Shared {
    /// 接続 `conn` が受け取ったメッセージのタグ列。
    pub(crate) fn tags(&self, conn: usize) -> String {
        // Terminate の到着を待つ。
        thread::sleep(Duration::from_millis(150));
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, t)| char::from(*t))
            .collect()
    }
}

pub(crate) fn start() -> (u16, Arc<Shared>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Shared::default());
    let s = Arc::clone(&shared);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let id = s.conns.fetch_add(1, Ordering::SeqCst);
            let s = Arc::clone(&s);
            thread::spawn(move || {
                let _ = handle(stream, id, &s);
            });
        }
    });
    (port, shared)
}

fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
    v.extend_from_slice(body);
    v
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn row_description() -> Vec<u8> {
    let mut b = 1i16.to_be_bytes().to_vec();
    b.extend(cstr("?column?"));
    b.extend(0i32.to_be_bytes());
    b.extend(0i16.to_be_bytes());
    b.extend(23i32.to_be_bytes());
    b.extend(4i16.to_be_bytes());
    b.extend((-1i32).to_be_bytes());
    b.extend(0i16.to_be_bytes());
    msg(b'T', &b)
}

fn data_row(v: &str) -> Vec<u8> {
    let mut b = 1i16.to_be_bytes().to_vec();
    b.extend(i32::try_from(v.len()).unwrap().to_be_bytes());
    b.extend_from_slice(v.as_bytes());
    msg(b'D', &b)
}

fn is_select_n(t: &str) -> Option<&str> {
    t.strip_prefix("SELECT ")
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

/// 1 文を実行した応答。`describe` が真なら行を返す文の前に `RowDescription` を付ける。
/// 戻り値の真偽はエラーかどうか。
fn exec(text: &str, describe: bool, shared: &Shared) -> (Vec<u8>, bool) {
    let t = text.trim();
    let mut out = Vec::new();
    if t.is_empty() {
        return (msg(b'I', &[]), false);
    }
    if t.contains("pg_isolation_test_session_is_blocked") {
        let v = if shared.locked.load(Ordering::SeqCst) {
            "t"
        } else {
            "f"
        };
        if describe {
            out.extend(row_description());
        }
        out.extend(data_row(v));
        out.extend(msg(b'C', &cstr("SELECT 1")));
    } else if t == "ERR" {
        let mut b = vec![b'S'];
        b.extend(cstr("ERROR"));
        b.push(b'M');
        b.extend(cstr("boom"));
        b.push(0);
        return (msg(b'E', &b), true);
    } else if t == "LOCKED" {
        shared.locked.store(true, Ordering::SeqCst);
        while shared.locked.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(2));
        }
        out.extend(msg(b'C', &cstr("LOCK")));
    } else if t == "RELEASE" {
        shared.locked.store(false, Ordering::SeqCst);
        out.extend(msg(b'C', &cstr("RELEASE")));
    } else if let Some(n) = is_select_n(t) {
        if describe {
            out.extend(row_description());
        }
        out.extend(data_row(n));
        out.extend(msg(b'C', &cstr("SELECT 1")));
    } else {
        out.extend(msg(b'C', &cstr("OK")));
    }
    (out, false)
}

fn read_cstr(b: &[u8]) -> (&str, &[u8]) {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    (
        std::str::from_utf8(&b[..end]).unwrap_or(""),
        b.get(end + 1..).unwrap_or(&[]),
    )
}

fn handle(mut s: TcpStream, id: usize, shared: &Shared) -> std::io::Result<()> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let mut startup = vec![0u8; usize::try_from(i32::from_be_bytes(len)).unwrap() - 4];
    s.read_exact(&mut startup)?;
    let mut hello = msg(b'R', &0i32.to_be_bytes());
    let mut key = i32::try_from(1000 + id).unwrap().to_be_bytes().to_vec();
    key.extend(7i32.to_be_bytes());
    hello.extend(msg(b'K', &key));
    hello.extend(msg(b'Z', b"I"));
    s.write_all(&hello)?;

    let mut stmts: HashMap<String, String> = HashMap::new();
    let mut portal = String::new();
    let mut in_error = false;
    loop {
        let mut head = [0u8; 5];
        s.read_exact(&mut head)?;
        let tag = head[0];
        let n =
            usize::try_from(i32::from_be_bytes([head[1], head[2], head[3], head[4]])).unwrap() - 4;
        let mut body = vec![0u8; n];
        s.read_exact(&mut body)?;
        shared.log.lock().unwrap().push((id, tag));
        let mut out = Vec::new();
        match tag {
            b'X' => return Ok(()),
            b'Q' => {
                let (sql, _) = read_cstr(&body);
                let parts: Vec<&str> = sql
                    .split(';')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect();
                if parts.is_empty() {
                    out.extend(msg(b'I', &[]));
                }
                for p in parts {
                    let (o, err) = exec(p, true, shared);
                    out.extend(o);
                    if err {
                        break;
                    }
                }
                out.extend(msg(b'Z', b"I"));
            }
            b'S' => {
                in_error = false;
                out.extend(msg(b'Z', b"I"));
            }
            _ if in_error => {}
            b'P' => {
                let (name, rest) = read_cstr(&body);
                let (query, _) = read_cstr(rest);
                stmts.insert(name.to_owned(), query.to_owned());
                out.extend(msg(b'1', &[]));
            }
            b'B' => {
                let (_, rest) = read_cstr(&body);
                let (stmt, _) = read_cstr(rest);
                portal = stmts.get(stmt).cloned().unwrap_or_default();
                out.extend(msg(b'2', &[]));
            }
            b'D' => {
                let t = portal.trim();
                if is_select_n(t).is_some() {
                    out.extend(row_description());
                } else {
                    out.extend(msg(b'n', &[]));
                }
            }
            b'E' => {
                let (o, err) = exec(&portal, false, shared);
                out.extend(o);
                in_error = err;
            }
            _ => {}
        }
        s.write_all(&out)?;
    }
}

/// テスト用のランナー設定。
pub(crate) fn options(port: u16, protocol: Protocol, detection: BlockingDetection) -> Options {
    Options {
        conn: ConnParams {
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            dbname: "d".into(),
            password: None,
            startup_params: vec![],
        },
        protocol,
        detection,
        block_timeout: Duration::from_millis(50),
        max_step_wait: Duration::from_secs(10),
    }
}

fn run(src: &str, port: u16, protocol: Protocol, detection: BlockingDetection) -> (String, bool) {
    let spec = spec::parse(src).unwrap();
    run_spec(&spec, "t", &options(port, protocol, detection))
}

const PLAIN: &str = "session s1\nstep a { SELECT 1; SELECT 2; }\nstep b { SELECT 3 }\n\
                     session s2\nstep c { ERR; SELECT 4 }\nstep d { }\n\
                     permutation a c b d\n";

#[test]
fn extended_output_equals_simple() {
    let (port, shared) = start();
    let (simple, f1) = run(PLAIN, port, Protocol::Simple, BlockingDetection::Pg);
    let (ext, f2) = run(PLAIN, port, Protocol::Extended, BlockingDetection::Pg);
    assert!(!f1 && !f2, "{simple}\n{ext}");
    assert_eq!(simple, ext);
    assert!(simple.contains("step a: SELECT 1; SELECT 2;"), "{simple}");
    assert!(simple.contains("ERROR:  boom"), "{simple}");
    assert!(simple.contains("?column?"), "{simple}");
    // 2 回目の run（Extended）は接続 3 が制御、4 が s1。ステップは P B D E ... S の形で届く。
    let tags = shared.tags(4);
    assert!(
        tags.contains('P') && tags.contains('E') && tags.ends_with('X'),
        "{tags}"
    );
}

#[test]
fn extended_sends_one_sync_per_step() {
    let (port, shared) = start();
    let src = "session s1\nstep a { SELECT 1; SELECT 2; SELECT 3 }\npermutation a\n";
    let (out, fatal) = run(src, port, Protocol::Extended, BlockingDetection::Timeout);
    assert!(!fatal, "{out}");
    // 接続 1 は s1。3 文 = (P B D E) x 3 + S、その後 Terminate。
    assert_eq!(shared.tags(1), "PBDEPBDEPBDESX");
}

#[test]
fn unterminated_step_falls_back_to_simple() {
    let (port, shared) = start();
    let src = "session s1\nstep a { SELECT 'x; }\npermutation a\n";
    let (_, _) = run(src, port, Protocol::Extended, BlockingDetection::Timeout);
    assert_eq!(shared.tags(1), "QX");
}

#[test]
fn control_connection_parses_once_and_binds_each_time() {
    let (port, shared) = start();
    let src = "session s1\nstep lock { LOCKED }\nsession s2\nstep rel { RELEASE }\n\
               permutation lock rel\n";
    for protocol in [Protocol::Simple, Protocol::Extended] {
        let before = shared.conns.load(Ordering::SeqCst);
        let (out, fatal) = run(src, port, protocol, BlockingDetection::Pg);
        assert!(!fatal, "{out}");
        assert_eq!(
            out,
            "Parsed test spec with 2 sessions\n\nstarting permutation: lock rel\n\
             step lock: LOCKED <waiting ...>\nstep rel: RELEASE\nstep lock: <... completed>\n"
        );
        let tags = shared.tags(before);
        // Parse 1 回 + Sync、その後は (Bind Execute Sync) の繰り返し、最後に Terminate。
        assert!(tags.starts_with("PS"), "{tags}");
        assert_eq!(tags.matches('P').count(), 1, "{tags}");
        let rest = tags.strip_prefix("PS").unwrap().strip_suffix('X').unwrap();
        assert!(!rest.is_empty(), "{tags}");
        assert!(rest.as_bytes().chunks(3).all(|c| c == b"BES"), "{tags}");
    }
}

#[test]
fn timeout_detection_does_not_prepare() {
    let (port, shared) = start();
    let src = "session s1\nstep lock { LOCKED }\nsession s2\nstep rel { RELEASE }\n\
               permutation lock rel\n";
    let (out, fatal) = run(src, port, Protocol::Simple, BlockingDetection::Timeout);
    assert!(!fatal, "{out}");
    assert!(out.contains("step lock: LOCKED <waiting ...>"), "{out}");
    assert_eq!(shared.tags(0), "X");
}
