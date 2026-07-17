# Agent and Session Code of Conduct

This document defines how Ardur Agent work is coordinated across LLM agents,
human operators, and parallel coding sessions.

## Operating Principle

Ardur Agent is a secure agent substrate. The development process must preserve
that security posture: every session must be accountable, isolated, auditable,
and reproducible.

## Required Session Start

Every session starts by running:

```sh
python3 scripts/agent_bootstrap.py
```

The bootstrap report is the session briefing. It tells the worker what Ardur
Agent is, which Linear work is active, what project progress Linear reports,
which `parallel:ready` issues are unowned, what local worktrees exist, and
which provider/memory paths are available.

The generic Linear connector can be authenticated to a different workspace. If
it shows anything other than workspace `ardur-agent` and team `ARD`, treat that
as unavailable for this repository and use the bootstrap's Keychain-backed
Linear GraphQL path instead.

## Source of Truth Contract

| Source | Authority |
|---|---|
| Linear `ARD` | Scope, priority, current state, active ownership, acceptance criteria, progress, blockers, completion evidence |
| EXTENDED drive | Local files, plans, session journals, audits, branch/worktree evidence |
| GitHub | Public PRs, verified merged code, CI status, release state |
| Notion | Searchable knowledge, long-form reference, decision summaries |

If sources disagree, use Linear for work state and EXTENDED for local evidence.
Write the discrepancy into the owning Linear issue before proceeding.

If Linear access resolves to another workspace, do not proceed with
implementation. Record the blocker locally, fix the Linear API/keychain path,
and only then claim ARD work.

## Non-Interference Rules

1. A worker owns exactly one Linear issue while making implementation changes.
2. A worker uses exactly one branch/worktree for that issue.
3. A worker writes one session journal under EXTENDED and references it in
   Linear.
4. A worker does not edit another session's file scope without a Linear handoff.
5. A worker does not reuse another session's branch for unrelated work.
6. A worker does not make destructive Git changes to recover from coordination
   mistakes.
7. A worker only starts from an unowned `parallel:ready` issue unless taking an
   explicit handoff.
8. A worker treats `integration:stitch` issues as coordination work, not as the
   first isolated task for a fresh parallel session.
9. A worker does not start a second implementation issue until the first
   branch is merged into `dev`, required workflows are green, and Linear has
   the evidence trail.

The minimum handoff comment in Linear must include:

```text
Handoff:
- From issue:
- To issue:
- Current branch/worktree:
- Session journal:
- Files/modules affected:
- Reason for handoff:
- Verification already run:
```

## Worktree Policy

Use an isolated worktree before code edits when the primary checkout is dirty or
when any parallel session is active.

Recommended command from `/Volumes/EXTENDED/ardur-agent`:

```sh
git fetch origin dev
git worktree add dev-workspace/<slug> -b gnanirahulnutakki/ARD-<number>-<slug> origin/dev
```

The worktree path is ignored by `.gitignore`, which keeps local session state out
of the public repository.

## Promotion Gate

Each implementation issue has one completion path:

1. Run local checks for the touched area and capture the command output summary.
2. Push the branch and create or update the GitHub PR into `dev`.
3. Wait for every required GitHub workflow to pass.
4. Merge the branch into `dev`.
5. Pull/update local `dev` from origin and run the final smoke check.
6. Add the PR or merge commit, workflow status, local smoke evidence, and any
   follow-up gaps to Linear.
7. Only then move the Linear issue to Done and choose the next `parallel:ready`
   item.

If any workflow fails, the session stays on the same Linear issue until the
failure is fixed or a handoff is recorded.

## Linear Label Contract

| Label | Meaning |
|---|---|
| `parallel:ready` | Safe for a new isolated session to claim after running bootstrap |
| `parallel:owned` | Already claimed; do not touch without a handoff comment |
| `integration:stitch` | Coordination target used to connect lane evidence and final test readiness |
| `source:plan-corpus` | Work item came from `/Volumes/EXTENDED/ardur-agent/plans` |
| `gate:needs-verification` | Requires fresh implementation/test evidence before Done |

For `Verify/implement plan` issues, the first task is verification, not coding:
read the plan, check the implementation, and close only when evidence proves the
plan is complete. If the plan is not complete, claim the issue and implement the
smallest coherent slice.

## Session Journal Policy

Create a journal before moving a Linear issue to In Progress:

```text
/Volumes/EXTENDED/ardur-agent/architect/sessions/<issue-slug>/journal.md
```

The journal records:

- Linear issue identifier and URL,
- branch/worktree,
- planned file scope,
- commands run,
- verification evidence,
- blockers and handoffs.

## Progress Policy

Use measurable progress, never intuition.

- Project progress: Linear native `progress`, `scope`, and `currentProgress`.
- Issue progress: completed estimate divided by total non-canceled estimate.
- Checklist progress: allowed only when issue estimates are unavailable; include
  numerator and denominator.
- Unknown progress: report `unknown`, not a guessed percentage.

## Security Policy

- Never print secret values. Report only `present` or `missing`.
- Never commit `.env`, Keychain exports, API keys, OAuth tokens, Slack signing
  secrets, or local model credentials.
- Prefer local no-key/stub or Ollama paths for first-pass testing to avoid
  unintended spend.
- Treat provider CLIs and model APIs as external dependencies with cost,
  latency, and failure modes.

## Verification Policy

Before claiming completion, run fresh checks and quote the command names in the
Linear issue. For the bootstrap surface, the minimum checks are:

```sh
python3 -m unittest tests.test_agent_bootstrap
python3 scripts/agent_bootstrap.py
python3 scripts/agent_bootstrap.py --json
python3 -m py_compile scripts/agent_bootstrap.py tests/test_agent_bootstrap.py
git diff --check
```

For runtime test readiness, start with the no-key baseline from `AGENTS.md` and
only add live-provider checks after confirming the relevant provider credentials
or local CLIs are present.
