# ardur-e2e-tests

The workspace's cross-crate **end-to-end** scenario host (§2.E / §19).

Per-crate integration suites prove a crate speaks its own contract. These
scenarios prove the Phase-1 substrate works **fused**: a single request driven
through more than three crates as one call path — cap-token authorization,
cost-gate admission, provider completion, runtime turn, receipt minting,
journal persistence, and memory. The coverage gaps and the nine planned
scenarios are catalogued in
[`architect/backlog/e2e-test-coverage-gaps.md`](../../architect/backlog/e2e-test-coverage-gaps.md).

Since Phase 2 there is a real fused entry point — `ardur_fused_runtime::FusedRuntime`,
a `ChatRuntime` that drives all ten stages (cap-token → cedar → cost-gate →
pre-submit hooks → provider → receipt → post-receipt hooks → finalize → memory →
journal) behind one `submit`. Scenarios #2–#4 drive *it* rather than assembling
the crates by hand the way #1 had to before the fused runtime existed.

This crate ships **no public API** (`publish = false`). `src/lib.rs` exists only
to host `fixtures`, the shared deterministic test helpers (temp roots, the
cap-token root key + issuer, the receipt signing key, the stub provider, a
permissive Cedar bundle, a manual clock, a cap-token mint helper, and a
pre-wired `fused_builder`).

## Running

```sh
cargo test -p ardur-e2e-tests      # just the E2E scenarios
cargo test --workspace             # picks them up automatically — no flag
```

All scenarios use the Anthropic **stub** provider and need no API key.

## Adding a scenario

1. Create `tests/scenario_NN_<name>.rs` (zero-padded `NN`, snake_case `<name>`
   matching the catalog entry, e.g. `scenario_02_cap_token_revoked_mid_session.rs`).
2. Encode the exercised crates in the test name so a regression points at the
   right plan-family on sight.
3. Build setup from `ardur_e2e_tests::fixtures`; add new shared helpers there
   rather than duplicating them per scenario.
4. A scenario never edits a prior scenario.

## Status

Implemented: scenario **#1** `cli_full_substrate_turn`, **#2**
`cap_revocation_mid_session`, **#3** `cost_ceiling_exhaustion`, **#4**
`receipt_chain_replay`. #2–#4 run on the Phase-2 `FusedRuntime`.

Remaining (#5–#9) are tracked in the backlog doc; #7 and #8 unblock after
`cedar-policy` / `injection-defense` reach the call-path surfaces they need.
