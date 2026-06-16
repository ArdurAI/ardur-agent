//! ardur-acp — the §12.4 ACP / ACPx interoperability foundation.
//!
//! Plan family: §12.4
//! (`plans/12.4-acp-acpx-interoperability-blueprint.md`) plus the related
//! §8.7 ACP server and §5.9 external-agent runtime plans.
//!
//! # Phase 1 (this crate)
//!
//! - [`AcpWireCodec`] / [`AcpFrameReader`] / [`AcpFrameWriter`] — ACP v1
//!   JSON-RPC 2.0 frames over newline-delimited UTF-8, matching the current ACP
//!   stdio transport. This crate intentionally does **not** use LSP
//!   `Content-Length` framing.
//! - [`AcpMessage`] / [`AcpRequest`] / [`AcpResponse`] /
//!   [`AcpNotification`] — the protocol-neutral JSON-RPC envelope used by
//!   ACP transports and method buses.
//! - [`AcpTransport`] — an object-safe async transport contract for stdio,
//!   HTTP/SSE, WebSocket, or in-process adapters.
//! - [`AcpPeerInterop`] and supporting traits — the closed interop surface for
//!   peer discovery, outbound delegation, inbound task intake, and receipt
//!   verification.
//! - Method and receipt constants for the ACP v1 core methods and the §12.4
//!   audit verbs.
//!
//! This slice is a foundation, not the full §12.4 system. Gateway admission,
//! peer trust policy, signed receipt emission, official-schema adapters, and
//! ACPx extension negotiation remain later phases.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod interop;
mod method;
mod transport;
mod types;
mod wire;

pub use error::AcpError;
pub use interop::{
    AcpCrossPeerReceiptVerifier, AcpDelegationRouter, AcpPeerDiscovery, AcpPeerInterop,
};
pub use method::{
    ACP_METHOD_AUTHENTICATE, ACP_METHOD_FS_READ_TEXT_FILE, ACP_METHOD_FS_WRITE_TEXT_FILE,
    ACP_METHOD_INITIALIZE, ACP_METHOD_SESSION_CANCEL, ACP_METHOD_SESSION_LOAD,
    ACP_METHOD_SESSION_NEW, ACP_METHOD_SESSION_PROMPT, ACP_METHOD_SESSION_SET_MODE,
    ACP_NOTIFICATION_SESSION_UPDATE, ACP_PROTOCOL_VERSION, RECEIPT_ACP_PEER_DISCOVERED,
    RECEIPT_ACP_TASK_DELEGATED_OUT, RECEIPT_ACP_TASK_RECEIVED_IN, RECEIPT_ACP_TRUST_REFUSED,
};
pub use transport::{AcpTransport, AcpTransportKind};
pub use types::{
    AcpAuthPosture, AcpDelegationRequest, AcpDelegationResponse, AcpInboundTask, AcpOutboundTask,
    AcpPeer, AcpPeerId, AcpPeerState, AcpPeerTrustClass, AcpTarget,
};
pub use wire::{
    AcpErrorObject, AcpFrameReader, AcpFrameWriter, AcpMessage, AcpNotification, AcpRequest,
    AcpRequestId, AcpResponse, AcpResponsePayload, AcpWireCodec, JSONRPC_VERSION,
};

// Shared value types owned by §1.0, re-exported so ACP interop does not drift
// into a second incompatible runtime schema.
pub use ardur_runtime::{CapTokenRef, ChatMessage, ReceiptId, SessionId};
