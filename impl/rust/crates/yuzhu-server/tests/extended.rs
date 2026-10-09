//! Extended Query のメッセージループ（`m5/04` §5.1・§5.2、XQ-1）を、実際の TCP の生のメッセージで確かめる。
//!
//! Session 側の `msg_*` は XQ-2a が埋めるまで `0A000` を返すスタブなので、ここで確かめるのは
//! サーバ側の規則（SkipTillSync、flush、F / d / c / f、本体の不正、Sync の応答の順序）。
//! 実行結果に依存するテストは `#[ignore]` にして、XQ-2a の後に外す。

#![allow(clippy::doc_markdown)] // protocol message names in docs

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use yuzhu_core::testing::TestCluster;
use yuzhu_server::Server;
use yuzhu_server::config::Config;

fn start_server() -> SocketAddr {
    let config = Config {
        listen: [127, 0, 0, 1].into(),
        port: 0,
        ..Config::default()
    };
    let cluster = TestCluster::new().cluster;
    let server = Server::with_cluster(&config, cluster).expect("bind");
    let addr = server.local_addr().expect("local_addr");
    std::thread::spawn(move || server.run());
    addr
}

#[derive(Debug)]
struct Msg {
    tag: u8,
    body: Vec<u8>,
}

impl Msg {
    /// ErrorResponse の `(コード, 値)`。
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

    fn sqlstate(&self) -> String {
        self.fields()[&b'C'].clone()
    }

    fn severity(&self) -> String {
        self.fields()[&b'S'].clone()
    }

    fn message(&self) -> String {
        self.fields()[&b'M'].clone()
    }
}

/// 生のメッセージを送る小さなクライアント。
struct Raw {
    s: TcpStream,
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = Self { s };
        let mut payload = Vec::new();
        for (k, v) in [("user", "postgres"), ("database", "postgres")] {
            payload.extend(cstr(k));
            payload.extend(cstr(v));
        }
        payload.push(0);
        let len = u32::try_from(8 + payload.len()).unwrap();
        let mut v = len.to_be_bytes().to_vec();
        v.extend_from_slice(&196_608u32.to_be_bytes());
        v.extend_from_slice(&payload);
        c.s.write_all(&v).unwrap();
        let _ = c.read_until_ready();
        c
    }

    fn send(&mut self, tag: u8, body: &[u8]) {
        let mut v = vec![tag];
        v.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        self.s.write_all(&v).unwrap();
    }

    fn query(&mut self, sql: &str) {
        self.send(b'Q', &cstr(sql));
    }

    fn parse(&mut self, name: &str, sql: &str) {
        let mut b = cstr(name);
        b.extend(cstr(sql));
        b.extend([0, 0]);
        self.send(b'P', &b);
    }

    fn sync(&mut self) {
        self.send(b'S', b"");
    }

    fn flush(&mut self) {
        self.send(b'H', b"");
    }

    fn read_msg(&mut self) -> Msg {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).unwrap();
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).unwrap();
        Msg { tag: h[0], body }
    }

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

    fn simple(&mut self, sql: &str) -> Vec<Msg> {
        self.query(sql);
        self.read_until_ready()
    }

    fn expect_eof(&mut self) {
        let mut buf = [0u8; 16];
        let n = self.s.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "expected the server to close the connection");
    }
}

fn tags(msgs: &[Msg]) -> String {
    msgs.iter().map(|m| m.tag as char).collect()
}

/// 本体の不正の ERROR が 1 つ届き、続く入力が Sync まで捨てられ、Sync で `I` に戻る。
fn assert_bad_body_recovers(c: &mut Raw, tag: u8, body: &[u8], state: &str, message: &str) {
    c.send(tag, body);
    // Sync までの間に送ったものは実行されない（読み捨て）。
    c.query("SELECT 1");
    c.flush();
    c.send(b'F', b"junk");
    c.send(b'd', b"data");
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].severity(), "ERROR");
    assert_eq!(msgs[0].sqlstate(), state);
    assert_eq!(msgs[0].message(), message);
    assert_eq!(msgs[1].body, b"I");
    // 復帰後は普通に動く。
    let msgs = c.simple("SELECT 1");
    assert_eq!(tags(&msgs), "TDCZ", "{msgs:?}");
}

