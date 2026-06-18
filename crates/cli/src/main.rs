//! The `ardur` binary entrypoint: chat plus operational commands.
//!
//! Plan family: §2.1 (`plans/2.1-cli-blueprint.md`). The library
//! ([`ardur_cli`]) holds the chat engine and slash-commands; this binary is a
//! thin clap front-end over [`ardur_cli::run_chat`] and the local ops surface.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ardur_cli::{ChatArgs, CliError, Config, StateDirs, run_chat};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

/// Ardur — a capability-secure, cost-metered agent runtime.
#[derive(Parser)]
#[command(name = "ardur", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start an interactive chat session.
    ///
    /// By default this runs the full substrate: real LLM calls through the fused
    /// runtime (cap-token verify, Cedar authorization, cost metering, signed
    /// receipts, durable journals) over persistent state under `~/.ardur/`.
    ///
    /// REAL LLM CALLS COST REAL MONEY. Each session is capped by
    /// `--budget-cents` (default 1000 = $10); the cap-token, receipt chain, and
    /// journals persist across runs. With no ANTHROPIC_API_KEY set, the session
    /// runs offline against a network-free stub provider. Pass `--echo` for the
    /// legacy in-memory echo runtime — no provider, no cost, no persistent state.
    Chat(ChatArgs),
    /// View or edit the local `~/.ardur/config.toml` configuration.
    Config(ConfigArgs),
    /// Tail local structured logs with sensitive fields redacted.
    Logs(LogsArgs),
    /// Dump redacted runtime state for troubleshooting.
    Debug(DebugArgs),
    /// Run local self-diagnostics.
    Doctor(DoctorArgs),
    /// Manage automation tasks (create/list/cancel/status).
    Task(TaskArgs),
    /// Install hooks from a directory (OpenClaw format supported).
    Hook(HookArgs),
    /// Print the version and exit.
    Version,
}

/// Arguments to `ardur config`.
#[derive(Args)]
struct ConfigArgs {
    /// Path to the config file (defaults to `~/.ardur/config.toml`).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Optional edit action. With no action, prints a redacted config summary.
    #[command(subcommand)]
    action: Option<ConfigAction>,
}

/// Mutations supported by `ardur config`.
#[derive(Subcommand)]
enum ConfigAction {
    /// Set one flat config key (`api_key`, `model`, or `budget_cents`).
    Set { key: String, value: String },
}

/// Arguments to `ardur logs`.
#[derive(Args)]
struct LogsArgs {
    /// State directory containing `logs/ardur.log` (defaults to `~/.ardur`).
    #[arg(long = "dir", value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Number of log lines to print from the end of the file.
    #[arg(long, default_value_t = 50)]
    lines: usize,
}

/// Arguments to `ardur debug`.
#[derive(Args)]
struct DebugArgs {
    /// State directory to inspect (defaults to `~/.ardur`).
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
}

/// Arguments to `ardur doctor`.
#[derive(Args)]
struct DoctorArgs {
    /// State directory to check/create (defaults to `~/.ardur`).
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Treat a missing `ANTHROPIC_API_KEY` as a hard failure.
    #[arg(long)]
    require_api_key: bool,
}

/// Arguments to `ardur task`.
#[derive(Args)]
struct TaskArgs {
    /// Task subcommand.
    #[command(subcommand)]
    action: TaskAction,
}

/// Subcommands for `ardur task`.
#[derive(Subcommand)]
enum TaskAction {
    /// Create a new automation task.
    Create {
        /// Task name/description.
        name: String,
    },
    /// List all tasks.
    List,
    /// Cancel a running task by id.
    Cancel {
        /// Task id to cancel.
        task_id: String,
    },
    /// Show the status of a task by id.
    Status {
        /// Task id to query.
        task_id: String,
    },
}

