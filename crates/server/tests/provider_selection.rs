//! `provider_selection` — the server boots over a provider chosen by the
//! `ARDUR_PROVIDER` selector, not just the hard-coded Anthropic backend.
//!
//! The binary's `main` builds the provider with
//! [`ardur_provider_selector::from_env`] and hands it to [`AppState::boot`].
//! This drives that same seam with an explicit selector value (no process-env
//! mutation needed): selecting `ollama` yields an Ollama-backed runtime that
//! boots cleanly and lays down its state directory. No turn runs, so no network
//! call is made to the local Ollama daemon.

mod support;

use ardur_provider_selector::{ModelId, select};
use ardur_server::AppState;

#[tokio::test]
async fn server_boots_with_ollama_provider_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    // Select the provider exactly as `ardur-server`'s `main` does, but with an
    // explicit selector value (= `ARDUR_PROVIDER=ollama`). Ollama needs no
    // credentials, so the build is infallible and defaults to the local daemon.
    let provider = select(Some("ollama"), ModelId::new(&config.model))
        .expect("ollama selection is infallible");
    assert_eq!(
        provider.id().0,
        "ollama",
        "the selector wired the Ollama backend"
    );

    // The whole substrate boots over the selected provider without panicking,
    // and lays down the persistent state directory layout.
    let tools = std::sync::Arc::new(ardur_server::example_registry("ollama", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("AppState boots over ollama");
    assert_eq!(state.data_dir(), dir.path());
    for sub in ["memory", "journals", "receipts", "keys"] {
        assert!(
            dir.path().join(sub).is_dir(),
            "boot creates the {sub}/ subdirectory over the selected provider"
        );
    }
}

#[tokio::test]
async fn server_boots_with_codex_provider_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    // Codex is likewise credential-free at boot (it wraps the local `codex` CLI
    // and does not probe it until a turn runs).
    let provider =
        select(Some("codex"), ModelId::new(&config.model)).expect("codex selection is infallible");
    assert_eq!(provider.id().0, "codex");

    let tools = std::sync::Arc::new(ardur_server::example_registry("codex", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("AppState boots over codex");
    assert_eq!(state.data_dir(), dir.path());
}
