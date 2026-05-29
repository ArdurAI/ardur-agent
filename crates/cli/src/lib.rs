//! ardur-cli — the `ardur` binary: a minimal interactive chat REPL over the
//! Phase-1 substrate.
//!
//! Plan family: §2.1 (`plans/2.1-cli-blueprint.md`). §18.8 keeps this PR scoped
//! to `crates/cli/**` plus the root manifest.
//!
//! # Phase 1 (this crate)
//!
//! - [`run_chat`] — load [`Config`], wire a [`ChatEngine`], and drive a
//!   `rustyline` REPL: a `/`-prefixed line dispatches through the §1.0
//!   [`CommandBus`]; anything else is submitted as a chat turn.
//! - [`register_default_commands`] / [`BudgetCommand`] — the built-in
//!   `/help`, `/quit`, `/exit`, and `/budget` slash-commands.
//! - [`ChatEngine`] / [`TurnOutcome`] — the wired runtime + cost-gate + provider
//!   substrate and the per-turn result.
//! - [`Config`] — `~/.ardur/config.toml` loading with defaults.
//! - [`CliError`] — the crate's single typed-error surface.
//!
//! The interactive `chat` subcommand routes everything through the §1.0
//! [`InMemoryRuntime`](ardur_runtime::InMemoryRuntime) echo stub and the §3.0
//! Anthropic stub; the inline `// TODO §2.1 Phase 2:` markers point at the live
//! provider dispatch, real cap-token minting, and projected cost envelopes.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod commands;
mod config;
mod engine;
mod error;

use std::io::Write;
use std::path::PathBuf;

use ardur_runtime::{ChatMessage, CommandBus, CommandContext, InMemoryCommandBus, RuntimeError};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

pub use commands::{BudgetCommand, register_default_commands};
pub use config::{Config, DEFAULT_BUDGET_CENTS, DEFAULT_MODEL};
pub use engine::{ChatEngine, TurnOutcome};
pub use error::CliError;

/// Arguments to the `ardur chat` subcommand.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct ChatArgs {
    /// Path to the config file (defaults to `~/.ardur/config.toml`).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// ANSI bold-cyan wrapping for the prompt.
const PROMPT_ON: &str = "\x1b[1;36m";
const PROMPT_OFF: &str = "\x1b[0m";

/// Run the interactive chat REPL: load config, wire the engine, register the
/// slash-commands, and loop reading lines until `/quit`, EOF, or interrupt.
pub fn run_chat(args: ChatArgs) -> Result<(), CliError> {
    // Logs go to stderr so they never interleave with the REPL's stdout. A
    // second `try_init` (e.g. from a test in-process) is a no-op rather than a
    // panic.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();

    let config = Config::load(args.config)?;
    if let Some(path) = Config::default_path() {
        tracing::debug!(config = %config::redacted_summary(&config, &path), "loaded config");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_chat_loop(config))
}

/// The async REPL body, driven by [`run_chat`]'s tokio runtime.
async fn run_chat_loop(config: Config) -> Result<(), CliError> {
    let engine = ChatEngine::new(&config)?;

    let mut bus = InMemoryCommandBus::new();
    register_default_commands(&mut bus);
    bus.register_command(
        "budget",
        Box::new(BudgetCommand::new(engine.budget_handle())),
    );

    let mut editor = DefaultEditor::new().map_err(readline_to_cli)?;
    println!(
        "ardur chat — model {} · type /help for commands.",
        config.model
    );

    let mut history: Vec<ChatMessage> = Vec::new();
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

                if let Some(rest) = line.strip_prefix('/') {
                    if dispatch_slash(&bus, rest) {
                        break; // a quit/exit command was handled
                    }
                } else {
                    run_chat_message(&engine, &mut history, line).await;
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
async fn run_chat_message(engine: &ChatEngine, history: &mut Vec<ChatMessage>, line: &str) {
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
