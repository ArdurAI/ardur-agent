# Example skills

A skill is a `SKILL.md` file: YAML frontmatter (`name`, `description`, optional
`metadata`) followed by a Markdown body. The model selects a skill by its
`description`, invokes it, receives the body, and uses that body to guide its
next step. Skills are instructions and guides — not code.

Load these by pointing `ARDUR_SKILLS_DIRS` at this directory (or a copy). Each
skill lives in its own folder; some include `@./resource.md` files for
[progressive disclosure](git-commit-message/conventions.md) — the body
references them, and a caller inlines them on demand via the `expand` argument
rather than paying for them every time.

These are starting points. Copy a folder, edit the body to match how your team
actually works, and drop it into your skills directory.

## Available skills

| Skill | Type | What it's for |
|-------|------|---------------|
| [git-commit-message](git-commit-message/SKILL.md) | git | Draft a Conventional-Commits message for a staged diff. |
| [code-review](code-review/SKILL.md) | review | Review a diff for correctness, security, and clarity issues. |
| [debug-test-failure](debug-test-failure/SKILL.md) | engineering | Systematically diagnose a failing test instead of guessing at fixes. |
| [refactor-without-breaking](refactor-without-breaking/SKILL.md) | engineering | Restructure code without changing behavior, one verifiable step at a time. |
| [investigate-performance-regression](investigate-performance-regression/SKILL.md) | engineering | Narrow down what made something slower using measurement, not intuition. |
| [api-design-review](api-design-review/SKILL.md) | code-review | Review a new or changed API surface against the contract concerns that bite later. |
| [write-runbook](write-runbook/SKILL.md) | incident | Turn an incident response into a reusable runbook the next on-call can follow. |
| [write-postmortem](write-postmortem/SKILL.md) | incident | Write a blameless postmortem that turns an incident into durable improvements. |
| [onboard-new-engineer](onboard-new-engineer/SKILL.md) | onboarding | Structure a new engineer's first week so they ship and build context, not just read. |
