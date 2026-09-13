# Current Status and Ready Features

Implementation baseline reviewed: `dev` at
`6285be20cbb1a8c9331adb7f2b85866cb5bc5cce` on 2026-09-12. This document is the
release checklist for `v0.1.0-beta.2`: every claim below is tied to that
reviewed code baseline and was re-verified against it.

## Repository and Verification Status

- GitHub PRs merged into `dev` for this baseline (the `v0.1.0-beta.2`
  hardening bundle, closing every finding the beta.1 reviews generated):
  - `#440` — Dockerfile builder re-pinned to the toolchain's rustc
    (closes `#436`), `.gitleaksignore` for the two synthetic redaction
    fixtures (closes `#437`), and the DCO exempt-list read moved to the base
    ref with append-only + full-SHA enforcement and CODEOWNERS coverage
    (closes `#435`).
  - `#441` — `shell.exec`, an argv-exec confinement path with exact
    `argv[0]` matching, bounded output capture, an absolute-only child
    `PATH`, and process-group teardown on timeout (closes `#420`).
  - `#442` — tool-loop receipts settled on cancel: a terminal
    `llm.completion.cancelled.v1` marker and no success return for an
    abandoned turn (closes `#422`).
- The previous baseline's PRs (`#418`, `#419`, `#421`, `#424`, `#423`,
  `#425`) remain in `dev` unchanged. Also merged in the
  `09b40df`..`6285be2` range, ahead of this bundle:
  - `#439` — admin-ui session-id journal reads confined to the sessions root,
    so an externally supplied session id cannot traverse outside it
    (closes `#430`). Merged to `dev` as `334540d`; its own required checks were
    green and it is included in the workflow evidence for the baseline commit
    below.
