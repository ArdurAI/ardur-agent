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
mod background_task;
mod commands;
mod config;
mod engine;
mod error;
mod fused;
mod links;
mod markdown;
mod schedule_exec;
mod secure_io;
mod slash;
mod state;
mod stream;
mod theme;
mod toolbox;
mod util;
mod welcome;

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ardur_provider_runtime::{TelemetryConfig, init_genai_tracing, shutdown_genai_tracing};
use ardur_runtime::{
    ChatMessage, CommandBus, CommandContext, InMemoryCommandBus, RuntimeError, SessionId,
};
use ardur_session_journals::JournalEntry;
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
pub use schedule_exec::{
    DEFAULT_DRIVER_INTERVAL_SECS, ScheduleRecord, read_schedule_records, run_schedule_fire,
    run_schedule_run,
};
pub use secure_io::{
    create_private_file_no_follow, directory_modified_no_follow, list_directory_names_no_follow,
    read_file_no_follow, read_string_no_follow, remove_directory_tree_no_follow,
    write_private_file_atomic_no_follow, write_private_file_no_follow,
};
pub use slash::{apply_theme_command, phase1_help};
pub use state::{SessionMetadata, StateDirs};
pub use stream::{RenderCtx, StreamOutcome, drive_fused_turn, drive_turn};
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

    /// Resume an existing durable session journal by UUID.
    ///
    /// The previous user/assistant transcript is replayed into context before
    /// new turns are submitted, and new journal entries append to the same
    /// `~/.ardur/journals/sessions/<uuid>/journal.jsonl` file.
    #[arg(long = "session-id", value_name = "UUID")]
    pub session_id: Option<String>,
}

/// The active chat substrate for a session: the default [`FusedEngine`] or the
/// legacy [`ChatEngine`] echo runtime (selected by `--echo`). Both expose the
/// budget handle and per-turn entry the REPL drives.
enum ActiveEngine {
    /// The FusedRuntime-backed substrate (default). `Arc`, not `Box`: §1.9
    /// background tasks are `tokio::spawn`ed onto their own future holding a
    /// clone of this handle, running concurrently with the foreground REPL
    /// loop that owns the original.
    Fused(Arc<FusedEngine>),
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

