//! Client authentication: message framing, per-method procedures and the
//! startup deadline (`m5/08` §3.5, §4.3, §5.2 to §5.5).
//!
//! Nothing here reads the normal protocol messages; until authentication ends the
//! `p` messages are read directly from the stream with small length limits.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use yuzhu_auth::AuthError;
use yuzhu_auth::scram::{self, MECHANISM, MOCK_NONCE_LEN, ServerExchange};
use yuzhu_core::Cluster;
use yuzhu_core::engine::RoleAuthInfo;
use yuzhu_core::util::clock::{Clock, SystemClock};

use crate::hba::HbaMethod;

pub const MAX_SASL_MESSAGE_LEN: usize = 1024;
pub const MAX_PASSWORD_MESSAGE_LEN: usize = 65535;

/// What authentication needs from the cluster. Tests replace it.
pub trait RoleDirectory {
    fn lookup_role(&self, user: &str) -> yuzhu_core::Result<Option<RoleAuthInfo>>;
    fn mock_auth_nonce(&self) -> [u8; MOCK_NONCE_LEN];
    /// Microseconds since the UNIX epoch.
    fn now_unix_micros(&self) -> i64;
}

impl RoleDirectory for Cluster {
    fn lookup_role(&self, user: &str) -> yuzhu_core::Result<Option<RoleAuthInfo>> {
        Cluster::lookup_role(self, user)
    }
    fn mock_auth_nonce(&self) -> [u8; MOCK_NONCE_LEN] {
        self.control().get().mock_auth_nonce
    }
    fn now_unix_micros(&self) -> i64 {
        SystemClock.unix_micros()
    }
}

/// A cluster together with the clock the server uses (tests inject a `ManualClock`).
#[derive(Debug)]
pub struct ClusterDirectory<'a> {
    pub cluster: &'a Cluster,
    pub clock: &'a dyn Clock,
}

impl RoleDirectory for ClusterDirectory<'_> {
    fn lookup_role(&self, user: &str) -> yuzhu_core::Result<Option<RoleAuthInfo>> {
        self.cluster.lookup_role(user)
    }
    fn mock_auth_nonce(&self) -> [u8; MOCK_NONCE_LEN] {
        self.cluster.control().get().mock_auth_nonce
    }
    fn now_unix_micros(&self) -> i64 {
        self.clock.unix_micros()
    }
}

pub struct AuthContext<'a> {
    pub directory: &'a dyn RoleDirectory,
    pub user: &'a str,
    pub database: &'a str,
    pub client_addr: IpAddr,
}

impl std::fmt::Debug for AuthContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthContext")
            .field("user", &self.user)
            .field("database", &self.database)
            .field("client_addr", &self.client_addr)
            .finish_non_exhaustive()
    }
}

/// A FATAL for the client. `log_detail` goes to the server log only.
#[derive(Debug)]
pub struct FatalError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub log_detail: Option<String>,
}

impl FatalError {
    pub fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            sqlstate,
            message: message.into(),
            detail: None,
            log_detail: None,
        }
    }
}

#[derive(Debug)]
pub enum AuthFailure {
    /// Send the FATAL and close.
    Fatal(FatalError),
    /// Read / write failure (including a disconnect). Send nothing.
    Io(io::Error),
    /// The overall deadline passed.
    Timeout,
}

fn fatal(sqlstate: &'static str, message: impl Into<String>) -> AuthFailure {
    AuthFailure::Fatal(FatalError::new(sqlstate, message))
}

impl From<io::Error> for AuthFailure {
    fn from(e: io::Error) -> Self {
        if is_timeout(&e) {
            AuthFailure::Timeout
        } else {
            AuthFailure::Io(e)
        }
    }
}

/// Whether an I/O error is a socket timeout.
pub fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// The overall limit for startup and authentication (`m5/08` §5.5).
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    until: Instant,
}

impl Deadline {
    pub fn new(total: Duration) -> Self {
        Self {
            until: Instant::now() + total,
        }
    }

    /// Time left, or `None` when it has passed.
    pub fn remaining(&self) -> Option<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    }
}

/// Enforces the overall startup deadline against a client that trickles bytes: when the time is
/// up it shuts down the read side of the socket, so a blocked read returns. `cancel` stops it.
#[derive(Debug)]
pub struct Watchdog {
    state: Arc<(Mutex<WdState>, Condvar)>,
}

#[derive(Debug, Default)]
struct WdState {
    cancelled: bool,
    fired: bool,
}

