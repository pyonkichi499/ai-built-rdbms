//! Simple Query プロトコルだけを話す最小限の同期クライアント。
//!
//! isolationtester は libpq の `PQsendQuery` / `PQisBusy` / `PQgetResult` / `PQconsumeInput`
//! を組み合わせて「送ったまま待たない」実行をする。ここでは同じ粒度の操作を手書きする。
//!
//! - 受信したメッセージは libpq の `pqParseInput3` と同じ規則で解釈する。NOTICE と NOTIFY は
//!   いつでも処理し、それ以外は「取り出されていない結果がある間は解釈を止める」。
//! - 結果は文ごとに 1 つ作る（複数文のステップでは、エラーより前の文の結果も残る）。
//! - プロトコルは Simple Query のみ（Extended Query を持たないサーバでも動かすため）。

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
use postgres_protocol::authentication;
use postgres_protocol::authentication::sasl::{self, ChannelBinding, ScramSha256};
use postgres_protocol::message::backend::{ErrorFields, Message};
use postgres_protocol::message::frontend;

/// 接続先。
#[derive(Debug, Clone)]
pub(crate) struct ConnParams {
    /// ホスト名、または `/` で始まる UNIX ソケットのディレクトリ。
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) dbname: String,
    pub(crate) password: Option<String>,
    /// 起動パケットに載せる追加のパラメータ（`DateStyle` など）。
    pub(crate) startup_params: Vec<(String, String)>,
}

#[derive(Debug)]
enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Stream {
    fn connect(p: &ConnParams) -> io::Result<Self> {
        if p.host.starts_with('/') {
            let path = format!("{}/.s.PGSQL.{}", p.host.trim_end_matches('/'), p.port);
            Ok(Self::Unix(UnixStream::connect(path)?))
        } else {
            let s = TcpStream::connect((p.host.as_str(), p.port))?;
            s.set_nodelay(true)?;
            Ok(Self::Tcp(s))
        }
    }

    fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_read_timeout(d),
            Self::Unix(s) => s.set_read_timeout(d),
        }
    }

    fn set_nonblocking(&self, b: bool) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_nonblocking(b),
            Self::Unix(s) => s.set_nonblocking(b),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Unix(s) => s.flush(),
        }
    }
}

/// `ErrorResponse` / `NoticeResponse` のフィールド（コード, 値）。
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorInfo {
    pub(crate) fields: Vec<(u8, String)>,
    /// libpq 自身が作るエラー（接続断など）の文字列。フィールドを持たない。
    raw: Option<String>,
    /// 実行中だった問い合わせの文字列（libpq の `errQuery`）。`P`（位置）フィールドが
    /// あるときだけ持ち、メッセージに `LINE n: ...` とカーソル `^` を付けるのに使う。
    query: Option<String>,
}

impl ErrorInfo {
    fn from_fields(mut it: ErrorFields<'_>) -> io::Result<Self> {
        let mut fields = Vec::new();
        while let Some(f) = it.next()? {
            fields.push((
                f.type_(),
                String::from_utf8_lossy(f.value_bytes()).into_owned(),
            ));
        }
        Ok(Self {
            fields,
            raw: None,
            query: None,
        })
    }

    /// libpq の `pqGetErrorNotice3` と同じく、位置フィールドがあれば実行中の問い合わせを覚える。
    fn with_query(mut self, query: Option<&str>) -> Self {
        if self.field(b'P').is_some() {
            self.query = query.map(str::to_owned);
        }
        self
    }

    /// libpq が作るエラー結果。`message` は改行で終わる。
    fn local(message: &str) -> Self {
        let mut m = message.to_owned();
        if !m.ends_with('\n') {
            m.push('\n');
        }
        Self {
            fields: Vec::new(),
            raw: Some(m),
            query: None,
        }
    }

