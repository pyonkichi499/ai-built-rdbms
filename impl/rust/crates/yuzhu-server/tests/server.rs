//! Integration tests: start the server in-process on an ephemeral port and
//! talk to it with the `postgres` crate and with raw TCP.
//!
//! These test protocol behavior, not SQL results (the core may still answer
//! queries with an error).

#![allow(clippy::doc_markdown)] // protocol message names in docs

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use yuzhu_core::testing::TestCluster;
use yuzhu_server::Server;
use yuzhu_server::config::Config;

fn start_server(max_connections: usize) -> SocketAddr {
    start_server_with(max_connections, |s| s)
}

// ---------------------------------------------------------------------------
// Raw protocol client
// ---------------------------------------------------------------------------

struct Raw {
    s: TcpStream,
}

#[derive(Debug)]
struct Msg {
    tag: u8,
    body: Vec<u8>,
}

impl Msg {
    /// Parses ErrorResponse / NoticeResponse fields.
    fn fields(&self) -> HashMap<u8, String> {
        let mut out = HashMap::new();
        let mut rest = self.body.as_slice();
        while let Some((&code, tail)) = rest.split_first() {
            if code == 0 {
                break;
            }
            let end = tail.iter().position(|&b| b == 0).unwrap();
            out.insert(code, String::from_utf8(tail[..end].to_vec()).unwrap());
            rest = &tail[end + 1..];
        }
        out
    }

    fn cstrs(&self) -> Vec<String> {
        self.body
            .split(|&b| b == 0)
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .collect()
    }
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self { s }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).unwrap();
    }

    fn send_startup_code(&mut self, code: u32, payload: &[u8]) {
        let len = u32::try_from(8 + payload.len()).unwrap();
        let mut v = len.to_be_bytes().to_vec();
        v.extend_from_slice(&code.to_be_bytes());
        v.extend_from_slice(payload);
        self.send(&v);
    }

    fn send_startup(&mut self, params: &[(&str, &str)]) {
        let mut payload = Vec::new();
        for (k, v) in params {
            payload.extend_from_slice(k.as_bytes());
            payload.push(0);
            payload.extend_from_slice(v.as_bytes());
            payload.push(0);
        }
        payload.push(0);
        self.send_startup_code(196_608, &payload);
    }

    fn send_msg(&mut self, tag: u8, body: &[u8]) {
        let mut v = vec![tag];
        v.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        self.send(&v);
    }

    fn query(&mut self, sql: &str) {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_msg(b'Q', &body);
    }

    fn read_byte(&mut self) -> u8 {
        let mut b = [0u8; 1];
        self.s.read_exact(&mut b).unwrap();
        b[0]
    }

    fn read_msg(&mut self) -> Msg {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).unwrap();
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).unwrap();
        Msg { tag: h[0], body }
    }

    /// Reads messages up to and including ReadyForQuery.
    fn read_until_ready(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        loop {
            let m = self.read_msg();
            let done = m.tag == b'Z';
            out.push(m);
            if done {
                return out;
            }
        }
    }

    fn expect_eof(&mut self) {
        let mut buf = [0u8; 16];
        let n = self.s.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "expected the server to close the connection");
    }

    /// Full startup as user `postgres`; returns the ParameterStatus values.
    fn handshake(addr: SocketAddr) -> (Self, HashMap<String, String>) {
        let mut c = Self::connect(addr);
        c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
        let msgs = c.read_until_ready();
        let params = check_startup_response(&msgs);
        (c, params)
    }
}

/// Validates R, S*, K, Z(I) and returns the parameters.
fn check_startup_response(msgs: &[Msg]) -> HashMap<String, String> {
    assert_eq!(msgs[0].tag, b'R', "{msgs:?}");
    assert_eq!(msgs[0].body, [0, 0, 0, 0]);
    let mut params = HashMap::new();
    let mut saw_key = false;
    for m in &msgs[1..msgs.len() - 1] {
        match m.tag {
            b'S' => {
                let s = m.cstrs();
                params.insert(s[0].clone(), s[1].clone());
            }
            b'K' => {
                assert_eq!(m.body.len(), 8);
                let pid = i32::from_be_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]);
                assert!(pid >= 1);
                saw_key = true;
            }
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert!(saw_key, "BackendKeyData missing");
    let last = msgs.last().unwrap();
    assert_eq!((last.tag, last.body.as_slice()), (b'Z', b"I".as_slice()));
    params
}

