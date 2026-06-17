mod support;

use ardur_acp::{ACP_METHOD_INITIALIZE, AcpMessage, AcpRequest, AcpResponsePayload};
use axum::http::Request;
use serde_json::json;

#[tokio::test]
async fn post_acp_accepts_initialize_and_mints_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config);
    let body = serde_json::to_vec(&AcpMessage::Request(AcpRequest::new(
        1_i64,
        ACP_METHOD_INITIALIZE,
        Some(json!({ "protocolVersion": 1 })),
    )))
    .expect("serialize acp request");

    let (status, bytes) = support::oneshot(
        router,
        Request::builder()
            .method("POST")
            .uri("/acp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
            .body(axum::body::Body::from(body))
            .expect("request"),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::OK);
    let message: AcpMessage = serde_json::from_slice(&bytes).expect("ACP response parses");
    let AcpMessage::Response(response) = message else {
        panic!("expected ACP response, got {message:?}");
    };
    match response.payload().expect("valid response payload") {
        AcpResponsePayload::Result(value) => {
            assert_eq!(value["accepted"], true);
            assert_eq!(value["method"], ACP_METHOD_INITIALIZE);
            assert!(value["receipt_id"].as_str().is_some_and(|s| !s.is_empty()));
        }
        AcpResponsePayload::Error(error) => panic!("unexpected ACP error: {error:?}"),
    }

    let receipt_log = dir.path().join("receipts/chain.jsonl");
    let receipt_lines = std::fs::read_to_string(receipt_log).expect("receipt log exists");
    assert_eq!(
        receipt_lines.lines().count(),
        1,
        "ACP endpoint is receipt-chained"
    );
}

#[tokio::test]
async fn post_acp_rejects_missing_bearer_and_invalid_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config);

    let (status, _) = support::oneshot(
        router.clone(),
        Request::builder()
            .method("POST")
            .uri("/acp")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    let (status, body) = support::oneshot(
        router,
        Request::builder()
            .method("POST")
            .uri("/acp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
            .body(axum::body::Body::from("{}"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("invalid ACP message"));
}