impl Watchdog {
    pub fn start(stream: TcpStream, total: Duration) -> io::Result<Self> {
        let state: Arc<(Mutex<WdState>, Condvar)> = Arc::default();
        let shared = Arc::clone(&state);
        std::thread::Builder::new()
            .name("auth-watchdog".into())
            .stack_size(64 << 10)
            .spawn(move || {
                let (lock, cv) = &*shared;
                let Ok(guard) = lock.lock() else { return };
                let Ok((mut st, _)) = cv.wait_timeout_while(guard, total, |s| !s.cancelled) else {
                    return;
                };
                if !st.cancelled {
                    st.fired = true;
                    let _ = stream.shutdown(Shutdown::Read);
                }
            })?;
        Ok(Self { state })
    }

    /// Whether the deadline passed and the read side was shut down.
    pub fn fired(&self) -> bool {
        self.state.0.lock().is_ok_and(|s| s.fired)
    }

    /// Stops the watchdog. Returns `false` if it had already fired.
    pub fn cancel(&self) -> bool {
        let Ok(mut st) = self.state.0.lock() else {
            return false;
        };
        if st.fired {
            return false;
        }
        st.cancelled = true;
        self.state.1.notify_all();
        true
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Server to client authentication requests.
#[derive(Debug, Clone, Copy)]
pub enum AuthRequest<'a> {
    CleartextPassword,
    Sasl(&'a [&'a str]),
    SaslContinue(&'a [u8]),
    SaslFinal(&'a [u8]),
}

pub fn encode_auth_request(req: &AuthRequest<'_>) -> Vec<u8> {
    let mut body = Vec::new();
    match req {
        AuthRequest::CleartextPassword => body.extend_from_slice(&3i32.to_be_bytes()),
        AuthRequest::Sasl(mechs) => {
            body.extend_from_slice(&10i32.to_be_bytes());
            for m in *mechs {
                body.extend_from_slice(m.as_bytes());
                body.push(0);
            }
            body.push(0);
        }
        AuthRequest::SaslContinue(data) => {
            body.extend_from_slice(&11i32.to_be_bytes());
            body.extend_from_slice(data);
        }
        AuthRequest::SaslFinal(data) => {
            body.extend_from_slice(&12i32.to_be_bytes());
            body.extend_from_slice(data);
        }
    }
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(b'R');
    out.extend_from_slice(
        &u32::try_from(body.len() + 4)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&body);
    out
}

/// Reads one `p` message. A wrong type byte, a length below 4 or a body larger than `limit`
/// is a 08P01 FATAL; an oversized body is not read.
pub fn read_p_message<R: Read>(
    r: &mut R,
    limit: usize,
    expect: &'static str,
) -> Result<Vec<u8>, AuthFailure> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    if tag[0] != b'p' {
        return Err(fatal(
            "08P01",
            format!("expected {expect}, got message type {}", tag[0]),
        ));
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = i64::from(i32::from_be_bytes(len));
    let invalid = || fatal("08P01", "invalid message length");
    if len < 4 {
        return Err(invalid());
    }
    let body_len = usize::try_from(len - 4).map_err(|_| invalid())?;
    if body_len > limit {
        return Err(invalid());
    }
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body)?;
    Ok(body)
}

pub fn no_hba_entry(addr: IpAddr, user: &str, database: &str) -> FatalError {
    FatalError::new(
        "28000",
        format!(
            "no pg_hba.conf entry for host \"{addr}\", user \"{user}\", database \"{database}\", no encryption"
        ),
    )
}

fn password_failed(user: &str, log: String) -> AuthFailure {
    AuthFailure::Fatal(FatalError {
        sqlstate: "28P01",
        message: format!("password authentication failed for user \"{user}\""),
        detail: None,
        log_detail: Some(log),
    })
}

fn map_auth_error(e: AuthError, user: &str, fail_log: String) -> AuthFailure {
    match e {
        AuthError::Failed => password_failed(user, fail_log),
        AuthError::Protocol { message, detail } => AuthFailure::Fatal(FatalError {
            sqlstate: "08P01",
            message: message.to_owned(),
            detail,
            log_detail: None,
        }),
        AuthError::Unsupported(m) => fatal("0A000", m),
        AuthError::InvalidParameter(m) => fatal("22023", m),
        AuthError::Random(_) => fatal("XX000", "could not generate random numbers"),
    }
}

fn core_fatal(e: &yuzhu_core::Error) -> AuthFailure {
    AuthFailure::Fatal(FatalError::new(e.sqlstate.0, e.message.clone()))
}

fn write_flush<W: Write>(w: &mut W, req: &AuthRequest<'_>) -> Result<(), AuthFailure> {
    w.write_all(&encode_auth_request(req))?;
    w.flush()?;
    Ok(())
}

/// The stored secret if the role may log in with a password right now, plus the reason it
/// cannot (for the server log).
fn usable_secret(
    role: Option<&RoleAuthInfo>,
    user: &str,
    now: i64,
) -> (Option<String>, Option<String>) {
    let Some(role) = role else {
        return (None, Some(format!("Role \"{user}\" does not exist.")));
    };
    let Some(pw) = &role.password else {
        return (
            None,
            Some(format!("User \"{user}\" has no password assigned.")),
        );
    };
    if role.valid_until.is_some_and(|v| v < now) {
        return (
            None,
            Some(format!("User \"{user}\" has an expired password.")),
        );
    }
    if !scram::is_scram_secret(pw) {
        return (
            None,
            Some(format!(
                "User \"{user}\" does not have a valid SCRAM secret."
            )),
        );
    }
    (Some(pw.clone()), None)
}

/// Authenticates one connection by `method`. On success `AuthenticationOk` is not written yet.
pub fn authenticate<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    method: HbaMethod,
    ctx: &AuthContext<'_>,
    tick: &mut dyn FnMut() -> Result<(), AuthFailure>,
) -> Result<(), AuthFailure> {
    match method {
        HbaMethod::Trust => Ok(()),
        HbaMethod::Reject => Err(fatal(
            "28000",
            format!(
                "pg_hba.conf rejects connection for host \"{}\", user \"{}\", database \"{}\", no encryption",
                ctx.client_addr, ctx.user, ctx.database
            ),
        )),
        HbaMethod::Password => authenticate_password(r, w, ctx, tick),
        HbaMethod::ScramSha256 => authenticate_scram(r, w, ctx, tick, false),
    }
}

