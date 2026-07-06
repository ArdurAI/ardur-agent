//! `provider_selection` — the server boots over a provider chosen by the
//! `ARDUR_PROVIDER` selector, not just the hard-coded Anthropic backend.
//!
//! The binary's `main` builds the provider with
//! [`ardur_provider_selector::from_env`] and hands it to [`AppState::boot`].
//! This drives that same seam with explicit selector values: selecting `ollama`
//! yields an Ollama-backed runtime, `codex` yields the local Codex wrapper, and
//! `openai-compat` yields the OpenAI-compatible HTTP backend. No turn runs, so
//! no provider network call or subprocess invocation is made.

mod support;

use std::sync::Arc;

use ardur_provider_selector::{ModelId, select};
use ardur_server::AppState;
use serial_test::serial;

const OPENAI_COMPAT_ENV: &[&str] = &[
    "OPENAI_COMPAT_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_COMPAT_BASE_URL",
    "OPENAI_COMPAT_TIMEOUT_SECS",
];

fn set_env(key: &str, value: &str) {
    // SAFETY: this test module uses the serialized `openai-compat` boot test
    // and restores every touched key before the test exits.
    unsafe { std::env::set_var(key, value) }
}

fn unset_env(key: &str) {
    // SAFETY: see `set_env`.
    unsafe { std::env::remove_var(key) }
}

struct CleanOpenAiCompatEnv {
    saved: Vec<(&'static str, Option<String>)>,
}

impl CleanOpenAiCompatEnv {
    fn new() -> Self {
        let saved = OPENAI_COMPAT_ENV
            .iter()
            .map(|&key| (key, std::env::var(key).ok()))
            .collect();
        for &key in OPENAI_COMPAT_ENV {
            unset_env(key);
        }
        Self { saved }
    }

    fn set(&self, key: &str, value: &str) {
        set_env(key, value);
    }
}

impl Drop for CleanOpenAiCompatEnv {
    fn drop(&mut self) {
        for (key, prior) in &self.saved {
            match prior {
                Some(value) => set_env(key, value),
                None => unset_env(key),
            }
        }
    }
}

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
    let tools = Arc::new(ardur_server::example_registry("ollama", "in-memory"));
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

    let tools = Arc::new(ardur_server::example_registry("codex", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("AppState boots over codex");
    assert_eq!(state.data_dir(), dir.path());
}

#[tokio::test]
#[serial]
async fn server_boots_with_openai_compat_provider_selection() {
    let env = CleanOpenAiCompatEnv::new();
    env.set("OPENAI_COMPAT_API_KEY", "sk-test-openai-compat");

    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let provider = select(Some("openai-compat"), ModelId::new(&config.model))
        .expect("openai-compat builds with a key present");
    assert_eq!(
        provider.id().0,
        "openai-compat",
        "the selector wired the OpenAI-compatible backend"
    );

    let tools = Arc::new(ardur_server::example_registry("openai-compat", "in-memory"));
    let state =
        AppState::boot(&config, provider, tools).expect("AppState boots over openai-compat");
    assert_eq!(state.data_dir(), dir.path());
}
