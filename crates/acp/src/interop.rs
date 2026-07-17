//! ACP peer interoperability traits.

use async_trait::async_trait;

use crate::error::AcpError;
use crate::types::{
    AcpDelegationRequest, AcpDelegationResponse, AcpInboundTask, AcpPeer, AcpTarget,
};
use crate::{ReceiptId, SessionId};

/// Peer discovery and protocol-version admission.
#[async_trait]
pub trait AcpPeerDiscovery: Send + Sync {
    /// Discover a peer at `target` and return its initialized posture.
    async fn discover_peer(&self, target: AcpTarget) -> Result<AcpPeer, AcpError>;
}

/// Outbound delegation from Ardur to another ACP peer.
#[async_trait]
pub trait AcpDelegationRouter: Send + Sync {
    /// Delegate one outbound task to one peer.
    async fn delegate_task(
        &self,
        request: AcpDelegationRequest,
    ) -> Result<AcpDelegationResponse, AcpError>;
}

/// Cross-peer receipt verification.
#[async_trait]
pub trait AcpCrossPeerReceiptVerifier: Send + Sync {
    /// Verify that `receipt_id` satisfies the expected verb for `peer`.
    async fn verify_cross_peer_receipt(
        &self,
        peer: &AcpPeer,
        receipt_id: ReceiptId,
        expected_verb: &str,
    ) -> Result<bool, AcpError>;
}

/// Closed §12.4 interop surface for bidirectional ACP peer exchange.
#[async_trait]
pub trait AcpPeerInterop:
    AcpPeerDiscovery + AcpDelegationRouter + AcpCrossPeerReceiptVerifier + Send + Sync
{
    /// Admit a task submitted by an external ACP peer.
    async fn receive_task(&self, task: AcpInboundTask) -> Result<AcpDelegationResponse, AcpError>;

    /// Refuse a peer or task by policy and return the local refusal receipt id.
    async fn refuse_untrusted_peer(
        &self,
        peer: AcpPeer,
        session_id: Option<SessionId>,
        reason: String,
    ) -> Result<ReceiptId, AcpError>;
}
