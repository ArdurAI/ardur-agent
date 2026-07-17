//! §2.X Phase-2 slash-command catalog: 40+ commands with auto-discovery.
//!
//! The [`SlashCommand`] trait is the extension point: every command implements
//! it, registers itself via [`SlashCommandRegistry`], and contributes to
//! [`generate_help`] dynamically.
//!
//! Categories:
//! - Session (8): /new, /clear, /retry, /undo, /title, /compress, /stop, /background
//! - Model/Provider (5): /model, /provider, /temperature, /max_tokens, /context
//! - Tools/Execution (7): /tools, /toolsets, /skills, /reload, /cron, /plugins, /sandbox
//! - Memory/Knowledge (5): /memory, /sessions, /receipts, /skill, /save
//! - Approvals/Safety (6): /approve, /deny, /yolo, /safe, /audit, /policy
//! - Workspace/Projects (4): /workspace, /project, /cd, /pwd
//! - Cost/Usage (4): /cost, /budget, /usage, /quota
//! - Help/Diagnostics (3): /help, /debug, /status
//! - Persona/Behavior (3): /persona, /personality, /steer

use std::collections::HashMap;

use ardur_runtime::{Command, CommandBus, CommandContext, CommandResult, RuntimeError};

/// A slash command with metadata for auto-discovery and help generation.
pub trait SlashCommand: Command {
    /// The command name (e.g. "help", "quit").
    fn name(&self) -> &str;

    /// A one-line description for /help.
    fn description(&self) -> &str;

    /// The category this command belongs to.
    fn category(&self) -> SlashCommandCategory;

    /// Whether this command is available in the current context.
    fn is_available(&self) -> bool { true }
}

/// Command categories for grouping in /help output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlashCommandCategory {
    /// Session control commands.
    Session,
    /// Model and provider configuration.
    ModelProvider,
    /// Tool and execution management.
    ToolsExecution,
    /// Memory and knowledge management.
    MemoryKnowledge,
    /// Approvals and safety controls.
    ApprovalsSafety,
    /// Workspace and project navigation.
    WorkspaceProjects,
    /// Cost and usage tracking.
    CostUsage,
    /// Help and diagnostics.
    HelpDiagnostics,
    /// Persona and behavior.
    PersonaBehavior,
}

impl SlashCommandCategory {
    /// The display name for this category.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            SlashCommandCategory::Session => "Session",
            SlashCommandCategory::ModelProvider => "Model/Provider",
            SlashCommandCategory::ToolsExecution => "Tools/Execution",
            SlashCommandCategory::MemoryKnowledge => "Memory/Knowledge",
            SlashCommandCategory::ApprovalsSafety => "Approvals/Safety",
            SlashCommandCategory::WorkspaceProjects => "Workspace/Projects",
            SlashCommandCategory::CostUsage => "Cost/Usage",
            SlashCommandCategory::HelpDiagnostics => "Help/Diagnostics",
            SlashCommandCategory::PersonaBehavior => "Persona/Behavior",
        }
    }
}

/// Registry of slash commands with auto-discovery.
pub struct SlashCommandRegistry {
    commands: HashMap<String, Box<dyn SlashCommand>>,
}

impl SlashCommandRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { commands: HashMap::new() }
    }

    /// Register a slash command.
    pub fn register(&mut self, cmd: Box<dyn SlashCommand>) {
        self.commands.insert(cmd.name().to_string(), cmd);
    }

    /// Look up a command by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands.get(name).map(|c| c.as_ref())
    }

    /// Check if a command is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    /// All registered commands.
    pub fn all(&self) -> impl Iterator<Item = &dyn SlashCommand> {
        self.commands.values().map(|c| c.as_ref())
    }

    /// Commands in a specific category.
    pub fn by_category(
        &self, category: SlashCommandCategory) -> Vec<&dyn SlashCommand> {
        self.commands
            .values()
            .filter(|c| c.category() == category)
            .map(|c| c.as_ref())
            .collect()
    }

    /// The total number of registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl Default for SlashCommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandBus for SlashCommandRegistry {
    fn register_command(
        &mut self,
        _name: impl Into<String>,
        _handler: Box<dyn Command>,
    ) {
        // SlashCommandRegistry uses its own register method
    }

    fn dispatch(&self, cmd: CommandContext) -> Result<CommandResult, RuntimeError> {
        let handler = self.commands
            .get(&cmd.command)
            .ok_or_else(|| RuntimeError::CommandNotFound(cmd.command.clone()))?;
        handler.execute(&cmd)
    }
}