- No GitHub PRs were open at this review.
- Required GitHub workflows were green before each merge, and the full
  dev-push check set (CodeQL, CodeQL/Rust, Trivy, build-healthcheck-scan,
  cargo-deny, hugo, macos-15/stable, ubuntu-latest/stable, qdrant integration
  `--ignored`) is green on the baseline commit: CI run
  [34686864849](https://github.com/ArdurAI/ardur-agent/actions/runs/34686864849),
  docker run
  [34686864841](https://github.com/ArdurAI/ardur-agent/actions/runs/34686864841),
  site-deploy run
  [34686864825](https://github.com/ArdurAI/ardur-agent/actions/runs/34686864825).
  DCO runs on PRs and was green before each merge. Each of `#440`, `#441` and
  `#442` merged with all 12 required checks passing and every review thread
  resolved.
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
  **decide-half** of ARD-463 / ARD-139.
- The **propose-half** is now reachable from a server boot (ARD-463). Setting
  `ARDUR_APPROVAL_GATED_CAPABILITIES` to a CSV of capability labels attaches the
  approval store to the runtime; a tool call carrying a listed capability then
  does not execute — it proposes a pending card into that same store, mints
  `approval.propose.created.v1`, and is refused until an operator decides it. A
  retry of the identical call (matched on `sha256(arguments)`) reuses the
  existing card rather than proposing a second one, and proceeds once approved.

  Gating is by capability, not tool name, so a capability stays gated however
  many tools declare it. The variable is **empty by default**, which builds the
  runtime with no approval store at all — the gate is absent rather than present
  and passing everything. A label containing whitespace can never match a
  capability, so it is rejected at config load rather than silently gating
  nothing. Covered by `crates/server/tests/approval_gate_boot.rs` (wiring) and
  `crates/fused-runtime/tests/approval_gate.rs` (gate behaviour).

  Known limitation: the gate is consulted on the tool-invocation path. Capability
  labels must match what the tool registry declares (`cap.shell_exec`,
  `cap.fs_write`, and so on); there is no wildcard form, and a label that matches
  no registered capability gates nothing.

- **Integration configuration** (ARD-459) lands the declaration surface, not yet
  the adapters. `[integrations.<name>]` blocks in `~/.ardur/config.toml` declare
  an external tool as either a `command` (an executable driven through
  argv-exec) or a `root` (a confining directory), validated strictly at load:
  unknown keys, wrong types, and missing or ambiguous endpoints all fail the
  parse rather than being normalised, because a silently-ignored key looks like
  a setting that took effect. `ARDUR_INTEGRATIONS_<NAME>_ENABLED`, `_COMMAND`
  and `_ROOT` adjust a declared integration but cannot introduce one, so the set
  of possible integrations stays in reviewed configuration rather than in an
  inherited environment variable.

  Everything is off by default and off means absent: a disabled integration is
  never passed to an adapter at all, so it cannot execute adapter code. An
  enabled integration with no compiled-in adapter fails the boot rather than
  running without a capability its configuration declares. `ardur doctor`
  reports each integration's enabled state and whether its backing resource is
  present — presence only, never values.

  What is **not** here: the `dolthub` and `obsidian` adapters. Until one
  exists, enabling those integrations fails the boot by design. Covered by 30
  unit tests in `crates/integrations` and
  `crates/cli/tests/cli_integrations_doctor.rs`, which drives the real
  `ardur doctor` binary.

- **The beads adapter** (`crates/integration-beads`) turns a declared
  `[integrations.beads]` block into six tools: `beads.ready`, `beads.list`,
  `beads.show`, `beads.create`, `beads.update`, `beads.close`. Each verb is its
  own tool, because a tool is the unit the runtime authorises, gates on cost,
  and receipts — collapsing them behind a `verb` argument would make
  `beads.close` indistinguishable from `beads.list` at authorisation time.

  Reads require `cap.integration.beads.read`, writes
  `cap.integration.beads.write`, so consulting the tracker and mutating it are
  separately grantable. Every verb also declares `cap.shell_exec` and
  `cap.process_spawn`: `invoke` runs `ShellExecTool` directly rather than
  dispatching through the runtime, so that tool's own capability requirements
  are never consulted, and declaring them here is what keeps a deployment's
  process-spawn gate honest.

  Mutating verbs attach structured detail (verb plus operands) to their output.
  The runtime does **not** consume it yet — `ToolCallReceipt` is built from the
  call name, an arguments digest, an output digest and the cost, appended
  uniformly for every tool call, and nothing reads `ToolOutput::receipt_data`.
  Beads mutations are therefore receipted like any other tool call today; the
  verb-level record exists ahead of the runtime learning to fold it in.

  Invocation delegates to `ShellExecTool` with a single-entry allowlist rather
  than spawning directly, so the #420 argv-exec confinement applies unchanged
  and improves in one place. A command path containing whitespace is refused at
  build, since the allowlist can never match one. Covered by 17 tests, four of
  which drive a real process: one proves a hostile issue title reaches the
  child as a single unexpanded argument, and one proves a non-string `status`
  is refused rather than silently running an unfiltered list.

- **The obsidian adapter** (`crates/integration-obsidian`) turns a declared
  `[integrations.obsidian]` block into `obsidian.read`, `obsidian.search` and
  `obsidian.write`, each confined to the configured vault root.

  Path resolution delegates to the `file.*` builtins rather than reimplementing
  containment: the root is canonicalized, `..` components rejected, and
  containment re-checked after canonicalization so a symlink cannot point out
  of the vault. A second containment implementation is the one that eventually
  has the bug.

  Reads declare `cap.integration.obsidian.read` and `cap.fs_read`; writes add
  `cap.integration.obsidian.write` and `cap.fs_write`. As with beads, the
  nested builtins' capabilities are re-declared explicitly because invoking a
  `Tool` inside a `Tool` bypasses the dispatcher that enforces them. A write's
  structured record names the path and mode but never the note's contents.

  A configured vault that does not exist still builds: a missing directory is a
  host fact an operator may legitimately have (an unmounted vault), reported by
  doctor rather than blocking boot. Covered by 15 tests, seven driving a real
  filesystem — four distinct escape attempts (`../`, nested `../..`, an
  absolute path, and a traversal to `/etc/passwd`) are all refused, and a
  refused write is proven to create nothing.

- **The dolthub adapter** (`crates/integration-dolthub`) turns a declared
  `[integrations.dolthub]` block into `dolthub.query`, and `dolthub.execute`
  only when the operator declares writable tables via `table:` entries — an
  integration with no declared table gets no write tool at all, rather than an
  unconstrained one.

  The admission gate is built around a verified fact: `dolt sql -q` executes
  **every** statement in its argument, so `select 1; insert into t values (99)`
  returns the select's rows and performs the insert. A read guard that inspects
  only the leading keyword is therefore a write hole, and multi-statement input
  is refused outright rather than classified statement by statement. Comments
  are stripped and quoting tracked first, so a `;` inside a literal is data and
  a `;` after a comment still counts.

  Writes additionally check the target table against the allowlist and refuse
  DDL: a table allowlist cannot constrain a statement that drops a table. Both
  verbs re-declare `cap.shell_exec` and `cap.process_spawn`, and SQL is passed
  as one argv entry.

  Two bypasses were found by probing a real database rather than reasoning
  about the grammar, and both are closed with regression tests: backslash
  escapes (`\'` keeps a checker's quote state out of step with Dolt's, hiding a
  separator) and CTE preambles (`with c as (select 1) insert ...` leads with a
  read keyword and writes).

  Review found a third bypass and a fourth surfaced while checking it: a
  multi-table `DELETE` names its target before `FROM`, and a multi-table
  `UPDATE` assigns through a join, so both reach a table the allowlist never
  sees. Multi-table write forms are now refused outright.

  The real-database tests are `#[ignore]`d and run by a dedicated CI job, for
  the reason #358 established: an early return on a missing binary is counted
  as a pass, so the whole suite could report green without executing.

  Covered by 37 unit tests and 6 against a **real Dolt database**, one of which
  demonstrates the stacked-statement execution the gate exists to stop, and one
  proving a refused write leaves the row count unchanged.

  With this adapter, ARD-459's config surface, registry, doctor checks and all
  three reference adapters are complete.

- **Tool advertisement is cap-token scoped** (gh#415). The list of tools sent
  to the provider is filtered through the same predicate the invocation path
  uses, so a turn is told about exactly the tools its cap-token permits. Tool
  names are not neutral — `dolthub.execute` or a customer-named connector
  discloses what a deployment is wired to — so a withheld capability is not
  discoverable by reading the tool list. Enforcement is unchanged: a call to an
  unadvertised tool is still denied at invocation, pinned by a test asserting
  the tool body never runs.

- **Destructive file writes can capture what they overwrite** (gh#413).
  `SnapshotStore` puts the prior bytes in a content-addressed store before
  `file.write` lands, so a mistaken write is recoverable and an audit can say
  what a file changed *from*, not just that it changed. Opt-in per deployment
  via `WriteFileTool::with_snapshots`; without it the tool behaves exactly as
  before. An absent file is recorded as absent rather than as empty content, so
  undoing a creation removes the file. A blob whose bytes no longer match its
  digest is refused rather than restored.

- **Post-write diagnostics (gh#414).** `file.write` can run syntax checkers
  over the content it just wrote, enabled through `BuiltinOpts::diagnostics`.
  Problems are attached to the tool output as `diagnostics`; the write itself
  still succeeds.

  Enable it on a server with `ARDUR_FILE_WRITE_DIAGNOSTICS=1` (off by default,
  and ignored unless `ARDUR_FILE_TOOL_ROOT` registers the file tools at all).

  Advisory by construction: a checker runs after the bytes are on disk, so
  failing the call would report a failed write that actually succeeded, and a
  model retrying on that error would write the same content twice.

  An append is checked against the resulting file on disk, not the appended
  fragment — a fragment rarely parses alone, so checking it in isolation would
  report spurious errors and miss real breakage. Files over 8 MiB are not read
  back for this, and report `checked: false`.

  The output distinguishes `checked: false` (nothing understood this file, or
  every applicable checker failed) from a clean check, because an unchecked
  file must not read as validated. The list is capped at 50 with
  `diagnostics_truncated` reporting the drop.

  A checker that panics is contained and reported as unchecked: checkers are
  where external language servers will plug in, and an unwinding checker would
  otherwise fail a write that already succeeded — which for an append means a
  retry duplicates the appended bytes.

  Diagnostics are written to both the tool output and the receipt payload, so
  an auditor reads the same result the model was given.

  Diagnostic messages carry the parser's message only, never its rendered
  source excerpt: `toml::de::Error`'s `Display` echoes the offending line, so a
  syntax error on a line holding a credential would copy that credential into
  the model's context and the receipt.

  Limitation, stated rather than implied: gh#414 asks for real language servers
  (pyright, gopls, rust-analyzer). What ships here is the extension point plus
  dependency-free JSON and TOML syntax checkers. Real language servers need an
  LSP/JSON-RPC client (absent from this workspace), host binaries, and process
  spawning that must go through the gh#420 argv-exec confinement — none of
  which is built.

  Enable it through `BuiltinOpts::snapshot_store`. Blobs are written
  owner-only (0600) under a 0700 store, verified against their digest before
  reuse and before restore, and written atomically so an interrupted capture
  cannot leave a blob a later capture would accept. A file over the configured
  ceiling (64 MiB by default) refuses the write rather than being read into
  memory. Symlinked paths are refused: writing through a link changes its
  target, so an undo cannot be expressed as restoring that entry.

  Limitations, stated rather than implied:

  - The snapshot id is written to `ToolOutput::receipt_data`, but the runtime
    builds its `ToolCallReceipt` unconditionally and never reads that field —
    so snapshots are **not** yet linked into the receipt chain.
  - Under concurrent writers to the same path, two captures can both record the
    original content before either write lands, so the second snapshot cannot
    restore the state immediately before it. The store stays consistent; the
    undo *history* is not linearisable. Serialising capture and write behind a
    per-path lock would fix it — tracked as gh#460 with the design questions
    it raises (lock-map eviction, canonical-path keying, block-vs-fail-fast).
  - Shadow-git proper (history semantics) is not built; it needs a git
    dependency this workspace does not have.

## Not Yet Turnkey

Do not treat this repo as a public production deployment without additional
hardening and operator work.

- Run live deployments in private channels first.
- Direct MCP exposes only capability-free tools until MCP requests can carry the
  same fused-runtime cap-token/Cedar context as normal tool calls.
- Hardened shell/file/http tools exist but stay off until an operator grant or
  server env opt-in (ARD-457).
- `#420`'s hardened sibling `shell.exec` is implemented and tested — it execs
  argv directly with no shell, matches `argv[0]` exactly, bounds captured
  output, gives the child an absolute-only `PATH`, and (on **Unix**) tears down
  the whole process group on timeout — but is not yet wired to a server config
  flag, so it is never registered at boot. `shell.run` remains a prefix gate,
  not full argv confinement. Deployments that construct a registry directly can
  opt in via `BuiltinOpts::enable_shell_exec`.

  Process-group teardown is Unix-only: the Windows path has `kill_on_drop`
  alone, which reaps the immediate child but not descendants it spawned, so a
  timed-out process can leave grandchildren running there. A Job Object
  implementation is follow-up work.

  Neither tool sandboxes the binary it runs: allowlisting `sh`, `env`,
  `xargs`, `find -exec`, or any interpreter grants what that binary can do.
  Cap-token and Cedar authorize *whether* a tool may be invoked — they never
  see argv — so confining a running process needs an OS-level sandbox or a
  genuinely leaf binary.
- Tool-loop intermediate receipts are settled on cancel for **non-streaming**
  submissions (`#422`): the receipt log is append-only, so a turn abandoned
  mid-loop cannot un-mint the rounds that already committed. `submit_inner`
  now appends a terminal `llm.completion.cancelled.v1` receipt so the chain
  never ends on an intermediate round, and an abandoned turn is never reported
  as a success carrying an earlier round's receipt.

  **The streaming path does not yet have this.** `stream_inner` commits one
  receipt per provider round but never records a cancellation marker, and
  `handle_http_stream` drops the fused stream when forwarding fails. An SSE
  client that disconnects after a tool-use round has persisted its receipt but
  before the final round settles can therefore still leave the chain ending on
  an intermediate receipt. Tracked as follow-up work below.
- Local STT/TTS providers exist in `ardur-media-audio`, but the server currently
  auto-registers Whisper transcription only.
- Approval *propose* (the agent creating a pending card before an irreversible
  tool) is not mounted; only decide endpoints exist.
- PWA push subscriptions still wait on a VAPID endpoint.
- Live provider/channel/Qdrant checks were not run in this no-key review.

## Recommended Next Work

Post-beta follow-ups (not part of the `v0.1.0-beta.2` gate):

1. `#420` follow-through: wire `shell.exec` to a server config flag
   (`ARDUR_ENABLE_SHELL_EXEC_TOOL` + binary allowlist) so operators can register
   the hardened exec path, and migrate grant-driven registration to prefer it
   over `shell.run`. Windows process-tree teardown (a Job Object equivalent of
   the Unix process-group kill) belongs with it.
2. `#422` follow-through: give the streaming path the same cancellation
   settlement as `submit_inner`, so an SSE client disconnecting mid tool-loop
   cannot leave the chain ending on an intermediate receipt.
3. ARD-463 propose-half: emit pending approval cards before irreversible tools,
   with `RequiresApproval` caveats.
4. Auto-register local STT/TTS when `ARDUR_LOCAL_STT_COMMAND` /
   `ARDUR_LOCAL_TTS_COMMAND` are set.