    /// **§1.8.** Record a checkpoint, when the active substrate has a journal.
    async fn checkpoint(
        &self,
        label: Option<String>,
    ) -> Result<ardur_fused_runtime::CheckpointOutcome, CliError> {
        match self {
            ActiveEngine::Fused(e) => e.checkpoint(label).await,
            ActiveEngine::Echo(_) => Err(CliError::State(
                "checkpoints are unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.8.** List recorded checkpoints, when the active substrate has a journal.
    async fn list_checkpoints(&self) -> Result<Vec<ardur_fused_runtime::CheckpointInfo>, CliError> {
        match self {
            ActiveEngine::Fused(e) => e.list_checkpoints().await,
            ActiveEngine::Echo(_) => Err(CliError::State(
                "checkpoints are unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.8.** Roll back to a checkpoint, when the active substrate has a journal.
    async fn rollback(
        &self,
        checkpoint_id: uuid::Uuid,
    ) -> Result<(ardur_fused_runtime::RollbackOutcome, Vec<ChatMessage>), CliError> {
        match self {
            ActiveEngine::Fused(e) => e.rollback(checkpoint_id).await,
            ActiveEngine::Echo(_) => Err(CliError::State(
                "rollback is unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.7.** Summarize and install a compaction checkpoint, when the
    /// active substrate has a journal and a real provider.
    async fn compact(
        &self,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<ardur_fused_runtime::CompactOutcome, CliError> {
        match self {
            ActiveEngine::Fused(e) => e.compact(history, focus).await,
            ActiveEngine::Echo(_) => Err(CliError::State(
                "compact is unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.7.** Preview a compaction candidate without installing it.
    async fn preview_compact(
        &self,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<String, CliError> {
        match self {
            ActiveEngine::Fused(e) => e.preview_compact(history, focus).await,
            ActiveEngine::Echo(_) => Err(CliError::State(
                "compact is unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.9.** A cloned handle to the fused engine, for spawning a
    /// background task onto its own future — `--echo` mode has no journal or
    /// receipts, so background tasks are unavailable there.
    fn fused_engine_for_background_task(&self) -> Result<Arc<FusedEngine>, CliError> {
        match self {
            ActiveEngine::Fused(e) => Ok(Arc::clone(e)),
            ActiveEngine::Echo(_) => Err(CliError::State(
                "background tasks are unavailable in --echo mode (no durable journal)".to_string(),
            )),
        }
    }

    /// **§1.9.** A borrowed handle to the fused engine, for minting a
    /// background task's cancellation receipt.
    fn fused_engine_ref(&self) -> Result<&FusedEngine, CliError> {
        match self {
            ActiveEngine::Fused(e) => Ok(e),
            ActiveEngine::Echo(_) => Err(CliError::State(
                "background tasks are unavailable in --echo mode (no durable journal)".to_string(),
            )),
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

fn parse_session_id(raw: &str) -> Result<SessionId, CliError> {
    uuid::Uuid::parse_str(raw)
        .map(SessionId)
        .map_err(|e| CliError::State(format!("invalid session id `{raw}`: {e}")))
}

fn session_journal_path(dirs: &StateDirs, session_id: SessionId) -> PathBuf {
    dirs.journals
        .join("sessions")
        .join(session_id.0.to_string())
        .join("journal.jsonl")
}

fn load_history_from_journal(path: &Path) -> Result<Vec<ChatMessage>, CliError> {
    let contents = secure_io::read_string_no_follow(path)?;
    let mut entries = Vec::new();
    for (line_no, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: JournalEntry = serde_json::from_str(line).map_err(|e| {
            CliError::State(format!(
                "failed to parse {} line {}: {e}",
                path.display(),
                line_no + 1
            ))
        })?;
        entries.push(entry);
    }
    Ok(journal_entries_to_history(&entries))
}

/// Reconstruct a session's live chat history from its full entry log.
///
/// Honors `Rollback` markers (§1.8): entries strictly after a rollback's
/// target checkpoint, up to and including the `Rollback` entry itself, are
/// excluded from the reconstruction — but never deleted from the log. The
/// journal stays append-only; those entries remain readable there for audit,
/// they are just not part of the live session view. Multiple rollbacks
/// compose: each contributes its own exclusion range, so a later rollback to
/// an earlier checkpoint correctly re-excises everything an earlier rollback
/// had already excised.
pub(crate) fn journal_entries_to_history(entries: &[JournalEntry]) -> Vec<ChatMessage> {
    let mut checkpoint_positions: HashMap<uuid::Uuid, usize> = HashMap::new();
    for (pos, entry) in entries.iter().enumerate() {
        if let JournalEntry::Checkpoint { checkpoint_id, .. } = entry {
            checkpoint_positions.insert(*checkpoint_id, pos);
        }
    }

    let mut excluded = vec![false; entries.len()];
    for (pos, entry) in entries.iter().enumerate() {
        if let JournalEntry::Rollback {
            target_checkpoint_id,
            ..
        } = entry
        {
            if let Some(&checkpoint_pos) = checkpoint_positions.get(target_checkpoint_id) {
                for slot in excluded.iter_mut().take(pos + 1).skip(checkpoint_pos + 1) {
                    *slot = true;
                }
            }
        }
    }

    let mut history = Vec::new();
    for (pos, entry) in entries.iter().enumerate() {
        if excluded[pos] {
            continue;
        }
        match entry {
            JournalEntry::UserMessage { content, .. } => {
                history.push(ChatMessage::user(content.clone()));
            }
            JournalEntry::AssistantMessage { content, .. } => {
                history.push(ChatMessage::assistant(content.clone()));
            }
            JournalEntry::ToolInvocation { .. }
            | JournalEntry::CostFinalized { .. }
            | JournalEntry::Checkpoint { .. }
            | JournalEntry::Invalidation { .. }
            | JournalEntry::Rollback { .. } => {}
        }
    }
    history
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

    if args.echo && args.session_id.is_some() {
        return Err(CliError::State(
            "--session-id cannot be used with --echo because echo sessions are in-memory"
                .to_string(),
        ));
    }

    let resume_session_id = args
        .session_id
        .as_deref()
        .map(parse_session_id)
        .transpose()?;

    let mut state = ReplState {
        theme,
        cost: SessionCost::default(),
        osc8,
    };

    let mut restored_history: Vec<ChatMessage> = Vec::new();

    let engine = if args.echo {
        ActiveEngine::Echo(Box::new(ChatEngine::new(&config)?))
    } else {
        let dirs = StateDirs::resolve()?;
        dirs.create()?;
        if let Some(session_id) = resume_session_id {
            let journal_path = session_journal_path(&dirs, session_id);
            if !journal_path.is_file() {
                return Err(CliError::State(format!(
                    "session `{}` not found at {}",
                    session_id.0,
                    journal_path.display()
                )));
            }
            restored_history = load_history_from_journal(&journal_path)?;
            println!(
                "resumed session {} with {} prior messages",
                session_id.0,
                restored_history.len()
            );
        }
        let fused =
            FusedEngine::new_for_session(&config, &dirs, config.budget_cents, resume_session_id)
                .await?;
        if fused.offline() {
            println!(
                "running in offline mode (stub provider) — set ANTHROPIC_API_KEY for real LLM calls."
            );
        }
        ActiveEngine::Fused(Arc::new(fused))
    };

    let mut bus = InMemoryCommandBus::new();
    register_default_commands(&mut bus);
    bus.register_command(
        "budget",
        Box::new(BudgetCommand::new(engine.budget_handle())),
    );
    let tasks = background_task::TaskRegistry::new();

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

    let mut history: Vec<ChatMessage> = restored_history;
    // A tty drives the rich line-editor; piped/redirected stdin reads lines
    // directly so `echo "hi" | ardur chat` (and the integration tests) work.
    if stdin_tty {
        run_interactive(
            &engine,
            &bus,
            &tasks,
            &mut state,
            &mut history,
            stream_enabled,
        )
        .await?;
    } else {
        run_piped(
            &engine,
            &bus,
            &tasks,
            &mut state,
            &mut history,
            stream_enabled,
        )
        .await;
    }
    Ok(())
}

/// The interactive REPL over a `rustyline` line-editor.
async fn run_interactive(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    tasks: &background_task::TaskRegistry,
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
                if handle_line(engine, bus, tasks, state, history, line, stream_enabled).await {
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
    tasks: &background_task::TaskRegistry,
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
        if handle_line(engine, bus, tasks, state, history, line, stream_enabled).await {
            break;
        }
    }
}

/// Handle one input line: dispatch a `/`-command, or submit the line as a chat
/// turn. Returns `true` when the loop should break (a quit/exit).
async fn handle_line(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    line: &str,
    stream_enabled: bool,
) -> bool {
    if let Some(rest) = line.strip_prefix('/') {
        dispatch_slash(engine, bus, tasks, state, history, rest).await
    } else {
        run_chat_message(engine, state, history, line, stream_enabled).await;
        false
    }
}

/// Dispatch a `/`-stripped command line. The §2.X-live commands (`/theme`,
/// `/cost`, `/clear`), the §1.8 session-control commands (`/checkpoint`,
/// `/checkpoints`, `/rollback`), and the §1.9 background-task commands
/// (`/background`, `/bg`, `/btw`, `/tasks`, `/task ...`) are handled here
/// against the [`ReplState`] (and, for rollback, `history`); everything else
/// (`/help`, `/budget`, `/quit`, `/exit`, unknown) routes through the bus.
/// Returns `true` if the command was `/quit` or `/exit`.
async fn dispatch_slash(
    engine: &ActiveEngine,
    bus: &InMemoryCommandBus,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
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
        "checkpoint" => {
            let label = (!args.is_empty()).then(|| args.to_string());
            match engine.checkpoint(label).await {
                Ok(outcome) => println!(
                    "{}",
                    state.theme.paint(
                        Role::Dim,
                        &format!(
                            "checkpoint {} created: {}",
                            outcome.checkpoint_id, outcome.summary
                        )
                    )
                ),
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            }
            false
        }
        "checkpoints" => {
            match engine.list_checkpoints().await {
                Ok(checkpoints) if checkpoints.is_empty() => {
                    println!("{}", state.theme.paint(Role::Dim, "no checkpoints yet"));
                }
                Ok(checkpoints) => {
                    for cp in checkpoints {
                        println!("{} — {}", cp.checkpoint_id, cp.summary);
                    }
                }
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            }
            false
        }
        "rollback" => {
            match uuid::Uuid::parse_str(args) {
                Ok(checkpoint_id) => match engine.rollback(checkpoint_id).await {
                    Ok((outcome, restored_history)) => {
                        *history = restored_history;
                        println!(
                            "{}",
                            state.theme.paint(
                                Role::Dim,
                                &format!(
                                    "rolled back to checkpoint {} ({} messages restored)",
                                    outcome.target_checkpoint_id,
                                    history.len()
                                )
                            )
                        );
                    }
                    Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
                },
                Err(_) => println!("usage: /rollback <checkpoint-id>"),
            }
            false
        }
        "compact" | "compress" => {
            dispatch_compact(engine, state, history, args).await;
            false
        }
        "background" | "bg" | "btw" => {
            dispatch_background_start(engine, tasks, state, args);
            false
        }
        "tasks" => {
            dispatch_tasks_list(tasks, state);
            false
        }
        "task" => {
            dispatch_task_sub(engine, tasks, state, args).await;
            false
        }
        "steer" | "tell" => {
            dispatch_steer(engine, tasks, state, args).await;
            false
        }
        "interrupt" => {
            dispatch_interrupt(engine, tasks, state, args).await;
            false
        }
        "queue" | "status" => {
            dispatch_queue_status(tasks, state);
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

/// A rough (~4 chars/token) token-count estimate over `history`'s content —
/// not a real tokenizer, just enough for `/compact status`'s before/after
/// sizing. Mirrors `ardur_fused_runtime`'s internal estimator; kept local
/// since it's a trivial, purely-local read over data the REPL already holds
/// (no need to round-trip through the runtime for it).
fn estimate_tokens(history: &[ChatMessage]) -> u64 {
    let chars: usize = history.iter().map(|m| m.content.len()).sum();
    (chars as u64).div_ceil(4)
}

/// **§1.7.** Dispatch a `/compact`/`/compress`-stripped argument string:
/// `/compact [focus text]` (summarize + install), `/compact preview [focus]`
/// (summarize without installing), `/compact status` (a rough context-size
/// estimate), `/compact history` (list compaction checkpoints — currently
/// the same list `/checkpoints` shows, since a compaction checkpoint and a
/// manual checkpoint are the same underlying journal record), `/compact get
/// <id>` (show one checkpoint's full summary text), and `/compact restore
/// <id>` (roll back to it — the same operation `/rollback` performs).
async fn dispatch_compact(
    engine: &ActiveEngine,
    state: &mut ReplState,
    history: &mut Vec<ChatMessage>,
    args: &str,
) {
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match sub {
        "status" => {
            let tokens = estimate_tokens(history);
            println!(
                "{}",
                state.theme.paint(
                    Role::Dim,
                    &format!(
                        "{} messages, ~{tokens} tokens (rough estimate, not a real tokenizer)",
                        history.len()
                    )
                )
            );
        }
        "history" => match engine.list_checkpoints().await {
            Ok(checkpoints) if checkpoints.is_empty() => {
                println!("{}", state.theme.paint(Role::Dim, "no checkpoints yet"));
            }
            Ok(checkpoints) => {
                for cp in checkpoints {
                    println!("{} — {}", cp.checkpoint_id, cp.summary);
                }
            }
            Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
        },
        "get" => match uuid::Uuid::parse_str(rest) {
            Ok(checkpoint_id) => match engine.list_checkpoints().await {
                Ok(checkpoints) => match checkpoints
                    .into_iter()
                    .find(|c| c.checkpoint_id == checkpoint_id)
                {
                    Some(cp) => println!("{}", cp.summary),
                    None => println!("checkpoint {checkpoint_id} not found"),
                },
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            },
            Err(_) => println!("usage: /compact get <checkpoint-id>"),
        },
        "restore" => match uuid::Uuid::parse_str(rest) {
            Ok(checkpoint_id) => match engine.rollback(checkpoint_id).await {
                Ok((outcome, restored_history)) => {
                    *history = restored_history;
                    println!(
                        "{}",
                        state.theme.paint(
                            Role::Dim,
                            &format!(
                                "restored checkpoint {} ({} messages)",
                                outcome.target_checkpoint_id,
                                history.len()
                            )
                        )
                    );
                }
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            },
            Err(_) => println!("usage: /compact restore <checkpoint-id>"),
        },
        "preview" => {
            let focus = (!rest.is_empty()).then(|| rest.to_string());
            match engine.preview_compact(history, focus).await {
                Ok(summary) => println!("{summary}"),
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            }
        }
        _ => {
            // Anything else is focus text for a real compact-and-install —
            // including the empty string, for a bare `/compact`.
            let focus = (!args.is_empty()).then(|| args.to_string());
            match engine.compact(history, focus).await {
                Ok(outcome) => {
                    *history = vec![ChatMessage::system(outcome.summary.clone())];
                    println!(
                        "{}",
                        state.theme.paint(
                            Role::Dim,
                            &format!(
                                "compacted: checkpoint {} (~{} -> ~{} tokens)",
                                outcome.checkpoint_id,
                                outcome.before_tokens_estimate,
                                outcome.after_tokens_estimate
                            )
                        )
                    );
                }
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            }
        }
    }
}

/// **§1.9.** `/background <prompt>` (aliases `/bg`, `/btw`): spawn `prompt`
/// as a new background task and print its id immediately — the task runs
/// concurrently; `/tasks`/`/task status <id>` poll its progress.
fn dispatch_background_start(
    engine: &ActiveEngine,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    args: &str,
) {
    if args.is_empty() {
        println!("usage: /background <prompt> (aliases: /bg, /btw)");
        return;
    }
    match engine.fused_engine_for_background_task() {
        Ok(fused) => {
            let id = tasks.spawn(fused, args.to_string());
            println!(
                "{}",
                state
                    .theme
                    .paint(Role::Dim, &format!("started background task {id}"))
            );
        }
        Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
    }
}

/// **§1.9.** `/tasks`: list every background task started this process, most
/// recent activity is not ordered (the blueprint's `--active`/`--status`/
/// `--runtime` filters are deferred).
fn dispatch_tasks_list(tasks: &background_task::TaskRegistry, state: &mut ReplState) {
    let mut all = tasks.list();
    if all.is_empty() {
        println!("{}", state.theme.paint(Role::Dim, "no background tasks"));
        return;
    }
    all.sort_by_key(|t| t.id.0);
    for task in all {
        println!("{} [{}] {}", task.id, task.status, task.prompt);
    }
}

/// **§1.9.** `/task status|log|result|cancel <task_id>`.
async fn dispatch_task_sub(
    engine: &ActiveEngine,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    args: &str,
) {
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    let Ok(id) = rest.parse::<uuid::Uuid>().map(background_task::TaskId) else {
        println!("usage: /task status|log|result|cancel <task-id>");
        return;
    };
    match sub {
        "status" | "log" => match tasks.get(id) {
            Some(task) => {
                println!(
                    "{} [{}] owner-session={} {}{}{}",
                    task.id,
                    task.status,
                    task.owner_session_id.0,
                    task.prompt,
                    task.result
                        .as_ref()
                        .map(|r| format!("\nresult: {r}"))
                        .unwrap_or_default(),
                    task.error
                        .as_ref()
                        .map(|e| format!("\nerror: {e}"))
                        .unwrap_or_default(),
                );
                for directive in &task.steer_directives {
                    println!("steer[{}]: {}", directive.receipt_id.0, directive.message);
                }
            }
            None => println!("task {id} not found"),
        },
        "result" => match tasks.get(id) {
            Some(task) => match (task.result, task.error) {
                (Some(result), _) => println!("{result}"),
                (None, Some(error)) => {
                    println!(
                        "{}",
                        state.theme.paint(Role::Error, &format!("failed: {error}"))
                    );
                }
                (None, None) => println!(
                    "{}",
                    state
                        .theme
                        .paint(Role::Dim, &format!("task {id} is still {}", task.status))
                ),
            },
            None => println!("task {id} not found"),
        },
        "cancel" => match engine.fused_engine_ref() {
            Ok(fused) => match tasks.cancel(fused, id).await {
                Ok(()) => println!(
                    "{}",
                    state
                        .theme
                        .paint(Role::Dim, &format!("cancelled task {id}"))
                ),
                Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
            },
            Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
        },
        _ => println!("usage: /task status|log|result|cancel <task-id>"),
    }
}

/// **§1.10.** `/steer <task-id> <message>` (alias `/tell`): accept a
/// steering directive against an active background task. Receipted and
/// durably recorded on the task, but — per
/// [`ardur_fused_runtime::FusedRuntime::accept_steer_directive`]'s
/// documented limitation — not yet consumed by the one-shot task runtime.
async fn dispatch_steer(
    engine: &ActiveEngine,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    args: &str,
) {
    let (id_str, message) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let message = message.trim();
    let Ok(id) = id_str.parse::<uuid::Uuid>().map(background_task::TaskId) else {
        println!("usage: /steer <task-id> <message> (alias: /tell)");
        return;
    };
    if message.is_empty() {
        println!("usage: /steer <task-id> <message> (alias: /tell)");
        return;
    }
    match engine.fused_engine_ref() {
        Ok(fused) => match tasks.steer(fused, id, message.to_string()).await {
            Ok(()) => println!(
                "{}",
                state.theme.paint(
                    Role::Dim,
                    &format!("steering directive accepted for task {id}")
                )
            ),
            Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
        },
        Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
    }
}

/// **§1.10.** `/interrupt <task-id>`: accept an interrupt against an active
/// background task, aborting it — mechanically identical to `/task cancel`
/// but a distinct receipted intent (see
/// [`ardur_fused_runtime::FusedRuntime::accept_interrupt`]).
async fn dispatch_interrupt(
    engine: &ActiveEngine,
    tasks: &background_task::TaskRegistry,
    state: &mut ReplState,
    args: &str,
) {
    let Ok(id) = args
        .trim()
        .parse::<uuid::Uuid>()
        .map(background_task::TaskId)
    else {
        println!("usage: /interrupt <task-id>");
        return;
    };
    match engine.fused_engine_ref() {
        Ok(fused) => match tasks.interrupt(fused, id).await {
            Ok(()) => println!(
                "{}",
                state
                    .theme
                    .paint(Role::Dim, &format!("interrupted task {id}"))
            ),
            Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
        },
        Err(e) => println!("{}", state.theme.paint(Role::Error, &format!("{e}"))),
    }
}

/// **§1.10.** `/queue` (alias `/status`): a summary of how many background
/// tasks are active vs. terminal, and how many steering directives have
/// been accepted but are not yet deliverable to their target.
fn dispatch_queue_status(tasks: &background_task::TaskRegistry, state: &mut ReplState) {
    let summary = tasks.queue_summary();
    println!(
        "{}",
        state.theme.paint(
            Role::Dim,
            &format!(
                "{} active, {} terminal, {} steering directive(s) pending delivery",
                summary.active_tasks, summary.terminal_tasks, summary.pending_steer_directives
            )
        )
    );
}

/// Submit `line` as a chat turn, appending it (and any reply) to `history`.
///
/// When the session streams (`--no-stream` absent) and the active engine is the
/// fused substrate backing a streaming-capable provider, the turn renders
/// progressively through [`FusedEngine::stream_turn`] and the full fused
/// pipeline. Otherwise it routes through [`run_turn`](FusedEngine::run_turn)/echo
/// and prints the reply with its cost.
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
            apply_streamed_outcome_to_history(history, outcome);
        }
        Err(e) => {
            // An I/O failure writing to stdout (not a provider error) — drop the
            // unanswered user message and report.
            history.pop();
            eprintln!("error: {e}");
        }
    }
}

fn apply_streamed_outcome_to_history(history: &mut Vec<ChatMessage>, outcome: StreamOutcome) {
    // Before the first receipt, a terminal error leaves no durable turn, so
    // remove the unanswered user message. Every receipt corresponds to one
    // journaled assistant response; preserve those round boundaries so live
    // tool-loop history matches a later journal replay. Any trailing partial
    // content after the last receipt remains display-only.
    if outcome.error.is_some() && outcome.receipt_ids.is_empty() {
        history.pop();
    } else if !outcome.receipt_ids.is_empty() {
        debug_assert_eq!(
            outcome.receipt_ids.len(),
            outcome.committed_assistant_messages.len()
        );
        history.extend(
            outcome
                .committed_assistant_messages
                .into_iter()
                .map(ChatMessage::assistant),
        );
    } else {
        history.push(ChatMessage::assistant(outcome.content));
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

#[cfg(test)]
mod streamed_history_tests {
    use super::*;
    use ardur_runtime::ReceiptId;

    #[test]
    fn pre_commit_stream_error_removes_the_unanswered_user_message() {
        let mut history = vec![ChatMessage::user("question")];
        apply_streamed_outcome_to_history(
            &mut history,
            StreamOutcome {
                content: "partial".to_string(),
                error: Some("provider unavailable".to_string()),
                ..StreamOutcome::default()
            },
        );
        assert!(history.is_empty());
    }

    #[test]
    fn post_commit_stream_error_preserves_durable_local_history() {
        let mut history = vec![ChatMessage::user("question")];
        apply_streamed_outcome_to_history(
            &mut history,
            StreamOutcome {
                content: "durable answer".to_string(),
                committed_assistant_messages: vec!["durable answer".to_string()],
                receipt_ids: vec![ReceiptId(uuid::Uuid::new_v4())],
                error: Some("later provider round failed".to_string()),
                ..StreamOutcome::default()
            },
        );
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "durable answer");
    }

    #[test]
    fn multi_round_stream_preserves_journal_assistant_boundaries() {
        let mut history = vec![ChatMessage::user("question")];
        apply_streamed_outcome_to_history(
            &mut history,
            StreamOutcome {
                content: "tool planfinal answer".to_string(),
                committed_assistant_messages: vec![
                    "tool plan".to_string(),
                    "final answer".to_string(),
                ],
                receipt_ids: vec![
                    ReceiptId(uuid::Uuid::new_v4()),
                    ReceiptId(uuid::Uuid::new_v4()),
                ],
                ..StreamOutcome::default()
            },
        );
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].content, "tool plan");
        assert_eq!(history[2].content, "final answer");
    }

    #[cfg(unix)]
    #[test]
    fn journal_history_loader_rejects_symlinked_files() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().canonicalize().expect("canonical tempdir");
        let target = dir_path.join("target.jsonl");
        let journal = dir_path.join("journal.jsonl");
        std::fs::write(
            &target,
            r#"{"kind":"UserMessage","content":"forged","at":1}"#,
        )
        .expect("target journal");
        symlink(&target, &journal).expect("journal symlink");

        let error = load_history_from_journal(&journal)
            .expect_err("session history must not follow a journal symlink");
        assert!(error.to_string().contains("symlink"), "{error}");
    }
}

#[cfg(test)]
mod journal_entries_to_history_tests {
    use super::*;

    fn user(content: &str) -> JournalEntry {
        JournalEntry::UserMessage {
            content: content.to_string(),
            at: ardur_session_journals::UnixTsMillis(0),
        }
    }

    fn assistant(content: &str) -> JournalEntry {
        JournalEntry::AssistantMessage {
            content: content.to_string(),
            at: ardur_session_journals::UnixTsMillis(0),
            receipt_id: ardur_runtime::ReceiptId(uuid::Uuid::new_v4()),
        }
    }

    fn checkpoint(id: uuid::Uuid) -> JournalEntry {
        JournalEntry::Checkpoint {
            checkpoint_id: id,
            summary: "cp".to_string(),
            at: ardur_session_journals::UnixTsMillis(0),
        }
    }

    fn rollback(target: uuid::Uuid) -> JournalEntry {
        JournalEntry::Rollback {
            target_checkpoint_id: target,
            receipt_id: ardur_runtime::ReceiptId(uuid::Uuid::new_v4()),
            at: ardur_session_journals::UnixTsMillis(0),
        }
    }

    /// With no `Rollback` markers, every user/assistant message survives in
    /// order and everything else (tool calls, cost, checkpoints) is dropped —
    /// unchanged behavior from before §1.8.
    #[test]
    fn no_rollback_keeps_every_message_in_order() {
        let entries = vec![user("hi"), assistant("hello"), user("bye")];
        let history = journal_entries_to_history(&entries);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "hi");
        assert_eq!(history[1].content, "hello");
        assert_eq!(history[2].content, "bye");
    }

    /// A rollback excludes exactly the messages between its target checkpoint
    /// and the rollback marker (inclusive of the marker itself), while
    /// messages before the checkpoint and after the marker both survive.
    #[test]
    fn rollback_excludes_only_the_rolled_back_range() {
        let cp = uuid::Uuid::new_v4();
        let entries = vec![
            user("keep 1"),         // 0
            checkpoint(cp),         // 1
            user("rolled back"),    // 2 — excluded
            assistant("also gone"), // 3 — excluded
            rollback(cp),           // 4 — excluded (marker itself)
            user("keep 2"),         // 5 — after the marker, survives
        ];
        let history = journal_entries_to_history(&entries);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "keep 1");
        assert_eq!(history[1].content, "keep 2");
    }

    /// Two sequential rollbacks each contribute their own exclusion range —
    /// a later rollback to an earlier checkpoint re-excises everything an
    /// earlier rollback had already excised, plus more.
    #[test]
    fn sequential_rollbacks_compose() {
        let cp_a = uuid::Uuid::new_v4();
        let cp_b = uuid::Uuid::new_v4();
        let entries = vec![
            checkpoint(cp_a), // 0
            user("branch A"), // 1
            checkpoint(cp_b), // 2
            user("branch B"), // 3
            rollback(cp_b),   // 4 — first rollback: excludes 3, 4
            user("branch C"), // 5 — new work after rollback 1
            rollback(cp_a),   // 6 — second rollback: excludes 1..=6 (re-excising "branch A" too)
        ];
        let history = journal_entries_to_history(&entries);
        // Every UserMessage after checkpoint A is excluded by the second
        // rollback's range (positions 1..=6); nothing survives past position 0.
        assert!(history.is_empty(), "{history:?}");
    }

    /// A `Rollback` entry naming a checkpoint id that isn't present in the
    /// log (shouldn't happen via the runtime's own validated path, but the
    /// pure reconstruction function must not panic on it) is simply a no-op
    /// exclusion — every message still survives.
    #[test]
    fn rollback_to_missing_checkpoint_id_is_a_no_op() {
        let entries = vec![user("a"), rollback(uuid::Uuid::new_v4()), user("b")];
        let history = journal_entries_to_history(&entries);
        assert_eq!(history.len(), 2);
    }
}
