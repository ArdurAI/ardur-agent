//! Shared test fixtures: a configurable [`CapTool`] that carries an arbitrary
//! id and capability set so the registry's capability queries can be exercised,
//! plus the §6.0 Phase-2 MCP scaffolding (an in-process Streamable-HTTP server
//! fronting an [`ArdurMcpServer`], and a throwaway [`ToolContext`]).
//!
//! Included by several test binaries; not every binary uses every helper, so the
//! module suppresses the resulting dead-code warnings.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use ardur_tool_registry::{
    ArdurMcpServer, CapTokenRef, Capability, CostTuple, EchoTool, HealthCheckTool, InvocationId,
    SessionId, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

/// A no-op tool that advertises whatever capabilities it is built with.
pub struct CapTool {
    id: ToolId,
    caps: Vec<Capability>,
    schema: ToolSchema,
}

impl CapTool {
    /// Build a tool with the given id and required capabilities.
    pub fn new(id: &str, caps: Vec<Capability>) -> Self {
        Self {
            id: ToolId::new(id),
            caps,
            schema: ToolSchema {
                description: format!("test tool {id}"),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for CapTool {
    fn id(&self) -> ToolId {
        self.id.clone()
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
            content: json!({}),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

/// A registry holding the two example tools shipped over MCP: [`EchoTool`] and
/// [`HealthCheckTool`].
pub fn registry_with_examples() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(EchoTool::new()))
        .expect("register echo");
    registry
        .register(Box::new(HealthCheckTool::new("anthropic", "in-memory")))
        .expect("register health_check");
    Arc::new(registry)
}

/// A tool that sleeps for a fixed duration before returning — stands in for a
/// hung/slow remote tool so the client-side `tools/call` timeout (and, on
/// repeated failure, the circuit breaker) can be exercised deterministically.
pub struct SlowTool {
    id: ToolId,
    delay: std::time::Duration,
    schema: ToolSchema,
}

impl SlowTool {
    pub fn new(id: &str, delay: std::time::Duration) -> Self {
        Self {
            id: ToolId::new(id),
            delay,
            schema: ToolSchema {
                description: "sleeps before responding".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for SlowTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(self.delay).await;
        Ok(ToolOutput {
            content: json!({}),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}

/// A registry holding one [`SlowTool`] (id `"slow"`) that sleeps `delay`
/// before responding.
pub fn registry_with_slow_tool(delay: std::time::Duration) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(SlowTool::new("slow", delay)))
        .expect("register slow");
    Arc::new(registry)
}

/// Mount `registry` behind an [`ArdurMcpServer`] on a Streamable-HTTP transport,
/// serve it over an ephemeral loopback port, and return the MCP endpoint URL.
///
/// The server task is detached; it lives until the test process exits.
pub async fn spawn_mcp_server(registry: Arc<ToolRegistry>) -> String {
    let handler = ArdurMcpServer::new(registry);
    let service: StreamableHttpService<ArdurMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    format!("http://{addr}/mcp")
}

/// A throwaway [`ToolContext`] for invoking a client-wrapped tool in tests — no
/// real cap-token, a wide budget, the test's working directory.
pub fn test_context() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: std::env::current_dir().unwrap_or_default(),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}
