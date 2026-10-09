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

/// Target of a Describe or Close message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DescribeTarget {
    /// `S`: a prepared statement.
    Statement,
    /// `P`: a portal.
    Portal,
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
    /// `H`: Flush.
    Flush,
    /// `P`: Parse. `param_types` are the declared OIDs (0 = unspecified).
    Parse {
        name: String,
        sql: String,
        param_types: Vec<u32>,
    },
    /// `B`: Bind. Format codes are raw (`0` / `1` are checked by the caller);
    /// `None` is a NULL parameter.
    Bind {
        portal: String,
        stmt: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    /// `D`: Describe.
    Describe {
        target: DescribeTarget,
        name: String,
    },
    /// `C`: Close.
    Close {
        target: DescribeTarget,
        name: String,
    },
    /// `E`: Execute. `max_rows <= 0` means all rows.
    Execute { portal: String, max_rows: i32 },
    /// `F`: FunctionCall (body discarded).
    FunctionCall,
    /// `d`: CopyData (raw bytes of a `COPY FROM STDIN` stream).
    CopyData(Vec<u8>),
    /// `c`: CopyDone.
    CopyDone,
    /// `f`: CopyFail with the client's message (empty if it is not valid
    /// UTF-8 or not NUL terminated).
    CopyFail(String),
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
    /// `W`: context (e.g. `COPY t, line 3`).
    pub context: Option<&'a str>,
    /// `s`: schema name.
    pub schema: Option<&'a str>,
    /// `t`: table name.
    pub table: Option<&'a str>,
    /// `c`: column name.
    pub column: Option<&'a str>,
    /// `n`: constraint name.
    pub constraint: Option<&'a str>,
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
    /// `G` CopyInResponse: overall format (0 = text) and one format code
    /// per column.
    CopyInResponse {
        format: u8,
        column_formats: &'a [i16],
    },
    /// `1` ParseComplete.
    ParseComplete,
    /// `2` BindComplete.
    BindComplete,
    /// `3` CloseComplete.
    CloseComplete,
    /// `n` NoData.
    NoData,
    /// `s` PortalSuspended.
    PortalSuspended,
    /// `t` ParameterDescription (type OIDs).
    ParameterDescription(&'a [u32]),
    /// `D` DataRow with values already encoded in each column's format
    /// (`None` is NULL).
    DataRowRaw(&'a [Option<Vec<u8>>]),
    /// `H` CopyOutResponse (same layout as `CopyInResponse`).
    CopyOutResponse {
        format: u8,
        column_formats: &'a [i16],
    },
    /// `d` CopyData (COPY TO).
    CopyData(&'a [u8]),
    /// `c` CopyDone (COPY TO).
    CopyDone,
    /// `E`ErrorResponse.
    ErrorResponse(ErrorFields<'a>),
    /// `N` NoticeResponse.
    NoticeResponse(ErrorFields<'a>),
    /// `v` NegotiateProtocolVersion.
    NegotiateProtocolVersion {
        newest_minor: i32,
        unrecognized: &'a [String],
    },
}
