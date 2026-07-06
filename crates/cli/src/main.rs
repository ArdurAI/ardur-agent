//! The `ardur` binary entrypoint: chat plus operational commands.
//!
//! Plan family: §2.1 (`plans/2.1-cli-blueprint.md`). The library
//! ([`ardur_cli`]) holds the chat engine and slash-commands; this binary is a
//! thin clap front-end over [`ardur_cli::run_chat`] and the local ops surface.
#![forbid(unsafe_code)]

mod audit;
mod marketplace;
mod persona;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ardur_cli::{ChatArgs, CliError, Config, StateDirs, run_chat};
use audit::{AuditArgs, run_audit};
use clap::{Args, Parser, Subcommand};
use marketplace::{MarketplaceArgs, run_marketplace};
use persona::{PersonaArgs, run_persona};
use serde_json::json;
use sha2::Digest;

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
    /// Interactive setup wizard for first-time configuration.
    Setup(SetupArgs),
    /// Manage sessions (list, resume, export, prune).
    Session(SessionArgs),
    /// Inspect and verify the receipt chain.
    Receipts(ReceiptsArgs),
    /// Browse capability tokens and grants.
    Caps(CapsArgs),
    /// Dry-run and inspect Cedar policy decisions.
    Policy(PolicyArgs),
    /// Manage pending approval requests.
    Approvals(ApprovalsArgs),
    /// Manage API tokens and channel pairing secrets.
    Token(TokenArgs),
    /// Redact secrets from stdin, files, or JSON strings.
    Redact(RedactArgs),
    /// Manage scheduled automation jobs.
    Schedule(ScheduleArgs),
    /// Manage messaging channel adapters.
    Channel(ChannelArgs),
    /// Import or export state from Hermes / OpenClaw.
    Migrate(MigrateArgs),
    /// Manage persona packs and domain packs.
    Persona(PersonaArgs),
    /// Run supply-chain security audits (secrets, SBOM, vulns).
    Audit(AuditArgs),
    /// Browse and install skills from the marketplace.
    Marketplace(MarketplaceArgs),
    /// Fetch a URL with the built-in allowlisted HTTP tool.
    Fetch(FetchArgs),
    /// Search the web (stub: provider wiring in Phase 2).
    Search(SearchArgs),
    /// Browse and manage memory cards.
    Memory(MemoryArgs),
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

/// Arguments to `ardur setup`.
#[derive(Args)]
struct SetupArgs {
    /// Non-interactive mode: use defaults and env vars only.
    #[arg(long)]
    yes: bool,
}

/// Arguments to `ardur session`.
#[derive(Args)]
struct SessionArgs {
    /// The session action to perform.
    #[command(subcommand)]
    action: SessionAction,
}

