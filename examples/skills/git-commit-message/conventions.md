# Commit conventions

- **Subject:** `type(scope): subject`, imperative mood, no trailing period, ≤ 72
  chars. Lowercase the subject's first word unless it is a proper noun.
- **Body:** wrap at 72 columns. Explain *what* and *why*, not *how*.
- **Breaking changes:** start a body paragraph with `BREAKING CHANGE:` and
  describe the migration.
- **Footers:** reference issues as `Refs: #123` / `Closes: #123`.
- One logical change per commit; do not mix a refactor with a behavior change.
