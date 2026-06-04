//! [`Config`] — the server's startup configuration, read from the environment.
//!
//! Every knob has an env var; the Slack credentials are always required, the
//! [`anthropic_api_key`] is required only when the Anthropic backend is selected
//! (the `ARDUR_PROVIDER` default), and the rest default. [`Config::from_env`] is
//! the production path; tests build a [`Config`] by hand (with a tempdir
//! [`data_dir`] and a wiremock [`slack_base_url`]) so the boot sequence runs
//! without touching the real environment.
//!
//! [`anthropic_api_key`]: Config::anthropic_api_key
//! [`data_dir`]: Config::data_dir
//! [`slack_base_url`]: Config::slack_base_url

use std::path::PathBuf;

use ardur_provider_selector::{ProviderKind, SELECTOR_ENV};

/// How the process emits tracing events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, ANSI-colored lines (the default).
    Text,
    /// One JSON object per event (`ARDUR_LOG_FORMAT=json`) — for log shippers.
    Json,
}

/// The fully-resolved server configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Anthropic API key (`ANTHROPIC_API_KEY`). Required only when the Anthropic
    /// backend is selected (the `ARDUR_PROVIDER` default); empty otherwise. The
    /// live Anthropic provider reads the key from the environment itself, so this
    /// field is informational — tests inject a stub provider and leave it empty.
    pub anthropic_api_key: String,
    /// Slack bot token (`SLACK_BOT_TOKEN`) for `chat.postMessage`.
    pub slack_bot_token: String,
    /// Slack signing secret (`SLACK_SIGNING_SECRET`) for inbound HMAC verification.
    pub slack_signing_secret: String,
    /// Slack app id (`SLACK_APP_ID`) — namespaces channel ids and drops the
    /// bot's own messages (loop-prevention).
    pub slack_app_id: String,
    /// Root of persistent state (`ARDUR_DATA_DIR`, default `./data`): the
    /// `memory/`, `journals/`, `receipts/`, and `keys/` subdirectories.
    pub data_dir: PathBuf,
    /// Address the HTTP listener binds (`ARDUR_BIND_ADDR`, default `0.0.0.0:3000`).
    pub bind_addr: String,
    /// Default model id (`ARDUR_MODEL`, default `claude-opus-4-8`).
    pub model: String,
    /// The per-process cost budget in cents (`ARDUR_COST_BUDGET_CENTS`, default
    /// `10000`). See the note on [`crate::state::AppState`] about why this is
    /// per-process rather than per-session under the Phase-2 cost-gate API.
    pub cost_budget_cents: u64,
    /// Optional path to a Cedar policy file (`ARDUR_CEDAR_POLICY_PATH`). When
    /// unset (or the file is absent) the built-in permissive-but-bounded policy
    /// is used.
    pub cedar_policy_path: Option<PathBuf>,
    /// Override for the Slack Web-API base URL — `None` in production (the
    /// adapter's default), `Some(mock.uri())` in tests.
    pub slack_base_url: Option<String>,
    /// How tracing events are formatted (`ARDUR_LOG_FORMAT`).
    pub log_format: LogFormat,
    /// Whether the §6.0 MCP surface is mounted (`ARDUR_MCP_ENABLED`, default
    /// `false`). When `true`, [`mcp_bearer_tokens`](Self::mcp_bearer_tokens) is
    /// required.
    pub mcp_enabled: bool,
    /// The bearer-token allowlist gating the MCP routes
    /// (`ARDUR_MCP_BEARER_TOKENS`, comma-separated). Required when
    /// [`mcp_enabled`](Self::mcp_enabled) is set; empty otherwise.
    pub mcp_bearer_tokens: Vec<String>,
    /// URL path prefix the MCP routes mount under (`ARDUR_MCP_PATH_PREFIX`,
    /// default `/mcp`). The per-server endpoint is `<prefix>/{server_name}`.
    pub mcp_path_prefix: String,
    /// Remote MCP servers to consume tools from (`ARDUR_MCP_REMOTE_SERVERS`,
    /// `name1=url1,name2=url2,…`), as `(name, url)` pairs. Parsed and surfaced
    /// for the client side; consumed once the runtime gains a tool-execution
    /// stage (`// TODO §6.0 Phase 3`).
    pub mcp_remote_servers: Vec<(String, String)>,
}

