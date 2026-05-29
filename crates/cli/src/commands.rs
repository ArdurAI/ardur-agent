//! The REPL's built-in slash-commands and their registration.
//!
//! Each is a [`Command`] dispatched through the §1.0 [`CommandBus`]. The
//! stateless ones (`/help`, `/quit`, `/exit`) are installed by
//! [`register_default_commands`]; `/budget` is stateful — it reads the live
//! remaining-cents counter — so the chat loop registers a [`BudgetCommand`]
//! bound to the running [`ChatEngine`](crate::ChatEngine)'s budget handle.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ardur_runtime::{Command, CommandBus, CommandContext, CommandResult, RuntimeError};

/// The canonical name the REPL treats as "leave the chat" (alongside `exit`).
pub const QUIT_COMMAND: &str = "quit";
/// The alias for [`QUIT_COMMAND`].
pub const EXIT_COMMAND: &str = "exit";

/// `/help` — list the available commands.
struct HelpCommand;

impl Command for HelpCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult {
            output: [
                "Commands:",
                "  /help            show this help",
                "  /budget          show the remaining session budget",
                "  /quit, /exit     leave the chat",
                "Type anything else to send it as a chat message.",
            ]
            .join("\n"),
        })
    }
}

/// `/quit` (and `/exit`) — leave the chat. The handler only produces the
/// farewell; the REPL recognizes the command name to actually break the loop.
struct QuitCommand;

impl Command for QuitCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult {
            output: "Goodbye.".to_string(),
        })
    }
}

/// `/budget` — report the session's remaining budget. Holds a shared handle to
/// the [`ChatEngine`](crate::ChatEngine)'s remaining-cents counter so it always
/// reflects what turns have spent.
pub struct BudgetCommand {
    remaining: Arc<AtomicU64>,
}

impl BudgetCommand {
    /// Bind a `/budget` handler to a remaining-cents counter.
    #[must_use]
    pub fn new(remaining: Arc<AtomicU64>) -> Self {
        Self { remaining }
    }
}

impl Command for BudgetCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        let cents = self.remaining.load(Ordering::SeqCst);
        Ok(CommandResult {
            output: format!("budget: {cents}c remaining"),
        })
    }
}

/// Register the stateless default commands (`/help`, `/quit`, `/exit`) on `bus`.
///
/// `/budget` is registered separately by the chat loop because it needs the
/// running engine's live budget handle.
pub fn register_default_commands<B: CommandBus>(bus: &mut B) {
    bus.register_command("help", Box::new(HelpCommand));
    bus.register_command(QUIT_COMMAND, Box::new(QuitCommand));
    bus.register_command(EXIT_COMMAND, Box::new(QuitCommand));
}