// ---------------------------------------------------------------------------
// postgres crate
// ---------------------------------------------------------------------------

fn pg_connect(addr: SocketAddr) -> postgres::Client {
    postgres::Client::connect(
        &format!(
            "host={} port={} user=postgres dbname=postgres application_name=itest",
            addr.ip(),
            addr.port()
        ),
        postgres::NoTls,
    )
    .expect("connect")
}

#[test]
fn postgres_crate_connects_and_queries_get_responses() {
    let addr = start_server(10);
    let mut client = pg_connect(addr);
    for sql in ["SELECT 1", "", "SELECT 1; SELECT 2", "nonsense query"] {
        match client.simple_query(sql) {
            Ok(_) => {}
            Err(e) => assert!(e.as_db_error().is_some(), "{sql:?}: {e}"),
        }
        assert!(!client.is_closed());
    }
    // The connection is still usable after errors.
    let _ = client.batch_execute("BEGIN; ROLLBACK");
    assert!(!client.is_closed());
    client.close().unwrap();
}

#[test]
fn postgres_crate_extended_query_gets_feature_not_supported() {
    let addr = start_server(10);
    let mut client = pg_connect(addr);
    let err = client.query("SELECT 1", &[]).unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code().code(), "0A000");
    // Simple Query still works afterwards.
    let _ = client.simple_query("SELECT 1");
    assert!(!client.is_closed());
}

#[test]
fn many_concurrent_clients() {
    let addr = start_server(100);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let mut c = pg_connect(addr);
                for _ in 0..5 {
                    let _ = c.simple_query("SELECT 1");
                }
                assert!(!c.is_closed());
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Raw protocol
// ---------------------------------------------------------------------------

#[test]
fn handshake_parameter_status() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup(&[
        ("user", "postgres"),
        ("database", "postgres"),
        ("application_name", "myapp"),
        ("client_encoding", "UTF8"),
    ]);
    let params = check_startup_response(&c.read_until_ready());
    let expected = [
        ("server_version", "17.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("IntervalStyle", "postgres"),
        ("TimeZone", "UTC"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("is_superuser", "on"),
        ("session_authorization", "postgres"),
        ("application_name", "myapp"),
        ("default_transaction_read_only", "off"),
        ("in_hot_standby", "off"),
    ];
    for (k, v) in expected {
        assert_eq!(params.get(k).map(String::as_str), Some(v), "parameter {k}");
    }
}

#[test]
fn backend_pids_are_distinct() {
    let addr = start_server(10);
    let pid = |addr| {
        let mut c = Raw::connect(addr);
        c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
        let msgs = c.read_until_ready();
        let k = msgs.iter().find(|m| m.tag == b'K').unwrap();
        i32::from_be_bytes([k.body[0], k.body[1], k.body[2], k.body[3]])
    };
    assert_ne!(pid(addr), pid(addr));
}

#[test]
fn ssl_and_gssenc_requests_are_declined() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup_code(80_877_103, &[]);
    assert_eq!(c.read_byte(), b'N');
    c.send_startup_code(80_877_104, &[]);
    assert_eq!(c.read_byte(), b'N');
    c.send_startup_code(80_877_103, &[]);
    assert_eq!(c.read_byte(), b'N');
    c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
    check_startup_response(&c.read_until_ready());
}

#[test]
fn query_gets_exactly_one_ready_for_query() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.query("SELECT 1");
    let msgs = c.read_until_ready();
    assert!(msgs.len() >= 2, "{msgs:?}");
    c.query("");
    let msgs = c.read_until_ready();
    assert!(msgs.iter().all(|m| m.tag != b'Z' || m.body.len() == 1));
}

