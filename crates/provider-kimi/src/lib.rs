//! ardur-provider-kimi — the [Kimi Code] CLI backend, driven as a one-shot
//! subprocess.
//!
//! The HTTP backends (Anthropic, OpenRouter, openai-compat) authenticate with
//! an API key and POST to a REST endpoint, billing per token. This backend is
//! different: it spawns the locally-installed `kimi` binary in its print
//! (one-shot) mode and reads a structured JSONL message stream from stdout.
//! Authentication is inherited from Kimi Code's own configured account
//! (`kimi login`, `~/.kimi`), so there is no API key in this crate's config
//! and every completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport it implements the same [`Provider`]
//! trait as the HTTP backends, so the runtime dispatches to it through the
//! generic [`ProviderRegistry`] with no catalog change — which is the point:
//! a wrapped Kimi Code runs behind the full fused pipeline (cap-token verify,
//! Cedar authorization, cost gate, signed receipt, durable journal).
//!
//! # The CLI protocol (verified against the installed kimi-cli 1.49.0 —
//! `cli/__init__.py`, `ui/print/__init__.py`, `ui/print/visualize.py`,
//! `agentspec.py`, and kosong's `message.py`, plus a live probe)
//!
//! One Ardur turn maps onto one child process:
//!
//! ```text
//! kimi --print --output-format stream-json [--agent-file <staged>] [-m <model>]
//! ```
//!
//! The prompt is written to stdin: with no `--prompt` argument and a piped
//! stdin, print mode reads the whole of stdin as the one-shot command and
//! exits after the turn. Every stdout JSON line is one kosong `Message`
//! object — the turn's answer is the text of the **last**
//! `{"role":"assistant","content":[{"type":"text","text":…},…]}` message
//! (`think` parts are skipped; a bare-string `content` is also tolerated).
//! `role:"tool"` result messages and plan/notification objects ride the same
//! stream and are retained for the audit body but never surfaced as the
//! answer. The stream-json vocabulary carries **no token usage**, so `Usage`
//! stays zero rather than being invented.
//!
//! Failures in print mode are reported as a **plain-text line on stdout**
//! (e.g. `Error code: 401 - {...}`) with exit code `1`, or `75`
//! (`EX_TEMPFAIL`) for retryable provider conditions (connection, timeout,
//! 429, 5xx). stderr carries logs and session hints only — so error
//! classification scans the captured stdout noise as well as stderr.
//!
//! Child tools are denied by default through Kimi Code's agent
//! specification: the child is spawned with `--agent-file` pointing at a
//! staged spec that resolves to the builtin default agent with an **empty
//! tool list** (`extend: default`, `tools: []`, `subagents: {}`). The
//! resolved agent then has no tool definitions at all, so no tool call can
//! be emitted — this is stronger than an approval deny, and it holds even
//! though `--print` auto-approves tool calls (which this crate never relies
//! on either way). That is the fail-closed default for a *completion*
//! backend.
//!
//! # Auth & install requirements
//!
//! The host must have the `kimi` binary on `PATH` (or
//! [`KimiConfig::binary`] pointed at it) and a usable account configured in
//! `~/.kimi` (`kimi login`). A missing binary surfaces as
//! [`ProviderError::Upstream`] ("Kimi Code CLI not installed …"); a missing
//! or expired login surfaces as [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! | Failure | [`ProviderError`] |
//! |----------------------------------|----------------------------------|
//! | binary not found on `PATH` | [`Upstream`](ProviderError::Upstream) ("Kimi Code CLI not installed …") |
//! | no usable account / expired login | [`Unauthorized`](ProviderError::Unauthorized) |
//! | turn exceeded `request_timeout` | [`NetworkFailure`](ProviderError::NetworkFailure) |
//! | exit 75 (retryable upstream: connection/timeout/429/5xx) | [`NetworkFailure`](ProviderError::NetworkFailure) (or [`RateLimited`](ProviderError::RateLimited) when the diagnostic names a quota) |
//! | non-zero exit / error line | [`Upstream`](ProviderError::Upstream) (stderr / stdout, secret-redacted) |
//! | turn finished with empty text | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in this phase
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole
//!   turn is awaited before a response is returned. The message feed is
//!   already line-oriented, so a later streaming path is a change to this
//!   crate only.
//! - **Tool-call parsing** — Kimi Code orchestrates its own tools inside its
//!   own process. This layer runs it with tools denied by default
//!   ([`KimiConfig::allow_child_tools`]) and surfaces only the final
//!   assistant text, never a [`FinishReason::ToolUse`]. Governing a child's
//!   *own* tool steps is delegation work, not provider work.
//! - **Selector registration** — selected at boot via `ARDUR_PROVIDER=kimi`
//!   (alias `kimi-agent`) in `provider-selector::from_env` →
//!   [`KimiProvider::from_env`].
//!
//! # Attribution
//!
//! Kimi Code CLI is published by Moonshot AI. See this crate's `README.md`.
//!
//! [Kimi Code]: https://github.com/MoonshotAI/kimi-cli
//! [`CostTuple`]: ardur_runtime::CostTuple
//! [`ProviderRegistry`]: ardur_provider_runtime::ProviderRegistry
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
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
const PROVIDER_ID: &str = "kimi";
/// The binary [`KimiConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "kimi";
/// Default per-turn timeout. A Kimi turn can involve several model
/// round-trips, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default [`KimiConfig::max_tokens_floor`]. A request asking for fewer
/// output tokens than this is refused, because Kimi Code cannot enforce a
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
/// Upper bound on retained non-JSON stdout noise bytes (banner text and, on
/// failure, the child's plain-text error line), used for classification.
const MAX_NOISE_BYTES: usize = 16 * 1024;
/// Maximum spawn retries when Linux returns `ETXTBSY` ("Text file busy"). See
/// [`spawn_kimi`] for why the retry exists.
const SPAWN_ETXTBSY_RETRIES: u32 = 6;
/// Kimi Code's retryable exit code (`EX_TEMPFAIL` from sysexits.h), returned
/// by print mode for connection errors, timeouts, HTTP 429, and HTTP 5xx
/// (`ui/print/__init__.py::_classify_provider_error`).
const EXIT_RETRYABLE: i32 = 75;

