//! Terminal tools implementing the Tool trait.

use crate::backends::{BackendKind, TerminalBackend};
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RECEIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn ensure_authorized(
    ctx: &ToolContext,
    cap: Capability,
) -> Result<(), ardur_tool_registry::ToolError> {
    if ctx.cap_token.0.trim().is_empty() {
        return Err(ardur_tool_registry::ToolError::CapabilityDenied(cap));
    }
    if ctx
        .env
        .get("ARDUR_CEDAR_DECISION")
        .is_some_and(|decision| decision.eq_ignore_ascii_case("deny"))
    {
        return Err(ardur_tool_registry::ToolError::Denied {
            reason: "Cedar policy denied terminal backend invocation".to_string(),
        });
    }
    Ok(())
}

fn backend_name(kind: BackendKind) -> &'static str {
    kind.as_str()
}

fn receipt(action: &str, backend: BackendKind, command: &str) -> serde_json::Value {
    let now = now_ms();
    let seq = RECEIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    json!({
        "receipt": {
            "id": format!("tr-{now}-{seq}"),
            "action": action,
            "backend": backend_name(backend),
            "command_digest": format!("len:{}", command.len()),
            "timestamp_ms": now
        },
        "policy": { "decision": "allow" },
        "action": action,
        "backend": backend_name(backend),
        "permitted": true
    })
}

/// `terminal.exec` — execute a command in a terminal backend.
pub struct TerminalExecTool {
    backend: Arc<dyn TerminalBackend>,
}

impl TerminalExecTool {
    /// Create a new terminal exec tool with the given backend.
    #[must_use]
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
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"},
                    "exit_code": {"type": "integer"},
                    "backend": {"type": "string"},
                    "duration_ms": {"type": "integer"},
                    "truncated": {"type": "boolean"}
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        ensure_authorized(ctx, Capability::ShellExec)?;
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);
        let result = self
            .backend
            .execute(command, timeout)
            .await
            .map_err(|e| match e {
                crate::TerminalError::PolicyDenied { reason } => {
                    ardur_tool_registry::ToolError::Denied { reason }
                }
                crate::TerminalError::Timeout { .. } => ardur_tool_registry::ToolError::Timeout,
                other => ardur_tool_registry::ToolError::ExecutionFailed(other.to_string()),
            })?;
        let content = json!({
            "stdout": result.stdout,
            "stderr": result.stderr,
            "exit_code": result.exit_code,
            "backend": backend_name(result.backend),
            "duration_ms": result.duration_ms,
            "truncated": result.truncated,
        });
        Ok(ToolOutput {
            content,
            cost: CostTuple::default(),
            receipt_data: receipt("terminal.exec", self.backend.kind(), command),
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
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for TerminalSessionTool {
    fn default() -> Self {
        Self::new()
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
        ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        // Authorize first so an unauthorized caller still sees a capability
        // denial rather than a not-implemented error.
        ensure_authorized(ctx, Capability::ShellExec)?;
        // The persistent-session backend does not exist yet. Previously this
        // returned `{"status":"ok"}` and minted a `permitted` receipt without
        // creating a session, running the command, or closing anything —
        // fabricating both a success and a signed audit record for work that
        // never happened. Fail honestly instead: return an explicit error so
        // the runtime mints no permitted receipt for the unperformed action.
        Err(ardur_tool_registry::ToolError::NotImplemented(
            "terminal.session: persistent session backend is not implemented".to_string(),
        ))
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
    use crate::backends::{LocalBackend, TerminalPolicy};

    #[test]
    fn terminal_exec_tool_id() {
        let tool = TerminalExecTool::new(Arc::new(LocalBackend::new(TerminalPolicy::permissive())));
        assert_eq!(tool.id().as_str(), "terminal.exec");
    }

    #[test]
    fn terminal_session_tool_id() {
        let tool = TerminalSessionTool::new();
        assert_eq!(tool.id().as_str(), "terminal.session");
    }

    fn ctx_with_token(token: &str) -> ToolContext {
        use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId};
        ToolContext {
            cap_token: CapTokenRef(token.to_string()),
            session_id: SessionId::new(),
            invocation_id: InvocationId::new(),
            cwd: std::path::PathBuf::from("."),
            env: std::collections::HashMap::new(),
            cost_budget_cents: u32::MAX,
        }
    }

    /// Regression for #354: `terminal.session` used to return `{"status":"ok"}`
    /// and mint a `permitted` receipt without creating a session or running the
    /// command. It must now fail honestly with `NotImplemented` so the runtime
    /// mints no permitted receipt for work that never ran.
    #[tokio::test]
    async fn terminal_session_fails_honestly_and_mints_no_receipt() {
        let tool = TerminalSessionTool::new();
        let ctx = ctx_with_token("cap-token");
        let result = tool
            .invoke(
                &ctx,
                json!({"session_id": "s1", "action": "exec", "command": "echo hi"}),
            )
            .await;
        match result {
            Err(ardur_tool_registry::ToolError::NotImplemented(_)) => {}
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    /// The capability check still fires before the not-implemented error, so an
    /// unauthorized caller sees a capability denial rather than leaking the
    /// not-implemented state.
    #[tokio::test]
    async fn terminal_session_denies_unauthorized_before_not_implemented() {
        let tool = TerminalSessionTool::new();
        let ctx = ctx_with_token("");
        let result = tool
            .invoke(&ctx, json!({"session_id": "s1", "action": "create"}))
            .await;
        assert!(matches!(
            result,
            Err(ardur_tool_registry::ToolError::CapabilityDenied(_))
        ));
    }
}
