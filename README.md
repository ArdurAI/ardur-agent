# Ardur

**A personal AI agent with a security substrate built into the wire format** —
Rust, multi-channel, multi-provider. Every action an agent takes is scoped by a
capability token, recorded in a signed receipt chain, and bounded by a cost
gate, so the agent is something you can audit and govern rather than an opaque
process.

## What is Ardur

Ardur is an agent runtime. You reach it from your terminal or a chat app; it
drives a turn through a single fused pipeline that authorizes the request,
checks policy and budget, scans for prompt injection, dispatches to a model
provider (running any tools the model asks for), then mints a signed receipt,
records a bi-temporal memory, and appends to an append-only journal — in that
order, short-circuiting on the first failure.

Three primitives thread through every edge of that pipeline:

- **Cap-tokens** — a scoped, revocable capability (a Biscuit, Ed25519-signed and
  offline-attenuable) authorizes each action. No ambient authority.
- **Receipt-chain** — every turn mints a JWS-ES256 receipt whose `parent_hash`
  links it to the prior one, so the audit trail is tamper-evident and replayable.
- **Cost-tuples** — a four-stage cost-admission gate projects, ceiling-checks,
  reserves, and finalizes the spend of every turn, refunding the difference.

## Quickstart

```sh
# Build and install the `ardur` CLI from the workspace
cargo install --path crates/cli
#   …or, without installing:
cargo build --release && ./target/release/ardur chat

# Talk to it — Anthropic is the default provider
ARDUR_PROVIDER=anthropic ANTHROPIC_API_KEY=sk-ant-... ardur chat

# Local, no API key — point at an Ollama daemon
ARDUR_PROVIDER=ollama ardur chat
```

The same environment variables select the provider, memory backend, channels,
tools, and skills for both the CLI and the server. The full operator runbook —
every provider, the Qdrant/hybrid memory backends, the four chat channels, MCP,
skills, OpenTelemetry, and the HTTP surface — lives in **[RUN.md](RUN.md)**.
For the current implementation inventory, see
**[docs/current-status.md](docs/current-status.md)**.

## Agent Bootstrap

Every LLM or agent session working in this repo must start with the read-only
bootstrap:

```sh
python3 scripts/agent_bootstrap.py
```

It reports the current Linear project/issue status, native project progress,
Git/worktree isolation state, EXTENDED-drive evidence root, `parallel:ready`
work candidates, Plan Corpus verification backlog, Keychain-backed ARD Linear
access status, provider posture, and the no-key smoke-test path. Each
implementation must be merged back to `dev` with required workflows green
before that session moves to another item. The conduct rules live in **[AGENTS.md](AGENTS.md)**
and **[docs/agents/session-code-of-conduct.md](docs/agents/session-code-of-conduct.md)**.

## Capabilities

**Channels (4).** Slack, Matrix, Discord, Telegram — each with bot-token auth,
per-channel allowlists, and self-message echo prevention. Inbound messages on
any channel run through the same fused turn pipeline; replies post back to the
originating channel.

**Providers (6).** Anthropic (Claude), OpenRouter, generic OpenAI-compatible
HTTP endpoints, Ollama (local daemon or hosted cloud), Codex (ChatGPT
subscription via the `codex` CLI), and Claude-CLI (Anthropic subscription via
the `claude` CLI). Selected at boot with `ARDUR_PROVIDER`. Anthropic,
OpenRouter, OpenAI-compatible endpoints, and Ollama stream token-by-token (SSE,
SSE, SSE, NDJSON respectively) through the uniform `Provider::stream` surface.

**Tool execution.** Tools run inside the fused pipeline — cap-token + Cedar
policy + cost gate + injection-defense scan + signed receipt + journal +
memory. The hardened built-in tools (`shell.run`, `file.read`, `file.write`,
`file.list`, `http.fetch`) are implemented and tested with command allowlists,
root-confinement, and SSRF defense, but default server boot currently registers
only the safe local tools (`echo`, `health_check`), optional `voice.transcribe`,
filesystem skills, and remote MCP tools. Default boot wiring for the hardened
built-ins is tracked by `ARD-457`. External tools arrive over MCP via
[`rmcp`](https://crates.io/crates/rmcp) (Ardur is both an MCP client and an MCP
server).

**Memory.** A bi-temporal store (event-time / valid-from-to / invalidation-time)
backed in-process by default, or durably by Qdrant. A hybrid retriever fuses
dense recall (fastembed BGE-small) and sparse recall (Tantivy BM25) with
reciprocal-rank fusion.

**Security substrate.** Biscuit cap-tokens (Ed25519, attenuated, revocable) ·
Cedar policies (Allow / Deny / Indeterminate) · a four-stage cost-admission
gate · a JWS-ES256 signed receipt chain (parent-hash linkage) · prompt-injection
defense · append-only JSONL session journals.

**CLI.** The `ardur` REPL renders streaming Markdown (comrak), syntax-highlighted
code (syntect), tool-call boxes, and a live cost line, with three themes
(`dawn` / `night` / `terminal`) and slash commands. A full-screen Phase-2 TUI is
designed (see [`docs/cli/`](docs/cli/) and ADR-Phase2-021) but not yet built.

**HTTP surface.** `ardur-server` exposes `POST /chat` (one full-pipeline turn,
reply returned), `POST /slack/events` (HMAC-verified webhook), `GET /healthz`,
and an optional bearer-gated MCP surface. Two companion binaries ship alongside:
`ardur-admin`, a read-only observability dashboard over the on-disk journals,
receipts, and memory; and `ardur-eval`, a Tau-Bench-style scenario evaluator
that POSTs to `/chat`.

**Skills.** Filesystem `SKILL.md` skills (YAML frontmatter + Markdown body, with
progressive disclosure via `@./file.md` references) register as tools from
`ARDUR_SKILLS_DIRS`. Ten example skills ship under
[`examples/skills/`](examples/skills/).

## Architecture

A turn is a single `FusedRuntime::submit` call that runs ten stages in order,
short-circuiting on the first failure: **(1)** cap-token verify → **(2)** Cedar
policy → **(3)** cost-gate admit → **(4)** pre-submit hooks, with **(4.5)**
injection-defense scan → **(5)** provider dispatch (looping any tool calls back
through the pipeline) → **(6)** mint + sign the receipt → **(7)** post-receipt
hooks → **(8)** cost-gate finalize/refund → **(9)** record bi-temporal memory →
**(10)** append to the session journal. A failure in stages 1–5 never reaches the
provider's billing; stages 6–10 run after the response and cannot un-happen the
turn. `FusedRuntime::stream` drives the same ten stages but yields a progressive
event feed, which is what the CLI consumes for its streaming UX.

Each subsystem is its own workspace package (47 in all). The design corpus and
architecture-decision records live under [`docs/`](docs/).

## Status

Implementation effort wrapping up **June 2026**. The substrate is
feature-complete and production-shaped, with CI green on `dev` and the no-key
baseline passing locally. It is not yet a turnkey production deployment — see
RUN.md's *Known gaps* and **[docs/current-status.md](docs/current-status.md)** —
so run it in a private channel before exposing it more widely.

## Contributing

Contributions are welcome. See **[CONTRIBUTING.md](CONTRIBUTING.md)** — note that
**DCO sign-off** and **SSH-signed commits** are required. Please also read the
[Code of Conduct](CODE_OF_CONDUCT.md) and the security policy in
[SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — chosen for its
explicit patent grant and enterprise clarity.
