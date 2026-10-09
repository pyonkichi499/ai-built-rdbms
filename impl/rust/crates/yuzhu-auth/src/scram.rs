//! SCRAM-SHA-256 (RFC 5802 / RFC 7677): secrets and the server-side exchange.

use std::fmt;
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit, Mac};
use pbkdf2::pbkdf2_hmac_array;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{AuthError, random, saslprep};

pub const MECHANISM: &str = "SCRAM-SHA-256";
pub const DEFAULT_ITERATIONS: u32 = 4096;
/// Salt length of newly created secrets.
pub const SALT_LEN: usize = 16;
/// Random bytes of the server nonce (24 base64 characters).
pub const SERVER_NONCE_RAW_LEN: usize = 18;
pub const KEY_LEN: usize = 32;
pub const MOCK_NONCE_LEN: usize = 32;
pub const MAX_ITERATIONS: u32 = i32::MAX as u32;

const PREFIX: &str = "SCRAM-SHA-256$";

pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; KEY_LEN] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

pub(crate) fn sha256(data: &[u8]) -> [u8; KEY_LEN] {
    Sha256::digest(data).into()
}

pub(crate) fn xor32(a: &[u8; KEY_LEN], b: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    for (o, (x, y)) in out.iter_mut().zip(a.iter().zip(b)) {
        *o = x ^ y;
    }
    out
}

/// `SaltedPassword = Hi(SASLprep(password), salt, iterations)`.
pub(crate) fn salted_password(password: &str, salt: &[u8], iterations: u32) -> [u8; KEY_LEN] {
    let pw = saslprep::prepare(password);
    pbkdf2_hmac_array::<Sha256, KEY_LEN>(pw.as_bytes(), salt, iterations)
}

/// `"SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>"`.
///
/// `iterations` is `i32` because PostgreSQL accepts any `i32` (including 0 and
/// negative values) as a well-formed secret; such secrets cannot authenticate.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramSecret {
    pub iterations: i32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; KEY_LEN],
    pub server_key: [u8; KEY_LEN],
}

impl ScramSecret {
    /// Strict parse of the stored format. `None` if it is not a SCRAM secret.
    pub fn parse(s: &str) -> Option<ScramSecret> {
        let rest = s.strip_prefix(PREFIX)?;
        let (iter_s, rest) = rest.split_once(':')?;
        let iterations = parse_strtol_i32(iter_s)?;
        let (salt_s, rest) = rest.split_once('$')?;
        let (stored_s, server_s) = rest.split_once(':')?;
        let salt = B64.decode(salt_s).ok()?;
        let stored_key: [u8; KEY_LEN] = B64.decode(stored_s).ok()?.try_into().ok()?;
        let server_key: [u8; KEY_LEN] = B64.decode(server_s).ok()?.try_into().ok()?;
        Some(ScramSecret {
            iterations,
            salt,
            stored_key,
            server_key,
        })
    }

    /// Inverse of [`parse`](Self::parse).
    pub fn encode(&self) -> String {
        format!(
            "{PREFIX}{}:{}${}:{}",
            self.iterations,
            B64.encode(&self.salt),
            B64.encode(self.stored_key),
            B64.encode(self.server_key)
        )
    }

    /// Derive a secret. `password` is `SASLprep`'d. `iterations` must be 1..=`MAX_ITERATIONS`
    /// (checked by [`make_secret_with_salt`]; larger values are clamped here).
    pub fn derive(password: &str, salt: &[u8], iterations: u32) -> ScramSecret {
        let iterations = iterations.min(MAX_ITERATIONS);
        let salted = salted_password(password, salt, iterations);
        let client_key = hmac_sha256(&salted, &[b"Client Key"]);
        ScramSecret {
            iterations: i32::try_from(iterations).unwrap_or(i32::MAX),
            salt: salt.to_vec(),
            stored_key: sha256(&client_key),
            server_key: hmac_sha256(&salted, &[b"Server Key"]),
        }
    }

