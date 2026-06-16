//! ACP transport abstractions.

use async_trait::async_trait;

use crate::error::AcpError;
use crate::wire::AcpMessage;

/// The concrete transport family carrying ACP JSON-RPC frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpTransportKind {
    /// Standard input/output newline-delimited JSON-RPC.
    Stdio,
    /// HTTP server-sent events plus request/response calls.
    HttpSse,
    /// WebSocket JSON-RPC frames.
    WebSocket,
    /// In-process test or embedding transport.
    InProcess,
    /// A transport family not yet modeled by this crate.
    Other(String),
}

/// A bidirectional ACP transport.
///
/// Object-safe via [`async_trait`] so ACP peers can be registered behind trait
/// objects while stdio, HTTP/SSE, WebSocket, and test transports evolve
/// independently.
#[async_trait]
pub trait AcpTransport: Send + Sync {
    /// Send one ACP JSON-RPC message.
    async fn send(&self, message: AcpMessage) -> Result<(), AcpError>;

    /// Receive the next ACP JSON-RPC message, or `None` when the peer closes.
    async fn receive(&self) -> Result<Option<AcpMessage>, AcpError>;

    /// Return the concrete transport family.
    fn kind(&self) -> AcpTransportKind;
}
