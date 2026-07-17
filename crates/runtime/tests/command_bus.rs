//! §1.0 Phase 1: the command bus registers named handlers, dispatches to them,
//! propagates handler errors, and returns `CommandNotFound` for unknown names.

use ardur_runtime::{
    Command, CommandBus, CommandContext, CommandResult, InMemoryCommandBus, RuntimeError,
};

struct Echo;
impl Command for Echo {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        let args = &ctx.args;
        Ok(CommandResult {
            output: format!("echo: {args}"),
        })
    }
}

struct AlwaysFails;
impl Command for AlwaysFails {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Err(RuntimeError::CostCeilingExceeded)
    }
}

fn invoke(command: &str, args: &str) -> CommandContext {
    CommandContext {
        command: command.to_string(),
        args: args.to_string(),
    }
}

#[test]
fn dispatches_to_registered_handler() {
    let mut bus = InMemoryCommandBus::new();
    bus.register_command("echo", Box::new(Echo));

    let result = bus
        .dispatch(invoke("echo", "ping"))
        .expect("registered command runs");
    assert_eq!(result.output, "echo: ping");
}

#[test]
fn unknown_command_is_not_found() {
    let bus = InMemoryCommandBus::new();
    match bus.dispatch(invoke("nope", "")) {
        Err(RuntimeError::CommandNotFound(name)) => assert_eq!(name, "nope"),
        other => panic!("expected CommandNotFound, got {other:?}"),
    }
}

#[test]
fn handler_error_propagates() {
    let mut bus = InMemoryCommandBus::new();
    bus.register_command("boom", Box::new(AlwaysFails));

    assert!(matches!(
        bus.dispatch(invoke("boom", "")),
        Err(RuntimeError::CostCeilingExceeded)
    ));
}