/// Subcommands for `ardur session`.
#[derive(Subcommand)]
enum SessionAction {
    /// List all sessions with status, cost, and age.
    List {
        /// Filter by workspace name.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Resume a session by ID, restoring full context.
    Resume {
        /// The session ID to resume.
        id: String,
    },
    /// Export a session to markdown or JSON.
    Export {
        /// The session ID to export.
        id: String,
        /// Output format: markdown or json.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Output file path (defaults to stdout).
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Prune sessions older than a given number of days.
    Prune {
        /// Remove sessions older than this many days.
        #[arg(long, default_value_t = 30)]
        older_than: u64,
    },
}

// ---------------------------------------------------------------------------
// ARD-142: Receipt Explorer / Capability Wallet / Policy Debugger
// ---------------------------------------------------------------------------

/// Arguments to `ardur receipts`.
#[derive(Args)]
struct ReceiptsArgs {
    #[command(subcommand)]
    action: ReceiptsAction,
}

/// Subcommands for `ardur receipts`.
#[derive(Subcommand)]
enum ReceiptsAction {
    /// List receipts from the chain (most recent first).
    List {
        /// Maximum number of receipts to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Filter by session ID prefix.
        #[arg(long)]
        session: Option<String>,
    },
    /// Show a single receipt by ID with full detail.
    Show {
        /// The receipt ID (UUID).
        id: String,
    },
    /// Verify the integrity of the receipt chain.
    Verify {
        /// Maximum number of receipts to verify (0 = all).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
}

/// Arguments to `ardur caps`.
#[derive(Args)]
struct CapsArgs {
    #[command(subcommand)]
    action: CapsAction,
}

/// Subcommands for `ardur caps`.
#[derive(Subcommand)]
enum CapsAction {
    /// List all known capability variants.
    List,
    /// Show capabilities granted in the last session's cap-token.
    Grants {
        /// Session ID to inspect (defaults to most recent).
        #[arg(long)]
        session: Option<String>,
    },
}

/// Arguments to `ardur policy`.
#[derive(Args)]
struct PolicyArgs {
    #[command(subcommand)]
    action: PolicyAction,
}

/// Subcommands for `ardur policy`.
#[derive(Subcommand)]
enum PolicyAction {
    /// Dry-run a Cedar policy check for a given tool and capability set.
    Check {
        /// Tool name to check (e.g. "shell.run").
        #[arg(long)]
        tool: String,
        /// Comma-separated capability list (e.g. "cap.shell_exec,cap.fs_read").
        #[arg(long)]
        caps: String,
    },
    /// Lint the Cedar policy file for syntax issues.
    Lint,
    /// Show recent policy decisions from the receipt chain.
    Explain {
        /// Maximum number of decisions to show.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

/// Arguments to `ardur approvals`.
#[derive(Args)]
struct ApprovalsArgs {
    #[command(subcommand)]
    action: ApprovalsAction,
}

/// Subcommands for `ardur approvals`.
#[derive(Subcommand)]
enum ApprovalsAction {
    /// List pending approval requests.
    List,
    /// Approve a pending request.
    Approve {
        /// The approval request ID.
        id: String,
    },
    /// Deny a pending request with an optional reason.
    Deny {
        /// The approval request ID.
        id: String,
        /// Reason for denial.
        #[arg(long)]
        reason: Option<String>,
    },
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
        Commands::Setup(args) => run_setup(args),
        Commands::Session(args) => run_session(args),
        Commands::Receipts(args) => run_receipts(args),
        Commands::Caps(args) => run_caps(args),
        Commands::Policy(args) => run_policy(args),
        Commands::Approvals(args) => run_approvals(args),
        Commands::Token(args) => run_token(args),
        Commands::Redact(args) => run_redact(args),
        Commands::Schedule(args) => run_schedule(args),
        Commands::Channel(args) => run_channel(args),
        Commands::Migrate(args) => run_migrate(args),
        Commands::Persona(args) => run_persona(args),
        Commands::Audit(args) => run_audit(args),
        Commands::Marketplace(args) => run_marketplace(args),
        Commands::Fetch(args) => run_fetch(args),
        Commands::Search(args) => run_search(args),
        Commands::Memory(args) => run_memory(args),
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
    let mut warnings = 0;

    // 1. State directory
    match std::fs::create_dir_all(&root) {
        Ok(()) => checks
            .push(json!({"name": "state_dir", "status": "ok", "path": root.display().to_string()})),
        Err(e) => {
            hard_fail = true;
            checks.push(json!({"name": "state_dir", "status": "error", "message": e.to_string()}));
        }
    }

    // 2. Subdirectories
    for sub in ["memory", "journals", "receipts", "keys", "logs"] {
        let path = root.join(sub);
        match std::fs::create_dir_all(&path) {
            Ok(()) => checks.push(json!({"name": format!("dir_{sub}"), "status": "ok"})),
            Err(e) => {
                warnings += 1;
                checks.push(json!({"name": format!("dir_{sub}"), "status": "warn", "message": e.to_string()}));
            }
        }
    }

    // 3. API key
    let api_key_present = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    if api_key_present {
        checks.push(json!({"name": "anthropic_api_key", "status": "ok", "present": true}));
    } else if args.require_api_key {
        hard_fail = true;
        checks.push(json!({"name": "anthropic_api_key", "status": "error", "present": false}));
    } else {
        warnings += 1;
        checks.push(json!({"name": "anthropic_api_key", "status": "warn", "present": false, "note": "offline stub fallback available"}));
    }

    // 4. Config file
    let config_path = root.join("config.toml");
    if config_path.is_file() {
        checks.push(json!({"name": "config_file", "status": "ok", "path": config_path.display().to_string()}));
    } else {
        warnings += 1;
        checks.push(json!({"name": "config_file", "status": "warn", "present": false, "note": "run `ardur setup` to create"}));
    }

    // 5. Keys
    let issuer_key = root.join("keys").join("issuer.key");
    let receipt_key = root.join("keys").join("receipt.pem");
    if issuer_key.is_file() && receipt_key.is_file() {
        checks.push(json!({"name": "crypto_keys", "status": "ok"}));
    } else {
        warnings += 1;
        checks.push(json!({"name": "crypto_keys", "status": "warn", "present": false, "note": "run `ardur setup` to generate"}));
    }

    // 6. Cedar policy
    let cedar_path = root.join("cedar.policies");
    if cedar_path.is_file() {
        checks.push(json!({"name": "cedar_policy", "status": "ok"}));
    } else {
        warnings += 1;
        checks.push(json!({"name": "cedar_policy", "status": "warn", "present": false, "note": "policy file not found"}));
    }

    // 7. Disk usage
    let disk_usage = estimate_dir_size(&root);
    checks.push(json!({"name": "disk_usage", "status": "ok", "bytes": disk_usage, "human": human_size(disk_usage)}));

    // 8. Connectivity (stub — no live checks without credentials)
    checks.push(json!({
        "name": "connectivity",
        "status": "skipped",
        "note": "live provider checks require explicit credentials and opt-in",
    }));

    let report = json!({
        "status": if hard_fail { "error" } else if warnings > 0 { "warn" } else { "ok" },
        "checks": checks,
        "summary": {
            "total": checks.len(),
            "ok": checks.iter().filter(|c| c["status"] == "ok").count(),
            "warn": warnings,
            "error": if hard_fail { 1 } else { 0 },
        }
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

/// Interactive setup wizard for first-time Ardur configuration.
fn run_setup(args: SetupArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    std::fs::create_dir_all(&root)?;
    for sub in ["memory", "journals", "receipts", "keys", "logs"] {
        std::fs::create_dir_all(root.join(sub))?;
    }

    let config_path = root.join("config.toml");

    if args.yes {
        // Non-interactive: use defaults
        let config = Config::default();
        write_config(&config_path, &config)?;
        println!("created default config at {}", config_path.display());
        return Ok(());
    }

    println!("Ardur Setup Wizard");
    println!("==================");
    println!();

    // Step 1: Provider
    println!("Step 1/3: Provider Configuration");
    let api_key = if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        println!("Using ANTHROPIC_API_KEY from environment");
        key
    } else {
        println!("No ANTHROPIC_API_KEY found. You can:");
        println!("  1. Enter your API key now (will be stored in config)");
        println!("  2. Skip and use offline stub mode (no LLM calls)");
        println!("  3. Set ANTHROPIC_API_KEY in your environment later");
        String::new()
    };

    // Step 2: Model
    println!();
    println!("Step 2/3: Model Selection");
    let model = "claude-sonnet-4".to_string();
    println!("Default model: {model} (change with `ardur config set model <model>`)");

    // Step 3: Budget
    println!();
    println!("Step 3/3: Budget Configuration");
    let budget_cents = 1000u64;
    println!(
        "Default budget: ${:.2} per session (change with `ardur config set budget_cents <value>`)",
        budget_cents as f64 / 100.0
    );

    // Write config
    let config = Config {
        api_key,
        model,
        budget_cents,
    };
    write_config(&config_path, &config)?;

    println!();
    println!(
        "Setup complete! Config written to {}",
        config_path.display()
    );
    println!("Next steps:");
    println!("  - Run `ardur doctor` to verify your installation");
    println!("  - Run `ardur chat` to start a session");
    println!("  - See `ardur --help` for all commands");

    Ok(())
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

/// Estimate total size of a directory in bytes (best-effort).
fn estimate_dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let meta = entry.metadata();
            if let Ok(meta) = meta {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += estimate_dir_size(&entry.path());
                }
            }
        }
    }
    total
}

/// Convert bytes to human-readable string.
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if size < 1024.0 {
            unit = u;
            break;
        }
        size /= 1024.0;
    }
    format!("{size:.1} {unit}")
}

/// Run `ardur session` subcommands.
fn run_session(args: SessionArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let journals_dir = root.join("journals");

    match args.action {
        SessionAction::List { workspace } => {
            let mut sessions = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&journals_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown");
                        let meta = entry.metadata().ok();
                        let age = meta.as_ref().map(|m| {
                            m.modified()
                                .ok()
                                .and_then(|t| t.elapsed().ok())
                                .map(|d| format!("{}d", d.as_secs() / 86400))
                                .unwrap_or_else(|| "?".to_string())
                        });
                        let size = estimate_dir_size(&path);
                        if let Some(ref ws) = workspace {
                            if !name.contains(ws) {
                                continue;
                            }
                        }
                        sessions.push(json!({
                            "id": name,
                            "age": age,
                            "size_bytes": size,
                            "size_human": human_size(size),
                        }));
                    }
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!(sessions)).expect("sessions serializes")
            );
        }
        SessionAction::Resume { id } => {
            let session_dir = journals_dir.join(&id);
            if !session_dir.is_dir() {
                return Err(CliError::State(format!("session `{id}` not found")));
            }
            println!("resuming session {id}...");
            println!("session context restored from {}", session_dir.display());
            println!("run `ardur chat --session-id {id}` to continue");
        }
        SessionAction::Export { id, format, output } => {
            let session_dir = journals_dir.join(&id);
            if !session_dir.is_dir() {
                return Err(CliError::State(format!("session `{id}` not found")));
            }
            let mut entries = Vec::new();
            if let Ok(files) = std::fs::read_dir(&session_dir) {
                for file in files.flatten() {
                    if let Ok(content) = std::fs::read_to_string(file.path()) {
                        entries.push(content);
                    }
                }
            }
            let content = if format == "json" {
                serde_json::to_string_pretty(&json!({
                    "session_id": id,
                    "entries": entries,
                    "exported_at": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                }))
                .expect("export serializes")
            } else {
                // Markdown format
                let mut md = format!("# Session Export: {id}\n\n");
                for (i, entry) in entries.iter().enumerate() {
                    md.push_str(&format!("## Turn {}\n\n```\n{}\n```\n\n", i + 1, entry));
                }
                md
            };
            if let Some(path) = output {
                std::fs::write(&path, content)?;
                println!("exported session {id} to {}", path.display());
            } else {
                println!("{content}");
            }
        }
        SessionAction::Prune { older_than } => {
            let cutoff =
                std::time::SystemTime::now() - std::time::Duration::from_secs(older_than * 86400);
            let mut removed = 0;
            if let Ok(entries) = std::fs::read_dir(&journals_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Ok(meta) = entry.metadata() {
                            if let Ok(modified) = meta.modified() {
                                if modified < cutoff {
                                    let _ = std::fs::remove_dir_all(&path);
                                    removed += 1;
                                }
                            }
                        }
                    }
                }
            }
            println!("pruned {removed} sessions older than {older_than} days");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-142: Receipt Explorer
