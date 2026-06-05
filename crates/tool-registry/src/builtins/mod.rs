//! §6.1 — capability-gated built-in tools that give a freshly-booted
//! ardur-agent useful shell and filesystem reach without any external MCP
//! server configuration.
//!
//! - [`ShellTool`] (`shell.run`) — run a shell command, optionally confined to
//!   an allowlist. **Read its [module security warning](shell) before use.**
//! - [`ReadFileTool`] (`file.read`), [`WriteFileTool`] (`file.write`),
//!   [`ListDirTool`] (`file.list`) — root-confined filesystem access; see the
//!   [`files`] module docs for the containment model.
//!
//! [`ToolRegistry::register_builtins`] is the convenience installer: it reads a
//! [`BuiltinOpts`] and registers only the tools whose configuration is present,
//! so a caller opts into shell and/or file access explicitly.
//!
//! Capability gating, restated:
//! - Shell access is opt-in per call site. An allowlist (production) is supplied
//!   via [`BuiltinOpts::shell_allowlist`]; omitting it registers the
//!   **unrestricted, dev-only** [`ShellTool::without_allowlist`].
//! - File access requires a [`BuiltinOpts::file_root`]; there is no constructor
//!   that grants the whole filesystem.

mod files;
mod shell;

pub use files::{ListDirTool, ReadFileTool, WriteFileTool};
pub use shell::ShellTool;

use std::path::PathBuf;

use crate::error::RegistryError;
use crate::registry::ToolRegistry;

/// Which §6.1 built-in tools [`ToolRegistry::register_builtins`] should install,
/// and how to configure them.
///
/// Every field is opt-in: the default registers nothing. A tool is installed
/// only when its configuration is present, so the caller — not the library —
/// decides which capabilities a booted agent exposes.
#[derive(Clone, Debug, Default)]
pub struct BuiltinOpts {
    /// Register [`ShellTool`] (`shell.run`). When `false`, no shell tool is
    /// installed regardless of `shell_allowlist`.
    pub enable_shell: bool,
    /// The shell command allowlist. `Some(patterns)` confines the shell to those
    /// patterns; `None` registers the **dev-only** unrestricted shell. Ignored
    /// unless `enable_shell` is `true`.
    pub shell_allowlist: Option<Vec<String>>,
    /// The root for the file tools (`file.read`, `file.write`, `file.list`).
    /// `Some(root)` registers all three confined to it; `None` installs no file
    /// tool.
    pub file_root: Option<PathBuf>,
}

impl ToolRegistry {
    /// Register the §6.1 built-in tools selected by `opts`.
    ///
    /// Installs [`ShellTool`] when `opts.enable_shell` is set — confined to
    /// `opts.shell_allowlist` when present, or **unrestricted (dev only)** when
    /// it is `None` — and the three file tools when `opts.file_root` is present.
    /// Each tool is independent: a disabled or unconfigured tool is simply
    /// skipped.
    ///
    /// # Security
    ///
    /// A shell with no allowlist runs arbitrary commands; supply an allowlist
    /// for any non-development deployment. See [`ShellTool`].
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateId`] if one of the built-in ids is
    /// already registered. On error, tools registered earlier in the call remain
    /// registered.
    pub fn register_builtins(&mut self, opts: BuiltinOpts) -> Result<(), RegistryError> {
        if opts.enable_shell {
            let tool = match opts.shell_allowlist {
                Some(allowlist) => ShellTool::with_allowlist(allowlist),
                None => ShellTool::without_allowlist(),
            };
            self.register(Box::new(tool))?;
        }

        if let Some(root) = opts.file_root {
            self.register(Box::new(ReadFileTool::with_root(root.clone())))?;
            self.register(Box::new(WriteFileTool::with_root(root.clone())))?;
            self.register(Box::new(ListDirTool::with_root(root)))?;
        }

        Ok(())
    }
}
