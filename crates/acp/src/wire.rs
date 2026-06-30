//! ACP v1 JSON-RPC wire envelopes and newline-delimited frame codec.

use std::str;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::AcpError;

/// The JSON-RPC version required by ACP v1.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpRequestId {
    /// A string request id.
    String(String),
    /// An integer request id.
    Number(i64),
}

impl From<&str> for AcpRequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for AcpRequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<i64> for AcpRequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

/// A JSON-RPC request carrying an ACP method call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpRequest {
    /// Always `2.0`.
    pub jsonrpc: String,
    /// Correlation id echoed by the response.
    pub id: AcpRequestId,
    /// ACP method name, for example `initialize` or `session/new`.
    pub method: String,
    /// Optional method parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl AcpRequest {
    /// Build a JSON-RPC request for an ACP method.
    pub fn new(
        id: impl Into<AcpRequestId>,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC notification carrying an ACP server or client event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpNotification {
    /// Always `2.0`.
    pub jsonrpc: String,
    /// ACP notification method name.
    pub method: String,
    /// Optional notification parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl AcpNotification {
    /// Build a JSON-RPC notification for an ACP method.
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpErrorObject {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl AcpErrorObject {
    /// Build a JSON-RPC error object.
    pub fn new(code: i64, message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

/// The result-or-error payload of a JSON-RPC response.
#[derive(Clone, Debug, PartialEq)]
pub enum AcpResponsePayload {
    /// A successful method result.
    Result(serde_json::Value),
    /// A failed method call.
    Error(AcpErrorObject),
}

/// A JSON-RPC response to an ACP request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpResponse {
    /// Always `2.0`.
    pub jsonrpc: String,
    /// Correlation id copied from the request.
    pub id: AcpRequestId,
    /// Successful response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Error response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpErrorObject>,
}

impl AcpResponse {
    /// Build a successful JSON-RPC response.
    pub fn success(id: impl Into<AcpRequestId>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    /// Build a failed JSON-RPC response.
    pub fn failure(id: impl Into<AcpRequestId>, error: AcpErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }

    /// Return the validated response payload.
    pub fn payload(&self) -> Result<AcpResponsePayload, AcpError> {
        match (&self.result, &self.error) {
            (Some(result), None) => Ok(AcpResponsePayload::Result(result.clone())),
            (None, Some(error)) => Ok(AcpResponsePayload::Error(error.clone())),
            (None, None) => Err(AcpError::InvalidMessage(
                "response must contain result or error".to_owned(),
            )),
            (Some(_), Some(_)) => Err(AcpError::InvalidMessage(
                "response cannot contain both result and error".to_owned(),
            )),
        }
    }
}

/// Any ACP JSON-RPC envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpMessage {
    /// A method request that expects a response.
    Request(AcpRequest),
    /// A response to an earlier request.
    Response(AcpResponse),
    /// A notification that does not expect a response.
    Notification(AcpNotification),
}

impl AcpMessage {
    /// Validate the JSON-RPC envelope invariants this crate depends on.
    pub fn validate(&self) -> Result<(), AcpError> {
        match self {
            Self::Request(request) => {
                validate_jsonrpc(&request.jsonrpc)?;
                if request.method.trim().is_empty() {
                    return Err(AcpError::InvalidMessage(
                        "request method cannot be empty".to_owned(),
                    ));
                }
            }
            Self::Response(response) => {
                validate_jsonrpc(&response.jsonrpc)?;
                response.payload()?;
            }
            Self::Notification(notification) => {
                validate_jsonrpc(&notification.jsonrpc)?;
                if notification.method.trim().is_empty() {
                    return Err(AcpError::InvalidMessage(
                        "notification method cannot be empty".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Stateless ACP frame codec for newline-delimited JSON-RPC messages.
pub struct AcpWireCodec;

impl AcpWireCodec {
    /// Serialize an ACP message as one compact JSON object followed by `\n`.
    pub fn encode_message(message: &AcpMessage) -> Result<Vec<u8>, AcpError> {
        message.validate()?;
        let json = serde_json::to_string(message)?;
        if json.as_bytes().iter().any(|b| matches!(b, b'\n' | b'\r')) {
            return Err(AcpError::EmbeddedNewline);
        }
        let mut frame = json.into_bytes();
        frame.push(b'\n');
        Ok(frame)
    }

    /// Decode one newline-delimited ACP frame.
    pub fn decode_line(line: &[u8]) -> Result<AcpMessage, AcpError> {
        let line = strip_line_ending(line);
        if line.is_empty() {
            return Err(AcpError::EmptyFrame);
        }
        if line.iter().any(|b| matches!(b, b'\n' | b'\r')) {
            return Err(AcpError::EmbeddedNewline);
        }
        let text = str::from_utf8(line)?;
        let value: serde_json::Value = serde_json::from_str(text)?;
        if !value.is_object() {
            return Err(AcpError::NonObjectFrame);
        }
        let message: AcpMessage = serde_json::from_value(value)?;
        message.validate()?;
        Ok(message)
    }
}

/// Async reader for ACP newline-delimited frames.
pub struct AcpFrameReader<R> {
    inner: R,
}

impl<R> AcpFrameReader<R> {
    /// Wrap an async buffered reader.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Return the wrapped reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R> AcpFrameReader<R>
where
    R: AsyncBufRead + Unpin,
{
    /// Read the next ACP message, returning `None` at EOF.
    pub async fn read_message(&mut self) -> Result<Option<AcpMessage>, AcpError> {
        let mut frame = Vec::new();
        let n = self.inner.read_until(b'\n', &mut frame).await?;
        if n == 0 {
            return Ok(None);
        }
        AcpWireCodec::decode_line(&frame).map(Some)
    }
}

/// Async writer for ACP newline-delimited frames.
pub struct AcpFrameWriter<W> {
    inner: W,
}

impl<W> AcpFrameWriter<W> {
    /// Wrap an async writer.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Return the wrapped writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W> AcpFrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    /// Write one ACP message and flush the writer.
    pub async fn write_message(&mut self, message: &AcpMessage) -> Result<(), AcpError> {
        let frame = AcpWireCodec::encode_message(message)?;
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

fn validate_jsonrpc(version: &str) -> Result<(), AcpError> {
    if version == JSONRPC_VERSION {
        Ok(())
    } else {
        Err(AcpError::InvalidJsonRpcVersion(version.to_owned()))
    }
}

fn strip_line_ending(mut line: &[u8]) -> &[u8] {
    if line.ends_with(b"\n") {
        line = &line[..line.len() - 1];
    }
    if line.ends_with(b"\r") {
        line = &line[..line.len() - 1];
    }
    line
}
