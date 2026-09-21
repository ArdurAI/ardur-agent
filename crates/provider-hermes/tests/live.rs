//! Gated live smoke against the real `hermes` binary. Off by default so CI
//! never spends money / requires an install.

use ardur_provider_hermes::HermesProvider;
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider};
use ardur_runtime::ChatMessage;

#[tokio::test]
#[ignore = "requires a logged-in hermes install; set HERMES_LIVE_TEST=1"]
async fn hermes_live_smoke() {
    if std::env::var("HERMES_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping hermes_live_smoke (set HERMES_LIVE_TEST=1 to run)");
        return;
    }
    let provider = HermesProvider::from_env();
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly the word: pong")],
        ModelId(String::new()),
        // At/above the default max_tokens_floor (4096).
        8_192,
    );
    let resp = provider
        .complete(req)
        .await
        .expect("live hermes completion");
    assert!(
        !resp.content.is_empty(),
        "live hermes returned empty content"
    );
    eprintln!("live hermes replied: {:?}", resp.content);
}
