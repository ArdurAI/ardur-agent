//! ardur-provider-claude-cli — the [Claude Code] CLI subscription backend (§3.3c).
//!
//! The HTTP backends (Anthropic §3.1, OpenRouter §3.2) authenticate with an API
//! key and POST to a REST endpoint, billing per token. This backend is
//! different — like the §3.3b Codex backend, it wraps the locally-installed
//! `claude` (Claude Code) CLI as a **subprocess**, running it non-interactively
//! via `claude -p --output-format json`. Authentication is inherited from
//! `claude login` (a logged-in Anthropic subscription), so a dogfooding turn
//! spends the subscription's **Agent SDK Credit pool** rather than a metered API
//! key — there is no API key in this crate's config, and every completion is
//! priced at **zero cents** (see [`CostTuple`]).
//!
//! > **Billing note.** As of 2026-06-15 Anthropic moved Agent SDK + `claude -p`
//! > usage onto a separate "Agent SDK Credit pool" with a fixed $20–$200/month
//! > allotment (plan-dependent), billed at API rates. Subscription-via-CLI is
//! > therefore *not* unbounded — see the crate README.
//!
//! Despite the very different transport, it implements the same §3.0
//! [`Provider`] trait as the HTTP backends, so the runtime dispatches to it
//! through the generic [`ProviderRegistry`] with no catalog change.
//!
//! # How a turn maps onto the CLI
//!
//! - The request's [`messages`](CompletionRequest::messages) are flattened into
//!   a single prompt transcript (system text on top, then `User:` / `Assistant:`
//!   turns) and piped to the subprocess on stdin. Claude Code is an agentic
//!   prompt surface, not a chat-completions endpoint, so this is a lossy P1
//!   rendering — see the `build_transcript` TODO for Phase 2.
//! - The chosen model is passed with `--model`: the request's [`ModelId`] wins,
//!   else the config's [`default_model`](ClaudeCliConfig::default_model), else
//!   the CLI's own default (a Sonnet 4.x).
//! - `claude -p --output-format json` writes a JSON value to stdout — in current
//!   CLI versions an **array** of stream events ending in a `result` object,
//!   though a single `result` object is also accepted. The `result` object's
//!   `result` field is the answer text and `usage.input_tokens` /
//!   `usage.output_tokens` carry the token counts.
//!
//! # Auth & install requirements
//!
//! The host must have the `claude` CLI on `PATH` (or
//! [`ClaudeCliConfig::claude_binary`] pointed at it) and an active session from
//! `claude login`. A missing binary surfaces as [`ProviderError::Upstream`]
//! ("Claude CLI not installed …"); a missing/expired login surfaces as
//! [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! The shared [`ProviderError`] enum has no `ConfigError`/`Timeout`/
//! `InvalidResponse` variants, so the §3.3c failure classes are mapped onto the
//! closest existing ones (matching the §3.3b codex precedent):
//!
//! | Failure                            | [`ProviderError`]                |
//! |------------------------------------|----------------------------------|
//! | binary not found on `PATH`         | [`Upstream`](ProviderError::Upstream) ("Claude CLI not installed …") |
//! | not logged in (`claude login`)     | [`Unauthorized`](ProviderError::Unauthorized) |
//! | Agent SDK Credit pool exhausted    | [`RateLimited`](ProviderError::RateLimited) |
//! | run exceeded `request_timeout`     | [`NetworkFailure`](ProviderError::NetworkFailure) (its docs name timeouts) |
//! | non-zero exit + stderr             | [`Upstream`](ProviderError::Upstream) (stderr verbatim) |
//! | success but unparseable output     | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in Phase 1
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole
//!   subprocess is awaited before a response is returned. Phase 2 is
//!   `--output-format stream-json`.
//! - **Tool-call parsing** — Claude Code runs tools inside its own session; this
//!   layer only surfaces the final assistant text, never a
//!   [`FinishReason::ToolUse`].
//! - **Rich message handling** — the flattened transcript loses turn structure.
//!
//! [Claude Code]: https://claude.com/code
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
use tokio::io::AsyncWriteExt;

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "claude-cli";
/// The binary [`ClaudeCliConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "claude";
/// Default per-run timeout — a `claude -p` turn can take a while.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Maximum spawn retries when Linux returns `ETXTBSY` ("Text file busy"). See
/// [`spawn_claude`] for why the retry exists.
const SPAWN_ETXTBSY_RETRIES: u32 = 6;