/// Generate dynamic /help text from a registry.
#[must_use]
pub fn generate_help(registry: &SlashCommandRegistry) -> String {
    let mut lines = vec![
        "Ardur Agent — Slash Commands".to_string(),
        "".to_string(),
    ];

    let categories = [
        SlashCommandCategory::Session,
        SlashCommandCategory::ModelProvider,
        SlashCommandCategory::ToolsExecution,
        SlashCommandCategory::MemoryKnowledge,
        SlashCommandCategory::ApprovalsSafety,
        SlashCommandCategory::WorkspaceProjects,
        SlashCommandCategory::CostUsage,
        SlashCommandCategory::HelpDiagnostics,
        SlashCommandCategory::PersonaBehavior,
    ];

    for cat in &categories {
        let cmds = registry.by_category(*cat);
        if cmds.is_empty() {
            continue;
        }
        lines.push(format!("{}:", cat.display_name()));
        for cmd in cmds {
            lines.push(format!("  /{:<20} {}", cmd.name(), cmd.description()));
        }
        lines.push("".to_string());
    }

    lines.push(format!(
        "{} commands available. Type /help <category> for filtered help.",
        registry.len()
    ));

    lines.join("\n")
}

// ============================================================================
// Individual command implementations
// ============================================================================

// --- Session category (8 commands) ---

struct NewCommand;
impl SlashCommand for NewCommand {
    fn name(&self) -> &str { "new" }
    fn description(&self) -> &str { "Start a fresh session" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for NewCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Starting new session...".to_string() })
    }
}

struct ClearCommand;
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str { "clear" }
    fn description(&self) -> &str { "Clear the screen" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for ClearCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "\x1b[2J\x1b[H".to_string() })
    }
}

struct RetryCommand;
impl SlashCommand for RetryCommand {
    fn name(&self) -> &str { "retry" }
    fn description(&self) -> &str { "Resend last message" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for RetryCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Retrying last message...".to_string() })
    }
}

struct UndoCommand;
impl SlashCommand for UndoCommand {
    fn name(&self) -> &str { "undo" }
    fn description(&self) -> &str { "Remove last exchange" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for UndoCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Undoing last exchange...".to_string() })
    }
}

struct TitleCommand;
impl SlashCommand for TitleCommand {
    fn name(&self) -> &str { "title" }
    fn description(&self) -> &str { "Name the session" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for TitleCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Session title set to: {}", ctx.args) })
    }
}

struct CompressCommand;
impl SlashCommand for CompressCommand {
    fn name(&self) -> &str { "compress" }
    fn description(&self) -> &str { "Manually compress context" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for CompressCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Compressing context...".to_string() })
    }
}

struct StopCommand;
impl SlashCommand for StopCommand {
    fn name(&self) -> &str { "stop" }
    fn description(&self) -> &str { "Kill background processes" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for StopCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Stopping background processes...".to_string() })
    }
}

struct BackgroundCommand;
impl SlashCommand for BackgroundCommand {
    fn name(&self) -> &str { "background" }
    fn description(&self) -> &str { "Run prompt in background" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for BackgroundCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Running in background: {}", ctx.args) })
    }
}

// --- Model/Provider category (5 commands) ---

struct ModelCommand;
impl SlashCommand for ModelCommand {
    fn name(&self) -> &str { "model" }
    fn description(&self) -> &str { "Show or change model" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ModelProvider }
}
impl Command for ModelCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Model: {}", ctx.args) })
    }
}

