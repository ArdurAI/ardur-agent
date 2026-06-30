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

use ardur_server::{Config, MemoryBackend};
use serial_test::serial;

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
    "ARDUR_CHAT_BEARER_TOKENS",
    "ARDUR_DEV_PERMISSIVE_POLICY",
    "ARDUR_MODEL",
    "ARDUR_COST_BUDGET_CENTS",
    "ARDUR_CEDAR_POLICY_PATH",
    "ARDUR_LOG_FORMAT",
    "ARDUR_MCP_ENABLED",
    "ARDUR_MCP_BEARER_TOKENS",
    "ARDUR_MCP_PATH_PREFIX",
    "ARDUR_MCP_REMOTE_SERVERS",
    "ARDUR_MEMORY",
    "QDRANT_URL",
    "QDRANT_COLLECTION",
    "ARDUR_CHANNEL_MATRIX",
    "ARDUR_CHANNEL_DISCORD",
    "DISCORD_BOT_TOKEN",
    "DISCORD_APPLICATION_ID",
    "ARDUR_CHANNEL_TELEGRAM",
    "TELEGRAM_BOT_TOKEN",
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
#[serial]
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
#[serial]
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
#[serial]
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
    assert_eq!(config.bind_addr, "127.0.0.1:3000");
    assert!(config.chat_bearer_tokens.is_empty());
    assert!(!config.dev_permissive_policy);
    assert_eq!(config.slack_app_id, "A0TEST");
}

#[test]
#[serial]
fn from_env_codex_does_not_require_anthropic_key() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "CODEX"); // case-insensitive; key unset

    let config = Config::from_env().expect("codex boot must not require an anthropic key");
    assert_eq!(config.anthropic_api_key, "");
}

#[test]
#[serial]
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
#[serial]
fn from_env_parses_chat_auth_tokens_and_dev_policy_flag() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_CHAT_BEARER_TOKENS", "chat-a, chat-b ,chat-c");
    set("ARDUR_DEV_PERMISSIVE_POLICY", "true");

    let config = Config::from_env().expect("chat auth config parses");
    assert_eq!(
        config.chat_bearer_tokens,
        vec!["chat-a", "chat-b", "chat-c"]
    );
    assert!(config.dev_permissive_policy);
}

#[test]
#[serial]
fn from_env_mcp_disabled_by_default() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");

    let config = Config::from_env().expect("boots with MCP unset");
    assert!(
        !config.mcp_enabled,
        "MCP is off unless ARDUR_MCP_ENABLED=true"
    );
    assert!(config.mcp_bearer_tokens.is_empty());
    assert_eq!(config.mcp_path_prefix, "/mcp");
    assert!(config.mcp_remote_servers.is_empty());
}

#[test]
#[serial]
fn from_env_mcp_enabled_requires_bearer_tokens() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MCP_ENABLED", "true"); // ARDUR_MCP_BEARER_TOKENS deliberately unset

    let err = Config::from_env().expect_err("enabled MCP must require a bearer allowlist");
    assert!(
        err.to_string().contains("ARDUR_MCP_BEARER_TOKENS"),
        "error should name the missing token list, got: {err}"
    );
}

#[test]
#[serial]
fn from_env_mcp_parses_tokens_prefix_and_remotes() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MCP_ENABLED", "true");
    set("ARDUR_MCP_BEARER_TOKENS", "tok-a, tok-b ,tok-c");
    set("ARDUR_MCP_PATH_PREFIX", "/tools");
    set(
        "ARDUR_MCP_REMOTE_SERVERS",
        "alpha=http://a.local/mcp, beta=http://b.local/mcp",
    );

    let config = Config::from_env().expect("MCP config parses");
    assert!(config.mcp_enabled);
    assert_eq!(config.mcp_bearer_tokens, vec!["tok-a", "tok-b", "tok-c"]);
    assert_eq!(config.mcp_path_prefix, "/tools");
    assert_eq!(
        config.mcp_remote_servers,
        vec![
            ("alpha".to_string(), "http://a.local/mcp".to_string()),
            ("beta".to_string(), "http://b.local/mcp".to_string()),
        ]
    );
}