/// Agent specification staged for the child when child tools are denied (the
/// default). `extend: default` resolves against Kimi Code's *builtin* default
/// agent (not a project file), and the explicit `tools: []` overrides the
/// inherited list during spec resolution, so the agent the child actually
/// builds has no tool definitions at all — no tool call can be emitted, which
/// is why `--print`'s auto-approval of tool calls is moot under this spec.
/// `subagents: {}` drops the default subagent registry for the same reason
/// (it is only reachable through the `Agent` tool, but an empty registry is
/// the honest reading of "no tools").
const DENY_ALL_AGENT_SPEC: &str = "version: 1\nagent:\n  extend: default\n  name: ardur-completion\n  tools: []\n  subagents: {}\n";

/// Env var [`KimiConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "KIMI_BINARY";
/// Env var [`KimiConfig::from_env`] reads the default model from. Kimi Code
/// models are named by its config aliases / model ids (its `--model` flag).
pub const DEFAULT_MODEL_ENV: &str = "KIMI_DEFAULT_MODEL";
/// Env var [`KimiConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "KIMI_WORKING_DIR";
/// Env var [`KimiConfig::from_env`] reads the per-turn timeout from.
pub const TIMEOUT_SECS_ENV: &str = "KIMI_TIMEOUT_SECS";
/// Env var [`KimiConfig::from_env`] reads the max-tokens floor from.
pub const MAX_TOKENS_FLOOR_ENV: &str = "KIMI_MAX_TOKENS_FLOOR";

/// How this backend locates and runs the `kimi` binary.
#[derive(Clone, Debug)]
pub struct KimiConfig {
    /// The binary to spawn. Resolved through `PATH` when relative.
    pub binary: PathBuf,
    /// Model used when a [`CompletionRequest`] does not name one, in Kimi
    /// Code's own naming. `None` leaves Kimi Code's configured default model
    /// in place.
    pub default_model: Option<String>,
    /// Working directory for the child. `None` inherits ours.
    pub working_directory: Option<PathBuf>,
    /// Wall-clock ceiling for one turn.
    pub request_timeout: Duration,
    /// The smallest per-request `max_tokens` this backend will accept.
    ///
    /// Kimi Code has no per-completion output cap, so any ceiling below this
    /// is refused rather than silently ignored (see [`Provider::complete`]).
    /// Above it, enforcement is knowingly delegated to Kimi Code's own
    /// limits. Set to `0` to accept every ceiling and delegate
    /// unconditionally.
    pub max_tokens_floor: u32,
    /// Whether the child may run its own tools. `false` (the default) spawns
    /// the child with a staged `--agent-file` whose resolved tool list is
    /// empty (see [`DENY_ALL_AGENT_SPEC`]): as a *completion* backend we want
    /// the model's answer, not an agent editing the filesystem outside
    /// Ardur's grant ledger. Turning this on omits `--agent-file` and hands
    /// the child the operator's own configured default agent — the fused
    /// pipeline cannot see steps taken inside another process.
    pub allow_child_tools: bool,
}

