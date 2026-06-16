//! Integration coverage for [`ardur_provider_selector::from_env`] — the real
//! `ARDUR_PROVIDER` read and the per-backend credential reads.
//!
//! These exercise process-global environment, so they share one serialization
//! lock and save/restore every variable they touch. (`std::env::set_var` is
//! `unsafe` under edition 2024; this is an integration-test crate, not the
//! `#![forbid(unsafe_code)]` library, so the localized `unsafe` is permitted and
//! confined to the test harness.)

use std::sync::{Mutex, MutexGuard, OnceLock};

use ardur_provider_selector::{ModelId, from_env};

/// Serializes the env-mutating tests in this binary (they all run in one
/// process and would otherwise race on `ARDUR_PROVIDER`).
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Set or clear a variable, returning its prior value so the caller can restore.
fn swap(key: &str, value: Option<&str>) -> Option<String> {
    let prior = std::env::var(key).ok();
    // SAFETY: serialized by `env_lock`; restored before the guard drops.
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    prior
}

fn restore(key: &str, prior: Option<String>) {
    // SAFETY: serialized by `env_lock`.
    unsafe {
        match prior {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

fn model() -> ModelId {
    ModelId::new("test-model")
}

#[test]
fn from_env_default_selects_anthropic_when_unset() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", None);
    let prior_key = swap("ANTHROPIC_API_KEY", Some("sk-test"));

    let provider = from_env(model()).expect("anthropic builds with a key present");
    assert_eq!(provider.id().0, "anthropic");

    restore("ANTHROPIC_API_KEY", prior_key);
    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_openrouter_selects_openrouter() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("openrouter"));
    let prior_key = swap("OPENROUTER_API_KEY", Some("sk-or-test"));

    let provider = from_env(model()).expect("openrouter builds with a key present");
    assert_eq!(provider.id().0, "openrouter");

    restore("OPENROUTER_API_KEY", prior_key);
    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_ollama_local_selects_ollama() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("ollama"));
    // No OLLAMA_API_KEY → local daemon. Infallible.
    let prior_key = swap("OLLAMA_API_KEY", None);

    let provider = from_env(model()).expect("ollama is infallible");
    assert_eq!(provider.id().0, "ollama");

    restore("OLLAMA_API_KEY", prior_key);
    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_ollama_cloud_selects_ollama() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("ollama"));
    // An API key with no explicit base URL routes the Ollama backend at the
    // hosted cloud; the build stays infallible and still reports id "ollama".
    let prior_key = swap("OLLAMA_API_KEY", Some("ollama-cloud-test"));
    let prior_url = swap("OLLAMA_BASE_URL", None);

    let provider = from_env(model()).expect("ollama cloud is infallible");
    assert_eq!(provider.id().0, "ollama");

    restore("OLLAMA_BASE_URL", prior_url);
    restore("OLLAMA_API_KEY", prior_key);
    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_codex_selects_codex() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("Codex")); // case-insensitive

    let provider = from_env(model()).expect("codex is infallible");
    assert_eq!(provider.id().0, "codex");

    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_claude_cli_selects_claude_cli() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("claude-subscription")); // alias

    let provider = from_env(model()).expect("claude-cli is infallible");
    assert_eq!(provider.id().0, "claude-cli");

    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_unknown_provider_returns_error() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("mistral"));
    let result = from_env(model());
    restore("ARDUR_PROVIDER", prior_sel);
    match result {
        Err(e) => {
            let msg = format!("{e}");
            assert!(msg.contains("invalid provider selection"), "error mentions invalid selection: {msg}");
            assert!(msg.contains("mistral"), "error mentions the invalid provider: {msg}");
        }
        Ok(_) => panic!("expected an error for unknown provider, but selection succeeded"),
    }
}
