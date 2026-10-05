//! Reading frontend messages and writing backend messages.
//!
//! All integers are big-endian (network byte order). Strings are
//! NUL-terminated ("CString").

use std::fmt;
use std::io::{self, Read, Write};

use super::messages::{
    BackendMessage, CANCEL_REQUEST_CODE, ErrorFields, ExtendedKind, FieldDescription,
    FrontendMessage, GSSENC_REQUEST_CODE, MAX_STARTUP_PACKET_LEN, SSL_REQUEST_CODE, StartupPacket,
};

thread_local! {
    /// `client_encoding` of this connection's thread: `true` for LATIN1.
    /// Each connection runs on its own thread.
    static CLIENT_LATIN1: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets the client encoding used by [`read_message`] and [`encode`] on this
/// thread (`true` = LATIN1, `false` = UTF8).
pub fn set_client_latin1(latin1: bool) {
    CLIENT_LATIN1.with(|c| c.set(latin1));
}

fn client_latin1() -> bool {
    CLIENT_LATIN1.with(std::cell::Cell::get)
}

/// Appends `s` in the client encoding. Characters LATIN1 cannot represent
/// become `?`.
fn put_text(buf: &mut Vec<u8>, s: &str) {
    if client_latin1() {
        buf.extend(
            s.chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')),
        );
    } else {
        buf.extend_from_slice(s.as_bytes());
    }
}

/// Error while reading a frontend message.
#[derive(Debug)]
pub enum ProtocolError {
    /// Underlying I/O error (including EOF in the middle of a message).
    Io(io::Error),
    /// The declared length exceeds the limit.
    TooLarge { len: usize, max: usize },
    /// The message is malformed (bad length, missing terminator, ...).
    Malformed(String),
    /// A Query string is not valid UTF-8. The whole message has been
    /// consumed, so the connection can continue after reporting an error.
    InvalidUtf8,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::TooLarge { len, max } => {
                write!(f, "invalid message length {len} (limit {max})")
            }
            Self::Malformed(m) => f.write_str(m),
            Self::InvalidUtf8 => f.write_str("invalid byte sequence for encoding \"UTF8\""),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Reads `buf.len()` bytes. Returns `Ok(false)` on a clean EOF before the
/// first byte; EOF after that is an `UnexpectedEof` error.
fn read_header<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Reads exactly `len` bytes without trusting `len` for pre-allocation.
fn read_body<R: Read>(r: &mut R, len: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    let n = r.by_ref().take(len as u64).read_to_end(&mut body)?;
    if n == len {
        Ok(body)
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

/// Validates a declared length (which includes the 4 length bytes) and
/// returns the body length.
fn body_len(declared: i32, min_total: usize, max_total: usize) -> Result<usize, ProtocolError> {
    let total = usize::try_from(declared)
        .map_err(|_| ProtocolError::Malformed(format!("invalid message length {declared}")))?;
    if total < min_total {
        return Err(ProtocolError::Malformed(format!(
            "invalid message length {total}"
        )));
    }
    if total > max_total {
        return Err(ProtocolError::TooLarge {
            len: total,
            max: max_total,
        });
    }
    Ok(total - 4)
}

/// Reads the first packet of a connection (or the packet following an `N`
/// answer to SSLRequest/GSSENCRequest). Returns `Ok(None)` on clean EOF.
pub fn read_startup_packet<R: Read>(r: &mut R) -> Result<Option<StartupPacket>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    if !read_header(r, &mut len_buf)? {
        return Ok(None);
    }
    if len_buf[0] == 0x16 {
        // TLS ClientHello (direct SSL negotiation); we do not speak TLS.
        return Err(ProtocolError::Malformed(
            "direct SSL connection is not supported".into(),
        ));
    }
    let blen = body_len(i32::from_be_bytes(len_buf), 8, MAX_STARTUP_PACKET_LEN)?;
    let body = read_body(r, blen)?;
    let code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let rest = &body[4..];
    match code {
        SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => {
            if !rest.is_empty() {
                return Err(ProtocolError::Malformed(
                    "invalid length of startup packet".into(),
                ));
            }
            Ok(Some(if code == SSL_REQUEST_CODE {
                StartupPacket::SslRequest
            } else {
                StartupPacket::GssEncRequest
            }))
        }
        CANCEL_REQUEST_CODE => {
            if rest.len() != 8 {
                return Err(ProtocolError::Malformed(
                    "invalid length of cancel request packet".into(),
                ));
            }
            Ok(Some(StartupPacket::CancelRequest {
                pid: i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]),
                secret: i32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]),
            }))
        }
        _ => {
            let [hi0, hi1, lo0, lo1] = code.to_be_bytes();
            let major = u16::from_be_bytes([hi0, hi1]);
            let minor = u16::from_be_bytes([lo0, lo1]);
            let params = if major == 3 {
                parse_startup_params(rest)?
            } else {
                Vec::new()
            };
            Ok(Some(StartupPacket::Startup {
                major,
                minor,
                params,
            }))
        }
    }
}