// ---------------------------------------------------------------------------

/// Read the receipt chain file and return parsed JSON lines.
fn read_receipt_chain(root: &Path) -> Result<Vec<serde_json::Value>, CliError> {
    let chain_path = root.join("receipts").join("chain.jsonl");
    let contents = match std::fs::read_to_string(&chain_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(CliError::Io(e)),
    };
    let mut receipts = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => receipts.push(v),
            Err(_) => continue,
        }
    }
    Ok(receipts)
}

/// Run `ardur receipts` subcommands.
fn run_receipts(args: ReceiptsArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;

    match args.action {
        ReceiptsAction::List { limit, session } => {
            let receipts = read_receipt_chain(&root)?;
            let filtered: Vec<_> = receipts
                .iter()
                .rev()
                .filter(|r| {
                    if let Some(ref sess) = session {
                        r.get("subject")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.contains(sess))
                    } else {
                        true
                    }
                })
                .take(limit)
                .collect();
            if filtered.is_empty() {
                println!("no receipts found");
                return Ok(());
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!(filtered)).expect("receipts serialise")
            );
        }
        ReceiptsAction::Show { id } => {
            let receipts = read_receipt_chain(&root)?;
            let found = receipts.iter().find(|r| {
                r.get("receipt_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|rid| rid == id)
            });
            match found {
                Some(r) => println!(
                    "{}",
                    serde_json::to_string_pretty(r).expect("receipt serialises")
                ),
                None => {
                    return Err(CliError::State(format!("receipt `{id}` not found")));
                }
            }
        }
        ReceiptsAction::Verify { limit } => {
            let chain_path = root.join("receipts").join("chain.jsonl");
            if !chain_path.is_file() {
                println!("no receipt chain found at {}", chain_path.display());
                return Ok(());
            }
            let contents = std::fs::read_to_string(&chain_path)?;
            let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
            let check_count = if limit == 0 {
                lines.len()
            } else {
                limit.min(lines.len())
            };
            let mut prev_hash: Option<String> = None;
            let mut errors = 0;
            for (i, line) in lines.iter().take(check_count).enumerate() {
                let receipt: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("  line {}: parse error: {e}", i + 1);
                        errors += 1;
                        continue;
                    }
                };
                let parent = receipt.get("parent_hash").and_then(|v| v.as_str());
                if i == 0 {
                    // Genesis receipt may or may not have parent_hash
                } else if let Some(ph) = parent {
                    if let Some(ref prev) = prev_hash {
                        if ph != prev {
                            eprintln!(
                                "  line {}: parent_hash mismatch (expected {prev}, got {ph})",
                                i + 1
                            );
                            errors += 1;
                        }
                    }
                } else {
                    eprintln!(
                        "  line {}: missing parent_hash in non-genesis receipt",
                        i + 1
                    );
                    errors += 1;
                }
                prev_hash = receipt
                    .get("jws_compact")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        receipt
                            .get("receipt_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    });
            }
            if errors == 0 {
                println!("verified {check_count} receipts: chain integrity OK");
            } else {
                println!("verified {check_count} receipts: {errors} error(s) found");
                return Err(CliError::State("chain verification failed".to_string()));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-142: Capability Wallet
// ---------------------------------------------------------------------------

/// Run `ardur caps` subcommands.
fn run_caps(args: CapsArgs) -> Result<(), CliError> {
    match args.action {
        CapsAction::List => {
            let caps = json!([
                {"variant": "FsRead", "label": "cap.fs_read", "description": "Read from the filesystem"},
                {"variant": "FsWrite", "label": "cap.fs_write", "description": "Write to the filesystem"},
                {"variant": "ShellExec", "label": "cap.shell_exec", "description": "Execute a shell command"},
                {"variant": "NetworkOut", "label": "cap.network_out", "description": "Open an outbound network connection"},
                {"variant": "ProcessSpawn", "label": "cap.process_spawn", "description": "Spawn a child process"},
                {"variant": "EnvRead", "label": "cap.env_read", "description": "Read process environment variables"},
                {"variant": "ClipboardRead", "label": "cap.clipboard_read", "description": "Read the system clipboard"},
                {"variant": "VoiceInput", "label": "cap.voice_input", "description": "Voice input (speech-to-text)"},
                {"variant": "VoiceOutput", "label": "cap.voice_output", "description": "Voice output (text-to-speech)"},
                {"variant": "ImageGenerate", "label": "cap.image_generate", "description": "Image generation"},
                {"variant": "ImageAnalyze", "label": "cap.image_analyze", "description": "Image analysis / description"},
            ]);
            println!(
                "{}",
                serde_json::to_string_pretty(&caps).expect("caps serialise")
            );
        }
        CapsAction::Grants { session } => {
            let root = StateDirs::resolve()?.root;
            let receipts = read_receipt_chain(&root)?;
            let filtered: Vec<_> = receipts
                .iter()
                .rev()
                .filter(|r| {
                    if let Some(ref sess) = session {
                        r.get("subject")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.contains(sess))
                    } else {
                        true
                    }
                })
                .take(1)
                .collect();
            if filtered.is_empty() {
                println!("no session receipts found to inspect cap-token grants");
                return Ok(());
            }
            let receipt = &filtered[0];
            let cap_token_id = receipt
                .get("cap_token_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_calls = receipt.get("tool_calls").cloned().unwrap_or(json!([]));
            let tools: Vec<String> = tool_calls
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|tc| {
                            tc.get("tool_id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let summary = json!({
                "cap_token_id": cap_token_id,
                "tools_invoked": tools,
                "subject": receipt.get("subject").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "receipt_id": receipt.get("receipt_id").and_then(|v| v.as_str()).unwrap_or("unknown"),
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("grants serialise")
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-142: Policy Debugger
// ---------------------------------------------------------------------------

/// Run `ardur policy` subcommands.
fn run_policy(args: PolicyArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;

    match args.action {
        PolicyAction::Check { tool, caps } => {
            let requested_caps: Vec<&str> = caps.split(',').map(|s| s.trim()).collect();
            let cedar_path = root.join("cedar.policies");
            let policy_text = if cedar_path.is_file() {
                std::fs::read_to_string(&cedar_path)?
            } else {
                "// No cedar.policies file found. All capabilities default to allow.".to_string()
            };
            let result = json!({
                "tool": tool,
                "requested_capabilities": requested_caps,
                "cedar_policy_present": cedar_path.is_file(),
                "policy_lines": policy_text.lines().count(),
                "decision": "would check Cedar policy: not yet wired to live evaluator",
                "note": "This is a structural dry-run. Full Cedar evaluation requires a live runtime context.",
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&result).expect("policy check serialises")
            );
        }
        PolicyAction::Lint => {
            let cedar_path = root.join("cedar.policies");
            if !cedar_path.is_file() {
                println!("no cedar.policies file found at {}", cedar_path.display());
                return Ok(());
            }
            let policy_text = std::fs::read_to_string(&cedar_path)?;
            let mut warnings = 0;
            let lines: Vec<&str> = policy_text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("//") {
                    continue;
                }
                // Basic structural checks
                if trimmed.contains("permit(") && !trimmed.contains("principal") {
                    eprintln!("  line {}: permit clause without principal", i + 1);
                    warnings += 1;
                }
                if trimmed.contains("forbid(") && !trimmed.contains("resource") {
                    eprintln!("  line {}: forbid clause without resource", i + 1);
                    warnings += 1;
                }
            }
            if warnings == 0 {
                println!(
                    "cedar policy lint: {} lines checked, no issues found",
                    lines.len()
                );
            } else {
                println!(
                    "cedar policy lint: {} lines checked, {warnings} warning(s)",
                    lines.len()
                );
            }
        }
        PolicyAction::Explain { limit } => {
            let receipts = read_receipt_chain(&root)?;
            let decisions: Vec<_> = receipts
                .iter()
                .rev()
                .filter(|r| {
                    let verb = r.get("verb").and_then(|v| v.as_str()).unwrap_or("");
                    verb.contains("deny") || verb.contains("allow")
                })
                .take(limit)
                .collect();
            if decisions.is_empty() {
                println!("no policy decisions found in receipt chain");
                return Ok(());
            }
            let summary: Vec<_> = decisions
                .iter()
                .map(|d| {
                    json!({
                        "receipt_id": d.get("receipt_id").and_then(|v| v.as_str()),
                        "verb": d.get("verb").and_then(|v| v.as_str()),
                        "subject": d.get("subject").and_then(|v| v.as_str()),
                        "cap_token_id": d.get("cap_token_id").and_then(|v| v.as_str()),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!(summary)).expect("decisions serialise")
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-139: Approval Cards
// ---------------------------------------------------------------------------

/// Run `ardur approvals` subcommands.
fn run_approvals(args: ApprovalsArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let approvals_dir = root.join("approvals");
    std::fs::create_dir_all(&approvals_dir)?;

    match args.action {
        ApprovalsAction::List => {
            let mut pending = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&approvals_dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "json") {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                                let status = v
                                    .get("status")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("pending");
                                if status == "pending" {
                                    pending.push(v);
                                }
                            }
                        }
                    }
                }
            }
            if pending.is_empty() {
                println!("no pending approvals");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(pending)).expect("approvals serialise")
                );
            }
        }
        ApprovalsAction::Approve { id } => {
            let path = approvals_dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("approval `{id}` not found")));
            }
            let mut approval: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| CliError::State(e.to_string()))?;
            approval["status"] = json!("approved");
            approval["decided_at"] = json!(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let json_str = serde_json::to_string_pretty(&approval)
                .map_err(|e| CliError::State(e.to_string()))?;
            std::fs::write(&path, json_str)?;
            println!("approved {id}");
        }
        ApprovalsAction::Deny { id, reason } => {
            let path = approvals_dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("approval `{id}` not found")));
            }
            let mut approval: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| CliError::State(e.to_string()))?;
            approval["status"] = json!("denied");
            approval["deny_reason"] = json!(reason.unwrap_or_default());
            approval["decided_at"] = json!(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let json_str = serde_json::to_string_pretty(&approval)
                .map_err(|e| CliError::State(e.to_string()))?;
            std::fs::write(&path, json_str)?;
            println!("denied {id}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-143: Schedule / Cron CLI
// ---------------------------------------------------------------------------

/// Arguments to `ardur schedule`.
#[derive(Args)]
struct ScheduleArgs {
    #[command(subcommand)]
    action: ScheduleAction,
}

/// Subcommands for `ardur schedule`.
#[derive(Subcommand)]
enum ScheduleAction {
    /// Create a new schedule from a natural-language or cron pattern.
    Create {
        /// Human-readable label.
        label: String,
        /// Schedule pattern: "every 5 minutes", "daily at 9am", or a cron expression.
        pattern: String,
        /// Prompt to run when the schedule fires.
        #[arg(short, long)]
        prompt: String,
    },
    /// List schedules.
    List,
    /// Show the next few fire times for a schedule.
    Next {
        /// Schedule ID.
        id: String,
        /// Number of fire times to compute.
        #[arg(long, default_value_t = 5)]
        count: usize,
    },
    /// Delete a schedule.
    Delete {
        /// Schedule ID.
        id: String,
    },
    /// Test fire a schedule now (dry-run).
    Fire {
        /// Schedule ID.
        id: String,
    },
}

/// Parsed schedule record stored in the state directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct ScheduleRecord {
    schedule_id: String,
    label: String,
    pattern: String,
    prompt: String,
    created_at: u64,
    enabled: bool,
}

/// Simple cron-like parser for the most common NL patterns.
fn parse_pattern(pattern: &str) -> Option<String> {
    let p = pattern.to_lowercase();
    if p == "every minute" {
        return Some("* * * * *".to_string());
    }
    if p == "every 5 minutes" {
        return Some("0,5,10,15,20,25,30,35,40,45,50,55 * * * *".to_string());
    }
    if p == "every 15 minutes" {
        return Some("0,15,30,45 * * * *".to_string());
    }
    if p == "every hour" {
        return Some("0 * * * *".to_string());
    }
    if p == "daily" || p == "every day" {
        return Some("0 0 * * *".to_string());
    }
    if p.starts_with("daily at ") {
        let time_part = p.strip_prefix("daily at ")?;
        return parse_time_to_cron(time_part);
    }
    if p == "weekly" {
        return Some("0 0 * * 0".to_string());
    }
    // Treat as raw cron if it looks like 5 fields.
    let fields: Vec<&str> = pattern.split_whitespace().collect();
    if fields.len() == 5 {
        return Some(pattern.to_string());
    }
    None
}

fn parse_time_to_cron(time_str: &str) -> Option<String> {
    // Accept "9am", "9:30am", "14:00", "2:30pm".
    let s = time_str.trim().to_lowercase();
    let re = regex::Regex::new(r"^(\d{1,2})(?::(\d{2}))?(am|pm)?$").ok()?;
    let caps = re.captures(&s)?;
    let mut hour: u32 = caps[1].parse().ok()?;
    let minute: u32 = caps
        .get(2)
        .map(|m| m.as_str().parse().ok())
        .unwrap_or(Some(0))?;
    let ampm = caps.get(3).map(|m| m.as_str());
    if let Some(ap) = ampm {
        if ap == "pm" && hour != 12 {
            hour += 12;
        }
        if ap == "am" && hour == 12 {
            hour = 0;
        }
    }
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(format!("{minute} {hour} * * *"))
}

/// Read schedule records from the state directory.
fn read_schedules(root: &Path) -> Result<Vec<ScheduleRecord>, CliError> {
    let dir = root.join("schedules");
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<ScheduleRecord>(&content) {
                        records.push(v);
                    }
                }
            }
        }
    }
    Ok(records)
}

