---
title: Ardur Agent high-assurance personal-agent roadmap synthesis
date: 2026-06-16
project: Ardur Agent
repo: ArdurAI/ardur-agent
status: durable roadmap note
kanban_task: t_dc5709e6
parent_synthesis_task: t_6b60645a
source_tasks:
  - t_ffa69171
  - t_59c0d71b
  - t_b12b8d1a
tags:
  - ardur-agent
  - roadmap
  - personal-agent
  - high-assurance
  - product-strategy
related_notes:
  - world-class-personal-agent-roadmap-2026-06-16
  - top-secret-gate-review-2026-06-16
  - deep-issue-review-2026-06-16
  - dev-ci-review-2026-06-16
---
# Ardur Agent high-assurance personal-agent roadmap synthesis — 2026-06-16

> Standalone durable roadmap note for Obsidian and repository readers. It synthesizes the feature baseline, persona/value matrix, and high-assurance gate review into one product roadmap. It intentionally preserves source paths, GitHub issue links, gate IDs, and task IDs so the note remains auditable after the Kanban card is closed.

**Recommended durable path:** `Hermes/Projects/Ardur Agent/roadmap-synthesis-2026-06-16.md` in the Obsidian vault.  
**Repository copy:** `docs/roadmap/high-assurance-personal-agent-roadmap-2026-06-16.md`.

## 1. Executive summary — selective parity, trust-first differentiation

Ardur Agent should **not replicate Hermes Agent and OpenClaw one-to-one**. It should **selectively replicate the capability classes that are now table stakes for world-class personal agents** — durable background work, memory/skills, safe tools/MCP, guided onboarding, a small number of polished channels, source-connected workflows, evals/observability, and multi-surface continuity — while **differentiating through visible high-assurance trust infrastructure**: authenticated/default-deny ingress, cap-token/Cedar tool authorization, exact cost settlement, signed/durable receipts, scoped memory, source/provenance, and approval-first automation.

One-to-one cloning would be harmful because Hermes' broad tool/gateway catalog and OpenClaw's broad channel/voice/companion surface multiply security, reliability, support, and product-focus risk. Ardur's wedge is not channel count or raw tool count; it is a personal/enterprise agent runtime whose safety, cost, memory, and evidence boundaries are understandable to non-experts and inspectable by experts.

The roadmap therefore uses:

- **Capability parity** where users will expect it: scheduled/background runs, memory, tools/MCP, connectors, guided setup, approvals, traces/evals, and channels.
- **Implementation differentiation** where Ardur's substrate is stronger: receipts, cap tokens, Cedar policy, cost gates, auditable runs, scoped memory, and policy-scoped connectors.
- **Deliberate deferral** for high-surface/high-risk features: ambient desktop control, voice/mobile companions, raw marketplace breadth, and long-tail chat adapters.

The first public product story should be: **connect a small number of tools, run a guided workflow, see the plan and risk, approve or dry-run, inspect sources/memory/cost/receipts, and repeat safely**.

## 2. Sourced baseline and source base used

- `t_ffa69171` — `/Users/gnutakki16/.hermes/kanban/boards/ardur-agent-roadmap/workspaces/t_ffa69171/ardur-agent-feature-baseline.md` — Hermes/OpenClaw/current-market baseline, parity bars, and one-to-one replication risks.
- `t_59c0d71b` — `/Users/gnutakki16/.hermes/kanban/boards/ardur-agent-roadmap/workspaces/t_59c0d71b/persona-use-case-value-gaps.md` — persona value gaps, adoption blockers, and measurable value signals.
- `t_b12b8d1a` — `/Users/gnutakki16/.hermes/kanban/boards/ardur-agent-roadmap/workspaces/t_b12b8d1a/high_assurance_gates_issues_120_133.md` — high-assurance issue gates #120-#133 and production go/no-go rules.
- `t_b12b8d1a` — `/Users/gnutakki16/.hermes/kanban/boards/ardur-agent-roadmap/workspaces/t_b12b8d1a/feature_gate_rubric.json` — machine-readable gate rubric used for feature gate tags.
- Obsidian note — `/Users/gnutakki16/Documents/Obsidian Vault/Hermes/Projects/Ardur Agent/top-secret-gate-review-2026-06-16.md` — original high-assurance ordering and verification facts.
- Obsidian note — `/Users/gnutakki16/Documents/Obsidian Vault/Hermes/Projects/Ardur Agent/deep-issue-review-2026-06-16.md` — underlying issue evidence and suggested fix ordering.

Confidence note: this is a synthesis of the parent research and local gate notes. It does not claim fresh live competitive data beyond those artifacts.

This roadmap is grounded in the repo, competitive baselines, persona research, and high-assurance issue gates available on 2026-06-16.

| Baseline source | What it contributes | Roadmap consequence |
|---|---|---|
| Ardur repo (`ArdurAI/ardur-agent`) | Rust workspace; CLI/HTTP/chat surfaces; Anthropic/OpenRouter/OpenAI-compatible/Ollama/Codex/Claude providers; cap-token authorization; Cedar policy; cost gate; prompt-injection scan; tool/MCP loop; ES256 receipts; append-only journals; bi-temporal memory with optional Qdrant/hybrid retrieval; `ardur-eval` and OTel spans. Known gaps include ARD-17 receipt/orphan durability and ARD-19 runtime memory recall wiring. | Ardur's wedge is not generic chat. The roadmap must make trust, cost, memory, and receipt infrastructure visible and enforced before broad autonomy claims. |
| Hermes Agent | Strong baseline for skills/procedural memory, persistent memory/session search, Kanban-style multi-agent orchestration, cron/background jobs with delivery, gateway/chat operation, MCP/tools, and operational deployment UX. | Replicate the user jobs: durable runs, memory, safe tools, scheduling, handoffs, delivery, and setup/doctor. Do not clone Hermes profiles/Kanban internals one-to-one. |
| OpenClaw | Strong baseline for assistant-in-existing-channels, broad channel coverage, local-first gateway, onboarding/doctor, DM pairing/allowlists, companion node mindset, sandboxing non-main sessions. | Make Discord/Telegram/CLI/HTTP excellent and safe first. Defer broad channels, voice, mobile, desktop nodes, and canvas until core trust loops are proven. |
| Broader market | ChatGPT Tasks/apps/connectors, Claude Code/Codex terminal/cloud/IDE flows, MCP, NotebookLM-style source grounding, OpenAI/Anthropic computer-use harnesses, OpenAI Agents SDK tracing/evals. | Users expect scheduled/background agency, connected tools, sources, evals/traces, and multi-surface continuity. Browser/computer control is valuable but must be sandboxed and approval-gated. |
| Persona/value research | Non-technical users, technical users, teachers, students, architects, sales, integrators, creators, and YouTubers all need safe first wins, scoped memory, approvals, source grounding, and role workflows. | Prioritize cross-persona trust infrastructure and repeatable role workflows over shallow feature breadth. |
| Gate review / GitHub issues #120-#133 | Public/beta claims are blocked unless security, auth, tool authorization, cost settlement, receipt durability, secret handling, supply chain, admin, channel, and runbook gates are enforced with tests/evidence. | Phase 0 is mandatory. Features depending on weak gates stay draft/beta/internal until the relevant gate score reaches the required threshold. |

