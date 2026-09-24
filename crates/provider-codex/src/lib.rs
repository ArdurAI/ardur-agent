//! ardur-provider-codex — the [OpenAI Codex] CLI backend, driven as a one-shot
//! subprocess.
//!
//! The HTTP backends (Anthropic, OpenRouter, openai-compat) authenticate with
//! an API key and POST to a REST endpoint, billing per token. This backend is
//! different: it spawns the locally-installed `codex` binary non-interactively
//! (`codex exec --json`) and reads a structured JSONL event stream from
//! stdout. Authentication is inherited from Codex's own configured session
//! (`codex login`, `~/.codex`), so there is no API key in this crate's config
//! and every completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport it implements the same [`Provider`]
//! trait as the HTTP backends, so the runtime dispatches to it through the
//! generic [`ProviderRegistry`] with no catalog change — which is the point:
//! a wrapped Codex runs behind the full fused pipeline (cap-token verify,
//! Cedar authorization, cost gate, signed receipt, durable journal).
//!
//! # The CLI protocol
//!
//! One Ardur turn maps onto one child process:
//!
//! ```text
//! codex exec --json --skip-git-repo-check --ephemeral --color never \
//!   -s <sandbox> [-C <dir>] [-m <model>]
//! ```
//!
//! The prompt is written to stdin: with no prompt argument, `exec` reads the
//! piped stdin text as the instructions. Every stdout line is one JSON event
//! object; the turn's answer is the **last**
//! `{"type":"item.completed","item":{"type":"agent_message","text":…}}`
//! event, and token usage rides on
//! `{"type":"turn.completed","usage":{"input_tokens":…,"output_tokens":…}}`.
//! `turn.failed` / `error` events describe failures. Non-JSON stdout lines
//! (stray log noise) are skipped rather than aborting the parse but retained
//! (capped) for failure classification. When no event is parseable at all,
//! ANSI-stripped raw stdout is used as the content — the plain-text fallback
//! for a child that ignored `--json`.
//!
//! The flag surface (`--json`, `--ephemeral`, `--skip-git-repo-check`,
//! `--color`, `-s/--sandbox` with `read-only | workspace-write |
//! danger-full-access`, `-C/--cd`, `-m/--model`, stdin-as-prompt) is verified
//! against the installed codex-cli 0.154.0 `exec --help`; the JSONL event
//! vocabulary (`item.completed` / `turn.completed` / `turn.failed`) was pinned
//! by the Phase-1 wrap's live probes (#67, #82).
//!
//! # Deny-by-default
//!
//! Model-generated shell commands inside the child run under codex's own
//! sandbox, selected with `-s`. The default is the most restrictive
//! [`SandboxMode::ReadOnly`] — the child may read the workspace but not write
//! files or run mutating commands — which is the fail-closed default for a
//! *completion* backend: we want the model's answer, not an agent mutating the
//! filesystem outside Ardur's grant ledger. The fused pipeline cannot see
//! steps taken inside another process. [`CODEX_SANDBOX_MODE_ENV`] opts into
//! [`SandboxMode::WorkspaceWrite`] or [`SandboxMode::DangerFullAccess`] when
//! the operator accepts that trade.
//!
//! # Auth & install requirements
//!
//! The host must have the `codex` binary on `PATH` (or
//! [`CodexConfig::codex_binary`] pointed at it) and an active session from
//! `codex login`. A missing binary surfaces as [`ProviderError::Upstream`]
//! ("Codex CLI not installed …"); a missing/expired login surfaces as
//! [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! | Failure | [`ProviderError`] |
//! |----------------------------------|----------------------------------|
//! | binary not found on `PATH` | [`Upstream`](ProviderError::Upstream) ("Codex CLI not installed …") |
//! | not logged in (`codex login`) | [`Unauthorized`](ProviderError::Unauthorized) |
//! | turn exceeded `request_timeout` | [`NetworkFailure`](ProviderError::NetworkFailure) |
//! | rate-limit / quota diagnostic | [`RateLimited`](ProviderError::RateLimited) |
//! | non-zero exit / failed turn | [`Upstream`](ProviderError::Upstream) (stderr / event, secret-redacted) |
//! | turn finished with empty text | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in this phase
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole
//!   turn is awaited before a response is returned. The event feed is already
//!   line-oriented, so a later streaming path is a change to this crate only.
//! - **Tool-call parsing** — codex orchestrates its own tools inside its own
//!   sandbox; this layer surfaces only the final assistant text, never a
//!   [`FinishReason::ToolUse`]. Governing a child's *own* tool steps is
//!   delegation work, not provider work.
//! - **Selector registration** — selected at boot via `ARDUR_PROVIDER=codex`
//!   (alias `codex-agent`) in `provider-selector::from_env` →
//!   [`CodexProvider::from_env`].
//!
//! # Attribution
//!
//! Codex CLI is published by OpenAI. See this crate's `README.md`.
//!
//! [OpenAI Codex]: https://github.com/openai/codex
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
const PROVIDER_ID: &str = "codex";
/// The binary [`CodexConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "codex";
/// Default per-turn timeout. A codex turn can involve several model
/// round-trips, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default [`CodexConfig::max_tokens_floor`]. A request asking for fewer
/// output tokens than this is refused, because `codex exec` exposes no
/// per-request output ceiling and silently overshooting the caller's
/// authorized budget is worse than failing the request.
const DEFAULT_MAX_TOKENS_FLOOR: u32 = 4_096;
/// Upper bound on retained event lines, so a chatty turn cannot grow the audit
/// body without limit.
const MAX_RETAINED_EVENTS: usize = 2_000;
/// Upper bound on captured stderr bytes, so a child spewing diagnostics cannot
/// grow memory without limit.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Upper bound on captured stdout bytes while draining the child.
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
/// Upper bound on retained non-JSON stdout noise bytes (stray log output),
/// used for classification.
const MAX_NOISE_BYTES: usize = 16 * 1024;
/// Maximum spawn retries when Linux returns `ETXTBSY` ("Text file busy"). See
/// [`spawn_codex`] for why the retry exists.
const SPAWN_ETXTBSY_RETRIES: u32 = 6;