#[test]
#[serial]
fn from_env_defaults_to_in_memory_without_qdrant_url() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    // Use a non-anthropic provider so the test isolates the *memory* default
    // (no ANTHROPIC_API_KEY needed). ARDUR_MEMORY + QDRANT_URL deliberately unset.
    set("ARDUR_PROVIDER", "ollama");

    let config = Config::from_env().expect("the default in-memory backend needs no QDRANT_URL");
    assert_eq!(config.memory_backend, MemoryBackend::InMemory);
    assert_eq!(config.qdrant_url, None);
    assert_eq!(config.qdrant_collection, None);
}

#[test]
#[serial]
fn from_env_requires_qdrant_url_when_qdrant_selected() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    // Non-anthropic provider so the Anthropic-key check passes first; the only
    // missing requirement is then QDRANT_URL — mirroring the Anthropic gate.
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MEMORY", "qdrant"); // QDRANT_URL deliberately unset

    let err = Config::from_env().expect_err("the qdrant backend must require QDRANT_URL");
    assert_eq!(
        err.to_string(),
        "required environment variable `QDRANT_URL` is unset or empty"
    );
}

#[test]
#[serial]
fn from_env_qdrant_selected_with_url_loads() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MEMORY", "qdrant");
    set("QDRANT_URL", "http://localhost:6334");
    set("QDRANT_COLLECTION", "ardur_test_collection");

    let config = Config::from_env().expect("qdrant + a URL loads");
    assert_eq!(config.memory_backend, MemoryBackend::Qdrant);
    assert_eq!(config.qdrant_url.as_deref(), Some("http://localhost:6334"));
    assert_eq!(
        config.qdrant_collection.as_deref(),
        Some("ardur_test_collection")
    );
}

#[test]
#[serial]
fn parses_hybrid_from_env() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MEMORY", "hybrid");
    set("QDRANT_URL", "http://localhost:6334");

    let config = Config::from_env().expect("hybrid + a URL loads");
    assert_eq!(config.memory_backend, MemoryBackend::Hybrid);
    // The §7.0c hybrid retriever layers BM25 + an embedder over the *same*
    // durable Qdrant store, so it requires `QDRANT_URL` exactly like `qdrant`.
    assert_eq!(config.qdrant_url.as_deref(), Some("http://localhost:6334"));
}

#[test]
#[serial]
fn from_env_requires_qdrant_url_when_hybrid_selected() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_MEMORY", "hybrid"); // QDRANT_URL deliberately unset

    let err = Config::from_env().expect_err("the hybrid backend must require QDRANT_URL");
    assert_eq!(
        err.to_string(),
        "required environment variable `QDRANT_URL` is unset or empty"
    );
}

#[test]
#[serial]
fn from_env_discord_disabled_by_default() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");

    let config = Config::from_env().expect("boots with the discord channel unset");
    assert!(
        !config.channel_discord,
        "discord is off unless ARDUR_CHANNEL_DISCORD is truthy"
    );
}

#[test]
#[serial]
fn from_env_discord_enabled_requires_credentials() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_CHANNEL_DISCORD", "true"); // DISCORD_* deliberately unset

    let err = Config::from_env().expect_err("enabled discord must require its credentials");
    assert!(
        err.to_string().contains("DISCORD_BOT_TOKEN"),
        "error should name the missing discord token, got: {err}"
    );
}

#[test]
#[serial]
fn from_env_discord_enabled_with_credentials_loads() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_CHANNEL_DISCORD", "true");
    set("DISCORD_BOT_TOKEN", "discord-token");
    set("DISCORD_APPLICATION_ID", "123456789012345678");

    let config = Config::from_env().expect("discord + credentials loads");
    assert!(config.channel_discord);
}

#[test]
#[serial]
fn from_env_telegram_enabled_requires_token() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_CHANNEL_TELEGRAM", "1"); // TELEGRAM_BOT_TOKEN deliberately unset

    let err = Config::from_env().expect_err("enabled telegram must require its token");
    assert!(
        err.to_string().contains("TELEGRAM_BOT_TOKEN"),
        "error should name the missing telegram token, got: {err}"
    );
}

#[test]
#[serial]
fn from_env_telegram_enabled_with_token_loads() {
    let _guard = env_lock();
    let _env = CleanEnv::new().with_slack();
    set("ARDUR_PROVIDER", "ollama");
    set("ARDUR_CHANNEL_TELEGRAM", "yes");
    set("TELEGRAM_BOT_TOKEN", "123:telegram-token");

    let config = Config::from_env().expect("telegram + token loads");
    assert!(config.channel_telegram);
}

#[test]
#[serial]
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