fn parse_startup_params(mut buf: &[u8]) -> Result<Vec<(String, String)>, ProtocolError> {
    let layout_err = || {
        ProtocolError::Malformed(
            "invalid startup packet layout: expected terminator as last byte".into(),
        )
    };
    let mut params = Vec::new();
    loop {
        let name = take_cstr(&mut buf).ok_or_else(layout_err)?;
        if name.is_empty() {
            if !buf.is_empty() {
                return Err(layout_err());
            }
            return Ok(params);
        }
        let value = take_cstr(&mut buf).ok_or_else(layout_err)?;
        let name = String::from_utf8(name.to_vec())
            .map_err(|_| ProtocolError::Malformed("invalid UTF-8 in startup packet".into()))?;
        let value = String::from_utf8(value.to_vec())
            .map_err(|_| ProtocolError::Malformed("invalid UTF-8 in startup packet".into()))?;
        params.push((name, value));
    }
}

/// Splits off a NUL-terminated string (without the NUL).
fn take_cstr<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let pos = buf.iter().position(|&b| b == 0)?;
    let s = &buf[..pos];
    *buf = &buf[pos + 1..];
    Some(s)
}

/// Reads one regular message (type byte + length + body). Returns
/// `Ok(None)` on clean EOF at a message boundary. Bodies of messages yuzhu
/// does not interpret are read and discarded.
pub fn read_message<R: Read>(
    r: &mut R,
    max_len: usize,
) -> Result<Option<FrontendMessage>, ProtocolError> {
    let mut header = [0u8; 5];
    if !read_header(r, &mut header)? {
        return Ok(None);
    }
    let tag = header[0];
    let declared = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let blen = body_len(declared, 4, max_len)?;
    let body = read_body(r, blen)?;
    Ok(Some(match tag {
        b'Q' => {
            let Some((&0, s)) = body.split_last() else {
                return Err(ProtocolError::Malformed("invalid string in message".into()));
            };
            if s.contains(&0) {
                return Err(ProtocolError::Malformed("invalid string in message".into()));
            }
            FrontendMessage::Query(if client_latin1() {
                s.iter().map(|&b| char::from(b)).collect()
            } else {
                String::from_utf8(s.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)?
            })
        }
        b'X' => FrontendMessage::Terminate,
        b'S' => FrontendMessage::Sync,
        _ => match ExtendedKind::from_tag(tag) {
            Some(kind) => FrontendMessage::Extended(kind),
            None => FrontendMessage::Unknown(tag),
        },
    }))
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn too_long() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "message too long")
}