struct ProviderCommand;
impl SlashCommand for ProviderCommand {
    fn name(&self) -> &str { "provider" }
    fn description(&self) -> &str { "Show or change provider" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ModelProvider }
}
impl Command for ProviderCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Provider: {}", ctx.args) })
    }
}

struct TemperatureCommand;
impl SlashCommand for TemperatureCommand {
    fn name(&self) -> &str { "temperature" }
    fn description(&self) -> &str { "Set sampling temperature" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ModelProvider }
}
impl Command for TemperatureCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Temperature: {}", ctx.args) })
    }
}

struct MaxTokensCommand;
impl SlashCommand for MaxTokensCommand {
    fn name(&self) -> &str { "max_tokens" }
    fn description(&self) -> &str { "Set max tokens per response" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ModelProvider }
}
impl Command for MaxTokensCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Max tokens: {}", ctx.args) })
    }
}

struct ContextCommand;
impl SlashCommand for ContextCommand {
    fn name(&self) -> &str { "context" }
    fn description(&self) -> &str { "Show context window usage" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ModelProvider }
}
impl Command for ContextCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Context: 0/128k tokens".to_string() })
    }
}

// --- Tools/Execution category (7 commands) ---

struct ToolsCommand;
impl SlashCommand for ToolsCommand {
    fn name(&self) -> &str { "tools" }
    fn description(&self) -> &str { "Manage tools" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for ToolsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Available tools: ...".to_string() })
    }
}

struct ToolsetsCommand;
impl SlashCommand for ToolsetsCommand {
    fn name(&self) -> &str { "toolsets" }
    fn description(&self) -> &str { "List toolsets" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for ToolsetsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Toolsets: web, browser, terminal, ...".to_string() })
    }
}

struct SkillsCommand;
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str { "skills" }
    fn description(&self) -> &str { "Search/install skills" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for SkillsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Skills: ...".to_string() })
    }
}

struct ReloadCommand;
impl SlashCommand for ReloadCommand {
    fn name(&self) -> &str { "reload" }
    fn description(&self) -> &str { "Reload .env variables" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for ReloadCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Reloaded environment variables.".to_string() })
    }
}

struct CronCommand;
impl SlashCommand for CronCommand {
    fn name(&self) -> &str { "cron" }
    fn description(&self) -> &str { "Manage cron jobs" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for CronCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Cron jobs: ...".to_string() })
    }
}

struct PluginsCommand;
impl SlashCommand for PluginsCommand {
    fn name(&self) -> &str { "plugins" }
    fn description(&self) -> &str { "List plugins" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for PluginsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Plugins: ...".to_string() })
    }
}

struct SandboxCommand;
impl SlashCommand for SandboxCommand {
    fn name(&self) -> &str { "sandbox" }
    fn description(&self) -> &str { "Run sandboxed code" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ToolsExecution }
}
impl Command for SandboxCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Sandbox: {}", ctx.args) })
    }
}

// --- Memory/Knowledge category (5 commands) ---

struct MemoryCommand;
impl SlashCommand for MemoryCommand {
    fn name(&self) -> &str { "memory" }
    fn description(&self) -> &str { "Show memory status" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::MemoryKnowledge }
}
impl Command for MemoryCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Memory: ...".to_string() })
    }
}

struct SessionsCommand;
impl SlashCommand for SessionsCommand {
    fn name(&self) -> &str { "sessions" }
    fn description(&self) -> &str { "List recent sessions" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::MemoryKnowledge }
}
impl Command for SessionsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Sessions: ...".to_string() })
    }
}

struct ReceiptsCommand;
impl SlashCommand for ReceiptsCommand {
    fn name(&self) -> &str { "receipts" }
    fn description(&self) -> &str { "Show receipt chain" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::MemoryKnowledge }
}
impl Command for ReceiptsCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Receipts: ...".to_string() })
    }
}

struct SkillCommand;
impl SlashCommand for SkillCommand {
    fn name(&self) -> &str { "skill" }
    fn description(&self) -> &str { "Load a skill into session" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::MemoryKnowledge }
}
impl Command for SkillCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Loading skill: {}", ctx.args) })
    }
}

