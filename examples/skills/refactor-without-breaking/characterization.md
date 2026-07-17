# Characterization tests

A characterization test asserts what code *currently does*, not what it
*should* do. You write them around code you are about to refactor but do not
fully understand, so that any behavior change shows up as a failing test.

## How to write one when you do not know the expected output

1. Call the code with a representative input.
2. Assert against a placeholder you know is wrong, e.g. `assert result == "?"`.
3. Run it. The failure message prints the *actual* value.
4. Paste the actual value into the assertion.
5. Re-run. Green.

You have now pinned the current behavior without having to derive it by reading.
The test documents "this is what it did on this date", which is exactly the
guarantee a refactor needs.

## What to characterize

- The common case — typical, valid input.
- The boundaries — empty input, a single element, the maximum size.
- The error paths — what it does on bad input today (throws? returns null?
  returns a default?). These are the behaviors most likely to shift unnoticed
  during a refactor.
- Any observable side effect — what it writes, logs, or calls.

## Cautions

- Characterization tests can lock in bugs. That is intentional and temporary:
  they exist to detect *change*, not to bless current behavior forever. When you
  later fix a bug, you update the characterization test in the same commit, and
  that diff is the record of the fix.
- Prefer the highest level you can still make deterministic. A snapshot of a
  function's full output is more robust to internal refactoring than asserting
  on intermediate state the refactor is meant to remove.
- Pin non-determinism (clocks, RNG, ordering) or the test characterizes noise.
