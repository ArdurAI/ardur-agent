/goal [ardur-agent] P0 — Release matrix: macOS arm64/x86_64 + Linux arm64 with sigstore/SBOM/SHA256SUMS parity (gh#532 · beads ardur-agent-coa.1)

MISSION
Extend release.yml from its single x86_64-linux lane to 4 targets (add aarch64-apple-darwin, x86_64-apple-darwin, aarch64-unknown-linux-gnu) with EVERY artifact getting the exact same sigstore bundle + SHA256SUMS + SPDX SBOM treatment. This gates brew (P1), npm (P2), binstall (P3). SPEED-FIRST: matrix the existing job, do not redesign the release pipeline. Pure CI/workflow lane — zero Rust source edits expected, zero collision with D0/D1.

CANONICAL REFS (re-verify at session start)
- https://github.com/ArdurAI/ardur-agent/issues/532 (§2 P0 row, §3 supply-chain invariants)
- Beads: ardur-agent-coa.1 (read back from `bd list --all | grep coa`)
- Skills: ardur-agent-implementation (gate + PR mechanics), gstack, hermes-kanban-goals

VERIFIED CONTEXT (dev a8893e1 — re-verify each)
- .github/workflows/release.yml: runs-on ubuntu-latest, single implicit host target, finds executables under target/release, packages `<name>-<tag>-x86_64-unknown-linux-gnu.tar.gz`, sigstore-bundles each, SHA256SUMS (+bundle), SPDX SBOM (+bundle). v0.2.0 shipped 6 binaries this way: ardur, ardur-server, ardur-admin, ardur-eval, ardur-healthcheck, ardur-memory-eval.
- Guard tests exist pinning toolchain `channel = "1.98.1"` and exact release-action SHAs — locate them (`grep -rn "1.98.1" --include="*.rs" crates | grep -v target`; `grep -rn "release" crates/*/tests 2>/dev/null | grep -i "sha\|action\|workflow" | head`). If your workflow edits change any pinned assertion, bump the assertion IN THE SAME PR — a red guard after merge is a self-inflicted dev break.
- CI runners available publicly: macos-15 (arm64), macos-15-intel or macos-13 (x86_64 — VERIFY current GitHub-hosted labels before use, do not assume), ubuntu-latest (x86_64), ubuntu-24.04-arm (arm64 — VERIFY availability for public repos; if unavailable, cross-compile with cross or zigbuild and record the choice + build-time evidence in the PR).
- Org self-hosted ARM64 runners exist elsewhere but PUBLIC CI MUST NOT require private runners (repo standard).
- Docker lane (docker.yml) is separate and NOT in scope.

DO-NOT-TOUCH
- All Rust source except a guard-test assertion bump forced by the workflow change.
- Live lanes now: D0 (provider-runtime/selector/config), D1 (delegate-tool/child), E4.3, TUI M1, PR #529. Your surface (.github/workflows/release.yml + maybe a scripts/ helper + guard tests) overlaps none of them.
- ci.yml/dco.yml/site-deploy.yml unless a release-lane assertion genuinely requires it (state why in the PR if so).

NON-NEGOTIABLES
- NO AI attribution. git commit -s (DCO). English only.
- Supply-chain parity is the acceptance, not a stretch goal: an unsigned or unsummed artifact on ANY new target = the milestone is NOT done. Never ship a subset silently.
- Exact release-action SHA pinning continues for any new action step (no floating tags).

SETUP (exact)
  cd ~/repos/ardur-agent && git checkout dev && git pull --ff-only
  python3 scripts/agent_bootstrap.py
  git worktree add dev-workspace/p0-release-matrix -b gnanirahulnutakki/532-release-matrix origin/dev
  cd dev-workspace/p0-release-matrix
  bd update ardur-agent-coa.1 --status in_progress
  bd comment ardur-agent-coa.1 "claimed: branch gnanirahulnutakki/532-release-matrix, worktree dev-workspace/p0-release-matrix, scope: .github/workflows/release.yml, guard-test bumps if forced, docs/release notes section. Base $(git rev-parse --short origin/dev)."

WORK ITEMS (acceptance + evidence contract)
1. Matrix the build job
   - 4 lanes: {x86_64-unknown-linux-gnu on ubuntu, aarch64-unknown-linux-gnu on arm runner OR cross-compiled (record decision), x86_64-apple-darwin on verified intel-mac label, aarch64-apple-darwin on macos-15}.
   - Same 6 binaries per lane; artifact names templated `<name>-<tag>-<target>.tar.gz`; macOS lanes tar with the right binary format (no lipo/universal in this slice — 2 thin macOS artifacts).
   - Sigstore bundle per artifact per lane; ONE merged SHA256SUMS covering all lanes (+bundle); SBOM stays release-level (verify whether the current SBOM is per-binary or per-release and keep its semantics — do not silently change granularity).
   - Rust toolchain in every lane matches the pinned 1.98.1 channel.
2. Notarization decision (owner-gated, do not decide silently)
   - Ship THIS PR with unsigned macOS binaries + a documented `xattr -d com.apple.quarantine` note ONLY if the owner confirms; otherwise hold the macOS lanes behind the decision. Post the decision request EARLY (see BLOCKED RULE) so the Linux-arm64 lane isn't hostage to it: if undecided by PR time, land linux-arm64 + matrix scaffolding with macOS lanes present but commented/flag-gated, and record the exact pending decision. Do not invent an Apple Developer ID situation.
3. Verification evidence (the honest core — a workflow you cannot run on dev needs a real exercise)
   - Push a prerelease tag from the merged workflow (coordinate naming: v0.2.1-rc.1 or similar; ASK the owner if any tag push is release-sensitive — tags are public) OR run a workflow_dispatch variant with the packaging steps against a non-tag ref if the workflow supports it; the acceptance needs one REAL run URL producing all lanes' artifacts.
   - For one artifact per lane: download and verify — `sha256sum -c` against SHA256SUMS AND a documented `cosign verify-blob --bundle` (or gh attestation verify) command that exits 0. Paste all four verify outputs.
   - macOS artifact smoke: `./ardur --version` on the matching runner arch inside the workflow (a build that tars a broken binary is worse than no lane).
4. Docs: release/verification section update (README or docs/) listing targets + the exact verify commands; only targets the workflow actually builds.

HALLUCINATION GUARDS
- VERIFY runner labels against current GitHub docs/api before writing them (`gh api /repos/ArdurAI/ardur-agent/actions/runners` shows self-hosted only; for hosted labels check the docs page — do not recall from memory, labels churn).
- Any "workflow is green" claim = run URL + conclusion field read from `gh run view`. Any artifact claim = the downloaded file's checksum output pasted. Never assert lane success from the matrix summary alone — open the lane's job.
- Watcher discipline: pin the SHA/run id you're watching; re-read each poll; 502/TLS = keep waiting, never settled.
- If cross-compiling arm64: prove the artifact actually targets aarch64 (`file` output pasted), not just that the job exited 0.
- Guard-test bumps: run the specific test RED before your bump (proving it watches the workflow) and GREEN after — a bump landed blind is a disabled guard.

GATE (speed posture)
- This lane is workflow-only; the full 10-command repo gate still runs ONCE before the PR (workflow edits can still break Python tests that lint workflows, and the guard tests are Rust). Paste exit-0 tails.
- CUT (state in PR): homebrew-core-grade polish, universal binaries, Windows, notarization implementation (decision only), per-lane cache tuning beyond correctness. KEPT: signing parity, checksum verification evidence, pinned actions, guard-test sync, smoke-run of built binaries.

PR + MERGE PROTOCOL (auto-merge; bounded force)
- PR to dev: `ci(release): build matrix for macOS arm64/x86_64 and Linux arm64 with signing parity (#532 P0)`.
- Arm auto-merge immediately: gh pr merge <N> --squash --auto --subject "ci(release): 4-target release matrix with signing parity (#532 P0) (#<N>)" --body "$(printf 'P0 per #532; gates brew/npm/binstall lanes.\n\nSigned-off-by: %s' "$(git log -1 --format='%an <%ae>')")"
- @codex review NUMBERED: 1 signing/SUMS parity across lanes, 2 pinned action SHAs on new steps, 3 runner-label validity, 4 guard-test sync, 5 no source edits beyond declared bumps (`git diff --stat origin/dev` pasted). Bots may skip dev-based PRs — note and proceed on green; never fabricate.
- Dispositions visible; unresolved threads block merge -> GraphQL resolveReviewThread after dispositioning. NEVER --admin; NEVER gh pr update-branch on your own branch (DCO brick #418) — rebase + --force-with-lease only.
- Sibling race (D0/D1 merging same window): fetch+rebase+force-with-lease after each sibling lands; full new CI cycle before merge; drain serially.

CLOSE-OUT
- Merged dev pull + boot_smoke/cli_smoke_echo re-run (proves no accidental source impact); paste exits.
- The real-run evidence (work item 3) must exist BEFORE closing: run URL + 4 verify outputs. If the prerelease-tag decision is still owner-pending, close is NOT allowed — block instead (needs_input) with everything else evidenced.
- bd close ardur-agent-coa.1 --result "merge SHA, PR URL, run URL, 4/4 lanes artifacts verified (sha256+cosign), macOS smoke output, notarization decision state" — name waived/pending items explicitly.
- Comment gh#532: P0 landed, P1/P2/P3/P5 unblocked (each is its own future card); notarization decision state.
- Knowledge capture once (runner labels verified, cross-compile decision, any trap).
- Remove worktree. LOOP RULE: P0 done -> P1/P2/P3 unblock but belong to NEW cards/sessions. STOP.

BLOCKED RULE
- Notarization + prerelease-tag questions go to the owner EARLY as ONE decision-sheet comment on gh#532 (recommended default per item, vetoable): (a) ship unsigned macOS + quarantine note now, Developer ID later [recommended]; (b) rc tag v0.2.1-rc.1 for the verification run [recommended] vs workflow_dispatch-only. If unanswered by PR-ready time: proceed on the recorded defaults for lane SCAFFOLDING, hold only the irreversible public actions (tag push) behind the block, per the vetoable-defaults convention.
- Infra flake (runner queue, transient 502): name it, re-run once, file on reproduce.
