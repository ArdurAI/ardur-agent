---
name: git-commit-message
description: Draft a Conventional-Commits message for a staged diff.
metadata:
  category: git
  version: 1
---
# Writing the commit message

Produce a single Conventional-Commits message describing the **staged** changes.

1. Pick the `type`: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, or
   `perf`.
2. Add an optional `(scope)` naming the area touched.
3. Write an imperative, present-tense subject line of 72 characters or fewer:
   `type(scope): subject`.
4. If the change needs explanation, add a blank line and a wrapped body.

For the full subject-line and body conventions this repo expects, ask for the
`@./conventions.md` resource (pass it in `expand`).
