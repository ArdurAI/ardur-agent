//! ardur-provider-opencode — the [OpenCode] CLI backend, driven as a one-shot
//! subprocess.
//!
//! The HTTP backends (Anthropic, OpenRouter, openai-compat) authenticate with
//! an API key and POST to a REST endpoint, billing per token. This backend is
//! different: it spawns the locally-installed `opencode` binary in its
//! one-shot run mode and reads a structured JSONL event stream from stdout.
//! Authentication is inherited from OpenCode's own configured providers
//! (`opencode auth login`), so there is no API key in this crate's config and
//! every completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport it implements the same [`Provider`]
//! trait as the HTTP backends, so the runtime dispatches to it through the
//! generic [`ProviderRegistry`] with no catalog change — which is the point:
//! a wrapped OpenCode runs behind the full fused pipeline (cap-token verify,
//! Cedar authorization, cost gate, signed receipt, durable journal).
//!
//! # The CLI protocol (verified against sst/opencode run.ts, v1.0.165 and master)
//!
//! One Ardur turn maps onto one child process:
//!
//! ```text
//! opencode run --format json
//! ```
//!
//! The prompt is written to stdin: with no message arguments, `run` reads the
//! piped stdin text as the prompt. Every stdout line is one JSON event object
//! shaped `{"type":…, "timestamp":…, "sessionID":…, …}`. The turn's answer is
//! the last `{"type":"text", "part":{"text":…}}` event; usage rides on
//! `{"type":"step_finish", "part":{"tokens":{"input":…, "output":…, …}}}`
//! events (one per model call, so counts are summed); failures arrive as
//! `{"type":"error", "error":{…}}` events. The process exits non-zero when any
//! error accumulated. Diagnostics stay on stderr.
//!
//! Child tools are denied by default through OpenCode's inline config: the
//! child is spawned with `OPENCODE_CONFIG_CONTENT={"permission":"deny",…}`.
//! Inline config outranks global, custom, and project config (only admin
//! *managed* config beats it), and explicit `deny` rules are enforced even in
//! `--auto` mode — which this crate never passes. That is the fail-closed
//! default for a *completion* backend.
//!
//! # Auth & install requirements
//!
//! The host must have the `opencode` binary on `PATH` (or
//! [`OpenCodeConfig::binary`] pointed at it) and a usable model provider
//! configured inside OpenCode. A missing binary surfaces as
//! [`ProviderError::Upstream`] ("OpenCode CLI not installed …") carrying the
//! install one-liner ([`INSTALL_HINT`]) so the failure is actionable without
//! opening the docs; a missing or expired login surfaces as
//! [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! | Failure | [`ProviderError`] |
//! |----------------------------------|----------------------------------|
//! | binary not found on `PATH` / via `OPENCODE_BINARY` | [`Upstream`](ProviderError::Upstream) ("OpenCode CLI not installed …" + install one-liner) |
//! | no usable model / not logged in | [`Unauthorized`](ProviderError::Unauthorized) |
//! | turn exceeded `request_timeout` | [`NetworkFailure`](ProviderError::NetworkFailure) |
//! | non-zero exit / in-band `error` event | [`Upstream`](ProviderError::Upstream) (stderr / error, secret-redacted) |
//! | turn finished with empty text | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in this phase
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole
//!   turn is awaited before a response is returned. The event feed is already
//!   line-oriented, so a later streaming path is a change to this crate only.
//! - **Tool-call parsing** — OpenCode orchestrates its own tools inside its
//!   own process. This layer runs it with tools denied by default
//!   ([`OpenCodeConfig::allow_child_tools`]) and surfaces only the final
//!   assistant text, never a [`FinishReason::ToolUse`]. Governing a child's
//!   *own* tool steps is delegation work, not provider work.
//! - **Selector registration** — selected at boot via `ARDUR_PROVIDER=opencode`
//!   (alias `opencode-agent`) in `provider-selector::from_env` →
//!   [`OpenCodeProvider::from_env`].
//!
//! # Attribution
//!
//! OpenCode is MIT-licensed (SST). See this crate's `README.md`.
//!
//! [OpenCode]: https://github.com/sst/opencode
//! [`CostTuple`]: ardur_runtime::CostTuple
//! [`ProviderRegistry`]: ardur_provider_runtime::ProviderRegistry
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, Role};
use ardur_session_journals::{default_secret_patterns, redact_text};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "opencode";
/// The binary [`OpenCodeConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "opencode";
/// Default per-turn timeout. An OpenCode turn can involve several model
/// round-trips, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default [`OpenCodeConfig::max_tokens_floor`]. A request asking for fewer
/// output tokens than this is refused, because OpenCode cannot enforce a
/// per-completion ceiling and silently overshooting the caller's authorized
/// budget is worse than failing the request.
const DEFAULT_MAX_TOKENS_FLOOR: u32 = 4_096;
/// Upper bound on retained event lines, so a chatty turn cannot grow the audit
/// body without limit.
const MAX_RETAINED_EVENTS: usize = 2_000;
/// Upper bound on captured stderr bytes, so a child spewing diagnostics cannot
/// grow memory without limit.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Upper bound on captured stdout bytes while draining the child.
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum spawn retries when Linux returns `ETXTBSY` ("Text file busy"). See
/// [`spawn_opencode`] for why the retry exists.
const SPAWN_ETXTBSY_RETRIES: u32 = 6;

