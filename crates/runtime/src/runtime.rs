//! The interactive chat runtime: submit a batch of messages to run one turn.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::types::{CapTokenRef, ChatMessage, CostTuple, ProviderId, ReceiptId, Role, SessionId};

/// A request to run one turn against the runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitRequest {
    /// The conversation so far, oldest first; the last user message is the
    /// prompt to respond to.
    pub messages: Vec<ChatMessage>,
    /// The capability token authorizing this turn.
    pub cap_token: CapTokenRef,
    /// The session this turn belongs to.
    pub session_id: SessionId,
    /// An optional explicit provider; `None` lets the runtime choose.
    pub requested_provider: Option<ProviderId>,
}

/// The outcome of a successful [`ChatRuntime::submit`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResult {
    /// The id of the receipt emitted for this turn.
    pub receipt_id: ReceiptId,
    /// The assistant's response message.
    pub response: ChatMessage,
    /// The cost charged for this turn.
    pub cost: CostTuple,
}

/// The interactive chat runtime: submit a batch of messages to run one turn,
/// receiving the response, its receipt id, and its cost.
pub trait ChatRuntime {
    /// Run a single turn: validate the request, produce a response, and return
    /// the receipt id and cost.
    fn submit(
        &self,
        req: SubmitRequest,
    ) -> impl Future<Output = Result<SubmitResult, RuntimeError>>;
}

/// An in-memory [`ChatRuntime`] that echoes the last user message back. It
/// signs no receipts and charges no cost — it exists to exercise the runtime
/// contract end-to-end before the provider and receipt wiring lands.
#[derive(Clone, Debug, Default)]
pub struct InMemoryRuntime;

impl InMemoryRuntime {
    /// Construct the echo runtime.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ChatRuntime for InMemoryRuntime {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult, RuntimeError> {
        if req.cap_token.0.is_empty() {
            return Err(RuntimeError::CapTokenMissing);
        }
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .ok_or_else(|| RuntimeError::Internal(anyhow::anyhow!("no user message to echo")))?;
        Ok(SubmitResult {
            // TODO §1.0 Phase 2: replace this minted placeholder with the id of
            // a receipt signed by ardur-receipt.
            receipt_id: ReceiptId::new(),
            response: ChatMessage::assistant(last_user.content.clone()),
            cost: CostTuple::default(),
        })
    }
}
