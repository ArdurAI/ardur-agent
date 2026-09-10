mod support;

use ardur_server::openapi::{GeneratedRustClient, generate_python_client, generate_rust_client};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::io::Write;
use std::process::Command;
use tempfile::NamedTempFile;

const ADMIN_TOKEN: &str = "openapi-admin-token-000000000000";

#[tokio::test]
async fn openapi_json_exposes_existing_server_endpoints() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    // OpenAPI endpoints are bearer-gated — no token returns 401.
    let unauthed = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .body(Body::empty())
        .expect("request builds");
    let (unauthed_status, _) = support::oneshot(router.clone(), unauthed).await;
    assert_eq!(unauthed_status, StatusCode::UNAUTHORIZED);

    let request = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let spec: serde_json::Value = serde_json::from_slice(&body).expect("spec json");
    assert_eq!(spec["openapi"], "3.0.3");
    assert!(spec["paths"].get("/healthz").is_some());
    assert!(spec["paths"].get("/chat").is_some());
    assert!(spec["paths"].get("/slack/events").is_some());
    assert!(
        spec["components"]["securitySchemes"]
            .get("BearerAuth")
            .is_some()
    );
}

#[test]
fn generated_clients_include_health_chat_and_auth_surfaces() {
    let rust = generate_rust_client();
    let python = generate_python_client();

    assert!(rust.contains("pub struct ArdurClient"));
    assert!(rust.contains("/healthz"));
    assert!(rust.contains("/chat"));
    assert!(python.contains("class ArdurClient"));
    assert!(python.contains("Authorization"));
    assert!(python.contains("/healthz"));
}

#[tokio::test]
async fn generated_rust_client_works_against_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = GeneratedRustClient::new(format!("http://{addr}"), None);
    let health = client.healthz().await.expect("health request succeeds");
    assert_eq!(health["status"], "ok");

    server.abort();
}

#[test]
fn generated_python_client_is_valid_python() {
    let mut file = NamedTempFile::new().expect("temp python file");
    file.write_all(generate_python_client().as_bytes()).unwrap();
    let status = Command::new("python3")
        .arg("-m")
        .arg("py_compile")
        .arg(file.path())
        .status()
        .expect("python3 available");
    assert!(status.success());
}