#[test]
fn extended_query_message_is_rejected_until_sync() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send_msg(b'P', b"\0SELECT 1\0\0\0");
    c.send_msg(b'B', b"\0\0\0\0\0\0\0\0");
    c.send_msg(b'D', b"P\0");
    c.send_msg(b'E', b"\0\0\0\0\0");
    c.send_msg(b'S', b"");
    let err = c.read_msg();
    assert_eq!(err.tag, b'E');
    let f = err.fields();
    assert_eq!(f[&b'S'], "ERROR");
    assert_eq!(f[&b'V'], "ERROR");
    assert_eq!(f[&b'C'], "0A000");
    assert!(f.contains_key(&b'M'));
    let z = c.read_msg();
    assert_eq!((z.tag, z.body.as_slice()), (b'Z', b"I".as_slice()));
    // Still alive.
    c.query("SELECT 1");
    c.read_until_ready();
}

#[test]
fn extended_query_error_is_flushed_before_sync() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send_msg(b'P', b"\0SELECT 1\0\0\0");
    c.send_msg(b'H', b"");
    // The error must arrive without waiting for Sync.
    let err = c.read_msg();
    assert_eq!(err.tag, b'E');
    assert_eq!(err.fields()[&b'C'], "0A000");
    c.send_msg(b'C', b"S\0");
    c.send_msg(b'S', b"");
    let z = c.read_msg();
    assert_eq!(z.tag, b'Z');
}

#[test]
fn terminate_closes_connection() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send_msg(b'X', b"");
    c.expect_eof();
}

#[test]
fn unknown_message_is_fatal_protocol_violation() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send_msg(b'z', b"junk");
    let err = c.read_msg();
    assert_eq!(err.tag, b'E');
    let f = err.fields();
    assert_eq!(f[&b'S'], "FATAL");
    assert_eq!(f[&b'C'], "08P01");
    c.expect_eof();
}

#[test]
fn oversized_message_is_fatal() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send(&[b'Q', 0x7f, 0xff, 0xff, 0xff]);
    let err = c.read_msg();
    assert_eq!(err.fields()[&b'C'], "08P01");
    assert_eq!(err.fields()[&b'S'], "FATAL");
    c.expect_eof();
}

#[test]
fn invalid_utf8_query_is_an_error_not_fatal() {
    let addr = start_server(10);
    let (mut c, _) = Raw::handshake(addr);
    c.send_msg(b'Q', b"SELECT '\xff'\0");
    let err = c.read_msg();
    assert_eq!(err.fields()[&b'C'], "22021");
    assert_eq!(c.read_msg().tag, b'Z');
    c.query("SELECT 1");
    c.read_until_ready();
}

#[test]
fn cancel_request_closes_connection() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    let mut payload = 1i32.to_be_bytes().to_vec();
    payload.extend_from_slice(&1234i32.to_be_bytes());
    c.send_startup_code(80_877_102, &payload);
    c.expect_eof();
}

/// Handshake that also returns the `BackendKeyData` (pid, secret).
fn handshake_with_key(addr: SocketAddr) -> (Raw, i32, i32) {
    let mut c = Raw::connect(addr);
    c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
    let msgs = c.read_until_ready();
    check_startup_response(&msgs);
    let k = msgs.iter().find(|m| m.tag == b'K').expect("BackendKeyData");
    let word =
        |i: usize| i32::from_be_bytes([k.body[i], k.body[i + 1], k.body[i + 2], k.body[i + 3]]);
    (c, word(0), word(4))
}

fn send_cancel(addr: SocketAddr, pid: i32, secret: i32) {
    let mut c = Raw::connect(addr);
    let mut payload = pid.to_be_bytes().to_vec();
    payload.extend_from_slice(&secret.to_be_bytes());
    c.send_startup_code(80_877_102, &payload);
    c.expect_eof();
}

#[test]
fn cancel_request_cancels_the_running_statement() {
    let addr = start_server(10);
    let (mut c, pid, secret) = handshake_with_key(addr);
    c.query("SELECT pg_sleep('30'::float8)");
    std::thread::sleep(Duration::from_millis(200));
    let started = std::time::Instant::now();
    send_cancel(addr, pid, secret);
    let msgs = c.read_until_ready();
    let err = msgs.iter().find(|m| m.tag == b'E').expect("ErrorResponse");
    assert_eq!(err.fields()[&b'C'], "57014");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The session is still usable and the next statement is not cancelled.
    c.query("SELECT 1");
    let msgs = c.read_until_ready();
    assert!(msgs.iter().all(|m| m.tag != b'E'), "{msgs:?}");
}

