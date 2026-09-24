# ardur-provider-codex

An Ardur [`Provider`] backend that drives the locally-installed **Codex CLI**
(`codex`, published by OpenAI) as a one-shot subprocess
(`codex exec --json --skip-git-repo-check --ephemeral --color never -s read-only`).

This is a *wrap*, not a port: Codex runs as a subprocess and keeps its own
session credentials (`codex login`, `~/.codex`), while Ardur keeps governance.
Because the crate implements the same `Provider` trait as the HTTP backends,
a wrapped Codex turn runs behind the full fused pipeline — cap-token
verification, Cedar authorization, the cost gate, a signed receipt, and the
durable journal.

## Upstream attribution

Codex CLI is published by OpenAI and is **not** vendored here — this crate
spawns the binary and speaks its published CLI interface. No upstream source
is copied, so no upstream license text is embedded; the credit below is the
attribution that applies.

> **Codex CLI** — OpenAI.
> <https://github.com/openai/codex>

The flag surface this crate speaks (`exec`, `--json`, `--ephemeral`,
`--skip-git-repo-check`, `--color never`, `-s/--sandbox`, `-C/--cd`,
`-m/--model`, stdin-as-prompt) is verified against the installed codex-cli
0.154.0 `exec --help`; the JSONL event vocabulary (`item.completed` /
`turn.completed` / `turn.failed`, `agent_message` items, `usage` token
counts) was pinned by the Phase-1 wrap's live probes (#67, #82).

## Requirements

- `codex` on `PATH` (or `CODEX_BINARY` pointed at it).
- An active Codex session (`codex login`; auth lives in `~/.codex`). This
  crate holds no API key of its own.

## Configuration

| Env var | Meaning | Default |
| --- | --- | --- |
| `CODEX_BINARY` | Binary to spawn | `codex` (via `PATH`) |
| `CODEX_DEFAULT_MODEL` | Model when the request names none (its `-m` value) | codex's own default |
| `CODEX_SANDBOX_MODE` | `read-only` \| `workspace-write` \| `danger-full-access` | `read-only` |
| `CODEX_WORKING_DIR` | Child working directory (`-C`) | inherited |
| `CODEX_TIMEOUT_SECS` | Wall-clock ceiling for one turn | `300` |
| `CODEX_MAX_TOKENS_FLOOR` | Smallest per-request `max_tokens` accepted | `4096` |

An unparseable or zero timeout keeps the default rather than producing a turn
that can never finish.

### Why there is a max-tokens floor

`codex exec` exposes no per-request output ceiling (its `-c` config overrides
do not include a per-completion cap). A caller's `max_tokens` therefore
cannot be enforced by this backend. Silently discarding it would let the
runtime authorize N output tokens, be billed for more, and still see a clean
`FinishReason::Stop` — so a ceiling below the floor is **refused** with
`InvalidRequest` instead. Above the floor (or `max_tokens == 0`), enforcement
is knowingly delegated to codex's own limits. Set `CODEX_MAX_TOKENS_FLOOR=0`
to accept every ceiling.

## Protocol

One Ardur turn maps onto one child process:

1. Spawn `codex exec --json --skip-git-repo-check --ephemeral --color never
   -s <sandbox>` (plus `-C <dir>` / `-m <model>` when configured).
2. Write the flattened prompt transcript to stdin and close it — with no
   prompt argument, `exec` reads the piped stdin text as the instructions.
3. Drain stdout JSONL until EOF: the **last**
   `{"type":"item.completed","item":{"type":"agent_message",…}}` event's text
   is the answer; `turn.completed.usage` carries the input/output token
   counts onto the turn's receipt. Other events are retained for the audit
   body only. Non-JSON lines are skipped (kept, capped, for classification).
   With no parseable events, ANSI-stripped raw stdout is the plain-text
   fallback.
4. Fail closed on non-zero exit, a `turn.failed`/`error` event, or empty
   final text. Classification scans stderr and stdout noise: auth →
   `Unauthorized`, rate-limit/quota → `RateLimited`, else redacted
   `Upstream`.

## Security posture

- **Deny-by-default sandbox.** Model-generated shell commands inside the
  child run under codex's own sandbox, and the default is the most
  restrictive `-s read-only`: the child may read its working directory but
  not write files or run mutating commands. As a completion backend we want
  the model's answer, not an agent mutating the filesystem outside Ardur's
  grant ledger — the fused pipeline cannot see steps taken inside another
  process. `CODEX_SANDBOX_MODE=workspace-write` (or `danger-full-access`,
  only in externally-sandboxed environments) is an explicit operator opt-in.
- **Fail-closed on an incomplete turn.** A child that dies with a non-zero
  status, emits a `turn.failed`/`error` event, or returns no text is an
  error — never a silent empty success.
- **Bounded everything.** The turn has a wall-clock timeout, the child is
  `kill_on_drop` (no orphan holding a model session), stderr is drained
  concurrently into a capped sink (a full pipe cannot deadlock the turn),
  and captured stdout / retained events / noise are capped.
- **Redacted diagnostics.** Child stderr and stdout error lines pass through
  the session-journals secret patterns before entering a `ProviderError`, so
  an echoed API key cannot surface in Ardur logs or receipts.
- **Typed auth failures.** A login/credential failure maps to
  `ProviderError::Unauthorized`, not a generic upstream error. Rate-limit and
  quota diagnostics that merely *mention* a key are deliberately excluded.

## Billing

Turns are paid by the user's ChatGPT subscription (`codex login`), so the
rate card is zeroed (`codex-subscription-v1`) and every completion is priced
at zero cents. Token counts reported by `turn.completed.usage` flow onto the
turn's receipt.

## Selector registration

Selected at boot via `ARDUR_PROVIDER=codex` (alias `codex-agent`) in
`provider-selector::from_env` → `CodexProvider::from_env`. Also available as
a router lane backend (`backend = "codex"`).
