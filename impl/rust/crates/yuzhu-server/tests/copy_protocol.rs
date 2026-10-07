//! `COPY FROM STDIN` のプロトコル（`m4/10` §5.2、§5.9）を、実際の TCP の生のメッセージで確かめる。
//!
//! 状態機械の全辺（`G`、細切れの `d`、`c`、`f`、エラー後の `d` `c` の無視、CopyIn 中の `Q`、
//! `H` / `S` の無視、アイドル中の `d` `c` `f` の無視）と、ErrorResponse の `W` `s` `t` `c` `n`。

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
    /// ErrorResponse / NoticeResponse のフィールド。
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

    /// フィールドの符号 `S V C M ...` を出てきた順に並べる。
    fn field_order(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = self.body.as_slice();
        while let Some((&code, tail)) = rest.split_first() {
            if code == 0 {
                break;
            }
            out.push(code);
            let end = tail.iter().position(|&b| b == 0).unwrap();
            rest = &tail[end + 1..];
        }
        out
    }
}

struct Client {
    s: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = Self { s };
        let mut payload = Vec::new();
        for (k, v) in [("user", "postgres"), ("database", "postgres")] {
            payload.extend_from_slice(k.as_bytes());
            payload.push(0);
            payload.extend_from_slice(v.as_bytes());
            payload.push(0);
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

    fn send_msg(&mut self, tag: u8, body: &[u8]) {
        let mut v = vec![tag];
        v.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        self.s.write_all(&v).unwrap();
    }

    fn query(&mut self, sql: &str) {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send_msg(b'Q', &body);
    }

    fn data(&mut self, bytes: &[u8]) {
        self.send_msg(b'd', bytes);
    }

    fn done(&mut self) {
        self.send_msg(b'c', b"");
    }

    fn fail(&mut self, message: &str) {
        let mut body = message.as_bytes().to_vec();
        body.push(0);
        self.send_msg(b'f', &body);
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

    /// `Q` を送って `Z` までの応答を返す。
    fn simple(&mut self, sql: &str) -> Vec<Msg> {
        self.query(sql);
        self.read_until_ready()
    }

    fn expect_eof(&mut self) {
        let mut buf = [0u8; 16];
        let n = self.s.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "expected the server to close the connection");
    }

    /// 列 1 つの SELECT の結果（テキスト）。
    fn column(&mut self, sql: &str) -> Vec<String> {
        let msgs = self.simple(sql);
        msgs.iter()
            .filter(|m| m.tag == b'D')
            .map(|m| {
                // Int16 列数、Int32 長さ、値（1 列目だけ）。
                let n = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
                if n < 0 {
                    "NULL".to_owned()
                } else {
                    String::from_utf8(m.body[6..6 + usize::try_from(n).unwrap()].to_vec()).unwrap()
                }
            })
            .collect()
    }
}

fn tags(msgs: &[Msg]) -> String {
    msgs.iter().map(|m| m.tag as char).collect()
}

fn cstr(m: &Msg) -> String {
    String::from_utf8(m.body[..m.body.len() - 1].to_vec()).unwrap()
}

fn setup(c: &mut Client) {
    let msgs = c.simple("CREATE TABLE t (a int, b text)");
    assert_eq!(tags(&msgs), "CZ", "{msgs:?}");
}

/// `G`: 全体の形式 0、列数 2、各列の形式 0。長さは 4 + 1 + 2 + 2n。
#[test]
fn copy_in_response_bytes_and_chunked_data() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    let g = c.read_msg();
    assert_eq!(g.tag, b'G');
    assert_eq!(g.body, [0, 0, 2, 0, 0, 0, 0]);
    // 行の途中でも、`\t` の途中でも切れる。
    for chunk in [&b"1\tone"[..], b"\n2\t", b"two\n3", b"\tthree\n", b""] {
        c.data(chunk);
    }
    c.done();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "CZ", "{msgs:?}");
    assert_eq!(cstr(&msgs[0]), "COPY 3");
    assert_eq!(msgs[1].body, b"I");
    assert_eq!(c.column("SELECT a FROM t ORDER BY a"), ["1", "2", "3"]);
}