impl Default for KimiConfig {
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

impl KimiConfig {
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

/// The Kimi Code CLI provider.
#[derive(Debug)]
pub struct KimiProvider {
    config: KimiConfig,
    rate_card: RateCard,
}

impl KimiProvider {
    /// Build a provider from an explicit config.
    #[must_use]
    pub fn new(config: KimiConfig) -> Self {
        Self {
            config,
            rate_card: kimi_delegated_rate_card(),
        }
    }

    /// Build a provider from [`KimiConfig::from_env`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(KimiConfig::from_env())
    }

    /// The model this turn should run against: the request's, else the
    /// config's, else Kimi Code's own default.
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
    /// Final assistant text (the last assistant message's text parts).
    content: String,
    /// Token counts. Kimi Code's stream-json vocabulary carries no usage
    /// events, so this stays zero rather than being invented.
    usage: Usage,
    /// Retained message lines, as the audit body.
    events: Vec<serde_json::Value>,
}

#[async_trait]
impl Provider for KimiProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let transcript = build_transcript(&req.messages);
        if transcript.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "kimi requires a non-empty prompt".into(),
            ));
        }
        // The caller's output-token ceiling must not be silently discarded.
        // `kimi --print` exposes no per-request output cap, so a ceiling this
        // backend cannot enforce is refused rather than ignored: the fused
        // runtime would otherwise authorize N tokens, be billed for more, and
        // still see a clean `FinishReason::Stop`.
        //
        // `max_tokens_floor` is the ceiling at or above which the operator
        // accepts that enforcement is delegated to Kimi Code's own limits.
        // Pass 0 to acknowledge the ceiling is delegated unconditionally.
        if req.max_tokens > 0 && req.max_tokens < self.config.max_tokens_floor {
            return Err(ProviderError::InvalidRequest(format!(
                "kimi --print cannot enforce a per-request output ceiling of {} tokens \
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
                "kimi turn exceeded {:?}",
                self.config.request_timeout
            ))
        })??;

        let cost = CostTuple {
            tokens_in: u64::from(outcome.usage.tokens_in),
            tokens_out: u64::from(outcome.usage.tokens_out),
            // Delegated billing: Kimi Code's own configured account pays for
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
        // The JSONL message feed is already line-oriented; wiring it to
        // `StreamEvent`s is a later, crate-local change.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// Counter making each staged deny-spec filename unique within this process.
static DENY_SPEC_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Stage the deny-all agent specification as a uniquely-named file in `dir`
/// and return its path. The caller deletes the file after the turn; a leaked
/// file is harmless (the OS reaps the temp dir) but a name collision is not,
/// so names carry the pid and a monotonic counter.
fn write_deny_agent_spec(dir: &Path) -> std::io::Result<PathBuf> {
    let seq = DENY_SPEC_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "ardur-kimi-deny-agent-{}-{seq}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, DENY_ALL_AGENT_SPEC)?;
    Ok(path)
}

impl KimiProvider {
    /// Spawn the child, feed the prompt on stdin, and parse the JSONL message
    /// stream from stdout.
    async fn run_turn(
        &self,
        transcript: &str,
        model: Option<&str>,
    ) -> Result<TurnOutcome, ProviderError> {
        let mut cmd = tokio::process::Command::new(&self.config.binary);
        cmd.arg("--print")
            // Structured JSONL for programmatic consumption.
            .arg("--output-format")
            .arg("stream-json");
        if let Some(model) = model {
            cmd.arg("-m").arg(model);
        }
        // Tools denied (the default): stage the empty-tools agent spec and
        // point the child at it. A staging failure must fail the turn *before
        // spawn* — running without the spec would hand the child the default
        // agent's full tool list under `--print`'s auto-approval.
        let deny_spec = if self.config.allow_child_tools {
            None
        } else {
            let path = write_deny_agent_spec(&std::env::temp_dir()).map_err(|e| {
                ProviderError::Upstream(format!(
                    "cannot stage the deny-by-default agent spec (refusing to run kimi with tools enabled): {e}"
                ))
            })?;
            cmd.arg("--agent-file").arg(&path);
            Some(path)
        };
        if let Some(cwd) = &self.config.working_directory {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout this future is dropped, which drops the child;
            // kill_on_drop ensures the kimi process dies with it rather
            // than surviving as an orphan holding a model session open.
            .kill_on_drop(true);

        let mut child = spawn_kimi(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Kimi Code CLI not installed (binary {:?} not found on PATH): {e}",
                    self.config.binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn kimi: {e}"))
            }
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Upstream("kimi child stdin was not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Upstream("kimi child stdout was not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderError::Upstream("kimi child stderr was not captured".into()))?;

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
            // report — the exit status and captured stdout/stderr are the
            // source of truth.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                return Err(ProviderError::Upstream(format!(
                    "writing prompt to kimi stdin: {e}"
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
                    return Err(ProviderError::Upstream(format!("reading kimi stdout: {e}")));
                }
                Ok(n) => {
                    if stdout_buf.len() >= MAX_STDOUT_BYTES {
                        return Err(ProviderError::Upstream(format!(
                            "kimi stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                    let room = MAX_STDOUT_BYTES - stdout_buf.len();
                    stdout_buf.extend_from_slice(&chunk[..n.min(room)]);
                    if n > room {
                        return Err(ProviderError::Upstream(format!(
                            "kimi stdout exceeded the {MAX_STDOUT_BYTES}-byte bound"
                        )));
                    }
                }
            }
        }
        let stdout_text = String::from_utf8_lossy(&stdout_buf).into_owned();

        let status = child
            .wait()
            .await
            .map_err(|e| ProviderError::Upstream(format!("waiting on kimi subprocess: {e}")))?;
        let stderr_text = stderr_task.await.unwrap_or_default();
        // The staged deny spec is only needed while the child runs; remove it
        // once the child has exited. Best-effort: a leftover file in the temp
        // dir is harmless, and the next turn stages a fresh unique name.
        if let Some(path) = &deny_spec {
            let _ = std::fs::remove_file(path);
        }

        let parsed = parse_events(&stdout_text);

        if !status.success() {
            // Kimi Code prints its failure line to *stdout* (plain text) and
            // keeps logs/session hints on stderr, so both are scanned and the
            // noisier stdout text is a first-class diagnostic here.
            let detail = if !stderr_text.trim().is_empty() {
                stderr_text.trim().to_string()
            } else if !parsed.noise.trim().is_empty() {
                parsed.noise.trim().to_string()
            } else {
                "kimi exited with a non-zero status".to_string()
            };
            // Classify stderr and stdout noise independently so an unrelated
            // warning cannot mask an auth/rate-limit result.
            if looks_like_auth_error(&detail)
                || looks_like_auth_error(&stderr_text)
                || looks_like_auth_error(&parsed.noise)
            {
                return Err(ProviderError::Unauthorized);
            }
            if looks_like_rate_limit(&detail)
                || looks_like_rate_limit(&stderr_text)
                || looks_like_rate_limit(&parsed.noise)
            {
                return Err(ProviderError::RateLimited { retry_after_ms: 0 });
            }
            if status.code() == Some(EXIT_RETRYABLE) {
                // EX_TEMPFAIL: the child already classified this as a
                // retryable upstream condition (connection, timeout, 5xx;
                // 429/quota was caught above). Map to the taxonomy's
                // transient slot rather than a hard upstream failure.
                return Err(ProviderError::NetworkFailure(format!(
                    "kimi reported a retryable upstream condition (exit {EXIT_RETRYABLE}): {}",
                    redact_child_diagnostic(&detail)
                )));
            }
            let code = status.code();
            return Err(classify_child_failure(&detail, code.map(i64::from)));
        }

        if parsed.content.trim().is_empty() {
            if looks_like_auth_error(&stderr_text) || looks_like_auth_error(&parsed.noise) {
                return Err(ProviderError::Unauthorized);
            }
            return Err(ProviderError::Upstream(
                "kimi turn produced no assistant text".into(),
            ));
        }

        Ok(TurnOutcome {
            content: parsed.content,
            usage: parsed.usage,
            events: parsed.events,
        })
    }
}

/// Fields pulled out of a `kimi --print --output-format stream-json` stdout
/// stream.
struct ParsedOutput {
    /// The last assistant message's joined text — the turn's final answer.
    content: String,
    /// Always zero: the stream-json vocabulary reports no token usage.
    usage: Usage,
    /// Non-JSON stdout lines (banner noise on success, the plain-text error
    /// line on failure), retained for classification. Capped.
    noise: String,
    /// Retained message lines, as the audit body.
    events: Vec<serde_json::Value>,
}

/// Parse the JSONL message stream Kimi Code writes to stdout. Non-JSON lines
/// are skipped rather than failing the whole parse (stray banner noise must
/// not abort an otherwise valid turn) but retained in `noise`, because print
/// mode reports failures as plain-text stdout lines.
fn parse_events(stdout: &str) -> ParsedOutput {
    let mut content = String::new();
    let usage = Usage::default();
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
        if value.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
            // The final assistant message is the turn's answer. Content is a
            // list of parts in the full printer (text parts joined, `think`
            // parts skipped); a bare string is tolerated for the
            // final-message-only shape.
            match value.get("content") {
                Some(serde_json::Value::String(text)) => {
                    content = text.clone();
                }
                Some(serde_json::Value::Array(parts)) => {
                    let mut joined = String::new();
                    for part in parts {
                        if part.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                            {
                                joined.push_str(text);
                            }
                        }
                    }
                    content = joined;
                }
                _ => {}
            }
        }
        if events.len() < MAX_RETAINED_EVENTS {
            events.push(value);
        }
    }

    ParsedOutput {
        content,
        usage,
        noise,
        events,
    }
}

