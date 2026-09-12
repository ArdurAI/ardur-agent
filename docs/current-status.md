# Current Status and Ready Features

Implementation baseline reviewed: `dev` at
`09b40df5019aac77ca8fe32563b2fa8e2aeb2043` on 2026-09-10. This document is the
release checklist for `v0.1.0-beta.1`: every claim below is tied to that
reviewed code baseline and was re-verified against it.

## Repository and Verification Status

- GitHub PRs merged into `dev` for this baseline:
  - `#418` — first-run Cedar policy UX (closes `#408`).
  - `#419` — ARD-457 CLI grant consumption for hardened tools.
  - `#421` — commit gate for caller-abandoned turns (closes `#359`).
  - `#424` — incremental receipt verification (`#355`) and Qdrant snapshot
    timeout (`#371`).
  - `#423` — GHCR publish on `v*` tags plus the fresh-machine runbook.
  - `#425` — ARD-460 PWA streaming against fused SSE with fail-closed CORS.
- No GitHub PRs were open at this review.
- Required GitHub workflows were green before each merge, and the full
  dev-push check set (CodeQL, CodeQL/Rust, Trivy, build-healthcheck-scan,
  cargo-deny, hugo, macos-15/stable, ubuntu-latest/stable, qdrant integration
  `--ignored`) is green on the baseline commit: CI run
  [34328847792](https://github.com/ArdurAI/ardur-agent/actions/runs/34328847792),
  docker run
  [34328847810](https://github.com/ArdurAI/ardur-agent/actions/runs/34328847810),
  site-deploy run
  [34328847846](https://github.com/ArdurAI/ardur-agent/actions/runs/34328847846).
  DCO runs on PRs and was green before each merge.
- Container release path: `.github/workflows/docker.yml` publishes
  `ghcr.io/ardurai/ardur-agent:<tag>` on `v*` tags — the same image that
  passed the job's Trivy gate and `/healthz` smoke, with build-provenance
  attestation. It never tags `:latest`.
- Release supply-chain path: the `release-supply-chain` workflow
  (`.github/workflows/release.yml`, job `release-sbom-sign`) runs when a
  GitHub Release is published for the tag: it builds release binaries,
  generates an SPDX SBOM and SHA256SUMS, signs assets with keyless cosign,
  and attaches build provenance.
- Fresh-machine runbook: [docs/fresh-machine.md](fresh-machine.md) is the
  operator runbook for the offline stub, one live provider, a private Slack
  channel, and the published-container smoke.
- Local no-key baseline on this review:
  - `cargo test -p ardur-e2e-tests`
  - `cargo test -p ardur-server --test boot_smoke`
  - `cargo test -p ardur-cli --test cli_smoke_echo`
  - `cargo build --workspace --bins`
- Cargo metadata reports 58 workspace packages.

## Ready Without External Accounts

These paths are usable on a developer machine without model-provider keys.

| Area | What is ready | How to use it |
| --- | --- | --- |
| Offline fused chat | `ardur chat` falls back to a network-free stub when `ANTHROPIC_API_KEY` is absent, while still exercising the fused runtime, receipts, journals, memory, cost gate, cap-token path, and Cedar policy. First-run `ardur setup --yes` writes a scoped starter Cedar policy so a fresh install is not a silent deny-all. | `ardur setup --yes`, then `cargo run -p ardur-cli -- chat --plain` |
| Legacy echo chat | A minimal in-memory echo path with no provider, cost, or persistent state. | `cargo run -p ardur-cli -- chat --echo` |
| Local setup and diagnostics | Setup, redacted config, redacted logs, redacted state snapshot, doctor checks, and [session lifecycle commands](session-lifecycle.md) are present in the CLI. | `ardur setup --yes`, `ardur doctor`, `ardur config`, `ardur logs`, `ardur debug`, `ardur sessions ...` |
| E2E substrate tests | Stub-provider scenarios prove fused cap-token, Cedar, cost gate, provider, receipt, journal, memory, and `/chat` SSE paths without network calls. | `cargo test -p ardur-e2e-tests` |
| Skills | Filesystem `SKILL.md` loading with progressive disclosure is implemented. Example skills include code review, runbooks, postmortems, onboarding, and `cite-or-refuse`. | Set `ARDUR_SKILLS_DIRS=./examples/skills` |
| Admin UI binary | `ardur-admin` is a read-only dashboard over journals, receipts, costs, memory, and Trust Center APIs. Approval approve/reject actions proxy to ardur-server. | `cargo run -p ardur-admin -- --help` |
| Evaluation harness | `ardur-eval` can run scenario files against the server `/chat` contract and emit JSON, JUnit, or Markdown. It posts the consolidated (non-stream) body. | `cargo run -p ardur-eval -- --help` |
| Static PWA shell | `web-client/` is installable as a static PWA. It streams `POST /chat` `{ stream: true }`, renders only fused `content` frames, and calls `/approvals/{id}/approve|reject`. Cross-origin use needs `ARDUR_CORS_ORIGINS`. | `cd web-client && python3 -m http.server 4173` (see `web-client/README.md`) |

## Ready With Local Services or Credentials

These features are implemented but need a provider key, local daemon, channel
token, or explicit operator configuration.

| Area | What is ready | Required inputs |
| --- | --- | --- |
| Providers | Anthropic, OpenRouter, generic OpenAI-compatible endpoints, Ollama, Codex CLI, and Claude CLI are selectable through `ARDUR_PROVIDER`. Anthropic, OpenRouter, OpenAI-compatible, and Ollama expose provider-level streaming. | API keys, local Ollama daemon, or logged-in `codex` / `claude` CLIs depending on provider. |
| HTTP agent API | `ardur-server` exposes `POST /chat` (JSON or SSE), `POST /acp`, optional `POST /slack/events`, `GET /healthz`, `GET /health`, `GET /metrics`, `GET /admin/runtime`, `GET /approvals`, `POST /approvals/{id}/approve`, `POST /approvals/{id}/reject`, `GET /openapi.json`, and generated Rust/Python clients. HTTP-only boot (no Slack credentials) is supported. | Server environment, optional Slack credentials, provider selection, chat/admin bearer tokens where configured. |
| Chat channels | Slack is the primary channel; Matrix, Discord, and Telegram can be enabled alongside it. All route inbound messages through the same fused runtime. | Bot credentials and allowlists. Use private channels first. |
| Durable memory | `in_memory` is the default. `qdrant` persists bi-temporal memory. `hybrid` adds Qdrant dense search plus a file-backed Tantivy/BM25 sparse index fused by reciprocal-rank fusion. | `QDRANT_URL` for `qdrant` or `hybrid`; local embedder download on first hybrid boot. |
| Observability | Provider calls emit OpenTelemetry GenAI spans; `/health`, `/metrics`, and `/admin/runtime` expose operational posture with secret redaction. | `ARDUR_OTEL_ENABLED=true` and an OTLP endpoint for export. |
| MCP | Ardur can serve MCP over bearer-gated Streamable HTTP and consume remote MCP servers into the runtime tool registry. | `ARDUR_MCP_ENABLED=true`, `ARDUR_MCP_BEARER_TOKENS`, optional `ARDUR_MCP_REMOTE_SERVERS`. |
| Voice transcription | `voice.transcribe` is registered by the server when Whisper credentials are present. The provider validates size, duration, HTTPS base URLs except loopback test URLs, and records provider receipt hashes. | `OPENAI_WHISPER_API_KEY` or `OPENAI_API_KEY`. |
| Local voice providers | `ardur-media-audio` has command-backed local STT and TTS providers for on-device engines such as whisper.cpp, Vosk, Piper, or OS speech tools. They execute commands directly, not through a shell. | `ARDUR_LOCAL_STT_COMMAND` / `ARDUR_LOCAL_TTS_COMMAND`; integration into server default registry is not yet automatic. |
| Operator grants | `ardur grant allow` records a ledger consumed by CLI chat (and env opt-ins on the server) to register hardened `shell.run` / `file.*` / `http.fetch` tools with scoped allowlists. | `ardur grant allow <tool> …`; server: `ARDUR_ENABLE_SHELL_TOOL` + allowlist, `ARDUR_ENABLE_HTTP_TOOL` + allowlist, `ARDUR_FILE_TOOL_ROOT`. |

## Security and Trust Features Available Now

- Cap-token authorization with offline attenuation and revocation-oriented
  design.
- Cedar policy evaluation in the fused runtime. Fresh CLI installs get a
  scoped starter policy from `ardur setup`; production server boots without a
  policy path remain deny-all unless `ARDUR_DEV_PERMISSIVE_POLICY=true`.
- Cost-gate projection, ceiling checks, reservation, finalization, and refund.
- Prompt-injection defense before provider dispatch and on tool output.
- JWS ES256 receipt chain with parent-hash linkage, including approval-decision
  receipts on `/approvals/{id}/approve|reject`.
- Append-only JSONL session journals.
- Receipt-linked memory writes and append-only memory forget/tombstone behavior.
- Redacted operator surfaces for config, logs, debug output, metrics, and admin
  runtime inspection.
- Webhook signature replay protection and hardened SSRF / shell-denylist checks.
- Fail-closed CORS: empty `ARDUR_CORS_ORIGINS` emits no CORS headers; `*` is
  refused at config load.

## Tooling Status

The fused runtime tool loop is implemented: model-requested tool calls can loop
back into the provider, are bounded by iteration and timeout limits, pass through
cost/injection/receipt handling, and record tool evidence.

Default `ardur-server` boot currently registers:

- `echo`
- `health_check`
- `voice.transcribe` when Whisper credentials are present
- filesystem skills from `ARDUR_SKILLS_DIRS`
- remote MCP tools from `ARDUR_MCP_REMOTE_SERVERS`
- operator-granted hardened built-ins only when the matching env opt-ins are set

The hardened built-in tools are implemented and tested in `ardur-tool-registry`:

- `shell.run`
- `shell.exec`
- `file.read`
- `file.write`
- `file.list`
- `http.fetch`

They are capability-gated and include command allowlists, filesystem root
confinement, HTTP host allowlists, and SSRF defenses. They are **not**
default-on. CLI chat consumes `~/.ardur/grants.json` from `ardur grant`; the
server consumes `ARDUR_ENABLE_SHELL_TOOL` / `ARDUR_ENABLE_HTTP_TOOL` /
`ARDUR_FILE_TOOL_ROOT` (ARD-457).

The platform tool crates are also implemented as explicit integration surfaces:

- `ardur-browser`: `browser.navigate`, `browser.click`, `browser.type`,
  `browser.screenshot`, `browser.extract`
- `ardur-terminal`: `terminal.exec`, `terminal.session`
- `ardur-web`: `web.fetch`, `web.parse`, `web.screenshot`, `web.form_fill`

## Automation, Learning, and Grounding

- `ardur-automation::DefaultTaskFlowOrchestrator` is no longer a placeholder.
  It validates DAG shape, dispatch allowlists, depth/fanout, retries, and
  fail-closed control-flow constraints. Effectful external dispatch is still a
  later phase.
- `ardur-automation::proactive` implements a scheduled/triggered automation loop
  with durable schedule storage, attenuated cap-token requirements, per-fire
  budget provisioning, fused-runtime submission, and channel delivery sinks.
  This is a programmatic Rust surface, not a complete operator UI.
- `ardur-automation::learning` implements a receipt-chained proposal loop for
  self-improvement playbooks, gated by cap-token, Cedar, and human approval.
- The `cite-or-refuse` example skill provides a strict grounding policy: cite
  every corpus-dependent claim or refuse when retrieval is empty/unsupported.

## Streaming and approvals

- `POST /chat` with `stream: true` returns `text/event-stream` of fused-runtime
  events (`stage_start`/`stage_end`, `content`, tool events, `usage`, `receipt`,
  `finish`, in-band `error`). Dropping the body closes the receiver and the
  worker drops the fused stream, attempting cancellation before
  receipt/journal/memory side effects commit; a fast stream can still commit if
  frames are already buffered. Covered by `crates/server/tests/streaming.rs`
  and `crates/e2e-tests/tests/scenario_streaming_chat_sse.rs`.
- Non-stream turns abandoned by the HTTP timeout or a client hang-up hit the
  commit gate (`#359` / `#421`): a flag set synchronously in the dropping
  thread is consulted after each provider round and before the
  receipt/journal/billing commit, so an abandoned turn releases its cost
  reservation and mints no final receipt. Intermediate tool-loop receipts from
  earlier rounds can still survive such a cancel — see `#422` below. Covered
  by `crates/fused-runtime/tests/turn_cancellation.rs` and
  `crates/server/tests/chat_turn_timeout.rs`.
- `GET /approvals`, `POST /approvals/{id}/approve`, and
  `POST /approvals/{id}/reject` are admin-bearer gated, persist to the same
  on-disk store as `ardur approvals`, and mint decision receipts. This is the
  **decide-half** of ARD-463 / ARD-139. Nothing server-side currently *produces*
  pending cards (the propose-half remains a follow-up).

## Not Yet Turnkey

Do not treat this repo as a public production deployment without additional
hardening and operator work.

- Run live deployments in private channels first.
- Direct MCP exposes only capability-free tools until MCP requests can carry the
  same fused-runtime cap-token/Cedar context as normal tool calls.
- Hardened shell/file/http tools exist but stay off until an operator grant or
  server env opt-in (ARD-457).
- `shell.run`'s allowlist is a prefix gate, not full argv confinement. The
  hardened sibling `shell.exec` (`#420`) is implemented and tested — it execs
  argv directly with no shell and matches `argv[0]` exactly — but is not yet
  wired to a server config flag, so it is never registered at boot. Deployments
  that construct a registry directly can opt in via
  `BuiltinOpts::enable_shell_exec`.
- Tool-loop intermediate receipts survive a later-round cancel (`#422`): a turn
  cancelled mid-loop can leave earlier-round tool receipts in the chain without
  a final settled receipt, so the chain can record cost for a turn the caller
  never saw finish.
- Local STT/TTS providers exist in `ardur-media-audio`, but the server currently
  auto-registers Whisper transcription only.
- Approval *propose* (the agent creating a pending card before an irreversible
  tool) is not mounted; only decide endpoints exist.
- PWA push subscriptions still wait on a VAPID endpoint.
- Live provider/channel/Qdrant checks were not run in this no-key review.

## Recommended Next Work

Post-beta follow-ups (not part of the `v0.1.0-beta.1` gate):

1. `#422`: tool-loop intermediate receipts survive a later-round cancel.
2. `#420` follow-through: wire `shell.exec` to a server config flag
   (`ARDUR_ENABLE_SHELL_EXEC_TOOL` + binary allowlist) so operators can register
   the hardened exec path, and migrate grant-driven registration to prefer it
   over `shell.run`.
3. ARD-463 propose-half: emit pending approval cards before irreversible tools,
   with `RequiresApproval` caveats.
4. Auto-register local STT/TTS when `ARDUR_LOCAL_STT_COMMAND` /
   `ARDUR_LOCAL_TTS_COMMAND` are set.