/// A required environment variable was unset or empty.
#[derive(Debug, thiserror::Error)]
#[error("required environment variable `{0}` is unset or empty")]
pub struct MissingEnvVar(pub String);

impl Config {
    /// Read the configuration from the process environment.
    ///
    /// `ANTHROPIC_API_KEY` is required only when the selected `ARDUR_PROVIDER`
    /// backend is `anthropic` (the default when unset). For `openrouter`,
    /// `ollama`, and `codex` it is optional, so a real boot under those backends
    /// does not demand an Anthropic key. An unrecognized `ARDUR_PROVIDER` is
    /// treated as non-Anthropic here (the key is not required); the selector
    /// itself rejects the bad value — with a message listing the supported ones —
    /// when the binary builds the provider.
    ///
    /// # Errors
    /// [`MissingEnvVar`] naming the first required variable that is unset or
    /// empty (`SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APP_ID`, and
    /// `ANTHROPIC_API_KEY` when the Anthropic backend is selected).
    pub fn from_env() -> Result<Self, MissingEnvVar> {
        // The Anthropic key gates only the Anthropic backend; under any other
        // selection it is optional (empty when unset).
        let anthropic_selected = matches!(
            ProviderKind::resolve(optional(SELECTOR_ENV).as_deref()),
            Ok(ProviderKind::Anthropic)
        );
        let anthropic_api_key = if anthropic_selected {
            require("ANTHROPIC_API_KEY")?
        } else {
            optional("ANTHROPIC_API_KEY").unwrap_or_default()
        };

        // The MCP surface is opt-in; when enabled it requires a non-empty bearer
        // allowlist (mirrors the Anthropic-key conditional-requirement pattern).
        let mcp_enabled = optional("ARDUR_MCP_ENABLED").as_deref() == Some("true");
        let mcp_bearer_tokens = parse_csv(optional("ARDUR_MCP_BEARER_TOKENS").as_deref());
        if mcp_enabled && mcp_bearer_tokens.is_empty() {
            return Err(MissingEnvVar("ARDUR_MCP_BEARER_TOKENS".to_string()));
        }

        Ok(Self {
            anthropic_api_key,
            slack_bot_token: require("SLACK_BOT_TOKEN")?,
            slack_signing_secret: require("SLACK_SIGNING_SECRET")?,
            slack_app_id: require("SLACK_APP_ID")?,
            data_dir: optional("ARDUR_DATA_DIR")
                .map_or_else(|| PathBuf::from("./data"), PathBuf::from),
            bind_addr: optional("ARDUR_BIND_ADDR").unwrap_or_else(|| "0.0.0.0:3000".to_string()),
            model: optional("ARDUR_MODEL").unwrap_or_else(|| "claude-opus-4-8".to_string()),
            cost_budget_cents: optional("ARDUR_COST_BUDGET_CENTS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10_000),
            cedar_policy_path: optional("ARDUR_CEDAR_POLICY_PATH").map(PathBuf::from),
            slack_base_url: None,
            log_format: match optional("ARDUR_LOG_FORMAT").as_deref() {
                Some("json") => LogFormat::Json,
                _ => LogFormat::Text,
            },
            mcp_enabled,
            mcp_bearer_tokens,
            mcp_path_prefix: optional("ARDUR_MCP_PATH_PREFIX")
                .unwrap_or_else(|| "/mcp".to_string()),
            mcp_remote_servers: parse_remote_servers(
                optional("ARDUR_MCP_REMOTE_SERVERS").as_deref(),
            ),
        })
    }
}

/// Parse a comma-separated token list, trimming whitespace and dropping empties.
fn parse_csv(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Parse `name1=url1,name2=url2,…` into `(name, url)` pairs, skipping malformed
/// entries (those without a single `=`).
fn parse_remote_servers(value: Option<&str>) -> Vec<(String, String)> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter_map(|entry| {
            let entry = entry.trim();
            let (name, url) = entry.split_once('=')?;
            let (name, url) = (name.trim(), url.trim());
            (!name.is_empty() && !url.is_empty()).then(|| (name.to_string(), url.to_string()))
        })
        .collect()
}

/// Read a required env var, mapping an unset/empty value to [`MissingEnvVar`].
fn require(key: &str) -> Result<String, MissingEnvVar> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(MissingEnvVar(key.to_string())),
    }
}

/// Read an optional env var, treating an empty value as unset.
fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}
