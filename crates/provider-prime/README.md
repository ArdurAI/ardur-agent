# ardur-provider-prime

An Ardur [`Provider`] backend that drives the locally-installed **Prime Agent**
CLI over its line-delimited JSON RPC mode (`prime-agent --mode rpc`).

This is a *wrap*, not a port: prime-agent runs as a subprocess and keeps its own
model credentials, while Ardur keeps governance. Because the crate implements the
same `Provider` trait as the HTTP backends, a wrapped Prime Agent turn runs behind
the full fused pipeline — cap-token verification, Cedar authorization, the cost
gate, a signed receipt, and the durable journal.

## Upstream attribution

Prime Agent is MIT-licensed and is **not** vendored here — this crate spawns the
binary and speaks its protocol. No upstream source is copied, so no upstream
license text is embedded; the credit below is the attribution that applies.

> **Prime Agent** — MIT License.
> Copyright (c) 2025 Mario Zechner.
> Copyright (c) 2026 Prime Intellect.
> <https://github.com/PrimeIntellect-ai/prime-agent>

The RPC command/event vocabulary this crate speaks (`prompt`,
`get_last_assistant_text`, the `response` envelope, and the `agent_start` →
`agent_end` event sequence) is Prime Agent's published interface, observed
against **prime-agent 0.9.5**.

## Requirements

- `prime-agent` on `PATH` (or `PRIME_AGENT_BINARY` pointed at it).
- A model provider configured inside prime-agent (`prime-agent /login`, or a
  provider entry such as `kimi-coding`). This crate holds no API key of its own.

## Configuration

| Env var | Meaning | Default |
| --- | --- | --- |
| `PRIME_AGENT_BINARY` | Binary to spawn | `prime-agent` (via `PATH`) |
| `PRIME_AGENT_PROVIDER` | prime-agent's own provider name (`--provider`) | prime-agent's default |
| `PRIME_AGENT_DEFAULT_MODEL` | Model when the request names none | prime-agent's default |
| `PRIME_AGENT_WORKING_DIR` | Child working directory (`--cwd`) | inherited |
| `PRIME_AGENT_TIMEOUT_SECS` | Wall-clock ceiling for one turn | `300` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

## Protocol

One Ardur turn maps onto one child process and two RPC commands:

1. `{"type":"prompt","message":<transcript>,"id":"ardur_prompt"}` — acknowledged
   with a `response` line, then streamed as events until `agent_end`.
2. `{"type":"get_last_assistant_text","id":"ardur_text"}` — returns the final
   assistant text in `data.text`.

Reading the final text with an explicit command, rather than reassembling
`message_update` deltas, keeps this crate independent of prime-agent's internal
streaming-chunk shape.

## Security posture

- **Child tools are off by default.** The child is spawned with `--no-tools`
  unless `PrimeConfig::allow_child_tools` is set. As a completion backend we want
  the model's answer, not an agent mutating the filesystem outside Ardur's grant
  ledger — the fused pipeline cannot see steps taken inside another process.
- **Fail-closed on an incomplete turn.** A child that dies before `agent_end`, or
  returns empty text, is an error — never a silent empty success.
- **Bounded everything.** The turn has a wall-clock timeout, the child is
  `kill_on_drop` (no orphan holding a model session), stderr is drained
  concurrently into a capped sink (a full pipe cannot deadlock the turn), and the
  retained event body is capped.
- **Typed auth failures.** A login/credential failure maps to
  `ProviderError::Unauthorized`, not a generic upstream error, so callers can
  distinguish "fix your credentials" from "the model failed".

## Billing

Turns are paid by prime-agent's own configured provider, so the rate card is
zeroed (`prime-delegated-v1`) and every completion is priced at zero cents. Token
counts reported by the child are passed through to `Usage`; when the child reports
none, the counts stay zero rather than being invented.

## Not in this phase

- **Streaming** (`supports_streaming()` is `false`). The event stream is already
  line-oriented, so this is a later crate-local change.
- **Tool-call surfacing.** prime-agent orchestrates tools in its own process;
  this layer never returns `FinishReason::ToolUse`. Governing a child's own tool
  steps is delegation work, not provider work.

## Tests

`cargo test -p ardur-provider-prime` runs 8 unit tests plus 8 RPC tests driven
against an executable shim that speaks the real protocol — no prime-agent install
and no model spend required. `vacuity_proof.sh` mutates each protected behavior in
turn and asserts the matching guard actually fails, so the suite cannot go
vacuous.
