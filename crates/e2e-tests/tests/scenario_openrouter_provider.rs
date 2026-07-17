//! Scenario §3.2 — `openrouter_provider`.
//!
//! Drives one happy-path turn through the *fused* substrate with the live-HTTP
//! [`OpenRouterProvider`] as the model backend, pointed at a `wiremock` server
//! standing in for OpenRouter's OpenAI-compatible `POST /chat/completions`. It
//! proves the new §3.2 provider plugs into the same `FusedRuntime` spine the
//! Anthropic backend uses (cap-token → cedar → cost-gate → provider → receipt →
//! finalize) and that the usage + cost OpenRouter reports is attributed onto the
//! turn's receipt.
//!
//! CI runs this offline: the only network call goes to the in-process mock, so
//! no `OPENROUTER_API_KEY` and no real spend are involved.

use std::sync::Arc;

use ardur_e2e_tests::fixtures;

use ardur_provider_openrouter::{OpenRouterConfig, OpenRouterProvider};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turn_through_fused_substrate_with_openrouter_backend() {
    // ---- The mock OpenRouter endpoint: one chat completion with usage + cost.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-or-test"))
        .and(header("x-title", "Ardur Agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "gen-e2e",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "routed-pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49, "cost": 0.05}
        })))
        .expect(1)
        .mount(&server)
        .await;

    // ---- The §3.2 provider, talking to the mock instead of real OpenRouter.
    let provider: Arc<dyn Provider> = Arc::new(OpenRouterProvider::new(
        OpenRouterConfig::new("sk-or-test").base_url(server.uri()),
        ModelId::new("anthropic/claude-3.5-sonnet"),
    ));
    assert_eq!(provider.id().0, "openrouter");

    // ---- The fused runtime, wired with the OpenRouter backend.
    let runtime = fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires with the OpenRouter provider");

    let token = fixtures::dev_valid_cap_token();
    let result = runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("ping through openrouter")],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
        .expect("the turn completes through the OpenRouter-backed substrate");

    // The assistant text is the provider's routed response.
    assert_eq!(result.response.content, "routed-pong");
    // The usage OpenRouter reported is attributed onto the turn's receipt cost.
    assert_eq!(result.cost.tokens_in, 42);
    assert_eq!(result.cost.tokens_out, 7);
    // 0.05 USD → 5¢, carried straight from OpenRouter's reported `usage.cost`.
    assert_eq!(result.cost.cents, 5);
    // A receipt was minted for the turn.
    assert!(
        !result.receipt_id.0.is_nil(),
        "a non-nil receipt id was minted"
    );

    // The provider was actually dispatched to exactly once (the `.expect(1)`
    // on the mock is asserted on drop).
    drop(server);
}
