//! [`Capability`] — the declarative permission a tool requires before it may
//! run.
//!
//! A tool advertises the capabilities it needs through
//! [`Tool::required_capabilities`](crate::Tool::required_capabilities). These are
//! enforced by the fused runtime before every invocation: each required
//! capability must appear in the verified cap-token's tool allowlist (as a
//! capability string obtained via [`Capability::as_str`]) or the call is denied
//! with [`RuntimeError::CapDenied`] before `invoke` runs.

use serde::{Deserialize, Serialize};

/// A discrete permission a tool needs to do its work.
///
/// The variants name the *resource classes* a tool can touch; the
/// authorization layers (cap-token + Cedar) decide whether a given session's
/// token actually grants them. [`Capability::Custom`] is the open extension
/// point for tools whose permission does not fit a built-in class.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read from the filesystem.
    FsRead,
    /// Write to the filesystem.
    FsWrite,
    /// Execute a shell command.
    ShellExec,
    /// Open an outbound network connection.
    NetworkOut,
    /// Spawn a child process.
    ProcessSpawn,
    /// Read process environment variables.
    EnvRead,
    /// Read the system clipboard.
    ClipboardRead,
    /// A capability outside the built-in classes, named by the tool author.
    Custom(String),
}

impl Capability {
    /// The stable string label a cap-token's `tool_allowlist` must include for
    /// this capability to be considered granted.
    ///
    /// Built-in capabilities map to `"cap.<snake_case>"` (e.g. `FsRead` →
    /// `"cap.fs_read"`); [`Capability::Custom`] uses `"cap.<name>"`. The fused
    /// runtime checks these labels against the verified cap-token claims before
    /// every `tool.invoke()`.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::FsRead => "cap.fs_read".to_string(),
            Self::FsWrite => "cap.fs_write".to_string(),
            Self::ShellExec => "cap.shell_exec".to_string(),
            Self::NetworkOut => "cap.network_out".to_string(),
            Self::ProcessSpawn => "cap.process_spawn".to_string(),
            Self::EnvRead => "cap.env_read".to_string(),
            Self::ClipboardRead => "cap.clipboard_read".to_string(),
            Self::Custom(name) => format!("cap.{name}"),
        }
    }
}