### Related durable notes

- [[world-class-personal-agent-roadmap-2026-06-16]] — broad world-class personal-agent roadmap research.
- [[top-secret-gate-review-2026-06-16]] — strict high-assurance issue gate review and GitHub issue list.
- [[deep-issue-review-2026-06-16]] — underlying issue evidence and suggested fix ordering.
- [[dev-ci-review-2026-06-16]] — CI/dev branch context feeding the gate posture.

### Gate issue references

| Gate | GitHub issue | Short name |
|---|---|---|
| G120 | https://github.com/ArdurAI/ardur-agent/issues/120 | Branch/review/deploy rulesets |
| G121 | https://github.com/ArdurAI/ardur-agent/issues/121 | Secret scanning and dependency security |
| G122 | https://github.com/ArdurAI/ardur-agent/issues/122 | Auth before live agent admission; default-deny ingress |
| G123 | https://github.com/ArdurAI/ardur-agent/issues/123 | Skill/filesystem resource confinement |
| G124 | https://github.com/ArdurAI/ardur-agent/issues/124 | Cap-token/Cedar/tool/MCP authorization |
| G125 | https://github.com/ArdurAI/ardur-agent/issues/125 | Cost settlement fail-closed |
| G126 | https://github.com/ArdurAI/ardur-agent/issues/126 | Private/authenticated admin surfaces |
| G127 | https://github.com/ArdurAI/ardur-agent/issues/127 | Chat adapter allowlists and default-deny channel ingress |
| G128 | https://github.com/ArdurAI/ardur-agent/issues/128 | Missing policy fails closed |
| G129 | https://github.com/ArdurAI/ardur-agent/issues/129 | Streaming cost accuracy |
| G130 | https://github.com/ArdurAI/ardur-agent/issues/130 | Receipt durability and boot reconciliation |
| G131 | https://github.com/ArdurAI/ardur-agent/issues/131 | Secret redaction and safe bearer-token endpoint validation |
| G132 | https://github.com/ArdurAI/ardur-agent/issues/132 | Pinned supply chain, scans, SBOM/provenance |
| G133 | https://github.com/ArdurAI/ardur-agent/issues/133 | Site/Docker/runbook validation |


## 3. Prioritization model

Features are prioritized by five criteria:

1. **Cross-persona activation/value**: does this unlock first success, repeat use, or willingness to trust delegation across several personas?
2. **Safety and launch blocking**: does this close a high-assurance issue gate or prevent unsafe spend, data exposure, tool execution, or audit divergence?
3. **Parity necessity**: is this capability now table stakes because Hermes, OpenClaw, Codex/Claude/ChatGPT, or connector ecosystems trained users to expect it?
4. **Ardur differentiation**: does the feature turn Ardur's cap-token/Cedar/cost/receipt/memory substrate into visible product trust?
5. **Dependency leverage**: does the feature unblock many later workflows, connectors, personas, or ops gates?

Priority meanings:

- **P0 / release-blocking**: must be closed before public beta, production, or high-assurance claims.
- **P0 / MVP**: needed for the first credible trusted-delegate product loop.
- **P1 / beta**: high-value persona or integration depth after the trust loop works.
- **P1 / GA**: required for repeatable self-hosted/partner operations.
- **P2 / strategic**: powerful expansion after the product has proven trust, eval, and ops maturity.

## 4. High-assurance operational and eval gate rubric

Gate score meanings follow the preserved rubric:

- **0** = not addressed, unknown, or trust/convention only.
- **1** = documented or partially implemented but not automatically enforced and lacking negative tests.
- **2** = implemented and tested, but enforcement is incomplete, manual, or missing live configuration evidence.
- **3** = enforced by code/CI/ruleset/deployment/runtime policy, covered by positive and negative tests, and backed by audit or live configuration evidence.

Launch rules:

- Any applicable P0 gate from #120-#125 scoring below 3 blocks production/public-beta use of affected features.
- Any applicable P1 gate from #126-#133 scoring below 2 blocks production unless an equivalent enforced compensating control is approved and audited; target is 3.
- ARD-17/orphan receipt durability and boot reconciliation must be closed before claiming secure/auditable agent turns.
- ARD-19 memory recall wiring must be closed before claiming Hermes-level or market-level memory parity.
- No broad channel, public HTTP, admin, tool/MCP, filesystem, cost, or connector expansion may ship without negative tests and live config/CI evidence.

Gate legend:

- **G120**: branch/review/deploy rulesets.
- **G121**: repo secret scanning and dependency security.
- **G122**: auth before live agent admission; private/default-deny ingress.
- **G123**: skill/filesystem resource confinement.
- **G124**: cap-token/Cedar/tool/MCP authorization before every invocation.
- **G125**: cost settlement fail-closed.
- **G126**: private/authenticated admin surfaces.
- **G127**: chat adapter allowlists and channel ingress default-deny.
- **G128**: missing policy fails closed.
- **G129**: streaming cost accuracy; no silent zero.
- **G130**: receipt durability and boot reconciliation.
- **G131**: secret redaction and safe bearer-token endpoint validation.
- **G132**: pinned supply chain, scans, SBOM/provenance.
- **G133**: site/Docker/runbook validation.

## 5. Persona value mapping

| Persona | Primary value need | Most relevant roadmap features | Measurable value signals |
|---|---|---|---|
| Non-technical users | First safe delegation, life admin, document/screenshot help, confidence before external sends | F04, F05, F06, F09, F13, F25, F26 | Time to first success, approval confidence, support interventions, weekly repeated automations |
| Technical users / developers | Repo-safe coding, debugging, scheduled monitors, reproducible evidence | F02, F03, F08, F10, F11, F12, F15, F27 | Review acceptance, tests per diff, MTTR, unsafe-command denials, cost per completed task |
| Teachers | Lesson/rubric prep, feedback drafts, privacy-safe class memory, sources | F04, F05, F06, F07, F14, F17, F20 | Prep hours saved, rubric agreement, source completeness, privacy incidents, LMS export success |
| Students | Socratic tutoring, study planning, cited research, proof of integrity | F06, F07, F09, F14, F17, F20, F26 | Quiz improvement, deadline adherence, tutor-mode usage, citation quality, proof-of-process exports |
| Architects | Evidence-backed ADRs, threat models, roadmap gate checks, diagrams | F00, F01, F02, F03, F06, F07, F12, F16, F20 | ADR cycle time, evidence coverage, security gate pass rate, stakeholder acceptance |
| Sales | Account briefs, grounded outreach drafts, CRM updates, follow-up commitments | F04, F05, F07, F09, F11, F13, F14, F18 | Brief time saved, CRM completion, follow-up latency, approval edits, hallucinated-claim corrections |
| Integrators / implementation partners | Connector build/simulation/deploy/debug, tenant safety, client handoff | F00, F01, F02, F03, F11, F12, F21, F22, F24, F27 | Connector time-to-first-success, secret-leak tests, MTTR, certified connectors, upgrade breakage |
| Creators | Research, voice-preserving drafts, asset packages, approval-only publishing | F04, F05, F06, F07, F09, F14, F19, F26, F28 | Draft acceptance/edit distance, source completeness, approval rate, content calendar adherence |
| YouTubers | Topic research, script outlines, packaging, analytics feedback, clip workflows | F07, F09, F11, F14, F19, F25, F26 | Topic-to-outline time, fact corrections, upload package completion, CTR/retention hypotheses |