/// Parse a 5-field cron string into a CronExpression.
/// Supported: * (any), n (exact), n-m (range), a,b (list), */n (step).
fn cron_to_expression(cron: &str) -> Result<ardur_cron::CronExpression, CliError> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CliError::State(format!(
            "cron must have 5 fields, got {}",
            fields.len()
        )));
    }
    Ok(ardur_cron::CronExpression::new(
        fields[0], fields[1], fields[2], fields[3], fields[4],
    ))
}

/// Compute next N fire times from a cron expression (UTC).
fn next_fire_times(
    cron: &str,
    count: usize,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>, CliError> {
    let expr = cron_to_expression(cron)?;
    let now = chrono::Utc::now();
    let mut fires = Vec::with_capacity(count);
    let mut probe = now;
    // Safety guard: stop searching after 1 year of minutes.
    let cutoff = now + chrono::Duration::days(366);
    while fires.len() < count && probe < cutoff {
        probe += chrono::Duration::minutes(1);
        if expr.is_due(probe) {
            fires.push(probe);
        }
    }
    Ok(fires)
}

/// Run `ardur schedule` subcommands.
fn run_schedule(args: ScheduleArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let schedules_dir = root.join("schedules");
    std::fs::create_dir_all(&schedules_dir)?;

    match args.action {
        ScheduleAction::Create {
            label,
            pattern,
            prompt,
        } => {
            let cron = parse_pattern(&pattern).ok_or_else(|| {
                CliError::State(format!("unrecognized schedule pattern: {pattern}"))
            })?;
            // Validate cron.
            cron_to_expression(&cron).map_err(|e| CliError::State(format!("invalid cron: {e}")))?;
            let id = uuid::Uuid::new_v4().to_string();
            let record = ScheduleRecord {
                schedule_id: id.clone(),
                label,
                pattern: cron,
                prompt,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                enabled: true,
            };
            std::fs::write(
                schedules_dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("created schedule {id}");
            let next = next_fire_times(&record.pattern, 1)?;
            if let Some(t) = next.first() {
                println!("next fire: {t}");
            }
        }
        ScheduleAction::List => {
            let records = read_schedules(&root)?;
            if records.is_empty() {
                println!("no schedules");
            } else {
                let summary: Vec<serde_json::Value> = records
                    .iter()
                    .map(|r| {
                        json!({
                            "schedule_id": r.schedule_id,
                            "label": r.label,
                            "pattern": r.pattern,
                            "prompt": r.prompt,
                            "enabled": r.enabled,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(summary)).expect("schedules serialise")
                );
            }
        }
        ScheduleAction::Next { id, count } => {
            let records = read_schedules(&root)?;
            let found = records.iter().find(|r| r.schedule_id == id);
            match found {
                Some(r) => {
                    let fires = next_fire_times(&r.pattern, count)?;
                    println!("next fire times for {id}:");
                    for t in fires {
                        println!("  {t}");
                    }
                }
                None => {
                    return Err(CliError::State(format!("schedule `{id}` not found")));
                }
            }
        }
        ScheduleAction::Delete { id } => {
            let path = schedules_dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("schedule `{id}` not found")));
            }
            std::fs::remove_file(&path)?;
            println!("deleted schedule {id}");
        }
        ScheduleAction::Fire { id } => {
            let records = read_schedules(&root)?;
            let found = records.iter().find(|r| r.schedule_id == id);
            match found {
                Some(r) => {
                    println!("dry-run fire schedule {id}");
                    println!("  prompt: {}", r.prompt);
                    println!("  pattern: {}", r.pattern);
                    println!("  note: execution engine not yet wired");
                }
                None => {
                    return Err(CliError::State(format!("schedule `{id}` not found")));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-138: Token / Pairing Secret Management
// ---------------------------------------------------------------------------

/// Arguments to `ardur token`.
#[derive(Args)]
struct TokenArgs {
    #[command(subcommand)]
    action: TokenAction,
}

/// Subcommands for `ardur token`.
#[derive(Subcommand)]
enum TokenAction {
    /// Create a new token with a label and optional scope.
    Create {
        /// Human-readable label for the token.
        label: String,
        /// Optional scope: read, write, admin (default: read).
        #[arg(long, default_value = "read")]
        scope: String,
    },
    /// List stored tokens (hashes only; values never displayed).
    List,
    /// Revoke a token by ID.
    Revoke {
        /// Token ID to revoke.
        id: String,
    },
}

/// Generate a URL-safe random token.
fn generate_token_value() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// Run `ardur token` subcommands.
fn run_token(args: TokenArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let tokens_dir = root.join("tokens");
    std::fs::create_dir_all(&tokens_dir)?;

    match args.action {
        TokenAction::Create { label, scope } => {
            let token_id = uuid::Uuid::new_v4().to_string();
            let token_value = generate_token_value();
            // Hash the value with a simple SHA-256 so we never store plaintext.
            let hash = sha2::Sha256::digest(token_value.as_bytes());
            let hash_hex = hex::encode(hash);
            let record = json!({
                "token_id": token_id,
                "label": label,
                "scope": scope,
                "hash": hash_hex,
                "created_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "revoked": false,
            });
            std::fs::write(
                tokens_dir.join(format!("{token_id}.json")),
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("created token {token_id}");
            println!("value: {token_value}");
            println!("warning: this is the only time the value is shown");
        }
        TokenAction::List => {
            let mut tokens = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&tokens_dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "json") {
                        if let Ok(content) = std::fs::read_to_string(entry.path()) {
                            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) {
                                // Never display the actual hash in list view.
                                v.as_object_mut()
                                    .map(|m| m.insert("hash".to_string(), json!("<redacted>")));
                                tokens.push(v);
                            }
                        }
                    }
                }
            }
            if tokens.is_empty() {
                println!("no tokens stored");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(tokens)).expect("tokens serialise")
                );
            }
        }
        TokenAction::Revoke { id } => {
            let path = tokens_dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("token `{id}` not found")));
            }
            let mut token: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| CliError::State(e.to_string()))?;
            token["revoked"] = json!(true);
            token["revoked_at"] = json!(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&token).map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("revoked token {id}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-140: Secret Redaction
// ---------------------------------------------------------------------------

/// Arguments to `ardur redact`.
#[derive(Args)]
struct RedactArgs {
    /// Input file; if omitted, reads from stdin.
    #[arg(short, long, value_name = "PATH")]
    input: Option<PathBuf>,
    /// Output file; if omitted, writes to stdout.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Additional secret patterns as regex strings (can be repeated).
    #[arg(short, long = "pattern")]
    patterns: Vec<String>,
    /// If set, treat input as JSON and redact string values recursively.
    #[arg(long)]
    json: bool,
}

/// Default secret patterns: API keys, tokens, passwords, private keys, etc.
fn default_secret_patterns() -> Vec<regex::Regex> {
    let patterns = [
        // OpenAI / Anthropic API keys
        r"(?i)sk-[a-z0-9]{48}",
        r"(?i)sk-[a-z0-9]{20,47}",
        // Generic secret-looking tokens
        r"(?i)bearer\s+[a-z0-9_\-\.]{20,}",
        r"(?i)token[a-z0-9_\-]*[:=]\s*[a-z0-9_\-\.]{8,}",
        r"(?i)api[_\-]?key[a-z0-9_\-]*[:=]\s*[a-z0-9_\-\.]{8,}",
        // Natural-language password/secret leakage
        r"(?i)pass(?:word)?\s*(?:is|=|:)\s*\S+",
        r"(?i)secret(?:\s+is|=|:)\s*\S+",
        // AWS-style access keys
        r"AKIA[0-9A-Z]{16}",
        // Private keys / certs
        r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END",
        // GitHub tokens
        r"gh[pousr]_[A-Za-z0-9_]{36,}",
    ];
    patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
}

/// Redact secrets in a plain string.
fn redact_text(text: &str, patterns: &[regex::Regex]) -> String {
    let mut out = text.to_string();
    for re in patterns {
        out = re.replace_all(&out, "<REDACTED>").to_string();
    }
    out
}

/// Recursively redact string values in a JSON object.
fn redact_json_value(value: &mut serde_json::Value, patterns: &[regex::Regex]) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let k_lower = k.to_lowercase();
                if k_lower.contains("token")
                    || k_lower.contains("secret")
                    || k_lower.contains("password")
                    || k_lower.contains("api_key")
                    || k_lower.contains("key")
                {
                    if let serde_json::Value::String(s) = v {
                        *s = "<REDACTED>".to_string();
                    } else {
                        redact_json_value(v, patterns);
                    }
                } else {
                    redact_json_value(v, patterns);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_json_value(v, patterns);
            }
        }
        serde_json::Value::String(s) => {
            *s = redact_text(s, patterns);
        }
        _ => {}
    }
}

