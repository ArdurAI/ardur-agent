# ardur-provider-opencode

An Ardur [`Provider`] backend that drives the locally-installed **OpenCode**
CLI as a one-shot subprocess (`opencode run --format json`).

This is a *wrap*, not a port: OpenCode runs as a subprocess and keeps its own
model credentials, while Ardur keeps governance. Because the crate implements
the same `Provider` trait as the HTTP backends, a wrapped OpenCode turn runs
behind the full fused pipeline — cap-token verification, Cedar authorization,
the cost gate, a signed receipt, and the durable journal.

## Upstream attribution

OpenCode is MIT-licensed and is **not** vendored here — this crate spawns the
binary and speaks its published CLI protocol. No upstream source is copied, so
no upstream license text is embedded; the credit below is the attribution that
applies.

> **OpenCode** — MIT License.
> Copyright (c) SST.
> <https://github.com/sst/opencode>

The one-shot / JSONL vocabulary this crate speaks (`opencode run --format json`,
the piped-stdin prompt, the `text` / `step_finish` / `error` event objects) is
OpenCode's published interface, verified against `packages/opencode/src/cli/cmd/run.ts`
(v1.0.165 and master).

## Requirements

- `opencode` on `PATH` (or `OPENCODE_BINARY` pointed at it). Install it with
  one of:

  ```sh
  curl -fsSL https://opencode.ai/install | bash   # official installer
  npm i -g opencode-ai                            # npm / bun / pnpm
  brew install sst/tap/opencode                   # macOS Homebrew
  ```

  Verify with `opencode --version`. A missing binary fails the turn with a
  typed `ProviderError::Upstream` ("OpenCode CLI not installed …") carrying
  this install line, so smoke can tell "not installed" apart from auth and
  rate-limit failures.
- A model provider configured inside OpenCode (`opencode auth login`).
  This crate holds no API key of its own.

## Configuration

| Env var | Meaning | Default |
| --- | --- | --- |
| `OPENCODE_BINARY` | Binary to spawn | `opencode` (via `PATH`) |
| `OPENCODE_DEFAULT_MODEL` | Model when the request names none (`provider/model`) | OpenCode's default |
| `OPENCODE_WORKING_DIR` | Child working directory | inherited |
| `OPENCODE_TIMEOUT_SECS` | Wall-clock ceiling for one turn | `300` |
| `OPENCODE_MAX_TOKENS_FLOOR` | Smallest per-request `max_tokens` accepted | `4096` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

### Why there is a max-tokens floor

OpenCode has no per-completion output cap (`opencode run` has no per-request
token ceiling). A caller's `max_tokens` therefore cannot be enforced by this
backend. Silently discarding it would let the runtime authorize N output
tokens, be billed for more, and still see a clean `FinishReason::Stop` — so a
ceiling below the floor is **refused** with `InvalidRequest` instead. Above the
floor (or `max_tokens == 0`), enforcement is knowingly delegated to OpenCode's
own limits. Set `OPENCODE_MAX_TOKENS_FLOOR=0` to accept every ceiling.

## Protocol

One Ardur turn maps onto one child process:

1. Spawn `opencode run --format json` (plus `--model provider/model` when a
   model is chosen) with `OPENCODE_CONFIG_CONTENT` set (see below).
2. Write the flattened prompt transcript to stdin and close it — with no
   message arguments, `run` reads the piped stdin text as the prompt.
3. Drain stdout JSONL until EOF: the last `{"type":"text", …}` event's
   `part.text` is the answer; `{"type":"step_finish", …}` events carry
   per-model-call `part.tokens` (`input` / `output`), summed across the turn.
4. Fail closed on non-zero exit, any `{"type":"error", …}` event, or empty
   assistant text.

## Security posture

- **Child tools are off by default.** The child is spawned with
  `OPENCODE_CONFIG_CONTENT={"permission":"deny","share":"disabled"}`. Inline
  config outranks OpenCode's global, custom, and project config (only
  admin-managed config beats it), and explicit `deny` rules are enforced even
  in `--auto` mode — which this crate never passes. As a completion backend we
  want the model's answer, not an agent mutating the filesystem outside
  Ardur's grant ledger — the fused pipeline cannot see steps taken inside
  another process. Setting `OpenCodeConfig::allow_child_tools` hands the child
  the operator's own configured permissions (`{"share":"disabled"}` stays
  pinned either way, so a turn's transcript cannot leak to a share URL).
- **Fail-closed on an incomplete turn.** A child that dies with a non-zero
  status, emits an `error` event, or returns empty text is an error — never a
  silent empty success.
- **Bounded everything.** The turn has a wall-clock timeout, the child is
  `kill_on_drop` (no orphan holding a model session), and stderr is drained
  concurrently into a capped sink (a full pipe cannot deadlock the turn).
- **Redacted diagnostics.** Child stderr and `error` events pass through the
  session-journals secret patterns before entering a `ProviderError`, so an
  echoed API key cannot surface in Ardur logs or receipts.
- **Typed auth failures.** A login/credential failure maps to
  `ProviderError::Unauthorized`, not a generic upstream error. Rate-limit and
  quota diagnostics that merely *mention* a key are deliberately excluded.

## Billing

Turns are paid by OpenCode's own configured provider, so the rate card is
zeroed (`opencode-delegated-v1`) and every completion is priced at zero cents.
Token counts come from the `step_finish` events' `tokens` object summed across
the turn; when the child reports none, the counts stay zero rather than being
invented.

## Selector registration

Selected at boot via `ARDUR_PROVIDER=opencode` (alias `opencode-agent`) in
`provider-selector::from_env` → `OpenCodeProvider::from_env`. Also available as
a router lane backend (`backend = "opencode"`).

## Not in this phase

- **Streaming** (`supports_streaming()` is `false`). The JSONL feed is already
  line-oriented, so this is a later crate-local change.
- **Tool-call surfacing.** OpenCode orchestrates tools in its own process;
  this layer never returns `FinishReason::ToolUse`. Governing a child's own
  tool steps is delegation work, not provider work.

## Tests

```sh
cargo test -p ardur-provider-opencode
```

Unit tests plus subprocess tests driven against an executable shim that speaks
the real `run --format json` protocol — no opencode install and no model spend
required.

A live round-trip against the real binary is available but `#[ignore]`d so CI
never spends money:

```sh
OPENCODE_LIVE_TEST=1 cargo test -p ardur-provider-opencode --test live -- --ignored --nocapture
```
