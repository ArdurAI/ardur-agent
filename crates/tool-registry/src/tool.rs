//! The tool contract: the [`Tool`] trait every tool implements, plus the value
//! types a tool call speaks in — its [`ToolSchema`], the [`ToolContext`] it runs
//! against, and the [`ToolOutput`] it returns.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ardur_runtime::{CapTokenRef, CostTuple, SessionId};

use crate::capability::Capability;
use crate::error::ToolError;

/// Stable identifier of a tool, e.g. `"fs.read"` or `"shell.exec"`.
///
/// The registry is keyed by this id; it is a plain string newtype so tool
/// authors can choose a stable, human-readable name rather than a minted UUID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolId(pub String);

impl ToolId {
    /// Construct a [`ToolId`] from anything string-like.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier of a single tool invocation (UUIDv4).
///
/// Minted per call so a tool's work can be correlated with the §11.14 receipt
/// it contributes to. Phase 1 mints a random v4 id unlinked to any signed body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InvocationId(pub Uuid);

impl InvocationId {
    /// Mint a fresh invocation id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InvocationId {
    fn default() -> Self {
        Self::new()
    }
}

/// A worked example of a tool call, surfaced to the model when prompting it to
/// choose and fill in a tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExample {
    /// What this example demonstrates, in one line.
    pub description: String,
    /// Example arguments, matching the tool's `input_schema`.
    pub args: serde_json::Value,
    /// The output those arguments produce, matching the tool's `output_schema`.
    pub output: serde_json::Value,
}

/// A tool's self-description: what it does and the shape of its input and
/// output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// A human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema of the arguments [`Tool::invoke`] accepts.
    pub input_schema: serde_json::Value,
    /// JSON Schema of the [`ToolOutput::content`] the tool returns.
    pub output_schema: serde_json::Value,
    /// Worked examples used when prompting the model.
    pub examples: Vec<ToolExample>,
}

/// The ambient context a [`Tool::invoke`] runs against.
///
/// Carries the authorization handle, the owning session, the filesystem and
/// environment the tool may consult, and the remaining cost budget for this
/// call.
#[derive(Clone, Debug)]
pub struct ToolContext {
    /// The capability token authorizing this invocation.
    pub cap_token: CapTokenRef,
    /// The session this invocation belongs to.
    pub session_id: SessionId,
    /// Correlates this invocation with the receipt it contributes to.
    pub invocation_id: InvocationId,
    /// Working directory for filesystem-touching tools.
    pub cwd: PathBuf,
    /// Environment variables visible to the tool.
    pub env: HashMap<String, String>,
    /// Budget remaining for this call, in whole US cents.
    pub cost_budget_cents: u32,
}

/// The result of a successful [`Tool::invoke`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The tool's result, matching the schema's `output_schema`.
    pub content: serde_json::Value,
    /// What the call cost — tokens, wall-clock, and so on.
    pub cost: CostTuple,
    /// Data folded into the §11.14 receipt payload for this invocation.
    pub receipt_data: serde_json::Value,
}

/// A capability the runtime can register, gate, and invoke.
///
/// The trait is object-safe so the [`ToolRegistry`](crate::ToolRegistry) can
/// hold `Box<dyn Tool>` values; `async-trait` boxes the future
/// [`Tool::invoke`] returns.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's stable registry key.
    fn id(&self) -> ToolId;

    /// The tool's input/output schema and prompting examples.
    fn schema(&self) -> &ToolSchema;

    /// Run the tool against `ctx` with the given JSON `args`.
    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;

    /// The capabilities the authorization layer must grant before this tool may
    /// run. Enforced by the fused runtime before every invocation.
    fn required_capabilities(&self) -> &[Capability];
}
