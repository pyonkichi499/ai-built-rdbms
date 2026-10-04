//! Per-connection handling: startup, the message loop, and the bridge to
//! `yuzhu-core` (`Session` / `ResultSink`). All core-touching code lives here.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use yuzhu_core::{
    ColumnDesc, Database, DatabaseConfig, Error, Notice, ResultSink, Session, Severity,
    StartupParams,
};

use crate::config::Config;
use crate::protocol::codec::{ProtocolError, read_message, read_startup_packet, write_message};
use crate::protocol::messages::{
    BackendMessage, DEFAULT_MAX_MESSAGE_LEN, ErrorFields, FieldDescription, FrontendMessage,
    StartupPacket,
};

/// Default limit for completing the startup phase (PostgreSQL's
/// `authentication_timeout` default).
pub const DEFAULT_AUTHENTICATION_TIMEOUT: Duration = Duration::from_mins(1);

/// Extra connection threads allowed beyond `max_connections` for clients
/// still in the startup phase. Beyond this ceiling sockets are rejected in
/// the accept loop without spawning a thread.
const STARTUP_MARGIN_MIN: usize = 16;

/// Write timeout used when rejecting a socket from the accept loop, so a
/// client that never reads cannot stall accepting.
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// State shared by all connections of a server.
#[derive(Debug)]
struct Shared {
    db: Arc<Database>,
    /// Limit on established sessions (post-startup).
    max_connections: usize,
    /// Hard ceiling on connection threads, including those still in startup.
    max_threads: usize,
    max_message_len: usize,
    authentication_timeout: Duration,
    /// Connection threads alive (any phase).
    threads: AtomicUsize,
    /// Established sessions.
    sessions: AtomicUsize,
    next_pid: AtomicI32,
}

/// Hard ceiling on connection threads for a given `max_connections`.
fn thread_ceiling(max_connections: usize) -> usize {
    max_connections.saturating_add(STARTUP_MARGIN_MIN.max(max_connections / 2))
}

/// A bound, not yet running server.
#[derive(Debug)]
pub struct Server {
    listener: TcpListener,
    shared: Arc<Shared>,
}

impl Server {
    /// Creates the database and binds the listening socket. Port 0 picks an
    /// ephemeral port (see [`Server::local_addr`]).
    pub fn bind(config: &Config) -> io::Result<Self> {
        let listener = TcpListener::bind((config.listen, config.port))?;
        let db = Database::new(DatabaseConfig {
            database_name: config.database_name.clone(),
        });
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                db,
                max_connections: config.max_connections,
                max_threads: thread_ceiling(config.max_connections),
                max_message_len: DEFAULT_MAX_MESSAGE_LEN,
                authentication_timeout: DEFAULT_AUTHENTICATION_TIMEOUT,
                threads: AtomicUsize::new(0),
                sessions: AtomicUsize::new(0),
                next_pid: AtomicI32::new(1),
            }),
        })
    }

    /// Overrides the startup-phase timeout (default
    /// [`DEFAULT_AUTHENTICATION_TIMEOUT`]). Must be called before [`Server::run`].
    #[must_use]
    pub fn with_authentication_timeout(mut self, timeout: Duration) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.authentication_timeout = timeout;
        }
        self
    }

    /// Overrides the hard ceiling on connection threads (default:
    /// `max_connections` plus a startup margin). Must be called before
    /// [`Server::run`].
    #[must_use]
    pub fn with_max_threads(mut self, max_threads: usize) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.max_threads = max_threads.max(1);
        }
        self
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections forever, one thread per connection.
    pub fn run(self) -> io::Result<()> {
        tracing::info!(addr = %self.listener.local_addr()?, "listening");
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => self.spawn_connection(stream),
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            }
        }
        Ok(())
    }

    fn spawn_connection(&self, stream: TcpStream) {
        let shared = Arc::clone(&self.shared);
        let guard = ThreadGuard::new(&shared);
        if guard.count > shared.max_threads {
            // Reject without parking a thread on this socket.
            tracing::warn!(
                count = guard.count,
                "too many connection threads; rejecting"
            );
            reject_too_many(stream);
            return;
        }
        let pid = shared.next_pid.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("conn-{pid}"))
            .spawn(move || {
                let _guard = guard;
                run_connection(stream, &shared, pid);
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "failed to spawn connection thread");
        }
    }
}

