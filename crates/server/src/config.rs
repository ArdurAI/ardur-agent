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

use std::fmt;
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

/// Which memory substrate the server boots behind the `MemoryRuntime` seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryBackend {
    /// The in-process §7.0 Phase 1 store — fast, but lost on restart (default).
    InMemory,
    /// The durable, Qdrant-backed §7.0 Phase 2 store (`ardur-memory-qdrant`),
    /// selected with `ARDUR_MEMORY=qdrant`; requires `QDRANT_URL`.
    Qdrant,
    /// The §7.0c dense+sparse hybrid retriever (`HybridMemoryRetriever`),
    /// selected with `ARDUR_MEMORY=hybrid`. Layers a BM25 lexical index and an
    /// embedding model over the same durable Qdrant store, so — like
    /// [`Qdrant`](Self::Qdrant) — it requires `QDRANT_URL`, and additionally
    /// adds fused recall via [`MemoryRuntime::search`](ardur_memory::MemoryRuntime::search).
    Hybrid,
}

/// The fully-resolved server configuration.
#[derive(Clone)]
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
    /// Address the HTTP listener binds (`ARDUR_BIND_ADDR`, default `127.0.0.1:3000`).
    pub bind_addr: String,
    /// Bearer-token allowlist required for `POST /chat`
    /// (`ARDUR_CHAT_BEARER_TOKENS`, comma-separated). When empty, `/chat`
    /// denies every request with `401` instead of processing the body.
    pub chat_bearer_tokens: Vec<String>,
    /// Bearer-token allowlist required for the runtime-inspection admin API
    /// (`ARDUR_ADMIN_BEARER_TOKENS`, comma-separated). When empty, admin routes
    /// deny every request with `401` (fail-closed).
    pub admin_bearer_tokens: Vec<String>,
    /// Explicit development escape hatch for the embedded permissive Cedar policy
    /// (`ARDUR_DEV_PERMISSIVE_POLICY=true`). Production boots without a configured
    /// policy use a deny-all policy, and a configured-but-missing path is an error.
    pub dev_permissive_policy: bool,
    /// Default model id (`ARDUR_MODEL`, default `claude-opus-4-8`).
    pub model: String,
    /// The per-process cost budget in cents (`ARDUR_COST_BUDGET_CENTS`, default
    /// `10000`). See the note on [`crate::state::AppState`] about why this is
    /// per-process rather than per-session under the Phase-2 cost-gate API.
    pub cost_budget_cents: u64,
    /// Optional path to a Cedar policy file (`ARDUR_CEDAR_POLICY_PATH`). When
    /// set, the path must exist and compile. When unset, production uses a
    /// deny-all policy unless `ARDUR_DEV_PERMISSIVE_POLICY=true` is explicitly set.
    pub cedar_policy_path: Option<PathBuf>,
    /// Override for the Slack Web-API base URL — `None` in production (the
    /// adapter's default), `Some(mock.uri())` in tests.
    pub slack_base_url: Option<String>,
    /// Whether to start the Matrix channel adapter alongside Slack
    /// (`ARDUR_CHANNEL_MATRIX`, default `false`). When `true`, the `MATRIX_*`
    /// credentials are required at config-load (the bin constructs
    /// `MatrixChannel::from_env` at boot); the adapter itself re-reads them.
    pub channel_matrix: bool,
    /// Whether to start the Discord channel adapter alongside Slack
    /// (`ARDUR_CHANNEL_DISCORD`, default `false`). When `true`, the
    /// `DISCORD_BOT_TOKEN` + `DISCORD_APPLICATION_ID` credentials are required at
    /// config-load (the bin constructs `DiscordChannel::from_env` at boot).
    pub channel_discord: bool,
    /// Whether to start the Telegram channel adapter alongside Slack
    /// (`ARDUR_CHANNEL_TELEGRAM`, default `false`). When `true`, the
    /// `TELEGRAM_BOT_TOKEN` credential is required at config-load (the bin
    /// constructs `TelegramChannel::from_env` at boot).
    pub channel_telegram: bool,
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
    /// Directories to load filesystem `SKILL.md` skills from
    /// (`ARDUR_SKILLS_DIRS`, comma-separated). Each is a collection of
    /// `<name>/SKILL.md` skill sub-directories; every discovered skill is
    /// registered as a tool the runtime can invoke (§8.X). Empty when unset.
    pub skills_dirs: Vec<PathBuf>,
    /// Which memory substrate to boot (`ARDUR_MEMORY`, default `in_memory`).
    pub memory_backend: MemoryBackend,
    /// The Qdrant endpoint (`QDRANT_URL`) — required only when the Qdrant memory
    /// backend is selected, mirroring how [`anthropic_api_key`] gates the
    /// Anthropic provider. The rest of the Qdrant config defaults through
    /// `ardur-memory-qdrant`, but URL is lifted here so a missing required
    /// endpoint surfaces at config time rather than at first use.
    ///
    /// [`anthropic_api_key`]: Config::anthropic_api_key
    pub qdrant_url: Option<String>,
    /// Optional Qdrant collection override (`QDRANT_COLLECTION`). Tests can set
    /// this field directly instead of mutating process-global environment.
    pub qdrant_collection: Option<String>,
}

