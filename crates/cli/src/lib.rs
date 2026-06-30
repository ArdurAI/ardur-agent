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

mod anim;
mod commands;
mod config;
mod engine;
mod error;
mod fused;
mod links;
mod markdown;
mod slash;
mod state;
mod stream;
mod theme;
mod toolbox;
mod util;
mod welcome;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use ardur_provider_runtime::{TelemetryConfig, init_genai_tracing, shutdown_genai_tracing};
use ardur_runtime::{ChatMessage, CommandBus, CommandContext, InMemoryCommandBus, RuntimeError};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

pub use anim::{TYPING_DOTS_FRAMES, TYPING_DOTS_HZ, TYPING_DOTS_TICK, TypingDots};
pub use commands::{BudgetCommand, register_default_commands};
pub use config::{Config, DEFAULT_BUDGET_CENTS, DEFAULT_MODEL};
pub use engine::{ChatEngine, TurnOutcome};
pub use error::CliError;
pub use fused::FusedEngine;
pub use links::{osc8_from_env, terminal_supports_osc8};
pub use markdown::{render_markdown, render_markdown_with};
pub use slash::{apply_theme_command, phase1_help};
pub use state::StateDirs;
pub use stream::{RenderCtx, StreamOutcome, drive_turn};
pub use theme::{Attr, Role, Theme, ThemeName};
pub use toolbox::{MAX_BOX_COLS, SessionCost, TurnStats, render_cost_line, render_tool_call_box};
pub use util::{display_width, layout_width};
pub use welcome::{default_state_path, is_first_launch, show_welcome_if_first, splash};

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

    /// Disable progressive streaming and render each turn from a single
    /// `complete()` call through the full fused pipeline (cap-token, Cedar, cost
    /// gate, signed receipt, journal). Useful for scripting and logging. The
    /// default streams tokens as they arrive when the backend supports it.
    #[arg(long)]
    pub no_stream: bool,

    /// Plain output: drop all colour/styling (as if `NO_COLOR` were set) and
    /// render each turn from a single `complete()` call so the Markdown is fully
    /// laid out — clean, escape-free output for piping and CI. Implies
    /// `--no-stream`. (`NO_COLOR` and a non-tty stdout select this automatically.)
    #[arg(long)]
    pub plain: bool,
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

    /// Run a `/memory` explorer command when the active substrate has memory.
    fn memory_command(&self, args: &str) -> String {
        match self {
            ActiveEngine::Fused(e) => e.memory_command(args),
            ActiveEngine::Echo(_) => "memory explorer is unavailable in --echo mode".to_string(),
        }
    }
}

/// The mutable per-session presentation state the REPL threads through every
/// turn: the active [`Theme`] (switchable live via `/theme`), the running
/// [`SessionCost`] tally (`/cost`), and whether OSC-8 hyperlinks are emitted.
struct ReplState {
    theme: Theme,
    cost: SessionCost,
    osc8: bool,
}

