# §6.1 — built-in tools

Capability-gated shell and filesystem tools that give a freshly-booted
ardur-agent useful reach **without** any external MCP server configuration.
Everything here is opt-in at registration: nothing is installed unless the
caller asks for it via [`BuiltinOpts`](./mod.rs).

| id          | tool            | capabilities             | gate                          |
| ----------- | --------------- | ------------------------ | ----------------------------- |
| `shell.run` | `ShellTool`     | `ShellExec`, `ProcessSpawn` | allowlist (opt-in)         |
| `file.read` | `ReadFileTool`  | `FsRead`                 | confined to a root            |
| `file.write`| `WriteFileTool` | `FsWrite`                | confined to a root            |
| `file.list` | `ListDirTool`   | `FsRead`                 | confined to a root            |

## Wiring it up

```rust
use std::path::PathBuf;
use ardur_tool_registry::{BuiltinOpts, ToolRegistry};

let mut registry = ToolRegistry::new();
registry.register_builtins(BuiltinOpts {
    enable_shell: true,
    // Production: confine the shell to leaf commands you trust.
    shell_allowlist: Some(vec!["git status".into(), "ls|cat|echo".into()]),
    // File tools are confined to this root; omit to install none.
    file_root: Some(PathBuf::from("/srv/agent/workspace")),
})?;
# Ok::<(), ardur_tool_registry::RegistryError>(())
```

Each field is independent — set only what you want. The default `BuiltinOpts`
registers nothing.

## Shell allowlist syntax

`shell_allowlist` is a `Vec<String>`. Each entry is one or more `|`-separated
command **prefixes**. A command is permitted when, after trimming leading
whitespace, it equals one of the alternatives or begins with one followed by
whitespace:

| allowlist entry | permits            | rejects                |
| --------------- | ------------------ | ---------------------- |
| `"git"`         | `git`, `git status`| `gitfoo`               |
| `"ls\|cat\|echo"` | `ls -la`, `cat x`  | `rm -rf /`             |
| `"git status"`  | `git status`       | `git push`             |

A command matching nothing is refused with `ToolError::Denied { reason }`.

## What is safe, what is not

- **`ShellTool::without_allowlist()` (i.e. `shell_allowlist: None` with
  `enable_shell: true`) is dev-only.** It runs *anything* — unrestricted remote
  code execution. Never register it on a server or behind any channel adapter an
  untrusted prompt can reach. Always pass an allowlist in production.
- **The allowlist is a prefix gate, not a sandbox.** It does not parse shell
  grammar. An allowed prefix that reaches a shell built-in or interpreter
  (`bash -c`, `sh`, `env`, `xargs`, `find -exec`, …) can pivot to arbitrary
  execution. Allowlist genuinely-leaf commands only.
- **File tools cannot address the whole filesystem.** There is no constructor
  without a root. Absolute paths and `..` traversal are refused, and the
  resolved path's nearest existing ancestor is canonicalized and checked to stay
  under the canonical root — which also catches symlink escapes at check time.
- **Containment has a TOCTOU gap.** A symlink swapped between the containment
  check and the subsequent open could still redirect outside the root. These
  tools are a convenience boundary, not a security sandbox. Pair them with the
  §11 capability-token + Cedar policy layers whenever the prompt is untrusted —
  the capabilities each tool declares (`required_capabilities`) are what those
  layers gate against.

## Read ceilings

- `file.read` truncates at `max_bytes` (default 64 KiB) and reports `truncated`.
- `file.list` truncates at `max_entries` (default 100) and reports `truncated`.
- `shell.run` kills the command past `timeout_secs` (default 30) and reports
  `timed_out`.
