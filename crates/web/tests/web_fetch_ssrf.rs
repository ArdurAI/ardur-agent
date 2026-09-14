//! RED-first guards for gh#364 gap 1: web.fetch must share http.fetch's
//! guarded transport instead of its weaker duplicate.
//!
//! Each test pins one guard that the pre-fix web.fetch lacked: redirects
//! re-validated per Location hop (reqwest's own follower is off), resolved
//! addresses vetted against internal/private IPs (DNS-rebind), and a hard
//! wall-clock timeout.
//! The evidence discipline requires these to FAIL against the unfixed code
//! and pass after the fix, with fault-injection proving each guard.

use ardur_runtime::SessionId;
use ardur_tool_registry::{CapTokenRef, InvocationId, Tool, ToolContext, ToolError};
use ardur_web::{WebFetchTool, WebPolicy};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
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

/// A host on the allowlist redirects to a host that is NOT on it. The fetch
/// must refuse the second hop, not follow it. `Denied` is distinguished from
/// a connect error: a regression that validates only the initial URL would
/// follow the redirect and then fail differently.
#[tokio::test]
async fn web_fetch_revalidates_each_redirect_target() {
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret"))
        .mount(&target)
        .await;
    let pivot = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pivot"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/secret", target.uri())),
        )
        .mount(&pivot)
        .await;

    // The allowlist names ONLY the pivot's host. The target's uri() is a
    // 127.0.0.1 URL whose host literal is not on that allowlist... unless the
    // pivot and target share the host string, in which case use the metadata
    // address as the unvetted hop instead.
    let (tool, url) = {
        let pivot_host = url::Url::parse(&pivot.uri()).unwrap();
        let target_host = url::Url::parse(&target.uri()).unwrap();
        if pivot_host.host_str() == target_host.host_str() {
            // Same host literal: redirect to the cloud-metadata link-local
            // address, which must be refused by the IP vet.
            let p = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/pivot"))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("location", "http://169.254.169.254/latest/meta-data/"),
                )
                .mount(&p)
                .await;
            let t = WebFetchTool::new(WebPolicy::dev_loopback());
            (t, format!("{}/pivot", p.uri()))
        } else {
            let t = WebFetchTool::new(
                WebPolicy::dev_loopback()
                    .with_allowlist(vec![pivot_host.host_str().unwrap().to_string()]),
            );
            (t, format!("{}/pivot", pivot.uri()))
        }
    };

    let err = tool
        .invoke(&ctx(), json!({ "url": url }))
        .await
        .expect_err("the redirect to an unvetted host must be refused");
    assert!(
        matches!(err, ToolError::Denied { .. }),
        "expected the SSRF denial, got {err:?}"
    );
}

/// A private/link-local target is refused as Denied (the SSRF blocklist),
/// not merely unconnectable. Runs without network access: the refusal must
/// happen before dialing.
#[tokio::test]
async fn web_fetch_refuses_private_ip_targets() {
    let tool = WebFetchTool::new(WebPolicy::dev_loopback());
    for url in [
        "https://169.254.169.254/latest/meta-data/",
        "https://10.0.0.8/",
        "https://192.168.1.1/",
    ] {
        let err = tool
            .invoke(&ctx(), json!({ "url": url }))
            .await
            .expect_err("an internal IP target must be refused");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected the SSRF denial for {url}, got {err:?}"
        );
    }
}

/// A slow server must hit the fetch's wall-clock ceiling and return Timeout
/// — the pre-fix tool had no timeout at all and would return 200 after the
/// server's full delay. The outer tokio timeout bounds the RED run.
#[tokio::test]
async fn web_fetch_enforces_a_wall_clock_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&server)
        .await;

    let tool = WebFetchTool::new(WebPolicy {
        timeout_secs: 2,
        ..WebPolicy::dev_loopback()
    });
    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(9),
        tool.invoke(&ctx(), json!({ "url": format!("{}/slow", server.uri()) })),
    )
    .await
    .expect("the invoke itself must not hang past the bounded window");
    let elapsed = start.elapsed();
    let err = result.expect_err("a slow response must time out");
    assert!(
        matches!(err, ToolError::Timeout),
        "expected the timeout refusal, got {err:?}"
    );
    assert!(
        elapsed < Duration::from_secs(9),
        "the timeout must fire before the server's 10s delay, took {elapsed:?}"
    );
}