## 6. Parity decision — selective capability parity, not one-to-one cloning

| Capability area | Recommendation | Rationale / implementation direction |
|---|---|---|
| Durable work/schedules | Replicate the capability, not Hermes' exact Kanban implementation | Build Ardur Runs: receipt-backed run ledger with schedules, triggers, retries, blockers, budgets, and projections to boards/Linear/GitHub. |
| Memory and skills | Partially replicate, then differentiate | Close ARD-19 recall wiring; expose scoped memory cards, sources, validity intervals, and skill/pack provenance rather than copying a flat memory/skill UX. |
| Gateway/channels | Selective parity only | Make CLI/HTTP/Discord/Telegram excellent with allowlists and pairing. Do not chase OpenClaw's 20+ channels before safety/reliability. |
| Tools/MCP/connectors | Selective parity with stricter governance | Offer fewer default connectors with capability manifests, Cedar/cap-token checks, secret handling, evals, and receipts. Avoid raw tool-count races. |
| Onboarding/doctor | Replicate as a core product feature | OpenClaw/Hermes-style guided setup/status is table stakes, but Ardur should include policy/gate/receipt diagnostics in plain language. |
| Approvals/receipts/costs | Differentiate aggressively | Make trust center the product: plans, approvals, receipts, cost, memory, revoke/export/delete, and evidence bundles visible to non-experts and inspectable by experts. |
| Voice/mobile/desktop companions | Delay and selectively replicate later | Not needed for MVP; add after core trust loop, pairing, channel safety, and memory/receipt UX are stable. |
| Browser/computer control | Delay and sandbox only | Do not expose ambient host control. Build isolated harness with allowlisted actions/sites, human confirmation, screenshots, and receipts. |
| Brand/visual polish | Differentiate with truthful status, not cutesy clones | The CLI/TUI should surface tool feed, memory, policy, cost, receipts, and source evidence with high clarity. |

## 7. Phased roadmap overview

### Phase 0 — Close high-assurance gates before public/beta claims

Goal: no feature should be sold as public beta/production until relevant P0 gates are enforced by code, CI/rulesets, deployment policy, runtime config validation, or audited approval workflow, with negative tests and live evidence.

- **F00** Enforced repo/security/CI/deploy/runbook gate baseline.
- **F01** Authenticated, default-deny ingress for HTTP/admin/chat adapters.
- **F02** Tool, skill, and MCP capability firewall.
- **F03** Exact cost settlement, receipt durability, and boot reconciliation.

### Phase 1 — MVP trusted delegate

Goal: prove the core Ardur thesis. A user can safely delegate real work through CLI/HTTP plus a small connector set, see permissions/receipts/memory/costs, schedule/background a run, and trust source-grounded outputs.

- **F04** Guided onboarding, doctor, and first-win task gallery.
- **F05** Trust center for permissions, approvals, receipts, cost, memory, and revocation.
- **F06** Scoped role/project workspaces with visible bi-temporal memory recall.
- **F07** Source/provenance layer for factual claims and generated work.
- **F08** Ardur Runs: durable scheduled/event/delegated run ledger.
- **F09** Dry-run, draft, approve, execute-with-policy automation modes.
- **F10** Rich CLI/TUI with live tool feed, memory pane, cost, and receipt-chain tail.
- **F11** Curated core connector pack with policy manifests.
- **F12** Workflow eval and observability harness as product infrastructure.

### Phase 2 — Beta persona workflows and integration depth

Goal: turn the generic trusted delegate into repeatable high-value workflows for the most promising personas and make Discord/Telegram plus collaboration/review queues useful in daily work.

- **F13** Polished Discord/Telegram gateway and notification delivery.
- **F14** Role-specific workflow templates and reusable packs.
- **F15** Developer/repo engineering workflow.
- **F16** Architect decision, ADR, diagram, and threat-model workflow.
- **F17** Teacher/student learning and integrity workflows.
- **F18** Sales/account intelligence and CRM workflow.
- **F19** Creator and YouTube production workflow.
- **F20** Collaboration, shared workspaces, review queues, and handoffs.

### Phase 3 — GA/self-hosted operations and ecosystem

Goal: make Ardur operable and extensible for production/self-hosted users and implementation partners.

- **F21** Admin/ops control plane, installers, doctor, backups, and upgrades.
- **F22** Connector SDK, certification, and safe marketplace foundations.
- **F23** Cost governance, provider routing, fallback, and budget recipes.
- **F24** Production deployment profiles: local-first, Docker, and controlled hosted options.

### Phase 4 — Strategic expansion after trust loop is proven

Goal: add high-risk/high-surface area capabilities only when the trust, policy, eval, and ops foundations are proven in earlier phases.

- **F25** Sandboxed browser/computer-control harness.
- **F26** Voice, mobile, desktop companions, and lightweight nodes.
- **F27** Multi-agent teams and external board projections.
- **F28** Versioned skill/agent marketplace and policy-reviewed pack ecosystem.

## 8. Prioritized feature catalog

### F00 — Enforced repo, security, CI, deploy, and runbook gate baseline

- **Phase / priority:** Phase 0 — P0 / release-blocking.
- **User value:** Users, integrators, and operators can trust that Ardur releases are reviewed, reproducible, scanned, and not shipped through convention-only controls.
- **Target personas:** Architects, integrators, technical users, all downstream users.
- **Parity/source inspiration:** Differentiation, not Hermes/OpenClaw clone. Hermes/OpenClaw establish ops maturity expectations; Ardur should exceed them with auditable high-assurance gates.
- **Implementation complexity:** M.
- **Dependencies and risks:** Requires GitHub repo/admin configuration evidence, protected environments, CI workflow changes, and operator discipline. No feature should claim production readiness while any relevant P0 gate is below score 3.
- **Operational gates:** G120, G121, G132, G133.
- **Safety / ops / eval requirements:** Live branch/ruleset evidence; secret scanning and push protection; dependency review/RustSec/CodeQL/container scan; SBOM/provenance/signing; site/Docker/runbook smoke tests.
- **Measurable success criteria:** main/dev cannot merge or deploy with failing/missing required checks; secret scanning/push protection/dependency alerts/local guard are active; CI validates Rust, site, Docker, runbook commands, scans, SBOM/provenance, and signing before release.

### F01 — Authenticated, default-deny ingress for HTTP/admin/chat adapters

