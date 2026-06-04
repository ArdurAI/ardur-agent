//! ardur-provider-codex — the [OpenAI Codex] CLI subscription backend (§3.3b).
//!
//! The HTTP backends (Anthropic §3.1, OpenRouter §3.2) authenticate with an API
//! key and POST to a REST endpoint, billing per token. This backend is
//! different: it wraps the locally-installed `codex` CLI as a **subprocess**,
//! running it non-interactively via `codex exec --json`. Authentication is
//! inherited from `codex login` (a logged-in ChatGPT Plus/Pro/Team
//! subscription), so a dogfooding turn spends the subscription rather than
//! metered API billing — there is no API key in this crate's config, and every
//! completion is priced at **zero cents** (see [`CostTuple`]).
//!
//! Despite the very different transport, it implements the same §3.0
//! [`Provider`] trait as the HTTP backends, so the runtime dispatches to it
//! through the generic [`ProviderRegistry`] with no catalog change.
//!
//! # How a turn maps onto the CLI
//!
//! - The request's [`messages`](CompletionRequest::messages) are flattened into
//!   a single prompt transcript (system text on top, then `User:` / `Assistant:`
//!   turns) and piped to the subprocess on stdin. Codex is an agentic prompt
//!   surface, not an OpenAI chat-completions endpoint, so this is a lossy P1
//!   rendering — see the `build_transcript` TODO for Phase 2.
//! - The chosen model is passed with `-m`: the request's [`ModelId`] wins, else
//!   the config's [`default_model`](CodexConfig::default_model), else codex's own
//!   default.
//! - `codex exec --json` writes a JSONL event stream to stdout. The final
//!   `agent_message` item is the response content; `turn.completed.usage`
//!   carries the input/output token counts. If no event is parseable, the raw
//!   stdout (ANSI-stripped) is used as the content (the plain-text fallback).
//!
//! # Auth & install requirements
//!
//! The host must have the `codex` CLI on `PATH` (or [`CodexConfig::codex_binary`]
//! pointed at it) and an active session from `codex login`. A missing binary
//! surfaces as [`ProviderError::Upstream`] ("Codex CLI not installed …"); a
//! missing/expired login surfaces as [`ProviderError::Unauthorized`].
//!
//! # Error-taxonomy mapping
//!
//! The shared [`ProviderError`] enum has no `ConfigError`/`Timeout`/
//! `InvalidResponse` variants, so the §3.3b failure classes are mapped onto the
//! closest existing ones:
//!
//! | Failure                         | [`ProviderError`]                |
//! |---------------------------------|----------------------------------|
//! | binary not found on `PATH`      | [`Upstream`](ProviderError::Upstream) ("Codex CLI not installed …") |
//! | not logged in (`codex login`)   | [`Unauthorized`](ProviderError::Unauthorized) |
//! | run exceeded `request_timeout`  | [`NetworkFailure`](ProviderError::NetworkFailure) (its docs name timeouts) |
//! | non-zero exit + stderr          | [`Upstream`](ProviderError::Upstream) (stderr verbatim) |
//! | success but unparseable output  | [`Upstream`](ProviderError::Upstream) |
//!
//! # Not in Phase 1
//!
//! - **Streaming** — [`Provider::supports_streaming`] is `false`; the whole
//!   subprocess is awaited before a response is returned. (Phase 2.)
//! - **Tool-call parsing** — codex runs tools inside its own sandbox; this layer
//!   only surfaces the final assistant text, never a [`FinishReason::ToolUse`].
//! - **Rich message handling** — the flattened transcript loses turn structure.
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
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "codex";
/// The binary [`CodexConfig`] resolves through `PATH` by default.
pub const DEFAULT_BINARY: &str = "codex";
/// Default per-run timeout — a `codex exec` turn can take a while.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Env var [`CodexConfig::from_env`] reads the binary path from.
pub const BINARY_ENV: &str = "CODEX_BINARY";
/// Env var [`CodexConfig::from_env`] reads the default model from.
pub const DEFAULT_MODEL_ENV: &str = "CODEX_DEFAULT_MODEL";
/// Env var [`CodexConfig::from_env`] reads the sandbox mode from.
pub const SANDBOX_MODE_ENV: &str = "CODEX_SANDBOX_MODE";
/// Env var [`CodexConfig::from_env`] reads the working directory from.
pub const WORKING_DIR_ENV: &str = "CODEX_WORKING_DIR";

