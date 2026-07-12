//! Terminal backend implementations.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bollard::Docker;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;

use crate::error::{Result, TerminalError};

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
}

impl TerminalPolicy {
    /// Strict deny-all policy.
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            command_allowlist: Vec::new(),
            max_output_bytes: 1024 * 1024,
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
        }
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
        let stdout_cap = max.min(self.stdout.len());
        truncate_to_char_boundary(&mut self.stdout, stdout_cap);
        let remaining = max.saturating_sub(self.stdout.len());
        truncate_to_char_boundary(&mut self.stderr, remaining);
    }
}

/// Truncate `s` to at most `max_bytes`, snapping the cut **down** to the nearest
/// UTF-8 char boundary.
///
/// All backends build their output with [`String::from_utf8_lossy`], so a
/// multi-byte character can straddle a raw byte limit. `String::truncate`
/// **panics** when handed an index that is not a char boundary, so truncating at
/// `max_output_bytes` directly is a reachable turn-abort DoS: any allowlisted
/// command whose combined output crosses the cap on a multi-byte char would kill
/// the tool call. Dropping the straddling character whole keeps the result valid
/// UTF-8 and can only make the output shorter than the cap, never longer.
fn truncate_to_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    s.truncate(boundary);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
        let start = now_ms();
        // `check_command` guarantees a non-empty, safe-charset command, so the
        // first token is the binary and the rest are literal arguments. Exec it
        // directly — no `/bin/sh -c` — so no metacharacter can ever be
        // interpreted (ARD-476).
        let mut tokens = command.split_whitespace();
        let binary = tokens
            .next()
            .expect("check_command guarantees a non-empty command");
        let fut = Command::new(binary)
            .args(tokens)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut)
            .await
            .map_err(|_| TerminalError::Timeout { secs: timeout_secs })?
            .map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
        Ok(ExecResult::new(
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
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

        let run = async {
            let mut stdout = String::new();
            let mut stderr = String::new();
            match docker
                .start_exec(&exec_id, None::<bollard::exec::StartExecOptions>)
                .await
                .map_err(|e| TerminalError::ExecutionFailed(format!("docker start_exec: {e}")))?
            {
                bollard::exec::StartExecResults::Attached { mut output, .. } => {
                    while let Some(item) = output.next().await {
                        let item =
                            item.map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
                        let text = item.to_string();
                        if matches!(item, bollard::container::LogOutput::StdErr { .. }) {
                            stderr.push_str(&text);
                        } else {
                            stdout.push_str(&text);
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
                .unwrap_or(0);
            Ok(ExecResult::new(
                stdout,
                stderr,
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
        let fut = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg(target)
            .arg(command)
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut)
            .await
            .map_err(|_| TerminalError::Timeout { secs: timeout_secs })?
            .map_err(|e| TerminalError::ExecutionFailed(e.to_string()))?;
        Ok(ExecResult::new(
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
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
        let value: serde_json::Value = response
            .json()
            .await
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

    /// ARD-H3: truncating output at a raw byte offset that lands inside a
    /// multi-byte UTF-8 character must not panic — it snaps down to the char
    /// boundary, dropping the straddling character whole.
    #[test]
    fn truncate_to_char_boundary_snaps_down_without_panicking() {
        // "é" is two bytes (0xC3 0xA9). Cutting at 3 bytes lands mid-character.
        let mut s = "aé".to_string(); // bytes: 'a'(1) + 'é'(2) = 3
        truncate_to_char_boundary(&mut s, 2);
        assert_eq!(s, "a", "the straddling multi-byte char is dropped whole");
        assert!(std::str::from_utf8(s.as_bytes()).is_ok());

        // A boundary that already lands cleanly is untouched.
        let mut clean = "abc".to_string();
        truncate_to_char_boundary(&mut clean, 2);
        assert_eq!(clean, "ab");

        // A limit at or beyond the length is a no-op.
        let mut short = "aé".to_string();
        truncate_to_char_boundary(&mut short, 99);
        assert_eq!(short, "aé");
    }

    /// The end-to-end `ExecResult::new` truncation path (the one that used to
    /// panic) survives multi-byte output crossing the cap on a char boundary.
    #[test]
    fn exec_result_truncation_survives_multibyte_boundary() {
        // 10 × "é" = 20 bytes of valid UTF-8; cap at 5 bytes lands mid-char.
        let stdout = "é".repeat(10);
        let result = ExecResult::new(stdout, String::new(), 0, BackendKind::Local, now_ms(), 5);
        assert!(result.truncated);
        assert!(result.stdout.len() <= 5);
        assert!(
            std::str::from_utf8(result.stdout.as_bytes()).is_ok(),
            "truncated output is still valid UTF-8"
        );
        // 5 bytes floors to 2 whole "é" characters (4 bytes).
        assert_eq!(result.stdout, "éé");
    }

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
}
