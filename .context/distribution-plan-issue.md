# ardur distribution — Implementation Plan (brew + npm + multi-platform release channel)

Date: 2026-09-18 · Goal: `brew install ardurai/tap/ardur` and `npm i -g @ardurai/ardur` work on macOS (arm64/x86_64) and Linux (x86_64/arm64), with the supply-chain posture the repo already has (sigstore bundles, SPDX SBOM, SHA256SUMS) extended to every new artifact — not weakened to ship faster.

## 0. Verified baseline (dev `a8893e1`, release v0.2.0)

What EXISTS today:
- GitHub Releases with 6 binaries (`ardur`, `ardur-server`, `ardur-admin`, `ardur-eval`, `ardur-healthcheck`, `ardur-memory-eval`) — **x86_64-unknown-linux-gnu ONLY** (`release.yml` builds on ubuntu-latest, single target, no matrix).
- Every artifact ships a sigstore `.bundle`, plus `SHA256SUMS` (+ bundle) and an SPDX SBOM (+ bundle). This posture is ahead of most agent CLIs and is non-negotiable baseline for new channels.
- Docker: `ghcr.io/ardurai/ardur-agent:<tag>` published on `v*` tags from the already-scanned CI image (docker.yml), with SBOM + provenance attestation.
- Source install: `cargo install --path crates/cli` (README quickstart is source-build only).
- `crates/config` exists; providers are env-selected. No crates.io publishing (`publish` unset everywhere, no release-to-crates workflow).
- Site `hugo.toml` baseURL is a TODO placeholder (`https://ardurai.dev/`), so no install docs page has a stable home yet.

What is MISSING for the stated goal: macOS targets entirely (both arches), Linux arm64, a Homebrew tap + formula, an npm wrapper package, cargo-binstall metadata, a `curl | sh` installer with checksum verification, and a README/site install section that reflects real channels.

## 1. Reuse posture (per #501 — wrap/port, credit MIT)

- prime-agent distributes as an npm package (TypeScript); hermes-agent via pip/uvx. Neither pattern transfers code to a Rust binary, but the **npm-wrapper-for-native-binary** pattern (esbuild/biome/turbo: a meta-package plus per-platform `optionalDependencies` carrying the binary, thin JS shim resolving the right one) is the industry-standard bridge and is what D2 below adopts. Credit not required (pattern, not code), but note the lineage in the package README.
- The npm channel matters strategically beyond convenience: it puts `ardur` in the same install motion prime-agent users already perform, which is the #148/#501 migration funnel.

## 2. Milestones (each independently shippable, full repo gate)

