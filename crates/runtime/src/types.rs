//! The runtime's shared value types: the stable ids, the capability-token and
//! provider handles, the cost tuple, and the chat-message envelope every turn
//! is expressed in.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable, time-ordered identifier of a session (UUIDv7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Mint a fresh, time-ordered session id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable, time-ordered identifier of a single turn (UUIDv7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub Uuid);

impl TurnId {
    /// Mint a fresh, time-ordered turn id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifier of an emitted execution receipt.
// TODO §1.0 Phase 2: re-export `ardur_receipt`'s receipt id instead of minting
// a local UUID, once the runtime is wired to the receipt signer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReceiptId(pub Uuid);

impl ReceiptId {
    /// Mint a placeholder receipt id (UUIDv4). Phase 1 does not yet sign
    /// receipts, so the id is unlinked to any signed body.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ReceiptId {
    fn default() -> Self {
        Self::new()
    }
}

/// An opaque handle to the capability token authorizing a session's turns.
// TODO §1.0 Phase 2: replace the string handle with a re-export of
// `ardur_cap_token`'s verified-token type so the runtime can attenuate scopes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapTokenRef(pub String);

/// Identifier of a model provider (e.g. `"anthropic"`, `"openai"`).
// TODO §1.0 Phase 2: resolve against the provider registry rather than a bare
// string id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

/// The cost of admitting and running a single turn.
// TODO §1.0 Phase 2: re-export `ardur_receipt`'s `CostTuple` so runtime cost
// accounting and receipt cost accounting share one type and one schema.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostTuple {
    /// Prompt/input tokens billed.
    pub tokens_in: u64,
    /// Completion/output tokens billed.
    pub tokens_out: u64,
    /// Monetary cost in whole US cents.
    pub cents: u64,
    /// Wall-clock duration of the turn, in milliseconds.
    pub wall_ms: u64,
    /// Share of human attention consumed, conventionally `0.0..=1.0`.
    pub attention_score: f64,
}

/// The author of a [`ChatMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// A message from the end user.
    User,
    /// A message from the assistant/model.
    Assistant,
    /// A system/developer instruction.
    System,
}

/// A single message in a chat exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who authored the message.
    pub role: Role,
    /// The message's textual content.
    pub content: String,
}

impl ChatMessage {
    /// Construct a [`Role::User`] message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Construct a [`Role::Assistant`] message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    /// Construct a [`Role::System`] message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
}