/// Env var [`CodexConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "CODEX_BINARY";
/// Env var [`CodexConfig::from_env`] reads the default model from.
pub const DEFAULT_MODEL_ENV: &str = "CODEX_DEFAULT_MODEL";
/// Env var [`CodexConfig::from_env`] reads the sandbox mode from.
pub const SANDBOX_MODE_ENV: &str = "CODEX_SANDBOX_MODE";
/// Env var [`CodexConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "CODEX_WORKING_DIR";
/// Env var [`CodexConfig::from_env`] reads the per-turn timeout from.
pub const TIMEOUT_SECS_ENV: &str = "CODEX_TIMEOUT_SECS";
/// Env var [`CodexConfig::from_env`] reads the max-tokens floor from.
pub const MAX_TOKENS_FLOOR_ENV: &str = "CODEX_MAX_TOKENS_FLOOR";

/// The sandbox policy `codex exec` runs model-generated shell commands under
/// (the `-s/--sandbox` flag).
///
/// Because this provider is used as a *text-completion* backend (we want the
/// model's answer, not file edits), the default is the most restrictive
/// [`ReadOnly`](SandboxMode::ReadOnly) — the deny-by-default posture. Codex
/// 0.136 replaced approval modes with sandbox modes; these are the values the
/// installed CLI accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// `read-only` — codex may read the workspace but not write or run mutating
    /// commands. The safe default for a completion backend.
    #[default]
    ReadOnly,
    /// `workspace-write` — codex may edit files inside its working directory.
    /// Opt-in: the fused pipeline cannot see or govern steps taken inside the
    /// child.
    WorkspaceWrite,
    /// `danger-full-access` — no sandbox. Use only in an externally-sandboxed
    /// environment.
    DangerFullAccess,
}

impl SandboxMode {
    /// The `-s/--sandbox` flag value codex expects.
    #[must_use]
    pub fn as_flag(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }

    /// Parse a [`SANDBOX_MODE_ENV`] value, tolerating the canonical flag spelling
    /// (`read-only`), an underscore spelling (`read_only`), and case. Returns
    /// `None` for an unrecognized value so the caller can keep the default.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Some(SandboxMode::ReadOnly),
            "workspace-write" | "workspacewrite" => Some(SandboxMode::WorkspaceWrite),
            "danger-full-access" | "dangerfullaccess" | "full-access" => {
                Some(SandboxMode::DangerFullAccess)
            }
            _ => None,
        }
    }
}

/// How a [`CodexProvider`] invokes the `codex` CLI.
///
/// Build it with [`CodexConfig::new`] (or [`CodexConfig::from_env`]) and tune
/// the optional fields with the builder methods. There is **no API key** —
/// auth is inherited from `codex login`.
#[derive(Clone, Debug)]
pub struct CodexConfig {
    codex_binary: PathBuf,
    default_model: Option<String>,
    working_directory: Option<PathBuf>,
    sandbox_mode: SandboxMode,
    request_timeout: Duration,
    max_tokens_floor: u32,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            codex_binary: PathBuf::from(DEFAULT_BINARY),
            default_model: None,
            working_directory: None,
            sandbox_mode: SandboxMode::default(),
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_tokens_floor: DEFAULT_MAX_TOKENS_FLOOR,
        }
    }
}