/// Run `ardur redact`.
fn run_redact(args: RedactArgs) -> Result<(), CliError> {
    let mut patterns = default_secret_patterns();
    for p in args.patterns {
        if let Ok(re) = regex::Regex::new(&p) {
            patterns.push(re);
        }
    }

    let input = match args.input {
        Some(path) => std::fs::read_to_string(&path)?,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };

    let output = if args.json {
        let mut value: serde_json::Value = serde_json::from_str(&input)
            .map_err(|e| CliError::State(format!("invalid json: {e}")))?;
        redact_json_value(&mut value, &patterns);
        serde_json::to_string_pretty(&value).map_err(|e| CliError::State(e.to_string()))?
    } else {
        redact_text(&input, &patterns)
    };

    match args.output {
        Some(path) => std::fs::write(&path, output)?,
        None => println!("{output}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-141: Memory Explorer
// ---------------------------------------------------------------------------

/// Arguments to `ardur memory`.
#[derive(Args)]
struct MemoryArgs {
    #[command(subcommand)]
    action: MemoryAction,
}

/// Subcommands for `ardur memory`.
#[derive(Subcommand)]
enum MemoryAction {
    /// List memory cards from the state directory.
    List {
        /// Filter by workspace/subject prefix.
        #[arg(long)]
        workspace: Option<String>,
        /// Maximum number of cards to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Search memory cards by keyword.
    Search {
        /// Search query.
        query: String,
        /// Maximum results.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show a single memory card by record ID.
    Show {
        /// The record ID (UUID).
        id: String,
    },
    /// Export memory cards to JSON.
    Export {
        /// Output file (defaults to stdout).
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// Filter by workspace.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Tombstone (forget) a memory card by record ID.
    Forget {
        /// The record ID (UUID) to tombstone.
        id: String,
        /// Reason for forgetting.
        #[arg(long, default_value = "user_requested")]
        reason: String,
    },
}

/// Read memory cards from the state directory.
fn read_memory_cards(root: &Path) -> Result<Vec<serde_json::Value>, CliError> {
    let memory_dir = root.join("memory");
    let mut cards = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&memory_dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        cards.push(v);
                    }
                }
            }
        }
    }
    Ok(cards)
}

/// Run `ardur memory` subcommands.
fn run_memory(args: MemoryArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;

    match args.action {
        MemoryAction::List { workspace, limit } => {
            let cards = read_memory_cards(&root)?;
            let filtered: Vec<_> = cards
                .iter()
                .filter(|c| {
                    if let Some(ref ws) = workspace {
                        c.get("subject")
                            .or_else(|| c.get("workspace"))
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.contains(ws))
                    } else {
                        true
                    }
                })
                .take(limit)
                .collect();
            if filtered.is_empty() {
                println!("no memory cards found");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(filtered)).expect("memory cards serialise")
                );
            }
        }
        MemoryAction::Search { query, limit } => {
            let cards = read_memory_cards(&root)?;
            let query_lower = query.to_lowercase();
            let results: Vec<_> = cards
                .iter()
                .filter(|c| {
                    let content = c
                        .get("content")
                        .or_else(|| c.get("body"))
                        .or_else(|| c.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    content.to_lowercase().contains(&query_lower)
                })
                .take(limit)
                .collect();
            if results.is_empty() {
                println!("no memory cards matching '{query}'");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(results))
                        .expect("search results serialise")
                );
            }
        }
        MemoryAction::Show { id } => {
            let cards = read_memory_cards(&root)?;
            let found = cards.iter().find(|c| {
                c.get("record_id")
                    .or_else(|| c.get("id"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|rid| rid == id)
            });
            match found {
                Some(c) => println!(
                    "{}",
                    serde_json::to_string_pretty(c).expect("card serialises")
                ),
                None => {
                    return Err(CliError::State(format!("memory card `{id}` not found")));
                }
            }
        }
        MemoryAction::Export { output, workspace } => {
            let cards = read_memory_cards(&root)?;
            let filtered: Vec<_> = cards
                .iter()
                .filter(|c| {
                    if let Some(ref ws) = workspace {
                        c.get("subject")
                            .or_else(|| c.get("workspace"))
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.contains(ws))
                    } else {
                        true
                    }
                })
                .collect();
            let export = json!({
                "exported_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "card_count": filtered.len(),
                "cards": filtered,
            });
            let json_str = serde_json::to_string_pretty(&export).expect("export serialises");
            if let Some(path) = output {
                std::fs::write(&path, json_str)?;
                println!("exported {} cards to {}", filtered.len(), path.display());
            } else {
                println!("{json_str}");
            }
        }
        MemoryAction::Forget { id, reason } => {
            let memory_dir = root.join("memory");
            let tombstone_path = memory_dir.join(format!("{id}.tombstone.json"));
            let tombstone = json!({
                "record_id": id,
                "tombstoned_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "reason": reason,
            });
            let json_str = serde_json::to_string_pretty(&tombstone).expect("tombstone serialises");
            std::fs::write(&tombstone_path, json_str)?;
            println!("tombstoned memory card {id} (reason: {reason})");
            println!("note: the card is not deleted — it is marked invalid for future recall");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-144 / ARD-145: Connector CLI stubs (fetch + search)
// ---------------------------------------------------------------------------

/// Arguments to `ardur fetch`.
#[derive(Args)]
struct FetchArgs {
    /// URL to fetch.
    url: String,
    /// Optional output file (defaults to stdout).
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Maximum response size in bytes.
    #[arg(long, default_value_t = 1_048_576)]
    max_bytes: usize,
    /// Additional allowed host (can be repeated).
    #[arg(long = "allow-host")]
    allow_hosts: Vec<String>,
}

/// Arguments to `ardur search`.
#[derive(Args)]
struct SearchArgs {
    /// Search query.
    query: String,
    /// Provider to use.
    #[arg(long, default_value = "web")]
    provider: String,
    /// Maximum number of results.
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

/// Run `ardur fetch`.
fn run_fetch(args: FetchArgs) -> Result<(), CliError> {
    let url = args.url;
    let root = StateDirs::resolve()?.root;

    // Read allowlist from config if present.
    let mut allowlist: Vec<String> = Vec::new();
    let allowlist_path = root.join("http_allowlist.txt");
    if allowlist_path.is_file() {
        let content = std::fs::read_to_string(&allowlist_path)?;
        allowlist.extend(
            content
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    allowlist.extend(args.allow_hosts);

    // Safety check: refuse non-HTTP(S) schemes.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(CliError::State(format!(
            "only http:// and https:// URLs are supported, got `{url}`"
        )));
    }

    if allowlist.is_empty() {
        return Err(CliError::State(
            "no HTTP allowlist configured. Add hosts to ~/.ardur/http_allowlist.txt or use --allow-host".to_string(),
        ));
    }

    let host = url
        .split('/')
        .nth(2)
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if !allowlist.iter().any(|h| h.to_lowercase() == host) {
        return Err(CliError::State(format!(
            "host `{host}` is not in the allowlist; add it to {} or use --allow-host",
            allowlist_path.display()
        )));
    }

    let rt = tokio::runtime::Runtime::new()?;
    let body = rt.block_on(async {
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| CliError::State(format!("request failed: {e}")))?;
        let text = response
            .text()
            .await
            .map_err(|e| CliError::State(format!("read failed: {e}")))?;
        Ok::<String, CliError>(text.chars().take(args.max_bytes).collect())
    })?;

    match args.output {
        Some(path) => {
            std::fs::write(&path, &body)?;
            println!("wrote {} bytes to {}", body.len(), path.display());
        }
        None => println!("{body}"),
    }
    Ok(())
}

