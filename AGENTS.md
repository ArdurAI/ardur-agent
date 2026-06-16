# Ardur Agent Session Instructions

These instructions apply to every LLM, coding agent, and human-assisted agent
session that works in this repository.

## Mandatory Bootstrap

Before planning, editing, testing, or creating a handoff, run:

```sh
python3 scripts/agent_bootstrap.py
```

Use `--json` only when a tool needs machine-readable output:

```sh
python3 scripts/agent_bootstrap.py --json
```

The bootstrap is read-only. It reports current Linear status, project progress,
Git/worktree state, provider and memory posture, unowned `parallel:ready`
candidates, the Plan Corpus verification backlog, and the required non-
interference rules for parallel sessions.

If the generic Linear connector shows a workspace or team other than
`ardur-agent` / `ARD`, do **not** claim that work. The bootstrap uses the
Keychain-backed helper at `architect/tools/linear_graphql.py` and must be the
source for ARD work discovery.

## Source Hierarchy

1. **Linear `ARD` is the internal source of truth** for work scope, active
   state, priority, acceptance criteria, progress, blockers, and verification
   evidence.
2. **EXTENDED drive is the primary file/evidence source** for plans, journals,
   audits, local branches, and implementation evidence.
3. **GitHub `ArdurAI/ardur-agent` is the verified public code surface** for PRs,
   CI, releases, and public collaboration.
4. **Notion is a knowledge projection**, useful for durable explanation and
   navigation, but it is not the authority for whether work is complete.

## Session Ownership Rule

Before code edits, a session must claim one unowned `parallel:ready` Linear
issue. After claiming it, update Linear to `parallel:owned` or In Progress and
record:

- one Linear issue,
- one branch or isolated worktree,
- one EXTENDED-drive session journal,
- one explicit file/module scope.

Never claim issues from a non-ARD workspace while working in this repository.
Wrong-workspace Linear access is a blocker, not a reason to switch projects.

Preferred branch format:

```text
gnanirahulnutakki/ARD-<number>-<short-slug>
```

Preferred worktree location:

```text
/Volumes/EXTENDED/ardur-agent/dev-workspace/<short-slug>
```

## Parallel Session Conduct

- Prefer unowned `parallel:ready` issues from the bootstrap output.
- Treat `parallel:owned` or In Progress issues as unavailable unless Linear has
  an explicit handoff comment from the owning session.
- Treat `integration:stitch` issues as coordination targets, not isolated
  implementation tasks, until related lane work has verification evidence.
- Do not edit files owned by another active Linear issue unless Linear contains
  an explicit handoff comment naming the issue, owner, branch/worktree, files,
  and reason.
- Do not use the primary checkout for implementation when unrelated dirty files
  are present. Create an isolated worktree from `origin/dev`.
- Do not mark Linear work Done until the issue's acceptance checks have fresh
  verification evidence.
- Do not print, commit, paste, or store secret values. Report only whether
  required sensitive inputs are present or missing.
- Do not treat GitHub issues or Notion pages as overriding Linear state.

## Implementation Promotion Gate

A session must finish the full promotion path before moving to another Linear
item:

1. Run the issue-specific local tests and record the commands in Linear.
2. Push the branch and use the agreed GitHub path into `dev`.
3. Confirm every required GitHub workflow is green.
4. Merge the implementation branch into `dev`.
5. Update local `dev` from origin and run the final smoke check that applies to
   the touched area.
6. Update Linear with the PR or merge commit, workflow evidence, final test
   evidence, and final status.

Do not start the next issue from the same session until `dev` contains the
implementation and Linear has the evidence trail. If workflows fail, keep the
issue owned and fix that branch before taking new work.

## Plan Verification Work Items

The Linear project `Plan Corpus Linearization` contains one `Verify/implement
plan` issue for every plan under `/Volumes/EXTENDED/ardur-agent/plans` that was
not already proven implemented or semantically tracked.

For these issues:

1. Read the source plan from EXTENDED first.
2. Prove whether the plan is already implemented before writing code.
3. If implemented, close the Linear issue only with file/test evidence.
4. If not implemented, claim the issue, create an isolated worktree, write a
   session journal, and then implement the smallest coherent slice.
5. If the plan is too large for one session, split it in Linear and relate the
   child issues back to the source plan issue and `ARD-80`.

## Progress Semantics

- For Linear projects, use native Linear progress first: `Project.progress`,
  `Project.scope`, and `Project.currentProgress`.
- For issue-only work, compute progress from issue estimates and workflow state:
  completed estimate divided by total non-canceled estimate.
- If neither native nor computed progress is available, report `unknown`; do not
  invent a percentage.

## Test-By-Tomorrow Baseline

The required local baseline for Ardur Agent test readiness is the no-key/stub
path first, followed by optional live-provider checks only when credentials or
local CLIs are available.

Start with:

```sh
cargo test -p ardur-e2e-tests
cargo test -p ardur-server --test boot_smoke
cargo test -p ardur-cli --test cli_smoke_echo
cargo build --workspace --bins
```