/// 1 つの `Query` の続きの文は、CopyDone の後に実行する。
#[test]
fn statements_after_copy_run_after_copy_done() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN; SELECT 'after'");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"1\tx\n");
    c.done();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "CTDCZ", "{msgs:?}");
    assert_eq!(cstr(&msgs[0]), "COPY 1");
    assert_eq!(cstr(&msgs[3]), "SELECT 1");
}

/// `COPY a FROM STDIN; COPY b FROM STDIN`: 2 つ目の `G` が `COPY 1` の直後に来て、`Z` は最後だけ。
#[test]
fn two_copies_in_one_query_run_in_turn() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    assert_eq!(tags(&c.simple("CREATE TABLE u (a int)")), "CZ");
    c.query("COPY t FROM STDIN; COPY u FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"1\tx\n");
    c.done();
    let m = c.read_msg();
    assert_eq!((m.tag, cstr(&m).as_str()), (b'C', "COPY 1"));
    let g = c.read_msg();
    assert_eq!(
        (g.tag, g.body.as_slice()),
        (b'G', [0u8, 0, 1, 0, 0].as_slice())
    );
    c.data(b"7\n8\n");
    c.done();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "CZ", "{msgs:?}");
    assert_eq!(cstr(&msgs[0]), "COPY 2");
    assert_eq!(c.column("SELECT a FROM u ORDER BY a"), ["7", "8"]);
}

/// エラーの後の `d` `d` `c` は無視され、`E` 1 つと `Z` 1 つだけが返る。
#[test]
fn data_after_an_error_is_ignored_and_ready_for_query_is_sent_once() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"notanumber\tx\n");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    let f = msgs[0].fields();
    assert_eq!(f[&b'C'], "22P02");
    assert!(f[&b'W'].starts_with("COPY t, line 1"), "{f:?}");
    // 後続のメッセージは、アイドル状態で無視される。
    c.data(b"1\tx\n");
    c.data(b"2\ty\n");
    c.done();
    c.fail("late");
    // 次の問い合わせの応答に `Z` が余計に混ざらない。
    let msgs = c.simple("SELECT 1");
    assert_eq!(tags(&msgs), "TDCZ", "{msgs:?}");
    assert!(c.column("SELECT a FROM t").is_empty());
}

#[test]
fn copy_fail_is_57014_with_the_client_message() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"1\tx\n");
    c.fail("boom");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    let f = msgs[0].fields();
    assert_eq!(f[&b'C'], "57014");
    assert_eq!(f[&b'M'], "COPY from stdin failed: boom");
    assert_eq!(f[&b'W'], "COPY t, line 2");
    assert!(c.column("SELECT a FROM t").is_empty());
}

/// CopyIn 中の `Q` は `08P01` の ERROR + FATAL で、接続を閉じる。
#[test]
fn query_during_copy_is_a_protocol_violation() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.query("SELECT 1");
    let e = c.read_msg();
    assert_eq!(e.tag, b'E');
    let f = e.fields();
    assert_eq!(f[&b'S'], "ERROR");
    assert_eq!(f[&b'C'], "08P01");
    assert_eq!(
        f[&b'M'],
        "unexpected message type 0x51 during COPY from stdin"
    );
    let e = c.read_msg();
    assert_eq!(e.tag, b'E');
    let f = e.fields();
    assert_eq!(f[&b'S'], "FATAL");
    assert_eq!(f[&b'C'], "08P01");
    assert_eq!(
        f[&b'M'],
        "terminating connection because protocol synchronization was lost"
    );
    c.expect_eof();
}

/// 拡張クエリのメッセージ（`P`）も、CopyIn 中は同じ扱い。
#[test]
fn extended_message_during_copy_is_a_protocol_violation() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.send_msg(b'P', b"\0SELECT 1\0\0\0");
    let e = c.read_msg();
    assert_eq!(
        e.fields()[&b'M'],
        "unexpected message type 0x50 during COPY from stdin"
    );
    assert_eq!(c.read_msg().fields()[&b'S'], "FATAL");
    c.expect_eof();
}

