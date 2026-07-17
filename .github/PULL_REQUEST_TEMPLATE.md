## Summary

<!-- What does this change do, and why? -->

## Scope

<!-- Keep the change tightly scoped to a single logical change. Link public issues/design docs when available. Private tracker IDs are optional for maintainers and are not required from external contributors. -->

- Public issue/design doc:

## Security impact

<!-- Describe auth, policy, secret-handling, cost, receipt, or data-boundary impact. Write "none" only when you checked. -->

## Verification gates passed

<!-- Check the gates that apply and note how you verified them. -->

- [ ] `cargo build` / `cargo check --workspace --all-targets`
- [ ] `cargo test --workspace`
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Site build (`npm ci && npm run build` in `site/`)
- [ ] Docker build + healthcheck
- [ ] Security scans (audit/SBOM/secret scan)
- [ ] Manual verification (describe below)

## DCO + signed-commit checklist

- [ ] Every commit is signed off (`git commit -s`) with a `Signed-off-by` trailer matching the author.
- [ ] Every commit is SSH-signed.
- [ ] Commits are scoped to a single logical change.

## Related issues

<!-- e.g. Closes #123 -->
