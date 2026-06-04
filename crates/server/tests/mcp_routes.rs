//! The MCP HTTP surface on `ardur-server`: the bearer gate admits configured
//! tokens and rejects everything else, and the routes are absent entirely when
//! the surface is disabled. Driven in-process via `oneshot` (no socket).

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use support::{boot_router, oneshot, test_config, test_config_with_mcp};
use tempfile::TempDir;

const BEARER: &str = "mcp-secret-token";

/// A minimal MCP `initialize` JSON-RPC request body.
const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

fn init_request(authorization: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp/ardur")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(auth) = authorization {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    builder.body(Body::from(INIT_BODY)).expect("request builds")
}

#[tokio::test]
async fn bearer_auth_rejects_unknown_token() {
    let dir = TempDir::new().unwrap();
    let config = test_config_with_mcp(&dir, None, vec![BEARER.to_string()]);
    let router = boot_router(&config);

    // No Authorization header at all → 401.
    let (status, _) = oneshot(router.clone(), init_request(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let (status, _) = oneshot(router, init_request(Some("Bearer not-the-token"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_auth_accepts_configured_token() {
    let dir = TempDir::new().unwrap();
    let config = test_config_with_mcp(&dir, None, vec![BEARER.to_string()]);
    let router = boot_router(&config);

    let (status, _) = oneshot(router, init_request(Some(&format!("Bearer {BEARER}")))).await;
    // The configured token clears the gate and reaches the MCP transport, which
    // completes the handshake — anything but 401 proves admission.
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn mcp_routes_absent_when_disabled() {
    let dir = TempDir::new().unwrap();
    let config = test_config(&dir, None); // MCP disabled
    let router = boot_router(&config);

    let (status, _) = oneshot(router, init_request(Some(&format!("Bearer {BEARER}")))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
