//! Integration coverage for [`ardur_provider_selector::from_env`] — the real
//! `ARDUR_PROVIDER` read and the per-backend credential reads.
//!
//! These exercise process-global environment, so they share one serialization
//! lock and save/restore every variable they touch via `CleanEnv`. The localized
//! `unsafe` calls are confined to the helper and are restored before the lock is
//! released.

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

/// Every variable this test file may alter. `CleanEnv` snapshots them, clears
/// them before the test body, and restores them on drop so panic paths do not
/// leak state into neighbouring tests or the parent process.
const TOUCHED: &[&str] = &[
    "ARDUR_PROVIDER",
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENAI_COMPAT_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_COMPAT_BASE_URL",
    "OPENAI_COMPAT_TIMEOUT_SECS",
    "OLLAMA_API_KEY",
    "OLLAMA_BASE_URL",
    "ARDUR_AZURE_OPENAI_API_KEY",
    "ARDUR_AZURE_OPENAI_RESOURCE",
    "ARDUR_AZURE_OPENAI_DEPLOYMENT",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_REGION",
    "ARDUR_VERTEX_ACCESS_TOKEN",
    "ARDUR_VERTEX_PROJECT",
];

fn set_env(key: &str, value: &str) {
    // SAFETY: all callers hold `env_lock`; `CleanEnv` restores each touched key
    // before that lock is released.
    unsafe { std::env::set_var(key, value) }
}

fn unset_env(key: &str) {
    // SAFETY: all callers hold `env_lock`; `CleanEnv` restores each touched key
    // before that lock is released.
    unsafe { std::env::remove_var(key) }
}

struct CleanEnv {
    saved: Vec<(&'static str, Option<String>)>,
}

impl CleanEnv {
    fn new() -> Self {
        let saved = TOUCHED
            .iter()
            .map(|&key| (key, std::env::var(key).ok()))
            .collect();
        for &key in TOUCHED {
            unset_env(key);
        }
        Self { saved }
    }

    fn set(&self, key: &str, value: &str) {
        set_env(key, value);
    }
}

impl Drop for CleanEnv {
    fn drop(&mut self) {
        for (key, prior) in &self.saved {
            match prior {
                Some(value) => set_env(key, value),
                None => unset_env(key),
            }
        }
    }
}

fn model() -> ModelId {
    ModelId::new("test-model")
}

#[test]
fn from_env_default_selects_anthropic_when_unset() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ANTHROPIC_API_KEY", "sk-test");

    let provider = from_env(model()).expect("anthropic builds with a key present");
    assert_eq!(provider.id().0, "anthropic");
}

#[test]
fn from_env_openrouter_selects_openrouter() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "openrouter");
    env.set("OPENROUTER_API_KEY", "sk-or-test");

    let provider = from_env(model()).expect("openrouter builds with a key present");
    assert_eq!(provider.id().0, "openrouter");
}

#[test]
fn from_env_openai_compat_selects_openai_compat() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "openai-compat");
    env.set("OPENAI_COMPAT_API_KEY", "***");

    let provider = from_env(model()).expect("openai-compat builds with a key present");
    assert_eq!(provider.id().0, "openai-compat");
}

#[test]
fn from_env_openai_compat_accepts_openai_api_key_fallback() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "openai");
    env.set("OPENAI_API_KEY", "***");

    let provider = from_env(model()).expect("openai alias builds from OPENAI_API_KEY");
    assert_eq!(provider.id().0, "openai-compat");
}

#[test]
fn from_env_ollama_local_selects_ollama() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "ollama");
    // No OLLAMA_API_KEY → local daemon. Infallible.

    let provider = from_env(model()).expect("ollama is infallible");
    assert_eq!(provider.id().0, "ollama");
}

#[test]
fn from_env_ollama_cloud_selects_ollama() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "ollama");
    // An API key with no explicit base URL routes the Ollama backend at the
    // hosted cloud; the build stays infallible and still reports id "ollama".
    env.set("OLLAMA_API_KEY", "ollama-cloud-test");

    let provider = from_env(model()).expect("ollama cloud is infallible");
    assert_eq!(provider.id().0, "ollama");
}

#[test]
fn from_env_codex_selects_codex() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "Codex"); // case-insensitive

    let provider = from_env(model()).expect("codex is infallible");
    assert_eq!(provider.id().0, "codex");
}

#[test]
fn from_env_claude_cli_selects_claude_cli() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "claude-subscription"); // alias

    let provider = from_env(model()).expect("claude-cli is infallible");
    assert_eq!(provider.id().0, "claude-cli");
}

#[test]
fn from_env_azure_openai_selects_azure_openai() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "azure-openai");
    env.set("ARDUR_AZURE_OPENAI_API_KEY", "***");
    env.set("ARDUR_AZURE_OPENAI_RESOURCE", "my-resource");
    env.set("ARDUR_AZURE_OPENAI_DEPLOYMENT", "gpt-4o-deployment");

    let provider = from_env(model()).expect("azure-openai builds with config present");
    assert_eq!(provider.id().0, "azure-openai");
}

#[test]
fn from_env_bedrock_selects_bedrock() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "bedrock");
    env.set("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
    env.set("AWS_SECRET_ACCESS_KEY", "***");

    let provider = from_env(model()).expect("bedrock builds with credentials present");
    assert_eq!(provider.id().0, "bedrock");
}

#[test]
fn from_env_vertex_selects_vertex() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "vertex");
    env.set("ARDUR_VERTEX_ACCESS_TOKEN", "***");
    env.set("ARDUR_VERTEX_PROJECT", "my-project");

    let provider = from_env(model()).expect("vertex builds with config present");
    assert_eq!(provider.id().0, "vertex");
}

#[test]
fn from_env_unknown_provider_returns_helpful_error() {
    let _guard = env_lock();
    let env = CleanEnv::new();
    env.set("ARDUR_PROVIDER", "mistral");

    let result = from_env(model());
    assert!(result.is_err(), "unknown provider should return an error");
    let message = match result {
        Err(e) => format!("{}", e),
        Ok(_) => panic!("expected error"),
    };
    assert!(message.contains("supported values are"), "{message}");
    assert!(message.contains("openai-compat"), "{message}");
}
