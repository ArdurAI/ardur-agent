//! ardur-provider-hermes — the [Hermes Agent] CLI backend, driven as a one-shot
//! subprocess.
//!
//! The HTTP backends (Anthropic, OpenRouter, openai-compat) authenticate with an
//! API key and POST to a REST endpoint, billing per token. This backend is
//! different: it spawns the locally-installed `hermes` binary in its one-shot
//! chat mode and reads a structured JSONL event stream from stdout.
//! Authentication is inherited from Hermes' own configured provider
//! (`hermes auth` / `hermes model`), so there is no API key in this crate's
//! config and every completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport it implements the same [`Provider`]
//! trait as the HTTP backends, so the runtime dispatches to it through the
//! generic [`ProviderRegistry`] with no catalog change — which is the point:
//! a wrapped Hermes Agent runs behind the full fused pipeline (cap-token verify,
//! Cedar authorization, cost gate, signed receipt, durable journal).
//!
//! # The CLI protocol (verified against Hermes Agent docs)
//!
//! One Ardur turn maps onto one child process:
//!
//! ```text
//! hermes chat --oneshot --query-file - --format stream-json --toolsets ""
//! ```
//!
//! The prompt is written to stdin (`--query-file -`). Every stdout line is one
//! JSON object. The terminal record is a `{"type":"result", …}` event carrying
//! `text`, `tokens`, `exit_code`, and optional `error`. Diagnostics stay on
//! stderr. Process exit code matches `result.exit_code` (`0` completed, `1`
//! failed / partial / init, `130` interrupted).
//!
//! An explicitly empty `--toolsets ""` is Hermes' deny-all selection (distinct
//! from omitting the flag, which keeps configured defaults). That is the
//! fail-closed default for a *completion* backend.
//!
//! # Auth & install requirements
//!
//! The host must have the `hermes` binary on `PATH` (or
//! [`HermesConfig::binary`] pointed at it) and a usable model provider
//! configured inside Hermes. A missing binary surfaces as
//! [`ProviderError::Upstream`] ("Hermes Agent CLI not installed …"); a missing
//! or expired login surfaces as [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! | Failure | [`ProviderError`] |
//! |----------------------------------|----------------------------------|
//! | binary not found on `PATH` | [`Upstream`](ProviderError::Upstream) ("Hermes Agent CLI not installed …") |
//! | no usable model / not logged in | [`Unauthorized`](ProviderError::Unauthorized) |
//! | turn exceeded `request_timeout` | [`NetworkFailure`](ProviderError::NetworkFailure) (its docs name timeouts) |
//! | non-zero exit / `result.error` | [`Upstream`](ProviderError::Upstream) (stderr / error, secret-redacted) |
//! | turn finished with empty text | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in this phase
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole turn
//!   is awaited before a response is returned. The stream-json feed is already
//!   line-oriented, so a later streaming path is a change to this crate only.
//! - **Tool-call parsing** — Hermes orchestrates its own tools inside its own
//!   process. This layer runs it with tools denied by default
//!   ([`HermesConfig::allow_child_tools`]) and surfaces only the final assistant
//!   text, never a [`FinishReason::ToolUse`]. Governing a child's *own* tool
//!   steps is delegation work, not provider work.
//! - **Selector registration** — `ProviderKind::Hermes` /
//!   `ARDUR_PROVIDER=hermes` in `provider-selector::from_env` is deferred to the
//!   D0 router lane. This crate is buildable and testable on its own.
//!
//! # Attribution
//!
//! Hermes Agent is MIT-licensed (Nous Research). See this crate's `README.md`.
//!
//! [Hermes Agent]: https://github.com/NousResearch/hermes-agent
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
const PROVIDER_ID: &str = "hermes";
/// The binary [`HermesConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "hermes";
/// Default per-turn timeout. A Hermes turn can involve several model
/// round-trips, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Upper bound on retained stream-json event lines, so a chatty turn cannot grow
/// the audit body without limit.
const MAX_RETAINED_EVENTS: usize = 2_000;
/// Upper bound on captured stderr bytes, so a child spewing diagnostics cannot
/// grow memory without limit.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Maximum spawn retries when Linux returns `ETXTBSY` ("Text file busy"). See
/// [`spawn_hermes`] for why the retry exists.
const SPAWN_ETXTBSY_RETRIES: u32 = 6;

/// Env var [`HermesConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "HERMES_BINARY";
/// Env var [`HermesConfig::from_env`] reads Hermes' provider name from.
pub const PROVIDER_ENV: &str = "HERMES_PROVIDER";
/// Env var [`HermesConfig::from_env`] reads the default model from.
pub const DEFAULT_MODEL_ENV: &str = "HERMES_DEFAULT_MODEL";
/// Env var [`HermesConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "HERMES_WORKING_DIR";
/// Env var [`HermesConfig::from_env`] reads the per-turn timeout from.
pub const TIMEOUT_SECS_ENV: &str = "HERMES_TIMEOUT_SECS";