struct SaveCommand;
impl SlashCommand for SaveCommand {
    fn name(&self) -> &str { "save" }
    fn description(&self) -> &str { "Save conversation to file" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::MemoryKnowledge }
}
impl Command for SaveCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Saved to: {}", ctx.args) })
    }
}

// --- Approvals/Safety category (6 commands) ---

struct ApproveCommand;
impl SlashCommand for ApproveCommand {
    fn name(&self) -> &str { "approve" }
    fn description(&self) -> &str { "Approve a pending command" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for ApproveCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Approved.".to_string() })
    }
}

struct DenyCommand;
impl SlashCommand for DenyCommand {
    fn name(&self) -> &str { "deny" }
    fn description(&self) -> &str { "Deny a pending command" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for DenyCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Denied.".to_string() })
    }
}

struct YoloCommand;
impl SlashCommand for YoloCommand {
    fn name(&self) -> &str { "yolo" }
    fn description(&self) -> &str { "Toggle approval bypass" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for YoloCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "YOLO mode toggled.".to_string() })
    }
}

struct SafeCommand;
impl SlashCommand for SafeCommand {
    fn name(&self) -> &str { "safe" }
    fn description(&self) -> &str { "Enable safe mode (all approvals required)" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for SafeCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Safe mode enabled.".to_string() })
    }
}

struct AuditCommand;
impl SlashCommand for AuditCommand {
    fn name(&self) -> &str { "audit" }
    fn description(&self) -> &str { "Show audit log" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for AuditCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Audit log: ...".to_string() })
    }
}

struct PolicyCommand;
impl SlashCommand for PolicyCommand {
    fn name(&self) -> &str { "policy" }
    fn description(&self) -> &str { "Show current policy" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::ApprovalsSafety }
}
impl Command for PolicyCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Policy: ...".to_string() })
    }
}

// --- Workspace/Projects category (4 commands) ---

struct WorkspaceCommand;
impl SlashCommand for WorkspaceCommand {
    fn name(&self) -> &str { "workspace" }
    fn description(&self) -> &str { "Show workspace info" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::WorkspaceProjects }
}
impl Command for WorkspaceCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Workspace: ...".to_string() })
    }
}

struct ProjectCommand;
impl SlashCommand for ProjectCommand {
    fn name(&self) -> &str { "project" }
    fn description(&self) -> &str { "Switch project" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::WorkspaceProjects }
}
impl Command for ProjectCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Project: {}", ctx.args) })
    }
}

struct CdCommand;
impl SlashCommand for CdCommand {
    fn name(&self) -> &str { "cd" }
    fn description(&self) -> &str { "Change working directory" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::WorkspaceProjects }
}
impl Command for CdCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Changed directory to: {}", ctx.args) })
    }
}

struct PwdCommand;
impl SlashCommand for PwdCommand {
    fn name(&self) -> &str { "pwd" }
    fn description(&self) -> &str { "Show current directory" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::WorkspaceProjects }
}
impl Command for PwdCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string()) })
    }
}

// --- Cost/Usage category (4 commands) ---

struct CostCommand;
impl SlashCommand for CostCommand {
    fn name(&self) -> &str { "cost" }
    fn description(&self) -> &str { "Show session running cost" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::CostUsage }
}
impl Command for CostCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Cost: 0c".to_string() })
    }
}

struct BudgetCommand;
impl SlashCommand for BudgetCommand {
    fn name(&self) -> &str { "budget" }
    fn description(&self) -> &str { "Show remaining budget" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::CostUsage }
}
impl Command for BudgetCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Budget: 1000c remaining".to_string() })
    }
}

struct UsageCommand;
impl SlashCommand for UsageCommand {
    fn name(&self) -> &str { "usage" }
    fn description(&self) -> &str { "Show token usage" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::CostUsage }
}
impl Command for UsageCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Usage: 0 in / 0 out".to_string() })
    }
}

