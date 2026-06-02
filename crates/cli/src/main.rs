//! The `ardur` binary entrypoint: `ardur chat` and `ardur version`.
//!
//! Plan family: §2.1 (`plans/2.1-cli-blueprint.md`). The library
//! ([`ardur_cli`]) holds the chat engine and slash-commands; this binary is a
//! thin clap front-end over [`ardur_cli::run_chat`].
#![forbid(unsafe_code)]

use std::process::ExitCode;

use ardur_cli::{ChatArgs, run_chat};
use clap::{Parser, Subcommand};

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
    /// Print the version and exit.
    Version,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Version => {
            println!("ardur {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Commands::Chat(args) => match run_chat(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}