/// How this backend locates and runs the `hermes` binary.
#[derive(Clone, Debug)]
pub struct HermesConfig {
    /// The binary to spawn. Resolved through `PATH` when relative.
    pub binary: PathBuf,
    /// Hermes' own provider name (its `--provider` flag), e.g. `openrouter`.
    /// `None` leaves Hermes' configured default in place.
    pub provider: Option<String>,
    /// Model used when a [`CompletionRequest`] does not name one.
    pub default_model: Option<String>,
    /// Working directory for the child. `None` inherits ours.
    pub working_directory: Option<PathBuf>,
    /// Wall-clock ceiling for one turn.
    pub request_timeout: Duration,
    /// Whether the child may run its own tools. `false` (the default) passes
    /// `--toolsets ""` (Hermes deny-all): as a *completion* backend we want the
    /// model's answer, not an agent editing the filesystem outside Ardur's grant
    /// ledger. Turning this on omits `--toolsets` and hands the child Hermes'
    /// configured defaults — the fused pipeline cannot see steps taken inside
    /// another process.
    pub allow_child_tools: bool,
}

impl Default for HermesConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            provider: None,
            default_model: None,
            working_directory: None,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            allow_child_tools: false,
        }
    }
}

impl HermesConfig {
    /// Build a config from the environment, falling back to [`Default`] for any
    /// unset variable.
    ///
    /// An unparseable or zero [`TIMEOUT_SECS_ENV`] keeps the default rather than
    /// producing a turn that can never finish.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Some(binary) = non_empty_env(BINARY_ENV) {
            config.binary = PathBuf::from(binary);
        }
        config.provider = non_empty_env(PROVIDER_ENV);
        config.default_model = non_empty_env(DEFAULT_MODEL_ENV);
        config.working_directory = non_empty_env(WORKING_DIR_ENV).map(PathBuf::from);
        if let Some(secs) = non_empty_env(TIMEOUT_SECS_ENV)
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
        {
            config.request_timeout = Duration::from_secs(secs);
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

/// The Hermes Agent CLI provider.
#[derive(Debug)]
pub struct HermesProvider {
    config: HermesConfig,
    rate_card: RateCard,
}

impl HermesProvider {
    /// Build a provider from an explicit config.
    #[must_use]
    pub fn new(config: HermesConfig) -> Self {
        Self {
            config,
            rate_card: hermes_delegated_rate_card(),
        }
    }

    /// Build a provider from [`HermesConfig::from_env`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(HermesConfig::from_env())
    }

    /// The model this turn should run against: the request's, else the config's,
    /// else Hermes' own default.
    fn chosen_model(&self, requested: &ModelId) -> Option<String> {
        let requested = requested.0.trim();
        if !requested.is_empty() {
            return Some(requested.to_string());
        }
        self.config.default_model.clone()
    }
}

/// What one stream-json turn produced.
struct TurnOutcome {
    /// Final assistant text.
    content: String,
    /// Token counts reported by the child, when it reported any.
    usage: Usage,
    /// Retained event lines, as the audit body.
    events: Vec<serde_json::Value>,
}

