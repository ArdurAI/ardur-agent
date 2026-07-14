//! §3.4 Phase 1 — wiremock round-trips against the Azure OpenAI backend's
//! public surface: build a real [`AzureOpenAiProvider`], drive it through the
//! [`Provider`]/[`EmbeddingProvider`] traits, assert on the returned types.
//! Never touches a real Azure resource — a `wiremock` server stands in for
//! the resource/deployment-scoped URL, so CI stays green with no credentials.

use ardur_provider_azure_openai::{AzureOpenAiConfig, AzureOpenAiProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, EmbeddingProvider, EmbeddingRequest, FinishReason, ModelId,
    Provider, ProviderError,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer, model: &str) -> AzureOpenAiProvider {
    AzureOpenAiProvider::new(
        AzureOpenAiConfig::new("azure-test-key", "my-resource", "gpt-4o-deployment")
            .base_url_override(server.uri()),
        ModelId::new(model),
    )
}

#[tokio::test]
async fn complete_round_trips_through_the_deployment_scoped_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/openai/deployments/gpt-4o-deployment/chat/completions",
        ))
        .and(header("api-key", "azure-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, "gpt-4o");
    let req = CompletionRequest::new(vec![ChatMessage::user("ping")], ModelId::new("gpt-4o"), 32);
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 10);
    assert_eq!(resp.usage.tokens_out, 2);
}

#[tokio::test]
async fn embed_round_trips_and_orders_vectors_by_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/openai/deployments/gpt-4o-deployment/embeddings"))
        .and(header("api-key", "azure-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"embedding": [0.5, 0.5], "index": 0},
                {"embedding": [0.1, 0.9], "index": 1}
            ],
            "usage": {"prompt_tokens": 4, "completion_tokens": 0}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, "gpt-4o");
    let req = EmbeddingRequest::new(
        vec!["hello".to_string(), "world".to_string()],
        "text-embedding-3-small",
    );
    let resp = EmbeddingProvider::embed(&provider, req)
        .await
        .expect("the call succeeds");

    assert_eq!(resp.vectors, vec![vec![0.5, 0.5], vec![0.1, 0.9]]);
    assert_eq!(resp.usage.tokens_in, 4);
}

#[tokio::test]
async fn unauthorized_upstream_maps_to_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/openai/deployments/gpt-4o-deployment/chat/completions",
        ))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"message": "access denied", "code": "401"}
        })))
        .mount(&server)
        .await;

    let provider = provider_for(&server, "gpt-4o");
    let err = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("gpt-4o"),
            16,
        ))
        .await
        .expect_err("a 401 is an error");
    assert!(matches!(err, ProviderError::Unauthorized));
}
