//! Gated live smoke against the real `opencode` binary. Off by default so CI
//! never spends money / requires an install.

use ardur_provider_opencode::OpenCodeProvider;
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider};
use ardur_runtime::ChatMessage;

#[tokio::test]
#[ignore = "requires a logged-in opencode install; set OPENCODE_LIVE_TEST=1"]
async fn opencode_live_smoke() {
    if std::env::var("OPENCODE_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping opencode_live_smoke (set OPENCODE_LIVE_TEST=1 to run)");
        return;
    }
    let provider = OpenCodeProvider::from_env();
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly the word: pong")],
        ModelId(String::new()),
        // At/above the default max_tokens_floor (4096).
        8_192,
    );
    let resp = provider
        .complete(req)
        .await
        .expect("live opencode completion");
    assert!(
        !resp.content.is_empty(),
        "live opencode returned empty content"
    );
    eprintln!("live opencode replied: {:?}", resp.content);
}
