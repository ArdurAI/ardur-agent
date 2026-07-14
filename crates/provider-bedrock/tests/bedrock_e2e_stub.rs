//! §3.4 Phase 1 — wiremock round-trip against the Bedrock backend's public
//! surface: build a real [`BedrockProvider`], drive it through [`Provider`],
//! assert on the returned type and the SigV4-signed request shape. Never
//! touches real AWS — a `wiremock` server stands in for
//! `bedrock-runtime.{region}.amazonaws.com`, so CI stays green with no
//! credentials.

use ardur_provider_bedrock::{BedrockConfig, BedrockProvider};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, FinishReason, ModelId, Provider};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer) -> BedrockProvider {
    BedrockProvider::new(
        BedrockConfig::new("AKIDEXAMPLE", "secret").base_url_override(server.uri()),
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
    )
}

#[tokio::test]
async fn complete_signs_and_round_trips_the_invoke_model_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
        ))
        .and(header_exists("authorization"))
        .and(header_exists("x-amz-date"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "pong"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 9);
    assert_eq!(resp.usage.tokens_out, 2);
}

#[tokio::test]
async fn tool_use_stop_reason_decodes_into_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "tool_use", "id": "call_1", "name": "echo", "input": {"msg": "hi"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("call echo")],
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    match resp.finish_reason {
        FinishReason::ToolUse(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call_1");
            assert_eq!(calls[0].name, "echo");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}
