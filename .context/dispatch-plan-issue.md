# ardur dispatch — Implementation Plan (governed multi-model orchestration + built-in router)

Date: 2026-09-18 · Track: parallel to #502 governance convergence and #504 TUI · Owner direction: build a Dispatch-class feature (one agent spawning and controlling many sessions across the models already configured in ardur-agent) that beats Claude Dispatch, Claude Code agent teams, Hermes `delegate_task`, and prime-agent — "an open router + dispatch built into ardur-agent" — reusing the hermes-agent and prime-agent surfaces per #501.

Base documents (read first — this plan does not restate them):
- #501 — adoption strategy: wrap prime-agent/hermes-agent, port contracts, credit MIT
- #490 — real provider-backed children: managed lifetime, cancellation, truthful accounting (authorized)
- #410 — steerable subagents contract (Hermes v0.21.0 semantics: list/steer/stop, output schema, per-delegation cost)
- #411 — fallback providers, credential pools, model_overrides
- #502 — three-valued verdicts (`compliant` / `violation` / `insufficient_evidence`)
- #504 — TUI M4 delegation panel (the visibility surface for this feature)

## 0. Verified baseline (dev `a8893e1`)

What already exists and is CI-covered — the plan builds on these, not from scratch:

- `crates/provider-runtime` — object-safe `Provider` trait (complete/stream/rate_card) over 7 backends; `crates/provider-selector::ProviderKind` = anthropic, openrouter, openai-compat, ollama, codex, claude-cli, prime (`ARDUR_PROVIDER`, single pick, no failover).
- `crates/delegate-child` (#516/W4) — `ChildSupervisor<P: Provider + ?Sized>`, `ChildSpec`, `ChildHandle` (cancel), `ParentBudget`/`BudgetReservation` (shared-ledger CAS reserve/settle, observed-rate admission), `TokenAuthority` for attenuated cap-tokens. Not yet wired: `delegate_task` still drives the echo child (#490).
- `crates/delegate-tool` — `delegate_task` registered tool: semaphore admission (`with_max_concurrency`), shared durable deny-list (#489/#494), cap-token verification per child turn.
- `crates/provider-openrouter` — retry policy + circuit breaker exist here but are crate-local, not shared router substrate.
- `crates/provider-prime` (#505) — prime-agent wrapped over RPC mode; `crates/multi-agent` — `CapVerifyingRuntime`, attenuated per-agent tokens, cents_used.
- Governance rails every child turn already crosses: cap-token (+PoP #512), Cedar, cost gate, injection defense, receipt mint, durable deny-list. `crates/dualrun` (B4 #524) — shadow-mode verdict comparison harness.
- Control-plane surfaces that make "dispatch from your phone" free: channel-telegram/discord/matrix/slack + messaging-gateway + approvals gate + server.
- `CostTuple` = tokens_in/tokens_out/cents/wall_ms/milli-attention — richer than any competitor's cost model.
- `crates/config` has NO provider/router keys today (env-only). The router table is new config surface.

## 1. Competitive read — what "beat them" means concretely

| Capability | Claude Dispatch (Cowork RP, 2026-03) | Claude Code desktop (2026-04) | Hermes `delegate_task` | prime-agent | ardur dispatch (this plan) |
| --- | --- | --- | --- | --- | --- |
| Models per dispatch | Claude only | Claude only | parent model unless pinned in config | its configured models | any mix of the 7 provider kinds per task, incl. local ollama |
| Delegation depth | exactly 1 level | 1 (teams experimental) | 1 | 1 | policy depth N enforced by cap-token attenuation chain, not convention |
| Child budget | none (plan quota) | none | none enforced | none | hard per-child ceiling via CAS ledger; per-dispatch envelope; observed-rate admission |
| Result trust | self-report | self-report | explicitly self-report + optional schema | self-report | schema fail-closed + verifier hook → three-valued verdict; `verified` vs `self_report` labeled |
| Audit | none | none | transcripts | session logs | signed receipt per child turn, parent rollup, failovers receipt-marked |
| Kill switch | close app | ctrl-c | stop tool | process kill | revocation propagates to in-flight children, durable across restarts |
| Survives restart | no (app must stay open) | no | no (session-scoped) | daemon mode | durable dispatch journal + settlement store; resume after crash |
| Phone control | proprietary app, Pro/Max only | no | no | no | any configured channel (Telegram/Discord/Matrix/Slack), self-hosted |
| Routing/failover | n/a | n/a | fallback in config | `--models` cycling | in-process router: task-class routing, ordered failover, credential pools, model_overrides, receipt-marked |

Two honest debts to competitors, adopted rather than denied: Hermes defined the managed-handle contract (list/steer/stop + schema retry — #410 ports it, with credit) and prime-agent's RPC/daemon mode is the harness-child transport (#501, with credit). OpenRouter-the-service stays available as one backend; the ardur router differs in kind: it routes *governed work* locally (pre-spend cost-gate projection, receipts, local-model lanes for private data) rather than proxying requests through a third-party cloud.

## 2. Architecture

```
                          ┌──────────────────────────────────────────────┐
 goal ("ship X") ──────►  │ Dispatcher (one governing session)           │
  from CLI / TUI /        │  decompose → TaskSpec DAG-lite (deps, class, │
  channel message         │  workspace, budget, output schema, verifier) │
                          └───────┬──────────────────────────────────────┘
                                  │ per task: route + reserve + attenuate
                                  ▼
              ┌──────────── Model Router (provider-runtime) ─────────────┐
              │ task-class table · ordered failover · credential pools · │
              │ model_overrides · health/circuit state · rate-card cost  │
              └───────┬───────────────────────────────┬──────────────────┘
                      ▼                               ▼
        provider child (in-process)          harness child (subprocess)
        ChildSupervisor over any             prime --mode rpc · hermes -q ·
        Arc<dyn Provider>                    codex/claude-cli
        full fused pipeline per turn         ingress/egress governance +
        (cap-token·Cedar·cost·receipt)       declared-capability manifest
                      │                               │
                      └───────────┬───────────────────┘
                                  ▼
            dispatch journal (durable) · receipts rollup · verdict lane
            list/steer/stop handles · TUI M4 panel · channel delivery
```

Honesty boundary stated up front: a wrapped harness executes tools in its own process, so per-tool Cedar gating cannot be claimed there. Harness children get ingress/egress governance (prompt in, output out, budget, wall-clock, receipts, revocation-kill) plus a declared-capability manifest; anything the harness did internally is labeled as such and can never yield `compliant` without the verifier lane. No competitor even states this boundary; we enforce and display it.

## 3. Milestones (each independently shippable, one worktree/session, full repo gate)

| # | Scope | Acceptance (evidence) | Absorbs |
| --- | --- | --- | --- |
| D0 | Model Router substrate in provider-runtime: `[router]` config table (task-class → ordered provider+model chain), credential pools with 429 cooldown, model_overrides, health/circuit state generalized out of provider-openrouter | failover turn completes on backup and its receipt marks the failover path; pool rotation under injected 429; override table changes cost-gate projection; unknown class falls back to default lane, typed error on empty chain | #411 |
| D1 | Real provider children: `delegate_task` spawns `ChildSupervisor` over routed `Arc<dyn Provider>`; permit lifetime == worker lifetime; action-boundary revocation checks before each provider dispatch; truthful actual-cost settlement | restart with revoked token refuses to resume; cancel during active turn settles actuals + keeps partials; declared-vs-actual divergence test; guard: dropped JoinHandle is not termination | #490 (authorized) |
| D2 | Managed handles: `dispatch.list` / `dispatch.steer` / `dispatch.stop`; steer = parent-authority injection at next round boundary; per-delegation `CostTuple` rolled into parent receipt; output JSON-schema fail-closed with one bounded correction retry, `schema_valid` in result | steer lands mid-run and alters child behavior in a scripted fixture; stop keeps partial + settles; schema-invalid output triggers exactly one retry then typed failure; child output passes injection-defense before entering parent context | #410 |
| D3 | Harness children: `ChildSpec.lane = provider \| harness`; prime-RPC and hermes-CLI adapters behind the same token/budget/receipt/revocation contract; spawn-time capability probe; version-pin guard tests | same dispatch runs one provider child + one prime child + one hermes child, each minting receipts under one parent envelope; kill-by-revocation terminates a harness child; manifest labels out-of-path execution on every harness result | #501 Tier 0/1 |
| D4 | Dispatcher core (new crate `dispatch`): goal → TaskSpec DAG-lite (explicit deps, cycle-refused), routing + reservation per task, bounded global concurrency, durable dispatch journal, resume-after-restart; `ardur dispatch "goal"` / `status` / `steer` / `stop`; depth-N sub-delegation via further attenuation (default 2) | kill -9 mid-dispatch then resume completes remaining tasks without re-running settled ones; depth-3 attempt under depth-2 policy refused by token, not convention; two-task dep chain executes in order; per-dispatch envelope exhaustion halts admission with typed refusal | new |
| D5 | Control surfaces: server endpoints; channel verbs (dispatch/status/steer/stop/approve from Telegram etc. with per-task completion + cost + verdict delivery); TUI M4 panel consumes dispatch state | end-to-end phone story: dispatch from Telegram, approve one gated action, receive receipts + verdicts; TUI panel snapshot tests (per #504 M4 acceptance) | #504 M4 surface |
| D6 | Verified-results lane: per-task verifier hook (artifact checks, command runs, schema+claims cross-check) producing three-valued verdict; `verified` vs `self_report` labeling everywhere results render; dualrun-style shadow scoring of router decisions | a child claiming "tests pass" with a failing artifact yields `violation` with evidence; absent verifier yields `insufficient_evidence` (amber), never green; no code path can synthesize `compliant` from ChildOutcome success | #502 consumer |
| D7 | Long-horizon: cron/standing-goals dispatch with continuity (durable per-job notepad, monitor-mode skip); RLM-style recursive decomposition for big goals (credit prime-agent) | recurring dispatched job dedupes against prior run's notepad; monitor-mode fire with no change spends ~0 and mints the cheap receipt; a >context goal completes via recursive partition with receipts at every level | #409, #501 RLM |

Sequencing constraints (live lanes, do not collide): E4.3 owns fused-runtime and CLI main.rs until its window closes; channel-env (#521/PR #529) also holds main.rs. D0–D3 land in provider-runtime / delegate-* / tool-registry and need neither file. D4's bounded server concurrency defers to the #360 E4 gate; the CLI dispatch path is not blocked by it. D0 ⊥ D1 (parallel lanes). D2 needs D1; D3 needs D1; D4 needs D1+D2 (D3 optional for v1); D5 needs D4; D6 needs D2; D7 needs D4.

## 4. Security invariants (fail-closed; carried from the shipped substrate)

- Children hold attenuated cap-tokens (audience/tool/budget carve-outs, PoP where bound); a child can only narrow, never mint, authority — depth bounds are cryptographic, not advisory.
- Budget: reservations through the shared ledger (CAS), admission against max(declared, worst-observed) per round, release at settlement; no receipt = no committed cost; `OperatorExpense` never enters caller billing.
- Revocation: durable deny-list consulted at action boundaries; in-flight children cancel; state survives restart.
- Steer/stop are parent-authority only; child/harness output is data — injection-defense before it touches parent context; typed denials end-to-end, no reason-string classification, no wildcard match arms on new error enums.
- Verdicts: exactly three values; `insufficient_evidence` is the floor for everything unverified, including every harness-internal action.

## 5. Testing

Per milestone: unit + guard tests proven RED→GREEN→RED-restored (mutation checked against live target text); event-replay fixtures for dispatcher state (same discipline as TUI M0's frozen records); restart/cancel/revocation lifecycle proofs with real child processes for D3 (no provider credentials in CI — stub RPC peers that exit when the real peer would); cost tests assert dispatch/bill COUNTS, not just terminal outcomes; full 10-command repo gate before every PR.

## 6. Risks

- #360 single-thread server worker caps real server-side parallelism until E4 lands — CLI dispatch ships first; server concurrency inherits the E4 gate. 
- Cost runaway (N children × failover chains) — envelope + ceilings + observed-rate admission + revocation kill; D6 shadow-scores router spend.
- Two children mutating one repo — default workspace scoping = worktree-per-task for repo tasks; collision policy in TaskSpec; merge reconciliation deferred with a named issue.
- Harness CLI drift (prime/hermes flag changes) — spawn-time probe + pinned-version guard tests, same pattern as toolchain pins.
- model_overrides staleness is operator-owned metadata — documented, receipt shows the table version used.

## 7. Process + tracker mapping

One milestone per isolated worktree through the full promotion gate (beads item per milestone, gstack review discipline, PRs → dev). #411/#490/#410 keep their issues (implementation detail lives there; this plan is the integration contract). #501 Tier 0/1 items land here as D3/D7. #504 M4 consumes D5 rather than duplicating it. #150's kanban dispatcher becomes a future consumer of the dispatch crate, not a parallel implementation. MIT credit per #501 mechanics (THIRD-PARTY.md + crate READMEs) for every ported contract.

## GSTACK REVIEW REPORT

Design-review discipline pass (self-review, single author; owner is the gate).

| Section | Findings | Disposition |
| --- | --- | --- |
| Scope challenge | D-numbering could have hidden that #490/#410/#411 already own 3 milestones — motivating quote: #490 "Replace stale empty-deny intro; … leave real-child/cancel requirements open" [P1, conf 9/10] | FIXED: absorption table in §3/§7; existing issues stay canonical for their slices |
| Architecture | Per-tool governance is physically out-of-path for harness children — "in-path enforcement is blind to out-of-path actions" [P1, conf 9/10] | FIXED: §2 honesty boundary; harness results floor at `insufficient_evidence` without the D6 verifier |
| Architecture | Server-side parallel dispatch is capped by #360's serialized worker [P2, conf 8/10] | FIXED: D4 ships CLI-first; server concurrency explicitly gated on E4 |
| Cost | Failover chains multiply worst-case spend per child [P2, conf 8/10] | FIXED: §4 admission rule + D0 acceptance includes projection under override table |
| Tests | Restart/resume proofs need real process kills, not dropped futures — #490: "dropping a JoinHandle is not stopping a worker" [P2, conf 9/10] | FIXED: D1/D4 acceptance name kill/restart evidence explicitly |
| Outside voice | Skipped (single-author plan); cross-model design review recommended before D4 (dispatcher decomposition is a product claim) | Recorded as limitation |

VERDICT: PASS WITH FINDINGS — plan is executable; D0 and D1 are the parallel keystones; D6 is the differentiation the market lacks.

**UNRESOLVED DECISIONS:**
- None blocking. Deferred-by-name: merge reconciliation across children (future issue at D4), server-side bounded concurrency (#360 E4), mobile-app-style push beyond existing channels (not planned — channels are the surface).