/// A required environment variable was unset or empty.
#[derive(Debug, thiserror::Error)]
#[error("required environment variable `{0}` is unset or empty")]
pub struct MissingEnvVar(pub String);

/// A configuration error: either a missing required variable or an invalid
/// value for one that was present. Returned by [`Config::from_env`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required environment variable was unset or empty.
    #[error(transparent)]
    Missing(#[from] MissingEnvVar),
    /// A present environment variable had a value that could not be parsed
    /// or was semantically invalid.
    #[error("invalid value for environment variable `{var}`: {reason}")]
    Invalid {
        /// The environment variable name.
        var: &'static str,
        /// Why the value is invalid.
        reason: String,
    },
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field(
                "anthropic_api_key",
                &redacted_present(&self.anthropic_api_key),
            )
            .field("slack_bot_token", &redacted_present(&self.slack_bot_token))
            .field(
                "slack_signing_secret",
                &redacted_present(&self.slack_signing_secret),
            )
            .field("slack_app_id", &self.slack_app_id)
            .field("data_dir", &self.data_dir)
            .field("bind_addr", &self.bind_addr)
            .field(
                "chat_bearer_tokens",
                &redacted_count(self.chat_bearer_tokens.len()),
            )
            .field(
                "admin_bearer_tokens",
                &redacted_count(self.admin_bearer_tokens.len()),
            )
            .field("dev_permissive_policy", &self.dev_permissive_policy)
            .field("model", &self.model)
            .field("cost_budget_cents", &self.cost_budget_cents)
            .field("cedar_policy_path", &self.cedar_policy_path)
            .field("slack_base_url", &self.slack_base_url)
            .field("channel_matrix", &self.channel_matrix)
            .field("channel_discord", &self.channel_discord)
            .field("channel_telegram", &self.channel_telegram)
            .field("log_format", &self.log_format)
            .field("mcp_enabled", &self.mcp_enabled)
            .field(
                "mcp_bearer_tokens",
                &redacted_count(self.mcp_bearer_tokens.len()),
            )
            .field("mcp_path_prefix", &self.mcp_path_prefix)
            .field("mcp_remote_servers", &self.mcp_remote_servers)
            .field("skills_dirs", &self.skills_dirs)
            .field("memory_backend", &self.memory_backend)
            .field("qdrant_url", &self.qdrant_url)
            .field("qdrant_collection", &self.qdrant_collection)
            .finish()
    }
}

fn redacted_present(value: &str) -> &'static str {
    if value.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

fn redacted_count(count: usize) -> String {
    format!("<redacted:{count}>")
}

impl Config {
    /// Read the configuration from the process environment.
    ///
    /// `ANTHROPIC_API_KEY` is required only when the selected `ARDUR_PROVIDER`
    /// backend is `anthropic` (the default when unset). For `openrouter`,
    /// `openai-compat`, `ollama`, `codex`, and `claude-cli` it is optional, so a
    /// real boot under those backends does not demand an Anthropic key. An
    /// unrecognized `ARDUR_PROVIDER` is treated as non-Anthropic here (the key
    /// is not required); the selector itself rejects the bad value — with a
    /// message listing the supported ones — when the binary builds the provider.
    ///
    /// `QDRANT_URL` follows the same conditional shape: it is required when
    /// `ARDUR_MEMORY=qdrant` selects the durable Qdrant memory backend, or
    /// `ARDUR_MEMORY=hybrid` selects the §7.0c dense+sparse retriever over that
    /// same store (the default `in_memory` backend needs no Qdrant).
    ///
    /// # Errors
    /// [`MissingEnvVar`] naming the first required variable that is unset or
    /// empty (`SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APP_ID`,
    /// `ANTHROPIC_API_KEY` when the Anthropic backend is selected, and
    /// `QDRANT_URL` when the Qdrant memory backend is selected).
    pub fn from_env() -> Result<Self, ConfigError> {
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
            return Err(ConfigError::Missing(MissingEnvVar(
                "ARDUR_MCP_BEARER_TOKENS".to_string(),
            )));
        }

        // The memory backend selector. `QDRANT_URL` is required when either
        // Qdrant-backed backend is selected — `qdrant` (durable) or `hybrid`
        // (durable + dense/sparse recall, §7.0c) — the same conditional shape as
        // the Anthropic key above. Under the default `in_memory` backend it is
        // optional (and ignored).
        let memory_backend = parse_memory_backend(optional("ARDUR_MEMORY").as_deref())?;
        let qdrant_url = if matches!(
            memory_backend,
            MemoryBackend::Qdrant | MemoryBackend::Hybrid
        ) {
            Some(require("QDRANT_URL")?)
        } else {
            optional("QDRANT_URL")
        };