impl ReplState {
    /// The column budget the current terminal affords, capped at [`MAX_BOX_COLS`].
    fn width(&self) -> usize {
        layout_width(MAX_BOX_COLS)
    }
}

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
    let result = runtime.block_on(run_chat_loop(config, &args));

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
async fn run_chat_loop(config: Config, args: &ChatArgs) -> Result<(), CliError> {
    let stdout_tty = std::io::stdout().is_terminal();
    let stdin_tty = std::io::stdin().is_terminal();

    // Plain (escape-free) output when asked for it, when `NO_COLOR` is set, or when
    // stdout is not a tty (piped/CI) — and that path renders each turn from a
    // single `complete()` so the Markdown is fully laid out for clean piping.
    let force_plain = args.plain || std::env::var_os("NO_COLOR").is_some() || !stdout_tty;
    let theme = {
        let t = Theme::from_env();
        if force_plain { t.plain() } else { t }
    };
    let osc8 = !force_plain && stdout_tty && osc8_from_env();
    // Streaming is the default; `--no-stream`, `--plain`, or a non-tty stdout opt a
    // session into the single-`complete()` full-pipeline path for every turn.
    let stream_enabled = !args.no_stream && !force_plain;

    let mut state = ReplState {
        theme,
        cost: SessionCost::default(),
        osc8,
    };

    let engine = if args.echo {
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

    // The brand splash shows once, on the first interactive launch only.
    if stdin_tty && stdout_tty {
        if let Some(path) = welcome::default_state_path() {
            let mut stdout = std::io::stdout();
            let _ = welcome::show_welcome_if_first(&path, &state.theme, &mut stdout);
        }
    }

    println!(
        "{} chat — model {} · type {} for commands.",
        state
            .theme
            .paint_attr(Role::Primary, &[Attr::Bold], "ardur"),
        config.model,
        state.theme.paint(Role::Accent, "/help"),
    );

    let mut history: Vec<ChatMessage> = Vec::new();
    // A tty drives the rich line-editor; piped/redirected stdin reads lines
    // directly so `echo "hi" | ardur chat` (and the integration tests) work.
    if stdin_tty {
        run_interactive(&engine, &bus, &mut state, &mut history, stream_enabled).await?;
    } else {
        run_piped(&engine, &bus, &mut state, &mut history, stream_enabled).await;
    }
    Ok(())
}

/// The interactive REPL over a `rustyline` line-editor.
async fn run_interactive(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    stream_enabled: bool,
) -> Result<(), CliError> {
    let mut editor = DefaultEditor::new().map_err(readline_to_cli)?;
    loop {
        let budget = state
            .theme
            .paint(Role::Dim, &format!("[{}c]", engine.remaining_cents()));
        let glyph = state.theme.paint_attr(Role::Primary, &[Attr::Bold], "›");
        let prompt = format!("{budget} {glyph} ");
        match editor.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line);
                if handle_line(engine, bus, state, history, line, stream_enabled).await {
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
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    stream_enabled: bool,
) {
    use std::io::BufRead as _;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if handle_line(engine, bus, state, history, line, stream_enabled).await {
            break;
        }
    }
}

/// Handle one input line: dispatch a `/`-command, or submit the line as a chat
/// turn. Returns `true` when the loop should break (a quit/exit).
async fn handle_line(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    line: &str,
    stream_enabled: bool,
) -> bool {
    if let Some(rest) = line.strip_prefix('/') {
        dispatch_slash(engine, bus, state, rest)
    } else {
        run_chat_message(engine, state, history, line, stream_enabled).await;
        false
    }
}

/// Dispatch a `/`-stripped command line. The §2.X-live commands (`/theme`,
/// `/cost`, `/clear`) are handled here against the [`ReplState`]; everything else
/// (`/help`, `/budget`, `/quit`, `/exit`, unknown) routes through the bus.
/// Returns `true` if the command was `/quit` or `/exit`.
fn dispatch_slash(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    state: &mut ReplState,
    rest: &str,
) -> bool {
    let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    match command {
        "theme" => {
            match apply_theme_command(args, &mut state.theme) {
                Ok(msg) => println!("{}", state.theme.paint(Role::Dim, &msg)),
                Err(err) => println!("{}", state.theme.paint(Role::Error, &err)),
            }
            false
        }
        "cost" => {
            println!("{}", state.cost.render(&state.theme));
            false
        }
        "memory" => {
            println!("{}", engine.memory_command(args));
            false
        }
        "clear" => {
            // Erase the screen and home the cursor.
            print!("\x1b[2J\x1b[H");
            let _ = std::io::stdout().flush();
            false
        }
        _ => {
            let ctx = CommandContext {
                command: command.to_string(),
                args: args.to_string(),
            };
            match bus.dispatch(ctx) {
                Ok(result) => println!("{}", result.output),
                Err(RuntimeError::CommandNotFound(name)) => {
                    println!(
                        "{}",
                        state
                            .theme
                            .paint(Role::Dim, &format!("unknown command: /{name} — type /help"))
                    );
                }
                Err(e) => println!("command error: {e}"),
            }
            matches!(command, commands::QUIT_COMMAND | commands::EXIT_COMMAND)
        }
    }
}

/// Submit `line` as a chat turn, appending it (and any reply) to `history`.
///
/// When the session streams (`--no-stream` absent) and the active engine is the
/// fused substrate backing a streaming-capable provider, the turn renders
/// progressively via [`FusedEngine::stream_turn`] (which bypasses the fused
/// pipeline — see [`crate::stream`]). Otherwise it routes through the full
/// pipeline [`run_turn`](FusedEngine::run_turn)/echo path and prints the reply
/// with its cost.
async fn run_chat_message(
    engine: &ActiveEngine,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    line: &str,
    stream_enabled: bool,
) {
    history.push(ChatMessage::user(line));

    // Streaming path: only the fused engine, only when enabled and the backend
    // should stream (a live, streaming-capable provider). `--no-stream`, a
    // non-streaming backend (Codex, Claude-CLI), and the offline stub all keep
    // the full-pipeline `complete()` path below.
    if let ActiveEngine::Fused(fused) = engine {
        if stream_enabled && fused.should_stream() {
            run_streamed_message(fused, state, history).await;
            return;
        }
    }

    match engine.run_turn(history).await {
        Ok(outcome) => {
            // Render the reply through the Markdown core, then the running cost in
            // a dim trailing line.
            let rendered =
                render_markdown_with(&outcome.response, &state.theme, state.width(), state.osc8);
            println!("{rendered}");
            state.cost.record(0, 0, outcome.used_cents as f64 / 100.0);
            println!(
                "{}",
                state.theme.paint(
                    Role::Dim,
                    &format!(
                        "used {}c · remaining {}c",
                        outcome.used_cents, outcome.remaining_cents
                    )
                )
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

/// Render one turn by streaming the provider directly to stdout, then record the
/// assembled reply in `history` and fold the turn's cost into the session tally.
async fn run_streamed_message(
    fused: &FusedEngine,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
) {
    let ctx = RenderCtx {
        theme: &state.theme,
        width: state.width(),
        osc8: state.osc8,
    };
    let mut stdout = std::io::stdout();
    match fused.stream_turn(history, &mut stdout, &ctx).await {
        Ok(outcome) => {
            if let Some(usage) = outcome.usage {
                state.cost.record(
                    u64::from(usage.tokens_in),
                    u64::from(usage.tokens_out),
                    outcome.cost_cents.unwrap_or(0) as f64 / 100.0,
                );
            }
            // A clean stream (or one that errored *after* emitting content) leaves
            // a usable reply to record; an error with no content drops the user
            // message so a retry starts clean — the error was already printed.
            if outcome.content.is_empty() && outcome.error.is_some() {
                history.pop();
            } else {
                history.push(ChatMessage::assistant(outcome.content));
            }
        }
        Err(e) => {
            // An I/O failure writing to stdout (not a provider error) — drop the
            // unanswered user message and report.
            history.pop();
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
