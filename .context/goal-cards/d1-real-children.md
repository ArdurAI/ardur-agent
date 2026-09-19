/goal [ardur-agent] D1 — Real provider children behind delegate_task: managed lifetime, action-boundary revocation, truthful settlement (gh#490 via gh#531 · beads ardur-agent-wu7.2)

MISSION
Replace delegate_task's in-memory echo child with a real ChildSupervisor-driven provider child: attenuated cap-token, CAS budget ledger, permit lifetime bound to ACTUAL worker lifetime, revocation checked at action boundaries, actual-cost settlement. This is the keystone D2/D3/D4 stack on. Owner authorization for real provider children is RECORDED on gh#490 ("Owner explicitly authorized" trail) — cite it, don't re-ask. SPEED-FIRST: wire the existing delegate-child crate (#516); invent nothing it already provides.

CANONICAL REFS (re-verify at session start — state moves between sessions)
- https://github.com/ArdurAI/ardur-agent/issues/490 — READ FULLY: "Additional scoped acceptance from the cross-boundary review" + "Retained acceptance" are the contract. #410 (steer/stop) is NOT this slice — D2 owns it.
- https://github.com/ArdurAI/ardur-agent/issues/531 §3 D1 row, §4 invariants.
- Beads: ardur-agent-wu7.2 (read id back from `bd list --all | grep wu7`).
- Skills: ardur-agent-implementation (mandatory — its test-design rules section encodes this exact crate's past bugs), gstack, hermes-kanban-goals.

VERIFIED CONTEXT (dev a8893e1, 2026-09-18 — re-verify each)
- crates/delegate-child (merged PR #516): ChildSupervisor<P: Provider + ?Sized> (?Sized is deliberate — the production handle is Arc<dyn Provider>; a test constructs through the trait object, keep it), ChildSpec, ChildHandle::cancel, ChildOutcome (verb/rounds/is_success), ChildError, ParentBudget::reserve -> BudgetReservation (CAS ledger; settle/record_spend/worst_round_cents/is_overdrawn), TokenAuthority (attenuated tokens).
- crates/delegate-tool: DelegateTaskTool — registered delegate_task tool; Semaphore admission (with_max_concurrency), with_deny_list (durable shared deny #489/#494), cap-token verify per child turn. THE ECHO CHILD LIVES HERE — this is your primary edit surface.
- crates/multi-agent: CapVerifyingRuntime — older analog, do NOT extend it; delegate-child supersedes for this path.
- Provider objects come from provider-selector (Arc<dyn Provider>). If D0's router has merged by the time you wire, take the routed provider transparently through the same seam — but D1 does NOT depend on D0; build against plain select()/from_env.
- Known past bugs in THIS surface (each has a guard-test rule in the ardur-agent-implementation skill — apply them): dropped JoinHandle read as termination; permit released while worker alive; post-hoc is_overdrawn missing mid-flight overrun (assert dispatch/bill COUNTS, not outcomes); budget passed by value double-spending (must be shared ledger CAS); fixture factories minting fresh tokens making revocation tests vacuous (thread ONE token; assert !token.revocation_ids().is_empty()); cancellation mutations reading VACUOUS because two independent stop paths exist (mutate ALL paths in one mutation; keep provider rounds SHORT relative to the observation window; assert spun_before_cancel > 0 then frozen count); completion requiring accepted finish reason, not just non-empty content.

DO-NOT-TOUCH (live lanes; check `git worktree list` + open PRs first)
- crates/fused-runtime/**, crates/cli/src/main.rs, cli_approvals.rs, server receipt/journal surfaces — E4.3 lane (ardur-agent-r2u.2). If server wiring for delegate needs a file E4.3 holds, build the library seam + tests standalone and record the integration debt on the beads item — do NOT edit into E4.3's scope.
- crates/cli TUI modules / lib.rs dispatch / fused.rs — TUI M1 lane.
- Sibling lanes NOW: D0 (provider-runtime router module, provider-selector, config) and P0 (release.yml). Overlap risk with D0 is provider-selector — you CONSUME it, don't edit it; if an edit seems needed, comment on both beads items and coordinate rather than racing.
- PR #519 (dependabot) not yours.

NON-NEGOTIABLES
- NO AI attribution in any artifact. git commit -s (DCO). Conventional messages. English only.
- Fail-closed: a child that cannot be verified does not run; ambiguous = denied; typed errors end-to-end (no reason-string classification, no `_ =>` wildcard arms on new enums).
- No secrets anywhere. Child output is UNTRUSTED data — through injection-defense before parent context.

SETUP (exact)
  cd ~/repos/ardur-agent && git checkout dev && git pull --ff-only
  python3 scripts/agent_bootstrap.py
  git worktree add dev-workspace/d1-real-children -b gnanirahulnutakki/490-real-provider-children origin/dev
  cd dev-workspace/d1-real-children
  bd update ardur-agent-wu7.2 --status in_progress
  bd comment ardur-agent-wu7.2 "claimed: branch gnanirahulnutakki/490-real-provider-children, worktree dev-workspace/d1-real-children, scope: delegate-tool (echo->supervisor), delegate-child (gaps only), tests. Base $(git rev-parse --short origin/dev). Owner authorization cited from gh#490 comment trail."

WORK ITEMS (acceptance + evidence contract per item)
1. Wire ChildSupervisor into DelegateTaskTool
   - delegate_task spawns a supervised provider child: TokenAuthority attenuates the parent token (audience/tool/budget carve-outs); ParentBudget::reserve BEFORE spawn; typed refusal when reservation fails.
   - Supervisor retained OUTSIDE the returned future (Arc<Mutex<Option<JoinHandle>>> pattern per #503 lesson): a caller dropping the result future must NOT detach or orphan the worker; drain reports a lost worker instead of hanging.
   - Semaphore permit held for the worker's ACTUAL lifetime: acquire before spawn, release on supervised termination only. Guard: cancel the awaiting future mid-flight, assert capacity is NOT released while the worker still runs, then released after real termination.
   Evidence: test names + exit 0 for: real-child-turn-completes; dropped-future-not-detached; permit-lifetime-vs-cancelled-waiter; reservation-refusal-typed.
2. Action-boundary revocation
   - Deny-list checked immediately BEFORE EACH provider dispatch (per-round), distinct from interrupting an already-dispatched request. Revocation mid-child: current round may finish; next round refuses with the typed revocation denial; settlement records actual spend to that point.
   - Restart: revoked token persisted in the durable deny-list refuses resume after process restart.
   - Thread ONE token through deny-list and spec; assert revocation_ids() non-empty (vacuity trap from this repo's history).
   Evidence: revocation-during-active-child test; restart-with-revoked-token test (fresh store open, real process boundary or documented equivalent); both RED-proven first.
3. Truthful settlement
   - Actual per-round CostTuples recorded via record_spend; admission per round against max(declared_envelope, worst_actually_billed) — the overrun test asserts the DISPATCH COUNT stays frozen, not just a terminal error.
   - Settle on: success, child failure, cancellation, revocation — all four paths release unspent reservation; assert ledger balance exactly.
   - Child success requires an accepted finish reason; a partial-content-then-error turn is a failure carrying its real cost, never a success verb.
   - No invented costs: a round with no receipt contributes no committed cost (unknown stays unknown; reservation still released per policy).
   Evidence: four settlement-path tests with exact expected ledger values; overrun test asserting count; finish-reason contrast test.
4. Typed child-denial taxonomy
   - Audience/tool/budget/revocation denials distinct end-to-end through delegate_task's result (gh#490 names Internal-flattening as a defect). Exhaustive match, no wildcard arm.
   Evidence: one test per variant asserting the SPECIFIC diagnostic (not just the variant — this repo's weak-key lesson).
5. Docs: delegate_task section in RUN.md — real children, budget semantics, revocation timing (round-boundary, not mid-request), what is NOT in this slice (steer/stop = #410/D2; harness children = D3).

HALLUCINATION GUARDS
- Grep for existing symbols before writing new ones; delegate-child already has most of what you need — wiring, not invention. Check workspace.members for any crate touched.
- Every claim carries command + exit code/URL. Counts read from output, never memory. Identifiers (beads ids, PR numbers, SHAs) read back from tools.
- Guard tests RED->GREEN->RED-restored; name the offending line in the RED failure. Before believing VACUOUS: assert the mutation target string still exists; count independent defense paths and mutate them ALL in one mutation.
- Fixture peers exit when the real peer would stop; fixture sleeps just above the test's own timeout, never 600s.
- `let Ok(x) = … else { return; }` in a test is forbidden — expect(), so a broken fixture fails loudly.
- No "restart" proof via same-process re-open sleight: release the writer lease properly (join/drain, drop supervisors) before reopening a settlement store; canonical temp roots on macOS (/var aliases rejected by the trusted-path contract).

GATE (speed posture)
- Iterate: `cargo test -p ardur-delegate-tool -p ardur-delegate-child`.
- FULL 10-command gate ONCE when the diff settles (same list as the repo playbook; paste exit-0 tails). CUT (state in PR): full workspace test locally (CI covers), fresh gstack design pass (#531 D1 row is the reviewed design; run the pre-emit quote check on your own findings only), broad doc updates. KEPT: 10 commands, DCO, fail-closed, RED-proven guards, scope discipline.

PR + MERGE PROTOCOL (auto-merge; bounded force)
- PR to dev: `feat(delegate-tool): real provider children with managed lifetime and truthful settlement (#490)`.
- Arm auto-merge immediately: gh pr merge <N> --squash --auto --subject "feat(delegate-tool): real provider children with managed lifetime and truthful settlement (#490) (#<N>)" --body "$(printf 'D1 per #531; echo child replaced by supervised provider child.\n\nSigned-off-by: %s' "$(git log -1 --format='%an <%ae>')")"
- @codex review with NUMBERED items: 1 permit-vs-worker lifetime, 2 revocation timing per-round vs mid-request, 3 settlement on all four exit paths + ledger exactness, 4 denial taxonomy exhaustiveness, 5 injection-defense on child output, 6 no fused-runtime/main.rs edits (paste `git diff --stat origin/dev`). Bots skip dev-based PRs sometimes — note and proceed on green checks; never fabricate a review.
- Dispositions visible: fixed/filed+guard/deferred+reason. Unresolved threads block merge (ruleset) — GraphQL resolveReviewThread after dispositioning; never --admin (both blockers have safe paths: rebase+--force-with-lease for BEHIND, GraphQL for threads; if truly stuck, ask the owner with the exact blocker).
- NEVER gh pr update-branch (DCO brick, #418). Rebase + --force-with-lease; verify claimed DCO failures with `git log -1 --format=fuller` before rewriting anything.
- Sibling merge race (D0/P0 same window): after any sibling lands, fetch+rebase+force-with-lease, wait a FULL new CI cycle, then merge. Watcher: gh pr checks exit codes; pin the head SHA per poll; 502/TLS = keep waiting.
- A new commit answering review restarts long CI — budget the cycle, don't merge on the previous SHA's evidence.

CLOSE-OUT
- dev pull; re-run boot_smoke + cli_smoke_echo + `cargo test -p ardur-delegate-tool` on merged dev; paste exits.
- bd close ardur-agent-wu7.2 --result "merge SHA, PR URL, 10/10 gate, guard tests <names> RED-proven, four settlement paths pinned, revocation restart evidence" (name any waived item explicitly).
- Comment gh#490: which retained-acceptance boxes this closes (real child + revocation propagation + durable deny survive-restart + both guard tests) and what remains open (steer/stop = #410/D2; per-dispatch harness children = D3). Comment gh#531: D1 landed, D2/D3/D4 unblocked.
- Knowledge capture once (~/.claude/skills/ai-dev-team/scripts/knowledge.py) — the supervisor/permit seam and any new trap.
- Remove worktree; clear its target/ if disk is tight. LOOP RULE: D1 done -> D2 is the next unblocked item in THIS lane (fresh session/card); do not start it in this session. STOP.

BLOCKED RULE
- E4.3-owned file needed: standalone seam + integration-debt note on beads; do not edit their files. Exact-ask precedent: PR #508's settlement boundary stop.
- Infra flake in untouched crate: name it, re-run once, file on reproduce. No retry-loop normalization.
- Owner input: bd block --kind needs_input with the exact question after finishing independent items.