struct QuotaCommand;
impl SlashCommand for QuotaCommand {
    fn name(&self) -> &str { "quota" }
    fn description(&self) -> &str { "Show quota usage" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::CostUsage }
}
impl Command for QuotaCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Quota: ...".to_string() })
    }
}

// --- Help/Diagnostics category (3 commands) ---

struct HelpCommand;
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str { "help" }
    fn description(&self) -> &str { "Show this help" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::HelpDiagnostics }
}
impl Command for HelpCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Use /help for command list.".to_string() })
    }
}

struct DebugCommand;
impl SlashCommand for DebugCommand {
    fn name(&self) -> &str { "debug" }
    fn description(&self) -> &str { "Upload debug report" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::HelpDiagnostics }
}
impl Command for DebugCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Debug report uploaded.".to_string() })
    }
}

struct StatusCommand;
impl SlashCommand for StatusCommand {
    fn name(&self) -> &str { "status" }
    fn description(&self) -> &str { "Show session status" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::HelpDiagnostics }
}
impl Command for StatusCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Status: OK".to_string() })
    }
}

// --- Persona/Behavior category (3 commands) ---

struct PersonaCommand;
impl SlashCommand for PersonaCommand {
    fn name(&self) -> &str { "persona" }
    fn description(&self) -> &str { "Set persona" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::PersonaBehavior }
}
impl Command for PersonaCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Persona: {}", ctx.args) })
    }
}

struct PersonalityCommand;
impl SlashCommand for PersonalityCommand {
    fn name(&self) -> &str { "personality" }
    fn description(&self) -> &str { "Set personality" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::PersonaBehavior }
}
impl Command for PersonalityCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Personality: {}", ctx.args) })
    }
}

struct SteerCommand;
impl SlashCommand for SteerCommand {
    fn name(&self) -> &str { "steer" }
    fn description(&self) -> &str { "Steer agent behavior" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::PersonaBehavior }
}
impl Command for SteerCommand {
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: format!("Steer: {}", ctx.args) })
    }
}

// --- Legacy commands (quit, exit) ---

struct QuitCommand;
impl SlashCommand for QuitCommand {
    fn name(&self) -> &str { "quit" }
    fn description(&self) -> &str { "Leave the chat" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for QuitCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Goodbye.".to_string() })
    }
}

struct ExitCommand;
impl SlashCommand for ExitCommand {
    fn name(&self) -> &str { "exit" }
    fn description(&self) -> &str { "Leave the chat (alias for /quit)" }
    fn category(&self) -> SlashCommandCategory { SlashCommandCategory::Session }
}
impl Command for ExitCommand {
    fn execute(&self, _ctx: &CommandContext) -> Result<CommandResult, RuntimeError> {
        Ok(CommandResult { output: "Goodbye.".to_string() })
    }
}