    pub(crate) fn field(&self, code: u8) -> Option<&str> {
        self.fields
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| v.as_str())
    }

    /// libpq の既定の詳細度（`PQERRORS_DEFAULT`、`PQSHOW_CONTEXT_ERRORS`）で組み立てた
    /// メッセージ。`PQerrorMessage` や notice processor に渡る文字列に相当する。
    pub(crate) fn libpq_message(&self, is_error: bool) -> String {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }
        let mut s = String::new();
        if let Some(sev) = self.field(b'S') {
            s.push_str(sev);
            s.push_str(":  ");
        }
        s.push_str(self.field(b'M').unwrap_or("missing error text"));
        // pqBuildErrorMessage3: 問い合わせ文が分かれば位置をカーソル表示にする。
        let mut cursor: Option<(&str, i64)> = None;
        if let Some(pos) = self.field(b'P') {
            if let Some(q) = &self.query {
                cursor = Some((q.as_str(), atoi(pos)));
            } else {
                s.push_str(" at character ");
                s.push_str(pos);
            }
        } else if let Some(pos) = self.field(b'p') {
            if let Some(q) = self.field(b'q') {
                cursor = Some((q, atoi(pos)));
            } else {
                s.push_str(" at character ");
                s.push_str(pos);
            }
        }
        s.push('\n');
        if let Some((q, pos)) = cursor
            && pos > 0
        {
            report_error_position(&mut s, q, pos);
        }
        for (code, label) in [(b'D', "DETAIL"), (b'H', "HINT"), (b'q', "QUERY")] {
            if let Some(v) = self.field(code) {
                s.push_str(label);
                s.push_str(":  ");
                s.push_str(v);
                s.push('\n');
            }
        }
        if is_error && let Some(v) = self.field(b'W') {
            s.push_str("CONTEXT:  ");
            s.push_str(v);
            s.push('\n');
        }
        s
    }
}

/// C の `atoi` と同じく、先頭の数字だけを読む（読めなければ 0）。
fn atoi(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let n = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i64, |acc, d| {
            acc.saturating_mul(10).saturating_add(i64::from(d - b'0'))
        });
    if neg { -n } else { n }
}

/// UTF-8 の 1 文字の表示幅（libpq の `pg_encoding_dsplen` を簡略化したもの。0 以下は 1 とみなす
/// ので、結局は東アジアの全角文字が 2、それ以外が 1）。
fn display_width(c: char) -> usize {
    let u = u32::from(c);
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// libpq の `reportErrorPosition`: `LINE n: <行>` と、エラー位置を指す `^` の行を足す。
/// `loc` は 1 始まりの文字位置。長い行は 60 桁に収まるよう `...` で切り詰める。
fn report_error_position(msg: &mut String, query: &str, loc: i64) {
    const DISPLAY_SIZE: usize = 60;
    const MIN_RIGHT_CUT: usize = 10;
    let Ok(loc) = usize::try_from(loc - 1) else {
        return;
    };
    // タブは空白に置き換える（幅は 1 のまま）。
    let chars: Vec<char> = query
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect();
    let mut scridx = Vec::with_capacity(chars.len() + 1);
    let mut scroffset = 0usize;
    let mut loc_line = 1;
    let mut ibeg = 0usize;
    let mut iend: Option<usize> = None;
    let mut cno = 0;
    while cno < chars.len() {
        let ch = chars[cno];
        scridx.push(scroffset);
        if ch == '\r' || ch == '\n' {
            if cno < loc {
                if ch == '\r' || cno == 0 || chars[cno - 1] != '\r' {
                    loc_line += 1;
                }
                ibeg = cno + 1;
            } else {
                iend = Some(cno);
                break;
            }
        }
        scroffset += display_width(ch);
        cno += 1;
    }
    let mut iend = iend.unwrap_or_else(|| {
        scridx.push(scroffset);
        cno
    });
    if loc > cno {
        return;
    }
    let mut beg_trunc = false;
    let mut end_trunc = false;
    if scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
        if scridx[ibeg] + DISPLAY_SIZE >= scridx[loc] + MIN_RIGHT_CUT {
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                iend -= 1;
            }
            end_trunc = true;
        } else {
            while scridx[loc] + MIN_RIGHT_CUT < scridx[iend] {
                iend -= 1;
                end_trunc = true;
            }
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                ibeg += 1;
                beg_trunc = true;
            }
        }
    }
    let mut prefix = format!("LINE {loc_line}: ");
    if beg_trunc {
        prefix.push_str("...");
    }
    // 接頭辞は ASCII だけなので、表示幅はバイト数に等しい。
    let indent = prefix.len() + scridx[loc] - scridx[ibeg];
    msg.push_str(&prefix);
    msg.extend(&chars[ibeg..iend]);
    if end_trunc {
        msg.push_str("...");
    }
    msg.push('\n');
    msg.extend(std::iter::repeat_n(' ', indent));
    msg.push_str("^\n");
}

