# M0 frozen REPL transcripts

These are preservation fixtures for the **pre-refactor** renderer, not output
constructed by the new reducer and not ratatui snapshots.

## Source binding

- Repository base: `f3ed034ce7dd9f0aaf9df9890c89e4ed33f84b1f`.
- Original `crates/cli/src/stream.rs` Git blob:
  `62508df4ad81ffae0c9158a0dfead557aac6bec8`.
- Original `stream.rs` SHA-256:
  `8996721eb08b7ccfb67bbc8f719655a63fbe4d1949face90e3f2bc58264b0121`.
- `baseline.json` SHA-256:
  `53da2cd7e7535f73f2f0c8f99468b989434e6b8f0dff5d747d599fa749774c08`.

The capture ran before production edits. The original renderer and input scripts,
raw unnormalized outputs, source manifests, command and exit status were retained
in the implementation evidence bundle outside the worktree. The capture passed
five transcript/stream-behavior tests and the eleven existing streaming tests.
A subsequent baseline run, still before production edits, passed seven transcript
and stream-behavior tests plus those eleven existing tests. The additional check
of normalization strictness is test-only; it did not regenerate the fixture.

## Inventory and scope

`baseline.json` contains **124** records:

- **112 turn transcripts/outcomes:** 28 scripted source sequences across plain,
  night, dawn and terminal themes, at 80 columns with OSC-8 disabled.
- **12 fixed-duration cost snapshots:** zero, paid and long-duration cases in
  those four themes.
- **108 records** compare every output byte without normalization.
- **16 usage-bearing turn transcripts** normalize only the decimal elapsed-seconds
  token on the final cost line to `<elapsed>`. No other bytes are normalized:
  ANSI, rule width, cost, tokens, whitespace and newlines are preserved. A missing
  or extra final newline fails. Durations long enough to change rule width also
  fail, rather than hiding the difference.

The runtime uses `std::time::Instant`; these usage transcripts do **not** prove
wall-clock timing parity. Fixed-duration cost snapshots cover the formatting
separately. The event-gated typing test exercises the real interval, two actual
flushed frames, and clearing on an empty content delta, without a speed claim or
an added test clock feature.

Sequences cover empty chunks/newlines, EOF, interleaved tools and arguments,
unknown results, duplicate starts, incomplete/non-JSON arguments, stage events,
pre/post-receipt errors, capability/policy denies, approval pending/rejected,
finish variants, committed round boundaries, usage with/without receipts, zero
receipt cost and saturating counters. Runtime-scanned tool output is deliberately
not rendered: the legacy REPL prints only assembled arguments at result time.

Outcomes were frozen along with output; the synchronous reducer tests compare
all 28 outcomes without a writer, theme or UI. Separate semantic tests preserve
typed errors and the unverified three-valued verdict seam. These are scripted
consumer tests, not a live-provider or signed-receipt-verification claim.

## Running and deliberate regeneration

Normal tests only read the fixture:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test -p ardur-cli --test m0_transcripts
```

Regeneration is allowed **only** in an isolated checkout of the exact base above,
with these test files copied in but production files unchanged:

```sh
ARDUR_REGENERATE_M0_GOLDENS=1 CARGO_TARGET_DIR="$PWD/target" \
  cargo test -p ardur-cli --test m0_transcripts frozen_repl_transcripts -- --exact
```

The test checks the base HEAD and the full tracked tree before writing, so
changes in transitive source types cannot silently replace the frozen outcomes.
Copied capture tests must remain untracked in the baseline checkout. Regression
coverage uses real temporary Git repositories and rejects both staged and
unstaged changes in direct and transitive source files, while allowing clean
tracked trees with copied untracked tests.

Both regeneration checks (HEAD and diff) and every fixture Git operation share
one local command constructor that removes inherited `GIT_*` variables. This
is a deliberate superset of `git rev-parse --local-env-vars`: it also removes
config injection and future Git overrides, without running an unsanitized Git
command to discover the list. Non-Git process environment, including `PATH`,
`HOME` and temporary-directory settings, is preserved; the parent process
is never mutated.

Separate child-process regressions contaminate HEAD/diff checks with a clean
foreign repository and contaminate fixture init/add/commit with
`GIT_OBJECT_DIRECTORY` or `GIT_COMMON_DIR`. Every source and foreign repository
is a fresh owned temporary directory. The write probes compare the foreign
repository's file bytes and directory inventory before and after each child,
then verify that the local commit is independently readable. Each child must
enter the exact requested test and report one passing test; zero-test success
is rejected. Object-directory and common-directory cases run independently, so
a failed read check cannot hide a write-isolation failure. Fixture writes use a
local fake identity, DCO sign-off, disabled signing, and an empty hooks path.

Any `CI` environment variable forbids regeneration, even when the explicit flag
is present. Never replace this fixture with post-refactor output to fix a drift.
