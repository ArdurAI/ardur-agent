# Current Status and Ready Features

Implementation baseline reviewed: `dev` at
`6c2599cb9c8d81a5afd8bf6d83910ece83b1a517` on 2026-09-14. This is the
implementation inventory for `v0.2.0`, not a claim that the open follow-ups
below have shipped. “Available” distinguishes shipped binary wiring from
library-only surfaces; a partial slice does not close its larger feature.

## Repository and Verification Status

The previous main promotion, `663cd1b` (`v0.1.0-beta.2`), has the same tree as
its integration baseline `44ed0aa`. The following 12 integration merges are
new relative to that main tree:

| PR / dev commit | Landed slice | What it did not deliver |
| --- | --- | --- |
| #450 / `c695d92` | ARD-463 approval proposals wired into opt-in server boot; approved cards consumed before invocation | Automatic resume, stable channel/ACP approval sessions, complete accounting of rejected provider rounds |
| #453 / `e5d24d1` | Strict integration declarations, registry API and doctor inspection | Server integration-config wiring or a CLI guarantee that invalid integrations abort startup |
| #454 / `7d00f37` | Beads reference adapter | Server registration or custom mutation metadata in signed receipts |
| #455 / `bea7404` | Obsidian reference adapter; CLI chat integration registration; dangling-symlink containment fix in shared file tools | Full-text vault search, server registration, snapshot/diagnostic hooks on Obsidian writes |
| #456 / `e527d14` | Dolt CLI SQL reference adapter and gated real-Dolt tests | A remote DoltHub session/sync service or server integration registration |
| #457 / `9194f62` | #415 capability/Cedar-scoped tool advertisement | Tool search, ranking or deferred schema loading; #458 remains open |
| #459 / `46a86cc` | #413 opt-in content-addressed capture/restore library | Binary enablement, receipt-linked rollback, shadow-git history or serialized capture/write; #460 remains open |
| #461 / `cacd7ee` | #414 advisory JSON/TOML post-write syntax diagnostics, server opt-in | Language-server semantic diagnostics, CLI/Obsidian enablement or readable diagnostics embedded in signed receipts |
| #463 / `06a8cd0` | #417 additive cap-token gate on approval decisions | Revocation, other write-admin endpoints or a general write-admin dashboard |
| #465 / `d822c85` | #361 shared-deny-list constructors for verifying sub-agents | Production `delegate_task` revocation, durable/cross-process revocation or in-flight cancellation |
| #466 / `0b01d6c` | #364 case-insensitive wildcard host matching with a dot boundary | `web.fetch` SSRF/timeout/redirect hardening, browser DNS validation or CDP argument encoding |
| #467 / `6c2599c` | #367 meter saturation and instance-bound reserve/rollback; redaction-pattern compilation; deny-by-default config helpers | Bounded SDK ingress queues, orphan-module cleanup or automatic redaction of journal writes |

The earlier shell-exec, cancellation, journal-path and supply-chain hardening
remains present, including the beta.2 promotion review fixes. It is inherited
work, not counted again in the table above.

Evidence for the reviewed implementation SHA:

