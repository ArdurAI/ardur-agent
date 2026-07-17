//! §2.1 Phase 1: a wired `ChatEngine` runs a turn end-to-end (admit → submit →
//! finalize) and echoes the user's message back as a non-empty response.

use ardur_cli::{ChatEngine, Config};
use ardur_runtime::ChatMessage;

#[test]
fn chat_echo_smoke() {
    // A config with a non-empty key so the Anthropic stub is constructible and
    // a known starting budget.
    let config = Config {
        api_key: "test-key".to_string(),
        model: "claude-opus-4-8".to_string(),
        budget_cents: 1000,
    };
    let engine = ChatEngine::new(&config).expect("the engine wires");

    let history = vec![ChatMessage::user("hello ardur")];
    let outcome = tokio::runtime::Runtime::new()
        .expect("tokio runtime builds")
        .block_on(engine.run_turn(&history))
        .expect("the echo turn succeeds");

    assert!(
        !outcome.response.is_empty(),
        "the echo runtime should return a non-empty response"
    );
    assert!(
        outcome.response.contains("hello ardur"),
        "the echo runtime should echo the user message, got: {}",
        outcome.response
    );
    // The stub bills nothing, so the budget is untouched.
    assert_eq!(outcome.used_cents, 0);
    assert_eq!(outcome.remaining_cents, 1000);
}
