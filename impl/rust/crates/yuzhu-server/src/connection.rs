//! Per-connection handling: startup, the message loop, and the bridge to
//! `yuzhu-core` (`Session` / `ResultSink`). All core-touching code lives here.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use yuzhu_core::storage::vfs::LocalVfs;
use yuzhu_core::{
    Cluster, ClusterOptions, ColumnDesc, DebugKnobs, Error, Notice, ResultSink, Session, Severity,
    StartupParams,
};

use crate::config::Config;
use crate::protocol::codec::{
    ProtocolError, read_message, read_startup_packet, set_client_latin1, write_message,
};
use crate::protocol::messages::{
    BackendMessage, DEFAULT_MAX_MESSAGE_LEN, ErrorFields, FieldDescription, FrontendMessage,
    StartupPacket,
};
use crate::shutdown::{Coordinator, Registration, ShutdownHandle, ShutdownMode};

/// How often the accept loop looks for new connections, stop requests and
/// a poisoned cluster.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Why [`Server::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Smart or fast shutdown completed; the cluster is cleanly shut down.
    Stopped,
    /// Immediate shutdown: nothing was written. The caller must exit the
    /// process without running destructors that write.
    Immediate,
    /// The cluster was poisoned (PANIC-level error). No checkpoint was
    /// taken; the next start is refused until `--ignore-unclean-shutdown`.
    Poisoned,
}

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
    db: Arc<Cluster>,
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
    coordinator: Arc<Coordinator>,
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
    /// Opens the cluster in `config.data_directory` (on the local file
    /// system) and binds the listening socket. Port 0 picks an ephemeral
    /// port (see [`Server::local_addr`]). The cluster is opened first so
    /// that the port only starts listening once the server is ready.
    pub fn bind(config: &Config) -> io::Result<Self> {
        let vfs = Arc::new(LocalVfs::new(config.data_directory.clone()));
        let cluster = Cluster::open(
            vfs,
            ClusterOptions {
                data_dir: config.data_directory.clone(),
                shared_buffers: config.shared_buffers,
                max_connections: u32::try_from(config.max_connections).unwrap_or(u32::MAX),
                checkpoint_timeout: config.checkpoint_timeout,
                max_wal_size: config.max_wal_size,
                background_checkpointer: true,
                knobs: DebugKnobs::default(),
            },
        )
        .map_err(|e| io::Error::other(format_core_error(&e)))?;
        Self::with_cluster(config, cluster)
    }

    /// Binds the listening socket for an already opened cluster (used by
    /// [`Server::bind`] and by tests running on a simulated disk). If
    /// binding fails the cluster is shut down cleanly again.
    pub fn with_cluster(config: &Config, cluster: Arc<Cluster>) -> io::Result<Self> {
        let listener = match TcpListener::bind((config.listen, config.port)) {
            Ok(l) => l,
            Err(e) => {
                if let Err(se) = cluster.shutdown() {
                    tracing::error!(error = %se, "cluster shutdown after bind failure failed");
                }
                return Err(e);
            }
        };
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                db: cluster,
                max_connections: config.max_connections,
                max_threads: thread_ceiling(config.max_connections),
                max_message_len: DEFAULT_MAX_MESSAGE_LEN,
                authentication_timeout: DEFAULT_AUTHENTICATION_TIMEOUT,
                threads: AtomicUsize::new(0),
                sessions: AtomicUsize::new(0),
                next_pid: AtomicI32::new(1),
                coordinator: Arc::new(Coordinator::default()),
            }),
        })
    }

    /// A handle for requesting smart / fast / immediate shutdown.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle::new(Arc::clone(&self.shared.coordinator))
    }

    /// The cluster this server runs.
    pub fn cluster(&self) -> &Arc<Cluster> {
        &self.shared.db
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

    /// Accepts connections (one thread each) until a shutdown is requested
    /// or the cluster is poisoned, then performs the shutdown:
    ///
    /// - smart / fast: refuse new connections with 57P03 (fast also tells
    ///   every session to stop), wait until all connections ended, then
    ///   `Cluster::shutdown` (final checkpoint).
    /// - immediate: return at once without writing anything.
    /// - poison: return at once without a checkpoint.
    pub fn run(self) -> io::Result<Outcome> {
        let shared = Arc::clone(&self.shared);
        let coordinator = &shared.coordinator;
        tracing::info!(addr = %self.listener.local_addr()?, "listening");
        let mut closed = false;
        let mut interrupted = false;
        loop {
            if shared.db.is_poisoned() {
                tracing::error!("cluster is poisoned; exiting without a checkpoint");
                return Ok(Outcome::Poisoned);
            }
            match coordinator.requested() {
                Some(ShutdownMode::Immediate) => {
                    tracing::warn!("immediate shutdown requested; exiting without a checkpoint");
                    return Ok(Outcome::Immediate);
                }
                Some(mode) => {
                    if !closed {
                        tracing::info!(?mode, "shutting down");
                        coordinator.close();
                        closed = true;
                    }
                    // Smart can be escalated to fast later; interrupt once.
                    if mode == ShutdownMode::Fast && !interrupted {
                        coordinator.interrupt_all();
                        interrupted = true;
                    }
                    if coordinator.is_empty() {
                        break;
                    }
                }
                None => {}
            }
            match self.listener.accept() {
                Ok((stream, _)) => self.spawn_connection(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
        tracing::info!("all connections ended; shutting down the cluster");
        self.shared
            .db
            .shutdown()
            .map_err(|e| io::Error::other(format_core_error(&e)))?;
        Ok(Outcome::Stopped)
    }

    fn spawn_connection(&self, stream: TcpStream) {
        let _ = stream.set_nonblocking(false);
        let shared = Arc::clone(&self.shared);
        let pid = shared.next_pid.fetch_add(1, Ordering::Relaxed);
        let Ok(registered_stream) = stream.try_clone() else {
            tracing::warn!(pid, "cannot clone the socket; dropping the connection");
            return;
        };
        let Some(registration) = shared.coordinator.register(pid, registered_stream) else {
            reject_shutting_down(stream);
            return;
        };
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
        let spawned = std::thread::Builder::new()
            .name(format!("conn-{pid}"))
            .spawn(move || {
                let _guard = guard;
                run_connection(stream, &shared, pid, &registration);
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "failed to spawn connection thread");
        }
    }
}

/// Sends FATAL 57P03 on a socket from the accept loop and closes it.
fn reject_shutting_down(stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
    let mut w = BufWriter::new(stream);
    let _ = send_fatal(&mut w, "57P03", "the database system is shutting down");
}

/// Message plus hint of a core error, for logs and exit messages.
fn format_core_error(e: &Error) -> String {
    match &e.hint {
        Some(h) => format!("{} (hint: {h})", e.message),
        None => e.message.clone(),
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
///
/// Every exit path (normal, I/O error, panic) ends the session with
/// `Session::terminate()`, which aborts an open transaction and releases the
/// writer lock. After a panic the cluster is poisoned first, because shared
/// state may be half-written (`m2.md` 6.3.4).
fn run_connection(stream: TcpStream, shared: &Shared, pid: i32, registration: &Registration) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "?".to_string(), |a| a.to_string());
    tracing::info!(pid, peer = %peer, "connection accepted");
    let _ = stream.set_nodelay(true);
    let panic_stream = stream.try_clone().ok();
    let mut session: Option<Session> = None;
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        handle_connection(stream, shared, pid, registration, &mut session)
    }));
    match result {
        Ok(r) => {
            end_session(&mut session, pid);
            match r {
                Ok(()) => tracing::info!(pid, "connection closed"),
                Err(e) => tracing::info!(pid, error = %e, "connection closed by I/O error"),
            }
        }
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            tracing::error!(pid, panic = %what, "connection thread panicked; poisoning the cluster");
            shared.db.poison();
            end_session(&mut session, pid);
            if let Some(mut s) = panic_stream {
                // Best effort: the client may be in the middle of a message.
                let _ = send_fatal(&mut s, "XX000", "internal error: server thread panicked");
                let _ = s.flush();
            }
        }
    }
}

