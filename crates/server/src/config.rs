//! [`Config`] — the server's startup configuration, read from the environment.
//!
//! Every knob has an env var; the secrets ([`anthropic_api_key`], the Slack
//! credentials) are required and the rest default. [`Config::from_env`] is the
//! production path; tests build a [`Config`] by hand (with a tempdir
//! [`data_dir`] and a wiremock [`slack_base_url`]) so the boot sequence runs
//! without touching the real environment.
//!
//! [`anthropic_api_key`]: Config::anthropic_api_key
//! [`data_dir`]: Config::data_dir
//! [`slack_base_url`]: Config::slack_base_url

use std::path::PathBuf;

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
    /// Anthropic API key (`ANTHROPIC_API_KEY`). Used only to build the live
    /// provider in the binary; tests inject a stub provider and leave this empty.
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
}

/// A required environment variable was unset or empty.
#[derive(Debug, thiserror::Error)]
#[error("required environment variable `{0}` is unset or empty")]
pub struct MissingEnvVar(pub String);

impl Config {
    /// Read the configuration from the process environment.
    ///
    /// # Errors
    /// [`MissingEnvVar`] naming the first required variable that is unset or
    /// empty (`ANTHROPIC_API_KEY`, `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`,
    /// `SLACK_APP_ID`).
    pub fn from_env() -> Result<Self, MissingEnvVar> {
        Ok(Self {
            anthropic_api_key: require("ANTHROPIC_API_KEY")?,
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
        })
    }
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
