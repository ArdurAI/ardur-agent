//! §6.0 Phase-2 — the MCP *server* surface: an [`ArdurMcpServer`] fronting a
//! seeded registry, exercised end-to-end over the Streamable-HTTP transport, and
//! the bearer-token admission helpers the HTTP layer gates on.

mod common;

use std::sync::Arc;

use common::{CapTool, registry_with_examples, spawn_mcp_server, test_context};

use ardur_tool_registry::{
    Capability, EchoTool, RemoteMcpToolset, ToolId, ToolRegistry, bearer_token_allowed,
    extract_bearer_token,
};
use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::json;

/// `tools/list` reflects exactly the tools registered locally.
#[tokio::test]
async fn tools_list_returns_registered_tools() {
    let url = spawn_mcp_server(registry_with_examples()).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to MCP server");

    let mut names = toolset.list_tool_names().await.expect("tools/list");
    names.sort();
    assert_eq!(names, vec!["echo".to_string(), "health_check".to_string()]);
}

/// Capability-bearing tools require a fused-runtime cap-token/Cedar context and
/// must not be advertised by the direct MCP server path while it uses an ambient
/// empty cap-token context.
#[tokio::test]
async fn tools_list_filters_capability_bearing_tools_without_mcp_cap_context() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(EchoTool::new()))
        .expect("register echo");
    registry
        .register(Box::new(CapTool::new(
            "dangerous.network",
            vec![Capability::NetworkOut],
        )))
        .expect("register capability-bearing tool");
    let url = spawn_mcp_server(Arc::new(registry)).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to MCP server");

    let mut names = toolset.list_tool_names().await.expect("tools/list");
    names.sort();
    assert_eq!(names, vec!["echo".to_string()]);
}

/// `tools/call` dispatches to the matching local tool and returns its output —
/// here the echo tool round-trips its arguments.
#[tokio::test]
async fn tools_call_invokes_local_tool() {
    let url = spawn_mcp_server(registry_with_examples()).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to MCP server");

    let tools = toolset.into_tools().await.expect("fetch tools");
    let echo = tools
        .iter()
        .find(|t| t.id() == ToolId::new("echo"))
        .expect("echo tool present");

    let args = json!({ "hello": "world" });
    let out = echo
        .invoke(&test_context(), args.clone())
        .await
        .expect("echo invocation");
    assert_eq!(out.content, args);
}

/// Closing the client session (which sends an MCP `DELETE`) is accepted by the
/// server's session manager without error.
#[tokio::test]
async fn delete_closes_session() {
    let url = spawn_mcp_server(registry_with_examples()).await;

    // Own the running service directly so the session can be explicitly closed.
    let transport = StreamableHttpClientTransport::from_uri(url);
    let service = ().serve(transport).await.expect("connect");
    // Drive one request so the session is actually established server-side.
    service.list_tools(None).await.expect("tools/list");

    // `cancel` tears the session down (DELETE on the wire); it must succeed.
    service.cancel().await.expect("session close");
}

/// The bearer allowlist rejects a token that is not configured.
#[test]
fn bearer_auth_rejects_unknown_token() {
    let allowlist = vec!["alpha-secret".to_string(), "beta-secret".to_string()];
    assert!(!bearer_token_allowed("not-a-known-token", &allowlist));
    // An empty allowlist admits nothing.
    assert!(!bearer_token_allowed("alpha-secret", &[]));
}

/// The bearer allowlist accepts a configured token, and the `Bearer ` scheme
/// prefix is stripped correctly.
#[test]
fn bearer_auth_accepts_configured_token() {
    let allowlist = vec!["alpha-secret".to_string(), "beta-secret".to_string()];
    assert!(bearer_token_allowed("beta-secret", &allowlist));

    let token = extract_bearer_token(Some("Bearer beta-secret")).expect("bearer token");
    assert_eq!(token, "beta-secret");
    assert!(bearer_token_allowed(token, &allowlist));

    // A non-Bearer / absent header yields no token.
    assert_eq!(extract_bearer_token(Some("Basic abc")), None);
    assert_eq!(extract_bearer_token(None), None);
}