- **Phase / priority:** Phase 0 — P0 / release-blocking.
- **User value:** Prevents arbitrary network callers or channel users from spending budget, writing memory/journals, or driving tools through the live runtime.
- **Target personas:** All, especially non-technical users, integrators, architects.
- **Parity/source inspiration:** OpenClaw DM pairing/allowlists and Hermes gateway controls are parity bars; Ardur differentiates by propagating authenticated identity into cap-token, Cedar, budget, audit, and receipts.
- **Implementation complexity:** M.
- **Dependencies and risks:** May break demo convenience. Must separate explicit loopback/dev bypasses from production defaults; public mode must be opt-in and strongly authenticated.
- **Operational gates:** G122, G126, G127, G128.
- **Safety / ops / eval requirements:** Unauthenticated `/chat` 401/403 tests; private bind by default; adapter allowlist negative tests; admin auth/private tests; missing Cedar policy boot failure tests; rate/size limits before expensive work.
- **Measurable success criteria:** No unauthenticated network request can trigger provider spend/tools/memory/journal/receipt writes; admin starts private/auth-required by default; Discord/Telegram/Matrix adapters drop out-of-allowlist messages before runtime admission.

### F02 — Tool, skill, and MCP capability firewall

- **Phase / priority:** Phase 0 — P0 / release-blocking.
- **User value:** Lets users connect useful tools without letting model-selected tool names or skill paths become the authorization boundary.
- **Target personas:** Technical users, integrators, architects, all users using connected tools.
- **Parity/source inspiration:** Hermes and OpenClaw set expectations for broad tool/MCP ecosystems. Ardur should selectively match tool categories but differentiate with per-tool cap-token/Cedar policy, skill confinement, and auditable denial receipts.
- **Implementation complexity:** L.
- **Dependencies and risks:** Requires central invocation enforcement in runtime and MCP server paths, declared capability manifests for remote MCP, canonical skill path checks, and explicit policy source. High regression risk if tools bypass the central registry.
- **Operational gates:** G123, G124, G128, G131.
- **Safety / ops / eval requirements:** Negative tests for chat-only token attempting shell/files/http/MCP; streaming and non-streaming invocation checks; traversal/absolute/symlink skill expansion tests; bearer endpoint validation and redacted Debug tests.
- **Measurable success criteria:** Every advertised and invoked tool is filtered and re-checked against authenticated identity, cap-token grants, Cedar, budget, and deployment policy; remote MCP tools cannot run without operator-declared capabilities and bounded identity/budget context; skill resource expansion cannot read outside declared skill directories.

### F03 — Exact cost settlement, receipt durability, and boot reconciliation

- **Phase / priority:** Phase 0 — P0 / release-blocking.
- **User value:** Makes the core promise of an auditable personal/enterprise agent true: successful turns have settled cost and durable receipts, or they fail closed with recoverable audit state.
- **Target personas:** All, especially architects, integrators, technical users.
- **Parity/source inspiration:** Differentiator. Hermes/OpenClaw have logs and gateway state; Ardur's unique wedge is receipt/cost/journal integrity as product trust infrastructure.
- **Implementation complexity:** L.
- **Dependencies and risks:** Launch-blocking for secure/auditable claims. Streaming paths and slow provider/tool turns are easy to under-account; durability failures and orphan receipts must be reconciled before accepting new turns.
- **Operational gates:** G125, G129, G130.
- **Safety / ops / eval requirements:** TTL-expiry/finalization failure tests; streamed OpenRouter/OpenAI-compatible cost tests; unwritable receipt-log integration tests; orphan receipt boot reconciliation; chain-tail ordering proof.
- **Measurable success criteria:** A turn cannot return success with unsettled/silent-zero cost or missing durable receipt; server boot reconciles journals/memory/cost/receipt state before accepting turns; admin/CLI can show provider/tool/combined cost and receipt chain status for each turn.

### F04 — Guided onboarding, doctor, and first-win task gallery

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Turns a secure substrate into a product: users pick a role, connect only the minimum needed integration, run a safe sample workflow, and know what passed/failed before trusting the agent.
- **Target personas:** Non-technical users, teachers, sales, creators, YouTubers, technical users.
- **Parity/source inspiration:** Selective parity with OpenClaw onboarding/doctor and Hermes setup/status. Differentiate by making high-assurance checks understandable instead of hiding them.
- **Implementation complexity:** M.
- **Dependencies and risks:** Depends on F00-F03 gate evidence and clear persona templates. Risk: exposing too many security concepts too early; solve with progressive disclosure and plain-language statuses.
- **Operational gates:** G122, G126, G127, G133.
- **Safety / ops / eval requirements:** Setup wizard must never enable unsafe public ingress by default; doctor checks auth/bind/allowlist/policy/runbook state; first-win workflows run in dry-run/draft mode unless explicitly approved.
- **Measurable success criteria:** Median time to first successful delegated task under a target threshold; setup completion and first-run success tracked per role/template; support tickets and unsafe configuration attempts decrease over successive onboarding revisions.

### F05 — Trust center: permissions, approvals, receipts, cost, memory, and revocation

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Users can see what the agent can access, why it asks for permission, what it plans to do, what it actually did, what it cost, what it remembered, and how to revoke or correct it.
- **Target personas:** All.
- **Parity/source inspiration:** Partial parity with Hermes approvals/gateway visibility and OpenClaw security defaults; differentiation is human-readable signed receipts, capability scopes, and memory cards as a first-class UI.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F01-F03. Hard UX problem: non-technical users need simple language; experts need trace, policy, and receipt detail. Avoid treating receipts as developer-only logs.
- **Operational gates:** G124, G125, G126, G130, G131.
- **Safety / ops / eval requirements:** Approval receipts for risky actions; revoke/export/delete memory controls; generic client errors with sensitive detail only in logs; redacted secrets in all views; audit lookup tests.
- **Measurable success criteria:** Users can answer what access was granted and revoke it without support; approval confidence score improves across onboarding cohorts; audit lookup succeeds for at least 95% of sampled successful runs and risky denials.

### F06 — Scoped role/project workspaces with visible bi-temporal memory recall

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** The agent becomes useful over time while keeping household, class, course, repo, client, account, channel, and creator memory separated, inspectable, correctable, exportable, and deletable.
- **Target personas:** All.
- **Parity/source inspiration:** Selective parity with Hermes memory/session recall and OpenClaw workspace skills; Ardur differentiates with bi-temporal memory, source labels, validity intervals, and receipt-linked influence traces.
- **Implementation complexity:** L.
- **Dependencies and risks:** ARD-19 recall wiring is a launch blocker for memory claims. Must prevent context bleed between workspaces and avoid hidden personalization.
- **Operational gates:** G122, G123, G124, G126, G130, G131.
- **Safety / ops / eval requirements:** Tenant/workspace isolation tests; recall path tests; memory influence trace in receipt/admin; memory correction/deletion tests; no secret leakage in memory cards or debug output.
- **Measurable success criteria:** Repeat-task success improves after accepted memory suggestions; memory correction rate and context-bleed incidents stay below thresholds; every memory used in an answer exposes source, workspace, validity, and receipt/run provenance.

