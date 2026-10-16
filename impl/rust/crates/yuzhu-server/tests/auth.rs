//! Connection-level authentication tests (`m5/08` §7.3): hba matching, trust / password /
//! SCRAM, protocol errors, the startup deadline and initdb's password file.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};
use yuzhu_auth::scram_client::ClientExchange;
use yuzhu_core::bootstrap::{InitAuth, InitdbOptions, default_hba, initdb};
use yuzhu_core::sql::ast::Secret;
use yuzhu_core::storage::vfs::LocalVfs;
use yuzhu_core::util::clock::ManualClock;
use yuzhu_server::config::Config;
use yuzhu_server::hba::HbaRules;
use yuzhu_server::{Outcome, Server, ShutdownHandle, ShutdownMode};

const ADMIN_PASSWORD: &str = "secret";
/// Hba used by most tests: the superuser by SCRAM, everybody else by SCRAM too.
const SCRAM_ALL: &str =
    "host all all 127.0.0.1/32 scram-sha-256\nhost all all ::1/128 scram-sha-256\n";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    dir: PathBuf,
    addr: SocketAddr,
    handle: ShutdownHandle,
    join: Option<JoinHandle<std::io::Result<Outcome>>>,
}

fn temp_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("yz-au2-{}-{n}", std::process::id()))
}

fn init(dir: &Path, auth: InitAuth, password: Option<&str>) -> yuzhu_core::Result<()> {
    let vfs = Arc::new(LocalVfs::new(dir.to_path_buf()));
    initdb(
        vfs,
        &InitdbOptions {
            no_sync: true,
            auth_host: auth,
            superuser_password: password.map(|p| Secret(p.to_owned())),
            ..InitdbOptions::default()
        },
    )
}

impl Fixture {
    fn start(hba: &str) -> Self {
        Self::start_with(hba, |s| s)
    }

    fn start_with(hba: &str, tweak: impl FnOnce(Server) -> Server) -> Self {
        let dir = temp_dir();
        init(&dir, InitAuth::ScramSha256, Some(ADMIN_PASSWORD)).unwrap();
        let config = Config {
            data_directory: dir.clone(),
            port: 0,
            ..Config::default()
        };
        let server = Server::bind(&config)
            .unwrap()
            .with_hba(HbaRules::parse(hba).unwrap());
        Self::run(dir, tweak(server))
    }

    fn run(dir: PathBuf, server: Server) -> Self {
        let addr = server.local_addr().unwrap();
        let handle = server.shutdown_handle();
        let join = Some(std::thread::spawn(move || server.run()));
        Fixture {
            dir,
            addr,
            handle,
            join,
        }
    }

