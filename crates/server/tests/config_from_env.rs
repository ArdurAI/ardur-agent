//! `config_from_env` — [`ardur_server::Config::from_env`] gates the Anthropic
//! API key on the selected `ARDUR_PROVIDER` backend.
//!
//! The Slack credentials are always required; `ANTHROPIC_API_KEY` is required
//! only when the Anthropic backend is selected (the default). A real server boot
//! under `ARDUR_PROVIDER=ollama` (or codex/openrouter) must therefore load its
//! config without an Anthropic key — otherwise provider selection is defeated at
//! config-load, before the selector ever runs.
//!
//! These mutate process-global environment, so they share one serialization lock
//! and save/restore every variable they touch. (`std::env::set_var` is `unsafe`
//! under edition 2024; this is an integration-test crate, not the
//! `#![forbid(unsafe_code)]` library, so the localized `unsafe` is permitted.)

use std::sync::{Mutex, MutexGuard, OnceLock};

use ardur_server::Config;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The variables `from_env` reads, so a test can save/restore the full set and
/// start from a known-clean slate.
const TOUCHED: &[&str] = &[
    "ARDUR_PROVIDER",
    "ANTHROPIC_API_KEY",
    "SLACK_BOT_TOKEN",
    "SLACK_SIGNING_SECRET",
    "SLACK_APP_ID",
    "ARDUR_DATA_DIR",
    "ARDUR_BIND_ADDR",
    "ARDUR_MODEL",
    "ARDUR_COST_BUDGET_CENTS",
    "ARDUR_CEDAR_POLICY_PATH",
    "ARDUR_LOG_FORMAT",
];

fn set(key: &str, value: &str) {
    // SAFETY: serialized by `env_lock`; restored before the guard drops.
    unsafe { std::env::set_var(key, value) }
}

fn unset(key: &str) {
    // SAFETY: serialized by `env_lock`.
    unsafe { std::env::remove_var(key) }
}

/// Snapshot every touched var, clear them all, then restore on drop — so each
/// test runs against a clean environment and leaks nothing to its neighbours.
struct CleanEnv {
    saved: Vec<(&'static str, Option<String>)>,
}

impl CleanEnv {
    fn new() -> Self {
        let saved = TOUCHED
            .iter()
            .map(|&k| (k, std::env::var(k).ok()))
            .collect();
        for &k in TOUCHED {
            unset(k);
        }
        Self { saved }
    }

    /// Set the three Slack credentials `from_env` always requires.
    fn with_slack(self) -> Self {
        set("SLACK_BOT_TOKEN", "xoxb-test");
        set("SLACK_SIGNING_SECRET", "signing-secret-test");
        set("SLACK_APP_ID", "A0TEST");
        self
    }
}

impl Drop for CleanEnv {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => set(k, val),
                None => unset(k),
            }
        }
    }
}

#[test]
fn from_env_requires_anthropic_key_when_default() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack(); // ARDUR_PROVIDER + ANTHROPIC_API_KEY unset

    let err = Config::from_env().expect_err("default (anthropic) must require the key");
    assert_eq!(
        err.to_string(),
        "required environment variable `ANTHROPIC_API_KEY` is unset or empty"
    );
}

#[test]
fn from_env_requires_anthropic_key_when_explicit() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "anthropic");

    let err = Config::from_env().expect_err("explicit anthropic must require the key");
    assert!(
        err.to_string().contains("ANTHROPIC_API_KEY"),
        "error should name the missing key, got: {err}"
    );
}

#[test]
fn from_env_ollama_does_not_require_anthropic_key() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama"); // ANTHROPIC_API_KEY deliberately unset

    let config = Config::from_env().expect("ollama boot must not require an anthropic key");
    assert_eq!(
        config.anthropic_api_key, "",
        "the key is empty under ollama"
    );
    // Sanity: the rest still resolved to their defaults.
    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(config.slack_app_id, "A0TEST");
}

#[test]
fn from_env_codex_does_not_require_anthropic_key() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "CODEX"); // case-insensitive; key unset

    let config = Config::from_env().expect("codex boot must not require an anthropic key");
    assert_eq!(config.anthropic_api_key, "");
}

#[test]
fn from_env_unknown_provider_does_not_require_anthropic_key() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    // An unrecognized selector is treated as non-anthropic here; the selector
    // itself rejects it later when the binary builds the provider.
    set("ARDUR_PROVIDER", "mistral");

    let config =
        Config::from_env().expect("an unknown provider must not require an anthropic key here");
    assert_eq!(config.anthropic_api_key, "");
}

#[test]
fn from_env_still_requires_slack_credentials() {
    let _guard = env_lock();
    let _env = CleanEnv::new(); // no slack creds, ollama selected
    set("ARDUR_PROVIDER", "ollama");

    let err = Config::from_env().expect_err("slack credentials are always required");
    assert!(
        err.to_string().contains("SLACK_"),
        "error should name a missing Slack variable, got: {err}"
    );
}