#[test]
fn cancel_request_with_a_wrong_key_is_ignored() {
    let addr = start_server(10);
    let (mut c, pid, secret) = handshake_with_key(addr);
    c.query("SELECT pg_sleep('0.6'::float8)");
    std::thread::sleep(Duration::from_millis(100));
    send_cancel(addr, pid, secret.wrapping_add(1));
    send_cancel(addr, pid.wrapping_add(1000), secret);
    let msgs = c.read_until_ready();
    assert!(msgs.iter().all(|m| m.tag != b'E'), "{msgs:?}");
}

#[test]
fn cancel_while_idle_is_discarded() {
    let addr = start_server(10);
    let (mut c, pid, secret) = handshake_with_key(addr);
    send_cancel(addr, pid, secret);
    std::thread::sleep(Duration::from_millis(100));
    c.query("SELECT 1");
    let msgs = c.read_until_ready();
    assert!(msgs.iter().all(|m| m.tag != b'E'), "{msgs:?}");
}

#[test]
fn backend_key_pid_is_the_session_pid() {
    let addr = start_server(10);
    let (mut c, pid, _) = handshake_with_key(addr);
    c.query("SELECT pg_backend_pid()");
    let msgs = c.read_until_ready();
    let row = msgs.iter().find(|m| m.tag == b'D').expect("DataRow");
    // Int16 field count, Int32 length, then the text value.
    assert_eq!(String::from_utf8_lossy(&row.body[6..]), pid.to_string());
}

#[test]
fn idle_session_timeout_terminates_with_57p05() {
    let addr = start_server(10);
    let (mut c, _, _) = handshake_with_key(addr);
    c.query("SET idle_session_timeout = 200");
    c.read_until_ready();
    let fatal = c.read_msg();
    assert_eq!(fatal.tag, b'E', "{fatal:?}");
    assert_eq!(fatal.fields()[&b'S'], "FATAL");
    assert_eq!(fatal.fields()[&b'C'], "57P05");
    c.expect_eof();
}

#[test]
fn idle_in_transaction_timeout_terminates_with_25p03_and_aborts() {
    let addr = start_server(10);
    let (mut c, _, _) = handshake_with_key(addr);
    c.query("SET idle_in_transaction_session_timeout = 200");
    c.read_until_ready();
    // Idle outside a transaction is not limited by this setting.
    std::thread::sleep(Duration::from_millis(400));
    c.query("BEGIN");
    let msgs = c.read_until_ready();
    assert_eq!(msgs.last().unwrap().body, b"T");
    let fatal = c.read_msg();
    assert_eq!(fatal.tag, b'E', "{fatal:?}");
    assert_eq!(fatal.fields()[&b'C'], "25P03");
    c.expect_eof();
}

#[test]
fn idle_in_transaction_timeout_covers_a_partial_message() {
    let addr = start_server(10);
    let (mut c, _, _) = handshake_with_key(addr);
    c.query("SET idle_in_transaction_session_timeout = 300");
    c.read_until_ready();
    c.query("BEGIN");
    c.read_until_ready();
    // Only the first byte of the next message arrives, then silence.
    c.send(b"Q");
    let fatal = c.read_msg();
    assert_eq!(fatal.tag, b'E', "{fatal:?}");
    assert_eq!(fatal.fields()[&b'C'], "25P03");
    c.expect_eof();
}

#[test]
fn a_running_statement_is_not_an_idle_timeout() {
    let addr = start_server(10);
    let (mut c, _, _) = handshake_with_key(addr);
    c.query("SET idle_session_timeout = 300");
    c.read_until_ready();
    // Activity within the limit keeps the session alive.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(100));
        c.query("SELECT 1");
        let msgs = c.read_until_ready();
        assert!(msgs.iter().all(|m| m.tag != b'E'), "{msgs:?}");
    }
}

#[test]
fn unsupported_protocol_version_is_rejected() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup_code(2 << 16, b"user\0postgres\0\0");
    let err = c.read_msg();
    assert_eq!(err.tag, b'E');
    assert_eq!(err.fields()[&b'S'], "FATAL");
    assert_eq!(err.fields()[&b'C'], "0A000");
    c.expect_eof();
}

