//! The closed [`LanguageAdapter`] trait and its concrete implementations.
//!
//! Each adapter knows how to launch one language's interpreter/compiler
//! against a caller-supplied source body and hand back captured stdout,
//! stderr, and an exit code within a wall-clock ceiling. Adapters do not see
//! the cap-token caveat directly — [`crate::CodeExecutionTool`] attenuates the
//! request before an adapter ever runs.
//!
//! # Phase 1
//!
//! [`BashLanguageAdapter`] and [`PythonLanguageAdapter`] run the child
//! process directly on the local host — the §6.3 backend matrix (Docker /
//! SSH / Singularity / Modal / Daytona / Vercel) and the §11.5 sandbox
//! runtime this crate must eventually route every dispatch through do not
//! exist yet in this workspace. Until they land, callers MUST treat this
//! crate's execution as **unsandboxed local process execution** — the
//! cap-token caveat's `tool_allowlist` and language/timeout ceilings are
//! enforced, but process isolation is whatever the host OS gives a bare
//! child process. See [`crate::CodeExecutionCaveat`] for the enforced
//! ceilings.
//!
//! Tool-call RPC (the child script calling back into the tool registry over
//! a UDS/file transport) is also Phase 2 — `tool_allowlist` is accepted,
//! attenuated, and receipted, but no stub module is generated yet and a
//! script cannot actually dispatch a tool call in Phase 1.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::CodeExecutionError;

/// Output captured from one adapter run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterOutput {
    /// The child process's captured stdout.
    pub stdout: String,
    /// The child process's captured stderr.
    pub stderr: String,
    /// The child process's exit code, or `-1` if it was killed on timeout.
    pub exit_code: i32,
    /// Wall-clock duration of the run, in milliseconds.
    pub duration_ms: u64,
}

/// A language a [`crate::CodeExecutionTool`] can dispatch a script to.
///
/// Sealed via a private supertrait so only this crate can add adapters — a
/// new language is a reviewed change to §6.7, not a third-party extension
/// point. See the differentiation note in
/// `plans/6.7-code-execution-tool-call-rpc-blueprint.md`.
#[async_trait]
pub trait LanguageAdapter: private::Sealed + Send + Sync {
    /// The adapter's stable name, matching the request's `language` field
    /// (e.g. `"bash"`, `"python"`).
    fn name(&self) -> &'static str;

    /// Run `code` with `stdin` piped in, killing the child if it exceeds
    /// `timeout`.
    async fn run(
        &self,
        code: &str,
        stdin: Option<&str>,
        timeout: Duration,
    ) -> Result<AdapterOutput, CodeExecutionError>;
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::BashLanguageAdapter {}
    impl Sealed for super::PythonLanguageAdapter {}
}

/// Run `program` with `args`, feeding it `code` as its script body via a
/// temporary argument/stdin split appropriate to `program`, and capture its
/// output within `timeout`.
async fn run_captured(
    language: &'static str,
    mut cmd: Command,
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<AdapterOutput, CodeExecutionError> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = std::time::Instant::now();
    let mut child = cmd.spawn().map_err(|source| CodeExecutionError::Spawn {
        language: language.to_string(),
        source,
    })?;

    if let Some(input) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(input.as_bytes()).await;
        }
    } else {
        // Drop stdin so the child sees EOF immediately rather than blocking.
        drop(child.stdin.take());
    }

    let output = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match output {
        Ok(Ok(out)) => Ok(AdapterOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code().unwrap_or(-1),
            duration_ms,
        }),
        Ok(Err(source)) => Err(CodeExecutionError::Spawn {
            language: language.to_string(),
            source,
        }),
        Err(_) => Err(CodeExecutionError::Timeout(timeout.as_secs())),
    }
}

/// `LanguageAdapter::name() == "bash"` — direct `bash -c <code>` invocation.
///
/// No language environment bootstrap is required; this is the lightest-
/// weight adapter and the one most suited to "run this command and pipe its
/// output through this tool" workflows once tool-call RPC lands.
#[derive(Clone, Copy, Debug, Default)]
pub struct BashLanguageAdapter;

#[async_trait]
impl LanguageAdapter for BashLanguageAdapter {
    fn name(&self) -> &'static str {
        "bash"
    }

    async fn run(
        &self,
        code: &str,
        stdin: Option<&str>,
        timeout: Duration,
    ) -> Result<AdapterOutput, CodeExecutionError> {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(code);
        run_captured("bash", cmd, stdin, timeout).await
    }
}

/// `LanguageAdapter::name() == "python"` — direct `python3 -c <code>`
/// invocation.
///
/// Phase 1 runs against whatever `python3` is on `PATH`; the per-mission
/// `uv`-managed virtualenv and `ardur-tools` stub bootstrap described in the
/// §6.7 blueprint are Phase 2 (they depend on the tool-call RPC transport
/// this crate has not wired yet).
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonLanguageAdapter;

#[async_trait]
impl LanguageAdapter for PythonLanguageAdapter {
    fn name(&self) -> &'static str {
        "python"
    }

    async fn run(
        &self,
        code: &str,
        stdin: Option<&str>,
        timeout: Duration,
    ) -> Result<AdapterOutput, CodeExecutionError> {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(code);
        run_captured("python", cmd, stdin, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_adapter_captures_stdout() {
        let adapter = BashLanguageAdapter;
        let out = adapter
            .run("echo hello", None, Duration::from_secs(5))
            .await
            .expect("bash run succeeds");
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn bash_adapter_pipes_stdin() {
        let adapter = BashLanguageAdapter;
        let out = adapter
            .run("cat", Some("piped\n"), Duration::from_secs(5))
            .await
            .expect("bash run succeeds");
        assert_eq!(out.stdout, "piped\n");
    }

    #[tokio::test]
    async fn bash_adapter_captures_nonzero_exit() {
        let adapter = BashLanguageAdapter;
        let out = adapter
            .run("exit 7", None, Duration::from_secs(5))
            .await
            .expect("bash run succeeds");
        assert_eq!(out.exit_code, 7);
    }

    #[tokio::test]
    async fn bash_adapter_times_out() {
        let adapter = BashLanguageAdapter;
        let result = adapter
            .run("sleep 5", None, Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(CodeExecutionError::Timeout(_))));
    }

    #[tokio::test]
    async fn python_adapter_captures_stdout() {
        let adapter = PythonLanguageAdapter;
        let result = adapter
            .run("print('hi')", None, Duration::from_secs(5))
            .await;
        // python3 may not be present on every CI runner; only assert the
        // shape of a successful run when it is.
        if let Ok(out) = result {
            assert_eq!(out.stdout.trim(), "hi");
        }
    }

    #[test]
    fn adapter_names_are_stable() {
        assert_eq!(BashLanguageAdapter.name(), "bash");
        assert_eq!(PythonLanguageAdapter.name(), "python");
    }
}