    fn admin(&self) -> Client {
        connect(self.addr, "postgres", "postgres", Some(ADMIN_PASSWORD)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.request(ShutdownMode::Fast);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn connect(
    addr: SocketAddr,
    user: &str,
    db: &str,
    password: Option<&str>,
) -> Result<Client, postgres::Error> {
    let mut c = postgres::Config::new();
    c.host(&addr.ip().to_string())
        .port(addr.port())
        .user(user)
        .dbname(db)
        .connect_timeout(Duration::from_secs(10));
    if let Some(p) = password {
        c.password(p);
    }
    c.connect(NoTls)
}

/// (SQLSTATE, message) of a failed connection.
fn db_error(r: Result<Client, postgres::Error>) -> (String, String) {
    let e = r.err().expect("the connection should fail");
    let d = e
        .as_db_error()
        .unwrap_or_else(|| panic!("not a database error: {e}"));
    (d.code().code().to_owned(), d.message().to_owned())
}

/// Minimal protocol client for raw messages.
struct Raw {
    s: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Raw { s }
    }

    fn startup(&mut self, user: &str, db: Option<&str>) {
        let mut body = 196_608_u32.to_be_bytes().to_vec();
        let mut params = vec![("user", user)];
        if let Some(d) = db {
            params.push(("database", d));
        }
        for (k, v) in params {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let len = u32::try_from(body.len() + 4).unwrap();
        self.s.write_all(&len.to_be_bytes()).unwrap();
        self.s.write_all(&body).unwrap();
    }

    fn send_p(&mut self, body: &[u8]) {
        self.send_typed(b'p', body);
    }

    fn send_typed(&mut self, tag: u8, body: &[u8]) {
        let mut v = vec![tag];
        v.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        self.s.write_all(&v).unwrap();
    }

    /// One message; `None` at EOF (or reset).
    fn read_msg(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut tag = [0u8; 1];
        if self.s.read(&mut tag).ok()? == 0 {
            return None;
        }
        let mut len = [0u8; 4];
        self.s.read_exact(&mut len).ok()?;
        let n = u32::from_be_bytes(len) as usize - 4;
        let mut body = vec![0u8; n];
        self.s.read_exact(&mut body).ok()?;
        Some((tag[0], body))
    }

    fn expect_eof(&mut self) {
        assert!(
            self.read_msg().is_none(),
            "expected the connection to be closed"
        );
    }

    /// Reads an `R` message and returns its code and payload.
    fn read_auth(&mut self) -> (i32, Vec<u8>) {
        let (tag, body) = self.read_msg().expect("an authentication message");
        assert_eq!(tag, b'R', "expected R, got {}", tag as char);
        (
            i32::from_be_bytes(body[..4].try_into().unwrap()),
            body[4..].to_vec(),
        )
    }

    /// Reads an `E` message as (SQLSTATE, message).
    fn read_error(&mut self) -> (String, String) {
        let (tag, body) = self.read_msg().expect("an error message");
        assert_eq!(tag, b'E', "expected E, got {}", tag as char);
        let f = error_fields(&body);
        (f[&b'C'].clone(), f[&b'M'].clone())
    }

    fn sasl_initial(&mut self, mechanism: &str, client_first: &[u8]) {
        let mut body = mechanism.as_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(&i32::try_from(client_first.len()).unwrap().to_be_bytes());
        body.extend_from_slice(client_first);
        self.send_p(&body);
    }
}

fn error_fields(body: &[u8]) -> HashMap<u8, String> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let code = body[i];
        let end = body[i + 1..].iter().position(|b| *b == 0).unwrap() + i + 1;
        out.insert(
            code,
            String::from_utf8_lossy(&body[i + 1..end]).into_owned(),
        );
        i = end + 1;
    }
    out
}

/// Starts a SCRAM exchange and returns the server-first-message.
fn scram_to_server_first(raw: &mut Raw, user: &str, client_first: &[u8]) -> Vec<u8> {
    raw.startup(user, Some("postgres"));
    let (code, payload) = raw.read_auth();
    assert_eq!(code, 10);
    assert_eq!(payload, b"SCRAM-SHA-256\0\0");
    raw.sasl_initial("SCRAM-SHA-256", client_first);
    let (code, server_first) = raw.read_auth();
    assert_eq!(code, 11);
    server_first
}

/// Runs a whole SCRAM login by hand. `Ok` on `AuthenticationOk`.
fn raw_scram_login(addr: SocketAddr, user: &str, password: &str) -> Result<(), (String, String)> {
    let mut raw = Raw::connect(addr);
    let mut client = ClientExchange::new(user, password).unwrap();
    let server_first = scram_to_server_first(&mut raw, user, &client.client_first());
    let fin = client.client_final(&server_first).unwrap();
    raw.send_p(&fin);
    match raw.read_msg().expect("a reply") {
        (b'R', body) => {
            assert_eq!(i32::from_be_bytes(body[..4].try_into().unwrap()), 12);
            client.verify_server_final(&body[4..]).unwrap();
            let (code, _) = raw.read_auth();
            assert_eq!(code, 0);
            Ok(())
        }
        (b'E', body) => {
            let f = error_fields(&body);
            Err((f[&b'C'].clone(), f[&b'M'].clone()))
        }
        (t, _) => panic!("unexpected message {}", t as char),
    }
}

fn salt_and_iterations(server_first: &[u8]) -> (String, String) {
    let text = String::from_utf8(server_first.to_vec()).unwrap();
    let mut salt = String::new();
    let mut iters = String::new();
    for part in text.split(',') {
        if let Some(v) = part.strip_prefix("s=") {
            v.clone_into(&mut salt);
        } else if let Some(v) = part.strip_prefix("i=") {
            v.clone_into(&mut iters);
        }
    }
    (salt, iters)
}

fn failed(user: &str) -> (String, String) {
    (
        "28P01".to_owned(),
        format!("password authentication failed for user \"{user}\""),
    )
}

/// 先頭の行・列をテキストで返す（Simple Query。バイナリ結果形式に依らない）。
fn text_value(c: &mut Client, sql: &str) -> String {
    c.simple_query(sql)
        .unwrap()
        .into_iter()
        .find_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_owned),
            _ => None,
        })
        .expect("a row")
}