### F07 — Source/provenance layer for factual claims and generated work

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Users can trust, check, and reuse outputs because factual claims show source, timestamp, confidence, and whether they were user-provided, retrieved, inferred, or generated.
- **Target personas:** Teachers, students, architects, sales, creators, YouTubers, non-technical users.
- **Parity/source inspiration:** Broader market parity with NotebookLM and ChatGPT deep research/source-connected apps. Ardur should integrate provenance with receipts and memory, not bolt on citations as decoration.
- **Implementation complexity:** M.
- **Dependencies and risks:** Requires source registry, citation schema, retrieval integration, and UI affordances. Risk: false confidence labels; citations must be verified by retrieval/source trace, not model assertion.
- **Operational gates:** G124, G126, G130.
- **Safety / ops / eval requirements:** Citation coverage evals; fact-check spot checks by workflow; source freshness labels; provenance fields in receipts; redacted source snippets for sensitive connectors.
- **Measurable success criteria:** At least 90% of factual claims in supported workflows carry a source/provenance marker; correction rate and hallucinated-claim corrections decline; source inspection/click-through is tracked for research-heavy workflows.

### F08 — Ardur Runs: durable scheduled/event/delegated run ledger

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Users can ask Ardur to run later, repeat, monitor, retry, hand off for approval, and deliver results without needing an always-open chat, while every run remains budgeted and auditable.
- **Target personas:** All, especially technical users, integrators, sales, creators, YouTubers.
- **Parity/source inspiration:** Selective parity with Hermes Cron/Kanban and OpenClaw tasks/cron. Do not clone Hermes Kanban one-to-one; build a receipt-backed run ledger that can project into boards, Linear, GitHub Issues, or channel messages.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F01-F03, queue/scheduler, idempotency, budget reservations, and delivery semantics. Risk: autonomous loops without turn/time/cost budgets.
- **Operational gates:** G122, G124, G125, G127, G128, G130.
- **Safety / ops / eval requirements:** Run budget and retry limits; schedule/event trigger auth; durable run state; blocker/approval state; delivery allowlists; no-agent/script mode only if sandboxed and auditable.
- **Measurable success criteria:** Recurring and one-shot runs complete with durable run row, receipt chain, cost, and delivery status; failed runs expose retry/blocker reason without losing audit state; budget overruns and unbounded recurrence are impossible by default.

### F09 — Dry-run, draft, approve, execute-with-policy automation modes

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Users get useful leverage before full trust: explain-only for learning, dry-run/draft for review, and policy-bounded execution for low-risk repeated tasks.
- **Target personas:** All, especially non-technical users, sales, creators, teachers, technical users, integrators.
- **Parity/source inspiration:** Partial parity with Hermes command approvals and market computer-use safety guidance. Ardur differentiates by tying modes to cap-token scopes, budgets, receipts, and reversible action policies.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F05, F08, connector-specific undo/draft support, and good default policy recipes. Risk: users over-approve broad policies; require narrow scopes and visible receipts.
- **Operational gates:** G124, G125, G127, G130.
- **Safety / ops / eval requirements:** Risk taxonomy; approval receipts; negative tests for external sends/purchases/destructive actions; rollback/outbox checks where supported; human confirmation for meaningful external consequences.
- **Measurable success criteria:** High-impact actions are never executed without explicit approval or narrow pre-approved policy; automation completion rate rises while incident rate stays below threshold; approval edits/cancellations inform template and policy improvements.

### F10 — Rich CLI/TUI with live tool feed, memory pane, cost, and receipt-chain tail

- **Phase / priority:** Phase 1 — P0 / MVP.
- **User value:** Technical and professional users can see exactly what the agent is doing, debug failures, inspect memory/provenance, and trust long-running tasks without leaving the terminal.
- **Target personas:** Technical users, architects, integrators, technical creators.
- **Parity/source inspiration:** Selective parity with Hermes status/task UX, OpenClaw TUI, Codex/Claude Code terminal UX. Differentiation: truthful receipt/memory/cost panes instead of tool-count or whimsical branding.
- **Implementation complexity:** M.
- **Dependencies and risks:** Depends on F03, F05, F06, F08, and local ADR renderer work. Risk: rendering polish before substrate truth; prioritize accurate state over animation.
- **Operational gates:** G126, G130, G133.
- **Safety / ops / eval requirements:** No secrets in panes; admin data auth/private if exposed beyond local CLI; terminal command examples smoke-tested; state shown must match receipt/run ledger.
- **Measurable success criteria:** Users can inspect tool calls, approvals, memory snippets, cost, and receipt status from one terminal session; mean time to diagnose failed runs decreases; TUI state and run/receipt ledger agree in sampled audits.

### F11 — Curated core connector pack with policy manifests

- **Phase / priority:** Phase 1 — P0/P1 / MVP-to-beta.
- **User value:** Ardur proves daily utility with a small set of high-value integrations instead of a brittle raw tool-count race.
- **Target personas:** Technical users, architects, integrators, creators, students, teachers, sales.
- **Parity/source inspiration:** Partial parity with Hermes broad tools/MCP and OpenClaw channels/nodes. Selective approach: local files/shell, HTTP/web fetch, GitHub, Obsidian/Notion, Google/Microsoft docs/calendar/email, Discord/Telegram before long-tail connectors.
- **Implementation complexity:** L.
- **Dependencies and risks:** Each connector needs capability manifest, auth/secrets handling, rate limits, action classifications, eval scenarios, and rollback/draft semantics where relevant. Scope creep is the biggest risk.
- **Operational gates:** G123, G124, G127, G131, G132.
- **Safety / ops / eval requirements:** Connector-level policy tests; secret storage/redaction; action allowlists; evals for read/write/draft/send flows; pinned dependencies and vulnerability scans.
- **Measurable success criteria:** Top connector paths achieve time-to-first-success targets with low support burden; connector failures are classified/traced/recoverable without leaking credentials; each production connector has eval scenarios, capability manifest, and security review record.

### F12 — Workflow eval and observability harness as product infrastructure

- **Phase / priority:** Phase 1 — P0/P1 / MVP-to-beta.
- **User value:** Users and operators can trust improvements because each agent workflow has scenario evals, traces, failure modes, and regression gates rather than vibes-only demos.
- **Target personas:** Technical users, architects, integrators, teachers, sales, creators.
- **Parity/source inspiration:** Ardur already has `ardur-eval` and OTel spans; broader market parity with OpenAI Agents SDK tracing/evals. Differentiate by making eval receipts visible per workflow and connector.
- **Implementation complexity:** M.
- **Dependencies and risks:** Needs golden scenarios by persona, trace retention policy, CI integration, and dashboard/reporting. Risk: evals become stale if not required in feature gate checklist.
- **Operational gates:** G125, G129, G130, G132, G133.
- **Safety / ops / eval requirements:** Scenario suites for tutoring integrity, sales claim grounding, connector secret safety, creator voice, video factuality, coding/test evidence, and cost/receipt correctness.
- **Measurable success criteria:** Every production workflow has a named eval suite and release gate threshold; regression failures block merge/deploy through enforced CI; MTTR decreases because traces link to run, receipt, cost, memory, and connector state.

