//! §6.2 — integration coverage for the built-in `http.fetch` tool: status +
//! body capture, error statuses, body truncation, the host allowlist, the SSRF
//! private-IP defence, scheme/method/relative-URL rejection, the timeout, the
//! manual redirect follower, and the `register_builtins` installer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ardur_tool_registry::{
    BuiltinOpts, CapTokenRef, HttpFetchOpts, HttpFetchTool, InvocationId, SessionId, Tool,
    ToolContext, ToolError, ToolId, ToolRegistry,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A throwaway context with a wide budget. `http.fetch` ignores the filesystem
/// fields, so a `.` cwd is fine.
fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

// ── happy paths ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_returns_200_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello body"))
        .mount(&server)
        .await;

    // Default tool reaches loopback (the wiremock host) with no allowlist.
    let tool = HttpFetchTool::new();
    let out = tool
        .invoke(&ctx(), json!({ "url": format!("{}/hello", server.uri()) }))
        .await
        .expect("fetch succeeds");

    assert_eq!(out.content["status"], 200);
    assert_eq!(out.content["body"], "hello body");
    assert_eq!(out.content["body_truncated"], false);
    assert_eq!(out.content["bytes_read"], 10);
    assert!(
        out.content["final_url"]
            .as_str()
            .unwrap()
            .ends_with("/hello")
    );
}

#[tokio::test]
async fn fetch_returns_error_status_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/boom"))
        .respond_with(ResponseTemplate::new(500).set_body_string("kaboom"))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new();
    // A 5xx is a successful Result — the agent decides what to do with it.
    let out = tool
        .invoke(&ctx(), json!({ "url": format!("{}/boom", server.uri()) }))
        .await
        .expect("a 500 is still an Ok result");

    assert_eq!(out.content["status"], 500);
    assert_eq!(out.content["body"], "kaboom");
}

#[tokio::test]
async fn fetch_truncates_at_max_bytes() {
    let server = MockServer::start().await;
    let long = "x".repeat(10_000);
    Mock::given(method("GET"))
        .and(path("/long"))
        .respond_with(ResponseTemplate::new(200).set_body_string(long))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new().with_max_bytes(16);
    let out = tool
        .invoke(&ctx(), json!({ "url": format!("{}/long", server.uri()) }))
        .await
        .expect("fetch succeeds");

    assert_eq!(out.content["bytes_read"], 16);
    assert_eq!(out.content["body_truncated"], true);
    assert_eq!(out.content["body"], "xxxxxxxxxxxxxxxx");
}

// ── allowlist ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_allowlist_blocks_disallowed_host() {
    let server = MockServer::start().await;
    // The host is 127.0.0.1, which is not on the allowlist.
    let tool = HttpFetchTool::new().with_allowlist(vec!["example.com".to_string()]);
    let err = tool
        .invoke(&ctx(), json!({ "url": format!("{}/x", server.uri()) }))
        .await
        .expect_err("disallowed host is denied");

    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[tokio::test]
async fn fetch_allowlist_permits_allowed_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new().with_allowlist(vec!["127.0.0.1".to_string()]);
    let out = tool
        .invoke(&ctx(), json!({ "url": format!("{}/ok", server.uri()) }))
        .await
        .expect("allowlisted host fetches");

    assert_eq!(out.content["status"], 200);
    assert_eq!(out.content["body"], "ok");
}

#[tokio::test]
async fn fetch_allowlist_wildcard_subdomain() {
    // `*.test.invalid` matches the subdomain (so it passes the allowlist gate)
    // but `.invalid` never resolves (RFC 2606) — the request fails at DNS, not
    // at the allowlist. A non-matching host is denied up front. Together these
    // prove the wildcard matches subdomains specifically.
    let tool = HttpFetchTool::new().with_allowlist(vec!["*.test.invalid".to_string()]);

    let passed = tool
        .invoke(&ctx(), json!({ "url": "http://api.test.invalid/" }))
        .await
        .expect_err("unresolvable host fails after passing the allowlist");
    assert!(
        !matches!(passed, ToolError::Denied { .. }),
        "matching subdomain must clear the allowlist, got {passed:?}"
    );

    let denied = tool
        .invoke(&ctx(), json!({ "url": "http://api.other.invalid/" }))
        .await
        .expect_err("non-matching host is denied");
    assert!(matches!(denied, ToolError::Denied { .. }), "got {denied:?}");
}

// ── SSRF / private IPs ───────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_rejects_private_ip_by_default() {
    // Strict default (empty allowlist) refuses anything but localhost.
    let tool = HttpFetchTool::new();
    let err = tool
        .invoke(&ctx(), json!({ "url": "http://192.168.1.1/" }))
        .await
        .expect_err("private IP is denied by default");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");

    // Even when the host is explicitly allowlisted, the SSRF IP check refuses a
    // private address — the allowlist does not override the IP defence.
    let allowlisted = HttpFetchTool::new().with_allowlist(vec!["10.0.0.5".to_string()]);
    let err = allowlisted
        .invoke(&ctx(), json!({ "url": "http://10.0.0.5/" }))
        .await
        .expect_err("allowlisted private IP is still denied");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[tokio::test]
async fn fetch_permits_private_ip_when_allowed() {
    // With private-IP access granted, the SSRF gate is lifted: the host clears
    // both gates and the request proceeds to the network (where it fails to
    // connect — nothing is listening). The point is that it is NOT a denial.
    let tool = HttpFetchTool::new()
        .with_allowlist(vec!["10.0.0.5".to_string()])
        .with_private_ip_access(true);
    let err = tool
        .invoke(
            &ctx(),
            json!({ "url": "http://10.0.0.5/", "timeout_secs": 2 }),
        )
        .await
        .expect_err("nothing is listening, so the connection fails");

    assert!(
        !matches!(err, ToolError::Denied { .. }),
        "private IP must clear both gates when allowed, got {err:?}"
    );
}

