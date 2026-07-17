//! ACP transport abstractions.

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::Mutex;

use crate::error::AcpError;
use crate::wire::{AcpFrameReader, AcpFrameWriter, AcpMessage};

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

/// ACP transport over newline-delimited JSON-RPC on a stdio-like reader/writer.
///
/// The generic form is intentionally testable with `tokio::io::duplex` and can
/// also wrap a child process's `stdout` (buffered) and `stdin`. The transport is
/// internally synchronized so callers can keep it behind `Arc<dyn AcpTransport>`
/// and call the object-safe [`AcpTransport::send`] / [`AcpTransport::receive`]
/// methods through shared references.
pub struct StdioAcpTransport<R, W> {
    reader: Mutex<AcpFrameReader<R>>,
    writer: Mutex<AcpFrameWriter<W>>,
}

impl<R, W> StdioAcpTransport<R, W> {
    /// Wrap a buffered reader and writer as an ACP stdio transport.
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Mutex::new(AcpFrameReader::new(reader)),
            writer: Mutex::new(AcpFrameWriter::new(writer)),
        }
    }
}

impl<R, W> std::fmt::Debug for StdioAcpTransport<R, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioAcpTransport")
            .field("kind", &AcpTransportKind::Stdio)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<R, W> AcpTransport for StdioAcpTransport<R, W>
where
    R: AsyncBufRead + Unpin + Send + Sync + 'static,
    W: AsyncWrite + Unpin + Send + Sync + 'static,
{
    async fn send(&self, message: AcpMessage) -> Result<(), AcpError> {
        self.writer.lock().await.write_message(&message).await
    }

    async fn receive(&self) -> Result<Option<AcpMessage>, AcpError> {
        self.reader.lock().await.read_message().await
    }

    fn kind(&self) -> AcpTransportKind {
        AcpTransportKind::Stdio
    }
}