### F13 — Polished Discord/Telegram gateway and notification delivery

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Ardur meets users where they already communicate while keeping channel ingress bounded, identity-aware, and auditable.
- **Target personas:** Non-technical users, technical users, creators, students, sales, teachers.
- **Parity/source inspiration:** Selective parity with Hermes gateway and OpenClaw channel-first thesis. Do not chase 20+ channels; make Discord and Telegram excellent, with Matrix/Slack as deliberate later/enterprise adapters.
- **Implementation complexity:** M.
- **Dependencies and risks:** Depends on F01, F05, F08, allowlists, identity pairing, delivery receipts, and channel-specific policy presets. Risk: arbitrary group ingress or leaked content in shared channels.
- **Operational gates:** G122, G124, G127, G130.
- **Safety / ops / eval requirements:** DM/group pairing tests; allowlist-drop tests; redacted summaries; delivery failure retries; channel-scoped memory/policy; approval prompts for external side effects.
- **Measurable success criteria:** Allowed-channel activation and recurring use increase without out-of-allowlist runtime admissions; notifications and approvals are delivered with receipt links and auditable status; channel-specific policy violations are caught in pre-release evals.

### F14 — Role-specific workflow templates and reusable packs

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Each persona gets a concrete first and repeatable use case instead of a blank chatbot: household admin, repo engineer, teacher, student, sales AE/SDR, architect, creator, YouTuber, and integrator.
- **Target personas:** All.
- **Parity/source inspiration:** Partial parity with Hermes skills and OpenClaw/ClawHub-style packs. Ardur differentiates by bundling templates with approval defaults, connector scopes, evals, and receipts.
- **Implementation complexity:** M.
- **Dependencies and risks:** Requires F04-F12 foundation. Risk: generic AI slop; templates must be source-grounded and role-specific with measurable outcomes.
- **Operational gates:** G124, G125, G130, G133.
- **Safety / ops / eval requirements:** Each pack declares inputs, connectors, allowed actions, risk mode, eval suite, success metric, and rollback/review behavior.
- **Measurable success criteria:** Template activation and repeat-use beat blank-chat baseline; each shipped pack has a passing eval suite and no unresolved high-risk gate failures; users can edit templates without accidentally broadening permissions.

### F15 — Developer/repo engineering workflow

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** A senior pair engineer that can inspect repos, make plans, work in sandboxes/worktrees, run tests, prepare reviewed diffs, and stop for approval before merge/deploy.
- **Target personas:** Technical users, architects, integrators.
- **Parity/source inspiration:** Parity with Claude Code/Codex/Hermes developer workflows; differentiate with cap-tokened tools, signed receipts, cost controls, and review-required handoffs.
- **Implementation complexity:** L.
- **Dependencies and risks:** Needs GitHub/CI connector, filesystem/shell sandbox, test runner evidence, diff review UI, and branch protection. Risk: repo damage, secret exposure, hallucinated test claims.
- **Operational gates:** G120, G121, G123, G124, G130, G132.
- **Safety / ops / eval requirements:** Worktree isolation; test-output receipts; unsafe command denial; secret scans; PR review gate; no auto-merge without explicit approval.
- **Measurable success criteria:** High percentage of agent diffs include relevant tests and real test output; review acceptance improves while rollback/unsafe-command rates stay low; no agent change bypasses branch protection, status checks, or reviewer approval.

### F16 — Architect decision, ADR, diagram, and threat-model workflow

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Architects get evidence-backed decision memos, trade-off analysis, diagrams, and threat models that challenge weak assumptions and preserve context over time.
- **Target personas:** Architects, technical leaders, integrators.
- **Parity/source inspiration:** Differentiates from general chat by combining provenance, memory, evals, and high-assurance gate checks. Partial parity with market source-connected research tools.
- **Implementation complexity:** M.
- **Dependencies and risks:** Depends on F06/F07/F11/F12 and diagram/document connectors. Risk: overconfident recommendations without source coverage or security gate mapping.
- **Operational gates:** G124, G126, G130, G133.
- **Safety / ops / eval requirements:** ADR evidence coverage checks; threat-model checklist; high-assurance gate mapping; citations for repo/docs/cloud inventory; review queue before external publication.
- **Measurable success criteria:** ADR cycle time decreases while evidence coverage and stakeholder acceptance increase; security gate findings are actionable and traceable to sources; rejected alternatives and assumptions are preserved in workspace memory.

### F17 — Teacher/student learning and integrity workflows

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Teachers save prep/grading time and students get useful coaching without silently finalizing grades, leaking student data, or enabling cheating.
- **Target personas:** Teachers, students, parents/schools.
- **Parity/source inspiration:** Market parity with education assistants and source-grounded notebooks, differentiated by integrity mode, privacy-scoped class/course workspaces, and proof-of-process receipts.
- **Implementation complexity:** M.
- **Dependencies and risks:** Needs LMS/Google Classroom/Canvas later, rubric/fairness evals, class/student memory boundaries, and approval gates. Risk: FERPA/privacy, biased grading, academic integrity misuse.
- **Operational gates:** G122, G124, G126, G130, G131.
- **Safety / ops / eval requirements:** Tutor-mode evals; rubric agreement tests; source completeness; privacy redaction; high-stakes grade/message approval gates; proof-of-process export.
- **Measurable success criteria:** Prep/grading hours saved with teacher-approved finalization; tutor-mode usage and quiz improvement rise without answer-mode abuse; no student data crosses class/course workspace boundaries in tests or audits.

### F18 — Sales/account intelligence and CRM workflow

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Sales users get grounded account briefs, compliant outreach drafts, faster follow-ups, and cleaner CRM updates without invented customer claims or accidental sends.
- **Target personas:** Sales, sales managers, customer-facing teams.
- **Parity/source inspiration:** Parity with enterprise assistant/connector patterns. Differentiation is approval-first external messaging, cited trigger events, CRM audit trail, and source-grounded value-prop library.
- **Implementation complexity:** M/L.
- **Dependencies and risks:** Depends on CRM/email/calendar connectors, provenance layer, approval queue, and compliance policies. Risk: hallucinated claims, spam, stale account data, external-send mistakes.
- **Operational gates:** G122, G124, G127, G130, G131.
- **Safety / ops / eval requirements:** External-send approvals; source freshness checks; hallucinated-claim eval; CRM write receipts; compliance policy tests; redacted account data in shared views.
- **Measurable success criteria:** Account brief time decreases and CRM field completion improves; follow-up latency decreases without increased compliance issues; hallucinated-claim corrections and external-send approval edits are tracked and decline.

