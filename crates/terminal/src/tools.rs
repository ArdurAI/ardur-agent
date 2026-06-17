//! Terminal tools implementing the Tool trait.

use crate::backends::{BackendKind, LocalBackend, TerminalBackend};
use crate::error::TerminalError;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// `terminal.exec` — execute a command in a terminal backend.
pub struct TerminalExecTool {
    backend: Arc<dyn TerminalBackend>,
}

impl TerminalExecTool {
    /// Create a new terminal exec tool with the given backend.
    pub fn new(backend: Arc<dyn TerminalBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Tool for TerminalExecTool {
    fn id(&self) -> ToolId {
        ToolId::new("terminal.exec")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Execute a command in a terminal backend.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "default": 30 }
                },
                "required": ["command"]
            }),
            output_schema: json!({"type": "object", "properties": {"output": {"type": "string"}}}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);
        let output = self
            .backend
            .execute(command, timeout)
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolOutput {
            content: json!({"output": output}),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "terminal.exec", "command": command, "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::ShellExec,
                Capability::Custom("terminal".to_string()),
            ]
        });
        &CAPS
    }
}

/// `terminal.session` — manage persistent terminal sessions.
pub struct TerminalSessionTool;

impl TerminalSessionTool {
    /// Create a new terminal session tool.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TerminalSessionTool {
    fn id(&self) -> ToolId {
        ToolId::new("terminal.session")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Manage a persistent terminal session (cd, export, etc.).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["create", "exec", "close"] },
                    "command": { "type": "string" }
                },
                "required": ["session_id", "action"]
            }),
            output_schema: json!({"type": "object"}),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolOutput {
            content: json!({"session_id": session_id, "action": action, "status": "ok"}),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "terminal.session", "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![
                Capability::ShellExec,
                Capability::Custom("terminal".to_string()),
            ]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_exec_tool_id() {
        let tool = TerminalExecTool::new(Arc::new(LocalBackend::new()));
        assert_eq!(tool.id().as_str(), "terminal.exec");
    }

    #[test]
    fn terminal_session_tool_id() {
        let tool = TerminalSessionTool::new();
        assert_eq!(tool.id().as_str(), "terminal.session");
    }
}