| # | Scope | Acceptance (evidence) |
| --- | --- | --- |
| P0 | Release matrix: add `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu` to release.yml (macos-15 runner ×2, cross or native arm64 runner for Linux arm64); every new artifact gets the same sigstore bundle + SHA256SUMS + SBOM treatment; tag-pin guard tests extended to the new lanes | a `v*` tag produces 6 binaries × 4 targets, all bundled+summed; `cosign verify-blob`/`gh attestation verify` documented and proven for one artifact per target in CI |
| P1 | Homebrew: `ArdurAI/homebrew-tap` repo; formula (bottle-less, pulls release tarballs by target, verifies sha256) generated + PR'd automatically by the release workflow; `brew install ardurai/tap/ardur` installs `ardur` (server et al. as separate formulae or `--with-server` style options decided at implementation) | fresh macOS arm64 + x86_64 (CI `macos-15`/`macos-15-intel` runners): `brew install` → `ardur --version` matches tag; formula rejects a tampered tarball (checksum mismatch test) |
| P2 | npm: `@ardurai/ardur` meta-package + per-platform packages (`@ardurai/ardur-darwin-arm64` etc.) carrying the release binaries; postinstall-free (binary resolved via `optionalDependencies` + `bin` shim, no network fetch at install time — the binary IS the platform package); provenance: `npm publish --provenance` from the release workflow | `npm i -g @ardurai/ardur` on macOS arm64 + Linux x86_64 CI → `ardur --version`; `npm audit signatures` passes; package README carries the verify-blob instructions |
| P3 | cargo-binstall + install.sh: `[package.metadata.binstall]` pkg-url template in crates/cli; `install.sh` (checksum-verifying, target-detecting, no sudo default) hosted in-repo and served from the site; README/site install section rewritten to real channels (brew, npm, binstall, curl-sh, docker, source) | `cargo binstall ardur-cli --dry-run` resolves the right URL per target; `sh install.sh` on a clean Linux container and macOS runner installs and verifies; README no longer implies source-build is the only path |
| P4 | crates.io evaluation (explicitly decide, don't drift): workspace has 60+ path-dep crates; publishing the full graph is heavy. Decide: publish `ardur-cli` + minimal dep closure, or skip crates.io and document binstall-from-GitHub as the cargo-native channel | ADR recording the decision with the dep-closure count as evidence; if publishing: `cargo publish --dry-run` green for the chosen set |
| P5 | Site: settle production baseURL (owner decision: ardurai.dev vs GitHub Pages default), publish /install page generated from the release matrix so docs never list a target that CI doesn't build | site /install lists exactly the targets release.yml builds (generated, not hand-written — same evidence-linked pattern as /status #515 S0) |

Sequencing: P0 gates everything. P1 ∥ P2 ∥ P3 after P0. P4/P5 independent. No collision with live lanes (release.yml is untouched by E4.3/channel-env/TUI; site work coordinates with #515 which owns the Hugo surface).

## 3. Security invariants (this is a supply-chain feature)

- No channel ships an unsigned/unsummed artifact; brew formula and install.sh verify sha256 before install; npm platform packages are content-addressed by the registry and published with provenance.
- The npm shim never downloads at postinstall time (no TOFU network fetch); the binary rides the platform package.
- Version-pin guard tests: formula/npm/binstall templates assert on the tag, not resolved digests (hermetic in CI, still catch a bump — same rule as the Docker image-pin guards).
- macOS binaries: notarization/codesign decision recorded at P0 (unsigned binaries trip Gatekeeper for brew-from-tarball less than direct download, but the decision must be explicit, not discovered by users).

## 4. Risks

- Linux arm64 lane cost: cross-compilation (`cross` or zig) vs native arm64 runners — pick at P0 with build-time evidence; the org already runs ARM64 self-hosted runners elsewhere, but public CI must not require private runners.
- Workspace `cargo install --path` uses a lockfile that must stay in sync — P3's binstall metadata rides the existing crate, no new drift surface.
- brew audit rules (formula in a tap are laxer than homebrew-core; homebrew-core submission is explicitly OUT of scope until adoption warrants it).

## 5. Tracker mapping

Beads epic + one child per milestone. Relates to: #501 (npm funnel rationale), #148 (migration funnel lands after install works), #515 (site /install page pattern), #149 (marketplace signing posture continuity). Does not touch #502/#504/#531 scopes.

## GSTACK REVIEW REPORT

Design-review discipline pass (self-review, single author; owner is the gate).

| Section | Findings | Disposition |
| --- | --- | --- |
| Scope challenge | crates.io full-workspace publish would be 60+ crates of maintenance for near-zero user value vs binstall — motivating source: Cargo.toml workspace member list [P1, conf 8/10] | FIXED: P4 is an explicit decision gate, not assumed work |
| Architecture | npm postinstall network fetch is the common but TOFU-weak pattern [P1, conf 9/10] | FIXED: platform-package pattern mandated in P2 acceptance |
| Security | Unsigned macOS binaries would surface as Gatekeeper friction discovered by users [P2, conf 7/10] | FIXED: P0 requires an explicit notarization decision |
| Tests | Docs listing targets CI doesn't build is the classic drift [P2, conf 8/10] | FIXED: P5 generates /install from the release matrix |
| Outside voice | Skipped (single-author); owner decision needed on baseURL (P5) and notarization (P0) | Recorded as limitation |

VERDICT: PASS WITH FINDINGS — P0 is the keystone; P1-P3 are parallel once the matrix exists.

**UNRESOLVED DECISIONS (owner, non-blocking to start P0):**
- Production site domain (P5) — ardurai.dev is currently a placeholder TODO in hugo.toml.
- macOS codesign/notarization identity (P0) — ship unsigned with documented `xattr` workaround, or acquire a Developer ID.
- crates.io posture (P4) — decided by ADR inside the milestone.
