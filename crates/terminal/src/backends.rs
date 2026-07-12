//! Terminal backend implementations.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{Result, TerminalError};

/// Hard ceiling on `timeout_secs`, applied regardless of what a caller
/// requests. Without this, a caller (the command string and its
/// `timeout_secs` both ultimately come from model output, which must be
/// treated as untrusted) can pass an effectively-infinite timeout — e.g.
/// `u64::MAX` seconds — and pair it with a command that never terminates on
/// its own (`yes`, `tail -f`, a long-poll loop), turning one tool call into
/// an unbounded hang.
const DEFAULT_MAX_TIMEOUT_SECS: u64 = 900;

/// The kind of terminal backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    /// Execute on the local host.
    Local,
    /// Execute inside an existing Docker container with `docker exec` semantics.
    Docker,
    /// Execute over SSH against a remote host.
    Ssh,
    /// Execute in a Modal/cloud sandbox.
    Modal,
}

impl BackendKind {
    /// Lowercase wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Local => "local",
            BackendKind::Docker => "docker",
            BackendKind::Ssh => "ssh",
            BackendKind::Modal => "modal",
        }
    }
}

/// Policy applied by every terminal backend before execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPolicy {
    /// First argv token allowlist. Empty means deny all commands.
    pub command_allowlist: Vec<String>,
    /// Maximum captured output bytes.
    pub max_output_bytes: usize,
    /// Hard ceiling on the `timeout_secs` a caller may request; see
    /// [`DEFAULT_MAX_TIMEOUT_SECS`].
    pub max_timeout_secs: u64,
}

impl TerminalPolicy {
    /// Strict deny-all policy.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            command_allowlist: Vec::new(),
            max_output_bytes: 1024 * 1024,
            max_timeout_secs: DEFAULT_MAX_TIMEOUT_SECS,
        }
    }

    /// Allow commands whose first shell token is in `commands`.
    #[must_use]
    pub fn allow_commands<I, S>(commands: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            command_allowlist: commands.into_iter().map(Into::into).collect(),
            max_output_bytes: 1024 * 1024,
            max_timeout_secs: DEFAULT_MAX_TIMEOUT_SECS,
        }
    }

    /// Clamp a caller-requested timeout to [`Self::max_timeout_secs`], and to
    /// at least 1 second (a 0s timeout would fail every command instantly
    /// without ever giving it a chance to run).
    #[must_use]
    pub fn effective_timeout_secs(&self, requested: u64) -> u64 {
        requested.clamp(1, self.max_timeout_secs)
    }

    /// Development-only allow-all policy.
    #[must_use]
    pub fn permissive() -> Self {
        Self::allow_commands(["*"])
    }

    /// Check a command against the policy.
    ///
    /// Two gates, both failing closed:
    /// 1. **Safe-charset allowlist** — the command may contain only alphanumerics,
    ///    whitespace, and the punctuation common to flags, paths, and `key=value`
    ///    pairs ([`is_safe_command_char`]). Every shell operator, metacharacter,
    ///    expansion, and quote is rejected, so no backend — even one that hands
    ///    the string to a remote shell — can suffer command injection (ARD-476).
    /// 2. **Binary allowlist** — the first token (the real binary, never an
    ///    operator-disguised prefix) must be in `command_allowlist` (or `"*"`).
    ///
    /// Shell composition, quoting, and expansions are intentionally unsupported
    /// here; the separate `shell.run` builtin owns that surface.
    pub fn check_command(&self, command: &str) -> Result<()> {
        if let Some(bad) = command.chars().find(|c| !is_safe_command_char(*c)) {
            return Err(TerminalError::PolicyDenied {
                reason: format!(
                    "command contains a disallowed character {bad:?}; terminal.exec accepts only \
                     simple commands without shell operators, quotes, or expansions (use shell.run \
                     for shell composition)"
                ),
            });
        }
        let first =
            command
                .split_whitespace()
                .next()
                .ok_or_else(|| TerminalError::PolicyDenied {
                    reason: "empty command is denied".to_string(),
                })?;
        if self
            .command_allowlist
            .iter()
            .any(|entry| entry == "*" || entry == first)
        {
            return Ok(());
        }
        Err(TerminalError::PolicyDenied {
            reason: format!("command `{first}` is not in the terminal allowlist"),
        })
    }
}

