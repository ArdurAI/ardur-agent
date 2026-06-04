//! §6.0 Phase-2 — the MCP *client* surface: [`RemoteMcpToolset`] connects to a
//! remote MCP server (here an in-process [`ArdurMcpServer`] shim), discovers its
//! tools, and forwards `tools/call` over the wire as local [`Tool`] invocations.

mod common;

use common::{registry_with_examples, spawn_mcp_server, test_context};

use ardur_tool_registry::{RemoteMcpToolset, ToolId};
use serde_json::json;

/// The toolset fetches the shim server's `tools/list` and surfaces each entry as
/// a registerable local tool with its remote schema.
#[tokio::test]
async fn remote_toolset_fetches_tools_list_from_shim_server() {
    let url = spawn_mcp_server(registry_with_examples()).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to shim server");

    let tools = toolset.into_tools().await.expect("fetch tools");
    let mut ids: Vec<String> = tools.iter().map(|t| t.id().0.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["echo".to_string(), "health_check".to_string()]);

    // The schema rode across: the health tool advertises its description.
    let health = tools
        .iter()
        .find(|t| t.id() == ToolId::new("health_check"))
        .expect("health tool");
    assert!(health.schema().description.contains("health"));
}

/// Invoking a wrapped remote tool forwards a `tools/call` and returns the remote
/// result — the health tool's structured status comes back over the wire.
#[tokio::test]
async fn remote_tool_call_forwards_over_mcp() {
    let url = spawn_mcp_server(registry_with_examples()).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to shim server");

    let tools = toolset.into_tools().await.expect("fetch tools");
    let health = tools
        .iter()
        .find(|t| t.id() == ToolId::new("health_check"))
        .expect("health tool");

    let out = health
        .invoke(&test_context(), json!({}))
        .await
        .expect("forwarded tools/call");
    assert_eq!(out.content["status"], "ok");
    assert_eq!(out.content["provider"], "anthropic");
    assert_eq!(out.content["memory_backend"], "in-memory");
}