#[tokio::test]
async fn fetch_rejects_reserved_ipv4_ranges_by_default() {
    let tool = HttpFetchTool::new().with_allowlist(vec!["*".to_string()]);

    for url in [
        "http://0.1.2.3/",         // 0.0.0.0/8 this-host range
        "http://100.64.0.1/",      // RFC 6598 carrier-grade NAT
        "http://192.0.2.1/",       // RFC 5737 TEST-NET-1
        "http://198.18.0.1/",      // RFC 2544 benchmarking
        "http://198.51.100.1/",    // RFC 5737 TEST-NET-2
        "http://203.0.113.1/",     // RFC 5737 TEST-NET-3
        "http://224.0.0.1/",       // multicast
        "http://240.0.0.1/",       // reserved for future use
        "http://255.255.255.255/", // limited broadcast
    ] {
        let err = tool
            .invoke(&ctx(), json!({ "url": url, "timeout_secs": 1 }))
            .await
            .expect_err("reserved address must be denied before network I/O");

        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{url} must be blocked by SSRF defence, got {err:?}"
        );
    }
}

#[tokio::test]
async fn fetch_rejects_ipv6_prefixes_that_embed_internal_ipv4() {
    let tool = HttpFetchTool::new().with_allowlist(vec!["*".to_string()]);

    for url in [
        "http://[64:ff9b::7f00:1]/", // NAT64 well-known prefix embedding 127.0.0.1
        "http://[64:ff9b::0a00:1]/", // NAT64 well-known prefix embedding 10.0.0.1
        "http://[2002:0a00:0001::1]/", // 6to4 prefix embedding 10.0.0.1
    ] {
        let err = tool
            .invoke(&ctx(), json!({ "url": url, "timeout_secs": 1 }))
            .await
            .expect_err("reserved address must be denied before network I/O");

        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{url} must be blocked by SSRF defence, got {err:?}"
        );
    }
}

// ── scheme / method / relative URL ───────────────────────────────────────────

#[tokio::test]
async fn fetch_rejects_non_http_scheme() {
    let tool = HttpFetchTool::new().with_allowlist(vec!["*".to_string()]);
    let err = tool
        .invoke(&ctx(), json!({ "url": "ftp://example.com/file" }))
        .await
        .expect_err("non-http scheme is denied");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[tokio::test]
async fn fetch_rejects_relative_url() {
    let tool = HttpFetchTool::new();
    let err = tool
        .invoke(&ctx(), json!({ "url": "/relative/path" }))
        .await
        .expect_err("relative url is rejected");
    assert!(matches!(err, ToolError::InvalidArgs(_)), "got {err:?}");
}

#[tokio::test]
async fn fetch_rejects_post_method() {
    let server = MockServer::start().await;
    let tool = HttpFetchTool::new();
    let err = tool
        .invoke(
            &ctx(),
            json!({ "url": format!("{}/x", server.uri()), "method": "POST" }),
        )
        .await
        .expect_err("POST is denied");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

// ── timeout ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_timeout_aborts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new();
    let err = tool
        .invoke(
            &ctx(),
            json!({ "url": format!("{}/slow", server.uri()), "timeout_secs": 1 }),
        )
        .await
        .expect_err("the request times out");
    assert!(matches!(err, ToolError::Timeout), "got {err:?}");
}

// ── redirects ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fetch_redirect_followed_up_to_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/b"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new();
    let out = tool
        .invoke(&ctx(), json!({ "url": format!("{}/a", server.uri()) }))
        .await
        .expect("redirect is followed to the final 200");

    assert_eq!(out.content["status"], 200);
    assert_eq!(out.content["body"], "arrived");
    assert!(out.content["final_url"].as_str().unwrap().ends_with("/b"));
}

#[tokio::test]
async fn fetch_redirect_exceeds_limit_errors() {
    let server = MockServer::start().await;
    // A self-redirect: /loop -> /loop, forever.
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;

    let tool = HttpFetchTool::new().with_redirect_limit(2);
    let err = tool
        .invoke(&ctx(), json!({ "url": format!("{}/loop", server.uri()) }))
        .await
        .expect_err("exceeding the redirect limit errors");
    assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
}

// ── register_builtins ────────────────────────────────────────────────────────

#[tokio::test]
async fn register_builtins_skips_http_when_none() {
    // No http config: the tool is not registered.
    let mut none = ToolRegistry::new();
    none.register_builtins(BuiltinOpts::default())
        .expect("no-op registration");
    assert!(none.get(&ToolId::new(HttpFetchTool::ID)).is_none());

    // http present but disabled: still not registered.
    let mut disabled = ToolRegistry::new();
    disabled
        .register_builtins(BuiltinOpts {
            http: Some(HttpFetchOpts {
                enable: false,
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("disabled http registration");
    assert!(disabled.get(&ToolId::new(HttpFetchTool::ID)).is_none());

    // Enabled: registered.
    let mut enabled = ToolRegistry::new();
    enabled
        .register_builtins(BuiltinOpts {
            http: Some(HttpFetchOpts {
                enable: true,
                allowlist: vec!["example.com".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("enabled http registration");
    assert!(enabled.get(&ToolId::new(HttpFetchTool::ID)).is_some());
}
