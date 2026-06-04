---
name: code-review
description: Review a diff for correctness, security, and clarity issues.
metadata:
  category: review
  version: 1
---
# Reviewing a change

Read the diff and report concrete, actionable findings. Prefer a few
high-confidence issues over a long list of nitpicks.

Check, in order:

1. **Correctness** — off-by-one errors, inverted conditions, unhandled `None`/
   error paths, resource leaks, race conditions.
2. **Security** — unvalidated input, injection, secret handling, unsafe
   defaults, missing authorization checks.
3. **Clarity & reuse** — duplicated logic, dead code, names that mislead,
   functions that could reuse an existing helper.
4. **Tests** — does the change ship coverage for its new behavior and edge
   cases?

For each finding, cite the file and line, state the impact, and propose a fix.
Distinguish blocking issues from optional suggestions.
