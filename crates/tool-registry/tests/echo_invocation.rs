//! §6.0 Phase 1 — invoking the `EchoTool` round-trips its input as its output.

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_tool_registry::{
    CapTokenRef, CostTuple, EchoTool, InvocationId, SessionId, Tool, ToolContext,
};
use serde_json::json;

fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef("test-token".to_string()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("/"),
        env: HashMap::new(),
        cost_budget_cents: 0,
    }
}

#[tokio::test]
async fn echo_returns_its_input_as_output() {
    let tool = EchoTool::new();
    let args = json!({ "msg": "hi" });

    let out = tool
        .invoke(&ctx(), args.clone())
        .await
        .expect("echo invocation succeeds");

    assert_eq!(out.content, args);
    assert_eq!(out.receipt_data, args);
    assert_eq!(out.cost, CostTuple::default());
}
