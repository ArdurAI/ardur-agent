//! ardur-provider-prime — the [Prime Agent] CLI backend, driven over its RPC mode.
//!
//! The HTTP backends (Anthropic, OpenRouter, openai-compat) authenticate with an
//! API key and POST to a REST endpoint, billing per token. This backend is
//! different: it spawns the locally-installed `prime-agent` binary in its
//! line-delimited JSON **RPC mode** (`prime-agent --mode rpc`) and drives one
//! turn over the child's stdin/stdout. Authentication is inherited from
//! prime-agent's own configured provider (`prime-agent /login`, or a provider
//! entry such as `kimi-coding`), so there is no API key in this crate's config
//! and every completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport it implements the same [`Provider`]
//! trait as the HTTP backends, so the runtime dispatches to it through the
//! generic [`ProviderRegistry`] with no catalog change — which is the point:
//! a wrapped Prime Agent runs behind the full fused pipeline (cap-token verify,
//! Cedar authorization, cost gate, signed receipt, durable journal).
//!
//! # The RPC protocol (verified against prime-agent 0.9.5)
//!
//! Commands are single-line JSON objects written to stdin, each carrying a
//! `type` and a caller-chosen `id`. Every command is answered with a
//! `{"type":"response","command":<type>,"success":<bool>, …}` line; a failed
//! command carries `error` instead of `data`. Between the `prompt` response and
//! the end of the turn the child emits a stream of event lines
//! (`agent_start`, `turn_start`, `message_start`, `message_update` …,
//! `message_end`, `turn_end`), terminated by `agent_end`.
//!
//! One turn therefore costs two commands:
//!
//! 1. `{"type":"prompt","message":<transcript>,"id":"req_1"}` — acknowledged
//!    immediately, then streamed until `agent_end`.
//! 2. `{"type":"get_last_assistant_text","id":"req_2"}` — returns the final
//!    assistant text in `data.text`.
//!
//! Reading the final text from an explicit command rather than reassembling
//! `message_update` deltas keeps this layer independent of prime-agent's
//! streaming-chunk shape, which is an internal detail.
//!
//! # Auth & install requirements
//!
//! The host must have the `prime-agent` binary on `PATH` (or
//! [`PrimeConfig::binary`] pointed at it) and a usable model provider configured
//! inside prime-agent. A missing binary surfaces as [`ProviderError::Upstream`]
//! ("Prime Agent CLI not installed …"); a missing or expired login surfaces as
//! [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! | Failure                          | [`ProviderError`]                |
//! |----------------------------------|----------------------------------|
//! | binary not found on `PATH`       | [`Upstream`](ProviderError::Upstream) ("Prime Agent CLI not installed …") |
//! | no usable model / not logged in  | [`Unauthorized`](ProviderError::Unauthorized) |
//! | turn exceeded `request_timeout`  | [`NetworkFailure`](ProviderError::NetworkFailure) (its docs name timeouts) |
//! | RPC command answered `success:false` | [`Upstream`](ProviderError::Upstream) (child's `error` verbatim) |
//! | child exited before `agent_end`  | [`Upstream`](ProviderError::Upstream) (captured stderr) |
//! | turn finished with empty text    | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in this phase
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole turn
//!   is awaited before a response is returned. The event stream is already
//!   line-oriented, so a later streaming path is a change to this crate only.
//! - **Tool-call parsing** — prime-agent orchestrates its own tools inside its
//!   own process. This layer runs it with tools disabled by default
//!   ([`PrimeConfig::allow_child_tools`]) and surfaces only the final assistant
//!   text, never a [`FinishReason::ToolUse`]. Governing a child's *own* tool
//!   steps is delegation work, not provider work.
//!
//! # Attribution
//!
//! Prime Agent is MIT-licensed. See this crate's `README.md`.
//!
//! [Prime Agent]: https://github.com/PrimeIntellect-ai/prime-agent
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
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "prime";
/// The binary [`PrimeConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "prime-agent";
/// Default per-turn timeout. A Prime Agent turn can involve several model
/// round-trips, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Default [`PrimeConfig::max_tokens_floor`]. A request asking for fewer output
/// tokens than this is refused, because prime-agent cannot enforce a
/// per-completion ceiling and silently overshooting the caller's authorized
/// budget is worse than failing the request.
const DEFAULT_MAX_TOKENS_FLOOR: u32 = 4_096;
/// Upper bound on retained event lines, so a chatty turn cannot grow the audit
/// body without limit.
const MAX_RETAINED_EVENTS: usize = 2_000;
/// Upper bound on captured stderr bytes, so a child spewing diagnostics cannot
/// grow memory without limit.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Upper bound on a single stdout line. Prime Agent frames one JSON object per
/// line; a line beyond this is treated as protocol garbage rather than buffered.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Env var [`PrimeConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "PRIME_AGENT_BINARY";
/// Env var [`PrimeConfig::from_env`] reads prime-agent's provider name from.
pub const PROVIDER_ENV: &str = "PRIME_AGENT_PROVIDER";
/// Env var [`PrimeConfig::from_env`] reads the default model from.
pub const DEFAULT_MODEL_ENV: &str = "PRIME_AGENT_DEFAULT_MODEL";
/// Env var [`PrimeConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "PRIME_AGENT_WORKING_DIR";
/// Env var [`PrimeConfig::from_env`] reads the per-turn timeout from.
pub const TIMEOUT_SECS_ENV: &str = "PRIME_AGENT_TIMEOUT_SECS";
/// Env var [`PrimeConfig::from_env`] reads the max-tokens floor from.
pub const MAX_TOKENS_FLOOR_ENV: &str = "PRIME_AGENT_MAX_TOKENS_FLOOR";

