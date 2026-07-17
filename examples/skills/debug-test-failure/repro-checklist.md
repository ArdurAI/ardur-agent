# Minimizing a reproduction

A minimal reproduction is the smallest program that still fails the same way.
Reaching it usually finds the bug for you.

## Reduce along these axes

- **Inputs.** Shrink data to the smallest case that still fails. Halve it
  repeatedly (a manual bisect on the input). Often a single element or an empty
  collection is enough.
- **Code path.** Remove setup, mocks, and branches the failure does not need.
  After each removal, re-run: if it still fails, the removal was safe.
- **Dependencies.** Replace a real database, network call, or clock with an
  inline fake or a fixed value. If the failure survives, the dependency was not
  the cause.
- **Concurrency.** Force single-threaded execution. If the failure disappears,
  you have confirmed a race; if it stays, concurrency is a red herring.

## Pin the non-determinism

The repro must fail *every* time, or it is not yet minimal. Pin every source of
variation:

- Seed every RNG with a constant.
- Freeze the clock to a fixed instant; do not read wall-clock time.
- Sort anything whose order is unspecified (map iteration, directory listings,
  query results without `ORDER BY`).
- Set timezone and locale explicitly.

## Stop when

You can delete nothing more without the failure changing or vanishing. At that
point the remaining code *is* the explanation — read it. Then lift it into a
named regression test so the failure can never return silently.