    /// Iteration count usable for authentication (`>= 1`).
    fn usable_iterations(&self) -> Option<u32> {
        u32::try_from(self.iterations).ok().filter(|&i| i >= 1)
    }
}

impl fmt::Debug for ScramSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScramSecret")
            .field("iterations", &self.iterations)
            .field("salt_len", &self.salt.len())
            .finish_non_exhaustive()
    }
}

/// C `strtol` base 10 semantics for the whole string: leading whitespace, optional sign,
/// at least one digit, nothing after. Must fit in `i32`.
fn parse_strtol_i32(s: &str) -> Option<i32> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match t.as_bytes().first()? {
        b'+' => (false, &t[1..]),
        b'-' => (true, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut v: i64 = 0;
    for b in digits.bytes() {
        v = v.saturating_mul(10).saturating_add(i64::from(b - b'0'));
        if v > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    i32::try_from(if neg { -v } else { v }).ok()
}

/// Create a stored secret with a random 16-byte salt.
pub fn make_secret(password: &str, iterations: u32) -> Result<String, AuthError> {
    check_iterations(iterations)?;
    let salt = random::bytes::<SALT_LEN>().map_err(|e| AuthError::Random(e.to_string()))?;
    make_secret_with_salt(password, &salt, iterations)
}

/// Create a stored secret with a given salt (tests, reproduction of PostgreSQL output).
pub fn make_secret_with_salt(
    password: &str,
    salt: &[u8],
    iterations: u32,
) -> Result<String, AuthError> {
    check_iterations(iterations)?;
    Ok(ScramSecret::derive(password, salt, iterations).encode())
}

fn check_iterations(iterations: u32) -> Result<(), AuthError> {
    if iterations == 0 || iterations > MAX_ITERATIONS {
        return Err(AuthError::InvalidParameter(format!(
            "SCRAM iteration count {iterations} is out of range (1..={MAX_ITERATIONS})"
        )));
    }
    Ok(())
}

pub fn is_scram_secret(s: &str) -> bool {
    ScramSecret::parse(s).is_some()
}

/// Verify a plaintext password against a stored secret. `false` if `secret` is not SCRAM.
pub fn verify_plain_password(password: &str, secret: &str) -> bool {
    let Some(sec) = ScramSecret::parse(secret) else {
        return false;
    };
    let Some(iter) = sec.usable_iterations() else {
        return false;
    };
    let derived = ScramSecret::derive(password, &sec.salt, iter);
    derived.stored_key.ct_eq(&sec.stored_key).into()
}

/// Fake secret for unknown users: stable salt derived from the user name and a cluster nonce.
pub fn mock_secret(username: &str, mock_nonce: &[u8; MOCK_NONCE_LEN]) -> ScramSecret {
    let mut h = Sha256::new();
    h.update(username.as_bytes());
    h.update(mock_nonce);
    let digest = h.finalize();
    ScramSecret {
        iterations: DEFAULT_ITERATIONS.cast_signed(),
        salt: digest[..SALT_LEN].to_vec(),
        stored_key: [0; KEY_LEN],
        server_key: [0; KEY_LEN],
    }
}

fn process_mock_nonce() -> Result<[u8; MOCK_NONCE_LEN], AuthError> {
    static MOCK: OnceLock<[u8; MOCK_NONCE_LEN]> = OnceLock::new();
    if let Some(n) = MOCK.get() {
        return Ok(*n);
    }
    let n = random::bytes::<MOCK_NONCE_LEN>().map_err(|e| AuthError::Random(e.to_string()))?;
    Ok(*MOCK.get_or_init(|| n))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Init,
    SaltSent,
    Done,
}

/// Server side of one SCRAM-SHA-256 exchange.
pub struct ServerExchange {
    state: State,
    salt: Vec<u8>,
    iterations: u32,
    stored_key: [u8; KEY_LEN],
    server_key: [u8; KEY_LEN],
    doomed: bool,
    gs2_header: String,
    client_first_bare: String,
    client_nonce: String,
    server_nonce: String,
    server_first: String,
}

impl fmt::Debug for ServerExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerExchange")
            .field("state", &self.state)
            .field("doomed", &self.doomed)
            .finish_non_exhaustive()
    }
}

