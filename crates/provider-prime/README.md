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
| `PRIME_AGENT_MAX_TOKENS_FLOOR` | Smallest per-request `max_tokens` accepted | `4096` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

### Why there is a max-tokens floor

prime-agent has no per-completion output cap (`--autonomous-max-tokens` bounds a
whole autonomous run, not one turn). A caller's `max_tokens` therefore cannot be
enforced by this backend. Silently discarding it would let the runtime authorize
N output tokens, be billed for more, and still see a clean `FinishReason::Stop`
— so a ceiling below the floor is **refused** with `InvalidRequest` instead.
Above the floor, enforcement is knowingly delegated to prime-agent's own limits.
Set the floor to `0` to accept every ceiling and delegate unconditionally.

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
  distinguish "fix your credentials" from "the model failed". Rate-limit and
  quota diagnostics that merely *mention* a key are deliberately excluded —
  classifying those as `Unauthorized` would turn a retryable failure into a
  permanent one.
- **A turn needs an acknowledged prompt.** `agent_end` alone does not complete a
  turn: without a successful `prompt` response the turn fails, so a child that
  ignored or rejected the prompt cannot look successful just because the session
  still holds text from an earlier turn.
- **Reads are bounded before buffering.** Both stdout and stderr are read
  through a byte budget rather than accumulating a whole line first, so a child
  emitting a newline-free flood is refused instead of exhausting memory.

## Billing

Turns are paid by prime-agent's own configured provider, so the rate card is
zeroed (`prime-delegated-v1`) and every completion is priced at zero cents. Token
counts reported by the child are **accumulated across every assistant message in
the turn** — with child tools enabled a single prompt can drive several model
calls, and keeping only the last record would under-report the run in the signed
receipt. When the child reports no usage at all, the counts stay zero rather than
being invented.

## Not in this phase

- **Streaming** (`supports_streaming()` is `false`). The event stream is already
  line-oriented, so this is a later crate-local change.
- **Tool-call surfacing.** prime-agent orchestrates tools in its own process;
  this layer never returns `FinishReason::ToolUse`. Governing a child's own tool
  steps is delegation work, not provider work.

## Tests

`cargo test -p ardur-provider-prime` runs 12 unit tests plus 14 RPC tests driven
against an executable shim that speaks the real protocol — no prime-agent install
and no model spend required. Every guard has been mutation-proven: reverting the
behavior it protects makes exactly that test fail, so the suite cannot go
vacuous.

A live round-trip against the real binary is available but `#[ignore]`d so CI
never spends money:

```sh
cargo test -p ardur-provider-prime --test live -- --ignored --nocapture
```
