//! [`ShellTool`] — run a shell command and capture its stdout, stderr, and exit
//! code.
//!
//! # ⚠️ Security warning
//!
//! This tool executes arbitrary commands through the system shell (`bash -c` on
//! Unix, `cmd /C` on Windows). It is the single most dangerous capability the
//! built-in toolset ships. **Treat every [`ShellTool`] as a remote-code-execution
//! primitive whose blast radius is whatever the host process can do.**
//!
//! - [`ShellTool::with_allowlist`] confines the tool to commands matching a
//!   caller-supplied set of prefixes. This is the only configuration suitable
//!   for any context where the model's input is not fully trusted.
//! - [`ShellTool::without_allowlist`] runs **anything**. It exists for local
//!   development only. Do not register it on a server, behind a public channel
//!   adapter, or anywhere an untrusted prompt can reach it.
//!
//! The allowlist is a prefix gate, not a sandbox: it does not parse shell
//! grammar, so an allowed prefix that invokes a shell built-in (`bash -c`, `env`,
//! `sh`, `xargs`, …) can still pivot to arbitrary execution. Allowlist only
//! genuinely-leaf commands, and pair the tool with the §11 capability + Cedar
//! layers for defence in depth.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// Default wall-clock ceiling for a command, in seconds, when the caller does
/// not supply `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// The policy [`ShellTool`] gates each command against.
enum Allowlist {
    /// Permit any command. Dev-only; see the module security warning.
    Any,
    /// Permit only commands matching one of these patterns. Each pattern is one
    /// or more `|`-separated prefixes.
    Patterns(Vec<String>),
}

impl Allowlist {
    /// Whether `command` is permitted under this policy.
    ///
    /// A pattern matches when the (leading-whitespace-trimmed) command equals
    /// one of its `|`-separated alternatives, or begins with one followed by
    /// whitespace — so `git` permits `git status` but not `gitfoo`.
    fn permits(&self, command: &str) -> bool {
        match self {
            Allowlist::Any => true,
            Allowlist::Patterns(patterns) => {
                let cmd = command.trim_start();
                patterns
                    .iter()
                    .flat_map(|p| p.split('|'))
                    .map(str::trim)
                    .filter(|alt| !alt.is_empty())
                    .any(|alt| {
                        cmd == alt
                            || cmd
                                .strip_prefix(alt)
                                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
                    })
            }
        }
    }
}

/// Arguments to a `shell.run` invocation.
#[derive(Deserialize)]
struct ShellArgs {
    /// The command line, run through the system shell.
    command: String,
    /// Wall-clock ceiling in seconds; the command is killed past it.
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    /// Working directory; falls back to the context's `cwd` when absent.
    #[serde(default)]
    cwd: Option<String>,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// A tool that runs a shell command and returns `{ stdout, stderr, exit_code,
/// timed_out }`.
///
/// See the [module security warning](self) before registering one. Construct
/// with [`ShellTool::with_allowlist`] for any non-dev context, or
/// [`ShellTool::without_allowlist`] for local development only.
pub struct ShellTool {
    schema: ToolSchema,
    allowlist: Allowlist,
    caps: Vec<Capability>,
}

impl ShellTool {
    /// The id [`ShellTool`] registers under.
    pub const ID: &'static str = "shell.run";

    /// A [`ShellTool`] confined to commands matching `commands`.
    ///
    /// Each entry is one or more `|`-separated command prefixes (e.g.
    /// `"git|cargo"` or `"ls"`). A command is permitted when it equals a prefix
    /// or begins with one followed by whitespace. A command matching nothing is
    /// refused with [`ToolError::Denied`]. This is the only construction
    /// appropriate where the prompt is not fully trusted.
    #[must_use]
    pub fn with_allowlist(commands: Vec<String>) -> Self {
        Self::build(Allowlist::Patterns(commands))
    }

    /// A [`ShellTool`] that permits **any** command.
    ///
    /// # ⚠️ Dev use only
    ///
    /// This is unrestricted remote code execution: it runs whatever the model
    /// asks. Never register it on a server or any surface an untrusted prompt
    /// can reach. Use [`ShellTool::with_allowlist`] in production.
    #[must_use]
    pub fn without_allowlist() -> Self {
        Self::build(Allowlist::Any)
    }

    fn build(allowlist: Allowlist) -> Self {
        let schema = ToolSchema {
            description: "Run a shell command. Returns stdout, stderr, exit code.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line to run." },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Wall-clock ceiling in seconds (default 30).",
                        "minimum": 1
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory; defaults to the session cwd."
                    }
                },
                "required": ["command"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "exit_code": { "type": "integer" },
                    "timed_out": { "type": "boolean" }
                },
                "required": ["stdout", "stderr", "exit_code", "timed_out"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            allowlist,
            // ShellExec is the headline grant; ProcessSpawn names the fork/exec
            // the shell performs, so both authorization layers see the full
            // surface this tool touches.
            caps: vec![Capability::ShellExec, Capability::ProcessSpawn],
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`command` must not be empty".to_string(),
            ));
        }
        if !self.allowlist.permits(&args.command) {
            return Err(ToolError::Denied {
                reason: format!("command is not on the shell allowlist: `{}`", args.command),
            });
        }

        let cwd = args.cwd.map_or_else(|| ctx.cwd.clone(), Into::into);

        // `bash -c`/`cmd /C` so the command line is interpreted as a shell would.
        #[cfg(windows)]
        let (program, flag) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (program, flag) = ("bash", "-c");

        let mut cmd = Command::new(program);
        cmd.arg(flag)
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // On timeout the `wait_with_output` future (which owns the child) is
            // dropped; `kill_on_drop` ensures the child is reaped rather than
            // left running.
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to spawn `{program}`: {e}")))?;

        let timeout = Duration::from_secs(args.timeout_secs);
        let content = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => json!({
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                // A signal-terminated process has no code; report -1.
                "exit_code": output.status.code().unwrap_or(-1),
                "timed_out": false,
            }),
            Ok(Err(e)) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "command i/o failed: {e}"
                )));
            }
            Err(_elapsed) => json!({
                "stdout": "",
                "stderr": "",
                "exit_code": -1,
                "timed_out": true,
            }),
        };

        Ok(ToolOutput {
            content: content.clone(),
            cost: CostTuple::default(),
            receipt_data: content,
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}