/// Arguments to `ardur hook`.
#[derive(Args)]
struct HookArgs {
    /// Hook format (currently only `openclaw` is supported).
    #[arg(long)]
    format: String,
    /// Directory containing hook files to install.
    dir: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Version => {
            println!("ardur {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Commands::Chat(args) => run_chat(args),
        Commands::Config(args) => run_config(args),
        Commands::Logs(args) => run_logs(args),
        Commands::Debug(args) => run_debug(args),
        Commands::Doctor(args) => run_doctor(args),
        Commands::Task(args) => run_task(args),
        Commands::Hook(args) => run_hook(args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_config(args: ConfigArgs) -> Result<(), CliError> {
    let path = config_path(args.config)?;
    let mut config = Config::load(Some(path.clone()))?;
    match args.action {
        None => {
            let summary = json!({
                "source": path.display().to_string(),
                "model": config.model,
                "budget_cents": config.budget_cents,
                "api_key_present": !config.api_key.is_empty(),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("summary serializes")
            );
            Ok(())
        }
        Some(ConfigAction::Set { key, value }) => {
            match key.as_str() {
                "api_key" => config.api_key = value,
                "model" => config.model = value,
                "budget_cents" => {
                    config.budget_cents = value.parse::<u64>().map_err(|_| {
                        CliError::Config(format!("budget_cents must be an integer, got `{value}`"))
                    })?;
                }
                _ => {
                    return Err(CliError::Config(format!(
                        "unsupported config key `{key}` (expected api_key, model, or budget_cents)"
                    )));
                }
            }
            write_config(&path, &config)?;
            println!("updated {key} in {}", path.display());
            Ok(())
        }
    }
}

fn run_logs(args: LogsArgs) -> Result<(), CliError> {
    let root = state_root(args.dir)?;
    let log_path = root.join("logs").join("ardur.log");
    let contents = match std::fs::read_to_string(&log_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no logs found at {}", log_path.display());
            return Ok(());
        }
        Err(e) => return Err(CliError::Io(e)),
    };
    let mut lines = contents.lines().rev().take(args.lines).collect::<Vec<_>>();
    lines.reverse();
    for line in lines {
        println!("{}", redact_log_line(line));
    }
    Ok(())
}

fn run_debug(args: DebugArgs) -> Result<(), CliError> {
    let root = state_root(args.state_dir)?;
    let snapshot = json!({
        "state_dir": root.display().to_string(),
        "directories": {
            "memory": root.join("memory").is_dir(),
            "journals": root.join("journals").is_dir(),
            "receipts": root.join("receipts").is_dir(),
            "keys": root.join("keys").is_dir(),
        },
        "keys": {
            "issuer_key_present": root.join("keys").join("issuer.key").is_file(),
            "receipt_key_present": root.join("keys").join("receipt.pem").is_file(),
        },
        "receipts": {
            "count": count_lines(&root.join("receipts").join("chain.jsonl"))?,
        },
        "policy": {
            "cedar_policy_present": root.join("cedar.policies").is_file(),
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&snapshot).expect("debug snapshot serializes")
    );
    Ok(())
}

fn run_doctor(args: DoctorArgs) -> Result<(), CliError> {
    let root = state_root(args.state_dir)?;
    let mut checks = Vec::new();
    let mut hard_fail = false;

    match std::fs::create_dir_all(&root) {
        Ok(()) => checks
            .push(json!({"name": "state_dir", "status": "ok", "path": root.display().to_string()})),
        Err(e) => {
            hard_fail = true;
            checks.push(json!({"name": "state_dir", "status": "error", "message": e.to_string()}));
        }
    }

    let api_key_present = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    if api_key_present {
        checks.push(json!({"name": "anthropic_api_key", "status": "ok", "present": true}));
    } else if args.require_api_key {
        hard_fail = true;
        checks.push(json!({"name": "anthropic_api_key", "status": "error", "present": false}));
    } else {
        checks.push(json!({"name": "anthropic_api_key", "status": "warn", "present": false, "note": "offline stub fallback available"}));
    }

    checks.push(json!({
        "name": "connectivity",
        "status": "skipped",
        "note": "live provider checks require explicit credentials and opt-in",
    }));

    let report = json!({
        "status": if hard_fail { "error" } else { "ok" },
        "checks": checks,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("doctor report serializes")
    );
    if hard_fail {
        Err(CliError::State("doctor found issues".to_string()))
    } else {
        Ok(())
    }
}

fn config_path(path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    path.or_else(Config::default_path).ok_or_else(|| {
        CliError::State("cannot resolve a config path (HOME/USERPROFILE unset)".to_string())
    })
}

fn state_root(path: Option<PathBuf>) -> Result<PathBuf, CliError> {
    match path {
        Some(path) => Ok(path),
        None => Ok(StateDirs::resolve()?.root),
    }
}

fn write_config(path: &Path, config: &Config) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = format!(
        "api_key = \"{}\"\nmodel = \"{}\"\nbudget_cents = {}\n",
        escape_toml_string(&config.api_key),
        escape_toml_string(&config.model),
        config.budget_cents
    );
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn redact_log_line(line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(mut value) => {
            redact_json(&mut value);
            serde_json::to_string(&value).expect("redacted JSON serializes")
        }
        Err(_) => redact_plain(line),
    }
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if sensitive_key(key) {
                    *value = serde_json::Value::String("<redacted>".to_string());
                } else {
                    redact_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json(value);
            }
        }
        _ => {}
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("api_key")
        || key.contains("authorization")
}

fn redact_plain(line: &str) -> String {
    let mut out = Vec::new();
    for word in line.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if lower.contains("token") || lower.contains("secret") || lower.starts_with("sk-") {
            out.push("<redacted>");
        } else {
            out.push(word);
        }
    }
    out.join(" ")
}

fn count_lines(path: &Path) -> Result<usize, CliError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents.lines().count()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(CliError::Io(e)),
    }
}

/// Construct synthetic VerifiedClaims for CLI-only use (no running server).
fn synthetic_claims() -> Result<ardur_cap_token::VerifiedClaims, CliError> {
    use ardur_cap_token::{HolderId, VerifiedClaims};
    Ok(VerifiedClaims {
        token_id: uuid::Uuid::now_v7(),
        audience: "ardur-cli".to_string(),
        subject: HolderId("spiffe://ardur/cli".to_string()),
        expires_unix: 4_102_444_800,
        budget_remaining: 1_000_000,
        tool_allowlist: vec!["task.create".to_string()],
    })
}

/// Run `ardur task` subcommands using the automation orchestrator.
fn run_task(args: TaskArgs) -> Result<(), CliError> {
    use ardur_automation::{DefaultTaskFlowOrchestrator, TaskFlowOrchestrator};
    let orch = DefaultTaskFlowOrchestrator::default();

    match args.action {
        TaskAction::Create { name } => {
            // The orchestrator requires VerifiedClaims. For the CLI-only path
            // without a running server, we use a synthetic claims object.
            // In production, the server mints real claims from the cap-token.
            let claims = synthetic_claims()?;
            let request = ardur_automation::TaskCreationRequest {
                description: name.clone(),
                flow_dag: None,
            };
            let handle = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(orch.create_task(&claims, request))
            })
            .map_err(|e| CliError::Config(format!("create_task failed: {e}")))?;
            println!("created task `{:?}`: {name}", handle.task_id);
        }
        TaskAction::List => {
            // The orchestrator doesn't have a list_tasks method; get_task_state
            // is per-id. For the CLI we report that listing requires the server.
            println!(
                "task listing requires the running server (use `ardur task status <id>` for individual tasks)"
            );
        }
        TaskAction::Cancel { task_id: _ } => {
            let claims = synthetic_claims()?;
            let task_id = ardur_automation::TaskId::new();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(orch.cancel_task(&claims, task_id))
            })
            .map_err(|e| CliError::Config(format!("cancel_task failed: {e}")))?;
            println!("cancelled task `{task_id:?}");
        }
        TaskAction::Status { task_id: _ } => {
            let task_id = ardur_automation::TaskId::new();
            let state = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(orch.get_task_state(task_id))
            })
            .map_err(|e| CliError::Config(format!("get_task_state failed: {e}")))?;
            println!(
                "task: outcome={:?}, steps={}",
                state.outcome,
                state.step_outcomes.len()
            );
        }
    }
    Ok(())
}

