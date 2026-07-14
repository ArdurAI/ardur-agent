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
  `idString`). `actor` = `subject` (SPIFFE URI). `budget_remaining` =
  `{"cost": <remaining>}`.
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

### Wiring into the runtime (proposed, not landed here)

The prototype exercises the seam through the real `ardur-receipt`/`ardur-cap-token`
public APIs. To make a running agent emit ERs, add one opt-in builder setter
(`FusedRuntimeBuilder::with_governance(Arc<dyn GovernanceEmitter>)`) invoked at
the existing receipt-mint point (`runtime.rs:1263-1352`), passing the already
-computed `VerifiedClaims` + tool-call record. This is intentionally left out of
this PR to avoid colliding with the ~30 in-flight fused-runtime lanes; it is a
small follow-up once this crate lands.

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