/// Env var [`ClaudeCliConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "CLAUDE_CLI_BINARY";
/// Env var [`ClaudeCliConfig::from_env`] reads the default model from.
pub const DEFAULT_MODEL_ENV: &str = "CLAUDE_CLI_DEFAULT_MODEL";
/// Env var [`ClaudeCliConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "CLAUDE_CLI_WORKING_DIR";
/// Env var [`ClaudeCliConfig::from_env`] reads the `--allowedTools` value from.
pub const ALLOWED_TOOLS_ENV: &str = "CLAUDE_CLI_ALLOWED_TOOLS";
/// Env var [`ClaudeCliConfig::from_env`] reads the permission mode from.
pub const PERMISSION_MODE_ENV: &str = "CLAUDE_CLI_PERMISSION_MODE";

/// The permission policy `claude -p` runs model-initiated tool calls under
/// (the `--permission-mode` flag).
///
/// Because this provider is used as a *text-completion* backend (we want the
/// model's answer, not file edits or shell runs), the default is the
/// most-restrictive [`Default`](PermissionMode::Default): in headless `-p` mode
/// it never prompts — tools requiring approval are simply declined rather than
/// stalling the subprocess on an interactive prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// `default` — standard policy; tools needing approval are declined in
    /// headless mode (never prompts). The safe default for a completion backend.
    #[default]
    Default,
    /// `acceptEdits` — auto-accept file-edit tool calls.
    AcceptEdits,
    /// `auto` — auto-approve tool calls the CLI deems safe.
    Auto,
    /// `bypassPermissions` — skip all permission checks. Use only in an
    /// externally-sandboxed environment.
    BypassPermissions,
    /// `dontAsk` — proceed without asking for confirmations.
    DontAsk,
    /// `plan` — planning mode; the model plans without executing tools.
    Plan,
}

impl PermissionMode {
    /// The `--permission-mode` flag value the CLI expects.
    #[must_use]
    pub fn as_flag(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Auto => "auto",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Plan => "plan",
        }
    }

    /// Parse a [`PERMISSION_MODE_ENV`] value, tolerating the canonical camelCase
    /// spelling (`acceptEdits`), kebab/underscore spellings (`accept-edits`,
    /// `accept_edits`), and case. Returns `None` for an unrecognized value so the
    /// caller can keep the default.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], "")
            .as_str()
        {
            "default" => Some(PermissionMode::Default),
            "acceptedits" => Some(PermissionMode::AcceptEdits),
            "auto" => Some(PermissionMode::Auto),
            "bypasspermissions" | "bypass" => Some(PermissionMode::BypassPermissions),
            "dontask" => Some(PermissionMode::DontAsk),
            "plan" => Some(PermissionMode::Plan),
            _ => None,
        }
    }
}

/// How a [`ClaudeCliProvider`] invokes the `claude` CLI.
///
/// Build it with [`ClaudeCliConfig::new`] (or [`ClaudeCliConfig::from_env`]) and
/// tune the optional fields with the builder methods. There is **no API key** —
/// auth is inherited from `claude login`.
#[derive(Clone, Debug)]
pub struct ClaudeCliConfig {
    claude_binary: PathBuf,
    default_model: Option<String>,
    working_directory: Option<PathBuf>,
    allowed_tools: Option<String>,
    permission_mode: PermissionMode,
    request_timeout: Duration,
}