/// テキスト形式の行の並び（NULL は `None`）。
pub(crate) type Rows = Vec<Vec<Option<String>>>;

/// 文 1 つ分の結果（libpq の `PGresult` に相当）。
#[derive(Debug, Clone)]
pub(crate) enum QueryResult {
    /// 行を返さない文（`PGRES_COMMAND_OK`）。
    Command,
    /// 空の問い合わせ（`PGRES_EMPTY_QUERY`）。
    Empty,
    /// 行を返す文（`PGRES_TUPLES_OK`）。値はテキスト形式、NULL は `None`。
    Tuples { fields: Vec<String>, rows: Rows },
    /// エラー（`PGRES_FATAL_ERROR`）。
    Error(ErrorInfo),
}

/// `NotificationResponse`。
#[derive(Debug, Clone)]
pub(crate) struct Notify {
    pub(crate) pid: i32,
    pub(crate) channel: String,
    pub(crate) payload: String,
}

/// libpq の `asyncStatus` に相当する状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsyncState {
    /// 問い合わせを実行していない（ReadyForQuery を受け取った）。
    Idle,
    /// 結果を待っている。
    Busy,
    /// 取り出されていない結果がある。
    Ready,
}

/// 1 本の接続。
#[derive(Debug)]
pub(crate) struct Conn {
    stream: Stream,
    params: ConnParams,
    inbuf: BytesMut,
    state: AsyncState,
    /// 組み立て中の行セット。
    partial: Option<(Vec<String>, Rows)>,
    ready: Option<QueryResult>,
    /// 受け取ったがまだ呼び出し側が取り出していない NOTICE（libpq 形式の文字列）。
    notices: Vec<String>,
    notifies: VecDeque<Notify>,
    /// 実行中の問い合わせ（libpq の `cmd_queue_head->query`）。エラー位置の表示に使う。
    current_query: Option<String>,
    /// 接続が切れた・プロトコル違反などで使えなくなった。
    broken: Option<String>,
    pub(crate) pid: i32,
    secret_key: i32,
}