/// E13: 不正な本体は ERROR（FATAL ではない）で、Sync で復帰する。
#[test]
fn e13_bad_bodies_are_errors_until_sync() {
    let mut c = Raw::connect(start_server());

    // 長さの足りない Bind（値の個数まで来ない）。
    let mut b = cstr("");
    b.extend(cstr(""));
    b.extend([0, 0]);
    assert_bad_body_recovers(
        &mut c,
        b'B',
        &b,
        "08P01",
        "insufficient data left in message",
    );

    // 値が残りのメッセージより長い。
    let mut b = cstr("");
    b.extend(cstr(""));
    b.extend([0, 0, 0, 1, 0, 0, 0, 9, b'x']);
    assert_bad_body_recovers(
        &mut c,
        b'B',
        &b,
        "08P01",
        "insufficient data left in message",
    );

    // D の対象が X。
    let mut b = vec![b'X'];
    b.extend(cstr("s"));
    assert_bad_body_recovers(
        &mut c,
        b'D',
        &b,
        "08P01",
        "invalid DESCRIBE message subtype 88",
    );
    assert_bad_body_recovers(
        &mut c,
        b'C',
        &b,
        "08P01",
        "invalid CLOSE message subtype 88",
    );

    // 本体の余り。
    let mut b = cstr("");
    b.extend([0, 0, 0, 0, 0]);
    assert_bad_body_recovers(&mut c, b'E', &b, "08P01", "invalid message format");

    // 終端のない文字列。
    assert_bad_body_recovers(&mut c, b'P', b"abc", "08P01", "invalid string in message");

    // UTF-8 でない問い合わせ。
    let mut b = cstr("");
    b.extend_from_slice(b"SELECT '\xff'\0");
    b.extend([0, 0]);
    assert_bad_body_recovers(
        &mut c,
        b'P',
        &b,
        "22021",
        "invalid byte sequence for encoding \"UTF8\": 0xff",
    );
}

/// E13: 未知の型バイトと長さの不正は FATAL 08P01 で接続を閉じる。
#[test]
fn e13_framing_errors_are_fatal() {
    let mut c = Raw::connect(start_server());
    c.send(b'z', b"abc");
    let m = c.read_msg();
    assert_eq!(m.tag, b'E');
    assert_eq!(m.severity(), "FATAL");
    assert_eq!(m.sqlstate(), "08P01");
    assert_eq!(m.message(), "invalid frontend message type 122");
    c.expect_eof();

    let mut c = Raw::connect(start_server());
    c.s.write_all(&[b'B', 0, 0, 0, 3]).unwrap();
    let m = c.read_msg();
    assert_eq!(
        (m.tag, m.severity(), m.sqlstate()),
        (b'E', "FATAL".into(), "08P01".into())
    );
    c.expect_eof();
}

/// 不正な本体の Q は ERROR と ReadyForQuery（SkipTillSync にならない）。
#[test]
fn bad_query_body_is_an_error_with_ready_for_query() {
    let mut c = Raw::connect(start_server());
    c.send(b'Q', b"SELECT '\xff'\0");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].sqlstate(), "22021");
    assert_eq!(tags(&c.simple("SELECT 1")), "TDCZ");
}

/// XQ-D13: ErrorResponse は Flush も Sync も待たずに届く。以降は Sync まで読み捨て。
#[test]
fn error_is_flushed_and_skips_until_sync() {
    let mut c = Raw::connect(start_server());
    // Session の msg_* が 0A000 を返す間は、Parse は常に失敗する。
    c.parse("", "SELECT 1");
    let m = c.read_msg();
    assert_eq!(m.tag, b'E');
    assert_eq!(m.sqlstate(), "0A000");
    // 読み捨てられるメッセージ: Q、F、P、H、不正な本体の B。
    c.query("SELECT 1");
    c.send(b'F', b"");
    c.parse("", "SELECT 2");
    c.flush();
    c.send(b'B', b"\xff");
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "Z", "{msgs:?}");
    assert_eq!(msgs[0].body, b"I");
    assert_eq!(tags(&c.simple("SELECT 1")), "TDCZ");
}

