# ardur-provider-claude-cli (§3.3c)

A [`Provider`](../provider-runtime) backend that wraps the locally-installed
[`claude` (Claude Code)](https://claude.com/code) CLI as a **subprocess**,
running it non-interactively via `claude -p --output-format json`.

Like the §3.3b [Codex backend](../provider-codex), it does **not** call a REST
endpoint with an API key. Authentication is inherited from `claude login`, so a
dogfooding turn spends the user's **Anthropic subscription** rather than a
metered API key. Every completion is therefore priced at **zero cents**.

## Why

It lets you dogfood ardur-agent against Claude using the subscription you
already pay for — no separate `ANTHROPIC_API_KEY`, no per-token metering in the
ardur cost ledger. One `claude login` and the same fused substrate
(cap-token → cedar → cost-gate → provider → receipt) runs against Claude.

## ⚠️ Billing is **not** unbounded — the Agent SDK Credit pool

As of **2026-06-15**, Anthropic moved Agent SDK + `claude -p` (headless) usage
onto a separate **Agent SDK Credit pool**, distinct from interactive Claude Code
usage. It is a **fixed $20–$200/month allotment** (depending on your plan),
**billed at API rates**, and it does **not** draw from the interactive
five-hour/weekly limits.

What this means for this provider:

- Subscription-via-CLI is **capped**, not free-flowing. A heavy dogfooding loop
  can exhaust the monthly credit pool.
- When the pool is exhausted, the CLI fails and this backend maps that onto
  [`ProviderError::RateLimited`](../provider-runtime) (see the table below).
- The ardur cost ledger still records **zero cents** for these calls (the spend
  is out-of-band, against the credit pool), so monitor the pool in your
  Anthropic account — ardur's budget gate will not see it.

## Prerequisites

1. Install the Claude Code CLI (see <https://claude.com/code>). This crate was
   developed against `claude` **v2.1.162**.
2. Run `claude login` once. Auth is then inherited by every subprocess this
   backend spawns — there is no API key in this crate's config.

If the `claude` binary is absent, the provider crate still builds and its tests
pass (they use a mocked subprocess shim); the binary is only required at turn
time.

## Usage

```rust
use std::sync::Arc;
use ardur_provider_claude_cli::{ClaudeCliConfig, ClaudeCliProvider, PermissionMode};
use ardur_provider_runtime::{ModelId, Provider, ProviderRegistry};

// Resolve `claude` through PATH, default model + most-restrictive permission:
let provider = ClaudeCliProvider::new(ClaudeCliConfig::new(), ModelId::new("sonnet"));

// …or tune the invocation:
let provider = ClaudeCliProvider::new(
    ClaudeCliConfig::new()
        .claude_binary("/opt/homebrew/bin/claude")
        .default_model("claude-opus-4-8")
        .working_directory("/tmp/ardur-claude")
        .permission_mode(PermissionMode::Default),
    ModelId::new("sonnet"),
);

// …or read the configuration from the environment (see below):
let provider = ClaudeCliProvider::from_env(ModelId::new("sonnet"));

// It plugs into the generic registry like any other provider:
let mut registry = ProviderRegistry::new();
registry.register(Arc::new(provider)); // registered under id "claude-cli"
```

The model on each `CompletionRequest` selects which model the CLI runs (`--model`);
the default model passed at construction is only the runtime's fallback.

## Configuration from the environment

`ClaudeCliConfig::from_env()` / `ClaudeCliProvider::from_env()` read:

- `CLAUDE_CLI_BINARY` — path to the `claude` binary. Unset/empty ⇒ resolve
  `claude` on `PATH`.
- `CLAUDE_CLI_DEFAULT_MODEL` — model passed with `--model` when a request leaves
  it unset. Unset ⇒ the CLI's own default (a Sonnet 4.x).
- `CLAUDE_CLI_WORKING_DIR` — the cwd the CLI runs in (its skill/file context).
- `CLAUDE_CLI_ALLOWED_TOOLS` — passed to `--allowedTools` for permissive
  non-interactive runs (e.g. `"Bash(git *) Edit"`). Unset ⇒ flag omitted.
- `CLAUDE_CLI_PERMISSION_MODE` — `default` (the safe, never-prompting default) |
  `acceptEdits` | `auto` | `bypassPermissions` | `dontAsk` | `plan`.

`from_env()` never fails: there is no API key to be missing (auth is
`claude login`), so a missing login is only discovered when a turn runs.

## How a turn maps onto the CLI

The chat transcript is flattened into a single prompt (system text on top, then
`User:`/`Assistant:` turns) and piped on stdin to:

```
claude -p --output-format json --permission-mode <mode> [--model <m>] [--allowedTools <t>]
```

`--output-format json` writes a JSON value to stdout — in current CLI versions
an **array** of stream events ending in a `{"type":"result", …}` object (a
single `result` object is also accepted). The parser reads:

- `result.result` → the response content (falling back to the last `assistant`
  message's text if absent).
- `result.stop_reason` → the `FinishReason` (`end_turn`/`stop` → `Stop`,
  `max_tokens` → `MaxTokens`, an `is_error` result → `Error`).
- `result.usage.input_tokens` / `output_tokens` → the billed `Usage` (priced at
  `0` cents).

## Error mapping

The shared `ProviderError` enum has no `ConfigError`/`Timeout`/`InvalidResponse`
variants, so failures map onto the closest existing ones:

| Condition | `ProviderError` |
|---|---|
| binary not found on `PATH` | `Upstream("Claude CLI not installed … Install from claude.com/code.")` |
| not logged in (`claude login`) / invalid key | `Unauthorized` |
| Agent SDK Credit pool / usage limit exhausted | `RateLimited { retry_after_ms: 0 }` |
| run exceeded `request_timeout` (default 5 min) | `NetworkFailure` |
| non-zero exit + stderr | `Upstream("claude -p exited with status <code>: <stderr>")` |
| exit 0 but `is_error: true` result | classified from the result text (Unauthorized / RateLimited / Upstream) |
| success but unparseable / no `result` object | `Upstream` |

## Cost reporting

Claude CLI calls are paid by the Anthropic subscription's Agent SDK Credit pool,
not per token in ardur's ledger. Token counts come from the `result.usage`
fields and populate the billed `CostTuple`, but every call is priced at `0`
cents (the rate card is `claude-cli-subscription-v1`, all zeros). See the
billing note above.

## Not in Phase 1

- **Streaming** — `supports_streaming()` is `false`; the whole subprocess is
  awaited. Phase 2 is `--output-format stream-json`.
- **Tool-call parsing** — Claude Code runs tools inside its own session; only
  the final assistant text is surfaced, never a `FinishReason::ToolUse`.
- **Rich message handling** — the flattened transcript loses turn structure.
