//! ardur-cli — the `ardur` binary: a minimal interactive chat REPL over the
//! Phase-1 substrate.
//!
//! Plan family: §2.1 (`plans/2.1-cli-blueprint.md`). §18.8 keeps this PR scoped
//! to `crates/cli/**` plus the root manifest.
//!
//! # Surface
//!
//! - [`run_chat`] — load [`Config`], wire the chat substrate, and drive a REPL:
//!   a `/`-prefixed line dispatches through the §1.0 [`CommandBus`]; anything
//!   else is submitted as a chat turn.
//! - [`register_default_commands`] / [`BudgetCommand`] — the built-in
//!   `/help`, `/quit`, `/exit`, and `/budget` slash-commands.
//! - [`FusedEngine`] — the **default** substrate: one [`FusedRuntime`] per turn
//!   runs the full ten-stage pipeline (cap-token verify, Cedar authorization,
//!   cost admission, real provider dispatch, signed-and-chained receipt, cost
//!   finalize, memory write, durable journal), over persistent state under
//!   `~/.ardur/` ([`StateDirs`]).
//! - [`ChatEngine`] / [`TurnOutcome`] — the legacy `InMemoryRuntime` **echo**
//!   substrate, retained behind `--echo` for cheap, key-free, cost-free smoke
//!   testing.
//! - [`Config`] — `~/.ardur/config.toml` loading with defaults.
//! - [`CliError`] — the crate's single typed-error surface.
//!
//! By default the interactive `chat` subcommand routes turns through the fused
//! runtime (real LLM + full substrate + persistent state). With no
//! `ANTHROPIC_API_KEY` set it falls back to the network-free Anthropic stub and
//! prints an offline notice; `--echo` selects the legacy echo runtime instead.
//!
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod commands;
mod config;
mod engine;
mod error;
mod fused;
mod state;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use ardur_provider_runtime::{TelemetryConfig, init_genai_tracing, shutdown_genai_tracing};
use ardur_runtime::{ChatMessage, CommandBus, CommandContext, InMemoryCommandBus, RuntimeError};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

pub use commands::{BudgetCommand, register_default_commands};
pub use config::{Config, DEFAULT_BUDGET_CENTS, DEFAULT_MODEL};
pub use engine::{ChatEngine, TurnOutcome};
pub use error::CliError;
pub use fused::FusedEngine;
pub use state::StateDirs;

/// The environment variable overriding the per-session budget, in US cents.
pub const BUDGET_CENTS_ENV: &str = "ARDUR_CLI_BUDGET_CENTS";

/// Arguments to the `ardur chat` subcommand.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct ChatArgs {
    /// Path to the config file (defaults to `~/.ardur/config.toml`).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Per-session budget ceiling, in US cents (default 1000 = $10). Overrides
    /// `ARDUR_CLI_BUDGET_CENTS` and the config file. Real LLM calls cost real
    /// money — this caps a session's spend.
    #[arg(long, value_name = "CENTS")]
    pub budget_cents: Option<u64>,

    /// Use the legacy in-memory echo runtime instead of the full FusedRuntime:
    /// no provider call, no cost, no persistent state. For cheap smoke testing
    /// without an API key.
    #[arg(long)]
    pub echo: bool,
}

/// The active chat substrate for a session: the default [`FusedEngine`] or the
/// legacy [`ChatEngine`] echo runtime (selected by `--echo`). Both expose the
/// budget handle and per-turn entry the REPL drives.
enum ActiveEngine {
    /// The FusedRuntime-backed substrate (default).
    Fused(Box<FusedEngine>),
    /// The legacy `InMemoryRuntime` echo substrate (`--echo`).
    Echo(Box<ChatEngine>),
}

impl ActiveEngine {
    /// A shared handle to the session's remaining-cents counter.
    fn budget_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        match self {
            ActiveEngine::Fused(e) => e.budget_handle(),
            ActiveEngine::Echo(e) => e.budget_handle(),
        }
    }

    /// The session's remaining budget, in cents.
    fn remaining_cents(&self) -> u64 {
        match self {
            ActiveEngine::Fused(e) => e.remaining_cents(),
            ActiveEngine::Echo(e) => e.remaining_cents(),
        }
    }

    /// Run one chat turn over `messages`.
    async fn run_turn(&self, messages: &[ChatMessage]) -> Result<TurnOutcome, CliError> {
        match self {
            ActiveEngine::Fused(e) => e.run_turn(messages).await,
            ActiveEngine::Echo(e) => e.run_turn(messages).await,
        }
    }
}

/// ANSI bold-cyan wrapping for the prompt.
const PROMPT_ON: &str = "\x1b[1;36m";
const PROMPT_OFF: &str = "\x1b[0m";

/// Run the interactive chat REPL: load config, wire the engine, register the
/// slash-commands, and loop reading lines until `/quit`, EOF, or interrupt.
pub fn run_chat(args: ChatArgs) -> Result<(), CliError> {
    // Telemetry: when `ARDUR_OTEL_ENABLED=true`, stand up the OpenTelemetry GenAI
    // pipeline (OTLP exporter + layered subscriber) so provider spans export to an
    // OTLP backend; otherwise install the plain stderr console subscriber. Only
    // one process-wide subscriber may be set, so these are mutually exclusive. A
    // second `try_init` (e.g. a test calling in-process) is a no-op, not a panic.
    let telemetry = TelemetryConfig::from_env();
    if telemetry.enabled {
        init_genai_tracing(telemetry.clone())
            .map_err(|e| CliError::State(format!("initializing OpenTelemetry tracing: {e}")))?;
        tracing::info!(
            otlp_endpoint = %telemetry.otlp_endpoint,
            service_name = %telemetry.service_name,
            "OpenTelemetry GenAI tracing enabled"
        );
    } else {
        // Logs go to stderr so they never interleave with the REPL's stdout.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .try_init();
    }

    let mut config = Config::load(args.config.clone())?;
    config.budget_cents = resolve_budget_cents(args.budget_cents, config.budget_cents);
    if let Some(path) = Config::default_path() {
        tracing::debug!(config = %config::redacted_summary(&config, &path), "loaded config");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_chat_loop(config, args.echo));

    // Flush any buffered OpenTelemetry spans before returning (a no-op when
    // telemetry was never initialized).
    shutdown_genai_tracing();
    result
}

