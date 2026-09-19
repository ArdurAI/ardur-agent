# ardur-provider-hermes

An Ardur [`Provider`] backend that drives the locally-installed **Hermes Agent**
CLI as a one-shot subprocess
(`hermes chat --oneshot --query-file - --format stream-json`).

This is a *wrap*, not a port: Hermes runs as a subprocess and keeps its own
model credentials, while Ardur keeps governance. Because the crate implements the
same `Provider` trait as the HTTP backends, a wrapped Hermes turn runs behind
the full fused pipeline — cap-token verification, Cedar authorization, the cost
gate, a signed receipt, and the durable journal.

## Upstream attribution

Hermes Agent is MIT-licensed and is **not** vendored here — this crate spawns the
binary and speaks its published CLI protocol. No upstream source is copied, so no
upstream license text is embedded; the credit below is the attribution that
applies.

> **Hermes Agent** — MIT License.
> Copyright (c) Nous Research.
> <https://github.com/NousResearch/hermes-agent>

The one-shot / stream-json vocabulary this crate speaks (`hermes chat --oneshot`,
`--query-file -`, `--format stream-json`, the terminal `result` event with
`text` / `tokens` / `exit_code`) is Hermes Agent's published interface, observed
against the Hermes Agent CLI reference docs.

## Requirements

- `hermes` on `PATH` (or `HERMES_BINARY` pointed at it).
- A model provider configured inside Hermes (`hermes auth` / `hermes model`).
  This crate holds no API key of its own.

## Configuration

| Env var | Meaning | Default |
| --- | --- | --- |
| `HERMES_BINARY` | Binary to spawn | `hermes` (via `PATH`) |
| `HERMES_PROVIDER` | Hermes' own provider name (`--provider`) | Hermes' default |
| `HERMES_DEFAULT_MODEL` | Model when the request names none | Hermes' default |
| `HERMES_WORKING_DIR` | Child working directory | inherited |
| `HERMES_TIMEOUT_SECS` | Wall-clock ceiling for one turn | `300` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

## Protocol

One Ardur turn maps onto one child process:

1. Spawn `hermes chat --oneshot --query-file - --format stream-json` (plus
   `--toolsets ""` unless child tools are explicitly allowed).
2. Write the flattened prompt transcript to stdin and close it.
3. Drain stdout JSONL until EOF; take the terminal `{"type":"result", …}`
   event's `text` / `tokens` / `exit_code`.
4. Fail closed on non-zero exit, in-band `error`, or empty assistant text.

## Security posture

- **Child tools are off by default.** The child is spawned with
  `--toolsets ""` (Hermes' explicit deny-all) unless
  `HermesConfig::allow_child_tools` is set. As a completion backend we want the
  model's answer, not an agent mutating the filesystem outside Ardur's grant
  ledger — the fused pipeline cannot see steps taken inside another process.
  `--yolo` is never passed.
- **Fail-closed on an incomplete turn.** A child that dies with a non-zero
  status, reports `result.error`, or returns empty text is an error — never a
  silent empty success.
- **Bounded everything.** The turn has a wall-clock timeout, the child is
  `kill_on_drop` (no orphan holding a model session), and stderr is drained
  concurrently into a capped sink (a full pipe cannot deadlock the turn).
- **Typed auth failures.** A login/credential failure maps to
  `ProviderError::Unauthorized`, not a generic upstream error. Rate-limit and
  quota diagnostics that merely *mention* a key are deliberately excluded.

## Billing

Turns are paid by Hermes' own configured provider, so the rate card is zeroed
(`hermes-delegated-v1`) and every completion is priced at zero cents. Token
counts come from the `result.tokens` object when present; otherwise they stay
zero rather than being invented.

## Deferred registration

Wiring `ProviderKind::Hermes` / `ARDUR_PROVIDER=hermes` into
`provider-selector::from_env` is **deferred** to the D0 model-router lane
(gh#411 / gh#531). This crate ships standalone so it does not fight that
branch's edits to `provider-selector`, `provider-runtime` router, or
`crates/config`.

## Not in this phase

- **Streaming** (`supports_streaming()` is `false`). The stream-json feed is
  already line-oriented, so this is a later crate-local change.
- **Tool-call surfacing.** Hermes orchestrates tools in its own process; this
  layer never returns `FinishReason::ToolUse`. Governing a child's own tool
  steps is delegation work, not provider work.

## Tests

```sh
cargo test -p ardur-provider-hermes
```

Unit tests plus subprocess tests driven against an executable shim that speaks
the real stream-json protocol — no hermes install and no model spend required.

A live round-trip against the real binary is available but `#[ignore]`d so CI
never spends money:

```sh
HERMES_LIVE_TEST=1 cargo test -p ardur-provider-hermes --test live -- --ignored --nocapture
```
