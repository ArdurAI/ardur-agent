# Governing ardur-agent under Ardur (MCEP) — seam design

**Status:** prototype (this crate). **Target branch:** `dev` (integration lane
merges). **Scope:** design + a compiling, tested seam; not a merge.

This document specifies how ardur-agent runs as a **governed workload** under the
Ardur governance layer (ArdurAI/ardur) — so the agent's tool-calls and
delegations produce **Ardur-verifiable Execution Receipts** and can be placed
under Ardur's kernel enforcement. It is written against the *actual* code of
both repos (verified APIs, no invention); every claim below has a file anchor in
the two source trees.

## 1. What Ardur is (the parts that matter here)

Ardur's protocol layer is **MCEP** (Mission-Controlled Execution Protocol), three
artifacts:

- **Mission Declaration (MD)** — issuer-signed mission/policy envelope.
- **Delegation Grant (DG)** — the delegated capability. Normatively an **AAT**
  (`draft-niyikiza-oauth-attenuating-agent-tokens-00`): an Ed25519-signed JWT
  chain with `del_depth` / `par_hash` / `cnf.jwk` / PoP, plus one profile claim
  `mission_ref`. Engine: `go/pkg/aat`. (ADR-017/018 also define a parallel
  **Biscuit-passport** delegation path with strict-narrowing verifier semantics.)
- **Execution Receipt (ER)** — per-hop signed evidence. **ES256 JWS**,
  `typ=application/ardur.er+jwt`, hash-chained via `parent_receipt_hash`
  (SHA-256 of the prior signed ER JWT). Schema:
  `docs/specs/execution-receipt-v0.1.schema.json` (25 required claims,
  `additionalProperties:false`).

Enforcement is two planes in `go/`: a per-cgroup **BPF-LSM** that can truly
block `exec` / file-open / IP-connect with `-EPERM` (`action=DENY,
enforce_mode=ENFORCE`; `go/pkg/kernelcapture/process_guard.bpf.c`), and an
observe-only exec/exit correlation harness. The production **verifier** is the
Python reference proxy `python/vibap/proxy.py` (`/session/start`, `/evaluate`,
`/delegate`, ...), which emits the signed ER to its receipts log.

## 2. What ardur-agent already has (the substrate)

The agent is not starting from zero — two crates are structurally isomorphic to
MCEP:

| MCEP artifact | ardur-agent substrate | Match |
|---|---|---|
| ER (ES256 JWS, SHA-256 hash-chain) | `crates/receipt`: `ReceiptSigner`/`ReceiptVerifier` (ES256 JWS, `typ=ardur-receipt+jws`), `ReceiptChain` (parent_hash = SHA-256 of prior compact JWS), `Es256*`/`Jwks` | **crypto identical; claim set differs** |
| DG (attenuating grant) | `crates/cap-token`: Ed25519 Biscuit, offline strict-narrowing attenuation, `VerifiedClaims.token_id` (UUIDv4) | **Biscuit path, not JWT-AAT** |
| Verifier gate | `crates/fused-runtime`: `authorize_tool_invocation` + `authorize_tool_capabilities` (cap-token + Cedar) at the tool-call boundary | userland gate, mirrors ER verdict inputs |
| Budget conservation | `crates/cost-gate`: reserve-before/refund-after | maps to ER `budget_remaining`/`budget_delta` |

So the seam is a **claim-set projection**, not new cryptography.

## 3. The seam (this crate: `crates/ardur-governance`)