#[test]
fn trust_connects() {
    let f = Fixture::start("host all all 127.0.0.1/32 trust\n");
    let mut c = connect(f.addr, "postgres", "postgres", None).unwrap();
    assert_eq!(text_value(&mut c, "SELECT current_user"), "postgres");
    // A role that does not exist is rejected after AuthenticationOk.
    let mut raw = Raw::connect(f.addr);
    raw.startup("ghost", Some("postgres"));
    let (code, _) = raw.read_auth();
    assert_eq!(code, 0);
    assert_eq!(
        raw.read_error(),
        (
            "28000".to_owned(),
            "role \"ghost\" does not exist".to_owned()
        )
    );
    raw.expect_eof();
}

#[test]
fn scram_success_for_the_superuser() {
    let f = Fixture::start(SCRAM_ALL);
    let mut c = f.admin();
    assert_eq!(text_value(&mut c, "SHOW is_superuser"), "on");
    assert_eq!(text_value(&mut c, "SELECT current_user"), "postgres");
}

#[test]
fn scram_wrong_password() {
    let f = Fixture::start(SCRAM_ALL);
    assert_eq!(
        db_error(connect(f.addr, "postgres", "postgres", Some("wrong"))),
        failed("postgres")
    );
    assert_eq!(
        raw_scram_login(f.addr, "postgres", "wrong").unwrap_err(),
        failed("postgres")
    );
    raw_scram_login(f.addr, "postgres", ADMIN_PASSWORD).unwrap();
}

#[test]
fn scram_no_such_role_looks_like_a_wrong_password() {
    let f = Fixture::start(SCRAM_ALL);
    f.admin()
        .batch_execute("CREATE ROLE alice LOGIN PASSWORD 'pw'")
        .unwrap();
    // Same message for a wrong password and for a role that does not exist.
    let wrong = raw_scram_login(f.addr, "alice", "nope").unwrap_err();
    let ghost = raw_scram_login(f.addr, "ghost", "nope").unwrap_err();
    assert_eq!(wrong.0, ghost.0);
    assert_eq!(wrong.1.replace("alice", "X"), ghost.1.replace("ghost", "X"));
    // The salt is stable for a name, and differs from a real role's and between names.
    let first = |user: &str| {
        let mut raw = Raw::connect(f.addr);
        let cf = ClientExchange::new(user, "x").unwrap().client_first();
        salt_and_iterations(&scram_to_server_first(&mut raw, user, &cf))
    };
    let g1 = first("ghost");
    assert_eq!(g1, first("ghost"));
    assert_eq!(g1, first("ghost"));
    assert_ne!(g1.0, first("alice").0);
    assert_ne!(g1.0, first("phantom").0);
}

#[test]
fn scram_no_password_or_expired() {
    let clock = ManualClock::new(1_000_000_000_000_000); // 2001-09-09
    let f = Fixture::start_with(SCRAM_ALL, |s| s.with_clock(clock.clone()));
    let mut admin = f.admin();
    admin
        .batch_execute(
            "CREATE ROLE nopw LOGIN;
             CREATE ROLE old LOGIN PASSWORD 'pw' VALID UNTIL '2005-01-01';
             CREATE ROLE forever LOGIN PASSWORD 'pw' VALID UNTIL 'infinity';",
        )
        .unwrap();
    assert_eq!(
        db_error(connect(f.addr, "nopw", "postgres", Some("pw"))),
        failed("nopw")
    );
    // The clock is before the deadline: still valid.
    connect(f.addr, "old", "postgres", Some("pw")).unwrap();
    clock.set(1_200_000_000_000_000); // 2008
    assert_eq!(
        db_error(connect(f.addr, "old", "postgres", Some("pw"))),
        failed("old")
    );
    connect(f.addr, "forever", "postgres", Some("pw")).unwrap();
}

