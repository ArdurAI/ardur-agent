# Current Status and Ready Features

Implementation baseline reviewed: `dev` at `c5754d52d37b` on 2026-06-29.
This document may be updated by later documentation-only commits; the feature
claims below are tied to that reviewed code baseline.

## Repository and Verification Status

- GitHub PRs `#186` and `#187` are merged into `dev`.
- No GitHub PRs were open at the time of this review.
- Required GitHub workflows were green before each merge:
  - `#186`: DCO and CI passed on macOS and Ubuntu.
  - `#187`: DCO and CI passed on macOS and Ubuntu after the dependency fixes.
- Local no-key baseline passed on updated `dev`:
  - `cargo test -p ardur-e2e-tests`
  - `cargo test -p ardur-server --test boot_smoke`
  - `cargo test -p ardur-cli --test cli_smoke_echo`
  - `cargo build --workspace --bins`
- Cargo metadata reports 47 workspace packages.

## Ready Without External Accounts

These paths are usable on a developer machine without model-provider keys.

| Area | What is ready | How to use it |
| --- | --- | --- |
| Offline fused chat | `ardur chat` falls back to a network-free stub when `ANTHROPIC_API_KEY` is absent, while still exercising the fused runtime, receipts, journals, memory, cost gate, cap-token path, and Cedar policy. | `cargo run -p ardur-cli -- chat --plain` |
| Legacy echo chat | A minimal in-memory echo path with no provider, cost, or persistent state. | `cargo run -p ardur-cli -- chat --echo` |
| Local setup and diagnostics | Setup, redacted config, redacted logs, redacted state snapshot, doctor checks, and session management commands are present in the CLI. | `ardur setup --yes`, `ardur doctor`, `ardur config`, `ardur logs`, `ardur debug`, `ardur session ...` |
| E2E substrate tests | Stub-provider scenarios prove fused cap-token, Cedar, cost gate, provider, receipt, journal, and memory paths without network calls. | `cargo test -p ardur-e2e-tests` |
| Skills | Filesystem `SKILL.md` loading with progressive disclosure is implemented. Example skills include code review, runbooks, postmortems, onboarding, and `cite-or-refuse`. | Set `ARDUR_SKILLS_DIRS=./examples/skills` |
| Admin UI binary | `ardur-admin` is a read-only dashboard over journals, receipts, costs, memory, and Trust Center APIs. | `cargo run -p ardur-admin -- --help` |
| Evaluation harness | `ardur-eval` can run scenario files against the server `/chat` contract and emit JSON, JUnit, or Markdown. | `cargo run -p ardur-eval -- --help` |
| Static PWA shell | `web-client/` is installable as a static PWA shell with in-memory bearer-token handling and approval deep-link hooks. | `cd web-client && python3 -m http.server 4173` |

## Ready With Local Services or Credentials

These features are implemented but need a provider key, local daemon, channel
token, or explicit operator configuration.

