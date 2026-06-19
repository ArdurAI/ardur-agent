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
    /// Interactive setup wizard for first-time configuration.
    Setup(SetupArgs),
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
        Err(CliError::State("doctor found critical issues".to_string()))
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
