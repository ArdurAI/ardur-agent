//! The ACP crate's typed error surface.

/// A failure while encoding, decoding, transporting, or admitting ACP traffic.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// A frame was empty after trimming its line ending.
    #[error("ACP frame is empty")]
    EmptyFrame,
    /// The frame was not valid UTF-8.
    #[error("ACP frame is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// A newline appeared inside a frame payload.
    #[error("ACP frame contains an embedded newline")]
    EmbeddedNewline,
    /// The decoded JSON value was not a JSON object.
    #[error("ACP frame must contain a JSON object")]
    NonObjectFrame,
    /// JSON serialization or deserialization failed.
    #[error("ACP JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON-RPC version was not `2.0`.
    #[error("unsupported JSON-RPC version {0:?}")]
    InvalidJsonRpcVersion(String),
    /// The JSON-RPC envelope was internally inconsistent.
    #[error("invalid ACP message: {0}")]
    InvalidMessage(String),
    /// The peer selected a protocol major version this crate does not speak.
    #[error("unsupported ACP protocol version {requested}; supported version is {supported}")]
    UnsupportedProtocolVersion {
        /// The peer-requested or peer-selected version.
        requested: u16,
        /// The only version this crate currently supports.
        supported: u16,
    },
    /// I/O failed while reading or writing a transport frame.
    #[error("ACP transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A transport adapter failed before a typed protocol error was available.
    #[error("ACP transport failed: {0}")]
    Transport(String),
    /// The peer or task was refused by local trust policy.
    #[error("ACP trust refused: {0}")]
    TrustRefused(String),
}