/// How this backend locates and runs the `prime-agent` binary.
#[derive(Clone, Debug)]
pub struct PrimeConfig {
    /// The binary to spawn. Resolved through `PATH` when relative.
    pub binary: PathBuf,
    /// prime-agent's own provider name (its `--provider` flag), e.g.
    /// `kimi-coding`. `None` leaves prime-agent's configured default in place.
    pub provider: Option<String>,
    /// Model used when a [`CompletionRequest`] does not name one.
    pub default_model: Option<String>,
    /// Working directory for the child (its `--cwd`). `None` inherits ours.
    pub working_directory: Option<PathBuf>,
    /// Wall-clock ceiling for one turn.
    pub request_timeout: Duration,
    /// The smallest per-request `max_tokens` this backend will accept.
    ///
    /// prime-agent has no per-completion output cap, so any ceiling below this
    /// is refused rather than silently ignored (see [`Provider::complete`]).
    /// Above it, enforcement is knowingly delegated to prime-agent's own limits.
    /// Set to `0` to accept every ceiling and delegate unconditionally.
    pub max_tokens_floor: u32,
    /// Whether the child may run its own tools. `false` (the default) passes
    /// `--no-tools`: as a *completion* backend we want the model's answer, not
    /// an agent editing the filesystem outside Ardur's grant ledger. Turning
    /// this on hands the child an ungoverned tool surface — the fused pipeline
    /// cannot see steps taken inside another process.
    pub allow_child_tools: bool,
}

impl Default for PrimeConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            provider: None,
            default_model: None,
            working_directory: None,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_tokens_floor: DEFAULT_MAX_TOKENS_FLOOR,
            allow_child_tools: false,
        }
    }
}

impl PrimeConfig {
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

/// The Prime Agent CLI provider.
#[derive(Debug)]
pub struct PrimeProvider {
    config: PrimeConfig,
    rate_card: RateCard,
}

impl PrimeProvider {
    /// Build a provider from an explicit config.
    #[must_use]
    pub fn new(config: PrimeConfig) -> Self {
        Self {
            config,
            rate_card: prime_delegated_rate_card(),
        }
    }

    /// Build a provider from [`PrimeConfig::from_env`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(PrimeConfig::from_env())
    }

    /// The model this turn should run against: the request's, else the config's,
    /// else prime-agent's own default.
    fn chosen_model(&self, requested: &ModelId) -> Option<String> {
        let requested = requested.0.trim();
        if !requested.is_empty() {
            return Some(requested.to_string());
        }
        self.config.default_model.clone()
    }
}

/// What one RPC turn produced.
struct TurnOutcome {
    /// Final assistant text.
    content: String,
    /// Token counts reported by the child, when it reported any.
    usage: Usage,
    /// Retained event lines, as the audit body.
    events: Vec<serde_json::Value>,
}

