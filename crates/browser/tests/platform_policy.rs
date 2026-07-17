use ardur_browser::{
    BrowserPolicy, CdpBrowser, ClickTool, ConfirmationLevel, NavigateTool, SharedBrowser,
    SiteAction,
};
use ardur_runtime::SessionId;
use ardur_tool_registry::{CapTokenRef, InvocationId, Tool, ToolContext, ToolError};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx() -> ToolContext {
    let mut env = HashMap::new();
    env.insert("ARDUR_CEDAR_DECISION".to_string(), "allow".to_string());
    ToolContext {
        cap_token: CapTokenRef("platform-cap".to_string()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("/tmp"),
        env,
        cost_budget_cents: 100,
    }
}

fn shared(policy: BrowserPolicy) -> Arc<tokio::sync::RwLock<SharedBrowser>> {
    Arc::new(tokio::sync::RwLock::new(SharedBrowser::new(
        CdpBrowser::mock(),
        policy,
    )))
}

#[tokio::test]
async fn browser_action_receipts_are_chained_across_actions() {
    let policy = BrowserPolicy::with_allowlist(vec![
        SiteAction::new("example.com", "navigate"),
        SiteAction::new("example.com", "click"),
    ])
    .with_confirmation(ConfirmationLevel::ExternalConsequences);
    let browser = shared(policy);

    let nav = NavigateTool::new(browser.clone());
    let click = ClickTool::new(browser.clone());

    let first = nav
        .invoke(
            &ctx(),
            json!({"url": "https://example.com", "confirmed": true}),
        )
        .await
        .expect("allowed navigation succeeds");
    let second = click
        .invoke(&ctx(), json!({"selector": "#ok", "confirmed": true}))
        .await
        .expect("allowed confirmed click succeeds");

    let first_id = first.receipt_data["receipt"]["id"].as_str().unwrap();
    assert_eq!(second.receipt_data["receipt"]["parent_id"], first_id);
    assert_eq!(second.receipt_data["policy"]["decision"], "allow");
}

#[tokio::test]
async fn browser_policy_blocks_disallowed_site_before_cdp() {
    let tool = NavigateTool::new(shared(BrowserPolicy::with_allowlist(vec![
        SiteAction::new("example.com", "navigate"),
    ])));

    let err = tool
        .invoke(
            &ctx(),
            json!({"url": "https://evil.example", "confirmed": true}),
        )
        .await
        .expect_err("non-allowlisted site is blocked");

    assert!(matches!(err, ToolError::Denied { .. }));
}

#[tokio::test]
async fn browser_sensitive_actions_require_human_confirmation() {
    let policy = BrowserPolicy::with_allowlist(vec![
        SiteAction::new("example.com", "navigate"),
        SiteAction::new("example.com", "click"),
    ])
    .with_confirmation(ConfirmationLevel::ExternalConsequences);
    let browser = shared(policy);
    NavigateTool::new(browser.clone())
        .invoke(
            &ctx(),
            json!({"url": "https://example.com", "confirmed": true}),
        )
        .await
        .unwrap();

    let err = ClickTool::new(browser)
        .invoke(&ctx(), json!({"selector": "#purchase", "confirmed": false}))
        .await
        .expect_err("click without confirmation is denied");

    assert!(format!("{err}").contains("confirmation required"));
}

#[tokio::test]
async fn browser_cap_token_and_cedar_gates_fail_closed() {
    let policy = BrowserPolicy::with_allowlist(vec![SiteAction::new("example.com", "navigate")]);
    let tool = NavigateTool::new(shared(policy));
    let mut denied_ctx = ctx();
    denied_ctx.cap_token = CapTokenRef(String::new());

    let err = tool
        .invoke(
            &denied_ctx,
            json!({"url": "https://example.com", "confirmed": true}),
        )
        .await
        .expect_err("missing cap-token fails closed");
    assert!(matches!(err, ToolError::CapabilityDenied(_)));

    let mut cedar_ctx = ctx();
    cedar_ctx
        .env
        .insert("ARDUR_CEDAR_DECISION".to_string(), "deny".to_string());
    let err = tool
        .invoke(
            &cedar_ctx,
            json!({"url": "https://example.com", "confirmed": true}),
        )
        .await
        .expect_err("Cedar deny fails closed");
    assert!(matches!(err, ToolError::Denied { .. }));
}