fn malformed(detail: impl Into<String>) -> AuthError {
    AuthError::protocol("malformed SCRAM message", detail)
}

/// Read `<expect>=<value>` from the front of `input`, consuming a trailing comma.
fn read_attr<'a>(input: &mut &'a str, expect: char) -> Result<&'a str, AuthError> {
    let mut chars = input.chars();
    match chars.next() {
        None => {
            return Err(malformed(format!(
                "Expected attribute \"{expect}\" but found end of message."
            )));
        }
        Some(c) if c != expect => {
            return Err(malformed(format!(
                "Expected attribute \"{expect}\" but found \"{c}\"."
            )));
        }
        Some(_) => {}
    }
    if chars.next() != Some('=') {
        return Err(malformed(format!(
            "Expected character \"=\" for attribute \"{expect}\"."
        )));
    }
    let body = &input[2..];
    let (value, rest) = body.split_once(',').unwrap_or((body, ""));
    *input = rest;
    Ok(value)
}

fn is_scram_printable(s: &str) -> bool {
    s.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b',')
}

impl ServerExchange {
    /// Uses a per-process random mock nonce for unknown users (tests; the server uses
    /// [`new_with_mock_nonce`](Self::new_with_mock_nonce)).
    pub fn new(username: &str, secret: Option<&str>) -> Result<Self, AuthError> {
        let nonce = process_mock_nonce()?;
        Self::new_with_mock_nonce(username, secret, &nonce)
    }

    /// If `secret` is `None` or unusable, proceeds with a fake secret and fails at the end.
    pub fn new_with_mock_nonce(
        username: &str,
        secret: Option<&str>,
        mock_nonce: &[u8; MOCK_NONCE_LEN],
    ) -> Result<Self, AuthError> {
        let real = secret
            .and_then(ScramSecret::parse)
            .and_then(|s| s.usable_iterations().map(|i| (s, i)));
        let (sec, iterations, doomed) = match real {
            Some((s, i)) => (s, i, false),
            None => (mock_secret(username, mock_nonce), DEFAULT_ITERATIONS, true),
        };
        let raw = random::bytes::<SERVER_NONCE_RAW_LEN>()
            .map_err(|e| AuthError::Random(e.to_string()))?;
        Ok(ServerExchange {
            state: State::Init,
            salt: sec.salt,
            iterations,
            stored_key: sec.stored_key,
            server_key: sec.server_key,
            doomed,
            gs2_header: String::new(),
            client_first_bare: String::new(),
            client_nonce: String::new(),
            server_nonce: B64.encode(raw),
            server_first: String::new(),
        })
    }

    /// Exchange runs on a fake secret and will end in `Failed`.
    pub fn is_doomed(&self) -> bool {
        self.doomed
    }

