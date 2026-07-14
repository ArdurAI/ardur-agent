//! §3.4 Phase 1 — wiremock round-trip against the Vertex backend's public
//! surface: build a real [`VertexProvider`], drive it through [`Provider`],
//! assert on the returned type. Never touches real GCP — a `wiremock` server
//! stands in for `{location}-aiplatform.googleapis.com`, so CI stays green
//! with no credentials.

use ardur_provider_runtime::{ChatMessage, CompletionRequest, FinishReason, ModelId, Provider};
use ardur_provider_vertex::{VertexConfig, VertexProvider};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer) -> VertexProvider {
    VertexProvider::new(
        VertexConfig::new("test-token", "my-project").base_url_override(server.uri()),
        ModelId::new("gemini-1.5-pro"),
    )
}

#[tokio::test]
async fn complete_round_trips_through_the_project_scoped_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-1.5-pro:generateContent",
        ))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "pong"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("gemini-1.5-pro"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 7);
    assert_eq!(resp.usage.tokens_out, 2);
}

#[tokio::test]
async fn function_call_decodes_into_tool_use() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-1.5-pro:generateContent",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"functionCall": {"name": "echo", "args": {"msg": "hi"}}}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("call echo")],
        ModelId::new("gemini-1.5-pro"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    match resp.finish_reason {
        FinishReason::ToolUse(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "echo");
            assert_eq!(calls[0].arguments, serde_json::json!({"msg": "hi"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthorized_upstream_maps_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-1.5-pro:generateContent",
        ))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": 401, "message": "invalid token", "status": "UNAUTHENTICATED"}
        })))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("gemini-1.5-pro"),
            16,
        ))
        .await
        .expect_err("a 401 is an error");
    assert!(matches!(
        err,
        ardur_provider_runtime::ProviderError::Unauthorized
    ));
}
