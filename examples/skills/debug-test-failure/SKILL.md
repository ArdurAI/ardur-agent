---
name: debug-test-failure
description: Systematically diagnose a failing test instead of guessing at fixes.
metadata:
  type: engineering
  version: 1
---
# Diagnosing a failing test

A failing test is a measurement, not an obstacle. Work the failure as evidence
before touching any code. The goal is to know *why* it fails before you change
anything — a fix applied without a diagnosis usually moves the failure rather
than removing it.

## 1. Read the failure, all of it

Start from the raw output, not your memory of what the test does.

- Find the assertion that failed and the exact line it failed on. Frameworks
  bury this under stack frames; the first frame inside *your* code is usually the
  one that matters.
- Record the **expected** and **actual** values verbatim. The shape of the
  mismatch tells you a lot: off-by-one, wrong type, empty vs populated,
  `None`/`null` where a value was due, a timestamp or ordering difference.
- Note whether it is an assertion failure, an uncaught exception, a timeout, or
  a panic/crash. These have different root-cause families — a timeout is rarely
  the same kind of bug as a wrong return value.
- Scan for a *second* failure. The first error often causes the rest; fix the
  first and the cascade may clear.

## 2. Confirm it is the test you think it is

Run the single test in isolation before forming any theory:

```
# pick the form your runner uses
cargo test path::to::test_name -- --exact --nocapture
pytest tests/test_mod.py::test_name -x -vv
go test ./pkg -run '^TestName$' -v
```

Two outcomes, two meanings:

- **Fails in isolation** — the bug is in the unit under test or this test's own
  setup. Good; you have a clean reproduction.
- **Passes in isolation, fails in the suite** — this is a *test-interaction*
  bug: shared global state, leaked fixtures, ordering dependence, a database or
  temp file not reset between tests, or a parallel-execution race. Hunt the
  state that leaks across tests, not the assertion.

## 3. Isolate the assertion

Narrow the failure to the smallest claim that is false.

- If the assertion compares a large structure, compare fields one at a time
  until you find the field that differs. Assert on that field alone.
- If it loops, find the first iteration that fails and pin the inputs for that
  iteration.
- Replace a complex expected value with an inline literal you computed by hand.
  If your hand-computed value also fails, the bug is in the code; if it passes,
  the bug was in how the test built its expectation.

## 4. Inputs versus expectation

A test fails for exactly one of two reasons. Decide which:

- **The code is wrong.** The inputs are right, the expectation is right, the
  output is wrong. Fix the code.
- **The test is wrong.** The expectation encodes an assumption that is no longer
  (or was never) true, or the test feeds inputs that can't occur in practice.
  Fix the test — but only after you are sure the production behavior is correct.

Do not assume the code is at fault. A test that was passing and now fails after
a deliberate behavior change should be *updated*, and the update is part of the
change, not an afterthought.

## 5. Look at what changed

If the test passed before, something moved it. In order of likelihood:

- Your working-tree changes. `git diff` and read them against the failing
  assertion. Did you change a default, a signature, an ordering, an error type?
- A dependency or toolchain bump. A lockfile change can alter behavior with no
  source change at all.
- Shared fixtures or test helpers edited for another test.
- Environment: timezone, locale, filesystem case-sensitivity, CPU count
  (parallelism), clock resolution, available memory.

`git bisect` earns its keep here when the regressing change is not obvious — let
the test itself be the bisect predicate.

## 6. Build a minimal reproduction

Strip the failure to the fewest lines that still reproduce it. A minimal repro
is both the fastest path to the cause and the seed of the regression test you
will add. See `@./repro-checklist.md` for how to drive a reproduction down to
its core.

## 7. Fix, then prove the fix

- Make the smallest change that addresses the *cause* you identified, not the
  symptom the assertion reported.
- Re-run the single test: it must pass.
- Re-run the whole suite: you must not have traded one failure for another.
- If the bug was a real defect (not just a stale expectation), add or keep a
  test that fails before your fix and passes after. A fix with no covering test
  invites the same regression back.

## Anti-patterns

- Loosening an assertion (`assert x > 0` weakened to `assert x >= 0`) to make
  red turn green. That hides the bug; it does not fix it.
- Adding a `sleep` to "fix" a timeout or flake. Sleeps mask races and rot into
  slow, still-flaky tests. Find the actual synchronization point.
- Marking the test skipped/ignored without a tracked reason and a follow-up. A
  silently skipped test is worse than a failing one — it lies about coverage.
- "Fixing" by re-running until it passes. Intermittent passing is itself the
  bug report.