        // The Matrix adapter is opt-in; when enabled, its credentials are
        // required at config-load (mirroring the Anthropic-key conditional) so a
        // misconfigured boot fails here rather than mid-startup.
        let channel_matrix = optional("ARDUR_CHANNEL_MATRIX")
            .as_deref()
            .is_some_and(is_truthy);
        if channel_matrix {
            require("MATRIX_HOMESERVER_URL")?;
            require("MATRIX_USER_ID")?;
            require("MATRIX_ACCESS_TOKEN")?;
        }

        // The Discord + Telegram adapters follow the same opt-in shape: when
        // enabled, their credentials are required at config-load.
        let channel_discord = optional("ARDUR_CHANNEL_DISCORD")
            .as_deref()
            .is_some_and(is_truthy);
        if channel_discord {
            require("DISCORD_BOT_TOKEN")?;
            require("DISCORD_APPLICATION_ID")?;
        }
        let channel_telegram = optional("ARDUR_CHANNEL_TELEGRAM")
            .as_deref()
            .is_some_and(is_truthy);
        if channel_telegram {
            require("TELEGRAM_BOT_TOKEN")?;
        }

        Ok(Self {
            anthropic_api_key,
            slack_bot_token: require("SLACK_BOT_TOKEN")?,
            slack_signing_secret: require("SLACK_SIGNING_SECRET")?,
            slack_app_id: require("SLACK_APP_ID")?,
            data_dir: optional("ARDUR_DATA_DIR")
                .map_or_else(|| PathBuf::from("./data"), PathBuf::from),
            bind_addr: optional("ARDUR_BIND_ADDR").unwrap_or_else(|| "127.0.0.1:3000".to_string()),
            chat_bearer_tokens: parse_csv(optional("ARDUR_CHAT_BEARER_TOKENS").as_deref()),
            admin_bearer_tokens: parse_csv(optional("ARDUR_ADMIN_BEARER_TOKENS").as_deref()),
            dev_permissive_policy: optional("ARDUR_DEV_PERMISSIVE_POLICY")
                .as_deref()
                .is_some_and(is_truthy),
            model: optional("ARDUR_MODEL").unwrap_or_else(|| "claude-opus-4-8".to_string()),
            cost_budget_cents: {
                match optional("ARDUR_COST_BUDGET_CENTS") {
                    None => 10_000,
                    Some(raw) => raw.parse::<u64>().map_err(|e| ConfigError::Invalid {
                        var: "ARDUR_COST_BUDGET_CENTS",
                        reason: format!("`{raw}` is not a valid u64: {e}"),
                    })?,
                }
            },
            cedar_policy_path: optional("ARDUR_CEDAR_POLICY_PATH").map(PathBuf::from),
            slack_base_url: None,
            channel_matrix,
            channel_discord,
            channel_telegram,
            log_format: match optional("ARDUR_LOG_FORMAT").as_deref() {
                None | Some("") | Some("text") => LogFormat::Text,
                Some("json") => LogFormat::Json,
                Some(other) => {
                    return Err(ConfigError::Invalid {
                        var: "ARDUR_LOG_FORMAT",
                        reason: format!("unrecognized value `{other}` (expected: text, json)"),
                    });
                }
            },
            mcp_enabled,
            mcp_bearer_tokens,
            mcp_path_prefix: optional("ARDUR_MCP_PATH_PREFIX")
                .unwrap_or_else(|| "/mcp".to_string()),
            mcp_remote_servers: parse_remote_servers(
                optional("ARDUR_MCP_REMOTE_SERVERS").as_deref(),
            ),
            skills_dirs: parse_csv(optional("ARDUR_SKILLS_DIRS").as_deref())
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            memory_backend,
            qdrant_url,
            qdrant_collection: optional("QDRANT_COLLECTION"),
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

fn parse_memory_backend(value: Option<&str>) -> Result<MemoryBackend, ConfigError> {
    match value {
        None | Some("") | Some("in_memory") => Ok(MemoryBackend::InMemory),
        Some("qdrant") => Ok(MemoryBackend::Qdrant),
        Some("hybrid") => Ok(MemoryBackend::Hybrid),
        Some(other) => Err(ConfigError::Invalid {
            var: "ARDUR_MEMORY",
            reason: format!("unrecognized value `{other}` (expected: in_memory, qdrant, hybrid)"),
        }),
    }
}

/// Whether a string is a truthy flag value: `true`/`1`/`yes`/`on`
/// (case-insensitive). Anything else (including unset) is false.
fn is_truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
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

#[cfg(test)]
mod tests {
    use super::{MemoryBackend, parse_memory_backend};

    #[test]
    fn parses_explicit_in_memory_literal() {
        assert_eq!(
            parse_memory_backend(Some("in_memory")).expect("in_memory parses"),
            MemoryBackend::InMemory
        );
    }
}