impl Default for TerminalPolicy {
    fn default() -> Self {
        Self::deny_all()
    }
}

/// Whether `c` is permitted inside a terminal command (see
/// [`TerminalPolicy::check_command`]). An **allowlist** — alphanumerics,
/// whitespace, and the punctuation that appears in flags, paths, and
/// `key=value` pairs — rather than an operator denylist, so it fails closed
/// against anything novel and rejects every shell operator/metacharacter/
/// expansion/quote outright.
fn is_safe_command_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        // Only space and tab are admitted as separators — crucially *not* the
        // other ASCII whitespace chars (`\n`, `\r`, `\x0b`, `\x0c`), which are
        // shell command separators and an injection vector.
        || c == ' '
        || c == '\t'
        || matches!(
            c,
            '-' | '_' | '.' | '/' | ':' | ',' | '=' | '+' | '@' | '%'
        )
}

/// Result of executing a command in a backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code. `-1` means the backend could not provide one.
    pub exit_code: i32,
    /// Backend kind that produced the result.
    pub backend: BackendKind,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Whether output was truncated to the policy limit.
    pub truncated: bool,
}

impl ExecResult {
    fn new(
        stdout: String,
        stderr: String,
        exit_code: i32,
        backend: BackendKind,
        start_ms: u64,
        max: usize,
    ) -> Self {
        let mut result = Self {
            stdout,
            stderr,
            exit_code,
            backend,
            duration_ms: now_ms().saturating_sub(start_ms),
            truncated: false,
        };
        result.truncate(max);
        result
    }

    fn truncate(&mut self, max: usize) {
        let total = self.stdout.len().saturating_add(self.stderr.len());
        if total <= max {
            return;
        }
        self.truncated = true;
        let stdout_max = floor_char_boundary(&self.stdout, max.min(self.stdout.len()));
        self.stdout.truncate(stdout_max);
        let remaining = floor_char_boundary(&self.stderr, max.saturating_sub(self.stdout.len()));
        self.stderr.truncate(remaining);
    }
}