/// Like [`authenticate`] for SCRAM, with the mutation-test switch that skips the proof check.
pub fn authenticate_scram<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    ctx: &AuthContext<'_>,
    tick: &mut dyn FnMut() -> Result<(), AuthFailure>,
    skip_proof_check: bool,
) -> Result<(), AuthFailure> {
    // `skip_proof_check` needs the `yuzhu-auth` hook (not available); see the report.
    let _ = skip_proof_check;
    let role = ctx
        .directory
        .lookup_role(ctx.user)
        .map_err(|e| core_fatal(&e))?;
    let now = ctx.directory.now_unix_micros();
    let (secret, reason) = usable_secret(role.as_ref(), ctx.user, now);
    let mut ex = ServerExchange::new_with_mock_nonce(
        ctx.user,
        secret.as_deref(),
        &ctx.directory.mock_auth_nonce(),
    )
    .map_err(|e| map_auth_error(e, ctx.user, String::new()))?;
    let mismatch = format!("Password does not match for user \"{}\".", ctx.user);
    let fail_log = reason.unwrap_or(mismatch);

    tick()?;
    write_flush(w, &AuthRequest::Sasl(&[MECHANISM]))?;
    tick()?;
    let msg = read_p_message(r, MAX_SASL_MESSAGE_LEN, "SASL response")?;

    let Some(nul) = msg.iter().position(|b| *b == 0) else {
        return Err(fatal(
            "08P01",
            "client selected an invalid SASL authentication mechanism",
        ));
    };
    if &msg[..nul] != MECHANISM.as_bytes() {
        return Err(fatal(
            "08P01",
            "client selected an invalid SASL authentication mechanism",
        ));
    }
    let rest = &msg[nul + 1..];
    let malformed = || fatal("08P01", "malformed SASLInitialResponse");
    let Some((n_bytes, initial)) = rest.split_first_chunk::<4>() else {
        return Err(malformed());
    };
    let n = i32::from_be_bytes(*n_bytes);
    let client_first: &[u8] = if n == -1 {
        &[]
    } else if n >= 0 && usize::try_from(n).ok() == Some(initial.len()) {
        initial
    } else {
        return Err(malformed());
    };
    let server_first = ex
        .client_first(client_first)
        .map_err(|e| map_auth_error(e, ctx.user, fail_log.clone()))?;
    tick()?;
    write_flush(w, &AuthRequest::SaslContinue(&server_first))?;
    tick()?;
    let msg2 = read_p_message(r, MAX_SASL_MESSAGE_LEN, "SASL response")?;
    let server_final = ex
        .client_final(&msg2)
        .map_err(|e| map_auth_error(e, ctx.user, fail_log))?;
    tick()?;
    write_flush(w, &AuthRequest::SaslFinal(&server_final))
}