#[async_trait]
impl Provider for PrimeProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let transcript = build_transcript(&req.messages);
        if transcript.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "prime-agent requires a non-empty prompt".into(),
            ));
        }

        // The caller's output-token ceiling must not be silently discarded.
        // prime-agent exposes no per-request output cap (`--autonomous-max-tokens`
        // bounds a whole autonomous run, not one completion), so a ceiling this
        // backend cannot enforce is refused rather than ignored: the fused
        // runtime would otherwise authorize N tokens, be billed for more, and
        // still see a clean `FinishReason::Stop`.
        //
        // `max_tokens_floor` is the ceiling at or above which the operator
        // accepts that enforcement is delegated to prime-agent's own limits.
        if req.max_tokens > 0 && req.max_tokens < self.config.max_tokens_floor {
            return Err(ProviderError::InvalidRequest(format!(
                "prime-agent cannot enforce a per-request output ceiling of {} tokens \
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
                "prime-agent turn exceeded {:?}",
                self.config.request_timeout
            ))
        })??;

        let cost = CostTuple {
            tokens_in: u64::from(outcome.usage.tokens_in),
            tokens_out: u64::from(outcome.usage.tokens_out),
            // Delegated billing: prime-agent's own configured provider pays for
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
        // The RPC event stream is already line-oriented; wiring it to
        // `StreamEvent`s is a later, crate-local change.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

impl PrimeProvider {
    /// Spawn the child, drive one prompt turn, and collect the final text.
    ///
    /// `model` is the already-resolved model for this turn (request's, else the
    /// config default, else `None` to leave prime-agent's own default alone).
    async fn run_turn(
        &self,
        transcript: &str,
        model: Option<&str>,
    ) -> Result<TurnOutcome, ProviderError> {
        let mut cmd = tokio::process::Command::new(&self.config.binary);
        cmd.arg("--mode").arg("rpc");
        if !self.config.allow_child_tools {
            cmd.arg("--no-tools");
        }
        if let Some(provider) = &self.config.provider {
            cmd.arg("--provider").arg(provider);
        }
        if let Some(model) = model {
            cmd.arg("--model").arg(model);
        }
        if let Some(cwd) = &self.config.working_directory {
            cmd.arg("--cwd").arg(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout this future is dropped, which drops the child;
            // kill_on_drop ensures the prime-agent process dies with it rather
            // than surviving as an orphan holding a model session open.
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Prime Agent CLI not installed (binary {:?} not found on PATH): {e}",
                    self.config.binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn prime-agent: {e}"))
            }
        })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Upstream("prime-agent child stdin was not captured".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::Upstream("prime-agent child stdout was not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ProviderError::Upstream("prime-agent child stderr was not captured".into())
        })?;

        // Drain stderr concurrently into a capped sink. A child that fills the
        // stderr pipe while we block reading stdout would deadlock, and an
        // unbounded sink would let a noisy child grow memory without limit.
        let stderr_task = tokio::spawn(async move {
            // Read into a fixed-size byte budget. Like the stdout reader, this
            // must bound the read itself: a child writing one enormous
            // newline-free diagnostic would otherwise grow the buffer without
            // limit before any cap was consulted. Draining continues past the
            // budget (a full pipe would deadlock the turn) but nothing further
            // is retained.
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

        let mut reader = BufReader::new(stdout);

        // 1. Ask for the turn. A model selection failure is reported by the
        //    child on this command's response, not by the spawn. The model is
        //    selected with the `--model` flag at spawn time, so the command body
        //    carries only the prompt.
        let prompt_cmd = serde_json::json!({
            "type": "prompt",
            "message": transcript,
            "id": "ardur_prompt",
        });
        write_command(&mut stdin, &prompt_cmd).await?;

        let mut events: Vec<serde_json::Value> = Vec::new();
        // Accumulated across every assistant message in the turn. With child
        // tools enabled one prompt can drive several model calls before
        // `agent_end`; keeping only the last record would under-report the run's
        // real token cost in the signed receipt.
        let mut usage = Usage {
            tokens_in: 0,
            tokens_out: 0,
            cost_cents: None,
        };
        let mut saw_agent_end = false;
        let mut prompt_ack = false;

        // 2. Consume the event stream until the turn ends.
        while let Some(value) = read_line_value(&mut reader).await? {
            let kind = value.get("type").and_then(serde_json::Value::as_str);
            if let Some(found) = extract_usage(&value) {
                accumulate_usage(&mut usage, found);
            }
            match kind {
                Some("response") => {
                    let command = value
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if command == "prompt" {
                        check_success(&value, "prompt", &stderr_task).await?;
                        prompt_ack = true;
                    }
                }
                Some("agent_end") => {
                    retain_event(&mut events, value);
                    saw_agent_end = true;
                    break;
                }
                _ => retain_event(&mut events, value),
            }
        }

        // Fail closed on BOTH conditions, independently. A child that emits
        // `agent_end` without ever acknowledging the prompt must not be treated
        // as a completed turn: otherwise an ignored or rejected prompt looks
        // successful as soon as the session holds any last-assistant text.
        if !saw_agent_end || !prompt_ack {
            let stderr_text = stderr_task.await.unwrap_or_default();
            if looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            let detail = stderr_text.trim();
            let stage = if !prompt_ack {
                "prime-agent ended the turn without acknowledging the prompt"
            } else {
                "prime-agent ended before the turn completed"
            };
            return Err(ProviderError::Upstream(if detail.is_empty() {
                stage.to_string()
            } else {
                format!("{stage}: {detail}")
            }));
        }

        // 3. Read the final assistant text out of the session explicitly.
        let text_cmd = serde_json::json!({
            "type": "get_last_assistant_text",
            "id": "ardur_text",
        });
        write_command(&mut stdin, &text_cmd).await?;

        let mut content = String::new();
        while let Some(value) = read_line_value(&mut reader).await? {
            if let Some(found) = extract_usage(&value) {
                accumulate_usage(&mut usage, found);
            }
            let is_text_response = value.get("type").and_then(serde_json::Value::as_str)
                == Some("response")
                && value.get("command").and_then(serde_json::Value::as_str)
                    == Some("get_last_assistant_text");
            if is_text_response {
                check_success(&value, "get_last_assistant_text", &stderr_task).await?;
                content = value
                    .get("data")
                    .and_then(|data| data.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                break;
            }
            retain_event(&mut events, value);
        }

        // Closing stdin lets the child shut down cleanly; kill_on_drop covers a
        // child that ignores EOF.
        drop(stdin);

        if content.trim().is_empty() {
            let stderr_text = stderr_task.await.unwrap_or_default();
            if looks_like_auth_error(&stderr_text) {
                return Err(ProviderError::Unauthorized);
            }
            return Err(ProviderError::Upstream(
                "prime-agent turn produced no assistant text".into(),
            ));
        }

        Ok(TurnOutcome {
            content,
            usage,
            events,
        })
    }
}

/// Append an event, stopping at [`MAX_RETAINED_EVENTS`] so the audit body stays
/// bounded for a chatty turn.
fn retain_event(events: &mut Vec<serde_json::Value>, value: serde_json::Value) {
    if events.len() < MAX_RETAINED_EVENTS {
        events.push(value);
    }
}

/// Fail a turn when an RPC command was answered `success:false`.
async fn check_success(
    value: &serde_json::Value,
    command: &str,
    stderr_task: &tokio::task::JoinHandle<String>,
) -> Result<(), ProviderError> {
    if value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let error = value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no error message")
        .to_string();
    if looks_like_auth_error(&error) {
        return Err(ProviderError::Unauthorized);
    }
    // The child may also have explained itself on stderr; it is aborting either
    // way, so this read cannot hang the happy path.
    let _ = stderr_task;
    Err(ProviderError::Upstream(format!(
        "prime-agent rejected `{command}`: {error}"
    )))
}

/// Write one newline-delimited JSON command to the child.
async fn write_command(
    stdin: &mut tokio::process::ChildStdin,
    command: &serde_json::Value,
) -> Result<(), ProviderError> {
    let mut line = serde_json::to_string(command)
        .map_err(|e| ProviderError::Upstream(format!("encoding prime-agent command: {e}")))?;
    line.push('\n');
    match stdin.write_all(line.as_bytes()).await {
        Ok(()) => {}
        // A child that already exited closes the read end, so the write races to
        // a BrokenPipe. That is not the failure to report — the missing
        // `agent_end` plus captured stderr are the source of truth.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => {
            return Err(ProviderError::Upstream(format!(
                "writing command to prime-agent stdin: {e}"
            )));
        }
    }
    stdin.flush().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            ProviderError::Upstream("prime-agent closed stdin before the command was sent".into())
        } else {
            ProviderError::Upstream(format!("flushing prime-agent stdin: {e}"))
        }
    })
}