/// Inline config handed to the child through `OPENCODE_CONFIG_CONTENT` when
/// child tools are denied (the default). `permission: "deny"` blocks every
/// tool action — explicit deny rules are enforced even under `--auto`, which
/// this crate never passes — and `share: "disabled"` keeps the turn's
/// transcript off any share URL. Inline config outranks global, custom, and
/// project config; only admin-managed config can override it.
const DENY_ALL_INLINE_CONFIG: &str = r#"{"permission":"deny","share":"disabled"}"#;
/// Inline config used when child tools are explicitly allowed: only session
/// sharing is pinned off, so the operator's own permission configuration
/// governs the tool surface exactly as it would for a direct `opencode run`.
const SHARE_DISABLED_INLINE_CONFIG: &str = r#"{"share":"disabled"}"#;

/// Env var [`OpenCodeConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "OPENCODE_BINARY";
/// Install guidance carried in the missing-binary
/// [`ProviderError::Upstream`] message, so an operator — or a smoke run —
/// can act on a "not installed" failure without opening the docs. Verified
/// against sst/opencode: the curl one-liner is the official installer; npm
/// and Homebrew are the packaged alternatives.
pub const INSTALL_HINT: &str = "curl -fsSL https://opencode.ai/install | bash \
     (or `npm i -g opencode-ai` / `brew install sst/tap/opencode`)";
/// Env var [`OpenCodeConfig::from_env`] reads the default model from. OpenCode
/// models are named in `provider/model` form (its `--model` flag).
pub const DEFAULT_MODEL_ENV: &str = "OPENCODE_DEFAULT_MODEL";
/// Env var [`OpenCodeConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "OPENCODE_WORKING_DIR";
/// Env var [`OpenCodeConfig::from_env`] reads the per-turn timeout from.
pub const TIMEOUT_SECS_ENV: &str = "OPENCODE_TIMEOUT_SECS";
/// Env var [`OpenCodeConfig::from_env`] reads the max-tokens floor from.
pub const MAX_TOKENS_FLOOR_ENV: &str = "OPENCODE_MAX_TOKENS_FLOOR";

/// How this backend locates and runs the `opencode` binary.
#[derive(Clone, Debug)]
pub struct OpenCodeConfig {
    /// The binary to spawn. Resolved through `PATH` when relative.
    pub binary: PathBuf,
    /// Model used when a [`CompletionRequest`] does not name one, in
    /// OpenCode's `provider/model` form. `None` leaves OpenCode's configured
    /// default in place.
    pub default_model: Option<String>,
    /// Working directory for the child. `None` inherits ours.
    pub working_directory: Option<PathBuf>,
    /// Wall-clock ceiling for one turn.
    pub request_timeout: Duration,
    /// The smallest per-request `max_tokens` this backend will accept.
    ///
    /// OpenCode has no per-completion output cap, so any ceiling below this
    /// is refused rather than silently ignored (see [`Provider::complete`]).
    /// Above it, enforcement is knowingly delegated to OpenCode's own limits.
    /// Set to `0` to accept every ceiling and delegate unconditionally.
    pub max_tokens_floor: u32,
    /// Whether the child may run its own tools. `false` (the default) spawns
    /// the child with `OPENCODE_CONFIG_CONTENT={"permission":"deny",…}` (see
    /// [`DENY_ALL_INLINE_CONFIG`]): as a *completion* backend we want the
    /// model's answer, not an agent editing the filesystem outside Ardur's
    /// grant ledger. Turning this on hands the child the operator's own
    /// configured permissions — the fused pipeline cannot see steps taken
    /// inside another process.
    pub allow_child_tools: bool,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            default_model: None,
            working_directory: None,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_tokens_floor: DEFAULT_MAX_TOKENS_FLOOR,
            allow_child_tools: false,
        }
    }
}