#[async_trait]
impl Provider for HermesProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let transcript = build_transcript(&req.messages);
        if transcript.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "hermes requires a non-empty prompt".into(),
            ));
        }

        let model = self.chosen_model(&req.model);
        let outcome = tokio::time::timeout(
            self.config.request_timeout,
            self.run_turn(&transcript, model.as_deref()),
        )
        .await
        .map_err(|_| {
            ProviderError::NetworkFailure(format!(
                "hermes turn exceeded {:?}",
                self.config.request_timeout
            ))
        })??;

        let cost = CostTuple {
            tokens_in: u64::from(outcome.usage.tokens_in),
            tokens_out: u64::from(outcome.usage.tokens_out),
            // Delegated billing: Hermes' own configured provider pays for
            // the call, so there is no per-call monetary cost to attribute here.
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
        };

        Ok(CompletionResponse {
            content: outcome.content,
            finish_reason: FinishReason::Stop,
            usage: outcome.usage,
            cost,
            raw_provider_response: Some(serde_json::Value::Array(outcome.events)),
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // The stream-json feed is already line-oriented; wiring it to
        // `StreamEvent`s is a later, crate-local change.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

impl HermesProvider {
    /// Spawn the child, feed the prompt on stdin, and parse the stream-json
    /// `result` event from stdout.
    async fn run_turn(
        &self,
        transcript: &str,
        model: Option<&str>,
    ) -> Result<TurnOutcome, ProviderError> {
        let mut cmd = tokio::process::Command::new(&self.config.binary);
        cmd.arg("chat")
            .arg("--oneshot")
            // Read the prompt from stdin so quotes / $(...) arrive verbatim.
            .arg("--query-file")
            .arg("-")
            // Structured JSONL for programmatic consumption.
            .arg("--format")
            .arg("stream-json");
        if !self.config.allow_child_tools {
            // Explicit empty selection = Hermes deny-all (distinct from omitting
            // the flag, which keeps configured defaults). Pass as a single
            // `--toolsets=` argv entry: a bare empty `.arg("")` is dropped by
            // execve on some platforms and would silently fall back to defaults.
            cmd.arg("--toolsets=");
        }
        if let Some(provider) = &self.config.provider {
            cmd.arg("--provider").arg(provider);
        }
        if let Some(model) = model {
            cmd.arg("--model").arg(model);
        }
        if let Some(cwd) = &self.config.working_directory {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout this future is dropped, which drops the child;
            // kill_on_drop ensures the hermes process dies with it rather
            // than surviving as an orphan holding a model session open.
            .kill_on_drop(true);

        let mut child = spawn_hermes(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Hermes Agent CLI not installed (binary {:?} not found on PATH): {e}",
                    self.config.binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn hermes: {e}"))
            }
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Upstream("hermes child stdin was not captured".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Upstream("hermes child stdout was not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ProviderError::Upstream("hermes child stderr was not captured".into())
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
                    "writing prompt to hermes stdin: {e}"
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
                        "reading hermes stdout: {e}"
                    )));
                }
                Ok(n) => stdout_buf.extend_from_slice(&chunk[..n]),
            }
        }
        let stdout_text = String::from_utf8_lossy(&stdout_buf).into_owned();

        let status = child
            .wait()
            .await
            .map_err(|e| ProviderError::Upstream(format!("waiting on hermes subprocess: {e}")))?;
        let stderr_text = stderr_task.await.unwrap_or_default();

        let parsed = parse_stream_json(&stdout_text);

        if !status.success() {
            let detail = if !stderr_text.trim().is_empty() {
                stderr_text.trim()
            } else {
                parsed
                    .error
                    .as_deref()
                    .unwrap_or("hermes exited with a non-zero status")
            };
            if looks_like_auth_error(detail)
                || looks_like_auth_error(&stderr_text)
                || parsed.error.as_deref().is_some_and(looks_like_auth_error)
            {
                return Err(ProviderError::Unauthorized);
            }
            let code = status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            let detail = redact_child_diagnostic(detail);
            return Err(ProviderError::Upstream(format!(
                "hermes exited with status {code}: {detail}"
            )));
        }

        if let Some(err) = parsed.error.as_deref() {
            if looks_like_auth_error(err) || looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            let err = redact_child_diagnostic(err);
            return Err(ProviderError::Upstream(format!(
                "hermes reported an error: {err}"
            )));
        }

        if parsed.exit_code.is_some_and(|c| c != 0) {
            let detail = parsed
                .error
                .as_deref()
                .or_else(|| Some(stderr_text.trim()).filter(|s| !s.is_empty()))
                .unwrap_or("hermes result carried a non-zero exit_code");
            if looks_like_auth_error(detail) {
                return Err(ProviderError::Unauthorized);
            }
            let detail = redact_child_diagnostic(detail);
            return Err(ProviderError::Upstream(format!(
                "hermes result exit_code {}: {detail}",
                parsed.exit_code.unwrap_or(-1)
            )));
        }

        if parsed.content.trim().is_empty() {
            if looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            return Err(ProviderError::Upstream(
                "hermes turn produced no assistant text".into(),
            ));
        }

        Ok(TurnOutcome {
            content: parsed.content,
            usage: parsed.usage,
            events: parsed.events,
        })
    }
}

/// Fields pulled out of a `hermes … --format stream-json` stdout stream.
struct ParsedOutput {
    content: String,
    usage: Usage,
    exit_code: Option<i64>,
    error: Option<String>,
    events: Vec<serde_json::Value>,
}

