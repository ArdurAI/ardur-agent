//! §6.1 — capability-gated built-in tools that give a freshly-booted
//! ardur-agent useful shell and filesystem reach without any external MCP
//! server configuration.
//!
//! - [`ShellTool`] (`shell.run`) — run a shell command, optionally confined to
//!   an allowlist. **Read its [module security warning](shell) before use.**
//! - [`ReadFileTool`] (`file.read`), [`WriteFileTool`] (`file.write`),
//!   [`ListDirTool`] (`file.list`) — root-confined filesystem access; see the
//!   [`files`] module docs for the containment model.
//! - [`HttpFetchTool`] (`http.fetch`) — §6.2 SSRF-defended HTTP(S) GET/HEAD,
//!   strict-by-default (localhost-only without an allowlist). See the [`http`]
//!   module docs for the SSRF model.
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
mod http;
mod shell;

pub use files::{ListDirTool, ReadFileTool, WriteFileTool};
pub use http::HttpFetchTool;
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
    /// Configuration for the §6.2 [`HttpFetchTool`] (`http.fetch`). `Some(opts)`
    /// with `enable` set registers it; `None` (or `enable: false`) installs no
    /// HTTP tool.
    pub http: Option<HttpFetchOpts>,
}

/// How [`ToolRegistry::register_builtins`] should configure the §6.2
/// [`HttpFetchTool`].
///
/// The defaults are the strict-by-default posture: no allowlist (localhost
/// only), no private-IP access, a 1 MiB body ceiling, and at most 5 redirects.
/// See the [`http`](http) module docs for the SSRF model.
#[derive(Clone, Debug)]
pub struct HttpFetchOpts {
    /// Register [`HttpFetchTool`] when `true`.
    pub enable: bool,
    /// Host allowlist patterns (exact, `*.example.com`, or `*`). Empty leaves
    /// the tool able to reach only localhost.
    pub allowlist: Vec<String>,
    /// Permit URLs that resolve to private/internal IPs (disables the SSRF
    /// blocklist). Leave `false` unless the deployment must reach internal
    /// hosts.
    pub allow_private_ips: bool,
    /// Response-body ceiling in bytes.
    pub max_bytes: usize,
    /// Cap on redirects followed before erroring.
    pub redirect_limit: usize,
}

impl Default for HttpFetchOpts {
    fn default() -> Self {
        Self {
            enable: false,
            allowlist: Vec::new(),
            allow_private_ips: false,
            max_bytes: 1024 * 1024,
            redirect_limit: 5,
        }
    }
}

impl ToolRegistry {
    /// Register the §6.1 built-in tools selected by `opts`.
    ///
    /// Installs [`ShellTool`] when `opts.enable_shell` is set — confined to
    /// `opts.shell_allowlist` when present, or **unrestricted (dev only)** when
    /// it is `None` — the three file tools when `opts.file_root` is present, and
    /// [`HttpFetchTool`] when `opts.http` is `Some` with `enable` set. Each tool
    /// is independent: a disabled or unconfigured tool is simply skipped.
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

        if let Some(http) = opts.http {
            if http.enable {
                let tool = HttpFetchTool::new()
                    .with_allowlist(http.allowlist)
                    .with_private_ip_access(http.allow_private_ips)
                    .with_max_bytes(http.max_bytes)
                    .with_redirect_limit(http.redirect_limit);
                self.register(Box::new(tool))?;
            }
        }

        Ok(())
    }
}
