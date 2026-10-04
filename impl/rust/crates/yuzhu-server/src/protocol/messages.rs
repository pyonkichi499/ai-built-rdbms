//! Protocol message types (PostgreSQL frontend/backend protocol 3.0).
//!
//! These types are independent of `yuzhu-core`: the connection layer converts
//! core values (errors, column descriptions, ...) into them.

/// Protocol version 3.0 as sent in a StartupMessage (`3 << 16 | 0`).
pub const PROTOCOL_V3: u32 = 196_608;
/// SSLRequest code (`1234 << 16 | 5679`).
pub const SSL_REQUEST_CODE: u32 = 80_877_103;
/// GSSENCRequest code (`1234 << 16 | 5680`).
pub const GSSENC_REQUEST_CODE: u32 = 80_877_104;
/// CancelRequest code (`1234 << 16 | 5678`).
pub const CANCEL_REQUEST_CODE: u32 = 80_877_102;

/// Maximum length of a startup-phase packet (same as PostgreSQL's
/// `MAX_STARTUP_PACKET_LENGTH`).
pub const MAX_STARTUP_PACKET_LEN: usize = 10_000;
/// Default maximum length of a regular message (PostgreSQL's
/// `PQ_LARGE_MESSAGE_LIMIT`, 1GB - 1).
pub const DEFAULT_MAX_MESSAGE_LEN: usize = 0x3fff_ffff;

/// The first packet of a connection (no type byte).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StartupPacket {
    /// SSLRequest: answer `N` and read another startup packet.
    SslRequest,
    /// GSSENCRequest: answer `N` and read another startup packet.
    GssEncRequest,
    /// CancelRequest: carries the target backend's key.
    CancelRequest { pid: i32, secret: i32 },
    /// StartupMessage for protocol `major.minor`.
    Startup {
        major: u16,
        minor: u16,
        /// `(name, value)` pairs in the order received.
        params: Vec<(String, String)>,
    },
}

/// Extended-query (and function-call) messages, which M1 does not support.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtendedKind {
    Parse,
    Bind,
    Describe,
    Execute,
    Close,
    Flush,
    FunctionCall,
}

impl ExtendedKind {
    /// Maps a message type byte to an extended-query kind.
    pub fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            b'P' => Self::Parse,
            b'B' => Self::Bind,
            b'D' => Self::Describe,
            b'E' => Self::Execute,
            b'C' => Self::Close,
            b'H' => Self::Flush,
            b'F' => Self::FunctionCall,
            _ => return None,
        })
    }
}

/// A regular (typed) frontend message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FrontendMessage {
    /// `Q`: Simple Query.
    Query(String),
    /// `X`: Terminate.
    Terminate,
    /// `S`: Sync.
    Sync,
    /// One of `P B D E C H F` (body discarded).
    Extended(ExtendedKind),
    /// Any other type byte (body discarded).
    Unknown(u8),
}

/// One column of a RowDescription.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldDescription<'a> {
    pub name: &'a str,
    pub table_oid: u32,
    pub column_attnum: i16,
    pub type_oid: u32,
    pub type_len: i16,
    pub type_modifier: i32,
    /// 0 = text, 1 = binary. Simple Query results are always text.
    pub format: i16,
}

/// Fields of an ErrorResponse / NoticeResponse.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ErrorFields<'a> {
    /// `S` and `V` (e.g. `ERROR`, `FATAL`, `WARNING`).
    pub severity: &'a str,
    /// `C`: SQLSTATE.
    pub code: &'a str,
    /// `M`: primary message.
    pub message: &'a str,
    /// `D`: detail.
    pub detail: Option<&'a str>,
    /// `H`: hint.
    pub hint: Option<&'a str>,
    /// `P`: 1-based character position in the query string.
    pub position: Option<u32>,
}

/// A backend message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BackendMessage<'a> {
    /// `R` AuthenticationOk.
    AuthenticationOk,
    /// `S` ParameterStatus.
    ParameterStatus { name: &'a str, value: &'a str },
    /// `K` BackendKeyData.
    BackendKeyData { pid: i32, secret: i32 },
    /// `Z` ReadyForQuery with status byte `I`, `T` or `E`.
    ReadyForQuery(u8),
    /// `T` RowDescription.
    RowDescription(&'a [FieldDescription<'a>]),
    /// `D` DataRow (text values; `None` is NULL).
    DataRow(&'a [Option<String>]),
    /// `C` CommandComplete.
    CommandComplete(&'a str),
    /// `I` EmptyQueryResponse.
    EmptyQueryResponse,
    /// `E` ErrorResponse.
    ErrorResponse(ErrorFields<'a>),
    /// `N` NoticeResponse.
    NoticeResponse(ErrorFields<'a>),
    /// `v` NegotiateProtocolVersion.
    NegotiateProtocolVersion {
        newest_minor: i32,
        unrecognized: &'a [String],
    },
}