impl OpenCodeConfig {
    /// Build a config from the environment, falling back to [`Default`] for
    /// any unset variable.
    ///
    /// An unparseable or zero [`TIMEOUT_SECS_ENV`] keeps the default rather
    /// than producing a turn that can never finish.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(binary) = non_empty_env(BINARY_ENV) {
            config.binary = PathBuf::from(binary);
        }
        config.default_model = non_empty_env(DEFAULT_MODEL_ENV);
        config.working_directory = non_empty_env(WORKING_DIR_ENV).map(PathBuf::from);
        if let Some(secs) = non_empty_env(TIMEOUT_SECS_ENV)
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
        {
            config.request_timeout = Duration::from_secs(secs);
        }
        // `0` is meaningful here (delegate unconditionally), so unlike the
        // timeout this accepts zero; only an unparseable value keeps the default.
        if let Some(floor) =
            non_empty_env(MAX_TOKENS_FLOOR_ENV).and_then(|raw| raw.parse::<u32>().ok())
        {
            config.max_tokens_floor = floor;
        }
        config
    }
}

/// Read an environment variable, treating empty/whitespace as unset.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

/// The OpenCode CLI provider.
#[derive(Debug)]
pub struct OpenCodeProvider {
    config: OpenCodeConfig,
    rate_card: RateCard,
}

impl OpenCodeProvider {
    /// Build a provider from an explicit config.
    #[must_use]
    pub fn new(config: OpenCodeConfig) -> Self {
        Self {
            config,
            rate_card: opencode_delegated_rate_card(),
        }
    }

    /// Build a provider from [`OpenCodeConfig::from_env`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(OpenCodeConfig::from_env())
    }

    /// The model this turn should run against: the request's, else the
    /// config's, else OpenCode's own default.
    fn chosen_model(&self, requested: &ModelId) -> Option<String> {
        let requested = requested.0.trim();
        if !requested.is_empty() {
            return Some(requested.to_string());
        }
        self.config.default_model.clone()
    }
}

/// What one JSONL turn produced.
struct TurnOutcome {
    /// Final assistant text (the last `text` event's part).
    content: String,
    /// Token counts summed across the turn's `step_finish` events.
    usage: Usage,
    /// Retained event lines, as the audit body.
    events: Vec<serde_json::Value>,
}