### F19 — Creator and YouTube production workflow

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Creators turn source material into briefs, drafts, scripts, titles, descriptions, clips, and review-ready publishing packages while preserving voice and avoiding auto-publishing mistakes.
- **Target personas:** Creators, YouTubers, writers, podcasters.
- **Parity/source inspiration:** Selective parity with content assistants and OpenClaw media/channel surfaces; differentiate by source/copyright provenance, voice memory, approval-only publishing, and analytics feedback loops.
- **Implementation complexity:** M/L.
- **Dependencies and risks:** Needs media/transcript ingestion, YouTube Studio/analytics, brand voice memory, source/copyright checks, and approval queue. Risk: AI slop, copyright problems, factual errors, direct-publish accidents.
- **Operational gates:** G122, G124, G127, G130, G131.
- **Safety / ops / eval requirements:** Source/citation completeness eval; brand voice/edit-distance check; copyright/asset provenance flags; approval-only publish staging; analytics hypothesis tracking.
- **Measurable success criteria:** Draft acceptance/edit distance improves vs blank prompt baseline; topic-to-outline and repurposing cycle time decrease; no content is published without explicit approval and logged receipt.

### F20 — Collaboration, shared workspaces, review queues, and handoffs

- **Phase / priority:** Phase 2 — P1 / beta.
- **User value:** Teams, classes, clients, and creator/sales workflows can share agent output safely, review risky actions, assign follow-ups, and preserve decision context.
- **Target personas:** Teachers, students, architects, sales, integrators, creators, technical teams.
- **Parity/source inspiration:** Partial parity with Hermes Kanban handoffs and OpenClaw multi-agent sessions. Ardur should use workspaces, run receipts, and approval queues rather than cloning board mechanics exactly.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F06/F08/F09/F13. Risk: shared workspace leaks, approval ambiguity, and notification spam.
- **Operational gates:** G122, G124, G126, G127, G130.
- **Safety / ops / eval requirements:** Role-based workspace access; approval owner tests; redacted shared traces; notification routing controls; immutable handoff receipts.
- **Measurable success criteria:** Multi-user workspace activation and review turnaround improve; shared artifacts preserve source/run/approval context; no sampled shared workspace exposes memory/receipts outside authorized users.

### F21 — Admin/ops control plane, installers, doctor, backups, and upgrades

- **Phase / priority:** Phase 3 — P1 / GA.
- **User value:** Operators and integrators can deploy, monitor, upgrade, back up, restore, and troubleshoot Ardur without treating the runbook as tribal knowledge.
- **Target personas:** Integrators, technical users, architects, enterprise operators.
- **Parity/source inspiration:** Parity with Hermes setup/doctor/status and OpenClaw local gateway daemon/onboard. Differentiate through receipt-aware ops, policy test dashboards, and high-assurance deploy evidence.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F00-F03/F12. Risk: admin surfaces expose sensitive runtime state; installers bypass secure defaults for convenience.
- **Operational gates:** G126, G128, G130, G132, G133.
- **Safety / ops / eval requirements:** Private/auth admin; backup/restore reconciliation tests; upgrade smoke tests; policy source visibility; generic external errors; runbook command CI.
- **Measurable success criteria:** Fresh install to healthy first run meets target time for technical operators; backup/restore preserves journals/receipts/costs/memory consistency; upgrade/downgrade paths have smoke tests and rollback instructions.

### F22 — Connector SDK, certification, and safe marketplace foundations

- **Phase / priority:** Phase 3 — P1 / GA.
- **User value:** Integrators can extend Ardur for client systems without turning every connector into a bespoke security risk.
- **Target personas:** Integrators, technical users, architects, creators using packs.
- **Parity/source inspiration:** Partial parity with MCP, Hermes skills/tools, and OpenClaw ClawHub. Differentiation is certification: capability manifest, policy tests, secret handling, evals, versioning, and audit receipts before distribution.
- **Implementation complexity:** XL.
- **Dependencies and risks:** Depends on F02/F11/F12/F21. Long-tail connectors create vulnerability and support load; certification must reject unsafe packs.
- **Operational gates:** G121, G123, G124, G131, G132.
- **Safety / ops / eval requirements:** Connector template with tests; manifest schema validation; secret redaction; SBOM/dependency scan; signed package/provenance; sandbox simulation.
- **Measurable success criteria:** Connector time-to-first-success drops for integrators; certified connectors pass security/eval gates before publication; vulnerable or over-permissioned packs are blocked automatically.

### F23 — Cost governance, provider routing, fallback, and budget recipes

- **Phase / priority:** Phase 3 — P1 / GA.
- **User value:** Users can trust autonomous work because spend is forecast, bounded, settled, visible, and tunable by workspace, persona, provider, and workflow.
- **Target personas:** All, especially technical users, integrators, sales, creators.
- **Parity/source inspiration:** Ardur differentiator built on cost gates/provider selection; partial parity with multi-provider agents. Avoid hidden provider SDK calls by keeping provider seam configurable and audited.
- **Implementation complexity:** M/L.
- **Dependencies and risks:** Depends on F03/F08/F12. Risk: fallback providers change quality/cost behavior; streaming and tool costs must be included.
- **Operational gates:** G125, G129, G130, G131.
- **Safety / ops / eval requirements:** Budget reservation/finalization tests; provider fallback receipts; per-model pricing fallback; cost-unavailable fail/audit mode; workspace budget alerts.
- **Measurable success criteria:** Budget overruns and silent zero-cost paid calls are eliminated in supported paths; users can forecast and inspect cost per workflow/run/workspace; provider fallback events are traceable and do not weaken safety gates.

### F24 — Production deployment profiles: local-first, Docker, and controlled hosted options

- **Phase / priority:** Phase 3 — P1 / GA.
- **User value:** Ardur can serve both private personal-agent users and high-assurance teams with deployment choices that have matching threat models and runbooks.
- **Target personas:** Integrators, architects, technical users, enterprise operators.
- **Parity/source inspiration:** Selective parity with Hermes deployment flexibility and OpenClaw local-first daemon. Differentiation: deployment profiles are policy/cost/receipt aware, not generic process wrappers.
- **Implementation complexity:** L/XL depending hosted scope.
- **Dependencies and risks:** Depends on F00-F03/F21. Hosted options add identity, tenancy, compliance, billing, and operational risk; do not launch before self-hosted gates are solid.
- **Operational gates:** G120, G121, G126, G132, G133.
- **Safety / ops / eval requirements:** Docker/site validation; TLS/proxy guidance; deployment environment approvals; backups; dependency scans; profile-specific smoke tests and security docs.
- **Measurable success criteria:** Documented deployment profiles pass smoke tests in CI and operator runbooks; self-hosted deployments expose health/admin/backup/restore and receipt reconciliation clearly; hosted/private modes do not share secrets, memory, or receipts across tenants.

### F25 — Sandboxed browser/computer-control harness

