# §6.1 / §6.2 — built-in tools

Capability-gated shell, filesystem, and HTTP-fetch tools that give a
freshly-booted ardur-agent useful reach **without** any external MCP server
configuration. Everything here is opt-in at registration: nothing is installed
unless the caller asks for it via [`BuiltinOpts`](./mod.rs).

| id           | tool            | capabilities                | gate                              |
| ------------ | --------------- | --------------------------- | --------------------------------- |
| `shell.run`  | `ShellTool`     | `ShellExec`, `ProcessSpawn` | allowlist (opt-in)                |
| `file.read`  | `ReadFileTool`  | `FsRead`                    | confined to a root                |
| `file.write` | `WriteFileTool` | `FsWrite`                   | confined to a root                |
| `file.list`  | `ListDirTool`   | `FsRead`                    | confined to a root                |
| `http.fetch` | `HttpFetchTool` | `NetworkOut`                | host allowlist + SSRF IP defence  |

## Wiring it up

```rust
use std::path::PathBuf;
use ardur_tool_registry::{BuiltinOpts, HttpFetchOpts, ToolRegistry};

let mut registry = ToolRegistry::new();
registry.register_builtins(BuiltinOpts {
    enable_shell: true,
    // Production: confine the shell to leaf commands you trust.
    shell_allowlist: Some(vec!["git status".into(), "ls|cat|echo".into()]),
    // File tools are confined to this root; omit to install none.
    file_root: Some(PathBuf::from("/srv/agent/workspace")),
    // HTTP fetch, confined to an explicit host allowlist; omit to install none.
    http: Some(HttpFetchOpts {
        enable: true,
        allowlist: vec!["api.github.com".into(), "*.example.com".into()],
        ..Default::default()
    }),
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
- `http.fetch` truncates the body at `max_bytes` (default 1 MiB) and reports
  `body_truncated`; it aborts a request past `timeout_secs` (default 30).

## `http.fetch` — §6.2

A side-effect-free HTTP(S) fetch that lets the agent read web pages without a
network MCP server. It accepts `{ url, method?, headers?, max_bytes?,
timeout_secs? }` and returns `{ status, headers, body, body_truncated,
bytes_read, final_url, elapsed_ms }`. A 4xx/5xx is a **successful** result with
that status and its body — the agent decides how to react; only a malformed
request, a refused destination, a timeout, or a transport failure is an error.

### Strict-by-default SSRF posture

A URL-controlled fetch is a classic [SSRF](https://owasp.org/www-community/attacks/Server_Side_Request_Forgery)
primitive — a malicious prompt could otherwise reach loopback admin panels, the
RFC 1918 LAN, or a cloud metadata endpoint (`169.254.169.254`). The tool is
locked down accordingly:

- **Methods.** Only `GET` and `HEAD`. `POST`/`PUT`/`DELETE`/etc. are refused
  with `ToolError::Denied`, so the tool can never mutate remote state.
- **Schemes.** Only `http` and `https`. `ftp://`, `file://`, etc. are denied.
  Relative URLs fail to parse and are rejected as `InvalidArgs`.
- **Localhost-only without an allowlist.** With no allowlist and no private-IP
  opt-in, the tool fetches **only localhost**. Every other host — public or
  private — is denied. This is the safe default for a freshly-registered tool.
- **Host allowlist.** Configure `allowlist` to widen it. Patterns:

  | pattern           | permits                          | rejects                |
  | ----------------- | -------------------------------- | ---------------------- |
  | `example.com`     | exactly `example.com`            | `evil.com`, `a.example.com` |
  | `*.example.com`   | `a.example.com`, `b.c.example.com` | the apex `example.com` |
  | `*`               | every host (**dev only**, logs a warning) | nothing       |

- **Private-IP defence (independent of the allowlist).** Every host — including
  every redirect hop — is resolved and each resulting IP is checked. Loopback
  (other than a deliberate localhost target), RFC 1918, IPv4 link-local
  (`169.254/16`), IPv6 link-local (`fe80::/10`), and unique-local (`fc00::/7`)
  addresses are refused unless `allow_private_ips` is set. IPv4-mapped IPv6
  (`::ffff:a.b.c.d`) is unwrapped first so it cannot smuggle a private address
  past the check. An allowlisted host that *resolves* to a private IP is still
  refused — the allowlist does not override this gate.
- **Pinned connections.** The vetted addresses are pinned into the request
  (reqwest `resolve_to_addrs`) so the socket lands on the exact IP that was
  checked, closing the DNS-rebind window between the check and the connect.
- **Manual redirect following.** The client's own redirect follower is disabled;
  redirects are followed by hand (default cap 5, `redirect_limit`) so the host
  allowlist and IP checks **re-run on every `Location`**. An allowlisted page
  that 302s to `169.254.169.254` is refused, not followed. Exceeding the cap is
  an error.

As with the shell and file tools this is a guard, not a sandbox. Pair it with
the §11 capability-token + Cedar layers when the prompt is untrusted — the
`NetworkOut` capability it declares is what those layers gate against.