/// `H`（Flush）と `S`（Sync）は CopyIn 中は無視される: `c` の応答の先頭が `C` になる。
#[test]
fn flush_and_sync_are_ignored_during_copy() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.send_msg(b'H', b"");
    c.send_msg(b'S', b"");
    c.data(b"1\tx\n");
    c.send_msg(b'S', b"");
    c.done();
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "CZ", "{msgs:?}");
    assert_eq!(cstr(&msgs[0]), "COPY 1");
}

/// アイドル中の `d` `c` `f` は無視される（応答なし）。
#[test]
fn copy_messages_while_idle_are_ignored() {
    let mut c = Client::connect(start_server());
    c.data(b"junk");
    c.done();
    c.fail("nothing");
    let msgs = c.simple("SELECT 1");
    assert_eq!(tags(&msgs), "TDCZ", "{msgs:?}");
}

/// ブロックの中の COPY: エラーでブロックは失敗状態（`Z` は `E`）になる。
#[test]
fn copy_error_inside_a_block_leaves_a_failed_transaction() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    assert_eq!(tags(&c.simple("BEGIN")), "CZ");
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"bad\tx\n");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[1].body, b"E");
    let msgs = c.simple("ROLLBACK");
    assert_eq!(msgs.last().unwrap().body, b"I");
}

/// COPY の開始前の失敗は `G` を送らず、通常の ErrorResponse + `Z`。
#[test]
fn copy_to_a_missing_table_sends_no_copy_in_response() {
    let mut c = Client::connect(start_server());
    let msgs = c.simple("COPY nosuch FROM STDIN");
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].fields()[&b'C'], "42P01");
}

/// データが来ない間も `statement_timeout` が効く（`copy_poll`）。その後の `Z` は 1 つ。
#[test]
fn statement_timeout_ends_a_silent_copy() {
    let mut c = Client::connect(start_server());
    setup(&mut c);
    assert_eq!(tags(&c.simple("SET statement_timeout = 300")), "CZ");
    c.query("COPY t FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    assert_eq!(msgs[0].fields()[&b'C'], "57014");
    assert_eq!(
        msgs[0].fields()[&b'M'],
        "canceling statement due to statement timeout"
    );
    c.data(b"1\tx\n");
    c.done();
    assert_eq!(tags(&c.simple("SELECT 1")), "TDCZ");
}

/// 制約違反の `ErrorResponse` は `W`（CONTEXT）`s` `t` `c` `n` を、PostgreSQL の順に持つ。
#[test]
fn constraint_errors_carry_table_column_and_constraint_fields() {
    let mut c = Client::connect(start_server());
    assert_eq!(
        tags(&c.simple("CREATE TABLE n (a int NOT NULL, b int CONSTRAINT b_pos CHECK (b > 0))")),
        "CZ"
    );
    c.query("COPY n FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"\\N\t1\n");
    let msgs = c.read_until_ready();
    assert_eq!(tags(&msgs), "EZ", "{msgs:?}");
    let f = msgs[0].fields();
    assert_eq!(f[&b'C'], "23502");
    assert_eq!(f[&b's'], "public");
    assert_eq!(f[&b't'], "n");
    assert_eq!(f[&b'c'], "a");
    assert!(f[&b'W'].starts_with("COPY n, line 1"), "{f:?}");
    assert_eq!(
        msgs[0].field_order(),
        b"SVCMDWstc"
            .to_vec()
            .into_iter()
            .filter(|b| f.contains_key(b))
            .collect::<Vec<_>>()
    );

    c.query("COPY n FROM STDIN");
    assert_eq!(c.read_msg().tag, b'G');
    c.data(b"1\t0\n");
    let msgs = c.read_until_ready();
    let f = msgs[0].fields();
    assert_eq!(f[&b'C'], "23514");
    assert_eq!(f[&b't'], "n");
    assert_eq!(f[&b'n'], "b_pos");
    assert!(f[&b'W'].starts_with("COPY n, line 1"), "{f:?}");
}