/// Run `ardur search` (stub; provider wiring in Phase 2).
fn run_search(args: SearchArgs) -> Result<(), CliError> {
    println!(
        "search: {} (provider: {}, limit: {})",
        args.query, args.provider, args.limit
    );
    println!("note: web search provider integration is a Phase 2 wiring task.");
    println!(
        "      configure ARDUR_SEARCH_PROVIDER and ARDUR_SEARCH_API_KEY to enable live results."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-146: Channel Manager CLI
// ---------------------------------------------------------------------------

/// Arguments to `ardur channel`.
#[derive(Args)]
struct ChannelArgs {
    #[command(subcommand)]
    action: ChannelAction,
}

/// Subcommands for `ardur channel`.
#[derive(Subcommand)]
enum ChannelAction {
    /// List configured channels.
    List,
    /// Add or update a channel adapter configuration.
    Add {
        /// Channel adapter type: discord, telegram, matrix, slack.
        #[arg(value_parser = ["discord", "telegram", "matrix", "slack"])]
        channel_type: String,
        /// Human-readable name.
        name: String,
    },
    /// Show details for a channel.
    Show {
        /// Channel name.
        name: String,
    },
    /// Remove a channel configuration.
    Remove {
        /// Channel name.
        name: String,
    },
    /// Enable or disable a channel.
    Set {
        /// Channel name.
        name: String,
        /// Desired status: enabled or disabled.
        #[arg(value_parser = ["enabled", "disabled"])]
        status: String,
    },
}

/// Channel configuration record stored in the state directory.
#[derive(serde::Serialize, serde::Deserialize)]
struct ChannelRecord {
    name: String,
    channel_type: String,
    enabled: bool,
    created_at: u64,
    #[serde(default)]
    env_prefix: String,
    #[serde(default)]
    notes: String,
}

fn channels_dir(root: &Path) -> PathBuf {
    root.join("channels")
}

fn read_channels(root: &Path) -> Result<Vec<ChannelRecord>, CliError> {
    let dir = channels_dir(root);
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<ChannelRecord>(&content) {
                        records.push(v);
                    }
                }
            }
        }
    }
    Ok(records)
}

