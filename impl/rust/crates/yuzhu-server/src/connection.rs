//! Per-connection handling: startup, the message loop, and the bridge to
//! `yuzhu-core` (`Session` / `ResultSink`). All core-touching code lives here.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use yuzhu_core::storage::vfs::LocalVfs;
use yuzhu_core::{
    Cluster, ClusterOptions, ColumnDesc, DebugKnobs, Error, ExtOutcome, Format, Notice, ResultSink,
    Session, Severity, StartupParams, sqlstate,
};

use yuzhu_core::util::clock::{Clock, SystemClock};

use crate::auth::{
    AuthContext, AuthFailure, ClusterDirectory, Deadline, FatalError, Watchdog, authenticate,
    no_hba_entry,
};
use crate::config::Config;
use crate::hba::HbaRules;
use crate::protocol::codec::{
    ProtocolError, RawMessage, decode, is_ext_tag, read_raw, read_startup_packet,
    set_client_latin1, write_message,
};
use crate::protocol::messages::{
    BackendMessage, DEFAULT_MAX_MESSAGE_LEN, DescribeTarget, ErrorFields, FieldDescription,
    FrontendMessage, StartupPacket,
};
use crate::shutdown::{Coordinator, Registration, ShutdownHandle, ShutdownMode};

/// How often the accept loop looks for new connections, stop requests and
/// a poisoned cluster.
/// Connection thread stack (virtual; pages are committed on use). Debug builds
/// have large frames, so the default 2 MB is not enough for deep queries.
const CONNECTION_STACK_SIZE: usize = 64 << 20;
/// Bytes `check_stack_depth` allows; the rest is headroom for unchecked frames.
const CONNECTION_STACK_BUDGET: usize = 16 << 20;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Length of one wait for `CopyData` before the session is polled for a
/// cancel request or `statement_timeout` (`m4/10` 5.8).
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    hba: HbaRules,
    clock: Arc<dyn Clock>,
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
        // The hba file is read before the cluster is opened (a broken file must not leave a
        // pid file or a half-started cluster behind).
        let hba = load_hba(&config.hba_path())?;
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
                ..ClusterOptions::new(config.data_directory.clone())
            },
        )
        .map_err(|e| io::Error::other(format_core_error(&e)))?;
        Ok(Self::with_cluster(config, cluster)?.with_hba(hba))
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
                authentication_timeout: config.authentication_timeout,
                hba: HbaRules::allow_all_trust(),
                clock: Arc::new(SystemClock),
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

    /// Replaces the hba rules (default: every connection is allowed with `trust`). Must be
    /// called before [`Server::run`].
    #[must_use]
    pub fn with_hba(mut self, rules: HbaRules) -> Self {
        for w in rules.warnings() {
            tracing::warn!("yuzhu_hba.conf: {w}");
        }
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.hba = rules;
        }
        self
    }

    /// Replaces the clock used for `VALID UNTIL` (tests). Must be called before [`Server::run`].
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.clock = clock;
        }
        self
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
            .stack_size(CONNECTION_STACK_SIZE)
            .spawn(move || {
                let _guard = guard;
                yuzhu_core::sql::set_stack_budget(CONNECTION_STACK_BUDGET);
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

/// Reads and parses the hba file.
fn load_hba(path: &std::path::Path) -> io::Result<HbaRules> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| io::Error::other(format!("could not open \"{}\": {e}", path.display())))?;
    HbaRules::parse(&text).map_err(io::Error::other)
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

/// Writes an authentication FATAL (with DETAIL when present). The caller flushes.
fn write_auth_fatal<W: Write>(w: &mut W, f: &FatalError) -> io::Result<()> {
    write_message(
        w,
        &BackendMessage::ErrorResponse(ErrorFields {
            severity: "FATAL",
            code: f.sqlstate,
            message: &f.message,
            detail: f.detail.as_deref(),
            ..ErrorFields::default()
        }),
    )
}

fn send_fatal<W: Write>(w: &mut W, code: &str, message: &str) -> io::Result<()> {
    send_error(w, "FATAL", code, message)?;
    w.flush()
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
            context: err.context(),
            schema: err.schema(),
            table: err.table(),
            column: err.column(),
            constraint: err.constraint(),
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

#[allow(clippy::too_many_lines)] // 接続 1 本の流れ（起動パケット→認証→セッション）を 1 関数で追えるようにしている。
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
    // The deadline covers startup and authentication together (`m5/08` 5.5).
    let deadline = Deadline::new(shared.authentication_timeout);
    stream.set_read_timeout(Some(shared.authentication_timeout))?;
    stream.set_write_timeout(Some(shared.authentication_timeout))?;
    let ctl = stream.try_clone()?;
    let Ok(client_addr) = stream.peer_addr().map(|a| a.ip().to_canonical()) else {
        let mut w = BufWriter::new(stream);
        return send_fatal(&mut w, "28000", "could not determine the client address");
    };
    // Cuts clients that trickle bytes: a per-read timeout alone never fires for them.
    let watchdog = Watchdog::start(stream.try_clone()?, shared.authentication_timeout)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    let mut params = match startup(&mut reader, &mut writer, shared, pid) {
        Ok(Startup::Ready(p)) => p,
        Ok(Startup::Close) => return Ok(()),
        Err(e) if is_timeout(&e) || watchdog.fired() => {
            tracing::info!(pid, "startup timed out");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    if watchdog.fired() {
        tracing::info!(pid, "startup timed out");
        return Ok(());
    }
    params.client_addr = Some(client_addr);

    let Some(rule) = shared
        .hba
        .find_rule(client_addr, &params.database, &params.user)
    else {
        let f = no_hba_entry(client_addr, &params.user, &params.database);
        tracing::info!(pid, user = %params.user, database = %params.database, "no hba entry");
        write_auth_fatal(&mut writer, &f)?;
        return writer.flush();
    };
    let method = rule.method;
    let hba_line = rule.line;
    let directory = ClusterDirectory {
        cluster: &shared.db,
        clock: &*shared.clock,
    };
    let ctx = AuthContext {
        directory: &directory,
        user: &params.user,
        database: &params.database,
        client_addr,
    };
    let mut tick = || match deadline.remaining() {
        None => Err(AuthFailure::Timeout),
        Some(left) => {
            let left = left.max(Duration::from_millis(1));
            ctl.set_read_timeout(Some(left)).map_err(AuthFailure::Io)?;
            ctl.set_write_timeout(Some(left)).map_err(AuthFailure::Io)?;
            Ok(())
        }
    };
    let outcome = authenticate(&mut reader, &mut writer, method, &ctx, &mut tick);
    let outcome = match outcome {
        Err(AuthFailure::Io(e)) if watchdog.fired() || is_timeout(&e) => Err(AuthFailure::Timeout),
        Ok(()) if !watchdog.cancel() => Err(AuthFailure::Timeout),
        other => other,
    };
    match outcome {
        Ok(()) => {
            tracing::info!(
                pid,
                user = %params.user,
                database = %params.database,
                method = method.as_str(),
                hba_line,
                "authenticated"
            );
        }
        Err(AuthFailure::Fatal(f)) => {
            tracing::info!(
                pid,
                user = %params.user,
                database = %params.database,
                method = method.as_str(),
                hba_line,
                reason = f.log_detail.as_deref().unwrap_or(&f.message),
                "authentication failed"
            );
            write_auth_fatal(&mut writer, &f)?;
            return writer.flush();
        }
        Err(AuthFailure::Io(e)) => return Err(e),
        Err(AuthFailure::Timeout) => {
            tracing::info!(
                pid,
                method = method.as_str(),
                hba_line,
                "authentication timed out"
            );
            let _ = writer
                .get_ref()
                .set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
            let _ = send_fatal(
                &mut writer,
                "57014",
                "canceling authentication due to timeout",
            );
            return Ok(());
        }
    }
    write_message(&mut writer, &BackendMessage::AuthenticationOk)?;
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
    let Ok(secret_bytes) = yuzhu_auth::random::bytes::<4>() else {
        return send_fatal(&mut writer, "XX000", "could not generate random numbers");
    };
    let secret = i32::from_be_bytes(secret_bytes);
    registration.set_flag(session.interrupt_flag());
    registration.set_key(backend_pid, secret);
    let session = slot.insert(session);

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

/// PostgreSQL cuts user and database names to NAMEDATALEN - 1 bytes (at a character boundary).
fn truncate_name(s: &str) -> String {
    const MAX: usize = 63;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
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
    let user = user.map(|u| truncate_name(&u));
    let database = database.map(|d| truncate_name(&d));
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
        client_addr: None,
    }))
}

/// After a `CopyData` / `CopyDone` / `CopyFail` was handled: sends
/// `ReadyForQuery` unless the COPY continues (or the connection is closing
/// after a FATAL), and flushes.
fn finish_copy_message<W: Write>(w: &mut W, session: &Session) -> io::Result<()> {
    if session.is_closing() {
        w.flush()
    } else if session.is_copying_in() {
        // A follow-up `COPY` statement may have sent a new `G`.
        w.flush()
    } else {
        ready_for_query(w, session)
    }
}

/// The type byte a frontend message arrived with.
fn frontend_tag(msg: &FrontendMessage) -> u8 {
    match msg {
        FrontendMessage::Query(_) => b'Q',
        FrontendMessage::Terminate => b'X',
        FrontendMessage::Sync => b'S',
        FrontendMessage::Flush => b'H',
        FrontendMessage::Parse { .. } => b'P',
        FrontendMessage::Bind { .. } => b'B',
        FrontendMessage::Describe { .. } => b'D',
        FrontendMessage::Close { .. } => b'C',
        FrontendMessage::Execute { .. } => b'E',
        FrontendMessage::FunctionCall => b'F',
        FrontendMessage::CopyData(_) => b'd',
        FrontendMessage::CopyDone => b'c',
        FrontendMessage::CopyFail(_) => b'f',
        FrontendMessage::Unknown(tag) => *tag,
    }
}

#[allow(clippy::too_many_lines)]
fn message_loop(
    reader: &mut BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    session: &mut Session,
    shared: &Shared,
    pid: i32,
) -> io::Result<()> {
    let interrupt = session.interrupt_flag();
    let mut applied_timeout = None;
    let mut ext = ExtState::Normal;
    loop {
        if interrupt.is_terminate_requested() {
            return terminated_by_administrator(writer);
        }
        set_client_latin1(session.client_encoding_is_latin1());
        // During COPY FROM STDIN the connection is not idle: the session
        // has no idle timeout, and the wait is cut into `COPY_POLL_INTERVAL`
        // slices so that cancel / statement_timeout are noticed.
        let copying = session.is_copying_in();
        let idle_timeout = if copying {
            None
        } else {
            session.idle_timeout()
        };
        let wait_timeout = if copying {
            Some(COPY_POLL_INTERVAL)
        } else {
            idle_timeout
        };
        let wait_started = Instant::now();
        match wait_for_input(reader, wait_timeout, &mut applied_timeout) {
            Ok(Wait::Ready) => {}
            Ok(Wait::TimedOut) if copying => {
                session.copy_poll(&mut Sink::new(writer))?;
                if session.is_closing() {
                    return writer.flush();
                }
                if !session.is_copying_in() {
                    ready_for_query(writer, session)?;
                }
                continue;
            }
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
        let raw = match read {
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
            Err(e) => {
                tracing::warn!(pid, error = %e, "protocol violation");
                return send_fatal(writer, "08P01", &e.to_string());
            }
        };
        // After an error in an Extended Query message everything up to
        // Sync (or Terminate) is dropped undecoded, Flush included.
        if ext == ExtState::SkipTillSync {
            match raw.tag {
                b'S' => ext = ExtState::Normal,
                b'X' => return Ok(()),
                _ => continue,
            }
        }
        let tag = raw.tag;
        let msg = match decode(raw) {
            Ok(m) => m,
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
            // While COPY waits for data any other message loses protocol
            // synchronization, whatever its body.
            Err(ProtocolError::BadBody { .. }) if copying => {
                return copy_sync_lost(writer, tag);
            }
            Err(ProtocolError::BadBody {
                tag,
                sqlstate,
                message,
            }) => {
                tracing::debug!(pid, tag, sqlstate, message = %message, "bad message body");
                send_error(writer, "ERROR", sqlstate, &message)?;
                writer.flush()?;
                if is_ext_tag(tag) {
                    session.msg_abort_implicit();
                    ext = ExtState::SkipTillSync;
                } else {
                    ready_for_query(writer, session)?;
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(pid, error = %e, "protocol violation");
                return send_fatal(writer, "08P01", &e.to_string());
            }
        };
        match msg {
            FrontendMessage::Terminate => return Ok(()),
            FrontendMessage::CopyData(data) if copying => {
                session.copy_data(&data, &mut Sink::new(writer))?;
                finish_copy_message(writer, session)?;
                if session.is_closing() {
                    return Ok(());
                }
            }
            FrontendMessage::CopyDone if copying => {
                session.copy_done(&mut Sink::new(writer))?;
                finish_copy_message(writer, session)?;
                if session.is_closing() {
                    return Ok(());
                }
            }
            FrontendMessage::CopyFail(message) if copying => {
                tracing::debug!(pid, message = %message, "COPY failed by the client");
                session.copy_fail(&message, &mut Sink::new(writer))?;
                finish_copy_message(writer, session)?;
            }
            // Flush and Sync do nothing while a COPY waits for data.
            FrontendMessage::Sync | FrontendMessage::Flush if copying => {}
            // Anything else ends the connection (PostgreSQL loses protocol
            // synchronization too).
            msg if copying => {
                let tag = frontend_tag(&msg);
                tracing::warn!(pid, tag, "unexpected message during COPY from stdin");
                return copy_sync_lost(writer, tag);
            }
            // Outside COPY, stray CopyData / CopyDone / CopyFail are ignored
            // (they follow an error that already ended the COPY).
            FrontendMessage::CopyData(_)
            | FrontendMessage::CopyDone
            | FrontendMessage::CopyFail(_) => {}
            FrontendMessage::Query(sql) => {
                tracing::debug!(pid, sql = %sql, "query");
                session.execute_simple(&sql, &mut Sink::new(writer))?;
                if session.is_closing() {
                    // A FATAL was sent (e.g. 57P01, PANIC escalation).
                    return writer.flush();
                }
                if !session.is_copying_in() {
                    ready_for_query(writer, session)?;
                }
            }
            FrontendMessage::Sync => {
                let result = session.msg_sync();
                for (name, value) in session.pending_parameter_status() {
                    write_message(
                        writer,
                        &BackendMessage::ParameterStatus {
                            name: &name,
                            value: &value,
                        },
                    )?;
                }
                if let Err(e) = result {
                    write_core_error(writer, &e, severity_str(e.severity))?;
                }
                if session.is_closing() {
                    return writer.flush();
                }
                ready_for_query(writer, session)?;
            }
            FrontendMessage::Flush => writer.flush()?,
            FrontendMessage::FunctionCall => {
                send_error(writer, "ERROR", "0A000", "function call is not supported")?;
                ready_for_query(writer, session)?;
            }
            msg @ (FrontendMessage::Parse { .. }
            | FrontendMessage::Bind { .. }
            | FrontendMessage::Describe { .. }
            | FrontendMessage::Close { .. }
            | FrontendMessage::Execute { .. }) => match handle_ext(session, msg, writer)? {
                Ok(Next::Continue) => {}
                // The message loop picks the COPY up through `is_copying_in`.
                Ok(Next::CopyIn) => writer.flush()?,
                Err(e) => {
                    if is_connection_lost(&e) {
                        return Ok(());
                    }
                    write_core_error(writer, &e, severity_str(e.severity))?;
                    writer.flush()?;
                    session.msg_abort_implicit();
                    if session.is_closing() {
                        return writer.flush();
                    }
                    ext = ExtState::SkipTillSync;
                }
            },
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
) -> Result<Option<RawMessage>, ProtocolError> {
    match idle_timeout {
        Some(t) => {
            let mut dr = DeadlineReader {
                inner: &mut *reader,
                deadline: wait_started + t,
            };
            let r = read_raw(&mut dr, max_len);
            let _ = dr.inner.get_ref().set_read_timeout(None);
            r
        }
        None => read_raw(reader, max_len),
    }
}

/// What `message_loop` does after an Extended Query message failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExtState {
    Normal,
    /// An Extended Query message failed: everything up to Sync (or
    /// Terminate) is read and dropped, Flush included.
    SkipTillSync,
}

/// What follows a successful Extended Query message.
enum Next {
    Continue,
    /// `Execute` started `COPY FROM STDIN` (`CopyInResponse` was sent).
    CopyIn,
}

/// The `ErrorResponse` that precedes closing after a COPY lost protocol sync.
fn copy_sync_lost<W: Write>(w: &mut W, tag: u8) -> io::Result<()> {
    send_error(
        w,
        "ERROR",
        "08P01",
        &format!("unexpected message type 0x{tag:02X} during COPY from stdin"),
    )?;
    send_fatal(
        w,
        "08P01",
        "terminating connection because protocol synchronization was lost",
    )
}

/// A failed write to the client (`08006`, FATAL): nothing more can be sent.
fn is_connection_lost(e: &Error) -> bool {
    e.severity == Severity::Fatal && e.sqlstate == sqlstate::CONNECTION_FAILURE
}

/// Converts wire format codes (`0` text, `1` binary) to [`Format`].
fn formats(codes: &[i16]) -> Result<Vec<Format>, Error> {
    codes
        .iter()
        .map(|&c| match c {
            0 => Ok(Format::Text),
            1 => Ok(Format::Binary),
            n => Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("unsupported format code: {n}"),
            )),
        })
        .collect()
}

fn write_row_description_or_no_data<W: Write>(
    w: &mut W,
    columns: Option<&[ColumnDesc]>,
) -> io::Result<()> {
    match columns {
        Some(cols) => Sink::new(w).row_description(cols),
        None => write_message(w, &BackendMessage::NoData),
    }
}

/// Runs one Extended Query message (`P B D E C`) and writes the success
/// responses. The outer `Result` is a failed write (close the connection),
/// the inner one an error of the session (`ErrorResponse`, then
/// `SkipTillSync`).
fn handle_ext<W: Write>(
    session: &mut Session,
    msg: FrontendMessage,
    w: &mut W,
) -> io::Result<Result<Next, Error>> {
    macro_rules! core {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            }
        };
    }
    match msg {
        FrontendMessage::Parse {
            name,
            sql,
            param_types,
        } => {
            core!(session.msg_parse(&name, &sql, &param_types));
            write_message(w, &BackendMessage::ParseComplete)?;
        }
        FrontendMessage::Bind {
            portal,
            stmt,
            param_formats,
            params,
            result_formats,
        } => {
            let pf = core!(formats(&param_formats));
            let rf = core!(formats(&result_formats));
            let values: Vec<Option<&[u8]>> = params.iter().map(Option::as_deref).collect();
            core!(session.msg_bind(&portal, &stmt, &pf, &values, &rf));
            write_message(w, &BackendMessage::BindComplete)?;
        }
        FrontendMessage::Describe {
            target: DescribeTarget::Statement,
            name,
        } => {
            let d = core!(session.msg_describe_statement(&name));
            write_message(w, &BackendMessage::ParameterDescription(&d.param_types))?;
            write_row_description_or_no_data(w, d.columns.as_deref())?;
        }
        FrontendMessage::Describe {
            target: DescribeTarget::Portal,
            name,
        } => {
            let columns = core!(session.msg_describe_portal(&name));
            write_row_description_or_no_data(w, columns.as_deref())?;
        }
        FrontendMessage::Execute { portal, max_rows } => {
            let max_rows = u32::try_from(max_rows).unwrap_or(0);
            match core!(session.msg_execute(&portal, max_rows, &mut Sink::new(w))) {
                ExtOutcome::Complete(tag) => {
                    write_message(w, &BackendMessage::CommandComplete(&tag))?;
                }
                ExtOutcome::Suspended => write_message(w, &BackendMessage::PortalSuspended)?,
                ExtOutcome::EmptyQuery => write_message(w, &BackendMessage::EmptyQueryResponse)?,
                ExtOutcome::CopyIn => return Ok(Ok(Next::CopyIn)),
            }
        }
        FrontendMessage::Close { target, name } => {
            match target {
                DescribeTarget::Statement => session.msg_close_statement(&name),
                DescribeTarget::Portal => session.msg_close_portal(&name),
            }
            write_message(w, &BackendMessage::CloseComplete)?;
        }
        other => {
            return Ok(Err(Error::internal(format!(
                "not an Extended Query message: 0x{:02X}",
                frontend_tag(&other)
            ))));
        }
    }
    Ok(Ok(Next::Continue))
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
                format: c.format as i16,
            })
            .collect();
        write_message(self.w, &BackendMessage::RowDescription(&fields))
    }

    fn data_row(&mut self, values: &[Option<String>]) -> io::Result<()> {
        write_message(self.w, &BackendMessage::DataRow(values))
    }

    fn data_row_raw(&mut self, values: &[Option<Vec<u8>>]) -> io::Result<()> {
        write_message(self.w, &BackendMessage::DataRowRaw(values))
    }

    fn copy_out_response(&mut self, format: u8, column_formats: &[u8]) -> io::Result<()> {
        let formats: Vec<i16> = column_formats.iter().map(|&f| i16::from(f)).collect();
        write_message(
            self.w,
            &BackendMessage::CopyOutResponse {
                format,
                column_formats: &formats,
            },
        )
    }

    fn copy_out_data(&mut self, chunk: &[u8]) -> io::Result<()> {
        write_message(self.w, &BackendMessage::CopyData(chunk))
    }

    fn copy_out_done(&mut self) -> io::Result<()> {
        write_message(self.w, &BackendMessage::CopyDone)
    }

    fn command_complete(&mut self, tag: &str) -> io::Result<()> {
        write_message(self.w, &BackendMessage::CommandComplete(tag))
    }

    fn empty_query(&mut self) -> io::Result<()> {
        write_message(self.w, &BackendMessage::EmptyQueryResponse)
    }

    fn error(&mut self, err: &Error) -> io::Result<()> {
        write_core_error(self.w, err, severity_str(err.severity))?;
        // An error reaches the client without Flush or Sync (XQ-D13).
        self.w.flush()
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
                ..ErrorFields::default()
            }),
        )?;
        self.w.flush()
    }

    fn copy_in_response(&mut self, format: u8, column_formats: &[i16]) -> io::Result<()> {
        write_message(
            self.w,
            &BackendMessage::CopyInResponse {
                format,
                column_formats,
            },
        )?;
        // The client waits for `G` before it sends data.
        self.w.flush()
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

#[cfg(test)]
mod sink_tests {
    use super::*;
    use yuzhu_core::sqlstate;

    /// Records the bytes written and when `flush` was called.
    #[derive(Default)]
    struct Recorder {
        bytes: Vec<u8>,
        flushed_at: Vec<usize>,
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed_at.push(self.bytes.len());
            Ok(())
        }
    }

    fn column(format: Format) -> ColumnDesc {
        ColumnDesc {
            name: "a".into(),
            table_oid: 0,
            column_attnum: 0,
            type_oid: 23,
            type_len: 4,
            type_modifier: -1,
            format,
        }
    }

    #[test]
    fn row_description_carries_the_column_format() {
        let mut w = Recorder::default();
        Sink::new(&mut w)
            .row_description(&[column(Format::Text), column(Format::Binary)])
            .unwrap();
        // 'T', len, count, then each field ends with its format code.
        assert_eq!(w.bytes[0], b'T');
        let first_end = 1 + 4 + 2 + (2 + 4 + 2 + 4 + 2 + 4 + 2);
        assert_eq!(&w.bytes[first_end - 2..first_end], [0, 0]);
        assert_eq!(&w.bytes[w.bytes.len() - 2..], [0, 1]);
        assert!(w.flushed_at.is_empty());
    }

    #[test]
    fn data_row_raw_writes_binary_values_and_nulls_without_flushing() {
        let mut w = Recorder::default();
        Sink::new(&mut w)
            .data_row_raw(&[Some(vec![0, 0, 0, 7]), None])
            .unwrap();
        assert_eq!(
            w.bytes,
            [
                b'D', 0, 0, 0, 18, 0, 2, 0, 0, 0, 4, 0, 0, 0, 7, 255, 255, 255, 255
            ]
        );
        assert!(w.flushed_at.is_empty());
    }

    #[test]
    fn error_and_notice_flush_but_rows_do_not() {
        let mut w = Recorder::default();
        let mut sink = Sink::new(&mut w);
        sink.command_complete("SELECT 0").unwrap();
        sink.error(&Error::new(sqlstate::FEATURE_NOT_SUPPORTED, "x"))
            .unwrap();
        let after_error = w.bytes.len();
        let mut sink = Sink::new(&mut w);
        sink.notice(&Notice::new(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            "n",
        ))
        .unwrap();
        assert_eq!(w.flushed_at, [after_error, w.bytes.len()]);
    }

    #[test]
    fn copy_out_messages_are_encoded() {
        let mut w = Recorder::default();
        let mut sink = Sink::new(&mut w);
        sink.copy_out_response(0, &[0, 0]).unwrap();
        sink.copy_out_data(b"1\t2\n").unwrap();
        sink.copy_out_done().unwrap();
        assert_eq!(
            w.bytes,
            [
                b'H', 0, 0, 0, 11, 0, 0, 2, 0, 0, 0, 0, //
                b'd', 0, 0, 0, 8, b'1', b'\t', b'2', b'\n', //
                b'c', 0, 0, 0, 4
            ]
        );
    }

    #[test]
    fn format_codes_are_checked() {
        assert_eq!(
            formats(&[0, 1, 0]).unwrap(),
            [Format::Text, Format::Binary, Format::Text]
        );
        let e = formats(&[0, 2]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "unsupported format code: 2");
    }

    #[test]
    fn connection_lost_is_a_fatal_08006() {
        let e = Error::new(sqlstate::CONNECTION_FAILURE, "x").with_severity(Severity::Fatal);
        assert!(is_connection_lost(&e));
        assert!(!is_connection_lost(&Error::new(
            sqlstate::CONNECTION_FAILURE,
            "x"
        )));
    }
}