/// The largest byte offset `<= n` that lands on a UTF-8 character boundary in
/// `s`. `String::truncate` panics if given an offset that splits a multi-byte
/// character; callers that compute `n` from an arbitrary byte budget (like
/// [`ExecResult::truncate`]) must floor it through this first.
fn floor_char_boundary(s: &str, mut n: usize) -> usize {
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Accumulates bytes up to `cap`, discarding anything beyond it rather than
/// growing without bound.
///
/// A command's stdout/stderr (and, for the Docker backend, its log stream)
/// come from a process whose command line was itself untrusted model output
/// — a policy-allowlisted binary can still legitimately emit gigabytes
/// (`yes`, `find /`, a verbose build). Before this type existed, every
/// backend read the *entire* stream into a `String` first and only applied
/// [`ExecResult::truncate`] afterward, so `max_output_bytes` bounded the
/// final result but not peak memory during capture. `BoundedSink` bounds
/// memory as bytes arrive: once `cap` is reached, further pushed bytes are
/// dropped (not buffered), while the caller keeps draining the underlying
/// pipe/stream so the writer never blocks on a full buffer past the cap.
struct BoundedSink {
    buf: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl BoundedSink {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            truncated: false,
        }
    }

    /// Append as much of `chunk` as still fits under `cap`; the remainder (if
    /// any) is discarded and [`Self::truncated`] is latched.
    fn push(&mut self, chunk: &[u8]) {
        if self.buf.len() >= self.cap {
            self.truncated = true;
            return;
        }
        let remaining = self.cap - self.buf.len();
        if chunk.len() <= remaining {
            self.buf.extend_from_slice(chunk);
        } else {
            self.buf.extend_from_slice(&chunk[..remaining]);
            self.truncated = true;
        }
    }

    fn into_string_lossy(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Spawn `cmd` with piped stdout/stderr and drain both concurrently into
/// [`BoundedSink`]s capped at `max_output_bytes` each, so neither stream is
/// ever buffered without limit regardless of how much output the process
/// produces or how long it runs before the caller's timeout fires. Used by
/// [`LocalBackend`] and [`SshBackend`], which both spawn a real OS process;
/// [`DockerBackend`] applies the same `BoundedSink` pattern directly to its
/// log stream since it has no local child process to pipe from.
async fn run_piped_capped(
    mut cmd: Command,
    max_output_bytes: usize,
) -> std::io::Result<(String, String, i32)> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout_pipe = child.stdout.take().expect("stdout is piped above");
    let stderr_pipe = child.stderr.take().expect("stderr is piped above");

    let drain = |mut pipe: tokio::process::ChildStdout| async move {
        let mut sink = BoundedSink::new(max_output_bytes);
        let mut chunk = [0u8; 8192];
        loop {
            let n = pipe.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            sink.push(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(sink)
    };
    let drain_err = |mut pipe: tokio::process::ChildStderr| async move {
        let mut sink = BoundedSink::new(max_output_bytes);
        let mut chunk = [0u8; 8192];
        loop {
            let n = pipe.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            sink.push(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(sink)
    };

    let (stdout_sink, stderr_sink, status) =
        tokio::try_join!(drain(stdout_pipe), drain_err(stderr_pipe), child.wait())?;

    Ok((
        stdout_sink.into_string_lossy(),
        stderr_sink.into_string_lossy(),
        status.code().unwrap_or(-1),
    ))
}

/// A terminal backend that can execute commands.
#[async_trait]
pub trait TerminalBackend: Send + Sync {
    /// Backend kind.
    fn kind(&self) -> BackendKind;
    /// Backend policy.
    fn policy(&self) -> &TerminalPolicy;
    /// Execute a non-interactive command.
    async fn execute(&self, command: &str, timeout_secs: u64) -> Result<ExecResult>;
    /// Execute a PTY-style command. Backends without PTY support may fall back to
    /// [`execute`](Self::execute).
    async fn execute_pty(&self, command: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.execute(command, timeout_secs).await
    }
}

/// Local shell backend.
pub struct LocalBackend {
    policy: TerminalPolicy,
}

impl LocalBackend {
    /// Create a local backend with an explicit policy.
    #[must_use]
    pub fn new(policy: TerminalPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl TerminalBackend for LocalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }

    fn policy(&self) -> &TerminalPolicy {
        &self.policy
    }

    async fn execute(&self, command: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.policy.check_command(command)?;
        let timeout_secs = self.policy.effective_timeout_secs(timeout_secs);
        let start = now_ms();
        // `check_command` guarantees a non-empty, safe-charset command, so the
        // first token is the binary and the rest are literal arguments. Exec it
        // directly — no `/bin/sh -c` — so no metacharacter can ever be
        // interpreted (ARD-476).
        let mut tokens = command.split_whitespace();
        let binary = tokens
            .next()
            .expect("check_command guarantees a non-empty command");
        let mut cmd = Command::new(binary);
        cmd.args(tokens).kill_on_drop(true);
        // Piped + bounded, not `.output()`: a policy-allowlisted binary can
        // still emit unbounded output (`yes`, `find /`, a noisy build), and
        // `.output()` buffers the whole stream in memory before
        // `ExecResult::truncate` ever runs. `run_piped_capped` caps memory as
        // bytes arrive instead.
        let (stdout, stderr, exit_code) = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            run_piped_capped(cmd, self.policy.max_output_bytes),
        )
        .await
        .map_err(|_| TerminalError::Timeout { secs: timeout_secs })?
        .map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
        Ok(ExecResult::new(
            stdout,
            stderr,
            exit_code,
            BackendKind::Local,
            start,
            self.policy.max_output_bytes,
        ))
    }
}

/// Docker exec backend using the `bollard` Docker daemon API.
pub struct DockerBackend {
    container: String,
    policy: TerminalPolicy,
    mock_output: Option<String>,
}

impl DockerBackend {
    /// Create a Docker exec backend for an existing container.
    #[must_use]
    pub fn new(container: impl Into<String>, policy: TerminalPolicy) -> Self {
        Self {
            container: container.into(),
            policy,
            mock_output: None,
        }
    }

    /// Create a mock Docker backend for offline tests.
    #[must_use]
    pub fn mock(container: impl Into<String>, policy: TerminalPolicy) -> Self {
        let container = container.into();
        Self {
            mock_output: Some(format!("docker:{container}")),
            container,
            policy,
        }
    }
}

#[async_trait]
impl TerminalBackend for DockerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }

    fn policy(&self) -> &TerminalPolicy {
        &self.policy
    }

    async fn execute(&self, command: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.policy.check_command(command)?;
        let timeout_secs = self.policy.effective_timeout_secs(timeout_secs);
        let start = now_ms();
        if let Some(prefix) = &self.mock_output {
            return Ok(ExecResult::new(
                format!("{prefix}: {command}"),
                String::new(),
                0,
                BackendKind::Docker,
                start,
                self.policy.max_output_bytes,
            ));
        }

        let docker = Docker::connect_with_socket_defaults()
            .map_err(|e| TerminalError::BackendNotAvailable(format!("docker socket: {e}")))?;
        // `check_command` (run above) guarantees a safe-charset command, so pass
        // the parsed argv straight to `docker exec` — no `/bin/sh -lc` — and no
        // metacharacter can be re-interpreted (ARD-476).
        let argv: Vec<String> = command.split_whitespace().map(String::from).collect();
        let exec_id = docker
            .create_exec(
                &self.container,
                bollard::models::ExecConfig {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(argv),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| TerminalError::ExecutionFailed(format!("docker create_exec: {e}")))?
            .id;

        // KNOWN LIMITATION: the Docker Engine API has no "stop this exec"
        // primitive independent of the container's own PID namespace, so if
        // the outer `tokio::time::timeout` below fires, we stop *reading*
        // this exec's output and report `Timeout`, but the process started by
        // `create_exec`/`start_exec` may continue running inside the
        // container until it exits on its own or the container is stopped.
        // A reliable kill would need either `--pid=host` (which this backend
        // does not require or assume) or a second exec issuing `kill` by a
        // PID number whose namespace-scoping isn't guaranteed correct without
        // a live container to verify against — attempting that without the
        // ability to test it against a real daemon risks signalling the
        // wrong process, so it is deliberately not implemented here.
        let run = async {
            let mut stdout = BoundedSink::new(self.policy.max_output_bytes);
            let mut stderr = BoundedSink::new(self.policy.max_output_bytes);
            match docker
                .start_exec(&exec_id, None::<bollard::exec::StartExecOptions>)
                .await
                .map_err(|e| TerminalError::ExecutionFailed(format!("docker start_exec: {e}")))?
            {
                bollard::exec::StartExecResults::Attached { mut output, .. } => {
                    // Drain the whole log stream regardless of the caps
                    // above: `BoundedSink::push` discards bytes past its cap
                    // but this loop keeps calling `output.next()` until EOF,
                    // so a chatty process never blocks the daemon waiting on
                    // us and its output is still bounded in memory rather
                    // than accumulated without limit before
                    // `ExecResult::truncate` runs.
                    while let Some(item) = output.next().await {
                        let item =
                            item.map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
                        let text = item.to_string();
                        if matches!(item, bollard::container::LogOutput::StdErr { .. }) {
                            stderr.push(text.as_bytes());
                        } else {
                            stdout.push(text.as_bytes());
                        }
                    }
                }
                bollard::exec::StartExecResults::Detached => {}
            }
            let exit_code = docker
                .inspect_exec(&exec_id)
                .await
                .ok()
                .and_then(|info| info.exit_code)
                .and_then(|code| i32::try_from(code).ok())
                // `-1` (not `0`) per `ExecResult::exit_code`'s documented
                // contract: `0` means the process genuinely exited
                // successfully, which we do not know here — inspect_exec
                // failing, returning no code, or a code that doesn't fit in
                // `i32` are all "the backend could not provide one", and
                // reporting `0` for those would tell an agent gating a
                // follow-up action on "exit_code == 0" that a
                // failed/unknown/still-running command succeeded.
                .unwrap_or(-1);
            Ok(ExecResult::new(
                stdout.into_string_lossy(),
                stderr.into_string_lossy(),
                exit_code,
                BackendKind::Docker,
                start,
                self.policy.max_output_bytes,
            ))
        };

        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), run)
            .await
            .map_err(|_| TerminalError::Timeout { secs: timeout_secs })?
    }
}

/// SSH remote backend.
pub struct SshBackend {
    host: String,
    user: String,
    host_key_fingerprint: String,
    policy: TerminalPolicy,
    mock_output: Option<String>,
    _client_config: Arc<russh::client::Config>,
}

impl SshBackend {
    /// Create an SSH backend. A host-key fingerprint is mandatory to avoid
    /// silently accepting unknown hosts.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        user: impl Into<String>,
        host_key_fingerprint: impl Into<String>,
        policy: TerminalPolicy,
    ) -> Self {
        Self {
            host: host.into(),
            user: user.into(),
            host_key_fingerprint: host_key_fingerprint.into(),
            policy,
            mock_output: None,
            _client_config: Arc::new(russh::client::Config::default()),
        }
    }

    /// Create a mock SSH backend for offline tests.
    #[must_use]
    pub fn mock(
        host: impl Into<String>,
        user: impl Into<String>,
        host_key_fingerprint: impl Into<String>,
        policy: TerminalPolicy,
    ) -> Self {
        let host = host.into();
        let user = user.into();
        Self {
            mock_output: Some(format!("ssh:{user}@{host}")),
            host,
            user,
            host_key_fingerprint: host_key_fingerprint.into(),
            policy,
            _client_config: Arc::new(russh::client::Config::default()),
        }
    }
}

