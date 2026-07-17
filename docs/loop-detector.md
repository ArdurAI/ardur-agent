# Loop Detection and Runaway-Agent Controls (§11.13)

`ardur-loop-detector` (`crates/loop-detector`) is the early guard against a
stuck agent. The §11.14 cost ceiling is the *last* guard — it refuses once a
budget is spent. This crate catches the loop that would spend it, watching every
tool-call admission and every turn boundary and tripping on **any one of three
signals** before the ceiling is reached.

## The three signals

1. **Same-tool-same-args repetition.** The same tool name + canonical argument
   fingerprint admitted `N` times inside an `M`-turn sliding window. Arguments
   are fingerprinted order- and whitespace-independently, so an agent that
   varies only formatting still trips.
2. **No progress.** `K` consecutive turns with no progress receipt — a durable
   artifact (memory write, checkpoint, child completion) or an externalized
   output (channel-outbound message). A run spending tool calls without
   producing anything is looping.
3. **Cost acceleration.** Per-turn cost growing by a factor of `R` for `W`
   consecutive windows (each window compares a turn to the one three turns
   back). Productive work varies in cost; a context-bloating loop accelerates.

Whitelisting is per-signal: a status poller or paginator can be exempted from
the *repetition* signal (via a polling key or pagination cursor) while still
being subject to the no-progress and cost-acceleration signals.

## Escalation: detect → halt → kill

| Stage | Verdict | Meaning |
|---|---|---|
| Trip | `SignalTripped` | A signal fired; a grace window opens so the agent or operator can react. The run continues. |
| Grace expiry | `HaltRequired` | The grace window closed with the signal still active. The runtime refuses the next tool-call admission; in-flight calls finish. |
| Escalation | `KillRequired` | Two signals active at once, an admission attempted after a halt, or an emergency-grade cost spike (≥ 2×`R`). The runtime tears the run down. |

Halt is recoverable via an operator override; kill is terminal. Every halt and
kill carries `LoopEvidence` — the offending receipt hashes and the per-turn cost
trajectory — so an operator auditing a halt sees exactly what tripped it.

## Cap-token-encoded loop budget

The active thresholds are a `LoopBudget` carried on the run's cap-token.
`derive_for_loop_budget` narrows a parent budget for a child run: every field
may be **tightened** (lower `N`, `K`, `W`, `R`, grace; a more aggressive override
action) but never **relaxed**. A relaxation attempt is a hard
`BudgetError::RelaxationAttempted`. §5.0's child-mission derivation routes
through this helper, so a sub-agent inherits — and can only shrink — its parent's
loop tolerance (ADR-Phase3-274).

The per-profile override action tunes what a trip does:

- `Halt` (default) — open the grace window, then halt.
- `Warn` — emit the detected verdict but never halt (dev / canary).
- `Kill` — escalate straight to a kill, skipping grace (paranoid profiles).

## Receipts

The detector never signs or chains a receipt itself. It names a verb and anchors
an evidence digest; the owning runtime mints the receipt through `ardur-receipt`,
chaining it onto the same hash-linked audit log as the tool calls that drove it.
The verbs (`ardur_loop_detector::verbs`):

| Verb | Emitted when |
|---|---|
| `agent.loop.detected.v1` | Any signal trips (grace opens) |
| `agent.loop.halted.v1` | Grace expires; halt fires |
| `agent.runaway.killed.v1` | Kill escalation |
| `agent.loop.signal_cleared.v1` | A trip recovers within grace |
| `agent.loop.detection_overridden.v1` | Operator override resumes the run |
| `tool.call.refused_by_loop_halt.v1` | Admission refused under an active halt |
| `cap_token.derivation.loop_budget_relaxation_refused.v1` | A budget derivation tried to relax |

## What this crate is and isn't

- **Pure and synchronous.** Signal checks are window inspection and hash
  comparison — no language-model call, no I/O — so the detector runs on the
  admission hot path without adding latency.
- **State is caller-owned.** `LoopDetectorState` is serializable; the runtime
  persists it to §7.0 memory for crash-resilient detection across a session
  resume, and exposes it over the §4.1 daemon IPC for live inspection.
- **Sealed traits.** `LoopDetector`, `RunawayHalter`, and the whitelist evaluator
  are sealed to a single workspace implementation, so the safety logic is
  single-sourced and auditable.

## Integration status

Phase 1 (this crate) lands the detector, halter, budget attenuation, whitelist,
and receipt vocabulary with unit, integration, and property tests. Wiring the
detector into the fused-runtime admission pipeline, persisting state to memory,
and the operator `ardur run resume --override-loop-detection` CLI verb are
follow-up integration work in the owning runtime and CLI crates.
