# ardur-provider-kimi

An Ardur [`Provider`] backend that drives the locally-installed **Kimi Code**
CLI as a one-shot subprocess (`kimi --print --output-format stream-json`).

This is a *wrap*, not a port: Kimi Code runs as a subprocess and keeps its own
account credentials (`~/.kimi`), while Ardur keeps governance. Because the
crate implements the same `Provider` trait as the HTTP backends, a wrapped
Kimi Code turn runs behind the full fused pipeline — cap-token verification,
Cedar authorization, the cost gate, a signed receipt, and the durable journal.

## Upstream attribution

Kimi Code CLI is published by Moonshot AI under the Apache License 2.0 and is
**not** vendored here — this crate spawns the binary and speaks its published
CLI interface. No upstream source is copied, so no upstream license text is
embedded; the credit below is the attribution that applies.

> **Kimi Code CLI** — Apache License 2.0.
> Copyright (c) 2025 Moonshot AI.
> <https://github.com/MoonshotAI/kimi-cli>

The one-shot / JSON vocabulary this crate speaks (`--print`,
`--output-format stream-json`, the piped-stdin prompt, the
`{"role":"assistant","content":[…]}` message objects, the `--agent-file`
specification with `extend` / `tools` resolution) is Kimi Code's own
interface, verified against the installed kimi-cli 1.49.0 sources
(`cli/__init__.py`, `ui/print/__init__.py`, `ui/print/visualize.py`,
`agentspec.py`, kosong `message.py`) and a live probe of the real binary.

## Requirements

- `kimi` on `PATH` (or `KIMI_BINARY` pointed at it).
- An account configured inside Kimi Code (`kimi login`; auth lives in
  `~/.kimi`). Subscription auth is **kimi-cli managed** — kimi-cli's own
  config names the provider `managed:kimi-code` — so this crate holds no
  API key of its own and there is no Ardur-side credential to configure,
  rotate, or leak.

### When the login is missing or expired

Auth failures (child exit with `Error code: 401` / `unauthorized` /
`not logged in` / `kimi login` hints on stdout or stderr) surface as
`ProviderError::Upstream` whose message **starts with the stable marker**
`kimi auth failed:` followed by operator guidance and the sanitized
diagnostic, e.g.

```text
kimi auth failed: the kimi CLI's own subscription login (managed:kimi-code) is missing or expired; run `kimi login` (or inspect ~/.kimi) on this host — this crate holds no API key, so there is no Ardur-side credential to rotate; upstream said: Error code: 401 - {'error': {'type': 'invalid_authentication_error'}}
```

(The message is one line; wrapped here only where terminals do.)

A smoke run matches the marker (`ardur_provider_kimi::AUTH_FAILURE_MARKER`)
to tell "run `kimi login`" apart from every other upstream error, without
pattern-matching on vendor text and without any secret material in the
message. This is deliberately **not** `ProviderError::Unauthorized`: that
variant means "the provider rejected the credential Ardur presented" and the
model router answers it by advancing to the next pooled credential —
meaningless here, because the credential lives in the child CLI's own login.
Both variants are fail-closed; the difference is only who can act on it.

## Configuration

| Env var | Meaning | Default |
| --- | --- | --- |
| `KIMI_BINARY` | Binary to spawn | `kimi` (via `PATH`) |
| `KIMI_DEFAULT_MODEL` | Model when the request names none (its `-m` value) | Kimi Code's default |
| `KIMI_WORKING_DIR` | Child working directory | inherited |
| `KIMI_TIMEOUT_SECS` | Wall-clock ceiling for one turn | `300` |
| `KIMI_MAX_TOKENS_FLOOR` | Smallest per-request `max_tokens` accepted | `4096` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

### Why there is a max-tokens floor

Kimi Code has no per-completion output cap (`kimi --print` has no per-request
token ceiling). A caller's `max_tokens` therefore cannot be enforced by this
backend. Silently discarding it would let the runtime authorize N output
tokens, be billed for more, and still see a clean `FinishReason::Stop` — so a
ceiling below the floor is **refused** with `InvalidRequest` instead. Above
the floor (or `max_tokens == 0`), enforcement is knowingly delegated to Kimi
Code's own limits. Set `KIMI_MAX_TOKENS_FLOOR=0` to accept every ceiling.

## Protocol

One Ardur turn maps onto one child process:

1. Spawn `kimi --print --output-format stream-json` (plus `-m <model>` when a
   model is chosen, plus `--agent-file <staged>` and
   `--mcp-config-file <staged>` when child tools are denied — see below).
2. Write the flattened prompt transcript to stdin and close it — with no
   `--prompt` argument and a piped stdin, print mode reads the whole of stdin
   as the one-shot command and exits after the turn.
