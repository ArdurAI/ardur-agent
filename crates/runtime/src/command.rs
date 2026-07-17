//! The typed command bus: a registry of named [`Command`] handlers and the
//! [`CommandBus`] that dispatches a [`CommandContext`] to them.

use std::collections::HashMap;

use crate::error::RuntimeError;

/// A dispatched command invocation: the registered name to run plus its raw
/// argument string. This is both the value handed to [`CommandBus::dispatch`]
/// and the context a [`Command`] executes against.
// TODO §1.0 Phase 2: carry the originating session and active cap token so
// commands can read history and emit signed receipts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandContext {
    /// The registered name of the command to run.
    pub command: String,
    /// The raw argument string for the command.
    pub args: String,
}

/// The outcome of executing a [`Command`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandResult {
    /// Human-readable output produced by the command.
    pub output: String,
}

/// A unit of work dispatchable through the [`CommandBus`].
pub trait Command {
    /// Execute the command against `ctx`, returning its result.
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError>;
}

/// A typed registry of named [`Command`] handlers.
pub trait CommandBus {
    /// Register `handler` under `name`, replacing any prior registration for
    /// that name.
    fn register_command(&mut self, name: impl Into<String>, handler: Box<dyn Command>);

    /// Dispatch `cmd` to the command registered under its
    /// [`command`](CommandContext::command) name, executing it against `cmd`.
    ///
    /// Returns [`RuntimeError::CommandNotFound`] if no handler is registered.
    fn dispatch(&self, cmd: CommandContext) -> Result<CommandResult, RuntimeError>;
}

/// An in-memory [`CommandBus`]: a name → boxed-handler map.
#[derive(Default)]
pub struct InMemoryCommandBus {
    handlers: HashMap<String, Box<dyn Command>>,
}

impl InMemoryCommandBus {
    /// Construct an empty command bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CommandBus for InMemoryCommandBus {
    fn register_command(&mut self, name: impl Into<String>, handler: Box<dyn Command>) {
        self.handlers.insert(name.into(), handler);
    }

    fn dispatch(&self, cmd: CommandContext) -> Result<CommandResult, RuntimeError> {
        let handler = self
            .handlers
            .get(&cmd.command)
            .ok_or_else(|| RuntimeError::CommandNotFound(cmd.command.clone()))?;
        handler.execute(&cmd)
    }
}
