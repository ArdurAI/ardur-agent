//! Scenario §12.5 — `openai_compat_provider`.
//!
//! Drives one happy-path turn through the *fused* substrate with the live-HTTP
//! [`OpenAiCompatProvider`] as the model backend, pointed at a `wiremock` server
//! standing in for an OpenAI-compatible `POST /chat/completions` endpoint. It
//! proves the new §12.5 provider plugs into the same `FusedRuntime` spine the
//! Anthropic backend uses (cap-token → cedar → cost-gate → provider → receipt →
//! finalize) and that reported usage + optional cost are attributed onto the
//! turn's receipt.
//!
//! CI runs this offline: the only network call goes to the in-process mock, so
//! no `OPENAI_COMPAT_API_KEY` and no real spend are involved.

use std::sync::Arc;

use ardur_e2e_tests::fixtures;

use ardur_provider_openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turn_through_fused_substrate_with_openai_compat_backend() {
    // ---- The mock OpenAI-compatible endpoint: one chat completion with usage + cost.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer sk-compat-test"))
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

    // ---- The §12.5 provider, talking to the mock instead of a live service.
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatProvider::new(
        OpenAiCompatConfig::new("sk-compat-test").base_url(server.uri()),
        ModelId::new("gpt-test"),
    ));
    assert_eq!(provider.id().0, "openai-compat");

    // ---- The fused runtime, wired with the OpenAiCompat backend.
    let runtime = fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires with the OpenAI-compatible provider");

    let token = fixtures::dev_valid_cap_token();
    let result = runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("ping through openai-compat")],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
        .expect("the turn completes through the OpenAI-compatible substrate");

    // The assistant text is the provider's routed response.
    assert_eq!(result.response.content, "routed-pong");
    // The usage reported by the endpoint is attributed onto the turn's receipt cost.
    assert_eq!(result.cost.tokens_in, 42);
    assert_eq!(result.cost.tokens_out, 7);
    // 0.05 USD → 5¢, carried straight from the optional `usage.cost` field.
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
