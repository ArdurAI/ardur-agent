//! [`CodeExecutionTool`] — the `code.exec` [`Tool`] impl that dispatches a
//! script to a [`LanguageAdapter`], attenuated by a [`CodeExecutionCaveat`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use ardur_injection_defense::{FilterRegistry, PatternBasedFilter, ScannableContent, Verdict};
use ardur_runtime::CostTuple;
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};

use crate::adapter::LanguageAdapter;
use crate::caveat::CodeExecutionCaveat;
use crate::receipt::{CodeExecutionReceipt, ReceiptKind};

/// The custom [`Capability`] every `code.exec` dispatch requires, in
/// addition to [`Capability::ProcessSpawn`].
#[must_use]
pub fn code_execution_capability() -> Capability {
    Capability::Custom("code_execution".to_string())
}

/// A `code.exec` request before cap-token-caveat attenuation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeExecutionRequest {
    /// The language to run — must match a registered [`LanguageAdapter::name`].
    pub language: String,
    /// The script body.
    pub code: String,
    /// Optional stdin piped to the child process.
    pub stdin: Option<String>,
    /// The caller's requested wall-clock ceiling, in seconds. Floored by the
    /// caveat's `max_timeout_secs`.
    pub timeout_secs: u64,
    /// Tools the caller states an intent to call back into. Intersected
    /// against the caveat's `permitted_tools`; Phase 1 does not yet dispatch
    /// these calls (see the `adapter` module's Phase 1 note).
    pub tool_allowlist: Vec<String>,
    /// Tools requested in `tool_allowlist` but dropped by attenuation.
    /// Populated by [`CodeExecutionCaveat::attenuate`]; callers should leave
    /// this empty on a fresh request.
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// Whether stdout reaches the caller. Defaults to `true` in the schema.
    pub expose_stdout: bool,
    /// Whether stderr reaches the caller. The caveat may force this `false`
    /// regardless of the caller's request.
    pub expose_stderr: bool,
}

fn default_true() -> bool {
    true
}

impl CodeExecutionRequest {
    fn from_args(args: &serde_json::Value) -> Result<Self, ToolError> {
        let language = args
            .get("language")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing `language`".to_string()))?
            .to_string();
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing `code`".to_string()))?
            .to_string();
        let stdin = args
            .get("stdin")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30);
        let tool_allowlist = args
            .get("tool_allowlist")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let expose_stdout = args
            .get("expose_stdout")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(default_true);
        let expose_stderr = args
            .get("expose_stderr")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        Ok(Self {
            language,
            code,
            stdin,
            timeout_secs,
            tool_allowlist,
            denied_tools: Vec::new(),
            expose_stdout,
            expose_stderr,
        })
    }
}

/// The `code.exec` [`Tool`].
///
/// Holds the closed set of [`LanguageAdapter`]s it may dispatch to and the
/// [`CodeExecutionCaveat`] every request is attenuated against before an
/// adapter runs.
pub struct CodeExecutionTool {
    adapters: HashMap<&'static str, Arc<dyn LanguageAdapter>>,
    caveat: CodeExecutionCaveat,
    injection_filters: FilterRegistry,
}

impl CodeExecutionTool {
    /// Build a tool over `adapters`, attenuating every dispatch against
    /// `caveat`. Registers the built-in pattern-based injection filter over
    /// captured stdout before it is returned to the caller.
    #[must_use]
    pub fn new(adapters: Vec<Arc<dyn LanguageAdapter>>, caveat: CodeExecutionCaveat) -> Self {
        let mut map = HashMap::new();
        for adapter in adapters {
            map.insert(adapter.name(), adapter);
        }
        let injection_filters = FilterRegistry::new();
        injection_filters.register(Arc::new(PatternBasedFilter::new()));
        Self {
            adapters: map,
            caveat,
            injection_filters,
        }
    }