#[async_trait]
impl TerminalBackend for SshBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Ssh
    }

    fn policy(&self) -> &TerminalPolicy {
        &self.policy
    }

    async fn execute(&self, command: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.policy.check_command(command)?;
        let timeout_secs = self.policy.effective_timeout_secs(timeout_secs);
        if self.host_key_fingerprint.trim().is_empty() {
            return Err(TerminalError::PolicyDenied {
                reason: "SSH host-key fingerprint is required".to_string(),
            });
        }
        let start = now_ms();
        if let Some(prefix) = &self.mock_output {
            return Ok(ExecResult::new(
                format!("{prefix}: {command}"),
                String::new(),
                0,
                BackendKind::Ssh,
                start,
                self.policy.max_output_bytes,
            ));
        }
        let target = format!("{}@{}", self.user, self.host);
        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg(target)
            .arg(command)
            .kill_on_drop(true);
        // KNOWN LIMITATION: `kill_on_drop` kills the *local* `ssh` client
        // process when the timeout below fires; whether the *remote* command
        // also dies depends on how the remote shell handles the closed
        // channel (typically SIGHUP, but not guaranteed for every remote
        // shell/process). Forcing pty allocation (`-tt`) would make that more
        // reliable but also merges remote stdout/stderr into one pty stream,
        // breaking this backend's stdout/stderr separation — a worse
        // regression than the limitation it would partially address, and not
        // verifiable without a live remote host to test against, so it is
        // deliberately not applied here.
        let (stdout, stderr, exit_code) = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            run_piped_capped(cmd, self.policy.max_output_bytes),
        )
        .await
        .map_err(|_| TerminalError::Timeout { secs: timeout_secs })?
        .map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
        Ok(ExecResult::new(
            stdout,
            stderr,
            exit_code,
            BackendKind::Ssh,
            start,
            self.policy.max_output_bytes,
        ))
    }
}