/// The sandbox policy `codex exec` runs model-generated shell commands under
/// (the `-s/--sandbox` flag).
///
/// This is the §3.3b adaptation of the plan's `ApprovalMode`
/// (`Suggest`/`AutoEdit`/`FullAuto`): codex 0.136 replaced approval modes with
/// sandbox modes, and these are the values the installed CLI actually accepts.
/// Because this provider is used as a *text-completion* backend (we want the
/// model's answer, not file edits), the default is the most restrictive
/// [`ReadOnly`](SandboxMode::ReadOnly).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// `read-only` — codex may read the workspace but not write or run mutating
    /// commands. The safe default for a completion backend.
    #[default]
    ReadOnly,
    /// `workspace-write` — codex may edit files inside its working directory.
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
/// Build it with [`CodexConfig::new`] (or [`CodexConfig::from_env`]) and tune the
/// optional fields with the builder methods. There is **no API key** — auth is
/// inherited from `codex login`.
#[derive(Clone, Debug)]
pub struct CodexConfig {
    codex_binary: PathBuf,
    default_model: Option<String>,
    working_directory: Option<PathBuf>,
    sandbox_mode: SandboxMode,
    request_timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            codex_binary: PathBuf::from(DEFAULT_BINARY),
            default_model: None,
            working_directory: None,
            sandbox_mode: SandboxMode::default(),
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
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
    /// Reads [`BINARY_ENV`], [`DEFAULT_MODEL_ENV`], [`SANDBOX_MODE_ENV`], and
    /// [`WORKING_DIR_ENV`]; any unset/empty/unparseable value falls back to its
    /// default. This is **infallible** — unlike the HTTP backends there is no API
    /// key to be missing, so there is nothing here that can fail (a missing
    /// `codex login` is only discovered when a turn actually runs).
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::new();
        if let Ok(bin) = std::env::var(BINARY_ENV) {
            if !bin.is_empty() {
                cfg.codex_binary = PathBuf::from(bin);
            }
        }
        if let Ok(model) = std::env::var(DEFAULT_MODEL_ENV) {
            if !model.is_empty() {
                cfg.default_model = Some(model);
            }
        }
        if let Ok(mode) = std::env::var(SANDBOX_MODE_ENV) {
            if let Some(parsed) = SandboxMode::parse(&mode) {
                cfg.sandbox_mode = parsed;
            }
        }
        if let Ok(dir) = std::env::var(WORKING_DIR_ENV) {
            if !dir.is_empty() {
                cfg.working_directory = Some(PathBuf::from(dir));
            }
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

    /// Set the working directory codex runs in (`-C`). Codex needs a cwd for its
    /// sandbox; when unset, it inherits this process's.
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
}

/// The Codex CLI subscription provider.
///
/// Construct it with [`CodexProvider::new`] (from a [`CodexConfig`] and a default
/// model) or [`CodexProvider::from_env`]. The model on each
/// [`CompletionRequest`] selects which model codex runs; `model_id` is only the
/// default the runtime stamps onto a request.
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

#[async_trait]
impl Provider for CodexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let prompt = build_transcript(&req.messages);

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
            .arg("-s")
            .arg(self.config.sandbox_mode.as_flag());
        if let Some(cwd) = &self.config.working_directory {
            cmd.arg("-C").arg(cwd);
        }
        if let Some(model) = self.chosen_model(&req.model) {
            cmd.arg("-m").arg(model);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout the `wait_with_output` future is dropped, which drops
            // the child; kill_on_drop ensures the codex process dies with it.
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Upstream(format!(
                    "Codex CLI not installed (binary {:?} not found on PATH): {e}",
                    self.config.codex_binary
                ))
            } else {
                ProviderError::Upstream(format!("failed to spawn codex: {e}"))
            }
        })?;

        // Feed the prompt on stdin, then close it so codex sees EOF and starts.
        // P1 prompts are small, so a full write before reading stdout cannot
        // deadlock; Phase 2's streaming path will interleave the two.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Upstream("codex child stdin was not captured".into()))?;
        match stdin.write_all(prompt.as_bytes()).await {
            Ok(()) => {}
            // A child that exits before draining stdin (e.g. it failed fast on a
            // bad flag, or is already done) closes the read end, so the write
            // races to a BrokenPipe. That is not our failure to report — let the
            // exit status and captured stderr be the source of truth.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => {
                return Err(ProviderError::Upstream(format!(
                    "writing prompt to codex stdin: {e}"
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
                        "waiting on codex subprocess: {e}"
                    )));
                }
                Err(_elapsed) => {
                    return Err(ProviderError::NetworkFailure(format!(
                        "codex exec timed out after {}s",
                        self.config.request_timeout.as_secs()
                    )));
                }
            };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            if looks_like_login_error(&stderr) {
                return Err(ProviderError::Unauthorized);
            }
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            let detail = stderr.trim();
            return Err(ProviderError::Upstream(format!(
                "codex exec exited with status {code}: {detail}"
            )));
        }

        let parsed = parse_codex_output(&stdout);

        // Prefer the parsed `agent_message`; fall back to ANSI-stripped raw
        // stdout (the plain-text path for output that carried no JSON events).
        let content = if parsed.content.is_empty() {
            strip_ansi(&stdout).trim().to_string()
        } else {
            parsed.content
        };
        if content.is_empty() {
            return Err(ProviderError::Upstream(
                "codex exec produced no parseable output".into(),
            ));
        }

        let cost = CostTuple {
            tokens_in: u64::from(parsed.usage.tokens_in),
            tokens_out: u64::from(parsed.usage.tokens_out),
            // Subscription-billed: the ChatGPT plan pays for the call, so there
            // is no per-call monetary cost to attribute onto the turn.
            cents: 0,
            wall_ms: 0,
            attention_score: 0.0,
        };

        Ok(CompletionResponse {
            content,
            finish_reason: parsed.finish_reason,
            usage: parsed.usage,
            cost,
            raw_provider_response: Some(serde_json::Value::Array(parsed.events)),
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // Phase 2: stream `codex exec --json` events as they arrive.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
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

/// Flatten a chat transcript into the single prompt string codex reads on stdin.
///
/// System messages are joined at the top as instructions; the remaining turns
/// are rendered `User:`/`Assistant:` so the model sees the conversation shape.
// TODO §3.3b Phase 2: codex is an agentic surface, not a chat-completions
// endpoint — richer handling (passing system as instructions, attaching prior
// assistant tool runs, image inputs via `-i`) would preserve more structure than
// this flattening does.
fn build_transcript(messages: &[ardur_runtime::ChatMessage]) -> String {
    let mut systems: Vec<&str> = Vec::new();
    let mut dialogue: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => systems.push(m.content.as_str()),
            Role::User => dialogue.push(format!("User: {}", m.content)),
            Role::Assistant => dialogue.push(format!("Assistant: {}", m.content)),
            // §6.0: the `codex` CLI orchestrates its own tools, so this provider
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

/// The fields pulled out of a `codex exec --json` stdout stream.
struct ParsedOutput {
    /// The final `agent_message` text (last one wins), empty if none.
    content: String,
    /// Token usage from the `turn.completed` event.
    usage: Usage,
    /// `Stop` normally, `Error` if a `turn.failed`/`error` event was seen.
    finish_reason: FinishReason,
    /// Every decoded JSON event, retained as the raw audit body.
    events: Vec<serde_json::Value>,
}

/// Parse the JSONL event stream codex writes to stdout. Non-JSON lines (stray
/// log output) are skipped rather than failing the whole parse.
fn parse_codex_output(stdout: &str) -> ParsedOutput {
    let mut content = String::new();
    let mut usage = Usage::default();
    let mut finish_reason = FinishReason::Stop;
    let mut events: Vec<serde_json::Value> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
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
        events.push(value);
    }

    ParsedOutput {
        content,
        usage,
        finish_reason,
        events,
    }
}

/// Whether codex's stderr indicates a missing/expired login (→
/// [`ProviderError::Unauthorized`]). Deliberately narrow so it does not trip on
/// the unrelated MCP-server auth noise codex prints to stderr.
fn looks_like_login_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not logged in")
        || lower.contains("codex login")
        || lower.contains("please log in")
        || lower.contains("please login")
        || lower.contains("no credentials")
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

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_runtime::ChatMessage;

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
            .request_timeout(Duration::from_secs(42));
        assert_eq!(cfg.codex_binary, PathBuf::from("/opt/bin/codex"));
        assert_eq!(cfg.default_model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(cfg.working_directory, Some(PathBuf::from("/tmp/work")));
        assert_eq!(cfg.sandbox_mode, SandboxMode::WorkspaceWrite);
        assert_eq!(cfg.request_timeout, Duration::from_secs(42));
    }

    #[test]
    fn default_config_resolves_codex_on_path() {
        let cfg = CodexConfig::new();
        assert_eq!(cfg.codex_binary, PathBuf::from(DEFAULT_BINARY));
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.sandbox_mode, SandboxMode::ReadOnly);
        assert_eq!(
            cfg.request_timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
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
    }

    #[test]
    fn parse_marks_turn_failed_as_error() {
        let stdout = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"model overloaded\"}}\n";
        let parsed = parse_codex_output(stdout);
        assert!(matches!(parsed.finish_reason, FinishReason::Error(m) if m == "model overloaded"));
    }

    #[test]
    fn login_error_detection_is_narrow() {
        assert!(looks_like_login_error(
            "Error: Not logged in. Run `codex login`."
        ));
        assert!(looks_like_login_error("no credentials found"));
        // MCP auth noise must NOT be mistaken for a codex login failure.
        assert!(!looks_like_login_error(
            "ERROR rmcp::transport: AuthRequired Authorization header required"
        ));
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let colored = "\u{1b}[1;32mgreen\u{1b}[0m plain";
        assert_eq!(strip_ansi(colored), "green plain");
    }

    #[test]
    fn provider_id_is_codex_and_not_streaming() {
        let provider = CodexProvider::new(CodexConfig::new(), ModelId::new("gpt-5-codex"));
        assert_eq!(provider.id(), ProviderId("codex".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.rate_card().version_id, "codex-subscription-v1");
    }
}
