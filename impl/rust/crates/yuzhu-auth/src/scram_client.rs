//! RFC 5802 client side, used as the counterpart in server tests (raw-message tests
//! where a regular client library cannot be used).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::scram::{KEY_LEN, hmac_sha256, salted_password, sha256, xor32};
use crate::{AuthError, random};

/// Client side of one SCRAM-SHA-256 exchange (no channel binding).
pub struct ClientExchange {
    password: String,
    client_nonce: String,
    client_first_bare: String,
    gs2_header: &'static str,
    server_signature: Option<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for ClientExchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientExchange").finish_non_exhaustive()
    }
}

fn escape_username(user: &str) -> String {
    user.replace('=', "=3D").replace(',', "=2C")
}

impl ClientExchange {
    /// Random client nonce.
    pub fn new(username: &str, password: &str) -> Result<Self, AuthError> {
        let raw = random::bytes::<18>().map_err(|e| AuthError::Random(e.to_string()))?;
        Ok(Self::with_nonce(username, password, &B64.encode(raw)))
    }

    /// Fixed client nonce (RFC vectors).
    pub fn with_nonce(username: &str, password: &str, client_nonce: &str) -> Self {
        ClientExchange {
            password: password.to_owned(),
            client_nonce: client_nonce.to_owned(),
            client_first_bare: format!("n={},r={client_nonce}", escape_username(username)),
            gs2_header: "n,,",
            server_signature: None,
        }
    }

    /// GS2 ヘッダを `y,,`（クライアントは対応するがサーバーが広告しなかった）にする。
    #[must_use]
    pub fn with_binding_flag_y(mut self) -> Self {
        self.gs2_header = "y,,";
        self
    }

    /// `n,,n=<user>,r=<nonce>`（`with_binding_flag_y` なら `y,,`）。
    pub fn client_first(&self) -> Vec<u8> {
        format!("{}{}", self.gs2_header, self.client_first_bare).into_bytes()
    }

    /// Process server-first-message and build client-final-message.
    pub fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, AuthError> {
        let bad = |d: &str| AuthError::protocol("malformed SCRAM message", d);
        let sf = std::str::from_utf8(server_first)
            .map_err(|_| bad("server-first-message is not UTF-8"))?;
        let mut nonce = None;
        let mut salt = None;
        let mut iters = None;
        for attr in sf.split(',') {
            match attr.split_once('=') {
                Some(("r", v)) => nonce = Some(v),
                Some(("s", v)) => salt = Some(v),
                Some(("i", v)) => iters = Some(v),
                _ => {}
            }
        }
        let nonce = nonce.ok_or_else(|| bad("missing nonce in server-first-message"))?;
        let salt = B64
            .decode(salt.ok_or_else(|| bad("missing salt in server-first-message"))?)
            .map_err(|_| bad("invalid salt in server-first-message"))?;
        let iterations: u32 = iters
            .and_then(|v| v.parse().ok())
            .filter(|&i| i >= 1)
            .ok_or_else(|| bad("invalid iteration count in server-first-message"))?;
        if !nonce.starts_with(&self.client_nonce) || nonce.len() == self.client_nonce.len() {
            return Err(bad("server nonce does not extend client nonce"));
        }

        let salted = salted_password(&self.password, &salt, iterations);
        let client_key = hmac_sha256(&salted, &[b"Client Key"]);
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, &[b"Server Key"]);
        let without_proof = format!("c={},r={nonce}", B64.encode(self.gs2_header));
        let auth_message = [
            self.client_first_bare.as_bytes(),
            b",",
            server_first,
            b",",
            without_proof.as_bytes(),
        ];
        let client_signature = hmac_sha256(&stored_key, &auth_message);
        let proof = xor32(&client_key, &client_signature);
        self.server_signature = Some(hmac_sha256(&server_key, &auth_message));
        Ok(format!("{without_proof},p={}", B64.encode(proof)).into_bytes())
    }

    /// Check `v=<ServerSignature>`.
    pub fn verify_server_final(&self, server_final: &[u8]) -> Result<(), AuthError> {
        let expected = self.server_signature.ok_or(AuthError::Protocol {
            message: "unexpected SCRAM message",
            detail: None,
        })?;
        let got = std::str::from_utf8(server_final)
            .ok()
            .and_then(|s| s.strip_prefix("v="))
            .and_then(|v| B64.decode(v).ok());
        if got.as_deref() == Some(expected.as_slice()) {
            Ok(())
        } else {
            Err(AuthError::Failed)
        }
    }
}
