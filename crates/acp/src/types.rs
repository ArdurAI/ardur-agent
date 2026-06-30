//! ACP peer and delegation value types.

use ardur_runtime::{CapTokenRef, ChatMessage, ReceiptId, SessionId};
use serde::{Deserialize, Serialize};

use crate::method::{
    ACP_PROTOCOL_VERSION, RECEIPT_ACP_TASK_DELEGATED_OUT, RECEIPT_ACP_TASK_RECEIVED_IN,
};

/// Stable identifier for an ACP peer known to Ardur.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AcpPeerId(pub String);

impl AcpPeerId {
    /// Wrap a peer id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for AcpPeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an ACP peer can be reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AcpTarget {
    /// A local executable launched over stdio.
    StdioCommand {
        /// Program path or executable name.
        program: String,
        /// Command-line arguments.
        args: Vec<String>,
    },
    /// A remote HTTP/SSE ACP endpoint.
    HttpSse {
        /// Base endpoint URL.
        url: String,
    },
    /// A remote WebSocket ACP endpoint.
    WebSocket {
        /// WebSocket URL.
        url: String,
    },
    /// A logical in-process peer used by tests or embedding hosts.
    InProcess {
        /// Registry key of the in-process peer.
        name: String,
    },
}

/// Authentication posture negotiated or required for an ACP peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AcpAuthPosture {
    /// No peer authentication is available.
    None,
    /// A bearer-style credential is required, but the secret value is never
    /// carried in this type.
    BearerRequired,
    /// Mutual TLS or equivalent channel identity is required.
    MutualTlsRequired,
    /// Authentication is delegated to a local launcher or embedding host.
    HostDelegated,
}

/// Local trust classification for an ACP peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPeerTrustClass {
    /// Peer is trusted for the requested task class.
    Trusted,
    /// Peer is known but requires explicit operator approval.
    RequiresApproval,
    /// Peer is known but not authorized for delegation.
    Refused,
    /// Peer has not yet been classified.
    Unknown,
}

/// Runtime state of a peer relationship.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPeerState {
    /// Peer has been discovered but not initialized.
    Discovered,
    /// `initialize` completed and protocol version was accepted.
    Initialized,
    /// Authentication completed, if required.
    Authenticated,
    /// Peer is unavailable or was closed.
    Closed,
}

/// A discovered ACP peer and the posture Ardur applies to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpPeer {
    /// Stable local id for the peer.
    pub peer_id: AcpPeerId,
    /// Reachability target.
    pub target: AcpTarget,
    /// Selected ACP protocol major version.
    pub protocol_version: u16,
    /// Authentication posture for the peer.
    pub auth_posture: AcpAuthPosture,
    /// Local trust class.
    pub trust_class: AcpPeerTrustClass,
    /// Current relationship state.
    pub state: AcpPeerState,
    /// Raw peer capabilities from `initialize`.
    pub capabilities: serde_json::Value,
}

impl AcpPeer {
    /// Build a discovered peer with ACP v1 as the selected protocol version.
    pub fn discovered(peer_id: AcpPeerId, target: AcpTarget) -> Self {
        Self {
            peer_id,
            target,
            protocol_version: ACP_PROTOCOL_VERSION,
            auth_posture: AcpAuthPosture::None,
            trust_class: AcpPeerTrustClass::Unknown,
            state: AcpPeerState::Discovered,
            capabilities: serde_json::Value::Object(Default::default()),
        }
    }
}

/// A task Ardur delegates to an external ACP peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpOutboundTask {
    /// Local session that owns the delegation.
    pub session_id: SessionId,
    /// Capability handle authorizing the delegation.
    pub cap_token: CapTokenRef,
    /// Prompt or transcript slice sent to the peer.
    pub messages: Vec<ChatMessage>,
    /// Caller-supplied metadata for routing, audit, or ACPx extension fields.
    pub metadata: serde_json::Value,
    /// Receipt verb expected for the outbound delegation.
    pub receipt_verb: String,
}

impl AcpOutboundTask {
    /// Build an outbound task with the §12.4 delegation receipt verb.
    pub fn new(session_id: SessionId, cap_token: CapTokenRef, messages: Vec<ChatMessage>) -> Self {
        Self {
            session_id,
            cap_token,
            messages,
            metadata: serde_json::Value::Object(Default::default()),
            receipt_verb: RECEIPT_ACP_TASK_DELEGATED_OUT.to_owned(),
        }
    }
}

/// A task an external ACP peer submitted to Ardur.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpInboundTask {
    /// Peer that submitted the task.
    pub peer: AcpPeer,
    /// Local session assigned to the task, if one has already been created.
    pub session_id: Option<SessionId>,
    /// Capability handle presented or minted for the inbound task.
    pub cap_token: CapTokenRef,
    /// Prompt or transcript slice received from the peer.
    pub messages: Vec<ChatMessage>,
    /// Raw metadata retained for audit or ACPx extension fields.
    pub metadata: serde_json::Value,
    /// Receipt verb expected for the inbound task.
    pub receipt_verb: String,
}

impl AcpInboundTask {
    /// Build an inbound task with the §12.4 received-in receipt verb.
    pub fn new(peer: AcpPeer, cap_token: CapTokenRef, messages: Vec<ChatMessage>) -> Self {
        Self {
            peer,
            session_id: None,
            cap_token,
            messages,
            metadata: serde_json::Value::Object(Default::default()),
            receipt_verb: RECEIPT_ACP_TASK_RECEIVED_IN.to_owned(),
        }
    }
}

/// Request to delegate one task to one peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpDelegationRequest {
    /// Peer that should receive the task.
    pub peer: AcpPeer,
    /// Task payload to send.
    pub task: AcpOutboundTask,
}

/// Result of inbound or outbound ACP task admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcpDelegationResponse {
    /// Whether the task was accepted for execution or delivery.
    pub accepted: bool,
    /// Session associated with the accepted task.
    pub session_id: Option<SessionId>,
    /// Receipt id created by the admitting layer, once receipt signing is wired.
    pub receipt_id: Option<ReceiptId>,
    /// Raw ACP or ACPx response data retained for audit.
    pub body: serde_json::Value,
}

impl AcpDelegationResponse {
    /// Build an accepted response.
    pub fn accepted(session_id: Option<SessionId>, receipt_id: Option<ReceiptId>) -> Self {
        Self {
            accepted: true,
            session_id,
            receipt_id,
            body: serde_json::Value::Object(Default::default()),
        }
    }

    /// Build a refused response with structured reason data.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            session_id: None,
            receipt_id: None,
            body: serde_json::json!({ "reason": reason.into() }),
        }
    }
}
