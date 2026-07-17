use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliCommand {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub args: Vec<CommandArg>,
    pub handler: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandArg {
    pub name: String,
    pub arg_type: String,
    pub required: bool,
    pub description: String,
}

impl CliCommand {
    pub fn new(name: &str, description: &str, usage: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            usage: usage.to_string(),
            args: Vec::new(),
            handler: name.to_string(),
        }
    }

    pub fn with_arg(mut self, name: &str, arg_type: &str, required: bool, description: &str) -> Self {
        self.args.push(CommandArg {
            name: name.to_string(),
            arg_type: arg_type.to_string(),
            required,
            description: description.to_string(),
        });
        self
    }
}

#[derive(Debug, Clone)]
pub struct CommandRegistry {
    commands: std::sync::Arc<std::sync::RwLock<HashMap<String, CliCommand>>>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            commands: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, command: CliCommand) -> crate::error::Result<()> {
        let mut commands = self.commands.write().map_err(|_| {
            crate::error::CliError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        commands.insert(command.name.clone(), command);
        Ok(())
    }

    pub fn get(&self, name: &str) -> crate::error::Result<CliCommand> {
        let commands = self.commands.read().map_err(|_| {
            crate::error::CliError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        commands
            .get(name)
            .cloned()
            .ok_or_else(|| crate::error::CliError::CommandNotFound(name.to_string()))
    }

    pub fn list(&self) -> crate::error::Result<Vec<CliCommand>> {
        let commands = self.commands.read().map_err(|_| {
            crate::error::CliError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(commands.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_creation() {
        let cmd = CliCommand::new("test", "A test command", "test <arg>");
        assert_eq!(cmd.name, "test");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn test_command_with_args() {
        let cmd = CliCommand::new("search", "Search command", "search <query>")
            .with_arg("query", "string", true, "Search query")
            .with_arg("limit", "integer", false, "Result limit");
        assert_eq!(cmd.args.len(), 2);
        assert!(cmd.args[0].required);
        assert!(!cmd.args[1].required);
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = CommandRegistry::new();
        let cmd = CliCommand::new("hello", "Say hello", "hello [name]");
        registry.register(cmd.clone()).unwrap();
        let retrieved = registry.get("hello").unwrap();
        assert_eq!(retrieved.name, "hello");
    }

    #[test]
    fn test_registry_list() {
        let registry = CommandRegistry::new();
        registry.register(CliCommand::new("cmd1", "First", "cmd1")).unwrap();
        registry.register(CliCommand::new("cmd2", "Second", "cmd2")).unwrap();
        let list = registry.list().unwrap();
        assert_eq!(list.len(), 2);
    }
}