- **Phase / priority:** Phase 4 — P2 / strategic.
- **User value:** Unlocks high-value UI workflows that lack APIs while keeping prompt injection, host escape, and real-world side effects under human and policy control.
- **Target personas:** Non-technical users, sales, creators, integrators, technical users.
- **Parity/source inspiration:** Delayed parity with Hermes browser/computer tools, OpenClaw browser/canvas, and OpenAI/Anthropic computer-use patterns. Do not expose ambient host control; use isolated sandbox, allowlists, screenshots, receipts, and human confirmation.
- **Implementation complexity:** XL.
- **Dependencies and risks:** Requires F01-F12, sandbox infra, allowed site/action manifests, screenshot/video retention policy, prompt-injection defenses, and reversible/approval semantics. High abuse and reliability risk.
- **Operational gates:** G122, G124, G125, G127, G130, G131.
- **Safety / ops / eval requirements:** UI-task eval suite; prompt-injection benchmarks; allowed-site/action enforcement; screenshot/action receipts; human confirmation for external consequences; no secrets typed by agent.
- **Measurable success criteria:** Sandboxed UI workflows meet reliability thresholds without host escape or unapproved high-impact actions; prompt-injection test cases are blocked or routed to approval; every UI action sequence has screenshot/action/receipt evidence and cost accounting.

### F26 — Voice, mobile, desktop companions, and lightweight nodes

- **Phase / priority:** Phase 4 — P2 / strategic.
- **User value:** Users get natural, always-available access after the core trust model is stable, especially for personal admin, study, creator, and field workflows.
- **Target personas:** Non-technical users, students, creators, YouTubers, sales.
- **Parity/source inspiration:** Delayed/selective parity with OpenClaw's mobile/desktop/voice companion thesis and broader market multi-surface expectations. Not required for MVP because trust and audit are the wedge.
- **Implementation complexity:** XL.
- **Dependencies and risks:** Depends on F05/F06/F08/F13/F21. Risk: voice creates accidental action and privacy problems; mobile nodes multiply auth/session/security surfaces.
- **Operational gates:** G122, G124, G127, G130, G131.
- **Safety / ops / eval requirements:** Wake/recording consent; identity/session pairing; channel allowlists; approval for sends/purchases/destructive actions; transcript redaction; device revocation.
- **Measurable success criteria:** Voice/mobile users complete supported workflows faster than text without increased incident rate; device/session revocation and audit history are understandable to non-technical users; no voice-triggered external side effects occur without approved policy or confirmation.

### F27 — Multi-agent teams and external board projections

- **Phase / priority:** Phase 4 — P2 / strategic.
- **User value:** Complex work can be decomposed across specialist agents, reviewers, and systems while Ardur keeps the authoritative run/receipt ledger and projects status to boards where teams already work.
- **Target personas:** Technical users, architects, integrators, sales teams, creator teams.
- **Parity/source inspiration:** Selective parity with Hermes Kanban and Claude-style agent teams. Avoid cloning Hermes profiles/SQLite board directly; build Ardur-native runs/handoffs with projections to Linear, GitHub Issues, Jira, or Kanban views.
- **Implementation complexity:** L.
- **Dependencies and risks:** Depends on F08/F20/F21. Risk: runaway agent loops, duplicated responsibility, unclear review/approval ownership, cost explosion.
- **Operational gates:** G124, G125, G127, G130.
- **Safety / ops / eval requirements:** Turn/runtime/cost budgets; dependency DAG validation; reviewer gates; final artifact verification; no recursive scheduling without policy; audit chain per sub-run.
- **Measurable success criteria:** Complex workflow cycle time decreases without increased review defects or cost overruns; each sub-agent handoff has clear owner/artifact/gates/receipt lineage; board projections match authoritative run ledger in sampled audits.

### F28 — Versioned skill/agent marketplace and policy-reviewed pack ecosystem

- **Phase / priority:** Phase 4 — P2 / strategic.
- **User value:** Users and integrators can install new workflows safely, understand what they do, and roll them back without loading unreviewed prompts/tools into every session.
- **Target personas:** Technical users, integrators, creators, teachers, sales.
- **Parity/source inspiration:** Partial parity with Hermes Skills Hub and OpenClaw ClawHub. Ardur should not race for pack count; it should win on review, versioning, capability manifests, evals, and provenance.
- **Implementation complexity:** L/XL depending distribution scope.
- **Dependencies and risks:** Depends on F02/F12/F22. Risk: supply-chain attacks through packs, prompt injection, overbroad capabilities, stale evals, incompatible versions.
- **Operational gates:** G121, G123, G124, G131, G132.
- **Safety / ops / eval requirements:** Pack signing/provenance; semantic versioning; migration/rollback; required eval suites; capability diff review; secret/resource confinement tests.
- **Measurable success criteria:** Install-to-first-success rate is high for certified packs; pack capability changes require explicit review and are visible before install/update; unsafe/vulnerable/overbroad packs are blocked before distribution.

## 9. Recommended sequencing and decision checkpoints

1. **Gate-first sequencing:** Close F00-F03 before promising public beta or high-assurance usage. These are not polish; they determine whether Ardur's trust claims are true.
2. **MVP wedge:** Ship F04-F12 as a narrow but complete trusted-delegate loop: guided first win, trust center, scoped memory, provenance, runs/schedules, dry-run approvals, CLI/TUI, curated connectors, and evals.
3. **Persona beta:** Use F13-F20 to validate repeatable paid/retained workflows. Prioritize the first 2-3 personas that show the strongest activation and willingness-to-pay signals rather than shipping shallow packs for all personas at once.
4. **GA operations:** Use F21-F24 to make the product operable by real self-hosted/implementation teams. Treat installer, doctor, backups, upgrades, and deployment profiles as product features.
5. **Strategic expansion:** Only pursue F25-F28 after the trust loop, evals, connector certification, and cost/receipt reliability are proven. These features are powerful but multiply security and support surface.

## 10. Roadmap risks and mitigations

| Risk | Why it matters | Mitigation |
|---|---|---|
| Trying to clone every Hermes/OpenClaw feature | Dilutes Ardur's trust-runtime wedge and multiplies security/ops surface | Use selective capability parity and defer channel/voice/browser breadth |
| Gate friction hurting activation | Users may churn before seeing value | Turn gates into product UX: guided setup, trust center, approval recipes, safe sample tasks |
| Memory becoming creepy or leaky | Cross-workspace leakage damages trust and compliance | Scoped workspaces, visible memory cards, validity/source labels, export/delete, isolation tests |
| Connector breadth becoming brittle | Long-tail integrations create outages and security incidents | Curated connector pack first; certification and evals before marketplace |
| Autonomy causing spend or action surprises | Budget overruns, accidental sends, destructive actions | Run budgets, dry-run defaults, approval queues, receipts, fail-closed cost settlement |
| Browser/computer control too early | Prompt injection, host escape, high-impact unapproved actions | Delay until sandbox, allowlists, human confirmation, and UI evals exist |

## 11. Clear product stance

Ardur should be framed as **a high-assurance personal/enterprise agent runtime that makes delegation auditable**, not as a Hermes/OpenClaw skin. The product should say yes to capability parity that users need to get work done — scheduled runs, memory, tools/MCP, channels, connectors, onboarding, traces, and workflow templates — and no or later to one-to-one surface breadth: every chat adapter, every voice/mobile/canvas feature, unrestricted desktop control, raw marketplace growth, and novelty UX.

If Ardur nails the trusted delegation loop, it can expand into channels, personas, teams, browser control, voice, and marketplace with far less risk. If it skips the high-assurance gates to chase visible parity, it loses the one defensible advantage identified by all parent research: auditable runtime trust.
