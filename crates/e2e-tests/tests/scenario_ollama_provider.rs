//! Scenario §3.3 — `ollama_provider`.
//!
//! Drives one happy-path turn through the *fused* substrate with the live-HTTP
//! [`OllamaProvider`] as the model backend, pointed at a `wiremock` server
//! standing in for Ollama's native `POST /api/chat` endpoint. It proves the new
//! §3.3 provider plugs into the same `FusedRuntime` spine the Anthropic and
//! OpenRouter backends use (cap-token → cedar → cost-gate → provider → receipt →
//! finalize) and that the token counts Ollama reports are attributed onto the
//! turn's receipt — at zero billed cents, since Ollama reports no dollar cost.
//!
//! CI runs this offline: the only network call goes to the in-process mock, so
//! no running daemon, no `OLLAMA_API_KEY`, and no real spend are involved.

use std::sync::Arc;

use ardur_e2e_tests::fixtures;

use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::Provider;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turn_through_fused_substrate_with_ollama_backend() {
    // ---- The mock Ollama endpoint: one chat completion with token counts.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "local-pong"},
            "done_reason": "stop",
            "done": true,
            "prompt_eval_count": 42,
            "eval_count": 7
        })))
        .expect(1)
        .mount(&server)
        .await;

    // ---- The §3.3 provider, talking to the mock instead of a real daemon.
    let provider: Arc<dyn Provider> = Arc::new(OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .default_model("llama3.2"),
    ));
    assert_eq!(provider.id().0, "ollama");

    // ---- The fused runtime, wired with the Ollama backend.
    let runtime = fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires with the Ollama provider");

    let token = fixtures::dev_valid_cap_token();
    let result = runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("ping through ollama")],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
        .expect("the turn completes through the Ollama-backed substrate");

    // The assistant text is the provider's local response.
    assert_eq!(result.response.content, "local-pong");
    // The token counts Ollama reported are attributed onto the turn's receipt.
    assert_eq!(result.cost.tokens_in, 42);
    assert_eq!(result.cost.tokens_out, 7);
    // Ollama reports no dollar cost, so the turn is billed zero cents.
    assert_eq!(result.cost.cents, 0);
    // A receipt was minted for the turn.
    assert!(
        !result.receipt_id.0.is_nil(),
        "a non-nil receipt id was minted"
    );

    // The provider was actually dispatched to exactly once (the `.expect(1)`
    // on the mock is asserted on drop).
    drop(server);
}
