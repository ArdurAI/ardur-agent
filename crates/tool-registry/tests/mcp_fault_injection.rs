//! Fault-injection tests for the resilience layer wired into the MCP client
//! (`ardur-resilience`, via `RemoteMcpToolset::connect_with`): a hung remote
//! tool call is timeout-bounded rather than hanging forever, and repeated
//! transport failures trip the shared circuit breaker so later calls fail
//! fast without waiting out the timeout again.

mod common;

use std::time::Duration;

use common::{registry_with_slow_tool, spawn_mcp_server, test_context};

use ardur_resilience::circuit_breaker::CircuitBreakerConfig;
use ardur_tool_registry::{McpResilienceConfig, RemoteMcpToolset, ToolId};
use serde_json::json;

#[tokio::test]
async fn slow_tool_call_times_out_rather_than_hanging_forever() {
    let url = spawn_mcp_server(registry_with_slow_tool(Duration::from_secs(5))).await;
    let toolset = RemoteMcpToolset::connect_with(
        url,
        None,
        McpResilienceConfig {
            call_timeout: Duration::from_millis(20),
            ..Default::default()
        },
    )
    .await
    .expect("connect to shim server");

    let tools = toolset.into_tools().await.expect("fetch tools");
    let slow = tools
        .iter()
        .find(|t| t.id() == ToolId::new("slow"))
        .expect("slow tool");

    let err = tokio::time::timeout(
        Duration::from_secs(2),
        slow.invoke(&test_context(), json!({})),
    )
    .await
    .expect("the call itself must not hang past the configured call timeout")
    .expect_err("a 5s sleep against a 20ms timeout must time out");
    assert!(format!("{err}").contains("timed out"), "got {err:?}");
}

#[tokio::test]
async fn repeated_timeouts_trip_the_breaker_and_further_calls_fail_fast() {
    let url = spawn_mcp_server(registry_with_slow_tool(Duration::from_secs(5))).await;
    let toolset = RemoteMcpToolset::connect_with(
        url,
        None,
        McpResilienceConfig {
            call_timeout: Duration::from_millis(20),
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 2,
                open_duration: Duration::from_secs(60),
            },
            ..Default::default()
        },
    )
    .await
    .expect("connect to shim server");

    let tools = toolset.into_tools().await.expect("fetch tools");
    let slow = tools
        .iter()
        .find(|t| t.id() == ToolId::new("slow"))
        .expect("slow tool");

    // Two timeouts trip the breaker (threshold 2).
    for _ in 0..2 {
        let err = slow
            .invoke(&test_context(), json!({}))
            .await
            .expect_err("times out");
        assert!(format!("{err}").contains("timed out"), "got {err:?}");
    }

    // A third call must fail fast — well under the 20ms call timeout —
    // because the breaker is now open and never invokes the tool at all.
    let start = std::time::Instant::now();
    let err = slow
        .invoke(&test_context(), json!({}))
        .await
        .expect_err("breaker is open");
    assert!(
        start.elapsed() < Duration::from_millis(15),
        "an open breaker must fail fast, not wait out the call timeout again: took {:?}",
        start.elapsed()
    );
    assert!(
        format!("{err}").contains("circuit breaker open"),
        "got {err:?}"
    );
}
