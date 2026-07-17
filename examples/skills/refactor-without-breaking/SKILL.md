---
name: refactor-without-breaking
description: Restructure code without changing its behavior, one verifiable step at a time.
metadata:
  type: engineering
  version: 1
---
# Refactoring without breaking things

Refactoring means changing the structure of code without changing what it does.
The discipline that makes it safe is keeping those two things — structure and
behavior — strictly separate, and proving behavior is unchanged after every
step. If you cannot tell whether behavior changed, you are not refactoring; you
are rewriting and hoping.

## The one rule

**Never change behavior and structure in the same commit.** A commit either:

- changes *behavior* (tests change, output changes) and leaves structure as-is,
  or
- changes *structure* (moves, renames, extracts) and every test passes before
  and after, unchanged.

When the two are mixed and something breaks, you cannot tell which change broke
it. Keeping them separate means a green test suite after a structural commit is
a real guarantee, and a failing one points at exactly the move you just made.

## 1. Lock in current behavior first

Before changing any structure, make sure the current behavior is covered.

- Run the existing tests. Note what passes. That green is your reference point —
  you are going to preserve it exactly.
- Find the gaps. The code you are about to move is only as safe as its test
  coverage. If a branch you will touch is untested, it can break silently.
- Write **characterization tests** for uncovered behavior: tests that assert
  what the code *currently does*, even if that behavior is ugly or arguably
  wrong. You are not fixing it now; you are pinning it so you notice if a
  refactor changes it. See `@./characterization.md` for how to write tests for
  code you do not fully understand yet.
- Resist fixing bugs you find. Note them. A bug fix is a behavior change and
  belongs in its own commit, after the refactor, when you can see it clearly.

## 2. Refactor in small steps

Small enough that each step is obviously correct and easy to revert.

- Make one structural change: extract a function, rename a variable, inline a
  helper, move a type to another module, replace a magic number with a named
  constant.
- Run the full test suite. It must be **exactly as green as before** — same
  tests passing, none newly failing, none newly skipped.
- Commit. A passing structural commit is a safe point you can return to.
- Repeat.

The temptation is to do five moves at once because they are "obviously fine".
Resist it. The cost of a small step is a few seconds; the cost of debugging
which of five moves broke the suite is much larger. Lean on your tools — IDE
rename and extract refactorings are mechanical and far less error-prone than
hand-editing.

## 3. Keep the suite fast and green

- If the test suite is too slow to run after every step, run the subset covering
  the code you are touching, and the full suite before each commit.
- A flaky test poisons this whole process — you cannot trust green if green is
  unreliable. Quarantine or fix flakes *before* you start refactoring, not
  during.
- If a refactor step turns a test red, do not debug forward. **Revert the step**
  and take a smaller one. The red proves your step changed behavior; a smaller
  step will isolate where.

## 4. Separate the behavior change, if there is one

Often the reason you are refactoring is to *then* make a behavior change easier.
Good — but do it as a distinct phase:

1. Refactor until the change you want is a small, local edit. ("Make the change
   easy, then make the easy change.")
2. Commit the refactor. Tests green, behavior identical.
3. Now make the behavior change. *Now* tests change, and the diff in the tests
   is the documentation of what behavior moved.

This ordering makes review tractable: the refactor commit is large but
behavior-neutral (a reviewer can skim it trusting the green suite), and the
behavior commit is small and where the reviewer should spend their attention.

## What counts as a behavior change (do these separately)

- Changing a return value, default, or error type.
- Changing the order of side effects (logging, writes, callbacks).
- Tightening or loosening validation.
- Changing performance characteristics enough to matter (an O(n) -> O(n^2)
  "cleanup" is a behavior change in practice).
- Changing a public signature clients depend on.

## When there are no tests at all

Sometimes you inherit code with no coverage and must refactor it anyway.

- Pin behavior from the outside first: snapshot/golden tests at the highest
  level you can (the function's public output, the endpoint's response, the
  CLI's stdout). These do not require understanding the internals.
- Refactor the internals behind that snapshot. As long as the snapshot is
  unchanged, external behavior held.
- Once the structure is clearer, add finer-grained tests and retire the snapshot
  if it has served its purpose.

## Done means

The suite is exactly as green as when you started, the structure is better, and
no commit in the series mixes a behavior change with a structural one. If you
cannot say all three, you are not finished.