impl CodexConfig {
    /// A config that resolves `codex` through `PATH`, with no default model, a
    /// read-only sandbox, and the default 5-minute timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a config from the environment.
    ///
    /// Reads [`BINARY_ENV`], [`DEFAULT_MODEL_ENV`], [`SANDBOX_MODE_ENV`],
    /// [`WORKING_DIR_ENV`], [`TIMEOUT_SECS_ENV`], and
    /// [`MAX_TOKENS_FLOOR_ENV`]; any unset/empty/unparseable value falls back
    /// to its default. This is **infallible** — unlike the HTTP backends there
    /// is no API key to be missing, so there is nothing here that can fail (a
    /// missing `codex login` is only discovered when a turn actually runs).
    ///
    /// An unparseable or zero [`TIMEOUT_SECS_ENV`] keeps the default rather
    /// than producing a turn that can never finish. The floor accepts zero
    /// (delegate unconditionally); only an unparseable value keeps the
    /// default.
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::new();
        if let Some(binary) = non_empty_env(BINARY_ENV) {
            cfg.codex_binary = PathBuf::from(binary);
        }
        if let Some(model) = non_empty_env(DEFAULT_MODEL_ENV) {
            cfg.default_model = Some(model);
        }
        if let Some(mode) = non_empty_env(SANDBOX_MODE_ENV) {
            if let Some(parsed) = SandboxMode::parse(&mode) {
                cfg.sandbox_mode = parsed;
            }
        }
        if let Some(dir) = non_empty_env(WORKING_DIR_ENV) {
            cfg.working_directory = Some(PathBuf::from(dir));
        }
        if let Some(secs) = non_empty_env(TIMEOUT_SECS_ENV)
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
        {
            cfg.request_timeout = Duration::from_secs(secs);
        }
        // `0` is meaningful here (delegate unconditionally), so unlike the
        // timeout this accepts zero; only an unparseable value keeps the default.
        if let Some(floor) =
            non_empty_env(MAX_TOKENS_FLOOR_ENV).and_then(|raw| raw.parse::<u32>().ok())
        {
            cfg.max_tokens_floor = floor;
        }
        cfg
    }

    /// Override the path to the `codex` binary.
    #[must_use]
    pub fn codex_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_binary = path.into();
        self
    }

    /// Set the model passed with `-m` when a request does not name its own.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Set the working directory codex runs in (`-C`). Codex needs a cwd for
    /// its sandbox; when unset, it inherits this process's.
    #[must_use]
    pub fn working_directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// Override the sandbox policy (`-s`).
    #[must_use]
    pub fn sandbox_mode(mut self, mode: SandboxMode) -> Self {
        self.sandbox_mode = mode;
        self
    }

    /// Override the per-run timeout.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the smallest per-request `max_tokens` this backend will
    /// accept; `0` accepts every ceiling. See the crate README ("Why there is
    /// a max-tokens floor") for the rationale.
    #[must_use]
    pub fn max_tokens_floor(mut self, floor: u32) -> Self {
        self.max_tokens_floor = floor;
        self
    }
}

/// Read an environment variable, treating empty/whitespace as unset.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

/// The Codex CLI provider.
///
/// Construct it with [`CodexProvider::new`] (from a [`CodexConfig`] and a
/// default model) or [`CodexProvider::from_env`]. The model on each
/// [`CompletionRequest`] selects which model codex runs; `model_id` is only
/// the default the runtime stamps onto a request.
pub struct CodexProvider {
    config: CodexConfig,
    model_id: ModelId,
    rate_card: RateCard,
}

impl CodexProvider {
    /// Build a provider from `config` with a default `model_id`.
    #[must_use]
    pub fn new(config: CodexConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: codex_subscription_rate_card(),
        }
    }

    /// Build a provider with a default `model_id`, reading the config from the
    /// environment ([`CodexConfig::from_env`]).
    #[must_use]
    pub fn from_env(model_id: ModelId) -> Self {
        Self::new(CodexConfig::from_env(), model_id)
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// The model to pass codex for this request: the request's own model when
    /// non-empty, else the config default. `None` lets codex pick its own
    /// default (no `-m` flag emitted).
    fn chosen_model<'a>(&'a self, req_model: &'a ModelId) -> Option<&'a str> {
        let requested = req_model.0.trim();
        if !requested.is_empty() {
            Some(requested)
        } else {
            self.config.default_model.as_deref()
        }
    }
}

