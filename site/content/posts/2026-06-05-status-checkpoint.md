---
title: "Status checkpoint — the implementation effort is wrapping up"
date: 2026-06-05
description: "Where Ardur lands as the build winds down: 32 crates, 71 merged PRs, 600+ tests green, four channels, five providers, and a security substrate wired into every turn."
---

This is a checkpoint, written as the implementation effort for Ardur winds
down. The substrate is feature-complete and production-shaped; what remains is
hardening and the two open durability tickets noted at the end. Here is where it
lands.

## By the numbers

- **32 workspace crates** — one per subsystem, each independently tested.
- **71 merged pull requests** since the initial commit.
- **600+ workspace tests** green, with CI passing on every push.
- **dev HEAD:** `62f18b6`.

## How it got here

The build went substrate-first: the security and observability primitives
landed before anything that could call a model, so every later feature inherited
them for free.

### The security substrate

The load-bearing primitives came first, each as its own crate:

- **Cap-tokens** (`cap-token`) — Biscuit-based, Ed25519-signed, offline-attenuable, revocable.
- **Receipts** (`receipt`) — JWS-ES256 over P-256, chained by `parent_hash` into a tamper-evident log.
- **Cost gate** (`cost-gate`) — a four-stage admission pipeline: project, ceiling-check, reserve, then finalize with a refund.
- **Cedar policy** (`cedar-policy`) — Allow / Deny / Indeterminate, with the principal derived from the verified cap-token rather than asserted by the caller.
- **Injection defense** (`injection-defense`) — an outbound-prompt scanner that can block or sanitize.
- **Session journals** (`session-journals`) — append-only JSONL, the replay source of truth.
- **Lifecycle hooks** (`lifecycle-hooks`) — pre-submit and post-receipt extension points.

### The fused runtime

`FusedRuntime` wires all of the above into a single `submit` call that runs ten
stages in order, short-circuiting on the first failure (#29, with the tool
execution stage in #67). A turn is: cap-token → Cedar → cost-gate → pre-submit
hooks → injection scan → provider → receipt → post-receipt hooks → cost
finalize → memory → journal. `FusedRuntime::stream` (#95) drives the same ten
stages but yields a progressive event feed, so the CLI gets token-by-token
streaming without bypassing the security and audit path.

### Providers (5)

Anthropic (#21), OpenRouter (#52), Ollama (#53), Codex (#54), and Claude-CLI
(#58), selected at boot by `ARDUR_PROVIDER` (#56). Anthropic (SSE, #73),
OpenRouter (SSE, #74), and Ollama (NDJSON, #72) stream through a uniform
`Provider::stream` surface (#83). OpenTelemetry GenAI semantic-convention
attributes ride every provider span (#60).

### Channels (4)

Slack (#28), Matrix (#62), then Discord and Telegram (#70). Each has bot-token
auth, a per-channel allowlist, and self-message echo prevention, and each runs
inbound messages through the same fused pipeline.

### Memory

A bi-temporal store, made durable on Qdrant (#63), with local embeddings
(fastembed) and a Tantivy BM25 index fused by reciprocal-rank fusion (#61). The
`HybridMemoryRetriever` (#66) is selectable at boot (#86).

### Tools and skills

The tool registry (#15) gained an MCP client and server via the official `rmcp`
SDK (#64), filesystem `SKILL.md` skills (#69), built-in `shell.run` / `file.*`
tools with capability gating (#88), and `http.fetch` with SSRF defense (#97).
Nine example skills ship under `examples/skills/` (#87).

### Surfaces

`ardur-server` (#45) exposes the Slack webhook and a generic `POST /chat`
endpoint (#93). Two companion binaries ship alongside it: `ardur-admin` (#75), a
read-only observability dashboard, and `ardur-eval` (#71, #98), a
Tau-Bench-style scenario evaluator. The `ardur` CLI was rewired onto the fused
runtime (#44) and given progressive streaming (#89) and a polished rendering
layer — Markdown, syntax highlighting, tool-call boxes, themes (#96).

## What's next

The substrate is done; the remaining work is operational hardening. Two
durability tickets are open and tracked in `RUN.md`:

- **ARD-17** — the journal-append / receipt-sign commit is still single-phase, so a crash in the window can orphan a receipt.
- **ARD-19** — the runtime's recall side does not yet call the hybrid memory surface, so semantic recall is not wired into the turn path.

Until those land, treat this as dev fidelity rather than a turnkey production
deployment — run it in a private channel first. Everything else is here, tested,
and ready to read.