fn proto_err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Conn {
    /// 接続して認証し、最初の `ReadyForQuery` まで進める。
    pub(crate) fn connect(params: &ConnParams, application_name: &str) -> io::Result<Self> {
        let mut stream = Stream::connect(params)?;
        let mut out = BytesMut::new();
        let mut startup = vec![
            ("user", params.user.as_str()),
            ("database", params.dbname.as_str()),
            ("application_name", application_name),
        ];
        startup.extend(
            params
                .startup_params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        );
        frontend::startup_message(startup, &mut out)?;
        stream.write_all(&out)?;
        let mut conn = Self {
            stream,
            params: params.clone(),
            inbuf: BytesMut::new(),
            state: AsyncState::Idle,
            partial: None,
            ready: None,
            notices: Vec::new(),
            notifies: VecDeque::new(),
            current_query: None,
            broken: None,
            pid: 0,
            secret_key: 0,
        };
        conn.startup()?;
        Ok(conn)
    }

    fn send(&mut self, out: &BytesMut) -> io::Result<()> {
        self.stream.write_all(out)?;
        self.stream.flush()
    }

    fn password(&self) -> io::Result<&str> {
        self.params
            .password
            .as_deref()
            .ok_or_else(|| proto_err("server requested a password, but none was given"))
    }

    fn read_message_blocking(&mut self) -> io::Result<Message> {
        loop {
            if let Some(m) = Message::parse(&mut self.inbuf)? {
                return Ok(m);
            }
            self.fill(None)?;
        }
    }

    fn startup(&mut self) -> io::Result<()> {
        let mut scram: Option<ScramSha256> = None;
        loop {
            let mut out = BytesMut::new();
            match self.read_message_blocking()? {
                Message::AuthenticationOk | Message::ParameterStatus(_) => {}
                Message::AuthenticationCleartextPassword => {
                    let pw = self.password()?.to_owned();
                    frontend::password_message(pw.as_bytes(), &mut out)?;
                    self.send(&out)?;
                }
                Message::AuthenticationMd5Password(body) => {
                    let hash = authentication::md5_hash(
                        self.params.user.as_bytes(),
                        self.password()?.as_bytes(),
                        body.salt(),
                    );
                    frontend::password_message(hash.as_bytes(), &mut out)?;
                    self.send(&out)?;
                }
                Message::AuthenticationSasl(body) => {
                    let mut mechs = body.mechanisms();
                    let mut ok = false;
                    while let Some(m) = mechs.next()? {
                        ok |= m == sasl::SCRAM_SHA_256;
                    }
                    if !ok {
                        return Err(proto_err("server offered no supported SASL mechanism"));
                    }
                    let s = ScramSha256::new(
                        self.password()?.as_bytes(),
                        ChannelBinding::unsupported(),
                    );
                    frontend::sasl_initial_response(sasl::SCRAM_SHA_256, s.message(), &mut out)?;
                    self.send(&out)?;
                    scram = Some(s);
                }
                Message::AuthenticationSaslContinue(body) => {
                    let s = scram
                        .as_mut()
                        .ok_or_else(|| proto_err("unexpected SASL continue"))?;
                    s.update(body.data())?;
                    frontend::sasl_response(s.message(), &mut out)?;
                    self.send(&out)?;
                }
                Message::AuthenticationSaslFinal(body) => {
                    scram
                        .as_mut()
                        .ok_or_else(|| proto_err("unexpected SASL final"))?
                        .finish(body.data())?;
                }
                Message::BackendKeyData(body) => {
                    self.pid = body.process_id();
                    self.secret_key = body.secret_key();
                }
                Message::NoticeResponse(body) => {
                    let info = ErrorInfo::from_fields(body.fields())?;
                    self.notices.push(info.libpq_message(false));
                }
                Message::ErrorResponse(body) => {
                    let info = ErrorInfo::from_fields(body.fields())?;
                    return Err(proto_err(info.libpq_message(true).trim_end().to_owned()));
                }
                Message::ReadyForQuery(_) => return Ok(()),
                _ => return Err(proto_err("unexpected message during startup")),
            }
        }
    }

    /// ソケットから読めるだけ読む。`timeout` が `None` なら 1 バイト以上届くまで待つ。
    /// `Some(Duration::ZERO)` なら待たない。届いたら `true`。
    fn fill(&mut self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut tmp = [0u8; 8192];
        let r = match timeout {
            Some(d) if d.is_zero() => {
                self.stream.set_nonblocking(true)?;
                let r = self.stream.read(&mut tmp);
                self.stream.set_nonblocking(false)?;
                r
            }
            Some(d) => {
                self.stream.set_read_timeout(Some(d))?;
                let r = self.stream.read(&mut tmp);
                self.stream.set_read_timeout(None)?;
                r
            }
            None => self.stream.read(&mut tmp),
        };
        match r {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                // libpq の文言に合わせる（fe-misc.c pqReadData）。
                "server closed the connection unexpectedly\n\
                 \tThis probably means the server terminated abnormally\n\
                 \tbefore or while processing the request.",
            )),
            Ok(n) => {
                self.inbuf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// 接続が壊れたときに、libpq と同じく「エラー結果 + Idle」にする。
    fn mark_broken(&mut self, e: &io::Error) {
        let msg = e.to_string();
        self.broken = Some(msg.clone());
        if self.state != AsyncState::Idle {
            self.partial = None;
            self.ready = Some(QueryResult::Error(ErrorInfo::local(&msg)));
            self.state = AsyncState::Ready;
        }
    }

    /// バッファ内のメッセージを libpq の `pqParseInput3` と同じ規則で解釈する。
    fn parse_input(&mut self) -> io::Result<()> {
        loop {
            let Some(header) = postgres_protocol::message::backend::Header::parse(&self.inbuf)?
            else {
                return Ok(());
            };
            let total = usize::try_from(header.len()).map_err(|_| proto_err("bad length"))? + 1;
            if self.inbuf.len() < total {
                return Ok(());
            }
            let tag = header.tag();
            // NOTICE・NOTIFY・ParameterStatus は状態に関係なく処理する。
            let is_async = matches!(tag, b'N' | b'A' | b'S');
            if !is_async && self.state != AsyncState::Busy && self.state == AsyncState::Ready {
                return Ok(());
            }
            // Idle 中に届いた想定外のメッセージは読み捨てる。
            let Some(msg) = Message::parse(&mut self.inbuf)? else {
                return Ok(());
            };
            match msg {
                Message::NoticeResponse(body) => {
                    let info =
                        ErrorInfo::from_fields(body.fields())?.with_query(self.active_query());
                    self.notices.push(info.libpq_message(false));
                }
                Message::NotificationResponse(body) => {
                    self.notifies.push_back(Notify {
                        pid: body.process_id(),
                        channel: body.channel()?.to_owned(),
                        payload: body.message()?.to_owned(),
                    });
                }
                Message::ParameterStatus(_) => {}
                _ if self.state == AsyncState::Idle => {}
                Message::RowDescription(body) => {
                    let names = body
                        .fields()
                        .map(|f| Ok(f.name().to_owned()))
                        .collect::<Vec<_>>()?;
                    self.partial = Some((names, Vec::new()));
                }
                Message::DataRow(body) => {
                    let buf = body.buffer();
                    let row = body
                        .ranges()
                        .map(|r| Ok(r.map(|r| String::from_utf8_lossy(&buf[r]).into_owned())))
                        .collect::<Vec<_>>()?;
                    match self.partial.as_mut() {
                        Some((_, rows)) => rows.push(row),
                        None => return Err(proto_err("DataRow without RowDescription")),
                    }
                }
                Message::CommandComplete(_) => {
                    self.ready = Some(match self.partial.take() {
                        Some((fields, rows)) => QueryResult::Tuples { fields, rows },
                        None => QueryResult::Command,
                    });
                    self.state = AsyncState::Ready;
                }
                Message::EmptyQueryResponse => {
                    self.partial = None;
                    self.ready = Some(QueryResult::Empty);
                    self.state = AsyncState::Ready;
                }
                Message::ErrorResponse(body) => {
                    self.partial = None;
                    let info =
                        ErrorInfo::from_fields(body.fields())?.with_query(self.active_query());
                    self.ready = Some(QueryResult::Error(info));
                    self.state = AsyncState::Ready;
                }
                Message::ReadyForQuery(_) => {
                    self.partial = None;
                    self.current_query = None;
                    self.state = AsyncState::Idle;
                }
                Message::CopyInResponse(_) | Message::CopyOutResponse(_) => {
                    return Err(proto_err("COPY is not supported by this runner"));
                }
                _ => {}
            }
        }
    }

    /// 実行中の問い合わせ。`ReadyForQuery` を受け取ったら（libpq がコマンドキューから
    /// 外すのと同じく）なくなる。
    fn active_query(&self) -> Option<&str> {
        if self.state == AsyncState::Idle {
            None
        } else {
            self.current_query.as_deref()
        }
    }

    /// 読めるだけ読んで解釈する（`PQconsumeInput`）。
    pub(crate) fn consume_input(&mut self) {
        if self.broken.is_some() {
            return;
        }
        let r = self
            .fill(Some(Duration::ZERO))
            .and_then(|_| self.parse_input());
        if let Err(e) = r {
            self.mark_broken(&e);
        }
    }

    /// `timeout` まで受信を待ってから解釈する。何か届いたら `true`（select(2) 相当）。
    pub(crate) fn wait_input(&mut self, timeout: Duration) -> bool {
        if self.broken.is_some() {
            return true;
        }
        match self.fill(Some(timeout)) {
            Ok(got) => {
                if let Err(e) = self.parse_input() {
                    self.mark_broken(&e);
                }
                got
            }
            Err(e) => {
                self.mark_broken(&e);
                true
            }
        }
    }

    /// 問い合わせを送る（`PQsendQuery`）。
    pub(crate) fn send_query(&mut self, sql: &str) -> io::Result<()> {
        if let Some(b) = &self.broken {
            return Err(proto_err(b.clone()));
        }
        if self.state != AsyncState::Idle {
            return Err(proto_err("another command is already in progress"));
        }
        let mut out = BytesMut::new();
        frontend::query(sql, &mut out)?;
        self.send(&out)?;
        self.current_query = Some(sql.to_owned());
        self.state = AsyncState::Busy;
        Ok(())
    }

    /// 結果がまだ揃っていないか（`PQisBusy`）。届いているメッセージは解釈する。
    pub(crate) fn is_busy(&mut self) -> bool {
        if self.broken.is_none()
            && let Err(e) = self.parse_input()
        {
            self.mark_broken(&e);
        }
        self.state == AsyncState::Busy
    }

    /// 次の結果を取り出す（`PQgetResult`）。必要なら届くまで待つ。全部取り出したら `None`。
    pub(crate) fn get_result(&mut self) -> Option<QueryResult> {
        loop {
            if self.broken.is_none()
                && let Err(e) = self.parse_input()
            {
                self.mark_broken(&e);
            }
            match self.state {
                AsyncState::Ready => {
                    let r = self.ready.take();
                    self.state = if self.broken.is_some() {
                        AsyncState::Idle
                    } else {
                        AsyncState::Busy
                    };
                    return r;
                }
                AsyncState::Idle => return None,
                AsyncState::Busy => {
                    if let Err(e) = self.fill(None) {
                        self.mark_broken(&e);
                    }
                }
            }
        }
    }

    /// 問い合わせを実行して最後の結果を返す（`PQexec`）。
    pub(crate) fn exec(&mut self, sql: &str) -> QueryResult {
        if let Err(e) = self.send_query(sql) {
            return QueryResult::Error(ErrorInfo::local(&e.to_string()));
        }
        let mut last = QueryResult::Command;
        while let Some(r) = self.get_result() {
            last = r;
        }
        last
    }

    /// 溜まっている NOTICE を取り出す。
    pub(crate) fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// 溜まっている NOTIFY を 1 つ取り出す（`PQnotifies`）。
    pub(crate) fn next_notify(&mut self) -> Option<Notify> {
        self.notifies.pop_front()
    }

    /// 実行中の問い合わせに `CancelRequest` を送る（`PQcancelBlocking`）。
    pub(crate) fn cancel(&self) -> io::Result<()> {
        let mut s = Stream::connect(&self.params)?;
        let mut out = BytesMut::new();
        frontend::cancel_request(self.pid, self.secret_key, &mut out);
        s.write_all(&out)?;
        s.flush()?;
        // サーバが接続を閉じるまで待つ。
        let mut tmp = [0u8; 64];
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        while matches!(s.read(&mut tmp), Ok(n) if n > 0) {}
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let mut out = BytesMut::new();
        frontend::terminate(&mut out);
        let _ = self.stream.write_all(&out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期待値はすべて `PostgreSQL` 17 の isolationtester（libpq）が実際に出した文字列。
    fn err(fields: &[(u8, &str)], query: Option<&str>) -> String {
        let info = ErrorInfo {
            fields: fields.iter().map(|(c, v)| (*c, (*v).to_owned())).collect(),
            raw: None,
            query: None,
        }
        .with_query(query);
        info.libpq_message(true)
    }

    const COL: (u8, &str) = (b'M', "column \"nocol\" does not exist");

    #[test]
    fn position_without_query_is_text() {
        assert_eq!(
            err(&[(b'S', "ERROR"), COL, (b'P', "8")], None),
            "ERROR:  column \"nocol\" does not exist at character 8\n"
        );
    }

    #[test]
    fn position_with_query_is_cursor() {
        assert_eq!(
            err(
                &[(b'S', "ERROR"), COL, (b'P', "8")],
                Some("SELECT nocol FROM pg_class;")
            ),
            "ERROR:  column \"nocol\" does not exist\n\
             LINE 1: SELECT nocol FROM pg_class;\n               ^\n"
        );
        // 2 行目・タブは空白に置き換え。
        assert_eq!(
            err(
                &[(b'S', "ERROR"), COL, (b'P', "10")],
                Some("\n\tSELECT\tnocol FROM t;")
            ),
            "ERROR:  column \"nocol\" does not exist\n\
             LINE 2:  SELECT nocol FROM t;\n                ^\n"
        );
        // CR LF は 1 行と数える。
        assert_eq!(
            err(
                &[(b'S', "ERROR"), COL, (b'P', "21")],
                Some("SELECT 1;\r\nSELECT\r\n nocol; ")
            ),
            "ERROR:  column \"nocol\" does not exist\n\
             LINE 3:  nocol; \n         ^\n"
        );
    }

    #[test]
    fn long_lines_are_truncated() {
        let a = "a".repeat(74);
        let q = format!("SELECT '{a}' AS v, nocol2 FROM t;");
        let pos = (q.find("nocol2").unwrap() + 1).to_string();
        assert_eq!(
            err(&[(b'S', "ERROR"), COL, (b'P', &pos)], Some(&q)),
            format!(
                "ERROR:  column \"nocol\" does not exist\n\
                 LINE 1: ...{}' AS v, nocol2 FRO...\n{}^\n",
                "a".repeat(42),
                " ".repeat(61)
            )
        );
        let b = "b".repeat(80);
        let q = format!("SELECT nocol3, '{b}' FROM t;");
        assert_eq!(
            err(&[(b'S', "ERROR"), COL, (b'P', "8")], Some(&q)),
            format!(
                "ERROR:  column \"nocol\" does not exist\n\
                 LINE 1: SELECT nocol3, '{}...\n               ^\n",
                "b".repeat(44)
            )
        );
        // 全角文字は 2 桁として数える。
        let j = "日本語テキスト".repeat(5);
        let q = format!("SELECT '{j}' AS j, nocol4 FROM t;");
        let pos = (q.chars().position(|c| c == 'n').unwrap() + 1).to_string();
        assert_eq!(
            err(&[(b'S', "ERROR"), COL, (b'P', &pos)], Some(&q)),
            format!(
                "ERROR:  column \"nocol\" does not exist\n\
                 LINE 1: ...{}' AS j, nocol4 FRO...\n{}^\n",
                "日本語テキスト".repeat(3),
                " ".repeat(61)
            )
        );
    }

    #[test]
    fn internal_query_position() {
        assert_eq!(
            err(
                &[
                    (b'S', "ERROR"),
                    COL,
                    (b'p', "8"),
                    (b'q', "SELECT nocol FROM t"),
                    (
                        b'W',
                        "PL/pgSQL function inline_code_block line 1 at PERFORM"
                    ),
                ],
                Some("DO $$ ... $$;")
            ),
            "ERROR:  column \"nocol\" does not exist\n\
             LINE 1: SELECT nocol FROM t\n               ^\n\
             QUERY:  SELECT nocol FROM t\n\
             CONTEXT:  PL/pgSQL function inline_code_block line 1 at PERFORM\n"
        );
    }
}
