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

Ardur follows a set of internal **Operating Rules** that govern engineering discipline, verification gates, and review expectations.

> **Note:** The full OPERATING-RULES document is private during early development and is not yet published in this repository. This section is a placeholder; it will be filled in (or linked) once the rules are ready for public release. In the meantime, the requirements on this page plus the PR template are the operative contract.

## Plan-doc discipline

Substantive design and implementation work is tracked through structured **plan documents**. Each plan doc follows the project's **13-canonical-section template** (problem statement, prior art, design, interfaces, data model, security, testing/verification gates, rollout, risks, alternatives considered, open questions, references, and changelog). When your contribution maps to a plan section, reference it by its `§X.Y` identifier in your commits and PR.

> The plan-doc template itself is maintained alongside the (currently private) operating rules and will be published with them.

## Commit-scope discipline

Keep each commit tightly scoped to a single logical change. Don't mix refactors with features, or unrelated files in one commit. A `check-commit-scope` hook will be added in a follow-up to enforce this automatically; until then, please self-police scope and split large changes into reviewable commits.

## Pull requests

- Branch from `dev` for ongoing work; `main` is protected and merges require review.
- Fill out the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) completely — including the `§X.Y` plan mapping, verification gates passed, and the DCO + signed-commit checklist.
- Keep PRs focused and reviewable. Large PRs will be asked to split.

## Code of Conduct

By participating you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).
