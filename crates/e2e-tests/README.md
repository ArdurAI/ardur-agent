# ardur-e2e-tests

The workspace's cross-crate **end-to-end** scenario host (§2.E / §19).

Per-crate integration suites prove a crate speaks its own contract. These
scenarios prove the Phase-1 substrate works **fused**: a single request driven
through more than three crates as one call path — cap-token authorization,
cost-gate admission, provider completion, runtime turn, receipt minting,
journal persistence, and memory. The coverage gaps and the nine planned
scenarios are catalogued in
[`architect/backlog/e2e-test-coverage-gaps.md`](../../architect/backlog/e2e-test-coverage-gaps.md).

This crate ships **no public API** (`publish = false`). `src/lib.rs` exists only
to host `fixtures`, the shared deterministic test helpers (temp roots, the
cap-token root key, the receipt signing key, the stub provider, a permissive
Cedar bundle).

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

Phase **2.E1 of 9**. Implemented: scenario #1 `cli_full_substrate_turn`.
Remaining (#2–#9) are tracked in the backlog doc; #7 and #8 unblock after
`cedar-policy` / `injection-defense` reach the call-path surfaces they need.
