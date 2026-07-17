//! §3.3 Phase 1 — wiremock round-trips against Ollama's `/api/chat` endpoint.
//!
//! These never touch a real Ollama daemon or the cloud: a `wiremock` server
//! stands in for `POST /api/chat`, so the request translation, response
//! parsing, token-count extraction, local-vs-cloud auth, model passthrough, and
//! error mapping are all asserted offline (CI has neither a daemon nor a key).

use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, ModelId, Provider, ProviderError,
};
use serde_json::Value;
use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A local provider (no auth) whose base URL points at `server`.
fn local_provider(server: &MockServer, model: &str) -> OllamaProvider {
    OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .default_model(model),
    )
}

#[tokio::test]
async fn local_no_auth_round_trips_with_token_counts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        // A local daemon is unauthenticated — no Authorization header rides out.
        .and(body_partial_json(serde_json::json!({
            "stream": false,
            "model": "llama3.2",
            "options": {"num_predict": 64},
        })))
        .respond_with(|req: &Request| {
            // Prove the local path sent *no* auth header.
            assert!(
                req.headers.get("authorization").is_none(),
                "a local daemon call carries no Authorization header"
            );
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "llama3.2",
                "message": {"role": "assistant", "content": "pong"},
                "done_reason": "stop",
                "done": true,
                "prompt_eval_count": 12,
                "eval_count": 3
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let provider = local_provider(&server, "llama3.2");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("llama3.2"),
        64,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 12);
    assert_eq!(resp.usage.tokens_out, 3);
    assert_eq!(resp.cost.tokens_in, 12);
    assert_eq!(resp.cost.tokens_out, 3);
    // Ollama never bills a dollar cost.
    assert_eq!(resp.cost.cents, 0);
    assert!(resp.raw_provider_response.is_some());
}

#[tokio::test]
async fn cloud_sends_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        // The cloud path Bearer-auths with the configured key.
        .and(header("authorization", "Bearer sk-cloud-test"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "gpt-oss:120b",
            "message": {"role": "assistant", "content": "cloud-pong"},
            "done_reason": "stop",
            "done": true,
            "prompt_eval_count": 5,
            "eval_count": 1
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .api_key("sk-cloud-test")
            .default_model("gpt-oss:120b"),
    );
    let resp = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("ping")],
            ModelId::new("gpt-oss:120b"),
            16,
        ))
        .await
        .expect("the cloud call succeeds");
    assert_eq!(resp.content, "cloud-pong");
    assert_eq!(resp.usage.tokens_in, 5);
    assert_eq!(resp.usage.tokens_out, 1);
}

#[tokio::test]
async fn arbitrary_model_string_passes_through_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(|req: &Request| {
            let body: Value = req.body_json().expect("a JSON request body");
            assert_eq!(body["model"], "qwen2.5:14b-instruct");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role": "assistant", "content": "hi"},
                "done_reason": "stop",
                "prompt_eval_count": 1,
                "eval_count": 1
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let provider = local_provider(&server, "qwen2.5:14b-instruct");
    let resp = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("qwen2.5:14b-instruct"),
            16,
        ))
        .await
        .expect("the call succeeds");
    assert_eq!(resp.content, "hi");
    assert_eq!(resp.cost.cents, 0);
}

#[tokio::test]
async fn unknown_model_maps_to_model_not_available() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "model 'vendor/nope' not found, try pulling it first"
        })))
        .mount(&server)
        .await;

    let provider = local_provider(&server, "vendor/nope");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("vendor/nope"),
            16,
        ))
        .await
        .expect_err("a 404 is an error");
    assert!(
        matches!(&err, ProviderError::ModelNotAvailable(m) if m.0 == "vendor/nope"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn bad_request_surfaces_error_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid options.num_predict: must be >= -1"
        })))
        .mount(&server)
        .await;

    let provider = local_provider(&server, "llama3.2");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("llama3.2"),
            16,
        ))
        .await
        .expect_err("a 400 is an error");
    match err {
        ProviderError::InvalidRequest(msg) => {
            assert!(msg.contains("num_predict"), "got {msg:?}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limited_parses_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;

    let provider = local_provider(&server, "llama3.2");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("llama3.2"),
            16,
        ))
        .await
        .expect_err("a 429 is an error");
    assert!(
        matches!(err, ProviderError::RateLimited { retry_after_ms } if retry_after_ms == 7_000),
        "got {err:?}"
    );
}

#[tokio::test]
async fn connection_refused_maps_to_upstream_with_hint() {
    // Point at a port nothing is listening on: the connect fails immediately.
    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url("http://127.0.0.1:1")
            .default_model("llama3.2"),
    );
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("llama3.2"),
            16,
        ))
        .await
        .expect_err("a refused connection is an error");
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("could not connect to Ollama"),
                "the hint points at a missing daemon: {msg:?}"
            );
            assert!(
                msg.contains("127.0.0.1:1"),
                "the base URL is named: {msg:?}"
            );
        }
        other => panic!("expected Upstream with a connect hint, got {other:?}"),
    }
}