/// Terminate は読み捨てられず、接続を閉じる。
#[test]
fn terminate_during_skip_closes_the_connection() {
    let mut c = Raw::connect(start_server());
    c.parse("", "SELECT 1");
    assert_eq!(c.read_msg().tag, b'E');
    c.send(b'X', b"");
    c.expect_eof();
}

/// Sync だけなら ReadyForQuery だけ。Flush は何も返さない。
#[test]
fn sync_and_flush() {
    let mut c = Raw::connect(start_server());
    c.flush();
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "Z", "{msgs:?}");
    c.flush();
    c.flush();
    c.sync();
    assert_eq!(tags(&c.read_until_ready()), "Z");
}

/// Close は存在しない名前でも成功し、エラーにならない。
#[test]
fn close_of_a_missing_name_succeeds() {
    let mut c = Raw::connect(start_server());
    for kind in *b"SP" {
        let mut b = vec![kind];
        b.extend(cstr("nope"));
        c.send(b'C', &b);
    }
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "33Z", "{msgs:?}");
}

/// COPY の外の `d` `c` `f` は黙って無視する（XQ-D18）。
#[test]
fn copy_messages_outside_copy_are_ignored() {
    let mut c = Raw::connect(start_server());
    c.send(b'd', b"1\t2\n");
    c.send(b'c', b"");
    c.send(b'f', &cstr("nope"));
    assert_eq!(tags(&c.simple("SELECT 1")), "TDCZ");
}

/// `F`（FunctionCall）は 0A000 と ReadyForQuery を返す（Sync を待たない）。
#[test]
fn function_call_is_rejected_with_ready_for_query() {
    let mut c = Raw::connect(start_server());
    c.send(b'F', b"\0\0\0\0\0\0\0\0\0\0\0\0");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].sqlstate(), "0A000");
    assert_eq!(msgs[0].message(), "function call is not supported");
    assert_eq!(tags(&c.simple("SELECT 1")), "TDCZ");
}

/// Bind の形式コードは 0・1 以外なら 22023（Session を呼ぶ前）。
#[test]
fn unsupported_format_code_is_22023() {
    let mut c = Raw::connect(start_server());
    let mut b = cstr("");
    b.extend(cstr(""));
    b.extend([0, 1, 0, 2]); // パラメータの形式コード 2
    b.extend([0, 0]); // 値 0 個
    b.extend([0, 0]); // 結果の形式コード 0 個
    c.send(b'B', &b);
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].sqlstate(), "22023");
    assert_eq!(msgs[0].message(), "unsupported format code: 2");
}

/// E15 のスタブ時点の形: Execute が失敗しても、Sync の応答は ReadyForQuery だけで終わる。
#[test]
fn e15_sync_after_failed_execute_is_ready_for_query_only() {
    let mut c = Raw::connect(start_server());
    c.parse("", "SET application_name = 'x'");
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
}

/// E15: `SET application_name` / `SET TimeZone` を Execute し、Sync で ParameterStatus、ReadyForQuery の順。
/// XQ-2a（`msg_*` と `pending_parameter_status`）の後に外す。
#[test]
#[ignore = "needs XQ-2a: Session::msg_* and pending_parameter_status"]
fn e15_parameter_status_comes_before_ready_for_query() {
    let mut c = Raw::connect(start_server());
    for sql in ["SET application_name = 'xq'", "SET TimeZone = 'UTC'"] {
        c.parse("", sql);
        let mut b = cstr("");
        b.extend(cstr(""));
        b.extend([0, 0, 0, 0, 0, 0]);
        c.send(b'B', &b);
        let mut e = cstr("");
        e.extend([0, 0, 0, 0]);
        c.send(b'E', &e);
    }
    c.sync();
    let msgs = c.read_until_ready();
    assert_eq!(
        tags(&msgs),
        "1".to_owned() + "2C" + "12C" + "SSZ",
        "{msgs:?}"
    );
}
