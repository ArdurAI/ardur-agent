//! End-to-end tests of the `delegate_task` `Tool` adapter: args parsing, the
//! happy path through the real `ardur-multi-agent` substrate, receipt
//! chaining, and the denial path when the caller's cap-token cannot back a
//! child turn.

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId, KeyPair, PublicKey,
};
use ardur_delegate_tool::DelegateTaskTool;
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_tool_registry::{InvocationId, Tool, ToolContext, ToolError};
use serde_json::json;

const AUDIENCE: &str = "ardur";
const EXPIRY_UNIX: u64 = 4_000_000_000; // ~2096, far past any test's runtime.

/// Issue a parent cap-token granting `tools`, returning it (base64) alongside
/// the issuer root the tool must be constructed with.
fn parent_token(tools: &[&str], budget: u64) -> (String, PublicKey) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("spiffe://ardur/agent/parent-test".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: EXPIRY_UNIX,
                budget_remaining: budget,
                tool_allowlist: tools.iter().map(|t| t.to_string()).collect(),
            },
        )
        .expect("issue parent token");
    (token.to_base64().expect("encode parent token"), root)
}

fn ctx_with(cap_token: String, invocation_id: InvocationId) -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(cap_token),
        session_id: SessionId::new(),
        invocation_id,
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: 1_000,
    }
}

#[tokio::test]
async fn delegate_task_completes_and_chains_receipt_to_invocation_id() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let invocation_id = InvocationId::new();
    let ctx = ctx_with(token, invocation_id);

    let output = tool
        .invoke(&ctx, json!({ "goal": "summarize the incident report" }))
        .await
        .expect("delegate_task should complete");

    assert_eq!(output.content["outcome"], "completed");
    // The echo child runtime returns the user content back as the reply.
    assert_eq!(output.content["response"], "summarize the incident report");
    assert_eq!(output.content["cents_used"], 100);

    // The termination receipt's parent anchor is exactly this call's
    // invocation id — the link an auditor walks from the fused runtime's
    // ToolCallReceipt to this sub-agent's termination receipt.
    assert_eq!(
        output.receipt_data["parent_receipt_id"],
        invocation_id.0.to_string()
    );
    assert_eq!(output.cost.cents, 100);
}

#[tokio::test]
async fn delegate_task_honors_max_cost_cents_override() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let ctx = ctx_with(token, InvocationId::new());

    let output = tool
        .invoke(
            &ctx,
            json!({ "goal": "draft a changelog entry", "max_cost_cents": 5 }),
        )
        .await
        .expect("delegate_task should complete");

    assert_eq!(output.content["cents_used"], 5);
}

#[tokio::test]
async fn delegate_task_denies_at_the_concurrency_ceiling() {
    // A zero ceiling makes admission deterministically fail on the very first
    // call, with no dependence on real-time scheduling — the blueprint's
    // default budget envelope names `max_concurrency = 3`; this proves the
    // gate itself is enforced (and fails fast, spawning nothing) rather than
    // racing real concurrent calls against wall-clock timing.
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_max_concurrency(root, AUDIENCE, 0);
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "should never spawn" }))
        .await
        .expect_err("a call at the concurrency ceiling must be denied");

    match err {
        ToolError::Denied { reason } => {
            assert!(reason.contains("concurrency ceiling (0)"), "got: {reason}");
        }
        other => panic!("expected Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn delegate_task_releases_its_permit_after_completing() {
    // A ceiling of exactly one: the first call must succeed and, on
    // returning, must release its permit so a second, later call is not
    // permanently locked out.
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_max_concurrency(root, AUDIENCE, 1);

    let first = tool
        .invoke(
            &ctx_with(token.clone(), InvocationId::new()),
            json!({ "goal": "first" }),
        )
        .await
        .expect("first call should complete");
    assert_eq!(first.content["outcome"], "completed");

    let second = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "second" }),
        )
        .await
        .expect("second call should complete once the first released its permit");
    assert_eq!(second.content["outcome"], "completed");
}

#[tokio::test]
async fn delegate_task_rejects_empty_goal() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "" }))
        .await
        .expect_err("empty goal must be rejected");

    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn delegate_task_rejects_missing_goal_field() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "task_name": "no goal here" }))
        .await
        .expect_err("missing goal must be rejected");

    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn delegate_task_fails_when_parent_token_lacks_chat_submit() {
    // A parent token that never granted `chat.submit` in the first place:
    // attenuation only narrows, so the child cannot gain it either. The child
    // spawns, but its first (only) turn is denied at the real-wire boundary.
    let (token, root) = parent_token(&["some.other.tool"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "this should not run" }))
        .await
        .expect_err("a child without chat.submit must be denied");

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("cap-token denied") || msg.contains("denied"),
                "expected a cap-token denial reason, got: {msg}"
            );
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn delegate_task_rejects_undecodable_cap_token() {
    let (_token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::new(root, AUDIENCE);
    let ctx = ctx_with("not-a-real-cap-token".to_string(), InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "should never spawn" }))
        .await
        .expect_err("a malformed cap-token must be denied before spawn");

    assert!(matches!(err, ToolError::Denied { .. }));
}

#[test]
fn schema_requires_goal_and_advertises_the_delegate_capability() {
    let tool = DelegateTaskTool::new(KeyPair::new().public(), AUDIENCE);
    assert_eq!(tool.id().as_str(), "delegate_task");

    let schema = tool.schema();
    let required = schema.input_schema["required"]
        .as_array()
        .expect("required array");
    assert!(required.iter().any(|v| v == "goal"));

    assert_eq!(tool.required_capabilities().len(), 1);
}