/// Read one line and parse it as JSON.
///
/// Returns `Ok(None)` at EOF. Non-JSON lines are skipped: prime-agent prints
/// human-facing diagnostics on stdout in some startup paths, and those must not
/// abort an otherwise valid turn.
async fn read_line_value(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<Option<serde_json::Value>, ProviderError> {
    let mut line = String::new();
    loop {
        line.clear();
        // Bound the read BEFORE the delimiter is found. A plain `read_line`
        // grows the buffer until a newline or EOF, so a child that emits a huge
        // record — or never emits a newline at all — could exhaust memory long
        // before any post-hoc length check ran. `take` caps the bytes this call
        // can consume, and hitting the cap without a newline is a protocol
        // error rather than an invitation to keep buffering.
        let read = (&mut *reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line)
            .await
            .map_err(|e| ProviderError::Upstream(format!("reading prime-agent stdout: {e}")))?;
        if read == 0 {
            return Ok(None);
        }
        if read > MAX_LINE_BYTES || !line.ends_with('\n') {
            return Err(ProviderError::Upstream(
                "prime-agent emitted an oversized stdout line".into(),
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return Ok(Some(value));
        }
    }
}

/// Pull token counts out of an event, wherever the child hangs them.
///
/// prime-agent reports usage as `{"input":N,"output":M,…}` on the assistant
/// message. The object appears either at the top level or under `message`, so
/// both are checked; anything else is ignored rather than guessed at.
fn extract_usage(value: &serde_json::Value) -> Option<Usage> {
    let usage = value
        .get("usage")
        .or_else(|| value.get("message").and_then(|m| m.get("usage")))?;
    let input = usage.get("input").and_then(serde_json::Value::as_u64);
    let output = usage.get("output").and_then(serde_json::Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        tokens_in: u32::try_from(input.unwrap_or(0)).unwrap_or(u32::MAX),
        tokens_out: u32::try_from(output.unwrap_or(0)).unwrap_or(u32::MAX),
        cost_cents: None,
    })
}

/// Whether a child diagnostic describes a *credential* failure rather than a
/// general failure.
///
/// The phrases are deliberately specific. A bare `"api key"` or `"expired"`
/// substring also appears in retryable diagnostics such as
/// `rate limit exceeded for api key ...`, and classifying those as
/// [`ProviderError::Unauthorized`] would tell the caller to go fix credentials
/// that are perfectly valid — turning a transient failure into a permanent one.
/// Only missing/invalid/revoked-credential wording counts.
fn looks_like_auth_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();

    // Rate-limit and quota diagnostics frequently name the key; they are not
    // credential failures and must never be reclassified as such.
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
        "/login",
    ];
    CREDENTIAL_FAILURES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// Add one usage record into the running total for a turn.
///
/// Saturating so a pathological child cannot wrap the counters.
fn accumulate_usage(total: &mut Usage, found: Usage) {
    total.tokens_in = total.tokens_in.saturating_add(found.tokens_in);
    total.tokens_out = total.tokens_out.saturating_add(found.tokens_out);
    match (total.cost_cents, found.cost_cents) {
        (Some(a), Some(b)) => total.cost_cents = Some(a.saturating_add(b)),
        (None, Some(b)) => total.cost_cents = Some(b),
        _ => {}
    }
}

/// Flatten a chat transcript into the single prompt string the RPC `prompt`
/// command takes.
///
/// prime-agent owns its own conversation state; one Ardur turn maps onto one
/// fresh child process, so the Ardur-side history is rendered into the prompt.
/// System text leads, then the labelled dialogue.
fn build_transcript(messages: &[ardur_runtime::ChatMessage]) -> String {
    let mut systems: Vec<&str> = Vec::new();
    let mut dialogue: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => systems.push(m.content.as_str()),
            Role::User => dialogue.push(format!("User: {}", m.content)),
            Role::Assistant => dialogue.push(format!("Assistant: {}", m.content)),
            // This provider does not run the runtime's tool loop, but a replayed
            // tool result still renders as a labelled line so the transcript
            // stays intelligible rather than silently dropping content.
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

/// A zeroed rate card. Prime Agent turns are paid by prime-agent's own
/// configured provider, not metered here, so every completion is priced at zero
/// cents — the card exists only to satisfy [`Provider::rate_card`].
fn prime_delegated_rate_card() -> RateCard {
    RateCard {
        version_id: "prime-delegated-v1".to_string(),
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
    fn usage_is_read_from_either_top_level_or_message() {
        let top = serde_json::json!({"usage": {"input": 12, "output": 5}});
        let nested = serde_json::json!({"message": {"usage": {"input": 7, "output": 3}}});
        assert_eq!(extract_usage(&top).unwrap().tokens_in, 12);
        assert_eq!(extract_usage(&nested).unwrap().tokens_out, 3);
    }

    #[test]
    fn usage_absent_or_unrecognised_is_not_invented() {
        assert!(extract_usage(&serde_json::json!({"type": "turn_end"})).is_none());
        // An object under `usage` with no recognised counters must not be read
        // as "zero tokens" — that would overwrite a real reading with a guess.
        assert!(extract_usage(&serde_json::json!({"usage": {"cacheRead": 4}})).is_none());
    }

    #[test]
    fn auth_errors_are_distinguished_from_general_failures() {
        assert!(looks_like_auth_error("Error: not logged in; run /login"));
        assert!(looks_like_auth_error("API key expired"));
        assert!(!looks_like_auth_error("model produced an internal error"));
    }

    #[test]
    fn rate_limits_naming_a_key_are_not_credential_failures() {
        // Review finding: a bare "api key" substring also appears in retryable
        // diagnostics. Classifying these as Unauthorized would tell the caller
        // to fix credentials that are valid, turning a transient failure into a
        // permanent one.
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
        // Genuine credential failures still classify.
        for credential in [
            "invalid api key",
            "missing API key",
            "api key expired",
            "token revoked",
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
    fn usage_accumulates_across_multiple_assistant_messages() {
        // Review finding: with child tools enabled one prompt can drive several
        // model calls; keeping only the last record under-reports the run.
        let mut total = Usage {
            tokens_in: 0,
            tokens_out: 0,
            cost_cents: None,
        };
        accumulate_usage(
            &mut total,
            Usage {
                tokens_in: 10,
                tokens_out: 3,
                cost_cents: None,
            },
        );
        accumulate_usage(
            &mut total,
            Usage {
                tokens_in: 7,
                tokens_out: 5,
                cost_cents: None,
            },
        );
        assert_eq!(total.tokens_in, 17, "input tokens must accumulate");
        assert_eq!(total.tokens_out, 8, "output tokens must accumulate");
    }

    #[test]
    fn usage_accumulation_saturates_rather_than_wrapping() {
        let mut total = Usage {
            tokens_in: u32::MAX,
            tokens_out: 0,
            cost_cents: None,
        };
        accumulate_usage(
            &mut total,
            Usage {
                tokens_in: 100,
                tokens_out: 0,
                cost_cents: None,
            },
        );
        assert_eq!(total.tokens_in, u32::MAX);
    }

    #[test]
    fn max_tokens_floor_is_configurable_and_zero_delegates() {
        let default = PrimeConfig::default();
        assert_eq!(default.max_tokens_floor, DEFAULT_MAX_TOKENS_FLOOR);
        let delegating = PrimeConfig {
            max_tokens_floor: 0,
            ..PrimeConfig::default()
        };
        assert_eq!(delegating.max_tokens_floor, 0);
    }

    #[test]
    fn request_model_wins_over_config_default() {
        let provider = PrimeProvider::new(PrimeConfig {
            default_model: Some("config-model".into()),
            ..PrimeConfig::default()
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
    fn events_are_retained_up_to_the_cap() {
        let mut events = Vec::new();
        for _ in 0..(MAX_RETAINED_EVENTS + 50) {
            retain_event(&mut events, serde_json::json!({"type": "message_update"}));
        }
        assert_eq!(events.len(), MAX_RETAINED_EVENTS);
    }

    #[test]
    fn zero_timeout_env_does_not_produce_an_unrunnable_turn() {
        // A zero timeout would make every turn fail instantly; the default must
        // survive a bad value.
        let parsed = Some("0".to_string())
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0);
        assert!(parsed.is_none());
    }
}
