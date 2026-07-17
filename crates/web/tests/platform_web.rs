use ardur_runtime::SessionId;
use ardur_tool_registry::{CapTokenRef, InvocationId, Tool, ToolContext, ToolError};
use ardur_web::{FormFillTool, HtmlParseTool, WebFetchTool, WebPolicy, WebScreenshotTool};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ctx() -> ToolContext {
    let mut env = HashMap::new();
    env.insert("ARDUR_CEDAR_DECISION".to_string(), "allow".to_string());
    ToolContext {
        cap_token: CapTokenRef("web-cap".to_string()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("/tmp"),
        env,
        cost_budget_cents: 100,
    }
}

#[tokio::test]
async fn web_fetch_allows_https_and_loopback_dev_only_and_receipts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<title>Ardur</title><h1>Hello</h1>"),
        )
        .mount(&server)
        .await;

    let tool = WebFetchTool::new(WebPolicy::dev_loopback());
    let output = tool
        .invoke(&ctx(), json!({"url": format!("{}/page", server.uri())}))
        .await
        .expect("loopback HTTP allowed in dev policy");

    assert_eq!(output.content["status"], 200);
    assert!(output.content["body"].as_str().unwrap().contains("Ardur"));
    assert_eq!(output.receipt_data["receipt"]["action"], "web.fetch");
    assert_eq!(output.receipt_data["policy"]["decision"], "allow");
}

#[tokio::test]
async fn web_fetch_rejects_plain_http_non_loopback() {
    let tool = WebFetchTool::new(WebPolicy::default().with_allowlist(vec!["example.com"]));
    let err = tool
        .invoke(&ctx(), json!({"url": "http://example.com/"}))
        .await
        .expect_err("plain http external URL denied");
    assert!(matches!(err, ToolError::Denied { .. }));
}

#[tokio::test]
async fn html_parse_extracts_title_text_links_and_forms() {
    let tool = HtmlParseTool::new();
    let output = tool
        .invoke(
            &ctx(),
            json!({"html": "<html><head><title>T</title></head><body><h1 id='main'>Hi</h1><a href='/x'>X</a><form action='/submit'><input name='email'></form></body></html>", "selector": "h1"}),
        )
        .await
        .expect("html parse succeeds");

    assert_eq!(output.content["title"], "T");
    assert_eq!(output.content["selection"][0], "Hi");
    assert_eq!(output.content["links"][0]["href"], "/x");
    assert_eq!(output.content["forms"][0]["action"], "/submit");
}

#[tokio::test]
async fn web_screenshot_captures_mock_png_receipt() {
    let tool = WebScreenshotTool::mock(WebPolicy::dev_loopback());
    let output = tool
        .invoke(
            &ctx(),
            json!({"url": "https://example.com/", "confirmed": true}),
        )
        .await
        .expect("mock screenshot succeeds");

    assert_eq!(output.content["format"], "png");
    assert!(output.content["data_base64"].as_str().unwrap().len() > 10);
    assert_eq!(output.receipt_data["receipt"]["action"], "web.screenshot");
}

#[tokio::test]
async fn form_fill_requires_confirmation_for_submit() {
    let tool = FormFillTool::mock(WebPolicy::dev_loopback());
    let err = tool
        .invoke(
            &ctx(),
            json!({"url": "https://example.com/form", "fields": {"email": "a@example.com"}, "submit": true, "confirmed": false}),
        )
        .await
        .expect_err("submit without confirmation denied");
    assert!(format!("{err}").contains("confirmation required"));

    let output = tool
        .invoke(
            &ctx(),
            json!({"url": "https://example.com/form", "fields": {"email": "a@example.com"}, "submit": true, "confirmed": true}),
        )
        .await
        .expect("confirmed submit succeeds");
    assert_eq!(output.content["submitted"], true);
    assert_eq!(output.receipt_data["receipt"]["action"], "web.form_fill");
}
