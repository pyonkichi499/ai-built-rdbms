//! 認証の互換テスト用クライアント（tests/compat/auth/*.sh が使う）。
//!
//!   authc connect HOST PORT USER DB [--password PW] [--sql SQL] [--app NAME] [--bind LOCAL_ADDR]
//!   authc stall   HOST PORT silent|startup|drip TIMEOUT_MS [LOCAL_ADDR]
//!
//! connect の出力（標準出力、1 行目が結果）:
//!   `OK <trust|cleartext|md5|scram>`（以降 `ROW <列を TAB 区切り>`、`PARAM <名前>=<値>`）
//!   `ERR <SQLSTATE> <severity> <message>`（認証の途中でも、認証後の FATAL でも）
//!   `CLOSED <詳細>`（エラーメッセージなしで接続が閉じた）
//! stall の出力: `CLOSED <経過ミリ秒> [ERR <SQLSTATE> <severity> <message>]` または `ALIVE <経過ミリ秒>`。
#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256, SCRAM_SHA_256};
use postgres_protocol::message::backend::{ErrorFields, Message};
use postgres_protocol::message::frontend;
use socket2::{Domain, Socket, Type};

struct Conn {
    stream: TcpStream,
    buf: BytesMut,
}

impl Conn {
    /// bind を渡すと、そのローカルアドレスから接続する（127.0.0.2 などで hba のアドレス照合を試す）。
    fn open(host: &str, port: &str, bind: Option<&str>) -> std::io::Result<Self> {
        let remote: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(std::io::Error::other)?;
        let sock = Socket::new(Domain::for_address(remote), Type::STREAM, None)?;
        if let Some(b) = bind {
            let local: IpAddr = b.parse().map_err(std::io::Error::other)?;
            sock.bind(&SocketAddr::new(local, 0).into())?;
        }
        sock.connect(&remote.into())?;
        let stream: TcpStream = sock.into();
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            buf: BytesMut::new(),
        })
    }

    /// 次のメッセージを読む。接続が閉じたら Ok(None)。
    fn read_msg(&mut self, timeout: Duration) -> std::io::Result<Option<Message>> {
        loop {
            if let Some(m) = Message::parse(&mut self.buf)? {
                return Ok(Some(m));
            }
            self.stream.set_read_timeout(Some(timeout))?;
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                // サーバが FATAL を送って閉じたとき、RST でエラーになっても読めた分は先に処理済み。
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes)
    }
}

fn error_line(fields: ErrorFields<'_>) -> String {
    let (mut code, mut sev, mut msg) = (String::new(), String::new(), String::new());
    let mut it = fields;
    while let Ok(Some(f)) = it.next() {
        let v = String::from_utf8_lossy(f.value_bytes()).to_string();
        match f.type_() {
            b'C' => code = v,
            b'S' => sev = v,
            b'M' => msg = v,
            _ => {}
        }
    }
    format!("ERR {code} {sev} {msg}")
}

fn usage() -> ExitCode {
    eprintln!("usage: authc connect HOST PORT USER DB [--password PW] [--sql SQL] [--app NAME] [--bind ADDR]\n       authc stall HOST PORT silent|startup|drip TIMEOUT_MS [BIND_ADDR]");
    ExitCode::from(2)
}

fn startup_packet(user: &str, db: &str, app: &str) -> BytesMut {
    let mut b = BytesMut::new();
    frontend::startup_message(
        [
            ("user", user),
            ("database", db),
            ("application_name", app),
            ("client_encoding", "UTF8"),
        ],
        &mut b,
    )
    .expect("startup");
    b
}

