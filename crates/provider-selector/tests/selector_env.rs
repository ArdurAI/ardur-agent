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
fn from_env_openai_compat_selects_openai_compat() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("openai-compat"));
    let prior_key = swap("OPENAI_COMPAT_API_KEY", Some("sk-compat-test"));
    let prior_openai_key = swap("OPENAI_API_KEY", None);
    let prior_base = swap("OPENAI_COMPAT_BASE_URL", None);
    let prior_timeout = swap("OPENAI_COMPAT_TIMEOUT_SECS", None);

    let provider = from_env(model()).expect("openai-compat builds with a key present");
    assert_eq!(provider.id().0, "openai-compat");

    restore("OPENAI_COMPAT_TIMEOUT_SECS", prior_timeout);
    restore("OPENAI_COMPAT_BASE_URL", prior_base);
    restore("OPENAI_API_KEY", prior_openai_key);
    restore("OPENAI_COMPAT_API_KEY", prior_key);
    restore("ARDUR_PROVIDER", prior_sel);
}

#[test]
fn from_env_openai_compat_accepts_openai_api_key_fallback() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("openai"));
    let prior_compat_key = swap("OPENAI_COMPAT_API_KEY", None);
    let prior_openai_key = swap("OPENAI_API_KEY", Some("sk-openai-test"));
    let prior_base = swap("OPENAI_COMPAT_BASE_URL", None);
    let prior_timeout = swap("OPENAI_COMPAT_TIMEOUT_SECS", None);

    let provider = from_env(model()).expect("openai alias builds from OPENAI_API_KEY");
    assert_eq!(provider.id().0, "openai-compat");

    restore("OPENAI_COMPAT_TIMEOUT_SECS", prior_timeout);
    restore("OPENAI_COMPAT_BASE_URL", prior_base);
    restore("OPENAI_API_KEY", prior_openai_key);
    restore("OPENAI_COMPAT_API_KEY", prior_compat_key);
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
fn from_env_unknown_provider_returns_helpful_error() {
    let _guard = env_lock();
    let prior_sel = swap("ARDUR_PROVIDER", Some("mistral"));

    let result = from_env(model());
    restore("ARDUR_PROVIDER", prior_sel);
    assert!(result.is_err(), "unknown provider should return an error");
    let message = match result {
        Err(e) => format!("{}", e),
        Ok(_) => panic!("expected error"),
    };
    assert!(message.contains("supported values are"), "{message}");
    assert!(message.contains("openai-compat"), "{message}");
}
