//! Scenario (§6.0 Phase 2): the MCP server surface, end to end.
//!
//! Boots the *real* `ardur-server` `AppState` + router with the MCP surface
//! enabled, serves it over a loopback socket, and:
//!
//! 1. connects a `RemoteMcpToolset` (the §6.0 MCP client) with the configured
//!    bearer token, fetches `tools/list`, and asserts both example tools are
//!    advertised;
//! 2. forwards a `tools/call` to `echo` and verifies the arguments round-trip;
//! 3. confirms a request with **no** bearer token is rejected with `401` before
//!    reaching the transport.
//!
//! This proves the whole bridge over a genuine network hop: axum routing, the
//! bearer gate, the rmcp Streamable-HTTP transport, and the registry dispatch
//! back into a local `Tool::invoke`.

use std::collections::HashMap;
use std::sync::Arc;

use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, Config, LogFormat, build_router};
use ardur_tool_registry::{
    CapTokenRef, InvocationId, RemoteMcpToolset, SessionId, ToolContext, ToolId,
};
use serde_json::json;

/// Boot the real server with MCP enabled and serve it on an ephemeral loopback
/// port. Returns the base MCP endpoint URL (`http://addr/mcp/ardur`).
async fn spawn_server(bearer: &str) -> String {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        anthropic_api_key: String::new(),
        slack_bot_token: "xoxb-e2e".to_string(),
        slack_signing_secret: "e2e-signing-secret-0000000000".to_string(),
        slack_app_id: "A0E2EMCP".to_string(),
        data_dir: data_dir.path().to_path_buf(),
        bind_addr: "127.0.0.1:0".to_string(),
        chat_bearer_tokens: vec!["e2e-chat-token".to_string()],
        dev_permissive_policy: true,
        model: "claude-opus-4-8".to_string(),
        cost_budget_cents: 10_000,
        cedar_policy_path: None,
        slack_base_url: None,
        channel_matrix: false,
        channel_discord: false,
        channel_telegram: false,
        log_format: LogFormat::Text,
        mcp_enabled: true,
        mcp_bearer_tokens: vec![bearer.to_string()],
        mcp_path_prefix: "/mcp".to_string(),
        mcp_remote_servers: Vec::new(),
        skills_dirs: Vec::new(),
        memory_backend: ardur_server::MemoryBackend::InMemory,
        qdrant_url: None,
    };
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let tools = Arc::new(ardur_server::example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("server boots with MCP enabled");
    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    // Keep the tempdir alive for the server's lifetime by leaking it into the
    // detached task — the process exits at test end.
    tokio::spawn(async move {
        let _keep = data_dir;
        let _ = axum::serve(listener, router).await;
    });

    format!("http://{addr}/mcp/ardur")
}

/// A throwaway context for invoking a client-wrapped tool.
fn test_context() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: std::env::current_dir().unwrap_or_default(),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

#[tokio::test]
async fn mcp_server_lists_and_calls_tools_with_bearer_and_rejects_without() {
    const BEARER: &str = "e2e-mcp-secret-token";
    let url = spawn_server(BEARER).await;

    // ---- 1 + 2. Authorized client: tools/list then tools/call(echo). ----
    let toolset = RemoteMcpToolset::connect(url.clone(), Some(BEARER.to_string()))
        .await
        .expect("authorized client connects");

    let mut names = toolset.list_tool_names().await.expect("tools/list");
    names.sort();
    assert_eq!(
        names,
        vec!["echo".to_string(), "health_check".to_string()],
        "the server advertises both example tools over MCP"
    );

    let tools = toolset.into_tools().await.expect("wrap remote tools");
    let echo = tools
        .iter()
        .find(|t| t.id() == ToolId::new("echo"))
        .expect("echo tool present");
    let args = json!({ "ping": "pong" });
    let out = echo
        .invoke(&test_context(), args.clone())
        .await
        .expect("tools/call(echo) forwards over MCP");
    assert_eq!(
        out.content, args,
        "echo round-trips its arguments over the wire"
    );

    // ---- 3. Unauthorized: no bearer token → 401 before the transport. ----
    let resp = reqwest::Client::new()
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"e2e","version":"1"}}}"#,
        )
        .send()
        .await
        .expect("request reaches the server");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a request with no bearer token is rejected with 401"
    );
}
