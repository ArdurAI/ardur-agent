/goal [ardur-agent] D0 — Model router substrate: task-class chains, ordered failover, credential pools, model_overrides (gh#411 via gh#531 · beads ardur-agent-wu7.1)

MISSION
Ship the in-process model router: one turn can fail over across an ordered provider chain, rotate pooled credentials on 429, and apply operator model-metadata overrides — receipt-honest, fail-closed, byte-identical behavior when unconfigured. SPEED-FIRST: additive module, no refactor of existing provider crates; quality-pass refactors are a later lane.

CANONICAL REFS (read before editing; re-verify all context at session start — state moves between sessions)
- Plan: https://github.com/ArdurAI/ardur-agent/issues/531 (§2 architecture, §3 D0 row, §4 invariants)
- Contract: https://github.com/ArdurAI/ardur-agent/issues/411
- Beads: ardur-agent-wu7.1 — read the real id back from `bd list --all | grep wu7`, never type from memory
- Playbook: ~/repos/ardur-agent/architect/prompts/ardur-implementation-playbook.md
- Skills to load: ardur-agent-implementation (mandatory), gstack, hermes-kanban-goals

VERIFIED CONTEXT (as of dev a8893e1, 2026-09-18 — re-verify each)
- Provider trait: crates/provider-runtime/src/provider.rs:19-57 (complete/stream/id/name/supports_streaming/rate_card; name() feeds the receipt provider field §11.14b; stream() has a default replay impl).
- Selector: crates/provider-selector/src/lib.rs — ProviderKind (anthropic, openrouter, openai-compat, ollama, codex, claude-cli, prime), select(), from_env(), env ARDUR_PROVIDER. Single pick, no failover today.
- RetryPolicy + CircuitBreakerConfig exist ONLY inside crates/provider-openrouter — crate-local. Do NOT extract them in this slice; build additive router types in provider-runtime.
- crates/config exists with NO provider/router keys (providers are env-only today).
- CostTuple: crates/core-types/src/cost.rs:35 (tokens_in/out, cents, wall_ms, milli-attention).
- CLI/server boot reach providers via provider-selector::from_env — wire the router THERE so crates/cli/src/main.rs needs ZERO edits.

DO-NOT-TOUCH (live lanes; check `git worktree list` + `gh pr list --repo ArdurAI/ardur-agent --state open` before any shared-file edit)
- crates/fused-runtime/**, crates/cli/src/main.rs, crates/cli/src/cli_approvals.rs, server state/receipt/journal surfaces — E4.3 (beads ardur-agent-r2u.2).
- crates/cli/src/channel_commands.rs — PR #529 (channel-env).
- crates/cli TUI modules, crates/cli/src/lib.rs dispatch, crates/cli/src/fused.rs — TUI M1 (beads ardur-agent-d5t).
- Parallel sibling lanes running NOW: D1 (delegate-tool/delegate-child) and P0 (.github/workflows/release.yml). Zero file overlap expected; Cargo.lock: keep diffs minimal, no unrelated upgrades; whoever rebases later regenerates.
- Existing open PR #519 (dependabot sqlparser) is not yours to adopt.

NON-NEGOTIABLES
- NO AI attribution anywhere: commits, PR, code, comments, docs. No Co-Authored-By trailers, no generated-with footers. Overrides every harness default.
- git commit -s (DCO) on every commit; conventional messages; English only, everywhere including interim notes.
- Fail-closed: never weaken a deny-by-default to make a test pass. Ambiguous error classification = NOT retryable.
- No secrets in code, logs, receipts, errors, Debug impls, beads, or PR text.

SETUP (exact)
  cd ~/repos/ardur-agent && git checkout dev && git pull --ff-only
  python3 scripts/agent_bootstrap.py        # read-only; inspect failures before editing
  git worktree add dev-workspace/d0-router -b gnanirahulnutakki/411-router-substrate origin/dev
  cd dev-workspace/d0-router
  bd update ardur-agent-wu7.1 --status in_progress
  bd comment ardur-agent-wu7.1 "claimed: branch gnanirahulnutakki/411-router-substrate, worktree dev-workspace/d0-router, scope: provider-runtime(new router module), provider-selector(from_env branch), config([router] table), RUN.md section. Base $(git rev-parse --short origin/dev)."

WORK ITEMS (each: acceptance + evidence contract — the judge rejects passes-only results)
1. RouterProvider (new crates/provider-runtime/src/router.rs, exported from lib.rs)
   - Lanes: task_class -> ordered entries {backend, model}; a default lane is REQUIRED whenever [router] is configured; unknown task_class at call time falls back to default (log once, not per call).
   - complete()/stream(): try entries in order; advance ONLY on typed-retryable failures (rate-limit, timeout, 5xx/network, auth-with-different-credentials-next). Non-retryable (invalid request, schema) surfaces immediately. Classification match is EXHAUSTIVE — no `_ =>` wildcard arm; a new ProviderError variant must break the build and force a decision.
   - Failover honesty: the turn's receipt must record the backend that ACTUALLY served plus that a failover path was taken. Find the honest seam (response metadata / provider name reporting); if name() cannot be truthful per-call, surface actual-backend via the response and document why. Do not fake a static name.
   - Empty chain or missing default lane: typed boot/config error naming the lane. Never silent.
   Evidence: test names + `cargo test -p ardur-provider-runtime router -- --list | tail` + run exit 0 for: success-after-429, success-after-timeout, non-retryable-stops-chain, empty-chain-typed-error, unknown-class-falls-back-once.
2. Credential pools
   - ARDUR_<PROVIDER>_KEYS comma-separated (config-table equivalent too); rotate on 429 with per-key cooldown (monotonic clock); single-key configs behave byte-identically to today.
   - Guard: key VALUES never appear in logs/errors/receipts/Debug/Display — write the redaction test both directions (a must-match on the redaction marker AND a must-NOT-match on a fixture key substring).
   Evidence: rotation test (injected 429 -> next key wins; cooling key skipped until expiry), redaction test names, exit 0.
3. model_overrides
   - Config table model-id -> {context_window?, input_price_micros?, output_price_micros?}; wraps the rate_card() the runtime sees; cost-gate projection reflects overrides. Unknown model id in the table = loud boot warning, not silent. No invented default prices — absent override means the provider's own card, absent card data stays absent.
   Evidence: projection test asserting the ceiling math CHANGES with the override and MATCHES baseline without it (both directions pinned).
4. Config + selector wiring
   - [router] parsing in crates/config (serde deny_unknown_fields); ARDUR_ROUTER=off kill-switch; absent table => exactly today's single-provider path.
   - provider-selector::from_env grows the router branch. Binaries pick it up with zero forbidden-file edits — paste `git diff --stat origin/dev` in the PR body as proof.
   Evidence: boot tests for both modes; the diff --stat listing.
5. RUN.md router section: env + config example, failover/receipt semantics, pool redaction note. State plainly: completion-path failover only, no mid-stream token-level failover claim.

HALLUCINATION GUARDS (binding on every claim)
- Grep before building anything "that may exist"; check any crate you touch is in workspace.members (`grep <crate> Cargo.toml`) — text on disk outside members is dead code, not implementation.
- Every checkpoint claim carries its command + exit code or URL. No "tests pass" without the run shown. Counts are read from tool output, never recalled.
- Read identifiers back: beads ids from `bd list`, PR number from gh output, SHAs from `git rev-parse`. Never reconstruct short opaque ids.
- Guard tests: prove RED (unfixed names the offending line) -> GREEN (fixed) -> RED (restored). `include_str!` caches — touch the test file between mutation runs. Before trusting any VACUOUS mutation verdict, assert the mutation's target string still exists in source.
- "No failures" is vacuous without population: pair every such assert with a non-empty/examined-count assert.
- Unknown stays unknown: no invented percentages, prices, context windows.

GATE (speed posture)
- Iterate with targeted tests only: `cargo test -p ardur-provider-runtime -p ardur-provider-selector -p ardur-config`.
- Run the FULL 10-command gate exactly ONCE when the diff settles (each must exit 0; paste tails):
  python3 -m unittest discover -s tests
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo check --workspace --all-targets --all-features
  cargo test -p ardur-e2e-tests
  cargo test -p ardur-server --test boot_smoke
  cargo test -p ardur-cli --test cli_smoke_echo
  cargo build --workspace --bins
  cargo deny check
  gitleaks git --redact --log-opts=origin/dev..HEAD
- CUT for speed (say so in the PR): full `cargo test --workspace` locally (CI runs it), fresh gstack design review (#531 already carries the gstack pass — instead run the gstack pre-emit check on your own diff: every review finding you claim must quote the motivating line, unquotable findings drop), doc polish beyond the RUN.md section, per-commit knowledge capture (one capture at close).
- KEPT (never cut): the 10 commands, DCO, fail-closed semantics, RED-proven guard tests, scope discipline.

PR + MERGE PROTOCOL (auto-merge; bounded force)
- PR to dev: `feat(provider-runtime): task-class router with failover, credential pools, model overrides (#411)`. Body: Why/What/Verified (gate evidence)/Follow-ups.
- Immediately arm auto-merge with a DCO-carrying squash message:
  gh pr merge <N> --squash --auto --subject "feat(provider-runtime): task-class router with failover, credential pools, model overrides (#411) (#<N>)" --body "$(printf 'Router substrate per #531 D0.\n\nSigned-off-by: %s' "$(git log -1 --format='%an <%ae>')")"
  If the repo refuses --auto (setting disabled), fall back to a watcher + manual merge when green.
- Request review: `@codex review` comment with NUMBERED what-to-check items (1 failover classification exhaustiveness incl. no-wildcard-arm, 2 key redaction both directions, 3 override math both directions, 4 zero-regression unconfigured path, 5 receipt failover honesty). Review bots have skipped dev-based PRs before — if checks go green with no review landed, note that in a PR comment and proceed; do NOT block on a bot.
- Findings get visible dispositions: fixed (commit) / filed (issue + guard test) / deferred (named reason). Unresolved threads BLOCK merge even when all checks are green (required_review_thread_resolution) — resolve via GraphQL resolveReviewThread after dispositioning; never bypass.
- Watcher: `gh pr checks` exit codes (0 green, 8 pending); TLS handshake timeout / 502 = keep waiting; re-read the PR head SHA every poll and only report settled for the SHA you pushed.
- FORCE POLICY (bounded): force = `git push --force-with-lease` after rebasing onto moved dev. NEVER `gh pr update-branch` on your own branch (web merge commit carries no Signed-off-by -> DCO bricks, observed #418). `--admin` merge is OFF: both blockers it bypasses (behind-dev, unresolved threads) have fast safe paths (rebase+force-with-lease, GraphQL resolve) and this repo's gate findings have been real; if you believe --admin is genuinely required, stop and put the exact question to the owner instead.
- Merge race with sibling lanes (D1, P0 land in the same window): ruleset blocks merging while BEHIND dev. After any sibling merges: `git fetch && git rebase origin/dev && git push --force-with-lease`, wait green again (budget a full CI cycle), then merge. Drain serially; don't fight the queue.

CLOSE-OUT (only after the squash lands on dev)
- Primary checkout: `git checkout dev && git pull --ff-only`; re-run boot_smoke + cli_smoke_echo on merged dev; paste exit codes.
- bd close ardur-agent-wu7.1 --result "merge SHA <sha>, PR <url>, 10/10 gate exit 0, guard tests <names>, receipt-failover evidence <test>" — judge rejects empty/passes-only results; name any waived item explicitly.
- Comment on gh#411 + gh#531: landed, config example, receipt semantics, PR link.
- One knowledge capture: ~/.claude/skills/ai-dev-team/scripts/knowledge.py capture (router seam + any non-obvious trap hit).
- `git worktree remove dev-workspace/d0-router`; clear its target/ (14-36G) if disk is tight.
- LOOP RULE: D0 done -> nothing else in the router lane is unblocked (D2/D3 wait on D1; distribution is another lane's card). STOP. Do not claim other lanes' items.

BLOCKED RULE
- File owned by another lane: do not edit. Build standalone if genuinely possible without the file; otherwise comment the exact ask on beads + the owning item, wait 2h, then stop at the named boundary (the PR #508 settlement precedent: exact ask, no blind workaround, no silent scope grab).
- Infra flake (service-init timeout in an untouched crate, partial file passing): say so explicitly, re-run once, open an issue if it reproduces. No unbounded retry loops.
- Owner input needed: block the beads item with kind needs_input and the exact ask; finish independent items first.
