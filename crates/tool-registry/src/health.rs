//! [`HealthCheckTool`] — a capability-free [`Tool`] that reports liveness: the
//! process uptime, the selected provider id, and the memory backend in use.
//!
//! It is the second of the two example tools shipped over MCP (alongside
//! [`EchoTool`](crate::EchoTool)). Where echo proves a trivial argument
//! round-trip, health-check proves a tool that synthesizes server-side state
//! into a structured result.

use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// A tool that returns the server's [`HealthStatus`](Self::ID): seconds of
/// uptime since the tool was constructed, the configured provider id, and the
/// memory backend label.
///
/// Like [`EchoTool`](crate::EchoTool) it requires no [`Capability`] — it reads
/// only ambient server metadata — so it is safe to expose over MCP unguarded.
pub struct HealthCheckTool {
    schema: ToolSchema,
    started: Instant,
    provider: String,
    memory_backend: String,
}

impl HealthCheckTool {
    /// The id [`HealthCheckTool`] registers under.
    pub const ID: &'static str = "health_check";

    /// Construct a [`HealthCheckTool`].
    ///
    /// `provider` is the selected provider id (e.g. `"anthropic"`) and
    /// `memory_backend` is a label for the memory store in use (e.g.
    /// `"in-memory"`). Uptime is measured from this call.
    pub fn new(provider: impl Into<String>, memory_backend: impl Into<String>) -> Self {
        let schema = ToolSchema {
            description: "Reports server health: uptime, provider, and memory backend.".to_string(),
            // No inputs — health is a server-state read.
            input_schema: json!({ "type": "object", "properties": {} }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string" },
                    "uptime_secs": { "type": "integer" },
                    "provider": { "type": "string" },
                    "memory_backend": { "type": "string" }
                },
                "required": ["status", "uptime_secs", "provider", "memory_backend"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            started: Instant::now(),
            provider: provider.into(),
            memory_backend: memory_backend.into(),
        }
    }
}

#[async_trait]
impl Tool for HealthCheckTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let status = json!({
            "status": "ok",
            "uptime_secs": self.started.elapsed().as_secs(),
            "provider": self.provider,
            "memory_backend": self.memory_backend,
        });
        Ok(ToolOutput {
            content: status.clone(),
            cost: CostTuple::default(),
            receipt_data: status,
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}