fn default_env_prefix(channel_type: &str, name: &str) -> String {
    let sanitized: String = name
        .to_uppercase()
        .replace(|c: char| !c.is_alphanumeric(), "_");
    format!("{}_{}", channel_type.to_uppercase(), sanitized)
}

/// Run `ardur channel` subcommands.
fn run_channel(args: ChannelArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let dir = channels_dir(&root);
    std::fs::create_dir_all(&dir)?;

    match args.action {
        ChannelAction::List => {
            let records = read_channels(&root)?;
            if records.is_empty() {
                println!("no channels configured");
            } else {
                println!(
                    "{NAME: <12} {TYPE: <10} {STATUS: <10} ENV_PREFIX",
                    NAME = "NAME",
                    TYPE = "TYPE",
                    STATUS = "STATUS"
                );
                for r in &records {
                    let status = if r.enabled { "enabled" } else { "disabled" };
                    println!(
                        "{: <12} {: <10} {: <10} {}",
                        r.name, r.channel_type, status, r.env_prefix
                    );
                }
            }
        }
        ChannelAction::Add { channel_type, name } => {
            let path = dir.join(format!("{name}.json"));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let record = ChannelRecord {
                name: name.clone(),
                channel_type: channel_type.clone(),
                enabled: true,
                created_at: now,
                env_prefix: default_env_prefix(&channel_type, &name),
                notes: format!(
                    "Set {prefix}_TOKEN (and any required IDs) to activate this channel.",
                    prefix = default_env_prefix(&channel_type, &name)
                ),
            };
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("added channel {name} ({channel_type})");
            println!("  env prefix: {}", record.env_prefix);
        }
        ChannelAction::Show { name } => {
            let records = read_channels(&root)?;
            let found = records.iter().find(|r| r.name == name);
            match found {
                Some(r) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(r)
                            .map_err(|e| CliError::State(e.to_string()))?
                    );
                }
                None => {
                    return Err(CliError::State(format!("channel `{name}` not found")));
                }
            }
        }
        ChannelAction::Remove { name } => {
            let path = dir.join(format!("{name}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("channel `{name}` not found")));
            }
            std::fs::remove_file(&path)?;
            println!("removed channel {name}");
        }
        ChannelAction::Set { name, status } => {
            let path = dir.join(format!("{name}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("channel `{name}` not found")));
            }
            let content = std::fs::read_to_string(&path)?;
            let mut record: ChannelRecord =
                serde_json::from_str(&content).map_err(|e| CliError::State(e.to_string()))?;
            record.enabled = status == "enabled";
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("channel {name} is now {status}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ARD-148: Migration Import / Export CLI
// ---------------------------------------------------------------------------

/// Arguments to `ardur migrate`.
#[derive(Args)]
struct MigrateArgs {
    #[command(subcommand)]
    action: MigrateAction,
}

/// Subcommands for `ardur migrate`.
#[derive(Subcommand)]
enum MigrateAction {
    /// Export ardur state to a directory.
    Export {
        /// Output directory path.
        path: PathBuf,
    },
    /// Import ardur state from a directory.
    Import {
        /// Source directory path.
        path: PathBuf,
    },
    /// Convert a Hermes directory into ardur state.
    FromHermes {
        /// Path to Hermes data directory.
        hermes_dir: PathBuf,
        /// Output ardur state directory.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Convert an OpenClaw directory into ardur state.
    FromOpenClaw {
        /// Path to OpenClaw data directory.
        openclaw_dir: PathBuf,
        /// Output ardur state directory.
        #[arg(short, long)]
        output: PathBuf,
    },
}

/// Copy all .json files from source dirs to the migration directory.
fn export_state(root: &Path, out: &Path) -> Result<usize, CliError> {
    let subdirs = [
        "sessions",
        "memory",
        "schedules",
        "channels",
        "approvals",
        "tokens",
    ];
    let mut count = 0;
    for sub in &subdirs {
        let src = root.join(sub);
        let dst = out.join(sub);
        if !src.is_dir() {
            continue;
        }
        std::fs::create_dir_all(&dst)?;
        for entry in std::fs::read_dir(&src)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let file_name = path
                    .file_name()
                    .ok_or_else(|| CliError::State("invalid file".into()))?;
                std::fs::copy(&path, dst.join(file_name))?;
                count += 1;
            }
        }
    }
    Ok(count)
}

fn import_state(src: &Path, root: &Path) -> Result<usize, CliError> {
    export_state(src, root)
}

fn migrate_from_hermes(hermes_dir: &Path, output: &Path) -> Result<usize, CliError> {
    std::fs::create_dir_all(output)?;
    // Hermes stores sessions as JSON files under .hermes/history/.
    let hermes_sessions = hermes_dir.join("history");
    let ardur_sessions = output.join("sessions");
    std::fs::create_dir_all(&ardur_sessions)?;
    let mut count = 0;
    if hermes_sessions.is_dir() {
        for entry in std::fs::read_dir(&hermes_sessions)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let out_path = ardur_sessions.join(path.file_name().unwrap_or_default());
                std::fs::copy(&path, out_path)?;
                count += 1;
            }
        }
    }
    println!("imported {count} Hermes session files");
    println!("note: Hermes-specific fields may need manual mapping");
    Ok(count)
}

fn migrate_from_openclaw(openclaw_dir: &Path, output: &Path) -> Result<usize, CliError> {
    std::fs::create_dir_all(output)?;
    // OpenClaw stores sessions under sessions/ and memory under memory/.
    let src_sessions = openclaw_dir.join("sessions");
    let dst_sessions = output.join("sessions");
    std::fs::create_dir_all(&dst_sessions)?;
    let mut count = 0;
    if src_sessions.is_dir() {
        for entry in std::fs::read_dir(&src_sessions)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let out_path = dst_sessions.join(path.file_name().unwrap_or_default());
                std::fs::copy(&path, out_path)?;
                count += 1;
            }
        }
    }
    let src_memory = openclaw_dir.join("memory");
    let dst_memory = output.join("memory");
    if src_memory.is_dir() {
        std::fs::create_dir_all(&dst_memory)?;
        for entry in std::fs::read_dir(&src_memory)?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let out_path = dst_memory.join(path.file_name().unwrap_or_default());
                std::fs::copy(&path, out_path)?;
                count += 1;
            }
        }
    }
    println!("imported {count} OpenClaw files");
    println!("note: OpenClaw-specific fields may need manual mapping");
    Ok(count)
}

/// Run `ardur migrate` subcommands.
fn run_migrate(args: MigrateArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;

    match args.action {
        MigrateAction::Export { path } => {
            std::fs::create_dir_all(&path)?;
            let count = export_state(&root, &path)?;
            let meta = json!({
                "exported_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "version": "0.0.1",
                "files": count,
            });
            std::fs::write(
                path.join("manifest.json"),
                serde_json::to_string_pretty(&meta).map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("exported {count} files to {}", path.display());
        }
        MigrateAction::Import { path } => {
            let count = import_state(&path, &root)?;
            println!("imported {count} files into {}", root.display());
        }
        MigrateAction::FromHermes { hermes_dir, output } => {
            migrate_from_hermes(&hermes_dir, &output)?;
            println!("wrote ardur state to {}", output.display());
        }
        MigrateAction::FromOpenClaw {
            openclaw_dir,
            output,
        } => {
            migrate_from_openclaw(&openclaw_dir, &output)?;
            println!("wrote ardur state to {}", output.display());
        }
    }
    Ok(())
}
