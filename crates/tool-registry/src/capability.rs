//! [`Capability`] — the declarative permission a tool requires before it may
//! run.
//!
//! A tool advertises the capabilities it needs through
//! [`Tool::required_capabilities`](crate::Tool::required_capabilities). The
//! fused runtime enforces them before every invocation (ARD-420): the
//! cap-token verifier (§11.14) and the Cedar policy engine (§11.0) gate against
//! the set before a [`Tool::invoke`](crate::Tool::invoke) is admitted.

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
    /// The canonical string form used to carry the capability through the
    /// cap-token's `capabilities` claim (which is a `Vec<String>` so the
    /// cap-token crate never depends on this one). Built-in capabilities use
    /// their variant name; [`Capability::Custom`] is `custom:<name>`.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Capability::FsRead => "FsRead".to_string(),
            Capability::FsWrite => "FsWrite".to_string(),
            Capability::ShellExec => "ShellExec".to_string(),
            Capability::NetworkOut => "NetworkOut".to_string(),
            Capability::ProcessSpawn => "ProcessSpawn".to_string(),
            Capability::EnvRead => "EnvRead".to_string(),
            Capability::ClipboardRead => "ClipboardRead".to_string(),
            Capability::Custom(name) => format!("custom:{name}"),
        }
    }
}