    async fn scan_output(&self, tool_id: &ToolId, output: &str) -> Result<(), ToolError> {
        let content = ScannableContent::ToolOutput {
            tool_id: tool_id.clone(),
            output: json!(output),
        };
        let scanned = self
            .injection_filters
            .scan_all(&content)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e.to_string())))?;
        if let Verdict::Block { reason } = scanned.verdict {
            return Err(ToolError::Denied {
                reason: format!("captured output blocked by injection filter: {reason}"),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for CodeExecutionTool {
    fn id(&self) -> ToolId {
        ToolId::new("code.exec")
    }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Run a script in a permitted language and capture its output. \
                Only the script's stdout (and, if permitted, stderr) reaches the caller."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "language": {"type": "string", "description": "e.g. \"bash\" or \"python\""},
                    "code": {"type": "string"},
                    "stdin": {"type": "string"},
                    "timeout_secs": {"type": "integer", "default": 30},
                    "tool_allowlist": {"type": "array", "items": {"type": "string"}, "default": []},
                    "expose_stdout": {"type": "boolean", "default": true},
                    "expose_stderr": {"type": "boolean", "default": false}
                },
                "required": ["language", "code"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"},
                    "exit_code": {"type": "integer"},
                    "duration_ms": {"type": "integer"},
                    "tool_calls_made": {"type": "integer"},
                    "tool_calls_denied": {"type": "integer"}
                }
            }),
            examples: vec![],
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        // `code_execution_capability()` is not `'static` (it wraps an owned
        // `String`), so this crate keeps a lazily-built static slice rather
        // than allocating one per call.
        static CAPS: std::sync::OnceLock<[Capability; 2]> = std::sync::OnceLock::new();
        CAPS.get_or_init(|| [Capability::ProcessSpawn, code_execution_capability()])
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let request = CodeExecutionRequest::from_args(&args)?;
        let tool_id = self.id();

        let requested_receipt = CodeExecutionReceipt::new(
            ReceiptKind::Requested,
            request.language.clone(),
            "dispatch requested",
        );

        let attenuated = self
            .caveat
            .attenuate(&request)
            .map_err(|e| ToolError::Denied {
                reason: e.to_string(),
            })?;

        let mut tool_denied_receipts = Vec::new();
        for denied in &attenuated.denied_tools {
            tool_denied_receipts.push(
                CodeExecutionReceipt::new(
                    ReceiptKind::ToolDenied,
                    request.language.clone(),
                    denied.clone(),
                )
                .with_parent(requested_receipt.receipt_id.clone()),
            );
        }

        let adapter = self
            .adapters
            .get(attenuated.language.as_str())
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidArgs(format!("unsupported language: {}", attenuated.language))
            })?;

        let timeout = Duration::from_secs(attenuated.timeout_secs.max(1));
        let run = adapter
            .run(&attenuated.code, attenuated.stdin.as_deref(), timeout)
            .await;

        let outcome = match run {
            Ok(output) => output,
            Err(source) => {
                let failed = CodeExecutionReceipt::new(
                    ReceiptKind::Failed,
                    attenuated.language.clone(),
                    source.to_string(),
                )
                .with_parent(requested_receipt.receipt_id.clone());
                return Err(ToolError::ExecutionFailed(format!(
                    "{source} (receipt {})",
                    failed.receipt_id
                )));
            }
        };

        if attenuated.expose_stdout && !outcome.stdout.is_empty() {
            self.scan_output(&tool_id, &outcome.stdout).await?;
        }

        let completed_receipt = CodeExecutionReceipt::new(
            ReceiptKind::Completed,
            attenuated.language.clone(),
            format!("exit={}", outcome.exit_code),
        )
        .with_parent(requested_receipt.receipt_id.clone());

        let mut receipt_data = json!({
            "requested": requested_receipt.to_receipt_json(),
            "completed": completed_receipt.to_receipt_json(),
            "tool_denied": tool_denied_receipts
                .iter()
                .map(CodeExecutionReceipt::to_receipt_json)
                .collect::<Vec<_>>(),
        });
        if let Some(obj) = receipt_data.as_object_mut() {
            obj.insert(
                "tool_calls_made".to_string(),
                json!(0), // Phase 1: no tool-call RPC transport is wired yet.
            );
            obj.insert(
                "tool_calls_denied".to_string(),
                json!(tool_denied_receipts.len()),
            );
        }

        let content = json!({
            "stdout": if attenuated.expose_stdout { outcome.stdout.clone() } else { String::new() },
            "stderr": if attenuated.expose_stderr { outcome.stderr.clone() } else { String::new() },
            "exit_code": outcome.exit_code,
            "duration_ms": outcome.duration_ms,
            "tool_calls_made": 0,
            "tool_calls_denied": tool_denied_receipts.len(),
        });

        let cost = CostTuple {
            tokens_in: 0,
            tokens_out: 0,
            cents: 0,
            wall_ms: outcome.duration_ms,
            attention_score: 0,
        };

        Ok(ToolOutput {
            content,
            cost,
            receipt_data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::BashLanguageAdapter;
    use ardur_runtime::{CapTokenRef, SessionId};
    use std::path::PathBuf;

    fn tool() -> CodeExecutionTool {
        CodeExecutionTool::new(
            vec![Arc::new(BashLanguageAdapter)],
            CodeExecutionCaveat::permissive_default(),
        )
    }

    fn ctx() -> ToolContext {
        ToolContext {
            cap_token: CapTokenRef("test-token".to_string()),
            session_id: SessionId::new(),
            invocation_id: Default::default(),
            cwd: PathBuf::from("."),
            env: HashMap::new(),
            cost_budget_cents: 1_000,
        }
    }

    #[tokio::test]
    async fn runs_a_permitted_bash_script() {
        let output = tool()
            .invoke(&ctx(), json!({"language": "bash", "code": "echo hi"}))
            .await
            .expect("invoke succeeds");
        assert_eq!(output.content["stdout"], "hi\n");
        assert_eq!(output.content["exit_code"], 0);
    }

    #[tokio::test]
    async fn rejects_an_unsupported_language() {
        let err = tool()
            .invoke(&ctx(), json!({"language": "ruby", "code": "puts 1"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    #[tokio::test]
    async fn hides_stderr_by_default() {
        let output = tool()
            .invoke(&ctx(), json!({"language": "bash", "code": "echo err 1>&2"}))
            .await
            .expect("invoke succeeds");
        assert_eq!(output.content["stderr"], "");
    }

    #[tokio::test]
    async fn missing_code_is_invalid_args() {
        let err = tool()
            .invoke(&ctx(), json!({"language": "bash"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn denied_tool_allowlist_entries_are_receipted() {
        let output = tool()
            .invoke(
                &ctx(),
                json!({
                    "language": "bash",
                    "code": "echo hi",
                    "tool_allowlist": ["shell.run"]
                }),
            )
            .await
            .expect("invoke succeeds");
        assert_eq!(output.content["tool_calls_denied"], 1);
    }
}