/// Errno for Linux's `ETXTBSY` ("Text file busy"). macOS never returns it.
const ETXTBSY: i32 = 26;

/// Whether a spawn error is `ETXTBSY` ("Text file busy").
fn is_etxtbsy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy || e.raw_os_error() == Some(ETXTBSY)
}

/// Spawn the kimi subprocess, retrying briefly on `ETXTBSY` ("Text file busy").
///
/// Mirrors the provider-codex / provider-claude-cli / provider-hermes /
/// provider-opencode spawn retry: parallel tests that write an executable
/// shim then exec it can race an inherited writable fd across `fork`→`execve`
/// on Linux.
async fn spawn_kimi(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
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
/// [`ProviderError`]. Kimi Code (or a shim) may echo an API key in stderr / a
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
            ProviderError::Upstream(format!("kimi exited with status {code}: {redacted}"))
        }
        None => ProviderError::Upstream(format!("kimi reported an error: {redacted}")),
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

    const CREDENTIAL_FAILURES: [&str; 19] = [
        "not logged in",
        "no api key",
        "missing api key",
        "invalid api key",
        "incorrect api key",
        "api key expired",
        "api key appears to be invalid",
        "revoked",
        "unauthorized",
        "unauthenticated",
        "authentication failed",
        "invalid_authentication",
        "invalid_api_key",
        "error code: 401",
        "status 401",
        "credentials",
        "auth login",
        "kimi login",
        "provider auth",
    ];
    CREDENTIAL_FAILURES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Flatten a chat transcript into the single prompt string Kimi Code reads on
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

