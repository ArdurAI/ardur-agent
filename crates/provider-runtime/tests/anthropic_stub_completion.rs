//! §3.0 Phase 1 — the Anthropic stub returns a fixed completion, rejects an
//! empty key, and reports its Phase-1 capabilities.

use ardur_provider_runtime::{
    AnthropicProvider, ChatMessage, CompletionRequest, CostTuple, FinishReason, ModelId, Provider,
    ProviderError, Usage,
};

fn req() -> CompletionRequest {
    CompletionRequest::new(
        vec![ChatMessage::user("hello")],
        ModelId::new("claude-opus-4-8"),
        256,
    )
}

#[tokio::test]
async fn stub_returns_fixed_completion() {
    let provider = AnthropicProvider::new("sk-test", ModelId::new("claude-opus-4-8"));

    let resp = provider.complete(req()).await.expect("stub completes");

    assert_eq!(resp.content, "[anthropic stub]");
    assert_eq!(resp.finish_reason, FinishReason::Stop);
    assert_eq!(
        resp.usage,
        Usage {
            tokens_in: 0,
            tokens_out: 0
        }
    );
    assert_eq!(resp.cost, CostTuple::default());
    assert!(resp.raw_provider_response.is_none());
}

#[tokio::test]
async fn empty_key_is_unauthorized() {
    let provider = AnthropicProvider::new("", ModelId::new("claude-opus-4-8"));

    let err = provider.complete(req()).await.unwrap_err();

    assert!(matches!(err, ProviderError::Unauthorized));
}

#[test]
fn reports_phase_1_capabilities() {
    let provider = AnthropicProvider::new("sk-test", ModelId::new("claude-opus-4-8"));

    assert_eq!(
        provider.id(),
        ardur_provider_runtime::ProviderId("anthropic".to_string())
    );
    assert!(
        !provider.supports_streaming(),
        "Phase 1 stub does not stream"
    );
    assert_eq!(provider.rate_card().version_id, "anthropic-2026-q2-v1");
    assert_eq!(provider.model_id(), &ModelId::new("claude-opus-4-8"));
}