impl Default for ClaudeCliConfig {
    fn default() -> Self {
        Self {
            claude_binary: PathBuf::from(DEFAULT_BINARY),
            default_model: None,
            working_directory: None,
            allowed_tools: None,
            permission_mode: PermissionMode::default(),
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

impl ClaudeCliConfig {
    /// A config that resolves `claude` through `PATH`, with no default model, the
    /// most-restrictive [`PermissionMode::Default`], and the default 5-minute
    /// timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a config from the environment.
    ///
    /// Reads [`BINARY_ENV`], [`DEFAULT_MODEL_ENV`], [`WORKING_DIR_ENV`],
    /// [`ALLOWED_TOOLS_ENV`], and [`PERMISSION_MODE_ENV`]; any unset/empty/
    /// unparseable value falls back to its default. This is **infallible** —
    /// unlike the HTTP backends there is no API key to be missing, so there is
    /// nothing here that can fail (a missing `claude login` is only discovered
    /// when a turn actually runs).
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::new();
        if let Ok(bin) = std::env::var(BINARY_ENV) {
            if !bin.is_empty() {
                cfg.claude_binary = PathBuf::from(bin);
            }
        }
        if let Ok(model) = std::env::var(DEFAULT_MODEL_ENV) {
            if !model.is_empty() {
                cfg.default_model = Some(model);
            }
        }
        if let Ok(dir) = std::env::var(WORKING_DIR_ENV) {
            if !dir.is_empty() {
                cfg.working_directory = Some(PathBuf::from(dir));
            }
        }
        if let Ok(tools) = std::env::var(ALLOWED_TOOLS_ENV) {
            if !tools.is_empty() {
                cfg.allowed_tools = Some(tools);
            }
        }
        if let Ok(mode) = std::env::var(PERMISSION_MODE_ENV) {
            if let Some(parsed) = PermissionMode::parse(&mode) {
                cfg.permission_mode = parsed;
            }
        }
        cfg
    }

    /// Override the path to the `claude` binary.
    #[must_use]
    pub fn claude_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_binary = path.into();
        self
    }

    /// Set the model passed with `--model` when a request does not name its own.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Set the working directory the CLI runs in. Claude Code uses its cwd for
    /// skill/file context; when unset, it inherits this process's.
    #[must_use]
    pub fn working_directory(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// Set the `--allowedTools` value passed for permissive non-interactive runs
    /// (e.g. `"Bash(git *) Edit"`). When unset, no `--allowedTools` flag is
    /// emitted.
    #[must_use]
    pub fn allowed_tools(mut self, tools: impl Into<String>) -> Self {
        self.allowed_tools = Some(tools.into());
        self
    }

    /// Override the permission mode (`--permission-mode`).
    #[must_use]
    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Override the per-run timeout.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// The Claude Code CLI subscription provider.
///
/// Construct it with [`ClaudeCliProvider::new`] (from a [`ClaudeCliConfig`] and a
/// default model) or [`ClaudeCliProvider::from_env`]. The model on each
/// [`CompletionRequest`] selects which model the CLI runs; `model_id` is only the
/// default the runtime stamps onto a request.
pub struct ClaudeCliProvider {
    config: ClaudeCliConfig,
    model_id: ModelId,
    rate_card: RateCard,
}

impl ClaudeCliProvider {
    /// Build a provider from `config` with a default `model_id`.
    #[must_use]
    pub fn new(config: ClaudeCliConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: claude_subscription_rate_card(),
        }
    }

    /// Build a provider with a default `model_id`, reading the config from the
    /// environment ([`ClaudeCliConfig::from_env`]).
    #[must_use]
    pub fn from_env(model_id: ModelId) -> Self {
        Self::new(ClaudeCliConfig::from_env(), model_id)
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// The model to pass the CLI for this request: the request's own model when
    /// non-empty, else the config default. `None` lets the CLI pick its own
    /// default (no `--model` flag emitted).
    fn chosen_model<'a>(&'a self, req_model: &'a ModelId) -> Option<&'a str> {
        let requested = req_model.0.trim();
        if !requested.is_empty() {
            Some(requested)
        } else {
            self.config.default_model.as_deref()
        }
    }
}

#[async_trait]
impl Provider for ClaudeCliProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let prompt = build_transcript(&req.messages);

        let mut cmd = tokio::process::Command::new(&self.config.claude_binary);
        cmd.arg("-p")
            // Stable, parseable stdout (JSON value, not the TUI).
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg(self.config.permission_mode.as_flag());
        if let Some(model) = self.chosen_model(&req.model) {
            cmd.arg("--model").arg(model);
        }
        if let Some(tools) = &self.config.allowed_tools {
            cmd.arg("--allowedTools").arg(tools);
        }
        if let Some(cwd) = &self.config.working_directory {
            // Claude Code keys its skill/file context off the process cwd.
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout the `wait_with_output` future is dropped, which drops
            // the child; kill_on_drop ensures the claude process dies with it.
            .kill_on_drop(true);

        let mut child = spawn_claude(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Claude CLI not installed (binary {:?} not found on PATH). \
                     Install from claude.com/code. ({e})",
                    self.config.claude_binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn claude: {e}"))
            }
        })?;

        // Feed the prompt on stdin, then close it so claude sees EOF and starts.
        // P1 prompts are small, so a full write before reading stdout cannot
        // deadlock; Phase 2's streaming path will interleave the two.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Upstream("claude child stdin was not captured".into()))?;
        match stdin.write_all(prompt.as_bytes()).await {
            Ok(()) => {}
            // A child that exits before draining stdin (e.g. it failed fast on a
            // bad flag, or is already done) closes the read end, so the write
            // races to a BrokenPipe. That is not our failure to report — let the
            // exit status and captured stderr be the source of truth.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                return Err(ProviderError::Upstream(format!(
                    "writing prompt to claude stdin: {e}"
                )));
            }
        }
        drop(stdin);

        let output =
            match tokio::time::timeout(self.config.request_timeout, child.wait_with_output()).await
            {
                Ok(Ok(output)) => output,
                Ok(Err(e)) => {
                    return Err(ProviderError::Upstream(format!(
                        "waiting on claude subprocess: {e}"
                    )));
                }
                Err(_elapsed) => {
                    return Err(ProviderError::NetworkFailure(format!(
                        "claude -p timed out after {}s",
                        self.config.request_timeout.as_secs()
                    )));
                }
            };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(classify_failure(&stderr, output.status.code()));
        }

        let parsed = parse_claude_output(&stdout)?;

        // An is_error result with a zero exit code (the CLI sometimes reports the
        // failure in-band rather than via the exit status) is classified from the
        // result/subtype text the same way a non-zero exit's stderr would be.
        if parsed.is_error {
            return Err(classify_failure(&parsed.error_text(), None));
        }

        if parsed.content.is_empty() {
            return Err(ProviderError::Upstream(
                "claude -p produced no parseable output".into(),
            ));
        }

        let cost = CostTuple {
            tokens_in: u64::from(parsed.usage.tokens_in),
            tokens_out: u64::from(parsed.usage.tokens_out),
            // Subscription-billed: the Agent SDK Credit pool pays for the call, so
            // there is no per-call monetary cost to attribute onto the turn.
            cents: 0,
            wall_ms: 0,
            attention_score: 0.0,
        };

        Ok(CompletionResponse {
            content: parsed.content,
            finish_reason: parsed.finish_reason,
            usage: parsed.usage,
            cost,
            raw_provider_response: Some(parsed.raw),
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // Phase 2: stream `claude -p --output-format stream-json` events.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed rate card. Claude CLI calls are paid by the user's Anthropic
/// subscription (the Agent SDK Credit pool), not per token, so every completion
/// is priced at zero cents — the card exists only to satisfy
/// [`Provider::rate_card`].
fn claude_subscription_rate_card() -> RateCard {
    RateCard {
        version_id: "claude-cli-subscription-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
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

/// Spawn the claude subprocess, retrying briefly on `ETXTBSY` ("Text file busy").
///
/// On Linux, `execve(2)` fails with `ETXTBSY` if **any** process still holds the
/// target file open for writing. In a multithreaded program that both writes
/// executables and spawns subprocesses — exactly this crate's test suite, where
/// parallel tests each write a shim then exec it — a sibling thread's
/// `fork()`+`execve()` transiently inherits the just-written file's writable fd
/// across the fork window, so our `execve` of that file races to `ETXTBSY` even
/// though our own writer was already closed. The inherited fd is `O_CLOEXEC`
/// (Rust's default), so the window is only as long as the racing child's
/// fork→exec gap; a bounded retry with a small linear backoff closes it
/// deterministically. macOS never returns `ETXTBSY`, so this is a no-op there
/// and the very first `spawn` succeeds. This mirrors the §3.3b `spawn_codex`
/// fix (PR #82), which root-caused the identical race in the codex backend.
async fn spawn_claude(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
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

/// Flatten a chat transcript into the single prompt string the CLI reads on
/// stdin.
///
/// System messages are joined at the top as instructions; the remaining turns
/// are rendered `User:`/`Assistant:` so the model sees the conversation shape.
// TODO §3.3c Phase 2: Claude Code is an agentic surface, not a chat-completions
// endpoint — richer handling (passing system as `--append-system-prompt`,
// attaching prior assistant tool runs) would preserve more structure than this
// flattening does.
fn build_transcript(messages: &[ardur_runtime::ChatMessage]) -> String {
    let mut systems: Vec<&str> = Vec::new();
    let mut dialogue: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => systems.push(m.content.as_str()),
            Role::User => dialogue.push(format!("User: {}", m.content)),
            Role::Assistant => dialogue.push(format!("Assistant: {}", m.content)),
            // §6.0: the `claude` CLI orchestrates its own tools, so this provider
            // is skipped for the runtime tool-call loop (P1). A replayed tool
            // result still renders as a labelled line so the transcript is
            // intelligible rather than dropped.
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

/// The fields pulled out of a `claude -p --output-format json` stdout value.
#[derive(Debug)]
struct ParsedOutput {
    /// The final `result` text, empty if none.
    content: String,
    /// Token usage from the `result` object's `usage`.
    usage: Usage,
    /// `Stop` normally; `MaxTokens` on `max_tokens`; `Error` on a failed turn.
    finish_reason: FinishReason,
    /// Whether the `result` object reported `is_error: true`.
    is_error: bool,
    /// The `subtype` of the `result` object (e.g. `"error_max_turns"`), for
    /// in-band error classification.
    subtype: String,
    /// The whole decoded JSON value, retained as the raw audit body.
    raw: serde_json::Value,
}

impl ParsedOutput {
    /// The text an in-band `is_error` result classifies against: the subtype plus
    /// any result content (the CLI sometimes carries the message in `result`).
    fn error_text(&self) -> String {
        format!("{} {}", self.subtype, self.content)
    }
}

/// Parse the JSON value `claude -p --output-format json` writes to stdout.
///
/// Current CLI versions emit a JSON **array** of stream events ending in a
/// `{"type":"result", …}` object; a single `result` object is also accepted (an
/// older / future single-object form). Returns [`ProviderError::Upstream`] when
/// stdout holds no parseable JSON or no `result` object at all (the taxonomy has
/// no dedicated `InvalidResponse` variant — see the lib docs).
fn parse_claude_output(stdout: &str) -> Result<ParsedOutput, ProviderError> {
    let trimmed = stdout.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        ProviderError::Upstream(format!("claude -p output was not valid JSON: {e}"))
    })?;

    let result = find_result(&value).ok_or_else(|| {
        ProviderError::Upstream("claude -p output carried no `result` object to parse".to_string())
    })?;

    // The answer text: the result's `result` field, falling back to the last
    // assistant message's text if the result object omitted it.
    let content = result["result"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| last_assistant_text(&value));

    let mut usage = Usage::default();
    let u = &result["usage"];
    usage.tokens_in = u["input_tokens"].as_u64().unwrap_or(0) as u32;
    usage.tokens_out = u["output_tokens"].as_u64().unwrap_or(0) as u32;

    let is_error = result["is_error"].as_bool().unwrap_or(false);
    let subtype = result["subtype"].as_str().unwrap_or("").to_string();

    let finish_reason = map_finish_reason(result["stop_reason"].as_str(), is_error, &subtype);

    Ok(ParsedOutput {
        content,
        usage,
        finish_reason,
        is_error,
        subtype,
        raw: value,
    })
}

/// Locate the `result` object in the CLI's stdout value: the last
/// `type == "result"` element of an event array, or a top-level object that is
/// itself the result (or at least carries a `result` field).
fn find_result(value: &serde_json::Value) -> Option<&serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().rev().find(|e| e["type"] == "result"),
        obj @ serde_json::Value::Object(_) => {
            if obj["type"] == "result" || obj.get("result").is_some() {
                Some(obj)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The text of the last `assistant` message in an event array — the fallback
/// content when the `result` object did not carry a `result` string.
fn last_assistant_text(value: &serde_json::Value) -> String {
    let serde_json::Value::Array(items) = value else {
        return String::new();
    };
    for event in items.iter().rev() {
        if event["type"] != "assistant" {
            continue;
        }
        if let Some(blocks) = event["message"]["content"].as_array() {
            let text: String = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// Map the CLI's `stop_reason` (plus the in-band error flags) onto a
/// [`FinishReason`]. `end_turn`/`stop` → `Stop`; `max_tokens` → `MaxTokens`; an
/// `is_error` result → `Error` (carrying the subtype); anything else → `Stop`.
fn map_finish_reason(stop_reason: Option<&str>, is_error: bool, subtype: &str) -> FinishReason {
    match stop_reason {
        Some("end_turn") | Some("stop") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::MaxTokens,
        _ if is_error => FinishReason::Error(if subtype.is_empty() {
            "claude -p reported an error".to_string()
        } else {
            subtype.to_string()
        }),
        _ => FinishReason::Stop,
    }
}

/// Classify a CLI failure (a non-zero exit's stderr, or an in-band `is_error`
/// result's text) onto the closest [`ProviderError`] variant.
fn classify_failure(text: &str, exit_code: Option<i32>) -> ProviderError {
    if looks_like_login_error(text) {
        return ProviderError::Unauthorized;
    }
    if looks_like_rate_limit(text) {
        // The CLI does not report a machine-readable back-off here, so retry
        // pacing is left to the caller; signal "retry later" with a zero hint.
        return ProviderError::RateLimited { retry_after_ms: 0 };
    }
    let detail = text.trim();
    match exit_code {
        Some(code) => {
            ProviderError::Upstream(format!("claude -p exited with status {code}: {detail}"))
        }
        None => ProviderError::Upstream(format!("claude -p reported an error: {detail}")),
    }
}

/// Whether the CLI's output indicates a missing/expired login (→
/// [`ProviderError::Unauthorized`]). Deliberately narrow so it does not trip on
/// unrelated MCP-server auth noise.
fn looks_like_login_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("not logged in")
        || lower.contains("claude login")
        || lower.contains("please log in")
        || lower.contains("please login")
        || lower.contains("no credentials")
        || lower.contains("invalid api key")
        || lower.contains("authentication")
}

/// Whether the CLI's output indicates the Agent SDK Credit pool / usage limit is
/// exhausted (→ [`ProviderError::RateLimited`]).
fn looks_like_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("usage limit")
        || lower.contains("credit")
        || lower.contains("quota")
        || lower.contains("overloaded")
        || lower.contains("too many requests")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_runtime::ChatMessage;

    #[test]
    fn permission_mode_flag_and_parse_roundtrip() {
        assert_eq!(PermissionMode::Default.as_flag(), "default");
        assert_eq!(PermissionMode::AcceptEdits.as_flag(), "acceptEdits");
        assert_eq!(PermissionMode::Auto.as_flag(), "auto");
        assert_eq!(
            PermissionMode::BypassPermissions.as_flag(),
            "bypassPermissions"
        );
        assert_eq!(PermissionMode::DontAsk.as_flag(), "dontAsk");
        assert_eq!(PermissionMode::Plan.as_flag(), "plan");

        assert_eq!(
            PermissionMode::parse("default"),
            Some(PermissionMode::Default)
        );
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            PermissionMode::parse("accept-edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            PermissionMode::parse("ACCEPT_EDITS"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            PermissionMode::parse("bypassPermissions"),
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            PermissionMode::parse("dontAsk"),
            Some(PermissionMode::DontAsk)
        );
        assert_eq!(PermissionMode::parse("plan"), Some(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("bogus"), None);
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn config_builder_sets_every_field() {
        let cfg = ClaudeCliConfig::new()
            .claude_binary("/opt/bin/claude")
            .default_model("sonnet")
            .working_directory("/tmp/work")
            .allowed_tools("Bash(git *) Edit")
            .permission_mode(PermissionMode::AcceptEdits)
            .request_timeout(Duration::from_secs(42));
        assert_eq!(cfg.claude_binary, PathBuf::from("/opt/bin/claude"));
        assert_eq!(cfg.default_model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.working_directory, Some(PathBuf::from("/tmp/work")));
        assert_eq!(cfg.allowed_tools.as_deref(), Some("Bash(git *) Edit"));
        assert_eq!(cfg.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(cfg.request_timeout, Duration::from_secs(42));
    }

    #[test]
    fn default_config_resolves_claude_on_path() {
        let cfg = ClaudeCliConfig::new();
        assert_eq!(cfg.claude_binary, PathBuf::from(DEFAULT_BINARY));
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.allowed_tools, None);
        assert_eq!(cfg.permission_mode, PermissionMode::Default);
        assert_eq!(
            cfg.request_timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
    }

    #[test]
    fn chosen_model_prefers_request_then_default() {
        let provider = ClaudeCliProvider::new(
            ClaudeCliConfig::new().default_model("sonnet"),
            ModelId::new("sonnet"),
        );
        // Request names a model → that wins.
        assert_eq!(
            provider.chosen_model(&ModelId::new("claude-opus-4-8")),
            Some("claude-opus-4-8")
        );
        // Empty request model → config default.
        assert_eq!(provider.chosen_model(&ModelId::new("")), Some("sonnet"));
        // No request, no default → None (the CLI picks its own).
        let bare = ClaudeCliProvider::new(ClaudeCliConfig::new(), ModelId::new(""));
        assert_eq!(bare.chosen_model(&ModelId::new("")), None);
    }

    #[test]
    fn transcript_puts_system_on_top_and_labels_turns() {
        let transcript = build_transcript(&[
            ChatMessage::system("be terse"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("hello"),
            ChatMessage::user("ping"),
        ]);
        assert_eq!(
            transcript,
            "be terse\n\nUser: hi\n\nAssistant: hello\n\nUser: ping"
        );
    }

    #[test]
    fn transcript_without_system_is_just_dialogue() {
        let transcript = build_transcript(&[ChatMessage::user("only me")]);
        assert_eq!(transcript, "User: only me");
    }

    #[test]
    fn parse_extracts_content_and_usage_from_event_array() {
        // The real CLI shape: an array of stream events ending in a `result`.
        let stdout = concat!(
            "[",
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"},",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}},",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,",
            "\"result\":\"pong\",\"stop_reason\":\"end_turn\",",
            "\"usage\":{\"input_tokens\":42,\"output_tokens\":7}}",
            "]"
        );
        let parsed = parse_claude_output(stdout).expect("parse");
        assert_eq!(parsed.content, "pong");
        assert_eq!(parsed.usage.tokens_in, 42);
        assert_eq!(parsed.usage.tokens_out, 7);
        assert!(matches!(parsed.finish_reason, FinishReason::Stop));
        assert!(!parsed.is_error);
    }

    #[test]
    fn parse_accepts_single_result_object() {
        // A single `result` object (older / future single-object form).
        let stdout = "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"hi\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}";
        let parsed = parse_claude_output(stdout).expect("parse");
        assert_eq!(parsed.content, "hi");
        assert_eq!(parsed.usage.tokens_in, 1);
        assert_eq!(parsed.usage.tokens_out, 2);
    }

    #[test]
    fn parse_falls_back_to_assistant_text_when_result_field_empty() {
        let stdout = concat!(
            "[",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"from-assistant\"}]}},",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}",
            "]"
        );
        let parsed = parse_claude_output(stdout).expect("parse");
        assert_eq!(parsed.content, "from-assistant");
    }

    #[test]
    fn parse_marks_max_tokens_finish_reason() {
        let stdout = "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"truncated\",\"stop_reason\":\"max_tokens\",\"usage\":{\"input_tokens\":1,\"output_tokens\":99}}";
        let parsed = parse_claude_output(stdout).expect("parse");
        assert!(matches!(parsed.finish_reason, FinishReason::MaxTokens));
    }

    #[test]
    fn parse_flags_in_band_error_result() {
        let stdout = "{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"result\":\"\",\"stop_reason\":null}";
        let parsed = parse_claude_output(stdout).expect("parse");
        assert!(parsed.is_error);
        assert_eq!(parsed.subtype, "error_max_turns");
        assert!(matches!(parsed.finish_reason, FinishReason::Error(m) if m == "error_max_turns"));
    }

    #[test]
    fn parse_rejects_non_json() {
        let err = parse_claude_output("not json at all").unwrap_err();
        assert!(matches!(err, ProviderError::Upstream(_)));
    }

    #[test]
    fn parse_rejects_array_without_result() {
        let err = parse_claude_output("[{\"type\":\"system\"}]").unwrap_err();
        match err {
            ProviderError::Upstream(m) => assert!(m.contains("no `result`"), "got: {m}"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn classify_login_error_is_unauthorized() {
        assert!(matches!(
            classify_failure("Invalid API key · Please run /login", Some(1)),
            ProviderError::Unauthorized
        ));
        assert!(matches!(
            classify_failure("Not logged in. Run `claude login`.", Some(1)),
            ProviderError::Unauthorized
        ));
    }

    #[test]
    fn classify_rate_limit_is_rate_limited() {
        assert!(matches!(
            classify_failure("Usage limit reached for your plan", Some(1)),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_failure("Credit balance is too low", Some(1)),
            ProviderError::RateLimited { .. }
        ));
    }

    #[test]
    fn classify_generic_failure_is_upstream() {
        match classify_failure("boom: internal error", Some(2)) {
            ProviderError::Upstream(m) => {
                assert!(m.contains("boom: internal error"), "got: {m}");
                assert!(m.contains("status 2"), "got: {m}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
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
    fn provider_id_is_claude_cli_and_not_streaming() {
        let provider = ClaudeCliProvider::new(ClaudeCliConfig::new(), ModelId::new("sonnet"));
        assert_eq!(provider.id(), ProviderId("claude-cli".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(
            provider.rate_card().version_id,
            "claude-cli-subscription-v1"
        );
    }
}