/// Parse the JSONL event stream Hermes writes to stdout. Non-JSON lines are
/// skipped rather than failing the whole parse (stray banner noise must not
/// abort an otherwise valid turn).
fn parse_stream_json(stdout: &str) -> ParsedOutput {
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut exit_code = None;
    let mut error = None;
    let mut events: Vec<serde_json::Value> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        if kind == Some("result") {
            if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
                content = text.to_string();
            }
            exit_code = value.get("exit_code").and_then(serde_json::Value::as_i64);
            if let Some(err) = value.get("error").and_then(serde_json::Value::as_str) {
                if !err.is_empty() {
                    error = Some(err.to_string());
                }
            }
            if let Some(tokens) = value.get("tokens") {
                let input = tokens
                    .get("input")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let output = tokens
                    .get("output")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                usage.tokens_in = u32::try_from(input).unwrap_or(u32::MAX);
                usage.tokens_out = u32::try_from(output).unwrap_or(u32::MAX);
            }
        }
        if events.len() < MAX_RETAINED_EVENTS {
            events.push(value);
        }
    }

    ParsedOutput {
        content,
        usage,
        exit_code,
        error,
        events,
    }
}

/// Errno for Linux's `ETXTBSY` ("Text file busy"). macOS never returns it.
const ETXTBSY: i32 = 26;

/// Whether a spawn error is `ETXTBSY` ("Text file busy").
fn is_etxtbsy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy || e.raw_os_error() == Some(ETXTBSY)
}

/// Spawn the hermes subprocess, retrying briefly on `ETXTBSY` ("Text file busy").
///
/// Mirrors the provider-codex / provider-claude-cli spawn retry: parallel tests
/// that write an executable shim then exec it can race an inherited writable fd
/// across `fork`→`execve` on Linux.
async fn spawn_hermes(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
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

/// Strip secret-shaped substrings from a child diagnostic before it enters a
/// [`ProviderError`]. Hermes (or a shim) may echo an API key in stderr / the
/// stream-json `result.error`; AGENTS.md forbids surfacing those values.
fn redact_child_diagnostic(detail: &str) -> String {
    redact_text(detail, &default_secret_patterns())
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

    const CREDENTIAL_FAILURES: [&str; 12] = [
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
        "hermes auth",
    ];
    CREDENTIAL_FAILURES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Flatten a chat transcript into the single prompt string Hermes reads on
/// stdin via `--query-file -`.
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

/// A zeroed rate card. Hermes turns are paid by Hermes' own configured
/// provider, not metered here, so every completion is priced at zero cents —
/// the card exists only to satisfy [`Provider::rate_card`].
fn hermes_delegated_rate_card() -> RateCard {
    RateCard {
        version_id: "hermes-delegated-v1".to_string(),
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
    fn parse_extracts_result_text_tokens_and_exit_code() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m\",\"session_id\":\"s1\"}\n",
            "{\"type\":\"text\",\"text\":\"hel\"}\n",
            "{\"type\":\"result\",\"session_id\":\"s1\",\"exit_code\":0,\"text\":\"hello\",\
             \"tokens\":{\"input\":11,\"output\":2,\"total\":13}}\n",
        );
        let parsed = parse_stream_json(stdout);
        assert_eq!(parsed.content, "hello");
        assert_eq!(parsed.usage.tokens_in, 11);
        assert_eq!(parsed.usage.tokens_out, 2);
        assert_eq!(parsed.exit_code, Some(0));
        assert!(parsed.error.is_none());
        assert!(parsed.events.len() >= 2);
    }

    #[test]
    fn parse_skips_non_json_noise() {
        let stdout = concat!(
            "warming up\n",
            "{\"type\":\"result\",\"exit_code\":0,\"text\":\"ok\",\"tokens\":{\"input\":1,\"output\":1}}\n",
        );
        let parsed = parse_stream_json(stdout);
        assert_eq!(parsed.content, "ok");
    }

    #[test]
    fn parse_captures_in_band_error() {
        let stdout = "{\"type\":\"result\",\"exit_code\":1,\"text\":\"\",\"error\":\"boom\",\"tokens\":{}}\n";
        let parsed = parse_stream_json(stdout);
        assert_eq!(parsed.error.as_deref(), Some("boom"));
        assert_eq!(parsed.exit_code, Some(1));
    }

    #[test]
    fn auth_errors_are_distinguished_from_general_failures() {
        assert!(looks_like_auth_error(
            "Error: not logged in; run hermes auth"
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
        let provider = HermesProvider::new(HermesConfig {
            default_model: Some("config-model".into()),
            ..HermesConfig::default()
        });
        assert_eq!(
            provider.chosen_model(&ModelId("request-model".into())),
            Some("request-model".to_string())
        );
        assert_eq!(
            provider.chosen_model(&ModelId(String::new())),
            Some("config-model".to_string())
        );
    }

    #[test]
    fn provider_id_is_hermes_and_not_streaming() {
        let provider = HermesProvider::new(HermesConfig::default());
        assert_eq!(provider.id(), ProviderId("hermes".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.rate_card().version_id, "hermes-delegated-v1");
    }

    #[test]
    fn default_config_denies_child_tools() {
        assert!(!HermesConfig::default().allow_child_tools);
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
}