/// Sends FATAL 53300 on a socket from the accept loop and closes it. Uses a
/// short write timeout so that a non-reading client cannot block accepting.
fn reject_too_many(stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
    let mut w = BufWriter::new(stream);
    let _ = send_fatal(&mut w, "53300", "sorry, too many clients already");
}

/// Counts connection threads; decrements on drop (also during unwinding).
#[derive(Debug)]
struct ThreadGuard {
    shared: Arc<Shared>,
    count: usize,
}

impl ThreadGuard {
    fn new(shared: &Arc<Shared>) -> Self {
        let count = shared.threads.fetch_add(1, Ordering::SeqCst) + 1;
        Self {
            shared: Arc::clone(shared),
            count,
        }
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        self.shared.threads.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Increments a counter; decrements on drop (also during unwinding).
#[derive(Debug)]
struct CountGuard<'a> {
    counter: &'a AtomicUsize,
    count: usize,
}

impl<'a> CountGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Self { counter, count }
    }
}

impl Drop for CountGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs one connection, isolating panics so that the server survives.
fn run_connection(stream: TcpStream, shared: &Shared, pid: i32) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "?".to_string(), |a| a.to_string());
    tracing::info!(pid, peer = %peer, "connection accepted");
    let _ = stream.set_nodelay(true);
    let panic_stream = stream.try_clone().ok();
    let result = panic::catch_unwind(AssertUnwindSafe(|| handle_connection(stream, shared, pid)));
    match result {
        Ok(Ok(())) => tracing::info!(pid, "connection closed"),
        Ok(Err(e)) => tracing::info!(pid, error = %e, "connection closed by I/O error"),
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            tracing::error!(pid, panic = %what, "connection thread panicked");
            if let Some(mut s) = panic_stream {
                // Best effort: the client may be in the middle of a message.
                let _ = send_fatal(&mut s, "XX000", "internal error: server thread panicked");
                let _ = s.flush();
            }
        }
    }
}

fn send_error<W: Write>(w: &mut W, severity: &str, code: &str, message: &str) -> io::Result<()> {
    write_message(
        w,
        &BackendMessage::ErrorResponse(ErrorFields {
            severity,
            code,
            message,
            ..ErrorFields::default()
        }),
    )
}

fn send_fatal<W: Write>(w: &mut W, code: &str, message: &str) -> io::Result<()> {
    send_error(w, "FATAL", code, message)?;
    w.flush()
}

/// Pseudo-random cancel key, using the randomly keyed std hasher.
fn random_secret(pid: i32) -> i32 {
    let mut h = RandomState::new().build_hasher();
    h.write_i32(pid);
    if let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    let bytes = h.finish().to_be_bytes();
    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "ERROR",
        Severity::Fatal => "FATAL",
        Severity::Panic => "PANIC",
        Severity::Warning => "WARNING",
        Severity::Notice => "NOTICE",
        Severity::Debug => "DEBUG",
        Severity::Info => "INFO",
        Severity::Log => "LOG",
    }
}

fn write_core_error<W: Write>(w: &mut W, err: &Error, severity: &str) -> io::Result<()> {
    write_message(
        w,
        &BackendMessage::ErrorResponse(ErrorFields {
            severity,
            code: err.sqlstate.0,
            message: &err.message,
            detail: err.detail.as_deref(),
            hint: err.hint.as_deref(),
            position: err.position,
        }),
    )
}

/// Result of the startup phase.
enum Startup {
    /// Continue with these parameters.
    Ready(StartupParams),
    /// Close the connection (EOF, `CancelRequest`, or a FATAL already sent).
    Close,
}

