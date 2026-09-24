//! Gated live smoke against the real `codex` CLI. Off by default so CI
//! never spends money / requires an install.

use ardur_provider_codex::CodexProvider;
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider};
use ardur_runtime::ChatMessage;

#[tokio::test]
#[ignore = "requires a logged-in codex install; set CODEX_LIVE_TEST=1"]
async fn codex_live_smoke() {
    if std::env::var("CODEX_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping codex_live_smoke (set CODEX_LIVE_TEST=1 to run)");
        return;
    }
    let provider = CodexProvider::from_env(ModelId::new(""));
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly the word: pong")],
        ModelId::new(""),
        // At/above the default max_tokens_floor (4096).
        8_192,
    );
    let resp = provider.complete(req).await.expect("live codex completion");
    assert!(
        !resp.content.is_empty(),
        "live codex returned empty content"
    );
    eprintln!("live codex replied: {:?}", resp.content);
}
