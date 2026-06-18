//! ARD-420 — capability enforcement at tool.invoke() boundary.
//!
//! Each test registers a tool that declares `required_capabilities` and
//! verifies that the fused runtime denies invocation when the cap-token's
//! granted capabilities don't include the required one.

mod support;

use std::sync::Arc;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatRuntime, CostTuple, ProviderId, RuntimeError, ToolCall};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use serde_json::json;

use support::{mint_token_with_capabilities, runtime_builder, user_request};

// ---- scripted provider -----------------------------------------------------

struct ScriptedProvider {
    tool_name: String,
    rate_card: RateCard,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: String::new(),
            finish_reason: FinishReason::ToolUse(vec![ToolCall {
                id: "call-1".to_string(),
                name: self.tool_name.clone(),
                arguments: json!({}),
            }]),
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("scripted".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

// ---- capability-gated tools ------------------------------------------------

/// A tool that requires `Capability::ShellExec`.
struct ShellTool {
    schema: ToolSchema,
}

impl ShellTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Run a shell command".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> ToolId {
        ToolId::new("shell")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!("ok"),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::ShellExec]
    }
}

/// A tool that requires `Capability::FsRead`.
struct FileReadTool {
    schema: ToolSchema,
}

impl FileReadTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Read a file".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn id(&self) -> ToolId {
        ToolId::new("file.read")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!("ok"),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::FsRead]
    }
}

/// A tool that requires `Capability::FsWrite`.
struct FileWriteTool {
    schema: ToolSchema,
}

impl FileWriteTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Write a file".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn id(&self) -> ToolId {
        ToolId::new("file.write")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!("ok"),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::FsWrite]
    }
}

/// A tool that requires `Capability::NetworkOut`.
struct HttpFetchTool {
    schema: ToolSchema,
}

impl HttpFetchTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Fetch a URL".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for HttpFetchTool {
    fn id(&self) -> ToolId {
        ToolId::new("http.fetch")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!("ok"),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::NetworkOut]
    }
}

/// A tool that requires no capabilities — always allowed.
struct NoCapsTool {
    schema: ToolSchema,
}

impl NoCapsTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "A tool with no required capabilities".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for NoCapsTool {
    fn id(&self) -> ToolId {
        ToolId::new("no-caps")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!("ok"),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}

// ---- helper to build a runtime with tools + a token ------------------------

fn build_runtime(
    tool_name: &str,
    tool: Box<dyn Tool>,
    caps: &[&str],
) -> (ardur_fused_runtime::FusedRuntime, String) {
    let mut registry = ToolRegistry::new();
    let _ = registry.register(tool);
    let provider = Arc::new(ScriptedProvider {
        tool_name: tool_name.to_string(),
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let runtime = runtime_builder(provider)
        .with_tools(Arc::new(registry))
        .build()
        .expect("runtime builds");
    let token = mint_token_with_capabilities(caps);
    (runtime, token)
}

// ---- tests ------------------------------------------------------------------

#[tokio::test]
async fn shell_tool_denied_without_shell_exec() {
    let (runtime, token) = build_runtime("shell", Box::new(ShellTool::new()), &[]);
    let req = user_request("run ls", &token);
    let result = runtime.submit(req).await;
    assert!(
        matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "shell tool should be denied without ShellExec, got: {result:?}"
    );
}

#[tokio::test]
async fn file_read_denied_without_fs_read() {
    let (runtime, token) = build_runtime("file.read", Box::new(FileReadTool::new()), &[]);
    let req = user_request("read file", &token);
    let result = runtime.submit(req).await;
    assert!(
        matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "file.read should be denied without FsRead, got: {result:?}"
    );
}

#[tokio::test]
async fn file_write_denied_without_fs_write() {
    let (runtime, token) = build_runtime("file.write", Box::new(FileWriteTool::new()), &[]);
    let req = user_request("write file", &token);
    let result = runtime.submit(req).await;
    assert!(
        matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "file.write should be denied without FsWrite, got: {result:?}"
    );
}

#[tokio::test]
async fn http_fetch_denied_without_network_out() {
    let (runtime, token) = build_runtime("http.fetch", Box::new(HttpFetchTool::new()), &[]);
    let req = user_request("fetch url", &token);
    let result = runtime.submit(req).await;
    assert!(
        matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "http.fetch should be denied without NetworkOut, got: {result:?}"
    );
}

#[tokio::test]
async fn no_caps_tool_always_allowed() {
    let (runtime, token) = build_runtime("no-caps", Box::new(NoCapsTool::new()), &[]);
    let req = user_request("do something", &token);
    let result = runtime.submit(req).await;
    assert!(
        !matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "no-caps tool should never be denied, got: {result:?}"
    );
}

#[tokio::test]
async fn shell_tool_allowed_with_shell_exec() {
    let (runtime, token) = build_runtime("shell", Box::new(ShellTool::new()), &["ShellExec"]);
    let req = user_request("run ls", &token);
    let result = runtime.submit(req).await;
    assert!(
        !matches!(result, Err(RuntimeError::CapabilityDenied { .. })),
        "shell tool should be allowed when ShellExec is granted, got: {result:?}"
    );
}