/// A zeroed rate card. Kimi Code turns are paid by Kimi Code's own configured
/// account, not metered here, so every completion is priced at zero cents —
/// the card exists only to satisfy [`Provider::rate_card`].
fn kimi_delegated_rate_card() -> RateCard {
    RateCard {
        version_id: "kimi-delegated-v1".to_string(),
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
    fn parse_extracts_last_assistant_text_and_retains_events() {
        let stdout = concat!(
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"SHIM-OK\"}]}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"think\",\"think\":\"hmm\"},{\"type\":\"text\",\"text\":\"FINAL\"}]}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "FINAL");
        assert_eq!(parsed.events.len(), 2);
        assert!(parsed.noise.is_empty());
        assert_eq!(parsed.usage.tokens_in, 0, "stream-json reports no usage");
        assert_eq!(parsed.usage.tokens_out, 0);
    }

    #[test]
    fn parse_joins_multiple_text_parts_in_one_message() {
        let stdout = "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Hello, \"},{\"type\":\"text\",\"text\":\"world.\"}]}\n";
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "Hello, world.");
    }

    #[test]
    fn parse_tolerates_bare_string_content() {
        // The final-message-only JSON printer emits content as a plain string.
        let stdout = "{\"role\":\"assistant\",\"content\":\"plain final text\"}\n";
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "plain final text");
    }

    #[test]
    fn parse_ignores_tool_and_user_messages_for_the_answer() {
        let stdout = concat!(
            "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"the prompt\"}]}\n",
            "{\"role\":\"tool\",\"content\":[{\"type\":\"text\",\"text\":\"tool output\"}]}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"the answer\"}]}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "the answer");
        assert_eq!(parsed.events.len(), 3);
    }

    #[test]
    fn parse_retains_non_json_noise_for_classification() {
        let stdout = concat!(
            "Error code: 401 - {'error': {'type': 'invalid_authentication_error'}}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}\n",
        );
        let parsed = parse_events(stdout);
        assert_eq!(parsed.content, "ok");
        assert!(
            parsed.noise.contains("Error code: 401"),
            "the plain-text error line must be retained: {:?}",
            parsed.noise
        );
    }

    #[test]
    fn auth_errors_are_distinguished_from_general_failures() {
        // The exact shape observed from the real CLI on an expired login.
        assert!(looks_like_auth_error(
            "Error code: 401 - {'error': {'message': 'The API Key appears to be invalid or may have expired.', 'type': 'invalid_authentication_error'}}"
        ));
        assert!(looks_like_auth_error("invalid_api_key"));
        assert!(looks_like_auth_error("authentication failed"));
        assert!(looks_like_auth_error("run kimi login"));
        assert!(!looks_like_auth_error("model produced an internal error"));
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
        // A long fake key of the shape the redaction set masks (\bsk-[a-z0-9_-]{16,}).
        let raw = "Error code: 500 for API key sk-abcdefghijklmnopqrstuvwxyz012345";
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
            redacted.contains("Error code: 500"),
            "non-secret text must remain: {redacted}"
        );
    }

    #[test]
    fn request_model_wins_over_config_default() {
        let provider = KimiProvider::new(KimiConfig {
            default_model: Some("config-model".into()),
            ..KimiConfig::default()
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
    fn provider_id_is_kimi_and_not_streaming() {
        let provider = KimiProvider::new(KimiConfig::default());
        assert_eq!(provider.id(), ProviderId("kimi".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.rate_card().version_id, "kimi-delegated-v1");
    }

    #[test]
    fn default_config_denies_child_tools() {
        assert!(!KimiConfig::default().allow_child_tools);
    }

    #[test]
    fn deny_agent_spec_extends_default_with_an_empty_tool_list() {
        // The staged spec must resolve to the builtin default agent with no
        // tools and no subagents; these are the exact keys kimi-cli's
        // agentspec resolution honors (verified against 1.49.0).
        assert!(DENY_ALL_AGENT_SPEC.contains("extend: default"));
        assert!(DENY_ALL_AGENT_SPEC.contains("tools: []"));
        assert!(DENY_ALL_AGENT_SPEC.contains("subagents: {}"));
        assert!(DENY_ALL_AGENT_SPEC.starts_with("version: 1\n"));
    }

    #[test]
    fn deny_agent_spec_stages_a_uniquely_named_file() {
        let dir = std::env::temp_dir();
        let first = write_deny_agent_spec(&dir).expect("stage first");
        let second = write_deny_agent_spec(&dir).expect("stage second");
        assert_ne!(first, second, "names must not collide across turns");
        let staged = std::fs::read_to_string(&first).expect("read staged spec");
        assert_eq!(staged, DENY_ALL_AGENT_SPEC);
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }

    #[test]
    fn deny_agent_spec_staging_failure_is_an_error_not_a_fallback() {
        // Staging into a directory that does not exist must fail: the
        // provider turns this into a pre-spawn error rather than running the
        // child with the default (tool-enabled) agent.
        let missing = Path::new("/nonexistent/ardur-kimi-no-such-dir");
        assert!(write_deny_agent_spec(missing).is_err());
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
        let default = KimiConfig::default();
        assert_eq!(default.max_tokens_floor, DEFAULT_MAX_TOKENS_FLOOR);
        let delegating = KimiConfig {
            max_tokens_floor: 0,
            ..KimiConfig::default()
        };
        assert_eq!(delegating.max_tokens_floor, 0);
    }
}