| Area | What is ready | Required inputs |
| --- | --- | --- |
| Providers | Anthropic, OpenRouter, generic OpenAI-compatible endpoints, Ollama, Codex CLI, and Claude CLI are selectable through `ARDUR_PROVIDER`. Anthropic, OpenRouter, OpenAI-compatible, and Ollama expose provider-level streaming. | API keys, local Ollama daemon, or logged-in `codex` / `claude` CLIs depending on provider. |
| HTTP agent API | `ardur-server` exposes `POST /chat`, `POST /acp`, `POST /slack/events`, `GET /healthz`, `GET /health`, `GET /metrics`, `GET /admin/runtime`, `GET /openapi.json`, and generated Rust/Python clients. | Server environment, Slack credentials, provider selection, chat/admin bearer tokens where configured. |
| Chat channels | Slack is the primary channel; Matrix, Discord, and Telegram can be enabled alongside it. All route inbound messages through the same fused runtime. | Bot credentials and allowlists. Use private channels first. |
| Durable memory | `in_memory` is the default. `qdrant` persists bi-temporal memory. `hybrid` adds Qdrant dense search plus a file-backed Tantivy/BM25 sparse index fused by reciprocal-rank fusion. | `QDRANT_URL` for `qdrant` or `hybrid`; local embedder download on first hybrid boot. |
| Observability | Provider calls emit OpenTelemetry GenAI spans; `/health`, `/metrics`, and `/admin/runtime` expose operational posture with secret redaction. | `ARDUR_OTEL_ENABLED=true` and an OTLP endpoint for export. |
| MCP | Ardur can serve MCP over bearer-gated Streamable HTTP and consume remote MCP servers into the runtime tool registry. | `ARDUR_MCP_ENABLED=true`, `ARDUR_MCP_BEARER_TOKENS`, optional `ARDUR_MCP_REMOTE_SERVERS`. |
| Voice transcription | `voice.transcribe` is registered by the server when Whisper credentials are present. The provider validates size, duration, HTTPS base URLs except loopback test URLs, and records provider receipt hashes. | `OPENAI_WHISPER_API_KEY` or `OPENAI_API_KEY`. |
| Local voice providers | `ardur-media-audio` has command-backed local STT and TTS providers for on-device engines such as whisper.cpp, Vosk, Piper, or OS speech tools. They execute commands directly, not through a shell. | `ARDUR_LOCAL_STT_COMMAND` / `ARDUR_LOCAL_TTS_COMMAND`; integration into server default registry is not yet automatic. |
| Platform tools | Browser, terminal, and web tool crates exist with policy checks and receipt metadata. | Explicit registration and policy setup; these are not yet default server boot tools. |

## Security and Trust Features Available Now

- Cap-token authorization with offline attenuation and revocation-oriented
  design.
- Cedar policy evaluation in the fused runtime.
- Cost-gate projection, ceiling checks, reservation, finalization, and refund.
- Prompt-injection defense before provider dispatch and on tool output.
- JWS ES256 receipt chain with parent-hash linkage.
- Append-only JSONL session journals.
- Receipt-linked memory writes and append-only memory forget/tombstone behavior.
- Redacted operator surfaces for config, logs, debug output, metrics, and admin
  runtime inspection.
- Webhook signature replay protection and hardened SSRF / shell-denylist checks
  from the latest green integration batch.

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

The hardened built-in tools are implemented and tested in `ardur-tool-registry`:

- `shell.run`
- `file.read`
- `file.write`
- `file.list`
- `http.fetch`

They are capability-gated and include command allowlists, filesystem root
confinement, HTTP host allowlists, and SSRF defenses. Wiring those hardened
built-ins into default boot is tracked by `ARD-457` and should not be described
as default-on yet.

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

## Not Yet Turnkey

Do not treat this repo as a public production deployment without additional
hardening and operator work.

- Run live deployments in private channels first.
- `ardur-server` still expects Slack credentials at config load, even if the
  operator mainly wants `/chat`.
- HTTP `/chat` currently accepts the synchronous request/response path. A
  `stream: true` request is rejected rather than downgraded, so the static PWA
  chat path needs server-side SSE follow-up before it is a complete chat client.
- PWA approval endpoints under `/approvals/...` are hooks for the approval-gate
  epic and are not mounted by the server yet.
- Direct MCP exposes only capability-free tools until MCP requests can carry the
  same fused-runtime cap-token/Cedar context as normal tool calls.
- Hardened shell/file/http tools exist but are not default-registered by
  `ardur-server` yet (`ARD-457`).
- Local STT/TTS providers exist in `ardur-media-audio`, but the server currently
  auto-registers Whisper transcription only.
- The only runbook-listed known gap at this review is `ARD-21`: Dependabot
  triage remains unmanaged/ad hoc.
- Live provider/channel/Qdrant checks were not run in this no-key review. The
  bootstrap on this workstation reported provider/channel/Qdrant credentials
  missing, while local tools `cargo`, `docker`, `ollama`, `codex`, and `claude`
  were present.

## Recommended Next Work

1. Finish `ARD-457`: default-register hardened shell/file/http tools with an
   operator-safe `ardur grant` flow.
2. Add HTTP SSE support for `/chat stream:true`, then smoke the PWA against a
   real server.
3. Mount the approval-gate endpoints behind bearer auth and receipt logging.
4. Add a server configuration mode for HTTP-only local use without mandatory
   Slack credentials.
5. Convert this status into a release checklist before tagging a public build.