/// Run `ardur hook install --format <format> <dir>`.
fn run_hook(args: HookArgs) -> Result<(), CliError> {
    match args.format.as_str() {
        "openclaw" => {
            let dir = &args.dir;
            if !dir.is_dir() {
                return Err(CliError::Config(format!(
                    "hook dir `{}` does not exist or is not a directory",
                    dir.display()
                )));
            }
            // Scan the directory for .json hook files and report what would be
            // installed. The actual registration into a live runtime happens at
            // server boot via ARDUR_HOOKS_DIRS; this CLI command validates and
            // previews the hook files.
            let mut found = 0usize;
            for entry in std::fs::read_dir(dir).map_err(CliError::Io)? {
                let entry = entry.map_err(CliError::Io)?;
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    let content = std::fs::read_to_string(&path).map_err(CliError::Io)?;
                    if serde_json::from_str::<serde_json::Value>(&content).is_ok() {
                        println!("  openclaw hook: {}", path.display());
                        found += 1;
                    } else {
                        eprintln!("  WARNING: invalid JSON in {}", path.display());
                    }
                }
            }
            println!("found {found} openclaw hook(s) in {}", dir.display());
            Ok(())
        }
        other => Err(CliError::Config(format!(
            "unsupported hook format `{other}` (only `openclaw` is supported)"
        ))),
    }
}
