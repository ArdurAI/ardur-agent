//! §3.1 Phase 1 — live Messages-API smoke test.
//!
//! Gated on `ANTHROPIC_API_KEY`: with no key the test is a no-op (it prints a
//! skip notice and returns), so CI — which has no key — passes without ever
//! touching the network. With a key present it issues one real completion and
//! asserts the response, finish reason, and usage accounting are wired through.

use ardur_provider_runtime::{
    AnthropicProvider, ChatMessage, CompletionRequest, ModelId, Provider,
};

#[tokio::test]
async fn live_completion_round_trips() {
    if std::env::var("ANTHROPIC_API_KEY")
        .map(|k| k.is_empty())
        .unwrap_or(true)
    {
        eprintln!("skipped: ANTHROPIC_API_KEY not set");
        return;
    }

    let model = ModelId::new("claude-opus-4-8");
    let provider =
        AnthropicProvider::from_env(model.clone()).expect("ANTHROPIC_API_KEY present → live");

    let mut req = CompletionRequest::new(
        vec![ChatMessage::user("Say only the word: ping")],
        model,
        10,
    );
    req.temperature = 0.0;

    let resp = provider
        .complete(req)
        .await
        .expect("live completion succeeds");

    assert!(
        resp.content.to_lowercase().contains("ping"),
        "expected the model to echo 'ping', got: {:?}",
        resp.content
    );
    // Token accounting must be populated from the upstream `usage` object.
    assert!(resp.usage.tokens_in > 0, "input tokens should be billed");
    assert!(resp.usage.tokens_out > 0, "output tokens should be billed");
    // The priced cost must carry the same token counts the provider billed.
    // (Per-1k rates round a tiny `ping` reply's `cents` to 0, so we assert the
    // token dimensions rather than `cents > 0` for a sub-cent call.)
    assert_eq!(resp.cost.tokens_in, u64::from(resp.usage.tokens_in));
    assert_eq!(resp.cost.tokens_out, u64::from(resp.usage.tokens_out));
    assert!(
        resp.raw_provider_response.is_some(),
        "live path retains the raw upstream body"
    );
}