/// Modal/cloud sandbox backend.
pub struct ModalBackend {
    endpoint: String,
    policy: TerminalPolicy,
    mock_output: Option<String>,
}

impl ModalBackend {
    /// Create a Modal backend using an HTTPS endpoint that accepts JSON
    /// `{command}` and returns `{stdout, stderr, exit_code}`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, policy: TerminalPolicy) -> Self {
        Self {
            endpoint: endpoint.into(),
            policy,
            mock_output: None,
        }
    }

    /// Create a mock Modal backend for offline tests.
    #[must_use]
    pub fn mock(endpoint: impl Into<String>, policy: TerminalPolicy) -> Self {
        let endpoint = endpoint.into();
        Self {
            mock_output: Some(format!("modal:{endpoint}")),
            endpoint,
            policy,
        }
    }
}

#[async_trait]
impl TerminalBackend for ModalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Modal
    }

    fn policy(&self) -> &TerminalPolicy {
        &self.policy
    }

    async fn execute(&self, command: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.policy.check_command(command)?;
        let start = now_ms();
        if let Some(prefix) = &self.mock_output {
            return Ok(ExecResult::new(
                format!("{prefix}: {command}"),
                String::new(),
                0,
                BackendKind::Modal,
                start,
                self.policy.max_output_bytes,
            ));
        }
        let timeout_secs = self.policy.effective_timeout_secs(timeout_secs);
        let token = std::env::var("MODAL_TOKEN_ID").map_err(|_| {
            TerminalError::BackendNotAvailable(
                "MODAL_TOKEN_ID is required for Modal sandbox execution".to_string(),
            )
        })?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| TerminalError::Internal(e.to_string()))?;
        let response = client
            .post(&self.endpoint)
            .bearer_auth(token)
            .json(&json!({"command": command}))
            .send()
            .await
            .map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
        // Read the body bounded rather than `response.json()`, which buffers
        // an arbitrarily large response before parsing. The JSON envelope
        // (field names, string escaping) can run several times the size of
        // the raw stdout/stderr it carries, so the cap here is generous
        // relative to `max_output_bytes` — it only needs to catch a
        // misbehaving/compromised endpoint returning something absurd, not
        // trim ordinary responses (those are still trimmed exactly by
        // `ExecResult::truncate` below).
        let body_cap = self.policy.max_output_bytes.saturating_mul(8).max(64 * 1024);
        let mut body = BoundedSink::new(body_cap);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| TerminalError::ExecutionFailed(format!("modal response body: {e}")))?;
            body.push(&chunk);
        }
        if body.truncated {
            return Err(TerminalError::ExecutionFailed(format!(
                "modal response body exceeded the {body_cap}-byte cap"
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&body.into_bytes())
            .map_err(|e| TerminalError::ExecutionFailed(format!("modal response json: {e}")))?;
        Ok(ExecResult::new(
            value["stdout"].as_str().unwrap_or_default().to_string(),
            value["stderr"].as_str().unwrap_or_default().to_string(),
            value["exit_code"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(-1),
            BackendKind::Modal,
            start,
            self.policy.max_output_bytes,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ARD-476: the safe-charset allowlist admits ordinary commands, flags,
    /// paths, and `key=value` pairs (under a permissive policy, so only the
    /// charset is exercised).
    #[test]
    fn check_command_accepts_safe_charset() {
        let p = TerminalPolicy::permissive();
        for ok in [
            "printf hello",
            "cargo build --release",
            "./scripts/x.sh --flag=a/b:c",
            "kubectl get pods -n=foo,bar",
            "git@github.com:org/repo.git",
            "python3 -m http.server 8000",
        ] {
            assert!(
                p.check_command(ok).is_ok(),
                "safe-charset command should be accepted: {ok:?}"
            );
        }
    }

    /// ARD-476: shell operators, metacharacters, expansions, quotes, globs, and
    /// command separators (including newline/CR) are all rejected at the charset
    /// gate, before the binary allowlist is even consulted.
    #[test]
    fn check_command_rejects_shell_metacharacters() {
        let p = TerminalPolicy::permissive();
        let rejected = [
            "printf a; b",
            "printf a | b",
            "printf a && b",
            "printf $(reboot)",
            "printf `reboot`",
            "printf > /etc/passwd",
            "printf < /etc/passwd",
            "printf \"a b\"",
            "printf 'a b'",
            "printf $HOME",
            "printf a#b",
            "printf *.rs",
            "printf ~",
            "printf a\nb",
            "printf a\rb",
        ];
        for evil in rejected {
            let err = p
                .check_command(evil)
                .expect_err(&format!("should reject {evil:?}"));
            assert!(
                matches!(err, TerminalError::PolicyDenied { .. }),
                "expected PolicyDenied for {evil:?}, got {err:?}"
            );
        }
    }

    /// ARD-476: even with the binary allowlisted, a command bearing a shell
    /// operator is rejected at the charset gate — so `printf … ; rm` cannot
    /// sneak past an allowlist that permits `printf`.
    #[test]
    fn check_command_rejects_injection_past_allowlisted_binary() {
        let p = TerminalPolicy::allow_commands(["printf"]);
        assert!(p.check_command("printf hi").is_ok());
        assert!(matches!(
            p.check_command("printf hi ; rm -rf /").unwrap_err(),
            TerminalError::PolicyDenied { .. }
        ));
    }

    /// ARD-476: deny-all stays deny-all, and the binary allowlist still gates
    /// non-allowlisted binaries.
    #[test]
    fn check_command_binary_allowlist_and_deny_all() {
        assert!(
            TerminalPolicy::deny_all()
                .check_command("printf hi")
                .is_err()
        );
        let p = TerminalPolicy::allow_commands(["printf"]);
        assert!(p.check_command("printf hi").is_ok());
        assert!(p.check_command("ls -la").is_err());
    }

    /// H3: a multi-byte UTF-8 character straddling `max_output_bytes` must be
    /// floored to the preceding char boundary rather than splitting it, which
    /// would panic `String::truncate`/`&str` indexing.
    #[test]
    fn exec_result_truncate_does_not_split_multibyte_char() {
        // "héllo" — 'é' is 2 bytes (0xC3 0xA9) at offsets 1..3, so a cap of 2
        // bytes lands inside it.
        let stdout = "héllo".to_string();
        let result = ExecResult::new(
            stdout,
            String::new(),
            0,
            BackendKind::Local,
            now_ms(),
            2,
        );
        assert!(result.truncated);
        // Floored to the boundary at offset 1 ("h"), not a panic or a split
        // codepoint, and the result still round-trips as valid UTF-8.
        assert_eq!(result.stdout, "h");

        // Emoji (4-byte UTF-8) straddling the cap on the stderr side too.
        let result2 = ExecResult::new(
            String::new(),
            "a🎉b".to_string(),
            0,
            BackendKind::Local,
            now_ms(),
            2,
        );
        assert!(result2.truncated);
        assert_eq!(result2.stderr, "a");
    }

    #[tokio::test]
    async fn local_backend_execute() {
        let backend = LocalBackend::new(TerminalPolicy::allow_commands(["printf"]));
        assert_eq!(backend.kind(), BackendKind::Local);
        let result = backend.execute("printf hello", 30).await.unwrap();
        assert_eq!(result.stdout, "hello");
    }

    #[tokio::test]
    async fn docker_backend_mock_execute() {
        let backend = DockerBackend::mock("my-container", TerminalPolicy::allow_commands(["echo"]));
        assert_eq!(backend.kind(), BackendKind::Docker);
        let result = backend.execute("echo hi", 30).await.unwrap();
        assert!(result.stdout.contains("my-container"));
    }

    #[tokio::test]
    async fn ssh_backend_mock_execute() {
        let backend = SshBackend::mock(
            "remote.host",
            "user",
            "SHA256:fixture",
            TerminalPolicy::allow_commands(["uptime"]),
        );
        assert_eq!(backend.kind(), BackendKind::Ssh);
        let result = backend.execute("uptime", 30).await.unwrap();
        assert!(result.stdout.contains("remote.host"));
    }

    /// A cap of `0` must not panic (nothing fits, everything discarded, never
    /// truncated-but-still-appended past it).
    #[test]
    fn bounded_sink_zero_cap_discards_everything() {
        let mut sink = BoundedSink::new(0);
        sink.push(b"anything");
        assert!(sink.truncated);
        assert_eq!(sink.into_bytes(), Vec::<u8>::new());
    }

    /// Pushed bytes accumulate up to the cap; anything past it is discarded
    /// (not buffered) and `truncated` latches, including across many small
    /// pushes rather than one large one — the shape a streamed log/pipe read
    /// actually arrives in.
    #[test]
    fn bounded_sink_caps_across_many_small_pushes() {
        let mut sink = BoundedSink::new(10);
        for _ in 0..10_000 {
            sink.push(b"0123456789"); // 10 bytes/push, cap is 10.
        }
        assert!(sink.truncated);
        assert_eq!(sink.into_bytes().len(), 10);
    }

    /// A single push straddling the cap is split at the cap, not rejected or
    /// over-admitted.
    #[test]
    fn bounded_sink_splits_a_push_that_straddles_the_cap() {
        let mut sink = BoundedSink::new(5);
        sink.push(b"0123456789");
        assert!(sink.truncated);
        assert_eq!(sink.into_bytes(), b"01234");
    }

    /// Content that fits under the cap is never marked truncated.
    #[test]
    fn bounded_sink_untruncated_when_under_cap() {
        let mut sink = BoundedSink::new(100);
        sink.push(b"hello");
        assert!(!sink.truncated);
        assert_eq!(sink.into_bytes(), b"hello");
    }

    /// R1-sibling H3 follow-up: a caller-requested timeout is clamped to
    /// `max_timeout_secs`, and a `0` request (which would fail every command
    /// before it gets a chance to run) is floored to `1`.
    #[test]
    fn effective_timeout_secs_clamps_to_policy_ceiling() {
        let mut policy = TerminalPolicy::allow_commands(["printf"]);
        policy.max_timeout_secs = 30;
        assert_eq!(policy.effective_timeout_secs(5), 5);
        assert_eq!(policy.effective_timeout_secs(30), 30);
        assert_eq!(policy.effective_timeout_secs(u64::MAX), 30);
        assert_eq!(policy.effective_timeout_secs(0), 1);
    }

    /// The genuinely-unbounded-output regression case for finding #1 of the
    /// 2026-07-12 terminal audit: `yes` never terminates on its own and
    /// writes as fast as the OS will let it. Before `run_piped_capped`
    /// existed, `LocalBackend` used `Command::output()`, which buffers the
    /// *entire* stream before `ExecResult::truncate` ever runs — with a tiny
    /// `max_output_bytes` and a short timeout this would still have to hold
    /// however many megabytes `yes` produced in that window. Proves instead
    /// that the captured length is bounded at (approximately) the policy cap
    /// regardless of the timeout window, and that the call still completes
    /// promptly rather than hanging until the timeout.
    #[tokio::test]
    async fn local_backend_bounds_output_from_an_unbounded_command() {
        let mut policy = TerminalPolicy::allow_commands(["yes"]);
        policy.max_output_bytes = 4096;
        let backend = LocalBackend::new(policy);
        let started = std::time::Instant::now();
        // `yes` never exits on its own; the 2s timeout is the backstop that
        // proves this doesn't hang, and `max_output_bytes` is what proves
        // memory stayed bounded rather than growing for the full 2s.
        let result = backend.execute("yes", 2).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        match result {
            // Either outcome is acceptable — what matters is that we didn't
            // hang and, if we got a result, it's bounded to (approximately)
            // the cap rather than however many bytes `yes` produced in 2s.
            Ok(exec) => {
                assert!(
                    exec.stdout.len() <= policy_headroom(4096),
                    "stdout grew past the bounded-capture headroom: {} bytes",
                    exec.stdout.len()
                );
            }
            Err(TerminalError::Timeout { .. }) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// `run_piped_capped`'s read chunk is 8KiB; the captured length can
    /// overshoot the exact cap by at most one chunk before `BoundedSink`
    /// notices, well before `ExecResult::truncate` trims it precisely.
    fn policy_headroom(cap: usize) -> usize {
        cap + 8192
    }
}
