# Contributing to Ardur

Thanks for your interest in Ardur. This project is in early, active design — APIs are unstable and the architecture is still being shaped. Contributions are welcome, but please read the rules below first; they are enforced.

## Two hard requirements

### 1. DCO sign-off (required)

Every commit must carry a `Signed-off-by` trailer certifying the [Developer Certificate of Origin](https://developercertificate.org/). Add it automatically with:

```sh
git commit -s -m "your message"
```

The trailer must match your commit author identity:

```
Signed-off-by: Your Name <your.email@example.com>
```

A CI check rejects any commit in a pull request that is missing a valid sign-off.

### 2. SSH-signed commits (required)

All commits must be cryptographically signed. Ardur uses **SSH commit signing**. Configure it once:

```sh
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/your_signing_key.pub
git config --global commit.gpgsign true
```

Then add the corresponding public key to your GitHub account as a **signing key** (Settings → SSH and GPG keys → New SSH key → key type *Signing Key*). The `main` and `dev` branches both require signed commits; unsigned commits will be rejected on push.

## Operating rules

Ardur's public contribution contract is this file, the pull-request template, `SECURITY.md`, and the checks in `.github/workflows/`. Contributors do **not** need private Linear access or private helper scripts to open a correct PR.

Required gates for code changes:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- site build, Docker build/healthcheck, dependency audit, SBOM, and secret-scan CI jobs when they apply

Reference public GitHub issues in PRs when available. Maintainers may mirror accepted work into private planning systems after the fact; that mirror is not part of the contributor workflow.

## Plan-doc discipline

Substantive design and implementation work should explain its problem statement, security impact, testing/verification gates, rollout risk, and alternatives considered either in the PR description or in a public design note under `docs/`. If a private planning section exists, maintainers can add that mapping during review; external contributors are not expected to know it.

## Commit-scope discipline

Keep each commit tightly scoped to a single logical change. Don't mix refactors with features, or unrelated files in one commit. A `check-commit-scope` hook will be added in a follow-up to enforce this automatically; until then, please self-police scope and split large changes into reviewable commits.

## Pull requests

- Branch from `dev` for ongoing work; `main` is protected and merges require review.
- Treat `dev` as the integration branch and `main` as the linear public release
  branch. Routine `dev` → `main` sync PRs must preserve `main`'s linear history:
  use a squash merge for multi-commit syncs, or a rebase merge for a single
  commit. Do not use merge commits for routine `main` syncs. Admin bypass is a
  break-glass path only and must be recorded in the PR and the owning Linear
  issue.
- Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) completely — including the `§X.Y` plan mapping, verification gates passed, and the DCO + signed-commit checklist.
- Keep PRs focused and reviewable. Large PRs will be asked to split.

## Code of Conduct

By participating you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).
