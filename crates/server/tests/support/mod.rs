//! Shared scaffolding for the `ardur-server` integration tests: deterministic
//! Slack test credentials, a genuine `v0=` request signer, a [`Config`] over a
//! tempdir + stub provider, and a `oneshot` helper that drives the router
//! in-process (no socket).

#![allow(dead_code)] // each test file uses a different subset.

use std::sync::Arc;

use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, Config, LogFormat, MemoryBackend, build_router, example_registry};
use axum::Router;
use axum::body::Bytes;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tempfile::TempDir;
use tower::ServiceExt as _;

type HmacSha256 = Hmac<Sha256>;

/// Test Slack credentials — shared by the signer and the booted adapter.
pub const BOT_TOKEN: &str = "xoxb-server-test-token";
pub const SIGNING_SECRET: &str = "server-signing-secret-000000000000";
pub const APP_ID: &str = "A0SERVERTEST";
pub const CHAT_TOKEN: &str = "server-chat-token-000000000000";

/// The current Unix time in seconds, as a string — a fresh Slack request
/// timestamp that clears the adapter's ±5-minute replay window.
#[must_use]
pub fn now_unix_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
        .to_string()
}

/// Recompute the genuine Slack `v0=<hex>` request signature over the basestring.
#[must_use]
pub fn sign(timestamp: &str, body: &str) -> String {
    let basestring = format!("v0:{timestamp}:{body}");
    let mut mac =
        HmacSha256::new_from_slice(SIGNING_SECRET.as_bytes()).expect("hmac accepts any key length");
    mac.update(basestring.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

/// A [`Config`] rooted at `data_dir`, pointing the Slack adapter at `slack_base`
/// (a wiremock URI) and carrying the test credentials. The Anthropic key is
/// empty — tests inject the stub provider.
#[must_use]
pub fn test_config(data_dir: &TempDir, slack_base: Option<String>) -> Config {
    Config {
        anthropic_api_key: String::new(),
        slack_enabled: true,
        slack_bot_token: Some(BOT_TOKEN.to_string()),
        slack_signing_secret: Some(SIGNING_SECRET.to_string()),
        slack_app_id: Some(APP_ID.to_string()),
        slack_allowed_senders: vec!["U4242".to_string()],
        data_dir: data_dir.path().to_path_buf(),
        bind_addr: "127.0.0.1:0".to_string(),
        chat_bearer_tokens: vec![CHAT_TOKEN.to_string()],
        admin_bearer_tokens: Vec::new(),
        dev_permissive_policy: true,
        model: "claude-opus-4-8".to_string(),
        cost_budget_cents: 10_000,
        cedar_policy_path: None,
        slack_base_url: slack_base,
        channel_matrix: false,
        channel_discord: false,
        channel_telegram: false,
        log_format: LogFormat::Text,
        mcp_enabled: false,
        mcp_bearer_tokens: Vec::new(),
        mcp_path_prefix: "/mcp".to_string(),
        mcp_remote_servers: Vec::new(),
        skills_dirs: Vec::new(),
        memory_backend: MemoryBackend::InMemory,
        qdrant_url: None,
        qdrant_collection: None,
    }
}

/// A [`Config`] like [`test_config`] but with the Slack channel disabled — the
/// HTTP-only boot mode. The three `slack_*` credentials are `None`, so
/// [`AppState::boot`] builds no Slack adapter and [`build_router`] omits the
/// `/slack/events` route. `/chat` still works.
#[must_use]
pub fn test_config_http_only(data_dir: &TempDir) -> Config {
    Config {
        slack_enabled: false,
        slack_bot_token: None,
        slack_signing_secret: None,
        slack_app_id: None,
        slack_allowed_senders: Vec::new(),
        ..test_config(data_dir, None)
    }
}

/// A [`Config`] like [`test_config`] but with the MCP surface enabled and gated
/// by `bearer_tokens`.
#[must_use]
pub fn test_config_with_mcp(
    data_dir: &TempDir,
    slack_base: Option<String>,
    bearer_tokens: Vec<String>,
) -> Config {
    Config {
        mcp_enabled: true,
        mcp_bearer_tokens: bearer_tokens,
        ..test_config(data_dir, slack_base)
    }
}

/// A [`Config`] like [`test_config`] but with the admin runtime-inspection API
/// bearer-gated by `admin_tokens`.
#[must_use]
pub fn test_config_with_admin(
    data_dir: &TempDir,
    slack_base: Option<String>,
    admin_tokens: Vec<String>,
) -> Config {
    Config {
        admin_bearer_tokens: admin_tokens,
        ..test_config(data_dir, slack_base)
    }
}

/// Boot an [`AppState`] over the deterministic stub provider (no network).
#[must_use]
pub async fn boot_stub(config: &Config) -> Arc<AppState> {
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let tools = Arc::new(example_registry("stub", "in-memory"));
    AppState::boot(config, provider, tools)
        .await
        .expect("AppState boots")
}

/// Boot the stub-backed router for `config`.
pub async fn boot_router(config: &Config) -> Router {
    build_router(boot_stub(config).await)
}

/// Drive a single request through `router` via `oneshot`, returning the status
/// and the collected body bytes.
pub async fn oneshot(router: Router, request: Request<axum::body::Body>) -> (StatusCode, Bytes) {
    let response = router.oneshot(request).await.expect("router responds");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    (status, body)
}
