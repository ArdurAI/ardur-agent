//! §3.1 Phase 1 — non-network E2E: drive the full request/response path through
//! the public crate API against the deterministic [`AnthropicProvider::stub`].
//!
//! This is the network-free half of the §3.1 end-to-end coverage; the live half
//! lives in `anthropic_http_smoke.rs` (gated on `ANTHROPIC_API_KEY`). Together
//! they exercise the public surface a caller actually touches: building a full
//! multi-turn [`CompletionRequest`], completing it, and reading back a
//! [`CompletionResponse`] with a populated [`CostTuple`].

use ardur_provider_runtime::{
    AnthropicProvider, ChatMessage, CompletionRequest, CostEnvelope, FinishReason, ModelId,
    Provider, RequestId,
};

/// Build a full request: a system instruction plus a multi-turn user/assistant
/// transcript, with explicit (non-default) envelope/stop fields so the
/// round-trip exercises every populated field.
fn full_request() -> CompletionRequest {
    CompletionRequest {
        request_id: RequestId::new(),
        messages: vec![
            ChatMessage::system("You are a terse assistant. Answer in one word."),
            ChatMessage::user("What is the capital of France?"),
            ChatMessage::assistant("Paris."),
            ChatMessage::user("And of Japan?"),
        ],
        model: ModelId::new("claude-opus-4-8"),
        max_tokens: 64,
        temperature: 0.2,
        stop_sequences: vec!["\n\n".to_string()],
        requested_cost_envelope: CostEnvelope {
            max_cents: Some(100),
            max_total_tokens: Some(4096),
        },
    }
}

#[tokio::test]
async fn stub_round_trips_full_request_through_public_api() {
    let provider = AnthropicProvider::stub(ModelId::new("claude-opus-4-8"));
    let req = full_request();

    let resp = provider
        .complete(req.clone())
        .await
        .expect("stub completes");

    // Deterministic stub output.
    assert_eq!(resp.content, "[anthropic stub]");
    assert_eq!(resp.finish_reason, FinishReason::Stop);

    // The CostTuple is populated and internally consistent: it is exactly what
    // the provider's rate card prices the reported usage at (`cents` is a u64,
    // so it is non-negative by construction), and its token dimensions mirror
    // that usage.
    assert_eq!(resp.cost, provider.rate_card().price(resp.usage));
    assert_eq!(resp.cost.tokens_in, u64::from(resp.usage.tokens_in));
    assert_eq!(resp.cost.tokens_out, u64::from(resp.usage.tokens_out));

    // The request survives a JSON serialize/deserialize round-trip unchanged,
    // proving the public request envelope is a faithful wire schema.
    let json = serde_json::to_string(&req).expect("request serializes");
    let decoded: CompletionRequest = serde_json::from_str(&json).expect("request deserializes");
    assert_eq!(decoded, req, "request round-trips through JSON unchanged");
}