3. Drain stdout JSONL until EOF: the last `{"role":"assistant", …}` message's
   text parts (`type:"text"`; `think` parts skipped) are the answer. Tool
   results, plans, and notifications ride the same stream and are retained
   for the audit body only. The stream carries **no token usage**, so `Usage`
   stays zero rather than being invented.
4. Fail closed on non-zero exit, empty assistant text, or an unstagable deny
   spec. Print mode reports failures as a plain-text line on **stdout**
   (e.g. `Error code: 401 - {...}`) with exit `1`, or `75` (`EX_TEMPFAIL`)
   for retryable provider conditions (connection/timeout/429/5xx); both
   stdout and stderr are scanned for classification.

## Security posture

- **Child tools are off by default — agent spec AND MCP config.** The child
  is spawned with a staged `--agent-file` that resolves to the builtin
  default agent with an **empty tool list** (`extend: default`,
  `tools: []`, `subagents: {}`). The resolved agent has no tool definitions
  at all, so no tool call can be emitted — stronger than an approval deny,
  and independent of `--print`'s auto-approval of tool calls (which this
  crate never relies on). MCP tools need a second lock: kimi-cli's
  `load_agent` registers MCP tools **independently of the agent spec's tool
  list**, and print mode falls back to the operator's global
  `~/.kimi/mcp.json` when no config file is passed — so the child also gets
  `--mcp-config-file` pointing at a staged **empty server set**
  (`{"mcpServers":{}}`), which suppresses that fallback. Without it, an
  MCP server configured in the operator's global config would run
  ungoverned under `--print` auto-approval (this is the tightened gap from
  issue #572). Known residual surface, documented rather than mitigated:
  tools installed as kimi-cli *plugins* (`~/.kimi/plugins/`) also bypass
  the agent spec's tool list and cannot be suppressed by any CLI flag — an
  operator who installs Kimi Code plugins accepts that a completion turn
  can call them. As a completion backend we want the model's answer, not an
  agent mutating the filesystem outside Ardur's grant ledger — the fused
  pipeline cannot see steps taken inside another process. Setting
  `KimiConfig::allow_child_tools` omits `--agent-file` **and**
  `--mcp-config-file` and hands the child the operator's own configured
  default agent and MCP set. If either staged file cannot be written, the
  turn fails **before spawn** — the provider never falls back to the
  tool-enabled default agent or the global MCP config.
- **Fail-closed on an incomplete turn.** A child that dies with a non-zero
  status or returns empty text is an error — never a silent empty success.
- **Bounded everything.** The turn has a wall-clock timeout, the child is
  `kill_on_drop` (no orphan holding a model session), stderr is drained
  concurrently into a capped sink (a full pipe cannot deadlock the turn), and
  retained stdout/noise bytes are capped.
- **Redacted diagnostics.** Child stderr and stdout error lines pass through
  the session-journals secret patterns before entering a `ProviderError`, so
  an echoed API key cannot surface in Ardur logs or receipts.
- **Typed auth failures.** A login/credential failure maps to
  `ProviderError::Upstream` prefixed with the stable
  `AUTH_FAILURE_MARKER` (`"kimi auth failed"`) plus operator guidance
  (`kimi login` / `~/.kimi`, no Ardur-side credential) — deliberately not
  `ProviderError::Unauthorized`, which the router would answer with a
  pointless credential-pool failover (see *Requirements* above). The
  diagnostic text carried after the guidance is secret-redacted.
  Rate-limit and quota diagnostics that merely *mention* a key are
  deliberately excluded and map to `RateLimited`. Exit `75` maps to the
  transient slot (`NetworkFailure`, or `RateLimited` when the diagnostic
  names a quota).

## Billing

Turns are paid by Kimi Code's own configured account, so the rate card is
zeroed (`kimi-delegated-v1`) and every completion is priced at zero cents.
The stream-json vocabulary reports no token counts, so receipt counts stay
zero rather than being invented.

## Selector registration

Selected at boot via `ARDUR_PROVIDER=kimi` (alias `kimi-agent`) in
`provider-selector::from_env` → `KimiProvider::from_env`. Also available as a
router lane backend (`backend = "kimi"`).

## Not in this phase

- **Streaming** (`supports_streaming()` is `false`). The JSONL feed is
  already line-oriented, so this is a later crate-local change.
- **Tool-call surfacing.** Kimi Code orchestrates tools in its own process;
  this layer never returns `FinishReason::ToolUse`. Governing a child's own
  tool steps is delegation work, not provider work.

## Tests

```sh
cargo test -p ardur-provider-kimi
```

Unit tests plus subprocess tests driven against an executable shim that speaks
the real `--print --output-format stream-json` protocol — no kimi install and
no model spend required.

A live round-trip against the real binary is available but `#[ignore]`d so CI
never spends money:

```sh
KIMI_LIVE_TEST=1 cargo test -p ardur-provider-kimi --test live -- --ignored --nocapture
```

[`Provider`]: https://docs.rs/ardur-provider-runtime
