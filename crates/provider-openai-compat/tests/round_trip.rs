//! §3.2 Phase 1 — wiremock round-trips against the OpenAI-compatible endpoint.
//!
//! These never touch a real provider: a `wiremock` server stands in for
//! `POST /chat/completions`, so the request translation, response parsing,
//! usage/cost extraction, model passthrough, and error mapping are all asserted
//! offline (CI has no API key).

use ardur_provider_openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, ModelId, Provider, ProviderError,
};
use serde_json::Value;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a provider whose base URL points at `server`.
fn provider_for(server: &MockServer, model: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(
        OpenAiCompatConfig::new("sk-test").base_url(server.uri()),
        ModelId::new(model),
    )
}

#[tokio::test]
async fn happy_path_round_trips_with_usage_and_cost() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // Auth goes out, but no provider-specific attribution headers are added
        // by the generic compatibility adapter.
        .and(header("authorization", "Bearer sk-test"))
        // Non-streaming, and the model passes through unchanged.
        .and(body_partial_json(serde_json::json!({
            "stream": false,
            "model": "anthropic/claude-3.5-sonnet",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "gen-abc",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15, "cost": 0.0234}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, "anthropic/claude-3.5-sonnet");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("anthropic/claude-3.5-sonnet"),
        64,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 12);
    assert_eq!(resp.usage.tokens_out, 3);
    // 0.0234 USD → 2.34¢ → 2¢, with the token counts carried onto the tuple.
    assert_eq!(resp.cost.cents, 2);
    assert_eq!(resp.cost.tokens_in, 12);
    assert_eq!(resp.cost.tokens_out, 3);
    assert!(resp.raw_provider_response.is_some());
}

#[tokio::test]
async fn arbitrary_model_string_passes_through_unchanged() {
    let server = MockServer::start().await;
    // Capture the request body to prove the exact model slug rode through.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().expect("a JSON request body");
            assert_eq!(body["model"], "meta-llama/llama-3.1-8b-instruct:free");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost": 0.0}
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, "meta-llama/llama-3.1-8b-instruct:free");
    let resp = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("meta-llama/llama-3.1-8b-instruct:free"),
            16,
        ))
        .await
        .expect("the call succeeds");
    assert_eq!(resp.content, "hi");
    // No cost reported → billed zero cents (decision #6 fallback).
    assert_eq!(resp.cost.cents, 0);
}

#[tokio::test]
async fn error_body_maps_to_invalid_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "temperature must be <= 2", "code": "invalid_request"}
        })))
        .mount(&server)
        .await;

    let provider = provider_for(&server, "openai/gpt-4o");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("openai/gpt-4o"),
            16,
        ))
        .await
        .expect_err("a 400 is an error");
    match err {
        ProviderError::InvalidRequest(msg) => {
            assert!(msg.contains("temperature must be <= 2"), "got {msg:?}");
            assert!(msg.contains("invalid_request"), "code surfaced: {msg:?}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_model_maps_to_model_not_available() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"message": "no such model", "code": 404}
        })))
        .mount(&server)
        .await;

    let provider = provider_for(&server, "vendor/does-not-exist");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("vendor/does-not-exist"),
            16,
        ))
        .await
        .expect_err("a 404 is an error");
    assert!(
        matches!(&err, ProviderError::ModelNotAvailable(m) if m.0 == "vendor/does-not-exist"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn rate_limited_parses_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;

    let provider = provider_for(&server, "openai/gpt-4o");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("openai/gpt-4o"),
            16,
        ))
        .await
        .expect_err("a 429 is an error");
    assert!(
        matches!(err, ProviderError::RateLimited { retry_after_ms } if retry_after_ms == 7_000),
        "got {err:?}"
    );
}