#[test]
fn newer_minor_version_is_negotiated_down() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup_code(
        196_610,
        b"user\0postgres\0database\0postgres\0_pq_.ext\0x\0\0",
    );
    let v = c.read_msg();
    assert_eq!(v.tag, b'v');
    let mut expected = 0i32.to_be_bytes().to_vec();
    expected.extend_from_slice(&1i32.to_be_bytes());
    expected.extend_from_slice(b"_pq_.ext\0");
    assert_eq!(v.body, expected);
    check_startup_response(&c.read_until_ready());
}

#[test]
fn missing_user_is_rejected() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup(&[("database", "postgres")]);
    let err = c.read_msg();
    assert_eq!(err.fields()[&b'S'], "FATAL");
    assert_eq!(err.fields()[&b'C'], "28000");
    c.expect_eof();
}

#[test]
fn unknown_database_is_rejected() {
    let addr = start_server(10);
    let mut c = Raw::connect(addr);
    c.send_startup(&[("user", "postgres"), ("database", "no_such_db")]);
    let err = c.read_msg();
    assert_eq!(err.tag, b'E');
    assert_eq!(err.fields()[&b'S'], "FATAL");
    assert_eq!(err.fields()[&b'C'], "3D000");
    c.expect_eof();
}

#[test]
fn too_many_connections() {
    let addr = start_server(1);
    let (first, _) = Raw::handshake(addr);
    let mut second = Raw::connect(addr);
    second.send_startup(&[("user", "postgres"), ("database", "postgres")]);
    let err = second.read_msg();
    assert_eq!(err.tag, b'E');
    assert_eq!(err.fields()[&b'S'], "FATAL");
    assert_eq!(err.fields()[&b'C'], "53300");
    assert_eq!(err.fields()[&b'M'], "sorry, too many clients already");
    second.expect_eof();
    drop(first);

    // The slot is released once the first connection's thread finishes.
    for _ in 0..100 {
        let mut c = Raw::connect(addr);
        c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
        if c.read_msg().tag == b'R' {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("connection slot was never released");
}

fn start_server_with(max_connections: usize, f: impl FnOnce(Server) -> Server) -> SocketAddr {
    let config = Config {
        listen: [127, 0, 0, 1].into(),
        port: 0,
        max_connections,
        ..Config::default()
    };
    let cluster = TestCluster::new().cluster;
    let server = f(Server::with_cluster(&config, cluster).expect("bind"));
    let addr = server.local_addr().expect("local_addr");
    std::thread::spawn(move || server.run());
    addr
}

/// Sockets that connect and send nothing must not consume session slots.
#[test]
fn idle_sockets_in_startup_do_not_lock_out_clients() {
    let addr = start_server(3);
    let idle: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(100));
    let (mut c, _) = Raw::handshake(addr);
    c.query("SELECT 1");
    let _ = c.read_until_ready();
    drop(idle);
}

/// A client that sends nothing during startup is disconnected after the
/// authentication timeout.
#[test]
fn startup_times_out() {
    let addr = start_server_with(3, |s| {
        s.with_authentication_timeout(Duration::from_millis(200))
    });
    let mut idle = Raw::connect(addr);
    idle.expect_eof();
    // A normal client is unaffected by the (cleared) timeout after startup.
    let (mut c, _) = Raw::handshake(addr);
    std::thread::sleep(Duration::from_millis(400));
    c.query("SELECT 1");
    let msgs = c.read_until_ready();
    assert_eq!(msgs.last().unwrap().tag, b'Z');
}

/// Beyond the thread ceiling, sockets are rejected immediately with 53300
/// from the accept loop; the slot is reusable once the idle sockets go away.
#[test]
fn thread_ceiling_rejects_immediately() {
    let addr = start_server_with(1, |s| s.with_max_threads(2));
    let idle: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(100));
    let mut rejected = Raw::connect(addr);
    let err = rejected.read_msg();
    assert_eq!(err.tag, b'E');
    assert_eq!(err.fields()[&b'S'], "FATAL");
    assert_eq!(err.fields()[&b'C'], "53300");
    rejected.expect_eof();
    drop(idle);
    for _ in 0..100 {
        let mut c = Raw::connect(addr);
        c.send_startup(&[("user", "postgres"), ("database", "postgres")]);
        if c.read_msg().tag == b'R' {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("thread slot was never released");
}
