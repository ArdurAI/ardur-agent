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

/// An opaque handle to the capability token authorizing a session's turns.
// TODO §1.0 Phase 2: replace the string handle with a re-export of
// `ardur_cap_token`'s verified-token type so the runtime can attenuate scopes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapTokenRef(pub String);

/// A model-requested tool invocation.
///
/// Surfaced by a provider via its `FinishReason::ToolUse`, and replayed in the
/// assistant turn that requested it (on [`ChatMessage::tool_calls`]) so the next
/// provider call sees the same call ids the tool results echo back.
///
/// Owned here (rather than in `ardur-provider-runtime`) so [`ChatMessage`] can
/// carry it without a dependency cycle — the provider layer re-exports it via
/// `pub use ardur_runtime::ToolCall`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id of this call, echoed back when returning the result.
    pub id: String,
    /// Name of the tool the model wants to run (matches a registry tool id).
    pub name: String,
    /// Arguments the model passed, as raw JSON.
    pub arguments: serde_json::Value,
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
    /// A tool result fed back to the model after the assistant requested a tool
    /// call. The result content rides in [`ChatMessage::content`] and the call
    /// it answers in [`ChatMessage::tool_call_id`].
    Tool,
}

/// A single message in a chat exchange.
///
/// `tool_calls` and `tool_call_id` are additive (`#[serde(default)]`, omitted
/// when empty) so a plain text transcript serializes exactly as before: a
/// [`Role::Assistant`] turn that requested tools carries `tool_calls`, and a
/// [`Role::Tool`] result carries the `tool_call_id` it answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who authored the message.
    pub role: Role,
    /// The message's textual content.
    pub content: String,
    /// Tool calls this (assistant) message requested. Empty for an ordinary
    /// message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The id of the tool call this (tool) message answers. `None` for an
    /// ordinary message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Construct a [`Role::User`] message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Construct a [`Role::Assistant`] message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Construct a [`Role::System`] message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Construct a [`Role::Assistant`] message that requested `tool_calls`. The
    /// `content` is the assistant's accompanying text (often empty on a
    /// tool-use turn).
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Construct a [`Role::Tool`] result answering the call `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}