fn handle_connection(stream: TcpStream, shared: &Shared, pid: i32) -> io::Result<()> {
    // Bound the startup phase (like `authentication_timeout`): a client that
    // connects and sends nothing must not hold a thread forever. The timeout
    // is set on the socket, so it applies to the cloned reader too.
    stream.set_read_timeout(Some(shared.authentication_timeout))?;
    stream.set_write_timeout(Some(shared.authentication_timeout))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let params = match startup(&mut reader, &mut writer, pid) {
        Ok(Startup::Ready(p)) => p,
        Ok(Startup::Close) => return Ok(()),
        Err(e) if is_timeout(&e) => {
            tracing::info!(pid, "startup timed out");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    writer.get_ref().set_read_timeout(None)?;
    writer.get_ref().set_write_timeout(None)?;

    // Only established sessions count against max_connections, so sockets
    // idling in startup cannot lock out real clients.
    let session_slot = CountGuard::new(&shared.sessions);
    if session_slot.count > shared.max_connections {
        tracing::warn!(pid, count = session_slot.count, "too many connections");
        return send_fatal(&mut writer, "53300", "sorry, too many clients already");
    }

    tracing::info!(pid, user = %params.user, database = %params.database, "startup");
    let mut session = match Session::new(Arc::clone(&shared.db), params) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(pid, error = %e, "session rejected");
            write_core_error(&mut writer, &e, "FATAL")?;
            return writer.flush();
        }
    };

    write_message(&mut writer, &BackendMessage::AuthenticationOk)?;
    for (name, value) in session.initial_parameter_status() {
        write_message(
            &mut writer,
            &BackendMessage::ParameterStatus {
                name: &name,
                value: &value,
            },
        )?;
    }
    write_message(
        &mut writer,
        &BackendMessage::BackendKeyData {
            pid,
            secret: random_secret(pid),
        },
    )?;
    ready_for_query(&mut writer, &session)?;

    message_loop(&mut reader, &mut writer, &mut session, shared, pid)
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn ready_for_query<W: Write>(w: &mut W, session: &Session) -> io::Result<()> {
    write_message(
        w,
        &BackendMessage::ReadyForQuery(session.transaction_status().as_byte()),
    )?;
    w.flush()
}

/// Reads startup packets until a `StartupMessage` arrives.
fn startup(
    reader: &mut BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    pid: i32,
) -> io::Result<Startup> {
    loop {
        let packet = match read_startup_packet(reader) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(Startup::Close),
            Err(ProtocolError::Io(e)) => return Err(e),
            Err(e) => {
                tracing::warn!(pid, error = %e, "invalid startup packet");
                send_fatal(writer, "08P01", &e.to_string())?;
                return Ok(Startup::Close);
            }
        };
        match packet {
            StartupPacket::SslRequest | StartupPacket::GssEncRequest => {
                if !reader.buffer().is_empty() {
                    // Data pipelined after the request (CVE-2021-23214).
                    send_fatal(
                        writer,
                        "08P01",
                        "received unencrypted data after SSL request",
                    )?;
                    return Ok(Startup::Close);
                }
                writer.write_all(b"N")?;
                writer.flush()?;
            }
            StartupPacket::CancelRequest { pid: target, .. } => {
                // M1 does not interrupt running queries; just close.
                tracing::info!(pid, target, "cancel request received (ignored)");
                return Ok(Startup::Close);
            }
            StartupPacket::Startup {
                major,
                minor,
                params,
            } => {
                if major != 3 {
                    send_fatal(
                        writer,
                        "0A000",
                        &format!(
                            "unsupported frontend protocol {major}.{minor}: server supports 3.0 to 3.0"
                        ),
                    )?;
                    return Ok(Startup::Close);
                }
                return build_startup_params(writer, minor, params);
            }
        }
    }
}

fn build_startup_params(
    writer: &mut BufWriter<TcpStream>,
    minor: u16,
    params: Vec<(String, String)>,
) -> io::Result<Startup> {
    let mut user = None;
    let mut database = None;
    let mut application_name = None;
    let mut options = Vec::new();
    let mut unrecognized = Vec::new();
    for (name, value) in params {
        match name.as_str() {
            "user" => user = Some(value),
            "database" => database = Some(value),
            "application_name" => application_name = Some(value),
            _ if name.starts_with("_pq_.") => unrecognized.push(name),
            _ => options.push((name, value)),
        }
    }
    if minor > 0 || !unrecognized.is_empty() {
        write_message(
            writer,
            &BackendMessage::NegotiateProtocolVersion {
                newest_minor: 0,
                unrecognized: &unrecognized,
            },
        )?;
    }
    let Some(user) = user.filter(|u| !u.is_empty()) else {
        send_fatal(
            writer,
            "28000",
            "no PostgreSQL user name specified in startup packet",
        )?;
        return Ok(Startup::Close);
    };
    let database = database
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| user.clone());
    Ok(Startup::Ready(StartupParams {
        user,
        database,
        application_name,
        options,
    }))
}