/// Resolve the effective per-session budget: the `--budget-cents` flag wins,
/// then the [`BUDGET_CENTS_ENV`] environment variable, then the config-file (or
/// default) value.
fn resolve_budget_cents(flag: Option<u64>, config_value: u64) -> u64 {
    flag.or_else(|| {
        std::env::var(BUDGET_CENTS_ENV)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    })
    .unwrap_or(config_value)
}

/// The async REPL body, driven by [`run_chat`]'s tokio runtime. Wires the
/// selected substrate, registers the slash-commands, and reads turns from either
/// an interactive line-editor (a tty) or piped stdin.
async fn run_chat_loop(config: Config, echo: bool) -> Result<(), CliError> {
    let engine = if echo {
        ActiveEngine::Echo(Box::new(ChatEngine::new(&config)?))
    } else {
        let dirs = StateDirs::resolve()?;
        dirs.create()?;
        let fused = FusedEngine::new(&config, &dirs, config.budget_cents)?;
        if fused.offline() {
            println!(
                "running in offline mode (stub provider) — set ANTHROPIC_API_KEY for real LLM calls."
            );
        }
        ActiveEngine::Fused(Box::new(fused))
    };

    let mut bus = InMemoryCommandBus::new();
    register_default_commands(&mut bus);
    bus.register_command(
        "budget",
        Box::new(BudgetCommand::new(engine.budget_handle())),
    );

    println!(
        "ardur chat — model {} · type /help for commands.",
        config.model
    );

    let mut history: Vec<ChatMessage> = Vec::new();
    // A tty drives the rich line-editor; piped/redirected stdin reads lines
    // directly so `echo "hi" | ardur chat` (and the integration tests) work.
    if std::io::stdin().is_terminal() {
        run_interactive(&engine, &bus, &mut history).await?;
    } else {
        run_piped(&engine, &bus, &mut history).await;
    }
    Ok(())
}

/// The interactive REPL over a `rustyline` line-editor.
async fn run_interactive(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    history: &mut Vec<ChatMessage>,
) -> Result<(), CliError> {
    let mut editor = DefaultEditor::new().map_err(readline_to_cli)?;
    loop {
        let prompt = format!(
            "{PROMPT_ON}[budget: {}c] > {PROMPT_OFF}",
            engine.remaining_cents()
        );
        match editor.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line);
                if handle_line(engine, bus, history, line).await {
                    break;
                }
            }
            // Ctrl-C / Ctrl-D leave the chat cleanly.
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => return Err(readline_to_cli(e)),
        }
    }
    Ok(())
}

/// The non-interactive loop over piped/redirected stdin: one turn per line, no
/// prompt, until a `/quit` or EOF.
async fn run_piped(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    history: &mut Vec<ChatMessage>,
) {
    use std::io::BufRead as _;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if handle_line(engine, bus, history, line).await {
            break;
        }
    }
}

/// Handle one input line: dispatch a `/`-command through `bus`, or submit the
/// line as a chat turn. Returns `true` when the loop should break (a quit/exit).
async fn handle_line(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    history: &mut Vec<ChatMessage>,
    line: &str,
) -> bool {
    if let Some(rest) = line.strip_prefix('/') {
        dispatch_slash(bus, rest)
    } else {
        run_chat_message(engine, history, line).await;
        false
    }
}

/// Dispatch a `/`-stripped command line through the bus, printing its output.
/// Returns `true` if the command was `/quit` or `/exit` (the caller should then
/// break the loop).
fn dispatch_slash(bus: &InMemoryCommandBus, rest: &str) -> bool {
    let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let ctx = CommandContext {
        command: command.to_string(),
        args: args.trim().to_string(),
    };
    match bus.dispatch(ctx) {
        Ok(result) => println!("{}", result.output),
        Err(RuntimeError::CommandNotFound(name)) => {
            println!("unknown command: /{name} — type /help");
        }
        Err(e) => println!("command error: {e}"),
    }
    matches!(command, commands::QUIT_COMMAND | commands::EXIT_COMMAND)
}

/// Submit `line` as a chat turn, appending it (and any reply) to `history` and
/// printing the response with its cost.
async fn run_chat_message(engine: &ActiveEngine, history: &mut Vec<ChatMessage>, line: &str) {
    history.push(ChatMessage::user(line));
    match engine.run_turn(history).await {
        Ok(outcome) => {
            println!("{}", outcome.response);
            println!(
                "(used: {}c, remaining: {}c)",
                outcome.used_cents, outcome.remaining_cents
            );
            history.push(ChatMessage::assistant(outcome.response));
        }
        Err(e) => {
            // Drop the unanswered user message so a retry starts clean.
            history.pop();
            let _ = std::io::stdout().flush();
            eprintln!("error: {e}");
        }
    }
}

/// Map a non-EOF line-editor failure onto [`CliError::Io`] (EOF/interrupt are
/// handled by the loop and never reach here).
fn readline_to_cli(e: ReadlineError) -> CliError {
    match e {
        ReadlineError::Io(io) => CliError::Io(io),
        other => CliError::Io(std::io::Error::other(other.to_string())),
    }
}
