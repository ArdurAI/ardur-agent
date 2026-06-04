//! The `ardur-eval` binary — a clap CLI over the harness library.
//!
//! ```text
//! ardur-eval run  --scenarios <dir> --server-url <url> [--output json|junit|markdown]
//! ardur-eval list --scenarios <dir>
//! ardur-eval new  --id <id> [--scenarios <dir>]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use ardur_eval::output::{Format, Summary, render};
use ardur_eval::runner::{RunConfig, run_scenario};
use ardur_eval::scenario::Scenario;

#[derive(Parser)]
#[command(
    name = "ardur-eval",
    about = "Tau-Bench-style evaluation harness for a running ardur-server",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run every scenario in a directory against a server and grade the replies.
    Run {
        /// Directory of `*.yaml` scenario files.
        #[arg(long, default_value = "scenarios")]
        scenarios: PathBuf,
        /// Base URL of the running ardur-server, e.g. `http://localhost:8080`.
        #[arg(long)]
        server_url: String,
        /// Report format.
        #[arg(long, default_value = "markdown")]
        output: String,
        /// Override the chat endpoint path (default `/chat`).
        #[arg(long, default_value = "/chat")]
        chat_path: String,
    },
    /// List the scenarios discovered in a directory.
    List {
        /// Directory of `*.yaml` scenario files.
        #[arg(long, default_value = "scenarios")]
        scenarios: PathBuf,
    },
    /// Scaffold a new scenario YAML file.
    New {
        /// The scenario id (also the file stem).
        #[arg(long)]
        id: String,
        /// Directory to write the new `<id>.yaml` into.
        #[arg(long, default_value = "scenarios")]
        scenarios: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            scenarios,
            server_url,
            output,
            chat_path,
        } => cmd_run(scenarios, server_url, output, chat_path).await,
        Command::List { scenarios } => cmd_list(scenarios),
        Command::New { id, scenarios } => cmd_new(id, scenarios),
    }
}

async fn cmd_run(
    scenarios: PathBuf,
    server_url: String,
    output: String,
    chat_path: String,
) -> ExitCode {
    let format: Format = match output.parse() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let cases = match Scenario::load_dir(&scenarios) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if cases.is_empty() {
        eprintln!("error: no scenarios found in {}", scenarios.display());
        return ExitCode::from(2);
    }

    let mut config = RunConfig::new(server_url);
    config.chat_path = chat_path;
    let client = reqwest::Client::new();

    let mut results = Vec::with_capacity(cases.len());
    for scenario in &cases {
        results.push(run_scenario(&client, &config, scenario).await);
    }

    println!("{}", render(&results, format));

    if Summary::of(&results).is_green() {
        ExitCode::SUCCESS
    } else {
        // Non-zero so CI fails the job on a red eval run.
        ExitCode::FAILURE
    }
}

fn cmd_list(scenarios: PathBuf) -> ExitCode {
    let cases = match Scenario::load_dir(&scenarios) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if cases.is_empty() {
        println!("No scenarios found in {}", scenarios.display());
        return ExitCode::SUCCESS;
    }
    println!("{} scenario(s) in {}:", cases.len(), scenarios.display());
    for s in &cases {
        println!("  {:<28} {}", s.id, s.description);
    }
    ExitCode::SUCCESS
}

fn cmd_new(id: String, scenarios: PathBuf) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(&scenarios) {
        eprintln!("error: creating {}: {e}", scenarios.display());
        return ExitCode::from(2);
    }
    let path = scenarios.join(format!("{id}.yaml"));
    if path.exists() {
        eprintln!("error: {} already exists", path.display());
        return ExitCode::from(2);
    }
    let scenario = Scenario::scaffold(&id);
    let yaml = match scenario.to_yaml() {
        Ok(y) => y,
        Err(e) => {
            eprintln!("error: serializing scenario: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::write(&path, yaml) {
        eprintln!("error: writing {}: {e}", path.display());
        return ExitCode::from(2);
    }
    println!("Wrote {}", path.display());
    ExitCode::SUCCESS
}
