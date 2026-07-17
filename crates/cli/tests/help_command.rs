//! §2.1 Phase 1: the default commands register, and dispatching `/help` through
//! the bus produces output that names the available commands.

use ardur_cli::register_default_commands;
use ardur_runtime::{CommandBus, CommandContext, InMemoryCommandBus};

#[test]
fn help_command_lists_commands() {
    let mut bus = InMemoryCommandBus::new();
    register_default_commands(&mut bus);

    let result = bus
        .dispatch(CommandContext {
            command: "help".to_string(),
            args: String::new(),
        })
        .expect("/help is a default command");

    let output = result.output;
    assert!(
        output.contains("Commands:"),
        "help output should head a command list, got: {output}"
    );
    for expected in ["/help", "/budget", "/quit"] {
        assert!(
            output.contains(expected),
            "help output should mention {expected}, got: {output}"
        );
    }
}
