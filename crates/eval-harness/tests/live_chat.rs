//! Gated live end-to-end test against a *real* `ardur-server` `/chat` endpoint.
//!
//! Unlike the `wiremock`-backed [`runner`](super) tests, this drives the
//! scenarios under `scenarios/live/` against an actual running server (and thus
//! a real model). It is **skipped by default** — it runs only when BOTH:
//!
//! - `ARDUR_LIVE_CHAT_TEST=1` is set, AND
//! - `ARDUR_LIVE_CHAT_URL=<base-url>` points at a running server,
//!
//! so CI (which has neither a server nor model credentials) is unaffected.
//!
//! ```sh
//! ARDUR_LIVE_CHAT_TEST=1 ARDUR_LIVE_CHAT_URL=http://localhost:8080 \
//!   cargo test -p ardur-eval --test live_chat -- --nocapture
//! ```

use std::path::Path;

use ardur_eval::output::Summary;
use ardur_eval::runner::{RunConfig, run_scenario};
use ardur_eval::scenario::Scenario;

/// The live scenario directory, resolved relative to this crate's manifest so
/// the test is independent of the working directory.
fn live_scenarios_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/live")
}

#[tokio::test]
async fn live_chat_scenarios_pass_against_real_server() {
    // Both gates must be present, else this is a no-op (and reports why).
    if std::env::var("ARDUR_LIVE_CHAT_TEST").as_deref() != Ok("1") {
        eprintln!("skipping live_chat: set ARDUR_LIVE_CHAT_TEST=1 to enable");
        return;
    }
    let Ok(server_url) = std::env::var("ARDUR_LIVE_CHAT_URL") else {
        eprintln!("skipping live_chat: set ARDUR_LIVE_CHAT_URL=<base-url> to enable");
        return;
    };

    let dir = live_scenarios_dir();
    let cases = Scenario::load_dir(&dir).expect("live scenarios load");
    assert!(!cases.is_empty(), "no live scenarios found in {dir:?}");

    let config = RunConfig::new(server_url);
    let client = reqwest::Client::new();

    let mut results = Vec::with_capacity(cases.len());
    for scenario in &cases {
        let result = run_scenario(&client, &config, scenario).await;
        eprintln!("{:<24} {:?}", result.id, result.outcome);
        results.push(result);
    }

    let summary = Summary::of(&results);
    assert!(
        summary.is_green(),
        "live eval not green: {} passed, {} failed, {} errored",
        summary.passed,
        summary.failed,
        summary.errored,
    );
}