fn put_i16(buf: &mut Vec<u8>, v: i16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn put_i32(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Writes a CString. Interior NUL bytes would corrupt the framing, so they
/// are dropped.
fn put_cstr(buf: &mut Vec<u8>, s: &str) {
    let mut tmp = Vec::with_capacity(s.len());
    put_text(&mut tmp, s);
    buf.extend(tmp.into_iter().filter(|&b| b != 0));
    buf.push(0);
}

fn put_error_fields(buf: &mut Vec<u8>, f: &ErrorFields<'_>) {
    buf.push(b'S');
    put_cstr(buf, f.severity);
    buf.push(b'V');
    put_cstr(buf, f.severity);
    buf.push(b'C');
    put_cstr(buf, f.code);
    buf.push(b'M');
    put_cstr(buf, f.message);
    if let Some(d) = f.detail {
        buf.push(b'D');
        put_cstr(buf, d);
    }
    if let Some(h) = f.hint {
        buf.push(b'H');
        put_cstr(buf, h);
    }
    if let Some(p) = f.position {
        buf.push(b'P');
        put_cstr(buf, &p.to_string());
    }
    buf.push(0);
}

fn put_row_description(buf: &mut Vec<u8>, fields: &[FieldDescription<'_>]) -> io::Result<()> {
    put_i16(buf, i16::try_from(fields.len()).map_err(|_| too_long())?);
    for f in fields {
        put_cstr(buf, f.name);
        put_u32(buf, f.table_oid);
        put_i16(buf, f.column_attnum);
        put_u32(buf, f.type_oid);
        put_i16(buf, f.type_len);
        put_i32(buf, f.type_modifier);
        put_i16(buf, f.format);
    }
    Ok(())
}

fn put_data_row(buf: &mut Vec<u8>, values: &[Option<String>]) -> io::Result<()> {
    put_i16(buf, i16::try_from(values.len()).map_err(|_| too_long())?);
    for v in values {
        match v {
            None => put_i32(buf, -1),
            Some(s) => {
                let start = buf.len();
                put_i32(buf, 0);
                put_text(buf, s);
                let n = i32::try_from(buf.len() - start - 4).map_err(|_| too_long())?;
                buf[start..start + 4].copy_from_slice(&n.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Encodes a backend message into bytes (type byte + length + body).
pub fn encode(msg: &BackendMessage<'_>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    let tag = match msg {
        BackendMessage::AuthenticationOk => b'R',
        BackendMessage::ParameterStatus { .. } => b'S',
        BackendMessage::BackendKeyData { .. } => b'K',
        BackendMessage::ReadyForQuery(_) => b'Z',
        BackendMessage::RowDescription(_) => b'T',
        BackendMessage::DataRow(_) => b'D',
        BackendMessage::CommandComplete(_) => b'C',
        BackendMessage::EmptyQueryResponse => b'I',
        BackendMessage::ErrorResponse(_) => b'E',
        BackendMessage::NoticeResponse(_) => b'N',
        BackendMessage::NegotiateProtocolVersion { .. } => b'v',
    };
    buf.push(tag);
    buf.extend_from_slice(&[0; 4]); // length placeholder
    match msg {
        BackendMessage::AuthenticationOk => put_i32(&mut buf, 0),
        BackendMessage::ParameterStatus { name, value } => {
            put_cstr(&mut buf, name);
            put_cstr(&mut buf, value);
        }
        BackendMessage::BackendKeyData { pid, secret } => {
            put_i32(&mut buf, *pid);
            put_i32(&mut buf, *secret);
        }
        BackendMessage::ReadyForQuery(status) => buf.push(*status),
        BackendMessage::RowDescription(fields) => put_row_description(&mut buf, fields)?,
        BackendMessage::DataRow(values) => put_data_row(&mut buf, values)?,
        BackendMessage::CommandComplete(tag) => put_cstr(&mut buf, tag),
        BackendMessage::EmptyQueryResponse => {}
        BackendMessage::ErrorResponse(f) | BackendMessage::NoticeResponse(f) => {
            put_error_fields(&mut buf, f);
        }
        BackendMessage::NegotiateProtocolVersion {
            newest_minor,
            unrecognized,
        } => {
            put_i32(&mut buf, *newest_minor);
            put_i32(
                &mut buf,
                i32::try_from(unrecognized.len()).map_err(|_| too_long())?,
            );
            for name in *unrecognized {
                put_cstr(&mut buf, name);
            }
        }
    }
    let len = i32::try_from(buf.len() - 1).map_err(|_| too_long())?;
    buf[1..5].copy_from_slice(&len.to_be_bytes());
    Ok(buf)
}

/// Writes a backend message.
pub fn write_message<W: Write>(w: &mut W, msg: &BackendMessage<'_>) -> io::Result<()> {
    w.write_all(&encode(msg)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::{DEFAULT_MAX_MESSAGE_LEN, PROTOCOL_V3};
    use std::io::Cursor;

    fn startup_bytes(code: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let len = i32::try_from(8 + payload.len()).unwrap();
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(&code.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn msg_bytes(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    fn read_startup(bytes: &[u8]) -> Result<Option<StartupPacket>, ProtocolError> {
        read_startup_packet(&mut Cursor::new(bytes.to_vec()))
    }

    fn read_msg(bytes: &[u8]) -> Result<Option<FrontendMessage>, ProtocolError> {
        read_message(&mut Cursor::new(bytes.to_vec()), DEFAULT_MAX_MESSAGE_LEN)
    }

    #[test]
    fn ssl_request() {
        let bytes = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];
        assert_eq!(
            read_startup(&bytes).unwrap(),
            Some(StartupPacket::SslRequest)
        );
    }

    #[test]
    fn gssenc_request() {
        let bytes = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x30];
        assert_eq!(
            read_startup(&bytes).unwrap(),
            Some(StartupPacket::GssEncRequest)
        );
    }

    #[test]
    fn ssl_request_with_extra_payload_is_malformed() {
        let bytes = startup_bytes(SSL_REQUEST_CODE, &[0, 0, 0, 0]);
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn cancel_request() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&42i32.to_be_bytes());
        payload.extend_from_slice(&(-7i32).to_be_bytes());
        let bytes = startup_bytes(CANCEL_REQUEST_CODE, &payload);
        assert_eq!(bytes[..8], [0, 0, 0, 16, 0x04, 0xd2, 0x16, 0x2e]);
        assert_eq!(
            read_startup(&bytes).unwrap(),
            Some(StartupPacket::CancelRequest {
                pid: 42,
                secret: -7
            })
        );
    }

    #[test]
    fn cancel_request_bad_length() {
        let bytes = startup_bytes(CANCEL_REQUEST_CODE, &[0, 0, 0, 1]);
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn startup_message_v3() {
        let payload = b"user\0alice\0database\0db1\0application_name\0psql\0\0";
        let bytes = startup_bytes(PROTOCOL_V3, payload);
        assert_eq!(bytes[4..8], [0, 3, 0, 0]);
        let Some(StartupPacket::Startup {
            major,
            minor,
            params,
        }) = read_startup(&bytes).unwrap()
        else {
            panic!("expected startup");
        };
        assert_eq!((major, minor), (3, 0));
        assert_eq!(
            params,
            vec![
                ("user".into(), "alice".into()),
                ("database".into(), "db1".into()),
                ("application_name".into(), "psql".into()),
            ]
        );
    }

    #[test]
    fn startup_message_minor_version() {
        let bytes = startup_bytes(196_610, b"user\0u\0\0");
        let Some(StartupPacket::Startup { major, minor, .. }) = read_startup(&bytes).unwrap()
        else {
            panic!("expected startup");
        };
        assert_eq!((major, minor), (3, 2));
    }

    #[test]
    fn startup_message_other_major_has_no_params() {
        let bytes = startup_bytes(2 << 16, b"garbage");
        assert_eq!(
            read_startup(&bytes).unwrap(),
            Some(StartupPacket::Startup {
                major: 2,
                minor: 0,
                params: vec![]
            })
        );
    }

    #[test]
    fn startup_message_missing_terminator() {
        let bytes = startup_bytes(PROTOCOL_V3, b"user\0alice\0");
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
        let bytes = startup_bytes(PROTOCOL_V3, b"user\0alice");
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
        let bytes = startup_bytes(PROTOCOL_V3, b"user\0alice\0\0extra");
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn startup_packet_length_limits() {
        // Too short.
        let bytes = [0, 0, 0, 4, 0, 3, 0, 0];
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
        // Negative.
        let bytes = [0xff, 0xff, 0xff, 0xff];
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
        // Too long.
        let bytes = 10_001i32.to_be_bytes();
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::TooLarge { len: 10_001, .. })
        ));
    }

    #[test]
    fn direct_tls_is_rejected() {
        let bytes = [0x16, 0x03, 0x01, 0x00, 0xff];
        assert!(matches!(
            read_startup(&bytes),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn startup_eof_handling() {
        assert!(read_startup(&[]).unwrap().is_none());
        let err = read_startup(&[0, 0]).unwrap_err();
        assert!(
            matches!(err, ProtocolError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
        // Body shorter than declared.
        let err = read_startup(&[0, 0, 0, 20, 0, 3, 0, 0]).unwrap_err();
        assert!(matches!(err, ProtocolError::Io(_)));
    }

    #[test]
    fn query_message() {
        let bytes = msg_bytes(b'Q', b"SELECT 1;\0");
        assert_eq!(
            read_msg(&bytes).unwrap(),
            Some(FrontendMessage::Query("SELECT 1;".into()))
        );
        let bytes = msg_bytes(b'Q', "SELECT 'ゆず'\0".as_bytes());
        assert_eq!(
            read_msg(&bytes).unwrap(),
            Some(FrontendMessage::Query("SELECT 'ゆず'".into()))
        );
    }

    #[test]
    fn query_message_malformed() {
        // No terminator.
        assert!(matches!(
            read_msg(&msg_bytes(b'Q', b"SELECT 1")),
            Err(ProtocolError::Malformed(_))
        ));
        // Empty body.
        assert!(matches!(
            read_msg(&msg_bytes(b'Q', b"")),
            Err(ProtocolError::Malformed(_))
        ));
        // Interior NUL.
        assert!(matches!(
            read_msg(&msg_bytes(b'Q', b"SELECT\0 1\0")),
            Err(ProtocolError::Malformed(_))
        ));
        // Invalid UTF-8.
        assert!(matches!(
            read_msg(&msg_bytes(b'Q', b"SELECT '\xff'\0")),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    #[test]
    fn invalid_utf8_consumes_whole_message() {
        let mut bytes = msg_bytes(b'Q', b"\xff\0");
        bytes.extend(msg_bytes(b'X', b""));
        let mut cur = Cursor::new(bytes);
        assert!(matches!(
            read_message(&mut cur, DEFAULT_MAX_MESSAGE_LEN),
            Err(ProtocolError::InvalidUtf8)
        ));
        assert_eq!(
            read_message(&mut cur, DEFAULT_MAX_MESSAGE_LEN).unwrap(),
            Some(FrontendMessage::Terminate)
        );
    }

    #[test]
    fn simple_messages() {
        assert_eq!(
            read_msg(&[b'X', 0, 0, 0, 4]).unwrap(),
            Some(FrontendMessage::Terminate)
        );
        assert_eq!(
            read_msg(&[b'S', 0, 0, 0, 4]).unwrap(),
            Some(FrontendMessage::Sync)
        );
    }

    #[test]
    fn extended_messages_are_discarded() {
        let cases = [
            (b'P', ExtendedKind::Parse),
            (b'B', ExtendedKind::Bind),
            (b'D', ExtendedKind::Describe),
            (b'E', ExtendedKind::Execute),
            (b'C', ExtendedKind::Close),
            (b'H', ExtendedKind::Flush),
            (b'F', ExtendedKind::FunctionCall),
        ];
        for (tag, kind) in cases {
            let mut bytes = msg_bytes(tag, b"some\0body\0\x00\x01");
            bytes.extend(msg_bytes(b'S', b""));
            let mut cur = Cursor::new(bytes);
            assert_eq!(
                read_message(&mut cur, DEFAULT_MAX_MESSAGE_LEN).unwrap(),
                Some(FrontendMessage::Extended(kind))
            );
            assert_eq!(
                read_message(&mut cur, DEFAULT_MAX_MESSAGE_LEN).unwrap(),
                Some(FrontendMessage::Sync)
            );
        }
    }

    #[test]
    fn unknown_message() {
        assert_eq!(
            read_msg(&msg_bytes(b'z', b"abc")).unwrap(),
            Some(FrontendMessage::Unknown(b'z'))
        );
    }

    #[test]
    fn message_length_limits() {
        assert!(matches!(
            read_msg(&[b'Q', 0, 0, 0, 3]),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(matches!(
            read_msg(&[b'Q', 0x80, 0, 0, 0]),
            Err(ProtocolError::Malformed(_))
        ));
        let mut cur = Cursor::new(msg_bytes(b'Q', b"SELECT 1\0"));
        assert!(matches!(
            read_message(&mut cur, 8),
            Err(ProtocolError::TooLarge { len: 13, max: 8 })
        ));
        // A huge declared length must not allocate up front; EOF is reported.
        let err = read_message(
            &mut Cursor::new(vec![b'Q', 0x3f, 0xff, 0xff, 0xff]),
            usize::MAX,
        )
        .unwrap_err();
        assert!(matches!(err, ProtocolError::Io(_)));
    }

    #[test]
    fn message_eof_handling() {
        assert!(read_msg(&[]).unwrap().is_none());
        assert!(matches!(read_msg(&[b'Q', 0]), Err(ProtocolError::Io(_))));
        assert!(matches!(
            read_msg(&[b'Q', 0, 0, 0, 10, b'a']),
            Err(ProtocolError::Io(_))
        ));
    }

    fn enc(msg: &BackendMessage<'_>) -> Vec<u8> {
        encode(msg).unwrap()
    }

    #[test]
    fn encode_authentication_ok() {
        assert_eq!(
            enc(&BackendMessage::AuthenticationOk),
            [b'R', 0, 0, 0, 8, 0, 0, 0, 0]
        );
    }

    #[test]
    fn encode_parameter_status() {
        let mut expected = vec![b'S', 0, 0, 0, 24];
        expected.extend_from_slice(b"server_version\x0016.0\0");
        assert_eq!(
            enc(&BackendMessage::ParameterStatus {
                name: "server_version",
                value: "16.0"
            }),
            expected
        );
    }

    #[test]
    fn encode_backend_key_data() {
        assert_eq!(
            enc(&BackendMessage::BackendKeyData { pid: 1, secret: -2 }),
            [b'K', 0, 0, 0, 12, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xfe]
        );
    }

    #[test]
    fn encode_ready_for_query() {
        assert_eq!(
            enc(&BackendMessage::ReadyForQuery(b'I')),
            [b'Z', 0, 0, 0, 5, b'I']
        );
        assert_eq!(
            enc(&BackendMessage::ReadyForQuery(b'E')),
            [b'Z', 0, 0, 0, 5, b'E']
        );
    }

    #[test]
    fn encode_row_description() {
        let fields = [FieldDescription {
            name: "a",
            table_oid: 16384,
            column_attnum: 1,
            type_oid: 23,
            type_len: 4,
            type_modifier: -1,
            format: 0,
        }];
        let mut expected = vec![b'T', 0, 0, 0, 26, 0, 1, b'a', 0];
        expected.extend_from_slice(&[0, 0, 0x40, 0x00]); // table oid 16384
        expected.extend_from_slice(&[0, 1]); // attnum
        expected.extend_from_slice(&[0, 0, 0, 23]); // type oid
        expected.extend_from_slice(&[0, 4]); // typlen
        expected.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]); // typmod
        expected.extend_from_slice(&[0, 0]); // format
        assert_eq!(enc(&BackendMessage::RowDescription(&fields)), expected);
        assert_eq!(
            enc(&BackendMessage::RowDescription(&[])),
            [b'T', 0, 0, 0, 6, 0, 0]
        );
    }

    #[test]
    fn encode_data_row() {
        let values = [Some("12".to_string()), None, Some(String::new())];
        assert_eq!(
            enc(&BackendMessage::DataRow(&values)),
            [
                b'D', 0, 0, 0, 20, 0, 3, 0, 0, 0, 2, b'1', b'2', 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn encode_command_complete_and_empty_query() {
        let mut expected = vec![b'C', 0, 0, 0, 15];
        expected.extend_from_slice(b"INSERT 0 1\0");
        assert_eq!(
            enc(&BackendMessage::CommandComplete("INSERT 0 1")),
            expected
        );
        assert_eq!(enc(&BackendMessage::EmptyQueryResponse), [b'I', 0, 0, 0, 4]);
    }

    #[test]
    fn encode_error_response_minimal() {
        let f = ErrorFields {
            severity: "ERROR",
            code: "0A000",
            message: "x",
            ..ErrorFields::default()
        };
        let mut expected = vec![b'E', 0, 0, 0, 29];
        expected.extend_from_slice(b"SERROR\0VERROR\0C0A000\0Mx\0\0");
        assert_eq!(enc(&BackendMessage::ErrorResponse(f)), expected);
    }

    #[test]
    fn encode_error_response_all_fields() {
        let f = ErrorFields {
            severity: "FATAL",
            code: "08P01",
            message: "m",
            detail: Some("d"),
            hint: Some("h"),
            position: Some(17),
        };
        let bytes = enc(&BackendMessage::ErrorResponse(f));
        assert_eq!(bytes[0], b'E');
        assert_eq!(
            &bytes[5..],
            b"SFATAL\0VFATAL\0C08P01\0Mm\0Dd\0Hh\0P17\0\0".as_slice()
        );
        let len = i32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        assert_eq!(usize::try_from(len).unwrap(), bytes.len() - 1);
    }

    #[test]
    fn encode_notice_response() {
        let f = ErrorFields {
            severity: "WARNING",
            code: "25P01",
            message: "there is no transaction in progress",
            ..ErrorFields::default()
        };
        let bytes = enc(&BackendMessage::NoticeResponse(f));
        assert_eq!(bytes[0], b'N');
        assert_eq!(
            &bytes[5..],
            b"SWARNING\0VWARNING\0C25P01\0Mthere is no transaction in progress\0\0".as_slice()
        );
    }

    #[test]
    fn encode_negotiate_protocol_version() {
        let names = ["_pq_.foo".to_string()];
        let mut expected = vec![b'v', 0, 0, 0, 21, 0, 0, 0, 0, 0, 0, 0, 1];
        expected.extend_from_slice(b"_pq_.foo\0");
        assert_eq!(
            enc(&BackendMessage::NegotiateProtocolVersion {
                newest_minor: 0,
                unrecognized: &names
            }),
            expected
        );
    }

    #[test]
    fn interior_nul_is_dropped() {
        assert_eq!(
            enc(&BackendMessage::CommandComplete("a\0b")),
            [b'C', 0, 0, 0, 7, b'a', b'b', 0]
        );
    }

    #[test]
    fn latin1_client_encoding_transcodes_text() {
        set_client_latin1(true);
        let rows = [Some("é€".to_string())];
        let bytes = encode(&BackendMessage::DataRow(&rows)).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], [0xE9, b'?']);
        assert_eq!(&bytes[7..11], [0, 0, 0, 2]);
        let mut q = vec![b'Q', 0, 0, 0, 7, 0xE9, b'x', b'y', 0];
        q[4] = 8;
        let msg = read_message(&mut Cursor::new(q), DEFAULT_MAX_MESSAGE_LEN).unwrap();
        set_client_latin1(false);
        assert_eq!(msg, Some(FrontendMessage::Query("éxy".into())));
    }

    #[test]
    fn write_message_writes_encoded_bytes() {
        let mut out = Vec::new();
        write_message(&mut out, &BackendMessage::ReadyForQuery(b'T')).unwrap();
        write_message(&mut out, &BackendMessage::EmptyQueryResponse).unwrap();
        assert_eq!(out, [b'Z', 0, 0, 0, 5, b'T', b'I', 0, 0, 0, 4]);
    }
}
