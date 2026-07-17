//! `boot_smoke` — [`AppState::boot`] wires the whole substrate over a stub
//! provider + tempdir without panicking, and lays down the persistent state
//! directory layout (the keys it mints on first boot, and the data subdirs).

mod support;

use std::sync::Arc;

use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, build_router, example_registry};
use axum::body::Body;
use axum::http::{Request, StatusCode};

#[tokio::test]
async fn boots_and_lays_down_the_state_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let state = support::boot_stub(&config).await;
    assert_eq!(state.data_dir(), dir.path());

    // The four state subdirectories exist.
    for sub in ["memory", "journals", "receipts", "keys"] {
        assert!(
            dir.path().join(sub).is_dir(),
            "boot creates the {sub}/ subdirectory"
        );
    }

    // The two long-lived keys were minted + persisted on first boot.
    assert!(
        dir.path().join("keys/issuer.key").is_file(),
        "the cap-token issuer key is persisted"
    );
    assert!(
        dir.path().join("keys/receipt.pem").is_file(),
        "the receipt signing key is persisted"
    );
}

/// A second boot over the *same* data dir reuses the persisted keys (it does not
/// panic re-reading them) — the restart path.
#[tokio::test]
async fn second_boot_reuses_persisted_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let _first = support::boot_stub(&config).await;
    let issuer_key = std::fs::read_to_string(dir.path().join("keys/issuer.key")).expect("read key");
    let journal_root = dir.path().join("journals/sessions");
    let first_journals = std::fs::read_dir(&journal_root)
        .expect("first boot journal directory")
        .count();

    let _second = support::boot_stub(&config).await;
    let reread = std::fs::read_to_string(dir.path().join("keys/issuer.key")).expect("read key");
    let second_journals = std::fs::read_dir(&journal_root)
        .expect("second boot journal directory")
        .count();

    assert_eq!(issuer_key, reread, "the issuer key is stable across boots");
    assert_eq!(first_journals, 1, "first boot creates one audit journal");
    assert_eq!(
        second_journals, 1,
        "restart reuses the stable audit journal needed for receipt reconciliation"
    );
}

/// The HTTP-only boot mode: with Slack disabled, `AppState::boot` still lays down
/// the full state directory layout and mints its long-lived keys — the server is
/// fully functional for `/chat` without any Slack credentials.
#[tokio::test]
async fn boots_without_slack_creds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_http_only(&dir);
    assert!(!config.slack_enabled, "the http-only config disables Slack");

    let state = support::boot_stub(&config).await;
    assert_eq!(state.data_dir(), dir.path());
    assert!(
        state.slack().is_none(),
        "no Slack adapter is built in HTTP-only mode"
    );

    // The state subdirectories and both long-lived keys still lay down.
    for sub in ["memory", "journals", "receipts", "keys"] {
        assert!(
            dir.path().join(sub).is_dir(),
            "boot creates the {sub}/ subdirectory even without Slack"
        );
    }
    assert!(
        dir.path().join("keys/issuer.key").is_file(),
        "the cap-token issuer key is persisted"
    );
    assert!(
        dir.path().join("keys/receipt.pem").is_file(),
        "the receipt signing key is persisted"
    );
}

/// Under an HTTP-only boot, `POST /chat` works but the `/slack/events` route is
/// never mounted — so a Slack webhook there gets a `404`.
#[tokio::test]
async fn http_only_chat_works_and_slack_events_is_unmounted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_http_only(&dir);
    let router = build_router(support::boot_stub(&config).await);

    // `/chat` still serves (authorized with the configured bearer token).
    let chat = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            serde_json::json!({ "message": "hello" }).to_string(),
        ))
        .expect("request builds");
    let (chat_status, _chat_body) = support::oneshot(router.clone(), chat).await;
    assert_eq!(chat_status, StatusCode::OK, "/chat works in HTTP-only mode");

    // `/slack/events` is not mounted, so any request there is a 404.
    let slack = Request::builder()
        .method("POST")
        .uri("/slack/events")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("request builds");
    let (slack_status, _slack_body) = support::oneshot(router, slack).await;
    assert_eq!(
        slack_status,
        StatusCode::NOT_FOUND,
        "/slack/events is unmounted when Slack is disabled"
    );
}

#[tokio::test]
async fn configured_missing_cedar_policy_fails_boot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.cedar_policy_path = Some(dir.path().join("missing-policy.cedar"));

    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let tools = Arc::new(example_registry("stub", "in-memory"));
    let err = match AppState::boot(&config, provider, tools).await {
        Ok(_) => panic!("missing policy unexpectedly booted"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("does not exist"),
        "error names missing policy path: {err}"
    );
}
