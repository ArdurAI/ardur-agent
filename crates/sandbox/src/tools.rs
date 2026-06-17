//! Sandbox tools implementing the Tool trait.

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::json;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use crate::sandbox::{Language, Sandbox, SandboxConfig};

/// `sandbox.exec` — execute code in a sandboxed environment.
pub struct SandboxExecTool {
    sandbox: Arc<Sandbox>,
}

impl SandboxExecTool {
    /// Create a new sandbox exec tool with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sandbox: Arc::new(Sandbox::new(SandboxConfig::default())),
        }
    }

    /// Create with a custom sandbox config.
    #[must_use]
    pub fn with_config(config: SandboxConfig) -> Self {
        Self {
            sandbox: Arc::new(Sandbox::new(config)),
        }
    }
}

#[async_trait]
impl Tool for SandboxExecTool {
    fn id(&self) -> ToolId {
        ToolId::new("sandbox.exec")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Execute code in a sandboxed environment (Python, JavaScript, Bash).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["python", "javascript", "bash"] },
                    "code": { "type": "string" },
                    "timeout_secs": { "type": "integer", "default": 30 }
                },
                "required": ["language", "code"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "exit_code": { "type": "integer" },
                    "timed_out": { "type": "boolean" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> std::result::Result<ToolOutput, ardur_tool_registry::ToolError> {
        let language_str = args.get("language").and_then(|v| v.as_str()).unwrap_or("");
        let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");

        let language = Language::parse(language_str)
            .ok_or_else(|| ardur_tool_registry::ToolError::ExecutionFailed(
                format!("unsupported language: {language_str}")
            ))?;

        let result = self.sandbox.execute(language, code).await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolOutput {
            content: json!({
                "stdout": result.stdout,
                "stderr": result.stderr,
                "exit_code": result.exit_code,
                "timed_out": result.timed_out,
                "duration_ms": result.duration_ms,
            }),
            cost: CostTuple::default(),
            receipt_data: json!({
                "action": "sandbox.exec",
                "language": language_str,
                "permitted": true,
            }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::ProcessSpawn, Capability::Custom("sandbox".to_string())]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_exec_tool_id() {
        let tool = SandboxExecTool::new();
        assert_eq!(tool.id().as_str(), "sandbox.exec");
    }

    #[test]
    fn sandbox_exec_tool_capabilities() {
        let tool = SandboxExecTool::new();
        let caps = tool.required_capabilities();
        assert!(caps.iter().any(|c| matches!(c, Capability::ProcessSpawn)));
        assert!(caps.iter().any(|c| matches!(c, Capability::Custom(s) if s == "sandbox")));
    }
}