/// `Session::terminate()` that cannot take the thread down again.
fn end_session(session: &mut Option<Session>, pid: i32) {
    if let Some(mut s) = session.take()
        && panic::catch_unwind(AssertUnwindSafe(|| s.terminate())).is_err()
    {
        tracing::error!(pid, "Session::terminate panicked");
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

fn handle_connection(
    stream: TcpStream,
    shared: &Shared,
    pid: i32,
    registration: &Registration,
    slot: &mut Option<Session>,
) -> io::Result<()> {
    // Bound the startup phase (like `authentication_timeout`): a client that
    // connects and sends nothing must not hold a thread forever. The timeout
    // is set on the socket, so it applies to the cloned reader too.
    stream.set_read_timeout(Some(shared.authentication_timeout))?;
    stream.set_write_timeout(Some(shared.authentication_timeout))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let params = match startup(&mut reader, &mut writer, shared, pid) {
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
    let session = match Session::new(Arc::clone(&shared.db), params) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(pid, error = %e, "session rejected");
            write_core_error(&mut writer, &e, "FATAL")?;
            return writer.flush();
        }
    };

    let backend_pid = session.backend_pid();
    let secret = random_secret(backend_pid);
    registration.set_flag(session.interrupt_flag());
    registration.set_key(backend_pid, secret);
    let session = slot.insert(session);

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
            pid: backend_pid,
            secret,
        },
    )?;
    ready_for_query(&mut writer, session)?;

    message_loop(&mut reader, &mut writer, session, shared, pid)
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
    shared: &Shared,
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
            StartupPacket::CancelRequest {
                pid: target,
                secret,
            } => {
                // Like PostgreSQL: no reply, and a wrong key looks the same.
                let hit = shared.coordinator.cancel(target, secret);
                tracing::info!(pid, target, hit, "cancel request received");
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
    let interrupt = session.interrupt_flag();
    let mut applied_timeout = None;
    loop {
        if interrupt.is_terminate_requested() {
            return terminated_by_administrator(writer);
        }
        set_client_latin1(session.client_encoding_is_latin1());
        let idle_timeout = session.idle_timeout();
        let wait_started = Instant::now();
        match wait_for_input(reader, idle_timeout, &mut applied_timeout) {
            Ok(Wait::Ready) => {}
            Ok(Wait::TimedOut) => {
                let err = session.idle_timeout_error();
                tracing::info!(pid, sqlstate = err.sqlstate.0, "idle timeout");
                write_core_error(writer, &err, "FATAL")?;
                return writer.flush();
            }
            Err(e) => {
                if interrupt.is_terminate_requested() {
                    return terminated_by_administrator(writer);
                }
                return Err(e);
            }
        }
        // The idle timeout also covers the rest of a message whose first
        // bytes have arrived (PostgreSQL stops the timer only after a full
        // message), so a stalled client cannot hold the writer lock forever.
        let read = read_next_message(reader, idle_timeout, wait_started, shared.max_message_len);
        let msg = match read {
            Ok(Some(m)) => m,
            Ok(None) => {
                if interrupt.is_terminate_requested() {
                    return terminated_by_administrator(writer);
                }
                tracing::debug!(pid, "client disconnected without Terminate");
                return Ok(());
            }
            Err(ProtocolError::Io(e)) => {
                if idle_timeout.is_some() && is_timeout(&e) {
                    let err = session.idle_timeout_error();
                    tracing::info!(pid, sqlstate = err.sqlstate.0, "idle timeout");
                    write_core_error(writer, &err, "FATAL")?;
                    return writer.flush();
                }
                if interrupt.is_terminate_requested() {
                    return terminated_by_administrator(writer);
                }
                return Err(e);
            }
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
                if session.is_closing() {
                    // A FATAL was sent (e.g. 57P01, PANIC escalation).
                    return writer.flush();
                }
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
/// Outcome of waiting for the first byte of the next message.
enum Wait {
    Ready,
    TimedOut,
}

/// Waits until a message starts (or EOF), applying `timeout` only to this
/// wait; the rest of a message is read without a timeout (`m3.md` 5.10).
/// `applied` remembers the timeout set on the socket to avoid redundant
/// system calls.
fn wait_for_input(
    reader: &mut BufReader<TcpStream>,
    timeout: Option<Duration>,
    applied: &mut Option<Duration>,
) -> io::Result<Wait> {
    if reader.buffer().is_empty() {
        if *applied != timeout {
            reader.get_ref().set_read_timeout(timeout)?;
            *applied = timeout;
        }
        loop {
            match reader.fill_buf() {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if timeout.is_some() && is_timeout(&e) => return Ok(Wait::TimedOut),
                Err(e) => return Err(e),
            }
        }
    }
    if applied.is_some() {
        reader.get_ref().set_read_timeout(None)?;
        *applied = None;
    }
    Ok(Wait::Ready)
}

/// Reads from the connection until `deadline`; after that every read fails
/// with `TimedOut`.
struct DeadlineReader<'a> {
    inner: &'a mut BufReader<TcpStream>,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inner.buffer().is_empty() {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.inner.get_ref().set_read_timeout(Some(remaining))?;
        }
        self.inner.read(buf)
    }
}

fn read_next_message(
    reader: &mut BufReader<TcpStream>,
    idle_timeout: Option<Duration>,
    wait_started: Instant,
    max_len: usize,
) -> Result<Option<FrontendMessage>, ProtocolError> {
    match idle_timeout {
        Some(t) => {
            let mut dr = DeadlineReader {
                inner: &mut *reader,
                deadline: wait_started + t,
            };
            let r = read_message(&mut dr, max_len);
            let _ = dr.inner.get_ref().set_read_timeout(None);
            r
        }
        None => read_message(reader, max_len),
    }
}

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

/// Fast shutdown: tell the client why the connection ends (57P01).
fn terminated_by_administrator<W: Write>(w: &mut W) -> io::Result<()> {
    send_fatal(
        w,
        "57P01",
        "terminating connection due to administrator command",
    )
}