/// What one `codex exec --json` turn produced.
struct TurnOutcome {
    /// Final assistant text (the last `agent_message`, or the ANSI-stripped
    /// stdout fallback).
    content: String,
    /// Token counts from the `turn.completed` event (zero when the child
    /// reported none, rather than invented).
    usage: Usage,
    /// Retained event lines, as the audit body.
    events: Vec<serde_json::Value>,
}

#[async_trait]
impl Provider for CodexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let transcript = build_transcript(&req.messages);
        if transcript.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "codex requires a non-empty prompt".into(),
            ));
        }
        // The caller's output-token ceiling must not be silently discarded.
        // `codex exec` exposes no per-request output cap, so a ceiling this
        // backend cannot enforce is refused rather than ignored: the fused
        // runtime would otherwise authorize N tokens, be billed for more, and
        // still see a clean `FinishReason::Stop`.
        //
        // `max_tokens_floor` is the ceiling at or above which the operator
        // accepts that enforcement is delegated to codex's own limits. Pass 0
        // to acknowledge the ceiling is delegated unconditionally.
        if req.max_tokens > 0 && req.max_tokens < self.config.max_tokens_floor {
            return Err(ProviderError::InvalidRequest(format!(
                "codex exec cannot enforce a per-request output ceiling of {} tokens \
                 (it has no per-completion cap); raise max_tokens to at least {} or use \
                 a provider that enforces it",
                req.max_tokens, self.config.max_tokens_floor
            )));
        }

        let model = self.chosen_model(&req.model).map(str::to_string);
        let outcome = tokio::time::timeout(
            self.config.request_timeout,
            self.run_turn(&transcript, model.as_deref()),
        )
        .await
        .map_err(|_| {
            ProviderError::NetworkFailure(format!(
                "codex turn exceeded {:?}",
                self.config.request_timeout
            ))
        })??;

        let cost = CostTuple {
            tokens_in: u64::from(outcome.usage.tokens_in),
            tokens_out: u64::from(outcome.usage.tokens_out),
            // Subscription-billed: the ChatGPT plan pays for the call, so
            // there is no per-call monetary cost to attribute onto the turn.
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

impl CodexProvider {
    /// Spawn the child, feed the prompt on stdin, and parse the JSONL event
    /// stream from stdout.
    async fn run_turn(
        &self,
        transcript: &str,
        model: Option<&str>,
    ) -> Result<TurnOutcome, ProviderError> {
        let mut cmd = tokio::process::Command::new(&self.config.codex_binary);
        cmd.arg("exec")
            .arg("--json")
            // Run anywhere, not just inside a git repo.
            .arg("--skip-git-repo-check")
            // A completion call should not litter session files on disk.
            .arg("--ephemeral")
            // Stable, parseable stdout.
            .arg("--color")
            .arg("never")
            // Deny-by-default: model-generated commands run under codex's
            // read-only sandbox unless the operator opts into more.
            .arg("-s")
            .arg(self.config.sandbox_mode.as_flag());
        if let Some(cwd) = &self.config.working_directory {
            cmd.arg("-C").arg(cwd);
        }
        if let Some(model) = model {
            cmd.arg("-m").arg(model);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout this future is dropped, which drops the child;
            // kill_on_drop ensures the codex process dies with it rather
            // than surviving as an orphan holding a model session open.
            .kill_on_drop(true);

        let mut child = spawn_codex(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Codex CLI not installed (binary {:?} not found on PATH): {e}",
                    self.config.codex_binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn codex: {e}"))
            }
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Upstream("codex child stdin was not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Upstream("codex child stdout was not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::Upstream("codex child stderr was not captured".into()))?;

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
            // A child that exits before draining stdin (e.g. it failed fast on
            // a bad flag, or is already done) closes the read end, so the
            // write races to a BrokenPipe. That is not our failure to
            // report — the exit status and captured stderr are the source of
            // truth.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                return Err(ProviderError::Upstream(format!(
                    "writing prompt to codex stdin: {e}"
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
                        "reading codex stdout: {e}"
                    )));
                }
                Ok(n) => {
                    if stdout_buf.len() >= MAX_STDOUT_BYTES {
                        return Err(ProviderError::Upstream(format!(
                            "codex stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                    let room = MAX_STDOUT_BYTES - stdout_buf.len();
                    stdout_buf.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room {
                        return Err(ProviderError::Upstream(format!(
                            "codex stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                }
            }
        }
        let stdout_text = String::from_utf8_lossy(&stdout_buf).into_owned();

        let status = child
            .wait()
            .await
            .map_err(|e| ProviderError::Upstream(format!("waiting on codex subprocess: {e}")))?;
        let stderr_text = stderr_task.await.unwrap_or_default();

        let parsed = parse_codex_output(&stdout_text);

        if !status.success() {
            // Codex reports failures on stderr; the stdout noise is scanned
            // too so an error line printed there cannot mask classification.
            let detail = if !stderr_text.trim().is_empty() {
                stderr_text.trim().to_string()
            } else if !parsed.noise.trim().is_empty() {
                parsed.noise.trim().to_string()
            } else {
                "codex exited with a non-zero status".to_string()
            };
            return Err(classify_child_failure(
                &detail,
                &stderr_text,
                &parsed.noise,
                status.code(),
            ));
        }

        // A `turn.failed` / `error` event must not look like a clean success:
        // fail closed with the event's diagnostic.
        if let FinishReason::Error(message) = &parsed.finish_reason {
            let detail = if message.is_empty() {
                "codex turn failed".to_string()
            } else {
                message.clone()
            };
            return Err(classify_child_failure(
                &detail,
                &stderr_text,
                &parsed.noise,
                status.code(),
            ));
        }

        // Prefer the parsed `agent_message`; fall back to ANSI-stripped raw
        // stdout (the plain-text path for output that carried no JSON events).
        let content = if parsed.content.is_empty() {
            strip_ansi(&stdout_text).trim().to_string()
        } else {
            parsed.content
        };
        if content.trim().is_empty() {
            if looks_like_auth_error(&stderr_text) || looks_like_auth_error(&parsed.noise) {
                return Err(ProviderError::Unauthorized);
            }
            return Err(ProviderError::Upstream(
                "codex turn produced no parseable output".into(),
            ));
        }

        Ok(TurnOutcome {
            content,
            usage: parsed.usage,
            events: parsed.events,
        })
    }
}

/// Fields pulled out of a `codex exec --json` stdout stream.
struct ParsedOutput {
    /// The final `agent_message` text (last one wins), empty if none.
    content: String,
    /// Token usage from the `turn.completed` event.
    usage: Usage,
    /// `Stop` normally, `Error` if a `turn.failed`/`error` event was seen.
    finish_reason: FinishReason,
    /// Non-JSON stdout lines (stray log output), retained for classification.
    noise: String,
    /// Every decoded JSON event, retained as the raw audit body.
    events: Vec<serde_json::Value>,
}

/// Parse the JSONL event stream codex writes to stdout. Non-JSON lines are
/// skipped rather than failing the whole parse (stray banner noise must not
/// abort an otherwise valid turn) but retained in `noise` for classification.
fn parse_codex_output(stdout: &str) -> ParsedOutput {
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut finish_reason = FinishReason::Stop;
    let mut noise = String::new();
    let mut events: Vec<serde_json::Value> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            if noise.len() < MAX_NOISE_BYTES {
                if !noise.is_empty() {
                    noise.push('\n');
                }
                let room = MAX_NOISE_BYTES - noise.len();
                noise.push_str(&line.chars().take(room).collect::<String>());
            }
            continue;
        };
        match value["type"].as_str() {
            Some("item.completed") => {
                if value["item"]["type"] == "agent_message" {
                    if let Some(text) = value["item"]["text"].as_str() {
                        // Last agent_message is the final answer.
                        content = text.to_string();
                    }
                }
            }
            Some("turn.completed") => {
                let u = &value["usage"];
                usage.tokens_in = u["input_tokens"].as_u64().unwrap_or(0) as u32;
                usage.tokens_out = u["output_tokens"].as_u64().unwrap_or(0) as u32;
            }
            Some("turn.failed") | Some("error") => {
                let msg = value["error"]["message"]
                    .as_str()
                    .or_else(|| value["message"].as_str())
                    .unwrap_or("codex turn failed");
                finish_reason = FinishReason::Error(msg.to_string());
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
        finish_reason,
        noise,
        events,
    }
}

/// Errno for Linux's `ETXTBSY` ("Text file busy"). macOS never returns it.
const ETXTBSY: i32 = 26;

/// Whether a spawn error is `ETXTBSY` ("Text file busy"). Matches both the
/// stable [`ErrorKind::ExecutableFileBusy`](std::io::ErrorKind::ExecutableFileBusy)
/// mapping and the raw errno, so it holds even if the kind mapping is absent.
fn is_etxtbsy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy || e.raw_os_error() == Some(ETXTBSY)
}

/// Spawn the codex subprocess, retrying briefly on `ETXTBSY` ("Text file busy").
///
/// On Linux, `execve(2)` fails with `ETXTBSY` if **any** process still holds the
/// target file open for writing. In a multithreaded program that both writes
/// executables and spawns subprocesses — exactly this crate's test suite, where
/// parallel tests each write a shim then exec it — a sibling thread's
/// `fork()`+`execve()` transiently inherits the just-written file's writable fd
/// across the fork window, so our `execve` of that file races to `ETXTBSY` even
/// though our own writer was already closed. A bounded retry with a small
/// linear backoff closes it deterministically. macOS never returns `ETXTBSY`,
/// so this is a no-op there.
async fn spawn_codex(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    let mut attempt: u32 = 0;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if is_etxtbsy(&e) && attempt < SPAWN_ETXTBSY_RETRIES => {
                attempt += 1;
                // Linear backoff: the racing fork's exec closes the inherited
                // writable fd within a few milliseconds, so a handful of short
                // sleeps is plenty without inflating the happy path.
                tokio::time::sleep(Duration::from_millis(2 * u64::from(attempt))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Strip secret-shaped substrings from a child diagnostic before it enters a
/// [`ProviderError`]. Codex (or a shim) may echo an API key in stderr / a
/// stdout error line; AGENTS.md forbids surfacing those values.
fn redact_child_diagnostic(detail: &str) -> String {
    redact_text(detail, &default_secret_patterns())
}

/// Whether a diagnostic describes a rate-limit / quota condition.
fn looks_like_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "quota",
        "too many requests",
        "error code: 429",
        "status 429",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Whether a diagnostic describes a *credential* failure rather than a general
/// failure.
///
/// The phrases are deliberately specific. Rate-limit / quota diagnostics that
/// merely mention a key must not be reclassified as
/// [`ProviderError::Unauthorized`].
fn looks_like_auth_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();

    const RETRYABLE: [&str; 7] = [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "quota",
        "too many requests",
        "error code: 429",
        "status 429",
    ];
    if RETRYABLE.iter().any(|needle| lowered.contains(needle)) {
        return false;
    }

    const CREDENTIAL_FAILURES: [&str; 14] = [
        "not logged in",
        "codex login",
        "please log in",
        "please login",
        "no credentials",
        "missing api key",
        "invalid api key",
        "unauthorized",
        "unauthenticated",
        "authentication failed",
        "error code: 401",
        "status 401",
        "credentials",
        "sign in",
    ];
    CREDENTIAL_FAILURES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Classify a failed child into the typed error taxonomy, redacting any
/// secret-shaped material from the retained diagnostic. The stderr text and
/// stdout noise are scanned alongside the primary detail so an unrelated
/// warning cannot mask an auth or rate-limit result.
fn classify_child_failure(
    detail: &str,
    stderr_text: &str,
    noise: &str,
    exit_code: Option<i32>,
) -> ProviderError {
    if looks_like_auth_error(detail)
        || looks_like_auth_error(stderr_text)
        || looks_like_auth_error(noise)
    {
        return ProviderError::Unauthorized;
    }
    if looks_like_rate_limit(detail)
        || looks_like_rate_limit(stderr_text)
        || looks_like_rate_limit(noise)
    {
        return ProviderError::RateLimited { retry_after_ms: 0 };
    }
    let redacted = redact_child_diagnostic(detail);
    match exit_code {
        Some(code) => {
            ProviderError::Upstream(format!("codex exec exited with status {code}: {redacted}"))
        }
        None => ProviderError::Upstream(format!("codex reported an error: {redacted}")),
    }
}

/// Flatten a chat transcript into the single prompt string codex reads on
/// stdin.
///
/// System messages are joined at the top as instructions; the remaining turns
/// are rendered `User:`/`Assistant:` so the model sees the conversation shape.
/// A replayed tool result renders as a labelled line so the transcript is
/// intelligible rather than dropped — codex orchestrates its own tools, so
/// this layer never drives a tool loop.
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

/// Strip ANSI CSI escape sequences (`ESC [ … letter`) from a string — the
/// plain-text fallback's cleanup so colored TUI output does not leak into the
/// response content.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC '[' then params, ending on a letter (final byte).
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // A bare ESC (or other escape) is just dropped.
        } else {
            out.push(c);
        }
    }
    out
}

/// A zeroed rate card. Codex calls are paid by the user's ChatGPT subscription,
/// not per token, so every completion is priced at zero cents — the card exists
/// only to satisfy [`Provider::rate_card`].
fn codex_subscription_rate_card() -> RateCard {
    RateCard {
        version_id: "codex-subscription-v1".to_string(),
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
    fn sandbox_mode_flag_and_parse_roundtrip() {
        assert_eq!(SandboxMode::ReadOnly.as_flag(), "read-only");
        assert_eq!(SandboxMode::WorkspaceWrite.as_flag(), "workspace-write");
        assert_eq!(
            SandboxMode::DangerFullAccess.as_flag(),
            "danger-full-access"
        );
        assert_eq!(SandboxMode::parse("read-only"), Some(SandboxMode::ReadOnly));
        assert_eq!(SandboxMode::parse("READ_ONLY"), Some(SandboxMode::ReadOnly));
        assert_eq!(
            SandboxMode::parse("workspace-write"),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            SandboxMode::parse("danger-full-access"),
            Some(SandboxMode::DangerFullAccess)
        );
        assert_eq!(SandboxMode::parse("bogus"), None);
        assert_eq!(SandboxMode::default(), SandboxMode::ReadOnly);
    }

    #[test]
    fn config_builder_sets_every_field() {
        let cfg = CodexConfig::new()
            .codex_binary("/opt/bin/codex")
            .default_model("gpt-5-codex")
            .working_directory("/tmp/work")
            .sandbox_mode(SandboxMode::WorkspaceWrite)
            .request_timeout(Duration::from_secs(42))
            .max_tokens_floor(1_024);
        assert_eq!(cfg.codex_binary, PathBuf::from("/opt/bin/codex"));
        assert_eq!(cfg.default_model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(cfg.working_directory, Some(PathBuf::from("/tmp/work")));
        assert_eq!(cfg.sandbox_mode, SandboxMode::WorkspaceWrite);
        assert_eq!(cfg.request_timeout, Duration::from_secs(42));
        assert_eq!(cfg.max_tokens_floor, 1_024);
    }

    #[test]
    fn default_config_denies_by_default_and_floors() {
        let cfg = CodexConfig::new();
        assert_eq!(cfg.codex_binary, PathBuf::from(DEFAULT_BINARY));
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.sandbox_mode, SandboxMode::ReadOnly);
        assert_eq!(
            cfg.request_timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
        assert_eq!(cfg.max_tokens_floor, DEFAULT_MAX_TOKENS_FLOOR);
    }

    #[test]
    fn max_tokens_floor_is_configurable_and_zero_delegates() {
        let delegating = CodexConfig::new().max_tokens_floor(0);
        assert_eq!(delegating.max_tokens_floor, 0);
    }

    #[test]
    fn chosen_model_prefers_request_then_default() {
        let provider = CodexProvider::new(
            CodexConfig::new().default_model("gpt-5"),
            ModelId::new("gpt-5"),
        );
        // Request names a model → that wins.
        assert_eq!(
            provider.chosen_model(&ModelId::new("gpt-5-codex")),
            Some("gpt-5-codex")
        );
        // Empty request model → config default.
        assert_eq!(provider.chosen_model(&ModelId::new("")), Some("gpt-5"));
        // No request, no default → None (codex picks its own).
        let bare = CodexProvider::new(CodexConfig::new(), ModelId::new(""));
        assert_eq!(bare.chosen_model(&ModelId::new("")), None);
    }

    #[test]
    fn transcript_puts_system_on_top_and_labels_turns() {
        let transcript = build_transcript(&[
            msg(Role::System, "be terse"),
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
            msg(Role::User, "ping"),
        ]);
        assert_eq!(
            transcript,
            "be terse\n\nUser: hi\n\nAssistant: hello\n\nUser: ping"
        );
    }

    #[test]
    fn transcript_without_system_is_just_dialogue() {
        let transcript = build_transcript(&[msg(Role::User, "only me")]);
        assert_eq!(transcript, "User: only me");
    }

    #[test]
    fn transcript_renders_tool_results_rather_than_dropping_them() {
        let transcript = build_transcript(&[msg(Role::Tool, "exit=0")]);
        assert_eq!(transcript, "Tool result: exit=0");
    }

    #[test]
    fn parse_extracts_content_and_usage_from_jsonl() {
        let stdout = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n",
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"pong\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":42,\"cached_input_tokens\":3,\"output_tokens\":7}}\n"
        );
        let parsed = parse_codex_output(stdout);
        assert_eq!(parsed.content, "pong");
        assert_eq!(parsed.usage.tokens_in, 42);
        assert_eq!(parsed.usage.tokens_out, 7);
        assert!(matches!(parsed.finish_reason, FinishReason::Stop));
        assert_eq!(parsed.events.len(), 4);
        assert!(parsed.noise.is_empty());
    }

    #[test]
    fn parse_skips_non_json_lines_and_takes_last_agent_message() {
        let stdout = concat!(
            "2026-06-03 some stray log line that is not json\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"first\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"final\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n"
        );
        let parsed = parse_codex_output(stdout);
        assert_eq!(parsed.content, "final");
        assert_eq!(parsed.events.len(), 3); // stray line skipped
        assert!(parsed.noise.contains("stray log line"));
    }

    #[test]
    fn parse_marks_turn_failed_as_error() {
        let stdout = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"model overloaded\"}}\n";
        let parsed = parse_codex_output(stdout);
        assert!(matches!(parsed.finish_reason, FinishReason::Error(m) if m == "model overloaded"));
    }

    #[test]
    fn auth_errors_are_distinguished_from_general_failures() {
        assert!(looks_like_auth_error(
            "Error: Not logged in. Run `codex login`."
        ));
        assert!(looks_like_auth_error("no credentials found"));
        assert!(looks_like_auth_error("401 unauthorized"));
        assert!(!looks_like_auth_error("model produced an internal error"));
        // MCP auth noise must NOT be mistaken for a codex login failure.
        assert!(!looks_like_auth_error(
            "ERROR rmcp::transport: AuthRequired Authorization header required"
        ));
    }

    #[test]
    fn rate_limits_naming_a_key_are_not_credential_failures() {
        for retryable in [
            "Error code: 429 - rate limit exceeded for API key sk-abc",
            "Rate-limit hit; retry later (api key quota)",
            "quota exhausted for this api key",
            "429 too many requests",
        ] {
            assert!(
                !looks_like_auth_error(retryable),
                "{retryable:?} must not be classified as a credential failure"
            );
            assert!(
                looks_like_rate_limit(retryable),
                "{retryable:?} should be classified as a rate limit"
            );
        }
    }

    #[test]
    fn child_diagnostics_are_redacted_before_entering_upstream_errors() {
        // A long fake key of the shape the redaction set masks
        // (\bsk-[a-z0-9_-]{16,}).
        let raw = "Error code: 500 for API key sk-abcdefghij0123456789";
        let redacted = redact_child_diagnostic(raw);
        assert!(
            !redacted.contains("sk-abcdefghij0123456789"),
            "secret must not survive redaction: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED>"),
            "expected redaction marker, got: {redacted}"
        );
        assert!(
            redacted.contains("Error code: 500"),
            "non-secret text must remain: {redacted}"
        );
    }

    #[test]
    fn classification_prefers_auth_and_rate_limit_over_generic_upstream() {
        assert!(matches!(
            classify_child_failure("Not logged in. Run `codex login`.", "", "", Some(1)),
            ProviderError::Unauthorized
        ));
        assert!(matches!(
            classify_child_failure("Error code: 429 - rate limit exceeded", "", "", Some(1)),
            ProviderError::RateLimited { .. }
        ));
        match classify_child_failure("boom: simulated codex internal error", "", "", Some(3)) {
            ProviderError::Upstream(msg) => {
                assert!(msg.contains("status 3"), "got: {msg}");
                assert!(msg.contains("simulated codex internal error"), "got: {msg}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let colored = "\u{1b}[1;32mgreen\u{1b}[0m plain";
        assert_eq!(strip_ansi(colored), "green plain");
    }

    #[test]
    fn is_etxtbsy_recognizes_text_file_busy() {
        // The raw errno path (what the kernel actually returns on a spawn race).
        assert!(is_etxtbsy(&std::io::Error::from_raw_os_error(ETXTBSY)));
        // The stable ErrorKind mapping, independent of errno.
        assert!(is_etxtbsy(&std::io::Error::from(
            std::io::ErrorKind::ExecutableFileBusy
        )));
        // A genuinely-missing binary must not be mistaken for the busy race.
        assert!(!is_etxtbsy(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn provider_id_is_codex_and_not_streaming() {
        let provider = CodexProvider::new(CodexConfig::new(), ModelId::new("gpt-5-codex"));
        assert_eq!(provider.id(), ProviderId("codex".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.rate_card().version_id, "codex-subscription-v1");
        assert_eq!(provider.model_id(), &ModelId::new("gpt-5-codex"));
    }
}