fn connect(args: &[String]) -> ExitCode {
    if args.len() < 4 {
        return usage();
    }
    let (host, port, user, db) = (&args[0], &args[1], &args[2], &args[3]);
    let (mut password, mut sql, mut app, mut bind) = (
        None::<String>,
        None::<String>,
        "authc".to_string(),
        None::<String>,
    );
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--password" => password = args.get(i + 1).cloned(),
            "--sql" => sql = args.get(i + 1).cloned(),
            "--app" => app = args.get(i + 1).cloned().unwrap_or(app),
            "--bind" => bind = args.get(i + 1).cloned(),
            _ => return usage(),
        }
        i += 2;
    }
    let mut c = match Conn::open(host, port, bind.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            println!("CLOSED 接続できない: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let t = Duration::from_secs(20);
    if c.send(&startup_packet(user, db, &app)).is_err() {
        println!("CLOSED 送信に失敗");
        return ExitCode::SUCCESS;
    }
    let mut method = "trust";
    let mut scram: Option<ScramSha256> = None;
    let mut out_rows: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut authenticated = false;
    let mut sql_sent = false;
    loop {
        let msg = match c.read_msg(t) {
            Ok(Some(m)) => m,
            Ok(None) => {
                println!("CLOSED 接続が閉じた");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                println!("CLOSED {e}");
                return ExitCode::SUCCESS;
            }
        };
        match msg {
            Message::AuthenticationCleartextPassword => {
                method = "cleartext";
                let Some(pw) = &password else {
                    println!("ERR 28P01 CLIENT パスワードが必要（クリアテキスト）");
                    return ExitCode::SUCCESS;
                };
                let mut b = BytesMut::new();
                frontend::password_message(pw.as_bytes(), &mut b).expect("pw");
                let _ = c.send(&b);
            }
            Message::AuthenticationMd5Password(body) => {
                method = "md5";
                let Some(pw) = &password else {
                    println!("ERR 28P01 CLIENT パスワードが必要（md5）");
                    return ExitCode::SUCCESS;
                };
                let h = postgres_protocol::authentication::md5_hash(
                    user.as_bytes(),
                    pw.as_bytes(),
                    body.salt(),
                );
                let mut b = BytesMut::new();
                frontend::password_message(h.as_bytes(), &mut b).expect("pw");
                let _ = c.send(&b);
            }
            Message::AuthenticationSasl(body) => {
                method = "scram";
                let mut offered = Vec::new();
                let mut mechs = body.mechanisms();
                while let Ok(Some(m)) = mechs.next() {
                    offered.push(m.to_string());
                }
                if !offered.iter().any(|m| m == SCRAM_SHA_256) {
                    println!("ERR 28000 CLIENT SCRAM-SHA-256 が提示されない: {offered:?}");
                    return ExitCode::SUCCESS;
                }
                let Some(pw) = &password else {
                    println!("ERR 28P01 CLIENT パスワードが必要（SCRAM）");
                    return ExitCode::SUCCESS;
                };
                let s = ScramSha256::new(pw.as_bytes(), ChannelBinding::unsupported());
                let mut b = BytesMut::new();
                frontend::sasl_initial_response(SCRAM_SHA_256, s.message(), &mut b).expect("sasl");
                let _ = c.send(&b);
                scram = Some(s);
            }
            Message::AuthenticationSaslContinue(body) => {
                let Some(s) = scram.as_mut() else { continue };
                if let Err(e) = s.update(body.data()) {
                    println!("ERR 28000 CLIENT SCRAM の処理に失敗: {e}");
                    return ExitCode::SUCCESS;
                }
                let mut b = BytesMut::new();
                frontend::sasl_response(s.message(), &mut b).expect("sasl");
                let _ = c.send(&b);
            }
            Message::AuthenticationSaslFinal(body) => {
                let Some(s) = scram.as_mut() else { continue };
                if let Err(e) = s.finish(body.data()) {
                    println!("ERR 28000 CLIENT サーバの署名の検証に失敗: {e}");
                    return ExitCode::SUCCESS;
                }
            }
            Message::AuthenticationOk => authenticated = true,
            Message::ParameterStatus(p) => {
                params.push(format!(
                    "PARAM {}={}",
                    p.name().unwrap_or("?"),
                    p.value().unwrap_or("?")
                ));
            }
            Message::ErrorResponse(e) => {
                println!("{}", error_line(e.fields()));
                return ExitCode::SUCCESS;
            }
            Message::RowDescription(_) => {}
            Message::DataRow(row) => {
                let buf = row.buffer();
                let mut cols = Vec::new();
                let mut ranges = row.ranges();
                while let Ok(Some(r)) = ranges.next() {
                    cols.push(
                        r.map(|r| String::from_utf8_lossy(&buf[r]).to_string())
                            .unwrap_or_else(|| "\\N".into()),
                    );
                }
                out_rows.push(format!("ROW {}", cols.join("\t")));
            }
            Message::ReadyForQuery(_) => {
                if !authenticated {
                    continue;
                }
                match (&sql, sql_sent) {
                    (Some(q), false) => {
                        let mut b = BytesMut::new();
                        frontend::query(q, &mut b).expect("query");
                        let _ = c.send(&b);
                        sql_sent = true;
                    }
                    _ => {
                        println!("OK {method}");
                        for p in &params {
                            println!("{p}");
                        }
                        for r in &out_rows {
                            println!("{r}");
                        }
                        let mut b = BytesMut::new();
                        frontend::terminate(&mut b);
                        let _ = c.send(&b);
                        return ExitCode::SUCCESS;
                    }
                }
            }
            _ => {}
        }
    }
}

fn stall(args: &[String]) -> ExitCode {
    if args.len() != 4 && args.len() != 5 {
        return usage();
    }
    let timeout_ms: u64 = args[3].parse().unwrap_or(5000);
    let mut c = match Conn::open(&args[0], &args[1], args.get(4).map(String::as_str)) {
        Ok(c) => c,
        Err(e) => {
            println!("CLOSED 0 接続できない: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let report = |tail: String| {
        println!("CLOSED {} {tail}", start.elapsed().as_millis());
    };
    match args[2].as_str() {
        "silent" => {}
        "startup" => {
            let _ = c.send(&startup_packet("postgres", "postgres", "authc-stall"));
        }
        "drip" => {
            // 起動パケットを 100ms に 1 バイトずつ送る（1 バイトごとのタイムアウトでは切れない）
            let p = startup_packet("postgres", "postgres", "authc-stall");
            for byte in p.iter().take(p.len() - 1) {
                if Instant::now() >= deadline {
                    break;
                }
                if c.send(&[*byte]).is_err() {
                    report(String::new());
                    return ExitCode::SUCCESS;
                }
                // 相手が閉じていないか、短く確かめながら待つ
                match c.read_msg(Duration::from_millis(100)) {
                    Ok(Some(Message::ErrorResponse(e))) => {
                        report(error_line(e.fields()));
                        return ExitCode::SUCCESS;
                    }
                    Ok(None) => {
                        report(String::new());
                        return ExitCode::SUCCESS;
                    }
                    Ok(Some(_)) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => {
                        report(String::new());
                        return ExitCode::SUCCESS;
                    }
                }
            }
        }
        _ => return usage(),
    }
    // ここから先は受け身で待つ（認証要求を受け取っても応答しない）
    loop {
        let now = Instant::now();
        if now >= deadline {
            println!("ALIVE {}", start.elapsed().as_millis());
            return ExitCode::SUCCESS;
        }
        match c.read_msg(deadline - now) {
            Ok(Some(Message::ErrorResponse(e))) => {
                report(error_line(e.fields()));
                return ExitCode::SUCCESS;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                report(String::new());
                return ExitCode::SUCCESS;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                report(String::new());
                return ExitCode::SUCCESS;
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("connect") => connect(&args[1..]),
        Some("stall") => stall(&args[1..]),
        _ => usage(),
    }
}