#[test]
fn scram_nologin_role() {
    let f = Fixture::start(SCRAM_ALL);
    f.admin()
        .batch_execute("CREATE ROLE nologin PASSWORD 'pw' NOLOGIN")
        .unwrap();
    // The password is checked first.
    assert_eq!(
        db_error(connect(f.addr, "nologin", "postgres", Some("bad"))),
        failed("nologin")
    );
    assert_eq!(
        db_error(connect(f.addr, "nologin", "postgres", Some("pw"))),
        (
            "28000".to_owned(),
            "role \"nologin\" is not permitted to log in".to_owned()
        )
    );
}

#[test]
fn password_method() {
    let f = Fixture::start(
        "host all postgres 127.0.0.1/32 scram-sha-256\nhost all all 127.0.0.1/32 password\n",
    );
    f.admin()
        .batch_execute("CREATE ROLE clear LOGIN PASSWORD 'pw'")
        .unwrap();
    connect(f.addr, "clear", "postgres", Some("pw")).unwrap();
    assert_eq!(
        db_error(connect(f.addr, "clear", "postgres", Some("bad"))),
        failed("clear")
    );
    assert_eq!(
        db_error(connect(f.addr, "ghost", "postgres", Some("pw"))),
        failed("ghost")
    );

    let ask = |body: &[u8]| {
        let mut raw = Raw::connect(f.addr);
        raw.startup("clear", Some("postgres"));
        assert_eq!(raw.read_auth().0, 3);
        raw.send_p(body);
        let e = raw.read_error();
        raw.expect_eof();
        e
    };
    assert_eq!(
        ask(b"\0"),
        (
            "28P01".to_owned(),
            "empty password returned by client".to_owned()
        )
    );
    assert_eq!(
        ask(b"no-terminator"),
        (
            "08P01".to_owned(),
            "invalid password packet size".to_owned()
        )
    );
    assert_eq!(
        ask(b"a\0b\0"),
        (
            "08P01".to_owned(),
            "invalid password packet size".to_owned()
        )
    );
}

#[test]
fn reject_and_no_match() {
    let f = Fixture::start(
        "host all postgres 127.0.0.1/32 scram-sha-256\nhost all banned 127.0.0.1/32 reject\nhost all banned 127.0.0.1/32 trust\nhost app all 127.0.0.1/32 trust\n",
    );
    f.admin().batch_execute("CREATE ROLE banned LOGIN").unwrap();
    assert_eq!(
        db_error(connect(f.addr, "banned", "postgres", None)),
        (
            "28000".to_owned(),
            "pg_hba.conf rejects connection for host \"127.0.0.1\", user \"banned\", database \"postgres\", no encryption".to_owned()
        )
    );
    assert_eq!(
        db_error(connect(f.addr, "u", "d", None)),
        (
            "28000".to_owned(),
            "no pg_hba.conf entry for host \"127.0.0.1\", user \"u\", database \"d\", no encryption".to_owned()
        )
    );
}