#[async_trait]
impl Provider for OpenCodeProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let transcript = build_transcript(&req.messages);
        if transcript.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "opencode requires a non-empty prompt".into(),
            ));
        }
        // The caller's output-token ceiling must not be silently discarded.
        // `opencode run` exposes no per-request output cap, so a ceiling this
        // backend cannot enforce is refused rather than ignored: the fused
        // runtime would otherwise authorize N tokens, be billed for more, and
        // still see a clean `FinishReason::Stop`.
        //
        // `max_tokens_floor` is the ceiling at or above which the operator
        // accepts that enforcement is delegated to OpenCode's own limits.
        // Pass 0 to acknowledge the ceiling is delegated unconditionally.
        if req.max_tokens > 0 && req.max_tokens < self.config.max_tokens_floor {
            return Err(ProviderError::InvalidRequest(format!(
                "opencode run cannot enforce a per-request output ceiling of {} tokens \
                 (it has no per-completion cap); raise max_tokens to at least {} or use \
                 a provider that enforces it",
                req.max_tokens, self.config.max_tokens_floor
            )));
        }

        let model = self.chosen_model(&req.model);
        let outcome = tokio::time::timeout(
            self.config.request_timeout,
            self.run_turn(&transcript, model.as_deref()),
        )
        .await
        .map_err(|_| {
            ProviderError::NetworkFailure(format!(
                "opencode turn exceeded {:?}",
                self.config.request_timeout
            ))
        })??;

        let cost = CostTuple {
            tokens_in: u64::from(outcome.usage.tokens_in),
            tokens_out: u64::from(outcome.usage.tokens_out),
            // Delegated billing: OpenCode's own configured provider pays for
            // the call, so there is no per-call monetary cost to attribute here.
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
        };

        let mut raw = serde_json::Map::new();
        if let Some(model) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            raw.insert("model".into(), serde_json::Value::String(model.to_string()));
        }
        raw.insert("events".into(), serde_json::Value::Array(outcome.events));

        Ok(CompletionResponse {
            content: outcome.content,
            finish_reason: FinishReason::Stop,
            usage: outcome.usage,
            cost,
            // Object shape so `response_model_attr` can read `.get("model")`.
            raw_provider_response: Some(serde_json::Value::Object(raw)),
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // The JSONL event feed is already line-oriented; wiring it to
        // `StreamEvent`s is a later, crate-local change.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

impl OpenCodeProvider {
    /// Spawn the child, feed the prompt on stdin, and parse the JSONL event
    /// stream from stdout.
    async fn run_turn(
        &self,
        transcript: &str,
        model: Option<&str>,
    ) -> Result<TurnOutcome, ProviderError> {
        let mut cmd = tokio::process::Command::new(&self.config.binary);
        cmd.arg("run")
            // Structured JSONL for programmatic consumption.
            .arg("--format")
            .arg("json");
        if let Some(model) = model {
            cmd.arg("--model").arg(model);
        }
        // The child inherits our environment; pin its inline config on top.
        // Tools denied (the default) blocks every permission and session
        // sharing; tools allowed still pins sharing off so a turn's transcript
        // cannot leak to a share URL through the operator's global config.
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            if self.config.allow_child_tools {
                SHARE_DISABLED_INLINE_CONFIG
            } else {
                DENY_ALL_INLINE_CONFIG
            },
        );
        if let Some(cwd) = &self.config.working_directory {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout this future is dropped, which drops the child;
            // kill_on_drop ensures the opencode process dies with it rather
            // than surviving as an orphan holding a model session open.
            .kill_on_drop(true);

        let mut child = spawn_opencode(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing_binary_error(&self.config.binary, &e)
            } else {
                ProviderError::Upstream(format!("failed to spawn opencode: {e}"))
            }
        })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Upstream("opencode child stdin was not captured".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Upstream("opencode child stdout was not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ProviderError::Upstream("opencode child stderr was not captured".into())
        })?;

        // Drain stderr concurrently into a capped sink. A child that fills the
        // stderr pipe while we block reading stdout would deadlock, and an
        // unbounded sink would let a noisy child grow memory without limit.
        let stderr_task = tokio::spawn(async move {
            let mut reader = stderr;
            let mut captured: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if captured.len() < MAX_STDERR_BYTES {
                            let room = MAX_STDERR_BYTES - captured.len();
                            captured.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&captured).into_owned()
        });

        match stdin.write_all(transcript.as_bytes()).await {
            Ok(()) => {}
            // A child that exits before draining stdin closes the read end, so
            // the write races to a BrokenPipe. That is not our failure to
            // report — the exit status and captured stderr are the source of
            // truth.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                return Err(ProviderError::Upstream(format!(
                    "writing prompt to opencode stdin: {e}"
                )));
            }
        }
        drop(stdin);

        let mut stdout_reader = stdout;
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout_reader.read(&mut chunk).await {
                Ok(0) => break,
                Err(e) => {
                    return Err(ProviderError::Upstream(format!(
                        "reading opencode stdout: {e}"
                    )));
                }
                Ok(n) => {
                    if stdout_buf.len() >= MAX_STDOUT_BYTES {
                        return Err(ProviderError::Upstream(format!(
                            "opencode stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                    let room = MAX_STDOUT_BYTES - stdout_buf.len();
                    stdout_buf.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room {
                        return Err(ProviderError::Upstream(format!(
                            "opencode stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                }
            }
        }
        let stdout_text = String::from_utf8_lossy(&stdout_buf).into_owned();

        let status = child
            .wait()
            .await
            .map_err(|e| ProviderError::Upstream(format!("waiting on opencode subprocess: {e}")))?;
        let stderr_text = stderr_task.await.unwrap_or_default();

        let parsed = parse_events(&stdout_text);

        if !status.success() {
            let detail = if !stderr_text.trim().is_empty() {
                stderr_text.trim().to_string()
            } else if !parsed.error.is_empty() {
                parsed.error.clone()
            } else {
                "opencode exited with a non-zero status".to_string()
            };
            // Classify stderr and in-band error events independently so an
            // unrelated stderr warning cannot mask an auth/rate-limit result.
            if looks_like_auth_error(&detail)
                || looks_like_auth_error(&stderr_text)
                || looks_like_auth_error(&parsed.error)
            {
                return Err(ProviderError::Unauthorized);
            }
            if looks_like_rate_limit(&detail)
                || looks_like_rate_limit(&stderr_text)
                || looks_like_rate_limit(&parsed.error)
            {
                return Err(ProviderError::RateLimited { retry_after_ms: 0 });
            }
            let code = status.code();
            return Err(classify_child_failure(&detail, code.map(i64::from)));
        }

        if !parsed.error.is_empty() {
            if looks_like_auth_error(&parsed.error) || looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            if looks_like_rate_limit(&parsed.error) || looks_like_rate_limit(&stderr_text) {
                return Err(ProviderError::RateLimited { retry_after_ms: 0 });
            }
            return Err(classify_child_failure(&parsed.error, None));
        }

        if parsed.content.trim().is_empty() {
            if looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            return Err(ProviderError::Upstream(
                "opencode turn produced no assistant text".into(),
            ));
        }

        Ok(TurnOutcome {
            content: parsed.content,
            usage: parsed.usage,
            events: parsed.events,
        })
    }
}

/// Fields pulled out of an `opencode run --format json` stdout stream.
struct ParsedOutput {
    /// The last `text` event's `part.text` — the turn's final assistant text.
    content: String,
    /// Token counts summed across `step_finish` events.
    usage: Usage,
    /// Messages from `error` events, joined with newlines. Empty when none.
    error: String,
    /// Retained event lines, as the audit body.
    events: Vec<serde_json::Value>,
}

/// Parse the JSONL event stream OpenCode writes to stdout. Non-JSON lines are
/// skipped rather than failing the whole parse (stray banner noise must not
/// abort an otherwise valid turn).
fn parse_events(stdout: &str) -> ParsedOutput {
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut error = String::new();
    let mut events: Vec<serde_json::Value> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                // Emitted when a text part completes (part.time.end set), so
                // each event is a finished assistant chunk; the last one is
                // the turn's answer.
                if let Some(text) = value
                    .get("part")
                    .and_then(|part| part.get("text"))
                    .and_then(serde_json::Value::as_str)
                {
                    content = text.to_string();
                }
            }
            Some("step_finish") => {
                // One per model call, so counts are summed: a turn whose child
                // ran tools can drive several calls, and keeping only the last
                // would under-report the run in the signed receipt.
                if let Some(tokens) = value.get("part").and_then(|part| part.get("tokens")) {
                    let input = tokens
                        .get("input")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let output = tokens
                        .get("output")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    usage.tokens_in = usage
                        .tokens_in
                        .saturating_add(u32::try_from(input).unwrap_or(u32::MAX));
                    usage.tokens_out = usage
                        .tokens_out
                        .saturating_add(u32::try_from(output).unwrap_or(u32::MAX));
                }
            }
            Some("error") => {
                let message = extract_error_message(&value);
                if !message.is_empty() {
                    if !error.is_empty() {
                        error.push('\n');
                    }
                    error.push_str(&message);
                }
            }
            _ => {}
        }
        if events.len() < MAX_RETAINED_EVENTS {
            events.push(value);
        }
    }

    ParsedOutput {
        content,
        usage,
        error,
        events,
    }
}

/// Pull a human-readable message out of an `error` event's `error` payload.
///
/// The child sends `{"name":…, "data":{"message":…}}` objects (a bare string
/// is also tolerated). The message is preferred over the name so diagnostics
/// carry the child's own explanation.
fn extract_error_message(event: &serde_json::Value) -> String {
    let Some(error) = event.get("error") else {
        return String::new();
    };
    if let Some(text) = error.as_str() {
        return text.to_string();
    }
    for key in ["data", "message"] {
        if let Some(message) = error
            .get(key)
            .and_then(|data| {
                if key == "data" {
                    data.get("message")
                } else {
                    Some(data)
                }
            })
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return message.to_string();
        }
    }
    error
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Errno for Linux's `ETXTBSY` ("Text file busy"). macOS never returns it.
const ETXTBSY: i32 = 26;

/// Whether a spawn error is `ETXTBSY` ("Text file busy").
fn is_etxtbsy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy || e.raw_os_error() == Some(ETXTBSY)
}

/// Spawn the opencode subprocess, retrying briefly on `ETXTBSY` ("Text file busy").
///
/// Mirrors the provider-codex / provider-claude-cli / provider-hermes spawn
/// retry: parallel tests that write an executable shim then exec it can race
/// an inherited writable fd across `fork`→`execve` on Linux.
async fn spawn_opencode(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    let mut attempt: u32 = 0;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if is_etxtbsy(&e) && attempt < SPAWN_ETXTBSY_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(2 * u64::from(attempt))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Build the typed error for a missing `opencode` binary.
///
/// This is the smoke gap from #571: "not installed" must be distinguishable
/// from auth, rate-limit, and general child failures, and the message must
/// carry the install one-liner ([`INSTALL_HINT`]) so the operator can act on
/// it directly. The attempted binary is named so an `OPENCODE_BINARY`
/// override pointing at a missing file is distinguishable from a plain
/// not-on-`PATH`.
fn missing_binary_error(binary: &std::path::Path, source: &std::io::Error) -> ProviderError {
    ProviderError::Upstream(format!(
        "OpenCode CLI not installed (binary {binary:?} not found on PATH or via \
         {BINARY_ENV}): {source}. Install: {INSTALL_HINT}, \
         then run `opencode auth login` once"
    ))
}

/// Strip secret-shaped substrings from a child diagnostic before it enters a
/// [`ProviderError`]. OpenCode (or a shim) may echo an API key in stderr / an
/// `error` event; AGENTS.md forbids surfacing those values.
fn redact_child_diagnostic(detail: &str) -> String {
    redact_text(detail, &default_secret_patterns())
}

/// Whether a diagnostic describes a rate-limit / quota condition.
fn looks_like_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["rate limit", "rate-limit", "quota", "too many requests"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Classify a failed child into the typed error taxonomy, redacting any
/// secret-shaped material from the retained diagnostic.
fn classify_child_failure(detail: &str, exit_code: Option<i64>) -> ProviderError {
    let redacted = redact_child_diagnostic(detail);
    if looks_like_auth_error(detail) {
        return ProviderError::Unauthorized;
    }
    if looks_like_rate_limit(detail) {
        return ProviderError::RateLimited { retry_after_ms: 0 };
    }
    match exit_code {
        Some(code) => {
            ProviderError::Upstream(format!("opencode exited with status {code}: {redacted}"))
        }
        None => ProviderError::Upstream(format!("opencode reported an error: {redacted}")),
    }
}

/// Whether a child diagnostic describes a *credential* failure rather than a
/// general failure.
///
/// The phrases are deliberately specific. Rate-limit / quota diagnostics that
/// merely mention a key must not be reclassified as
/// [`ProviderError::Unauthorized`].
fn looks_like_auth_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();

    const RETRYABLE: [&str; 4] = ["rate limit", "rate-limit", "quota", "too many requests"];
    if RETRYABLE.iter().any(|needle| lowered.contains(needle)) {
        return false;
    }

    const CREDENTIAL_FAILURES: [&str; 15] = [
        "not logged in",
        "no api key",
        "missing api key",
        "invalid api key",
        "incorrect api key",
        "api key expired",
        "revoked",
        "unauthorized",
        "unauthenticated",
        "authentication failed",
        "credentials",
        "auth login",
        "providerautherror",
        "provider auth",
        "no provider",
    ];
    CREDENTIAL_FAILURES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Flatten a chat transcript into the single prompt string OpenCode reads on
/// stdin.
fn build_transcript(messages: &[ardur_runtime::ChatMessage]) -> String {
    let mut systems: Vec<&str> = Vec::new();
    let mut dialogue: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => systems.push(m.content.as_str()),
            Role::User => dialogue.push(format!("User: {}", m.content)),
            Role::Assistant => dialogue.push(format!("Assistant: {}", m.content)),
            Role::Tool => dialogue.push(format!("Tool result: {}", m.content)),
        }
    }
    let mut out = String::new();
    if !systems.is_empty() {
        out.push_str(&systems.join("\n\n"));
        if !dialogue.is_empty() {
            out.push_str("\n\n");
        }
    }
    out.push_str(&dialogue.join("\n\n"));
    out
}

/// A zeroed rate card. OpenCode turns are paid by OpenCode's own configured
/// provider, not metered here, so every completion is priced at zero cents —
/// the card exists only to satisfy [`Provider::rate_card`].
fn opencode_delegated_rate_card() -> RateCard {
    RateCard {
        version_id: "opencode-delegated-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_runtime::ChatMessage;

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    #[test]
    fn missing_binary_is_typed_upstream_with_install_guidance() {
        // #571: a missing opencode binary must be a typed Upstream (no
        // panic, no generic error), distinguishable from auth / rate-limit /
        // general child failures, and the message must carry the install
        // one-liner so the operator can act on it without opening the docs.
        let err = missing_binary_error(
            std::path::Path::new("opencode"),
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        match err {
            ProviderError::Upstream(msg) => {
                assert!(msg.contains("not installed"), "install diagnostic: {msg}");
                assert!(
                    msg.contains("curl -fsSL https://opencode.ai/install | bash"),
                    "install one-liner must ride in the message: {msg}"
                );
                assert!(
                    msg.contains("opencode auth login"),
                    "auth step must be named: {msg}"
                );
                assert!(
                    msg.contains("\"opencode\""),
                    "attempted binary must be named: {msg}"
                );
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_message_names_an_explicit_override_path() {
        // OPENCODE_BINARY pointed at a missing file: the message names that
        // path, so a misconfigured override is distinguishable from a plain
        // not-on-PATH.
        let err = missing_binary_error(
            std::path::Path::new("/opt/opencode/bin/opencode"),
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        match err {
            ProviderError::Upstream(msg) => {
                assert!(msg.contains("not installed"), "install diagnostic: {msg}");
                assert!(
                    msg.contains("/opt/opencode/bin/opencode"),
                    "override path must be named: {msg}"
                );
                assert!(
                    msg.contains(BINARY_ENV),
                    "override env var must be named: {msg}"
                );
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn transcript_puts_system_text_first_then_labelled_dialogue() {
        let rendered = build_transcript(&[
            msg(Role::User, "hello"),
            msg(Role::System, "be terse"),
            msg(Role::Assistant, "hi"),
        ]);
        assert_eq!(rendered, "be terse\n\nUser: hello\n\nAssistant: hi");
    }

    #[test]
    fn transcript_renders_tool_results_rather_than_dropping_them() {
        let rendered = build_transcript(&[msg(Role::Tool, "exit=0")]);
        assert_eq!(rendered, "Tool result: exit=0");
    }

    #[test]
    fn parse_extracts_text_tokens_and_retains_events() {
        let stdout = concat!(
            "{\"type\":\"step_start\",\"timestamp\":1,\"sessionID\":\"s1\",\"part\":{\"type\":\"step-start\"}}\n",
            "{\"type\":\"text\",\"timestamp\":2,\"sessionID\":\"s1\",\"part\":{\"type\":\"text\",\"text\":\"hello\",\"time\":{\"start\":1,\"end\":2}}}\n",
            "{\"type\":\"step_finish\",\"timestamp\":3,\"sessionID\":\"s1\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\",\"cost\":0,\"tokens\":{\"input\":11,\"output\":2,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "hello");
        assert_eq!(parsed.usage.tokens_in, 11);
        assert_eq!(parsed.usage.tokens_out, 2);
        assert!(parsed.error.is_empty());
        assert_eq!(parsed.events.len(), 3);
    }

    #[test]
    fn parse_last_text_part_wins() {
        // With child tools allowed a turn can emit several text parts; the
        // final one is the turn's answer.
        let stdout = concat!(
            "{\"type\":\"text\",\"sessionID\":\"s1\",\"part\":{\"type\":\"text\",\"text\":\"working on it\"}}\n",
            "{\"type\":\"tool_use\",\"sessionID\":\"s1\",\"part\":{\"type\":\"tool\"}}\n",
            "{\"type\":\"text\",\"sessionID\":\"s1\",\"part\":{\"type\":\"text\",\"text\":\"final answer\"}}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "final answer");
    }

    #[test]
    fn parse_sums_usage_across_step_finish_events() {
        let stdout = concat!(
            "{\"type\":\"step_finish\",\"sessionID\":\"s1\",\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":10,\"output\":3,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}}\n",
            "{\"type\":\"step_finish\",\"sessionID\":\"s1\",\"part\":{\"type\":\"step-finish\",\"tokens\":{\"input\":7,\"output\":5,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.usage.tokens_in, 17, "input tokens must accumulate");
        assert_eq!(parsed.usage.tokens_out, 8, "output tokens must accumulate");
    }

    #[test]
    fn parse_skips_non_json_noise() {
        let stdout = concat!(
            "warming up\n",
            "{\"type\":\"text\",\"sessionID\":\"s1\",\"part\":{\"type\":\"text\",\"text\":\"ok\"}}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "ok");
    }

    #[test]
    fn parse_captures_error_event_messages() {
        let stdout = concat!(
            "{\"type\":\"error\",\"sessionID\":\"s1\",\"error\":{\"name\":\"ProviderAuthError\",\"data\":{\"message\":\"not logged in\"}}}\n",
            "{\"type\":\"error\",\"sessionID\":\"s1\",\"error\":{\"name\":\"UnknownError\",\"data\":{\"message\":\"boom\"}}}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.error, "not logged in\nboom");
    }

    #[test]
    fn error_message_falls_back_to_the_error_name() {
        let event = serde_json::json!({"error": {"name": "ProviderAuthError"}});
        assert_eq!(extract_error_message(&event), "ProviderAuthError");
        let bare = serde_json::json!({"error": "plain failure"});
        assert_eq!(extract_error_message(&bare), "plain failure");
    }

    #[test]
    fn auth_errors_are_distinguished_from_general_failures() {
        assert!(looks_like_auth_error(
            "Error: not logged in; run opencode auth login"
        ));
        assert!(looks_like_auth_error(
            "ProviderAuthError: missing credentials"
        ));
        assert!(looks_like_auth_error("API key expired"));
        assert!(!looks_like_auth_error("model produced an internal error"));
    }

    #[test]
    fn rate_limits_naming_a_key_are_not_credential_failures() {
        for retryable in [
            "rate limit exceeded for API key sk-abc",
            "Rate-limit hit; retry later (api key quota)",
            "quota exhausted for this api key",
            "429 too many requests",
        ] {
            assert!(
                !looks_like_auth_error(retryable),
                "{retryable:?} must not be classified as a credential failure"
            );
        }
        for credential in [
            "invalid api key",
            "missing API key",
            "api key expired",
            "authentication failed",
            "401 unauthorized",
        ] {
            assert!(
                looks_like_auth_error(credential),
                "{credential:?} should be a credential failure"
            );
        }
    }

    #[test]
    fn child_diagnostics_are_redacted_before_entering_upstream_errors() {
        // A long fake key of the shape the redaction set masks (\bsk-[a-z0-9_-]{16,}).
        let raw = "rate limit exceeded for API key sk-abcdefghijklmnopqrstuvwxyz012345";
        let redacted = redact_child_diagnostic(raw);
        assert!(
            !redacted.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "secret must not survive redaction: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED>"),
            "expected redaction marker, got: {redacted}"
        );
        assert!(
            redacted.contains("rate limit exceeded"),
            "non-secret text must remain: {redacted}"
        );
    }

    #[test]
    fn request_model_wins_over_config_default() {
        let provider = OpenCodeProvider::new(OpenCodeConfig {
            default_model: Some("provider/config-model".into()),
            ..OpenCodeConfig::default()
        });
        assert_eq!(
            provider.chosen_model(&ModelId("provider/request-model".into())),
            Some("provider/request-model".to_string())
        );
        assert_eq!(
            provider.chosen_model(&ModelId(String::new())),
            Some("provider/config-model".to_string())
        );
    }

    #[test]
    fn provider_id_is_opencode_and_not_streaming() {
        let provider = OpenCodeProvider::new(OpenCodeConfig::default());
        assert_eq!(provider.id(), ProviderId("opencode".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.rate_card().version_id, "opencode-delegated-v1");
    }

    #[test]
    fn default_config_denies_child_tools() {
        assert!(!OpenCodeConfig::default().allow_child_tools);
    }

    #[test]
    fn deny_inline_config_blocks_permissions_and_sharing() {
        let parsed: serde_json::Value =
            serde_json::from_str(DENY_ALL_INLINE_CONFIG).expect("inline config is valid JSON");
        assert_eq!(parsed["permission"], "deny");
        assert_eq!(parsed["share"], "disabled");
    }

    #[test]
    fn tools_allowed_inline_config_keeps_operator_permissions() {
        // Opting into child tools must not smuggle a permission override back
        // in: only session sharing stays pinned off, so the operator's own
        // permission configuration governs the tool surface.
        let parsed: serde_json::Value = serde_json::from_str(SHARE_DISABLED_INLINE_CONFIG)
            .expect("inline config is valid JSON");
        assert_eq!(parsed["share"], "disabled");
        assert!(parsed.get("permission").is_none());
    }

    #[test]
    fn zero_timeout_env_does_not_produce_an_unrunnable_turn() {
        let parsed = Some("0".to_string())
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0);
        assert!(parsed.is_none());
    }

    #[test]
    fn is_etxtbsy_recognizes_text_file_busy() {
        assert!(is_etxtbsy(&std::io::Error::from_raw_os_error(ETXTBSY)));
        assert!(is_etxtbsy(&std::io::Error::from(
            std::io::ErrorKind::ExecutableFileBusy
        )));
        assert!(!is_etxtbsy(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn max_tokens_floor_is_configurable_and_zero_delegates() {
        let default = OpenCodeConfig::default();
        assert_eq!(default.max_tokens_floor, DEFAULT_MAX_TOKENS_FLOOR);
        let delegating = OpenCodeConfig {
            max_tokens_floor: 0,
            ..OpenCodeConfig::default()
        };
        assert_eq!(delegating.max_tokens_floor, 0);
    }
}