fn message_loop(
    reader: &mut BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    session: &mut Session,
    shared: &Shared,
    pid: i32,
) -> io::Result<()> {
    loop {
        let msg = match read_message(reader, shared.max_message_len) {
            Ok(Some(m)) => m,
            Ok(None) => {
                tracing::debug!(pid, "client disconnected without Terminate");
                return Ok(());
            }
            Err(ProtocolError::Io(e)) => return Err(e),
            Err(ProtocolError::InvalidUtf8) => {
                send_error(
                    writer,
                    "ERROR",
                    "22021",
                    "invalid byte sequence for encoding \"UTF8\"",
                )?;
                ready_for_query(writer, session)?;
                continue;
            }
            Err(e) => {
                tracing::warn!(pid, error = %e, "protocol violation");
                return send_fatal(writer, "08P01", &e.to_string());
            }
        };
        match msg {
            FrontendMessage::Query(sql) => {
                tracing::debug!(pid, sql = %sql, "query");
                session.execute_simple(&sql, &mut Sink::new(writer))?;
                ready_for_query(writer, session)?;
            }
            FrontendMessage::Terminate => return Ok(()),
            FrontendMessage::Sync => ready_for_query(writer, session)?,
            FrontendMessage::Extended(kind) => {
                tracing::debug!(pid, ?kind, "extended query message rejected");
                send_error(
                    writer,
                    "ERROR",
                    "0A000",
                    "extended query protocol is not supported yet",
                )?;
                writer.flush()?;
                if !discard_until_sync(reader, writer, shared, pid)? {
                    return Ok(());
                }
                ready_for_query(writer, session)?;
            }
            FrontendMessage::Unknown(tag) => {
                tracing::warn!(pid, tag, "invalid frontend message type");
                return send_fatal(
                    writer,
                    "08P01",
                    &format!("invalid frontend message type {tag}"),
                );
            }
        }
    }
}

/// Discards messages until Sync. Returns `false` if the connection should
/// be closed (EOF, Terminate, or a fatal protocol error already reported).
fn discard_until_sync(
    reader: &mut BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    shared: &Shared,
    pid: i32,
) -> io::Result<bool> {
    loop {
        match read_message(reader, shared.max_message_len) {
            Ok(Some(FrontendMessage::Sync)) => return Ok(true),
            Ok(Some(FrontendMessage::Terminate) | None) => return Ok(false),
            Ok(Some(_)) | Err(ProtocolError::InvalidUtf8) => {}
            Err(ProtocolError::Io(e)) => return Err(e),
            Err(e) => {
                tracing::warn!(pid, error = %e, "protocol violation");
                send_fatal(writer, "08P01", &e.to_string())?;
                return Ok(false);
            }
        }
    }
}

/// [`ResultSink`] that encodes results as protocol messages.
#[derive(Debug)]
pub(crate) struct Sink<'a, W: Write> {
    w: &'a mut W,
}

impl<'a, W: Write> Sink<'a, W> {
    pub(crate) fn new(w: &'a mut W) -> Self {
        Self { w }
    }
}

impl<W: Write> ResultSink for Sink<'_, W> {
    fn row_description(&mut self, columns: &[ColumnDesc]) -> io::Result<()> {
        let fields: Vec<FieldDescription<'_>> = columns
            .iter()
            .map(|c| FieldDescription {
                name: &c.name,
                table_oid: c.table_oid,
                column_attnum: c.column_attnum,
                type_oid: c.type_oid,
                type_len: c.type_len,
                type_modifier: c.type_modifier,
                format: 0,
            })
            .collect();
        write_message(self.w, &BackendMessage::RowDescription(&fields))
    }

    fn data_row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        write_message(self.w, &BackendMessage::DataRow(values))
    }

    fn command_complete(&mut self, tag: &str) -> io::Result<()> {
        write_message(self.w, &BackendMessage::CommandComplete(tag))
    }

    fn empty_query(&mut self) -> io::Result<()> {
        write_message(self.w, &BackendMessage::EmptyQueryResponse)
    }

    fn error(&mut self, err: &Error) -> io::Result<()> {
        write_core_error(self.w, err, severity_str(err.severity))
    }

    fn notice(&mut self, notice: &Notice) -> io::Result<()> {
        write_message(
            self.w,
            &BackendMessage::NoticeResponse(ErrorFields {
                severity: severity_str(notice.severity),
                code: notice.sqlstate.0,
                message: &notice.message,
                detail: notice.detail.as_deref(),
                hint: notice.hint.as_deref(),
                position: None,
            }),
        )
    }

    fn parameter_status(&mut self, name: &str, value: &str) -> io::Result<()> {
        write_message(self.w, &BackendMessage::ParameterStatus { name, value })
    }
}