#[test]
fn ipv6_rules_apply_to_ipv6_peers() {
    let dir = temp_dir();
    init(&dir, InitAuth::Trust, None).unwrap();
    let config = Config {
        data_directory: dir.clone(),
        port: 0,
        listen: "::1".parse().unwrap(),
        ..Config::default()
    };
    let Ok(server) = Server::bind(&config) else {
        // No IPv6 loopback in this environment.
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let server = server.with_hba(HbaRules::parse("host all all ::1/128 reject\n").unwrap());
    let f = Fixture::run(dir, server);
    let (code, msg) = db_error(connect(f.addr, "postgres", "postgres", None));
    assert_eq!(code, "28000");
    assert!(msg.contains("host \"::1\""), "{msg}");
}

#[test]
fn sameuser_and_default_database() {
    let f = Fixture::start(
        "host all postgres 127.0.0.1/32 scram-sha-256\nhost sameuser all 127.0.0.1/32 trust\n",
    );
    f.admin().batch_execute("CREATE ROLE bob LOGIN").unwrap();
    // Database omitted: it defaults to the user name, which matches `sameuser`
    // (the database itself does not exist, which is reported after authentication).
    let mut raw = Raw::connect(f.addr);
    raw.startup("bob", None);
    assert_eq!(raw.read_auth().0, 0);
    assert_eq!(raw.read_error().0, "3D000");
    assert_eq!(
        db_error(connect(f.addr, "bob", "postgres", None)).0,
        "28000"
    );
}

#[test]
fn scram_protocol_errors() {
    #![allow(clippy::many_single_char_names)] // プロトコルの記法に合わせた短い名前。
    let f = Fixture::start(SCRAM_ALL);
    let cf = |user: &str| ClientExchange::new(user, "x").unwrap().client_first();
    let start = |raw: &mut Raw| {
        raw.startup("postgres", Some("postgres"));
        assert_eq!(raw.read_auth().0, 10);
    };

    // Wrong mechanism.
    let mut raw = Raw::connect(f.addr);
    start(&mut raw);
    raw.sasl_initial("SCRAM-SHA-1", &cf("postgres"));
    assert_eq!(
        raw.read_error(),
        (
            "08P01".to_owned(),
            "client selected an invalid SASL authentication mechanism".to_owned()
        )
    );
    raw.expect_eof();

    // No initial response (length -1).
    let mut raw = Raw::connect(f.addr);
    start(&mut raw);
    let mut body = b"SCRAM-SHA-256\0".to_vec();
    body.extend_from_slice(&(-1i32).to_be_bytes());
    raw.send_p(&body);
    assert_eq!(raw.read_error().0, "08P01");
    raw.expect_eof();

    // Too long: only the header is sent; the server must not wait for the body.
    let mut raw = Raw::connect(f.addr);
    start(&mut raw);
    let mut header = vec![b'p'];
    header.extend_from_slice(&(4 + 1025u32).to_be_bytes());
    raw.s.write_all(&header).unwrap();
    assert_eq!(
        raw.read_error(),
        ("08P01".to_owned(), "invalid message length".to_owned())
    );
    raw.expect_eof();

    // Wrong message type.
    let mut raw = Raw::connect(f.addr);
    start(&mut raw);
    raw.send_typed(b'Q', b"SELECT 1\0");
    assert_eq!(
        raw.read_error(),
        (
            "08P01".to_owned(),
            "expected SASL response, got message type 81".to_owned()
        )
    );
    raw.expect_eof();

    // Channel binding requested.
    let mut raw = Raw::connect(f.addr);
    start(&mut raw);
    raw.sasl_initial("SCRAM-SHA-256", b"p=tls-server-end-point,,n=,r=abc");
    assert_eq!(raw.read_error().0, "08P01");
    raw.expect_eof();

    // Tampered client-final-messages.
    let tamper = |edit: &dyn Fn(&str) -> String| -> (String, String) {
        let mut raw = Raw::connect(f.addr);
        let mut client = ClientExchange::new("postgres", ADMIN_PASSWORD).unwrap();
        let sf = scram_to_server_first(&mut raw, "postgres", &client.client_first());
        let fin = String::from_utf8(client.client_final(&sf).unwrap()).unwrap();
        raw.send_p(edit(&fin).as_bytes());
        let e = raw.read_error();
        raw.expect_eof();
        e
    };
    // Nonce.
    let (c, m) = tamper(&|fin| {
        let i = fin.find(",p=").unwrap();
        let mut s = fin[..i].to_owned();
        s.push('X');
        format!("{s}{}", &fin[i..])
    });
    assert_eq!(
        (c.as_str(), m.as_str()),
        ("08P01", "invalid SCRAM response")
    );
    // Channel binding attribute.
    let (c, _) = tamper(&|fin| fin.replacen("c=biws", "c=eSws", 1));
    assert_eq!(c, "08P01");
    // Proof.
    let (c, m) = tamper(&|fin| {
        let i = fin.find(",p=").unwrap() + 3;
        let mut b = fin.as_bytes().to_vec();
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        String::from_utf8(b).unwrap()
    });
    assert_eq!((c, m), failed("postgres"));
}

#[test]
fn channel_binding_flag_y_succeeds() {
    let f = Fixture::start(SCRAM_ALL);
    let mut raw = Raw::connect(f.addr);
    // `y,,` と、それに合わせた `c=eSws`（client-final-without-proof は AuthMessage に含まれる）。
    let mut client = ClientExchange::new("postgres", ADMIN_PASSWORD)
        .unwrap()
        .with_binding_flag_y();
    let sf = scram_to_server_first(&mut raw, "postgres", &client.client_first());
    let fin = client.client_final(&sf).unwrap();
    raw.send_p(&fin);
    let (code, _) = raw.read_auth();
    assert_eq!(code, 12);
    assert_eq!(raw.read_auth().0, 0);
}

#[test]
fn auth_timeout() {
    let f = Fixture::start_with(SCRAM_ALL, |s| {
        s.with_authentication_timeout(Duration::from_millis(300))
    });

    // (1) Nothing sent: closed without a message.
    let mut raw = Raw::connect(f.addr);
    let t = Instant::now();
    raw.expect_eof();
    assert!(t.elapsed() < Duration::from_secs(5));

    // (2) Startup sent, no SASLInitialResponse: FATAL 57014.
    let mut raw = Raw::connect(f.addr);
    raw.startup("postgres", Some("postgres"));
    assert_eq!(raw.read_auth().0, 10);
    assert_eq!(
        raw.read_error(),
        (
            "57014".to_owned(),
            "canceling authentication due to timeout".to_owned()
        )
    );
    raw.expect_eof();

    // (3) One byte every 100 ms: closed near the total deadline.
    let mut raw = Raw::connect(f.addr);
    let t = Instant::now();
    let mut packet = 196_608_u32.to_be_bytes().to_vec();
    packet.extend_from_slice(b"user\0postgres\0\0");
    let mut all = u32::try_from(packet.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    all.extend_from_slice(&packet);
    for b in &all {
        if raw.s.write_all(&[*b]).is_err() || t.elapsed() > Duration::from_millis(800) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    raw.expect_eof();
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn backend_key_secrets_differ() {
    let f = Fixture::start("host all all 127.0.0.1/32 trust\n");
    let key = |f: &Fixture| {
        let mut raw = Raw::connect(f.addr);
        raw.startup("postgres", Some("postgres"));
        loop {
            let (tag, body) = raw.read_msg().unwrap();
            if tag == b'K' {
                return (
                    i32::from_be_bytes(body[..4].try_into().unwrap()),
                    i32::from_be_bytes(body[4..8].try_into().unwrap()),
                    raw,
                );
            }
        }
    };
    let (p1, s1, _a) = key(&f);
    let (p2, s2, _b) = key(&f);
    assert_ne!(p1, p2);
    assert_ne!(s1, s2);
}

#[test]
fn initdb_pwfile_password_works() {
    let f = Fixture::start(SCRAM_ALL);
    f.admin();
    // The hba file written by initdb is accepted by `Server::bind`.
    let dir = temp_dir();
    init(&dir, InitAuth::ScramSha256, Some("pw1")).unwrap();
    let config = Config {
        data_directory: dir.clone(),
        port: 0,
        ..Config::default()
    };
    let g = Fixture::run(dir, Server::bind(&config).unwrap());
    connect(g.addr, "postgres", "postgres", Some("pw1")).unwrap();
    assert_eq!(
        db_error(connect(g.addr, "postgres", "postgres", Some("pw2"))),
        failed("postgres")
    );
}

#[test]
fn initdb_requires_a_password_for_password_methods() {
    for auth in [InitAuth::ScramSha256, InitAuth::Password] {
        let dir = temp_dir();
        let e = init(&dir, auth, None).unwrap_err();
        assert_eq!(e.sqlstate.0, "22023");
        assert!(
            e.message
                .contains("must specify a password for the superuser")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn default_hba_loads_and_broken_files_are_refused() {
    let dir = temp_dir();
    init(&dir, InitAuth::Trust, None).unwrap();
    let text = std::fs::read_to_string(dir.join("yuzhu_hba.conf")).unwrap();
    assert_eq!(text, default_hba(InitAuth::Trust));
    let config = Config {
        data_directory: dir.clone(),
        port: 0,
        ..Config::default()
    };
    drop(Server::bind(&config).unwrap());
    // 正常終了せず drop しただけなので pid ファイルが残る。壊れたファイルの bind が作らないことを確かめるため消しておく。
    std::fs::remove_file(dir.join("yuzhu.pid")).unwrap();

    // A broken file: `bind` fails before the cluster is opened (no pid file is left).
    std::fs::write(dir.join("yuzhu_hba.conf"), "host all all all bogus\n").unwrap();
    let e = Server::bind(&config).unwrap_err();
    assert!(e.to_string().contains("yuzhu_hba.conf line 1"), "{e}");
    assert!(!dir.join("yuzhu.pid").exists());
    // A missing file.
    std::fs::remove_file(dir.join("yuzhu_hba.conf")).unwrap();
    let e = Server::bind(&config).unwrap_err();
    assert!(e.to_string().contains("could not open"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