- CI run [34792700474](https://github.com/ArdurAI/ardur-agent/actions/runs/34792700474),
  docker run [34792700473](https://github.com/ArdurAI/ardur-agent/actions/runs/34792700473),
  and site run [34792700493](https://github.com/ArdurAI/ardur-agent/actions/runs/34792700493)
  completed successfully. These links prove that SHA, not a later promotion.
- `.github/workflows/ci.yml` defines the Python checks, formatting, all-target
  clippy/check, all-feature workspace tests, security analysis, and separate
  live Qdrant and Dolt jobs. DCO runs on PRs. Branch protection and optional
  analysis annotations are distinct gates; inspect the current check set.
- Cargo metadata reports 62 workspace packages.
- `.github/workflows/docker.yml` publishes the scanned and healthchecked image
  as `ghcr.io/ardurai/ardur-agent:<tag>` on eligible `v*` tags, with provenance;
  it does not publish `:latest`. The package remains **private**.
- `.github/workflows/release.yml` runs on Release publication and attaches Linux
  binaries, an SPDX SBOM, `SHA256SUMS`, cosign bundles and provenance. Tag,
  promoted-main SHA, workflow results and image digest belong in the
  [release record](https://github.com/ArdurAI/ardur-agent/releases), not in
  inferred claims about a newer branch tip.
- [Fresh-machine runbook](fresh-machine.md) describes offline CLI, live-provider,
  private-channel and container checks. Source inspection or an offline test
  does not substitute for a live provider, channel or remote-store smoke.

### Required local release checks

These are requirements, not completed validation evidence:

- Run `python3 -m unittest discover -s tests`, `cargo fmt --all -- --check`,
  all-target clippy/check, `cargo test --workspace` (not just `--lib`), the
  e2e/boot/CLI smoke targets, workspace binaries, `cargo deny check`, and
  push-range secret scanning. Record results against the exact PR head.
  Provider keys must be unset for the no-key suite.

## Ready Without External Accounts

| Area | Available path | Boundary |
| --- | --- | --- |
| Offline fused CLI chat | With the default `anthropic` backend, absent `ANTHROPIC_API_KEY` selects the CLI stub; use the [offline CLI recipe](#offline-cli-recipe) | Exercises local authorization, cost, receipts, journals and memory; not a live-provider test or a server fallback |
| Legacy echo chat | `cargo run -p ardur-cli -- chat --echo` | Minimal in-memory echo, without the fused persistent substrate |
| Setup and inspection | `ardur setup`, `doctor`, `config`, `logs`, `debug`, and [session lifecycle commands](session-lifecycle.md) | Redacted inspection/export surfaces are not a promise that raw persisted journals contain no sensitive content |
| E2E substrate suite | `cargo test -p ardur-e2e-tests` | Stub scenarios run locally; live-service scenarios are explicitly ignored unless opted in |
| Skills | Filesystem `SKILL.md` discovery and progressive disclosure via `ARDUR_SKILLS_DIRS` | Executable tool authority still depends on the caller's grants/policy |
| Operator UI | `ardur-admin` provides read-oriented inspection and can proxy approval decisions | Not the general write-admin dashboard requested in #417 |
| Static PWA | `web-client/` streams `/chat` and calls approval endpoints | Needs a running server; cross-origin use needs `ARDUR_CORS_ORIGINS`; push delivery still lacks a VAPID endpoint |
| Evaluation harness | `ardur-eval` drives the consolidated `/chat` contract and emits JSON/JUnit/Markdown | Needs a running server; not an independent offline provider |

### Offline CLI recipe

With `ardur` already built/installed and on `PATH`, on a machine **without an
existing `~/.ardur` state tree** (`ardur setup --yes` writes a default
configuration and would overwrite one):

```sh
(
  export ARDUR_PROVIDER=anthropic
  unset ANTHROPIC_API_KEY
  ardur setup --yes &&
    ardur chat --plain
)
```

An already-configured machine should skip the `setup` line; the subshell's
export/unset is all the offline path needs.

The subshell overrides inherited provider selection and removes an inherited
key without changing the parent shell. `FusedEngine::new_for_session` resolves
`ARDUR_PROVIDER` before fallback: clearing `ANTHROPIC_API_KEY` alone does not
make a non-key backend offline.

## Ready With Services or Explicit Configuration

- `ARDUR_PROVIDER` selects hosted, compatible HTTP, local-daemon or CLI-backed
  providers. Required credentials/services depend on that selection.
  **The default server provider requires `ANTHROPIC_API_KEY`; unlike CLI chat,
  the server has no automatic offline-stub fallback.** This key requirement
  is conditional on the selected backend, not universal to every server boot
  (`Config::from_env` in `crates/server/src/config.rs`).
- The HTTP router exposes `/chat` (JSON/SSE), `/acp`, `/healthz`, `/health`,
  `/metrics`, `/admin/runtime`, approval list/decide endpoints and
  `/openapi.json`; Slack events are mounted when configured. HTTP-only boot
  without Slack is supported. `/metrics` and `/admin/runtime` use admin
  authorization. The OpenAPI document is incomplete and duplicates approval
  paths at this baseline (#366, #464); it is not a complete route inventory.
- Slack, Matrix, Discord and Telegram route inbound messages through the fused
  runtime when enabled with credentials and allowlists. Start with private
  channels. Empty ingress allowlists deny access.
- `in_memory` is the default memory backend. `qdrant` provides durable
  bi-temporal memory; `hybrid` adds a file-backed BM25/Tantivy sparse index and
  reciprocal-rank fusion. Qdrant-backed modes require `QDRANT_URL`; local dense
  embedding may require a first-run model download. The scroll ceiling in
  #357 remains a correctness limitation, including forget/tombstone reads.
- `ARDUR_OTEL_ENABLED=true` plus an OTLP endpoint enables telemetry export.
  Operational inspection and exported diagnostics are redacted where their
  readers implement redaction; this is not an at-rest encryption guarantee.
- MCP serving is opt-in with `ARDUR_MCP_ENABLED=true` and
  `ARDUR_MCP_BEARER_TOKENS`; remote tool discovery uses
  `ARDUR_MCP_REMOTE_SERVERS`. Direct MCP exposes only capability-free tools
  until its requests carry the fused authorization context.
- Voice transcription is registered when `OPENAI_WHISPER_API_KEY` or
  `OPENAI_API_KEY` is present. Local STT/TTS providers exist as command-backed
  library surfaces but are not automatically registered by server boot.

## Tool Registration and Authority

`assemble_tool_registry` in `crates/server/src/mcp.rs` is the server assembly
entry point. It includes example tools, `delegate_task`, explicitly enabled
built-ins, credential-dependent media tools, filesystem skills and discovered
remote MCP tools. Registration does not mean every caller may invoke a tool.

Hardened `shell.run`, `file.read`, `file.write`, `file.list` and `http.fetch`
are off until the corresponding CLI grant or server configuration enables them.
Server controls include `ARDUR_ENABLE_SHELL_TOOL`, `ARDUR_ENABLE_HTTP_TOOL`,
allowlists, and `ARDUR_FILE_TOOL_ROOT`. CLI chat reads its grant ledger.

`shell.exec` is the direct argv-exec sibling: exact executable allowlisting,
bounded output, sanitized absolute-only `PATH`, and Unix process-group cleanup
on timeout. It is available through `BuiltinOpts::enable_shell_exec` and is
used internally by command integrations, but no general server flag or CLI
grant registers it as a standalone tool. Windows descendant cleanup remains
incomplete. Neither shell tool is an OS sandbox: granting an interpreter or
launcher grants what that executable can do. Authorization is not argv-level
or process-level confinement.

`tool_defs_for` in `crates/fused-runtime/src/runtime.rs` filters advertised
schemas through name-scoped cap-token/Cedar authorization and declared
capabilities. Invocation still checks authorization independently. This #415
slice does **not** implement #458 catalog search or deferred loading: every
admitted tool's full schema is still advertised. Advertisement also does not
promise later approval, budget admission or execution success.

`ardur-browser`, `ardur-terminal` and `ardur-web` remain explicit library
integration surfaces, not the normal binary registries. In particular, real
CDP transport is not implemented, and the weak `web.fetch` implementation must
not be equated with the separately hardened `http.fetch`.

## Approvals: Shipped Server Loop, Explicit Limits

ARD-463's propose-half **is wired into server boot**. A nonempty
`ARDUR_APPROVAL_GATED_CAPABILITIES` CSV attaches the approval store and its
capability set together; empty configuration leaves the gate absent. Labels
must match registered capabilities exactly, with no wildcard expansion.

The runtime matches a card on tool name, argument digest and session. A gated
call proposes a pending card and attempts an `approval.propose.created.v1`
receipt instead of running the tool. An approved card is consumed before the
retried invocation, so even a failed tool does not make that approval reusable.
See `authorize_or_propose_approval`, `crates/server/tests/approval_gate_boot.rs`
and `crates/fused-runtime/tests/approval_gate.rs`.

Limits that remain:

- This is a retry loop, not automatic execution after approval. HTTP retries
  must preserve the session ID. Channel and ACP messages create fresh sessions
  and cannot complete that matching loop (#451). CLI chat does not attach this
  server approval gate.
- A rejected tool request has already incurred its provider round, but that
  round is not charged to the budget on the rejection path (#452).
- Server cards live under its configured data directory. CLI `ardur approvals`
  uses its own home-based state layout. They share a store only when those
  locations actually coincide; the CLI has no data-dir override yet (#366).
- Card persistence and receipt/journal persistence are not one transaction.
  Proposal storage precedes receipt creation. HTTP decisions persist before
  receipt minting; a mint/journal failure is logged and can still return 200.
  A returned decision receipt ID is not written back into the persisted card.

The #417 slice adds `ARDUR_ADMIN_CAP_TOKEN_GATE=1`: approval decisions require
both the existing admin bearer (401 on failure) and an
`X-Ardur-Cap-Token` authorizing `approval.decide` (403 on failure). The token
is checked before mutation and used for the attempted decision receipt; CORS
allows the header. Verification uses the normal issuer key.

This is **approvals-only, opt-in, and without admin revocation**: the verifier
uses an empty deny list. Keep token lifetimes short. Config/key/MCP/webhook/
cron/skill write-admin endpoints and the broader dashboard are not delivered
by this slice; #417 remains open.

## Integrations: Three Reference Adapters, CLI Wiring

`[integrations.<name>]` blocks in CLI configuration declare exactly one
`command` or `root` endpoint. Parsing rejects unknown keys, invalid types and
ambiguous endpoints. `ARDUR_INTEGRATIONS_<NAME>_{ENABLED,COMMAND,ROOT}` may
adjust declared integrations but cannot introduce a new integration. Disabled
entries are not passed to adapters.

The library registry returns an error for an active unknown/rejected adapter.
**CLI chat catches parse/override/adapter errors, warns and omits integration
tools rather than aborting startup.** A missing configuration file yields no
tools; a configuration file that exists but cannot be read or parsed aborts
`ardur chat` before integration loading (`Config::load` in
`crates/cli/src/config.rs`), so only integration-specific errors — and files
that parse but declare nothing usable — take the warn-and-omit path. `ardur
doctor` reports validation and resource presence; it does not prove that an
invocation or remote service works.

All three adapters are reachable through CLI chat and doctor via the shared
`integration_registry` constructor in `crates/cli/src/fused.rs`. The server's
environment-based assembly does not load these integration declarations or
register these adapters.

| Adapter | Tools and confinement | Boundary |
| --- | --- | --- |
| Beads | `beads.ready`, `list`, `show`, `create`, `update`, `close`; configured executable via `ShellExecTool` | Read/write capabilities are separate; both also require shell-exec and process-spawn authority |
| Obsidian | `obsidian.read`, `search`, `write`; delegates to root-confined file built-ins | `search` is directory listing, not full-text/semantic search; writes enable neither snapshots nor diagnostics |
| DoltHub | `dolthub.query`; `dolthub.execute` only with declared `table:` write targets | Runs the configured **local Dolt CLI** in the tool context's cwd; does not establish remote DoltHub credentials or sync |

Dolt SQL is passed as one argv operand. Admission is a strict-subset parse
boundary (gh#469): a real SQL parser (MySQL dialect) resolves table
identifiers from the parse tree against the allowlist — no lexical blanking
or quote-stripping — and everything that does not parse is refused
(fail-closed). Executable `/*! ... */` comments, `#` comments (dolt's
statement splitter splits on `;` inside them while the parser does not),
control characters and non-ASCII characters (dolt ends `#` comments at
codepoints the parser treats as comment text) are refused before parsing.
`DESCRIBE`, `SHOW DATABASES`/`SCHEMAS` and read-only `WITH` CTEs are
admitted; `EXPLAIN` wrapping DML, DML inside CTE bodies (at any nesting
depth — the gate walks every query node in the parsed statement, so a `WITH`
carrying DML inside a parenthesized query, set operand, derived table,
scalar subquery, `INSERT ... SELECT` source or modeled SHOW filter is
refused too) and `SELECT ... INTO <table>` are not reads. Unmodeled SHOW
variants (`SHOW ENGINES` and kin) are refused: the parser's fallback for
them swallows `;` — `show engines; delete from secrets` parsed as one read
while dolt executed both — and a token-level guard refuses any single
parsed statement containing a non-trailing `;`. Single-table plain `INSERT`/`UPDATE`/`DELETE` only (MySQL `REPLACE`, which deletes conflicting rows before inserting, is refused); DDL,
multi-table writes and unknown targets are refused. The dedicated ignored
tests in `crates/integration-dolthub/tests/dolthub_sql_gate.rs` prove each
refusal against a real Dolt database, including the engine-side premise
(dolt executing the smuggled statement) for every bypass shape. The gate is
still not a complete SQL parser: valid MySQL outside the admitted subset is
refused (over-refusal is the acceptable direction), and a future engine
update that changes dolt's comment or splitting behavior needs a fresh
review of these premises.

Nested tools do not inherit dispatcher authorization automatically. These
adapters explicitly declare the capabilities of their inner shell/file tools.
Their custom mutation metadata is not a separate signed receipt. Current
`ToolCallReceipt` records call ID, tool name, argument/output-content digests
and cost; neither fused path consumes `ToolOutput::receipt_data`. Successful
fused calls therefore retain verb-specific digest evidence, not the custom
mutation operands or an atomic transaction over the external store.

## File Writes: Two Partial Features

### Snapshot capture (#413, #460)

`SnapshotStore`, `WriteFileTool::with_snapshots` and
`BuiltinOpts::snapshot_store` provide library opt-in capture/restore. Capture
records prior content or prior absence before a write; restoring prior absence
removes a created file. Blobs are digest-checked, written through a temporary
file and rename, and use owner-only store/blob modes on Unix. Symlinked capture
paths are refused and prior files above the default 64 MiB ceiling refuse the
write.

No shipped binary enables this hook: server opts use `snapshot_store: None`,
and CLI grants and Obsidian construct plain root-confined writers. The snapshot
ID exists only in `receipt_data`, which the runtime does not consume. Thus
**no binary-enabled, receipt-linked rollback or shadow-git history ships**.
Capture plus write also lacks a per-path critical section: concurrent writers
can both capture the same old state, yielding non-linearizable undo history
(#460). #413 remains open.

### Advisory syntax diagnostics (#414)

Server operators may set `ARDUR_FILE_WRITE_DIAGNOSTICS=1` alongside
`ARDUR_FILE_TOOL_ROOT`. `BuiltinOpts::diagnostics` then enables built-in JSON
and TOML syntax checkers after `file.write`. Findings do not turn an already
completed write into a failure. Unsupported files, failed checkers and caught
checker panics report unchecked rather than clean. Results are capped at 50,
with `diagnostics_truncated` indicating omitted findings.

Append checks inspect the resulting file, not just the new fragment; the read-
back is limited to 8 MiB. That is not a universal bound on overwrite input or
all checker memory. Parser source excerpts are withheld from diagnostic
messages. Results are in the returned tool content; signed receipts bind its
**digest**, not a readable diagnostic payload. The separate `receipt_data`
copy is currently ignored by the runtime.

CLI and Obsidian writes do not enable this hook. There is a checker extension
trait, but no language-server client or semantic diagnostics integration for
pyright, gopls or rust-analyzer. #414 remains open.

## Security Hardening: What the Audit Slices Mean

Core paths include cap-token/Cedar authorization, cost projection/reservation/
finalization/refund, injection scanning, ES256 hash-linked receipts and
append-only journals/memory tombstones. Server policy defaults to deny-all
without a configured policy unless `ARDUR_DEV_PERMISSIVE_POLICY=true`;
first-run CLI setup writes a scoped starter policy. Empty CORS configuration
emits no allow headers, and `*` is refused.

The Phase 5 audit slices are deliberately narrower than full audit closure:

- **#361:** `CapVerifyingRuntime::with_deny_list` and
  `InMemoryMultiAgentRuntime::verifying_with_deny` permit a shared deny list and
  reject revoked authority on a later child submission. Production
  `delegate_task` still uses the empty-deny constructor and an in-memory echo
  child, not another provider runtime. The fused list is process-local;
  `FileDenyList` has no production wiring. Durable/cross-process revocation and
  in-flight cancellation are not delivered.
- **#364:** wildcard web/browser host checks now require a dot boundary and
  ignore case. `web.fetch` still lacks the hardened fetch path's SSRF guard,
  timeout and validated redirect handling. Browser DNS validation (#320) and
  JSON-encoded CDP selector/text arguments remain open; dormant transport is
  not evidence that those surfaces are safe to activate.
- **#367:** sub-agent release saturates rather than wrapping; reserve/rollback
  binds to the instance that reserved, not a reused ID. Redaction constants
  fail loudly on compilation errors, and Discord/Telegram config helpers now
  agree with deny-by-default live ingress. Matrix/Discord/Telegram SDK queues
  are still unbounded. Orphan automation/ACP/webhook modules and other
  low-severity residuals remain on #367. Journal-export redaction does not
  mean `FileSessionJournal::append` redacts persisted entries.
- **#362/#363:** attenuation's nominal budget axis is not a spend cap;
  `CostEnvelope` is the actual ceiling. Cap-tokens remain bearer credentials:
  possession confers authority, subject to verification and expiry. No general
  per-request replay cache or proof-of-possession is delivered. Do not log
  tokens or infer durable revocation from the existence of a storage type.

## Streaming, Automation and Remaining Ceilings

`POST /chat` with `stream: true` returns fused stage/content/tool/usage/receipt/
finish events and in-band errors. Disconnecting closes forwarding and drops
the stream, but already committed provider rounds are not undone. SSE has no
terminal tool-loop cancellation marker yet.

For non-stream submissions, the #359 commit handshake blocks successful final
completion after cancellation wins. When earlier rounds have committed, #422
attempts a zero-cost `llm.completion.cancelled.v1` terminal marker; its
persistence failure is logged, so settlement is best-effort, not guaranteed.
The worker drains abandoned submissions to let that cleanup run. The unbounded
drain (#447) and current serial turn worker (#360) remain throughput limits.
See `crates/fused-runtime/tests/turn_cancellation.rs`,
`crates/server/tests/chat_turn_timeout.rs` and the streaming suites.

The task-flow orchestrator validates DAGs, allowlists, depth/fanout and control
flow, but **simulates step results rather than dispatching effectful work**.
Its `execute_parallel` walks branches sequentially. Conditional DAGs are
rejected by validation, so an unreachable conditional execution branch is not
a shipped feature (#352). Proactive scheduling and learning/proposal loops
remain programmatic surfaces rather than a complete operator UI. The
`cite-or-refuse` example skill provides a grounding policy, not proof that a
retrieval backend returned the right material.

Open work includes the security/durability/concurrency remainders above, #357
Qdrant pagination (a late tombstone must still suppress recall), #366 operability
and data-directory consistency, #464 raw OpenAPI duplicates, #458 catalog
search and #460 snapshot ordering. The ambient-provider-key test defect #462
also remains open. These must not be inferred complete from the v0.2.0 tag.
Run deployments in private channels and review these limitations before
assuming public production readiness.
