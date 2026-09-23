//! Gated live smoke against the real `kimi` binary. Off by default so CI
//! never spends money / requires an install.

use ardur_provider_kimi::KimiProvider;
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider};
use ardur_runtime::ChatMessage;

#[tokio::test]
#[ignore = "requires a logged-in kimi install; set KIMI_LIVE_TEST=1"]
async fn kimi_live_smoke() {
    if std::env::var("KIMI_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping kimi_live_smoke (set KIMI_LIVE_TEST=1 to run)");
        return;
    }
    let provider = KimiProvider::from_env();
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly the word: pong")],
        ModelId(String::new()),
        // At/above the default max_tokens_floor (4096).
        8_192,
    );
    let resp = provider.complete(req).await.expect("live kimi completion");
    assert!(!resp.content.is_empty(), "live kimi returned empty content");
    eprintln!("live kimi replied: {:?}", resp.content);
}