Non-invasive by construction: the native receipt chain (single-writer under the
fused runtime's `commit_lock`/`chain_tail`) is untouched. The ER is a **mirror
record** projected from the same facts.

```
 tool call ─▶ fused-runtime gate ──▶ VerifiedClaims (cap-token)  ┐
             (authorize_tool_*)      + AuthOutcome (verdict)      ├─▶ project_execution_receipt
                                     + normalized ToolInvocation  ┘        │
                                                                           ▼
                                              ExecutionReceipt (ER v0.1 claim set)
                                                                           │  ErSigner (ES256, typ=application/ardur.er+jwt)
                                                                           ▼
                                              SignedExecutionReceipt ──▶ ER mirror log (hash-chained)
                                                                           │
        VerifiedClaims ──▶ EnforcementProfile.from_claims ──▶ DaemonApplyPolicyRequest (BPF-LSM)
        VerifiedClaims + MissionRef ──▶ GrantDescriptor (present to proxy /session/start biscuit path)
```

Mapping decisions (all in code, all tested):

- **`grant_id` = cap-token `VerifiedClaims.token_id`** (UUIDv4 satisfies ER
  `idString`). `actor` = `subject` (SPIFFE-style URI; a naming convention, not
  an attested SVID — see `SECURITY.md`). `budget_remaining` =
  `{"cost": <remaining>}` on the legacy path; #545 adds the registry path —
  `StepContext::per_class_budget_remaining` projects per-class keys through
  the shared effect-bucket registry (`src/effect.rs`), where a key outside
  the normative five-class namespace fails projection.
- **Effect-bucket registry (#545 / GOV-06 / D1).** ONE versioned table
  (`effect-bucket-registry.v1`, `src/effect.rs`) maps the native
  `CostTuple` axes onto the normative MIC effect classes and is shared by
  the MD author (budget keys must be registry classes), the emitter
  vocabulary (`normalize_effect_class` covers the §6.2 side-effect
  taxonomy; cross-crate agreement pinned against
  `ardur-observed-events`), and the ER adapter (`budget_remaining` keys).
  Native `cents` and `wall_ms` stay ECONOMIC axes — zero bucket
  contribution — and no owner-selected cost control changes. Units are
  steps, rounding is floor (never ceil: no invented usage), reservation/
  commit/refund/fail conserve, sibling carves bound children by their own
  ceilings (§5.5/§9.4), replayed settles and corrupt serialized ledgers
  are refused, and a descriptor change must carry a new version. Mapping:
  `read ← tokens_in (1/1)`, `write ← tokens_out (1/1)`,
  `exec ← milli_attention (1/1000)`; `network` and `external_send` have
  no native axis in v1 (emitter-classified steps only).
- **Verdict/denial mapping** follows verifier-contract §9 fail-closed table:
  cap-token `Expired`/`AudienceMismatch`/`ToolNotAllowed` → `violation` +
  `policy_denied`; `BudgetExhausted` → `violation` + `budget_exhausted`;
  `Revoked` → `violation` + `revoked`; `SignatureInvalid`/`Malformed` →
  `violation` + `chain_invalid`; missing telemetry → `insufficient_evidence` +
  `telemetry_missing`. The schema's `allOf` invariant (compliant ⇒ no denial
  fields; else both) is enforced in `check_verdict_invariant` before signing.
- **Chaining** mirrors the reference impl: `parent_receipt_hash` = SHA-256 of the
  prior signed ER JWT; `parent_receipt_id` = `parent_receipt_hash[..16]`;
  `receipt_id`/`jti` are stable hashes over the id-free step material.
- **One key, one JWKS.** ER JWS is signed with the same P-256 custody as native
  receipts and the `kid` derivation is identical, so a governed runtime publishes
  a single JWKS covering both `ardur-receipt+jws` and `application/ardur.er+jwt`.
- **Enforcement mirrors the userland gate.** `EnforcementProfile::from_claims`
  turns the effective capability set (`cap.shell_exec`→Exec, `cap.fs_read`→
  FileRead+`path_allow[cwd]`, `cap.fs_write`→FileWrite, `cap.network_out`→
  NetConnect; absent ⇒ `Deny`) into an Ardur `DaemonApplyPolicyRequest`, so the
  kernel enforces the *same* authority the tool-call gate already applied.

### Wiring into the runtime (landed — #502 Seam B7, Phase 1)

The prototype exercised the seam through the real `ardur-receipt`/`ardur-cap-token`
public APIs; the runtime wiring landed as the B7 integration PR:
`FusedRuntimeBuilder::with_governance(Arc<dyn GovernanceEmitter>)` is the
opt-in setter, and the emitter is invoked at the commit decision — inside the
commit lock, immediately after the native receipt append — so abandoned /
cancelled turns mint no round ER (Phase 1 semantics; the terminal cancellation
marker is deliberately not mirrored). The shipped file-backed implementation
is `ardur_fused_runtime::ErMirrorEmitter`, which signs with the same P-256
custody as native receipts and chains signed ERs into a
`governance/er-chain.jsonl` mirror log (one line per committed round,
verified and resumed across restarts). Per-tool effect classification and
durable pre-effect evidence were #543 and landed as Phase 2 below.

### Durable per-event evidence (landed — #543, Phase 2)

Phase 1's round ERs cannot reconstruct what the verifier contract wants — one
ER per **evaluated event** — because the native receipt aggregates a tool
round and never carries arguments/outputs inline, while refusal / timeout /
scan failure exit before its append and memory work happens after it. Phase 2
adds a durable evidence journal beside the mirror log:

- **`governance/events.jsonl`** — one JSON record per line, appended with the
  same hardened no-follow fsync writer and single-writer fork guard as the
  chains. A **pre-effect record** (grant facts, normalized invocation
  classification, canonical arguments — inline up to 1 MiB, otherwise the
  content-addressing digests only) is durable **before** the effect runs; a
  **post-effect record** (observed effect digest + incurred cost + output
  admission, a typed denial, or an explicitly **unknown** outcome) lands at
  the event's terminal point. Event identity is deterministic
  (`ev:<hash(session, iteration, ordinal, call_id)>`), so replay reconstructs
  the same identity rather than minting siblings.
- **One ER per evaluated event**, projected from the records at the terminal
  point — including events whose round never commits (denial after an earlier
  successful tool, timeout, scan rejection, memory-write denial), which the
  round mirror cannot cover. The round ER (Phase 1) is unchanged.
- **Crash replay is idempotent and never re-executes.** At open the emitter
  re-projects every terminal event the chain does not already carry (dedup by
  `step_id` = event id); an event stranded pre-observation (crash mid-invoke,
  dropped stream) mints an explicit `insufficient_evidence` ER — the tool is
  never re-run. The journal itself is checked, not trusted: a torn tail, a
  dangling post, a duplicate record, or arguments that do not hash to the
  recorded digests fails the open.
- **Missing reconstruction inputs never mint compliance.** An otherwise
  compliant event whose arguments exceeded the inline cap reports
  `insufficient_evidence` (`arguments_evidence_omitted`) with the recorded
  digests still binding the invocation; denials stay denials — the gate's
  decision is itself the sufficient fact.

The mirror remains strictly observational: recording and projection are
best-effort and never gate admission (no second admit stack), default-off via
`ARDUR_GOVERNANCE`, and the native receipt chain is untouched (verified
byte-identical across crash replay in the guard suite).

## 4. Cross-repo dependencies (Ardur-side vs agent-side)

- **CR-1 — DG wire-format gap (Ardur-side or agent-side).** Spec DG = JWT-AAT
  (Ed25519); the agent has Biscuit cap-tokens + ES256 receipts and no Rust
  JWT-AAT issuer. Full DG-chain verification (AAT §7) needs **either** an
  Ardur-published Rust AAT surface **or** an agent-side JWT-AAT issuer. The
  prototype routes delegation via the proxy's `token_type=biscuit`
  `/session/start` path and carries the cap-token as the grant.
- **CR-2 — Biscuit-schema alignment (Ardur-side confirm).** The proxy biscuit
  path needs a configured issuer key that trusts the agent cap-token root key and
  shares the Datalog fact-family/symbol schema (cap-token `verify.rs` uses
  `CUSTOM_SYMBOL_OFFSET=1024`). Confirm the public proxy accepts externally
  -minted biscuits.
- **CR-3 — Enforcement IPC contract (Ardur-side).** `DaemonApplyPolicyRequest` +
  the seccomp-listener/cgroup handoff are Go-internal (`daemon_protocol.go`). A
  **stable socket/IPC contract** must be published for a non-Go workload to hand
  policy to `ardur-kernelcaptured` and be bound into a managed cgroup. True LSM
  deny is Linux + cgroup only; the agent's dev host is macOS (observe-only).
- **CR-4 — Identity + Mission conventions (joint).** ER requires
  `verifier_id`/`iss`/`trace_id`; the DG profile binds `mission_ref` → MD. The
  agent has no MD concept; Ardur issues MDs via proxy `/issue`. Agree a governed
  -workload `verifier_id` namespace + MD issuance flow. The prototype carries a
  supplied `mission_ref` but does not author MDs.
- **CR-5 — ER exp/TTL (agent-side, minor).** Reference impl sets `exp=iat+300`;
  the prototype exposes a `ttl_secs` knob defaulting to 300s.

## 5. Verification

`cargo test -p ardur-governance` — 3 unit (JCS) + 5 E2E, all through real public
APIs: a real minted+verified+attenuated cap-token, ER projection, ES256 sign,
2-hop mirror-chain verify (and reorder-rejection), schema-shape assertions,
verdict/denial invariants, and enforcement-profile derivation. `cargo clippy
--all-targets -- -D warnings` and `cargo fmt --check` are green.