fn authenticate_password<R: Read, W: Write>(
    r: &mut R,
    w: &mut W,
    ctx: &AuthContext<'_>,
    tick: &mut dyn FnMut() -> Result<(), AuthFailure>,
) -> Result<(), AuthFailure> {
    let role = ctx
        .directory
        .lookup_role(ctx.user)
        .map_err(|e| core_fatal(&e))?;
    let now = ctx.directory.now_unix_micros();
    tick()?;
    write_flush(w, &AuthRequest::CleartextPassword)?;
    tick()?;
    let msg = read_p_message(r, MAX_PASSWORD_MESSAGE_LEN, "password response")?;
    let invalid = || fatal("08P01", "invalid password packet size");
    let Some((&0, body)) = msg.split_last() else {
        return Err(invalid());
    };
    if body.contains(&0) {
        return Err(invalid());
    }
    if body.is_empty() {
        return Err(fatal("28P01", "empty password returned by client"));
    }
    let (secret, reason) = usable_secret(role.as_ref(), ctx.user, now);
    let ok = match (std::str::from_utf8(body), &secret) {
        (Ok(pw), Some(s)) => scram::verify_plain_password(pw, s),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(password_failed(
            ctx.user,
            reason.unwrap_or_else(|| format!("Password does not match for user \"{}\".", ctx.user)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Fake {
        role: Option<RoleAuthInfo>,
        now: i64,
    }

    impl RoleDirectory for Fake {
        fn lookup_role(&self, _user: &str) -> yuzhu_core::Result<Option<RoleAuthInfo>> {
            Ok(self.role.clone())
        }
        fn mock_auth_nonce(&self) -> [u8; MOCK_NONCE_LEN] {
            [9; MOCK_NONCE_LEN]
        }
        fn now_unix_micros(&self) -> i64 {
            self.now
        }
    }

    fn role(password: Option<&str>, valid_until: Option<i64>) -> RoleAuthInfo {
        RoleAuthInfo {
            oid: 16384,
            name: "alice".into(),
            password: password.map(str::to_owned),
            valid_until,
            can_login: true,
            conn_limit: -1,
            superuser: false,
        }
    }

    fn run(method: HbaMethod, fake: &Fake, input: Vec<u8>) -> (Result<(), AuthFailure>, Vec<u8>) {
        let ctx = AuthContext {
            directory: fake,
            user: "alice",
            database: "db",
            client_addr: "127.0.0.1".parse().unwrap(),
        };
        let mut r = Cursor::new(input);
        let mut w = Vec::new();
        let res = authenticate(&mut r, &mut w, method, &ctx, &mut || Ok(()));
        (res, w)
    }

    fn p_msg(body: &[u8]) -> Vec<u8> {
        let mut v = vec![b'p'];
        v.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    fn code(res: Result<(), AuthFailure>) -> (&'static str, String) {
        match res {
            Err(AuthFailure::Fatal(f)) => (f.sqlstate, f.message),
            other => panic!("expected a FATAL, got {other:?}"),
        }
    }

    #[test]
    fn encodes_requests() {
        assert_eq!(
            encode_auth_request(&AuthRequest::CleartextPassword),
            [b'R', 0, 0, 0, 8, 0, 0, 0, 3]
        );
        let sasl = encode_auth_request(&AuthRequest::Sasl(&["SCRAM-SHA-256"]));
        assert_eq!(&sasl[..9], &[b'R', 0, 0, 0, 23, 0, 0, 0, 10]);
        assert_eq!(&sasl[9..], b"SCRAM-SHA-256\0\0");
        let c = encode_auth_request(&AuthRequest::SaslContinue(b"r=x"));
        assert_eq!(c, [b'R', 0, 0, 0, 11, 0, 0, 0, 11, b'r', b'=', b'x']);
        let f = encode_auth_request(&AuthRequest::SaslFinal(b"v=y"));
        assert_eq!(f[8], 12);
    }

    #[test]
    fn p_message_limits() {
        let mut ok = Cursor::new(p_msg(b"abc"));
        assert_eq!(read_p_message(&mut ok, 10, "x").unwrap(), b"abc");
        let mut wrong = Cursor::new(vec![b'Q', 0, 0, 0, 4]);
        let (c, m) = code(read_p_message(&mut wrong, 10, "SASL response").map(|_| ()));
        assert_eq!(
            (c, m.as_str()),
            ("08P01", "expected SASL response, got message type 81")
        );
        // Oversized: the body is not read (the input has no body at all).
        let mut big = Cursor::new(vec![b'p', 0, 0, 4, 5]);
        let (c, m) = code(read_p_message(&mut big, 1024, "x").map(|_| ()));
        assert_eq!((c, m.as_str()), ("08P01", "invalid message length"));
        let mut small = Cursor::new(vec![b'p', 0, 0, 0, 3]);
        assert!(matches!(
            read_p_message(&mut small, 1024, "x"),
            Err(AuthFailure::Fatal(_))
        ));
        let mut neg = Cursor::new(vec![b'p', 0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(
            read_p_message(&mut neg, 1024, "x"),
            Err(AuthFailure::Fatal(_))
        ));
        let mut eof = Cursor::new(Vec::new());
        assert!(matches!(
            read_p_message(&mut eof, 10, "x"),
            Err(AuthFailure::Io(_))
        ));
    }

    #[test]
    fn trust_and_reject() {
        let fake = Fake { role: None, now: 0 };
        let (res, out) = run(HbaMethod::Trust, &fake, Vec::new());
        assert!(res.is_ok() && out.is_empty());
        let (c, m) = code(run(HbaMethod::Reject, &fake, Vec::new()).0);
        assert_eq!(c, "28000");
        assert_eq!(
            m,
            "pg_hba.conf rejects connection for host \"127.0.0.1\", user \"alice\", database \"db\", no encryption"
        );
    }

    #[test]
    fn password_method() {
        let secret = scram::make_secret("pencil", 4).unwrap();
        let fake = Fake {
            role: Some(role(Some(&secret), None)),
            now: 10,
        };
        let (res, out) = run(HbaMethod::Password, &fake, p_msg(b"pencil\0"));
        assert!(res.is_ok());
        assert_eq!(out, encode_auth_request(&AuthRequest::CleartextPassword));
        let (c, m) = code(run(HbaMethod::Password, &fake, p_msg(b"nope\0")).0);
        assert_eq!(
            (c, m.as_str()),
            ("28P01", "password authentication failed for user \"alice\"")
        );
        let (c, m) = code(run(HbaMethod::Password, &fake, p_msg(b"\0")).0);
        assert_eq!(
            (c, m.as_str()),
            ("28P01", "empty password returned by client")
        );
        for bad in [&b"abc"[..], b"a\0b\0", b""] {
            let (c, m) = code(run(HbaMethod::Password, &fake, p_msg(bad)).0);
            assert_eq!((c, m.as_str()), ("08P01", "invalid password packet size"));
        }
    }

    #[test]
    fn password_method_fails_without_usable_secret() {
        let secret = scram::make_secret("pencil", 4).unwrap();
        let cases = [
            None,
            Some(role(None, None)),
            Some(role(Some(&secret), Some(5))),
            Some(role(Some("plaintext"), None)),
        ];
        for r in cases {
            let fake = Fake { role: r, now: 10 };
            let (c, m) = code(run(HbaMethod::Password, &fake, p_msg(b"pencil\0")).0);
            assert_eq!(
                (c, m.as_str()),
                ("28P01", "password authentication failed for user \"alice\"")
            );
        }
        // `VALID UNTIL` in the future still works.
        let fake = Fake {
            role: Some(role(Some(&secret), Some(11))),
            now: 10,
        };
        assert!(
            run(HbaMethod::Password, &fake, p_msg(b"pencil\0"))
                .0
                .is_ok()
        );
    }

    #[test]
    fn scram_protocol_errors() {
        let fake = Fake { role: None, now: 0 };
        // Wrong mechanism.
        let (c, m) = code(
            run(
                HbaMethod::ScramSha256,
                &fake,
                p_msg(b"SCRAM-SHA-1\0\0\0\0\0"),
            )
            .0,
        );
        assert_eq!(
            (c, m.as_str()),
            (
                "08P01",
                "client selected an invalid SASL authentication mechanism"
            )
        );
        // Length that does not match the rest.
        let mut body = b"SCRAM-SHA-256\0".to_vec();
        body.extend_from_slice(&5i32.to_be_bytes());
        body.extend_from_slice(b"n,,");
        let (c, m) = code(run(HbaMethod::ScramSha256, &fake, p_msg(&body)).0);
        assert_eq!((c, m.as_str()), ("08P01", "malformed SASLInitialResponse"));
        // No initial response.
        let mut body = b"SCRAM-SHA-256\0".to_vec();
        body.extend_from_slice(&(-1i32).to_be_bytes());
        let (c, m) = code(run(HbaMethod::ScramSha256, &fake, p_msg(&body)).0);
        assert_eq!((c, m.as_str()), ("08P01", "malformed SCRAM message"));
    }

    #[test]
    fn deadline_expires() {
        let d = Deadline::new(Duration::from_millis(20));
        assert!(d.remaining().is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(d.remaining().is_none());
    }
}