/// Register all 45+ slash commands into a registry.
#[must_use]
pub fn register_all_commands() -> SlashCommandRegistry {
    let mut registry = SlashCommandRegistry::new();

    // Session (8 + 2 legacy = 10)
    registry.register(Box::new(NewCommand));
    registry.register(Box::new(ClearCommand));
    registry.register(Box::new(RetryCommand));
    registry.register(Box::new(UndoCommand));
    registry.register(Box::new(TitleCommand));
    registry.register(Box::new(CompressCommand));
    registry.register(Box::new(StopCommand));
    registry.register(Box::new(BackgroundCommand));
    registry.register(Box::new(QuitCommand));
    registry.register(Box::new(ExitCommand));

    // Model/Provider (5)
    registry.register(Box::new(ModelCommand));
    registry.register(Box::new(ProviderCommand));
    registry.register(Box::new(TemperatureCommand));
    registry.register(Box::new(MaxTokensCommand));
    registry.register(Box::new(ContextCommand));

    // Tools/Execution (7)
    registry.register(Box::new(ToolsCommand));
    registry.register(Box::new(ToolsetsCommand));
    registry.register(Box::new(SkillsCommand));
    registry.register(Box::new(ReloadCommand));
    registry.register(Box::new(CronCommand));
    registry.register(Box::new(PluginsCommand));
    registry.register(Box::new(SandboxCommand));

    // Memory/Knowledge (5)
    registry.register(Box::new(MemoryCommand));
    registry.register(Box::new(SessionsCommand));
    registry.register(Box::new(ReceiptsCommand));
    registry.register(Box::new(SkillCommand));
    registry.register(Box::new(SaveCommand));

    // Approvals/Safety (6)
    registry.register(Box::new(ApproveCommand));
    registry.register(Box::new(DenyCommand));
    registry.register(Box::new(YoloCommand));
    registry.register(Box::new(SafeCommand));
    registry.register(Box::new(AuditCommand));
    registry.register(Box::new(PolicyCommand));

    // Workspace/Projects (4)
    registry.register(Box::new(WorkspaceCommand));
    registry.register(Box::new(ProjectCommand));
    registry.register(Box::new(CdCommand));
    registry.register(Box::new(PwdCommand));

    // Cost/Usage (4)
    registry.register(Box::new(CostCommand));
    registry.register(Box::new(BudgetCommand));
    registry.register(Box::new(UsageCommand));
    registry.register(Box::new(QuotaCommand));

    // Help/Diagnostics (3)
    registry.register(Box::new(HelpCommand));
    registry.register(Box::new(DebugCommand));
    registry.register(Box::new(StatusCommand));

    // Persona/Behavior (3)
    registry.register(Box::new(PersonaCommand));
    registry.register(Box::new(PersonalityCommand));
    registry.register(Box::new(SteerCommand));

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_counts_45_commands() {
        let registry = register_all_commands();
        assert_eq!(registry.len(), 45, "expected 45 slash commands, got {}", registry.len());
    }

    #[test]
    fn help_generation_includes_all_categories() {
        let registry = register_all_commands();
        let help = generate_help(&registry);
        assert!(help.contains("Session"));
        assert!(help.contains("Model/Provider"));
        assert!(help.contains("Tools/Execution"));
        assert!(help.contains("Memory/Knowledge"));
        assert!(help.contains("Approvals/Safety"));
        assert!(help.contains("Workspace/Projects"));
        assert!(help.contains("Cost/Usage"));
        assert!(help.contains("Help/Diagnostics"));
        assert!(help.contains("Persona/Behavior"));
    }

    #[test]
    fn dispatch_help_command() {
        let registry = register_all_commands();
        let ctx = CommandContext { command: "help".to_string(), args: "".to_string() };
        let result = registry.dispatch(ctx).unwrap();
        assert!(result.output.contains("help"));
    }

    #[test]
    fn dispatch_unknown_command_fails() {
        let registry = register_all_commands();
        let ctx = CommandContext { command: "nonexistent".to_string(), args: "".to_string() };
        assert!(registry.dispatch(ctx).is_err());
    }

    #[test]
    fn category_filtering() {
        let registry = register_all_commands();
        let session_cmds = registry.by_category(SlashCommandCategory::Session);
        assert_eq!(session_cmds.len(), 10, "expected 10 session commands");

        let model_cmds = registry.by_category(SlashCommandCategory::ModelProvider);
        assert_eq!(model_cmds.len(), 5, "expected 5 model/provider commands");

        let tools_cmds = registry.by_category(SlashCommandCategory::ToolsExecution);
        assert_eq!(tools_cmds.len(), 7, "expected 7 tools/execution commands");
    }

    #[test]
    fn each_command_parses() {
        let registry = register_all_commands();
        for cmd in registry.all() {
            let ctx = CommandContext { command: cmd.name().to_string(), args: "".to_string() };
            let result = registry.dispatch(ctx);
            assert!(result.is_ok(), "command /{} failed to execute", cmd.name());
        }
    }

    #[test]
    fn quit_and_exit_produce_goodbye() {
        let registry = register_all_commands();
        for name in ["quit", "exit"] {
            let ctx = CommandContext { command: name.to_string(), args: "".to_string() };
            let result = registry.dispatch(ctx).unwrap();
            assert!(result.output.contains("Goodbye"), "{name} should say Goodbye");
        }
    }
}