    /// Handle client-first-message; returns server-first-message.
    pub fn client_first(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError> {
        if self.state != State::Init {
            return Err(AuthError::Protocol {
                message: "unexpected SCRAM message",
                detail: None,
            });
        }
        let msg = decode_message(msg)?;
        if msg.is_empty() {
            return Err(malformed("The message is empty."));
        }
        let mut chars = msg.chars();
        match chars.next() {
            Some('n' | 'y') => {}
            Some('p') => {
                return Err(malformed(
                    "The client selected SCRAM-SHA-256 without channel binding, but the SCRAM message includes channel binding data.",
                ));
            }
            Some(c) => {
                return Err(malformed(format!(
                    "Unexpected channel-binding flag \"{c}\"."
                )));
            }
            None => unreachable!("checked non-empty"),
        }
        match chars.next() {
            Some(',') => {}
            Some(c) => {
                return Err(malformed(format!(
                    "Comma expected, but found character \"{c}\"."
                )));
            }
            None => return Err(malformed("Comma expected, but found end of message.")),
        }
        match chars.next() {
            Some('a') => {
                return Err(AuthError::Unsupported(
                    "client uses authorization identity, but it is not supported",
                ));
            }
            Some(',') => {}
            Some(c) => {
                return Err(malformed(format!(
                    "Unexpected attribute \"{c}\" in client-first-message."
                )));
            }
            None => return Err(malformed("Unexpected end of client-first-message.")),
        }
        let (gs2, bare) = msg.split_at(3);
        if bare.starts_with('m') {
            return Err(AuthError::Unsupported(
                "client requires an unsupported SCRAM extension",
            ));
        }
        let mut input = bare;
        read_attr(&mut input, 'n')?;
        let nonce = read_attr(&mut input, 'r')?;
        if nonce.is_empty() {
            return Err(malformed(
                "Expected attribute \"r\" but found end of message.",
            ));
        }
        if !is_scram_printable(nonce) {
            return Err(AuthError::protocol(
                "non-printable characters in SCRAM nonce",
                "The client nonce contains characters outside 0x21-0x7E.",
            ));
        }
        gs2.clone_into(&mut self.gs2_header);
        bare.clone_into(&mut self.client_first_bare);
        nonce.clone_into(&mut self.client_nonce);
        self.server_first = format!(
            "r={}{},s={},i={}",
            self.client_nonce,
            self.server_nonce,
            B64.encode(&self.salt),
            self.iterations
        );
        self.state = State::SaltSent;
        Ok(self.server_first.clone().into_bytes())
    }

    /// Handle client-final-message; returns server-final-message or `Err(Failed)`.
    pub fn client_final(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError> {
        if self.state != State::SaltSent {
            return Err(AuthError::Protocol {
                message: "unexpected SCRAM message",
                detail: None,
            });
        }
        self.state = State::Done;
        let msg = decode_message(msg)?;
        let Some(idx) = msg.rfind(",p=") else {
            return Err(malformed(
                "Expected attribute \"p\" but found end of message.",
            ));
        };
        let without_proof = &msg[..idx];
        let proof_b64 = &msg[idx + 3..];
        if proof_b64.contains(',') {
            return Err(malformed(
                "Garbage found at the end of client-final-message.",
            ));
        }
        let mut input = without_proof;
        let channel = read_attr(&mut input, 'c')?;
        let nonce = read_attr(&mut input, 'r')?;
        let proof: [u8; KEY_LEN] = B64
            .decode(proof_b64)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| malformed("Malformed proof in client-final-message."))?;
        if channel != B64.encode(&self.gs2_header) {
            return Err(AuthError::protocol(
                "unexpected SCRAM channel-binding attribute in client-final-message",
                "The channel-binding data does not match the client-first-message header.",
            ));
        }
        if nonce.strip_prefix(self.client_nonce.as_str()) != Some(self.server_nonce.as_str()) {
            return Err(AuthError::protocol(
                "invalid SCRAM response",
                "Nonce does not match.",
            ));
        }

        let auth_message = [
            self.client_first_bare.as_bytes(),
            b",",
            self.server_first.as_bytes(),
            b",",
            without_proof.as_bytes(),
        ];
        let client_signature = hmac_sha256(&self.stored_key, &auth_message);
        let client_key = xor32(&proof, &client_signature);
        let matches: bool = sha256(&client_key).ct_eq(&self.stored_key).into();
        // Always compute the server signature too so that doomed exchanges cost the same.
        let server_signature = hmac_sha256(&self.server_key, &auth_message);
        if !matches || self.doomed {
            return Err(AuthError::Failed);
        }
        Ok(format!("v={}", B64.encode(server_signature)).into_bytes())
    }

    #[cfg(test)]
    pub(crate) fn set_server_nonce_for_test(&mut self, nonce: &str) {
        self.server_nonce = nonce.to_owned();
    }
}

fn decode_message(msg: &[u8]) -> Result<&str, AuthError> {
    let s = std::str::from_utf8(msg).map_err(|_| malformed("The message is not valid UTF-8."))?;
    if s.contains('\0') {
        return Err(malformed("The message contains a NUL byte."));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scram_client::ClientExchange;
    use proptest::prelude::*;

    const RFC_SALT: [u8; 16] = [
        0x5b, 0x6d, 0x99, 0x68, 0x9d, 0x12, 0x35, 0x8e, 0xec, 0xa0, 0x4b, 0x14, 0x12, 0x36, 0xfa,
        0x81,
    ];
    const RFC_SECRET: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    const RFC_SERVER_NONCE: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const RFC_C1: &str = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const RFC_S1: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const RFC_C2: &str = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    const RFC_S2: &str = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";

    fn hex(b: &[u8]) -> String {
        b.iter().fold(String::new(), |s, x| s + &format!("{x:02x}"))
    }

    fn rfc_server() -> ServerExchange {
        let mut ex = ServerExchange::new("user", Some(RFC_SECRET)).unwrap();
        ex.set_server_nonce_for_test(RFC_SERVER_NONCE);
        ex
    }

    #[test]
    fn pbkdf2_rfc7914() {
        let k = pbkdf2_hmac_array::<Sha256, 32>(b"passwd", b"salt", 1);
        assert_eq!(
            hex(&k),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn secret_matches_rfc_example() {
        assert_eq!(
            make_secret_with_salt("pencil", &RFC_SALT, 4096).unwrap(),
            RFC_SECRET
        );
        let sp = salted_password("pencil", &RFC_SALT, 4096);
        assert_eq!(
            hex(&sp),
            "c4a49510323ab4f952cac1fa99441939e78ea74d6be81ddf7096e87513dc615d"
        );
        let s = ScramSecret::parse(RFC_SECRET).unwrap();
        assert_eq!(
            hex(&s.stored_key),
            "586e5df283e6dceb5c3e791d8b8528ec191e664045ce971792e2e6b5bb13e2a6"
        );
        assert_eq!(
            hex(&s.server_key),
            "c1f3cbc1c13a9d35a14c0990eed97629ea225863e566a4314ab99f3f00e5d9d5"
        );
        assert_eq!(s.encode(), RFC_SECRET);
    }

    #[test]
    fn make_secret_random_and_errors() {
        let a = make_secret("pw", 4096).unwrap();
        let b = make_secret("pw", 4096).unwrap();
        assert_ne!(a, b);
        assert!(verify_plain_password("pw", &a));
        assert!(matches!(
            make_secret("pw", 0),
            Err(AuthError::InvalidParameter(_))
        ));
        assert!(matches!(
            make_secret("pw", MAX_ITERATIONS + 1),
            Err(AuthError::InvalidParameter(_))
        ));
    }

    #[test]
    fn parse_rejects_malformed() {
        let ok_salt = "W22ZaJ0SNY7soEsUEjb6gQ==";
        let key = "WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=";
        let k31 = B64.encode([1u8; 31]);
        let k33 = B64.encode([1u8; 33]);
        let cases: Vec<String> = vec![
            String::new(),
            "scram-sha-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$a:b".into(),
            format!("SCRAM-SHA-1$4096:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}{key}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}$${key}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}:{key}:"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}:{key}$x"),
            format!("SCRAM-SHA-256$4096x:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$+:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$abc:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$2147483648:{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$4096 :{ok_salt}${key}:{key}"),
            format!("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ=${key}:{key}"),
            format!("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ$ {key}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${k31}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${k33}:{key}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}:{k31}"),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}:{k33}"),
            format!(
                "SCRAM-SHA-256$4096:{ok_salt}${}:{key}",
                key.trim_end_matches('=')
            ),
            format!(
                "SCRAM-SHA-256$4096:{ok_salt}${key}:{}",
                key.trim_end_matches('=')
            ),
            format!("SCRAM-SHA-256$4096:{ok_salt}${key}:{key}x"),
        ];
        for c in &cases {
            assert!(ScramSecret::parse(c).is_none(), "should reject {c:?}");
            assert!(!is_scram_secret(c));
        }
        assert!(!is_scram_secret("md5d41d8cd98f00b204e9800998ecf8427e"));
        assert!(!is_scram_secret("plaintext"));
    }

    #[test]
    fn parse_accepts_strtol_forms() {
        let rest = RFC_SECRET.strip_prefix("SCRAM-SHA-256$4096").unwrap();
        for (it, want) in [
            ("+4096", 4096),
            (" 4096", 4096),
            ("-4096", -4096),
            ("0", 0),
            ("2147483647", i32::MAX),
            ("-2147483648", i32::MIN),
        ] {
            let s = ScramSecret::parse(&format!("SCRAM-SHA-256${it}{rest}"))
                .unwrap_or_else(|| panic!("{it}"));
            assert_eq!(s.iterations, want);
        }
        // Well-formed but unusable for authentication.
        let zero = format!("SCRAM-SHA-256$0{rest}");
        assert!(is_scram_secret(&zero));
        assert!(!verify_plain_password("pencil", &zero));
        let ex = ServerExchange::new("user", Some(&zero)).unwrap();
        assert!(ex.is_doomed());
    }

    #[test]
    fn verify_plain_password_cases() {
        assert!(verify_plain_password("pencil", RFC_SECRET));
        assert!(!verify_plain_password("pencil2", RFC_SECRET));
        assert!(!verify_plain_password("", RFC_SECRET));
        let empty = make_secret("", 10).unwrap();
        assert!(verify_plain_password("", &empty));
        assert!(!verify_plain_password("x", &empty));
        for bad in [
            "",
            "plain",
            "md5d41d8cd98f00b204e9800998ecf8427e",
            "SCRAM-SHA-256$bad",
        ] {
            assert!(!verify_plain_password("plain", bad));
        }
    }

    #[test]
    fn verify_non_ascii_nfc_nfd() {
        let nfc = "\u{00e9}a";
        let nfd = "e\u{0301}a";
        let s1 = make_secret(nfc, 5).unwrap();
        let s2 = make_secret(nfd, 5).unwrap();
        for s in [&s1, &s2] {
            assert!(verify_plain_password(nfc, s));
            assert!(verify_plain_password(nfd, s));
            assert!(!verify_plain_password("ea", s));
        }
    }

    #[test]
    fn debug_hides_keys() {
        let s = ScramSecret::parse(RFC_SECRET).unwrap();
        let d = format!("{s:?}");
        assert!(!d.contains("WG5d8oPm"));
        assert!(!d.contains(&B64.encode(&s.salt)));
        assert!(d.contains("4096"));
    }

    #[test]
    fn rfc7677_exchange_bytes() {
        let mut ex = rfc_server();
        assert!(!ex.is_doomed());
        let s1 = ex.client_first(RFC_C1.as_bytes()).unwrap();
        assert_eq!(s1, RFC_S1.as_bytes());
        let s2 = ex.client_final(RFC_C2.as_bytes()).unwrap();
        assert_eq!(s2, RFC_S2.as_bytes());
        // Done: further use is a protocol error.
        assert!(matches!(
            ex.client_final(RFC_C2.as_bytes()),
            Err(AuthError::Protocol { .. })
        ));
    }

    #[test]
    fn rfc7677_client_side() {
        let mut c = ClientExchange::with_nonce("user", "pencil", "rOprNGfwEbeRWgbNEkqO");
        assert_eq!(c.client_first(), RFC_C1.as_bytes());
        assert_eq!(
            c.client_final(RFC_S1.as_bytes()).unwrap(),
            RFC_C2.as_bytes()
        );
        c.verify_server_final(RFC_S2.as_bytes()).unwrap();
        assert_eq!(c.verify_server_final(b"v=AAAA"), Err(AuthError::Failed));
    }

    fn first_err(input: &str) -> AuthError {
        ServerExchange::new("u", Some(RFC_SECRET))
            .unwrap()
            .client_first(input.as_bytes())
            .unwrap_err()
    }

    #[test]
    fn client_first_error_table() {
        let protocol = [
            "",
            "p=tls-server-end-point,,n=,r=x",
            "x,,n=,r=x",
            "n;n=,r=x",
            "n,x=foo,n=,r=x",
            "n,,r=x",
            "n,,n=u",
            "n,,n=,r=",
            "n,,n=,r=a b",
            "n,,n=,r=a\u{7f}",
            "n",
            "n,",
        ];
        for c in protocol {
            assert!(matches!(first_err(c), AuthError::Protocol { .. }), "{c:?}");
        }
        assert!(matches!(
            first_err("n,a=foo,n=,r=x"),
            AuthError::Unsupported(_)
        ));
        assert!(matches!(
            first_err("n,,m=x,n=,r=x"),
            AuthError::Unsupported(_)
        ));
        let mut ex = ServerExchange::new("u", None).unwrap();
        assert!(matches!(
            ex.client_first(b"n,,n=u,r=a\0b"),
            Err(AuthError::Protocol { .. })
        ));
        assert!(matches!(
            ex.client_first(&[0xff, 0xfe]),
            Err(AuthError::Protocol { .. })
        ));
    }

    #[test]
    fn client_first_accepts_y_and_extensions() {
        let mut ex = ServerExchange::new("u", Some(RFC_SECRET)).unwrap();
        let s1 = ex.client_first(b"y,,n=u,r=abc,ext=1").unwrap();
        assert!(String::from_utf8(s1).unwrap().starts_with("r=abc"));
    }

    #[test]
    fn client_final_error_table() {
        // out of order
        let mut ex = rfc_server();
        assert!(matches!(
            ex.client_final(RFC_C2.as_bytes()),
            Err(AuthError::Protocol { .. })
        ));
        let run = |msg: &str| {
            let mut ex = rfc_server();
            ex.client_first(RFC_C1.as_bytes()).unwrap();
            ex.client_final(msg.as_bytes())
        };
        let nonce = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let proof = "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let proto = [
            format!("c=eSws,r={nonce},p={proof}"),
            format!("p=x,,r={nonce},p={proof}"),
            format!("c=biws,r={nonce}x,p={proof}"),
            format!("c=biws,r=other,p={proof}"),
            format!("c=biws,r={nonce},p={}", B64.encode([1u8; 31])),
            format!("c=biws,r={nonce},p={}", B64.encode([1u8; 33])),
            format!("c=biws,r={nonce},p=!!!"),
            format!("c=biws,r={nonce}"),
            format!("c=biws,p={proof}"),
            format!("r={nonce},p={proof}"),
            format!("r={nonce},c=biws,p={proof}"),
            format!("c=biws,r={nonce},p={proof},x=1"),
            String::new(),
        ];
        for m in &proto {
            assert!(matches!(run(m), Err(AuthError::Protocol { .. })), "{m:?}");
        }
        // extension between r and p is ignored
        assert!(run(&format!("c=biws,r={nonce},x=1,p={proof}")).is_err()); // changes AuthMessage => Failed
        // one-bit-different proof
        let mut bad = B64.decode(proof).unwrap();
        bad[0] ^= 1;
        assert_eq!(
            run(&format!("c=biws,r={nonce},p={}", B64.encode(bad))),
            Err(AuthError::Failed)
        );
        assert_eq!(run(RFC_C2).unwrap(), RFC_S2.as_bytes());
    }

    #[test]
    fn mock_salt_vector() {
        let m = mock_secret("alice", &[0; 32]);
        assert_eq!(B64.encode(&m.salt), "Uk7MU63Yg/hh/ptZ9hodwg==");
        assert_eq!(m.iterations, 4096);
    }

    fn salt_of(s1: &[u8]) -> (String, String) {
        let s = std::str::from_utf8(s1).unwrap();
        let salt = s
            .split(",s=")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .to_owned();
        let i = s.rsplit(",i=").next().unwrap().to_owned();
        (salt, i)
    }

    #[test]
    fn doomed_exchanges() {
        let nonce = [7u8; 32];
        let md5 = "md5d41d8cd98f00b204e9800998ecf8427e";
        for secret in [None, Some("plain"), Some(md5)] {
            let mut ex = ServerExchange::new_with_mock_nonce("bob", secret, &nonce).unwrap();
            assert!(ex.is_doomed());
            let mut c = ClientExchange::new("bob", "pw").unwrap();
            let s1 = ex.client_first(&c.client_first()).unwrap();
            let (salt, i) = salt_of(&s1);
            assert_eq!(salt, B64.encode(&mock_secret("bob", &nonce).salt));
            assert_eq!(i, "4096");
            let c2 = c.client_final(&s1).unwrap();
            assert_eq!(ex.client_final(&c2), Err(AuthError::Failed));
        }
        // same user + nonce => same salt, different server nonce; different user => different salt.
        let first = |user: &str, n: &[u8; 32]| {
            let mut ex = ServerExchange::new_with_mock_nonce(user, None, n).unwrap();
            String::from_utf8(ex.client_first(b"n,,n=,r=abc").unwrap()).unwrap()
        };
        let a = first("bob", &nonce);
        let b = first("bob", &nonce);
        assert_ne!(a, b);
        assert_eq!(salt_of(a.as_bytes()), salt_of(b.as_bytes()));
        assert_ne!(
            salt_of(a.as_bytes()).0,
            salt_of(first("carol", &nonce).as_bytes()).0
        );
        assert_ne!(
            salt_of(a.as_bytes()).0,
            salt_of(first("bob", &[8u8; 32]).as_bytes()).0
        );
        // process-wide nonce is stable
        let x = ServerExchange::new("zed", None).unwrap();
        let y = ServerExchange::new("zed", None).unwrap();
        assert_eq!(x.salt, y.salt);
    }

    #[test]
    fn server_nonce_shape() {
        let ex = ServerExchange::new("u", Some(RFC_SECRET)).unwrap();
        assert_eq!(ex.server_nonce.len(), 24);
        assert!(!ex.server_nonce.contains('='));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn roundtrip(pw in "\\PC{1,16}", salt in proptest::collection::vec(any::<u8>(), 1..24), iters in 1u32..200) {
            let secret = make_secret_with_salt(&pw, &salt, iters).unwrap();
            prop_assert!(verify_plain_password(&pw, &secret));
            let mut server = ServerExchange::new("u", Some(&secret)).unwrap();
            let mut client = ClientExchange::new("u", &pw).unwrap();
            let s1 = server.client_first(&client.client_first()).unwrap();
            let c2 = client.client_final(&s1).unwrap();
            let s2 = server.client_final(&c2).unwrap();
            prop_assert!(client.verify_server_final(&s2).is_ok());
        }

        #[test]
        fn wrong_password_fails(pw in "[ -~]{1,16}", idx in 0usize..16) {
            let secret = make_secret_with_salt(&pw, b"saltsalt", 3).unwrap();
            let mut bytes = pw.clone().into_bytes();
            let i = idx % bytes.len();
            bytes[i] = if bytes[i] == b'a' { b'b' } else { b'a' };
            let wrong = String::from_utf8(bytes).unwrap();
            let mut server = ServerExchange::new("u", Some(&secret)).unwrap();
            let mut client = ClientExchange::new("u", &wrong).unwrap();
            let s1 = server.client_first(&client.client_first()).unwrap();
            let c2 = client.client_final(&s1).unwrap();
            prop_assert_eq!(server.client_final(&c2), Err(AuthError::Failed));
        }
    }
}
