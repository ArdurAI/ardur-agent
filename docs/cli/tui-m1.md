# Full-screen chat (M1, opt-in)

The rich REPL remains the default. To enter the full-screen surface:

```sh
ARDUR_TUI=1 ardur chat
```

This is an environment opt-in, **not an `ardur tui` subcommand**. `ARDUR_TUI=0`
or leaving it unset keeps the existing REPL. Any other value is rejected.
Both stdin and stdout must be terminals; `--echo`, `--plain` and `--no-stream`
conflict with this mode. These checks happen before persistent state is created.
Use at least 72 columns and 16 rows; a smaller terminal shows a resize notice.

The same selected provider, capability checks, Cedar policy, cost gate,
receipts and durable session journal run through `FusedEngine`. There is no
provider-only shortcut. Missing policy remains deny-by-default, including with
the offline stub. Configure policy using the ordinary CLI setup/runbook; the
TUI does not grant authority or install permissive policy. The header identifies
the session and marks an offline stub. `--session-id <UUID>` resumes an existing
session's durable messages.

## Keys and local commands

| Input | Effect |
| --- | --- |
| Enter | Send the draft (one turn at a time) |
| Alt-Enter / Shift-Enter | Insert a newline, where the terminal reports that combination |
| Left / Right, Home / End | Move the editing cursor by grapheme or to the draft ends |
| Backspace / Delete | Remove the adjacent grapheme |
| Up / Down | Recall earlier prompts / restore the saved draft |
| Bracketed paste | Insert text, neutralizing terminal controls |
| Tab | Switch focus between input and transcript |
| PageUp / PageDown | Scroll transcript |
| Up / Down in transcript | Scroll a row |
| Home / End in transcript | Oldest visible text / follow latest output |
| Ctrl-K | Command palette; Up/Down select, Enter run, Esc dismiss |
| Ctrl-T | Cycle night / dawn / terminal theme |
| Ctrl-C | Cancel the running turn, or clear the idle draft |
| Ctrl-D | Exit, cancelling and settling a running turn first |

The local commands are `/help`, `/cost`, `/theme [night|dawn|terminal]`, and
`/quit` (`/exit` also exits). They do not dispatch provider requests. Other REPL
slash commands are not implemented in this opt-in surface. Editing remains
available during a turn, but Enter cannot submit an overlapping turn.

Approval-required output is **prompt-only**. Use `ardur approvals` outside the
TUI to inspect/decide through the existing machine-operator CLI. No TUI key or
command approves/rejects a request. Public failure labels come from typed
errors, not provider/tool diagnostic strings.

## Honest status

- `turn receipt` reports only observed authoritative receipt costs; provider
  usage costs are not substituted. Until a receipt update arrives it is
  `unknown`, including if cancellation interrupted that notification.
- `ledger` is the last observed engine ledger balance, identified by the session
  in the header. It is refreshed after owning-stream drop and settlement drain;
  while a turn runs it is explicitly marked pending, not a live spend estimate.
- Context uses the **latest request's input tokens**, never an aggregate across
  rounds. Capacity is unknown unless a positive integer is explicitly supplied
  as `ARDUR_TUI_CONTEXT_TOKENS`. This is an operator-provided denominator, not
  model-capacity discovery. Percentages use the existing 50%/85% thresholds.
- Governance stays `? unverified` without explicit verification evidence.
  A successful stage, finish, receipt or denial does not manufacture a verdict.
- The activity line uses actual pipeline stages, elapsed time and observed
  output tokens. Missing token data is labelled unknown.

`NO_COLOR` disables colors/styles and animation, without disabling the fused
stream. `ARDUR_TUI_ANIMATE=0` disables the animated glyph separately (`1` is the
default). Theme switching cannot override `NO_COLOR`. Generated Markdown/code/
tool formatting is shared with the REPL through a cell-width adapter; incoming
controls are neutralized and terminal hyperlinks are not emitted.

## Lifecycle and limits

Keys, resize events, ticks and fused updates are multiplexed in one async
consumer. The source is pull-based: no detached turn task, prefetch queue or
second accounting reducer. Cancellation/error drops the **owning** source
before settlement drain. Durable history is then replayed from the journal,
so a commit that preceded its receipt notification is not lost, and speculative
output is never added to the next request. A cancelled turn can remain visible
in the transcript; only its committed messages enter durable context.

Terminal raw mode, alternate screen, cursor and paste mode have restoration on
normal return, partial setup failure, I/O failure and unwinding. As with other
terminal programs, forced process termination (for example SIGKILL) cannot run
cleanup. Runtime diagnostic logging is not installed on stderr by the TUI;
typed public notices are shown in the frame instead.

Input is capped at 16 KiB; prompt recall at 128 entries. Visible transcript is
capped at 128 blocks / 256 KiB, with 32 KiB per block, 512 rendered lines per
block plus a truncation notice, and 8192 visible lines total. These are display
bounds, not a bound on durable journal/history storage or a performance claim.

## Focused checks

```sh
cargo test -p ardur-cli --lib tui
cargo test -p ardur-cli --test tui_entry --test tui_pty
cargo test -p ardur-cli --test m0_transcripts --test m0_updates --test streaming
```

The Unix PTY test needs Python 3; it checks actual binary activation and terminal
modes, not visual acceptance. Styled `TestBackend` goldens cover streaming,
denial and approval prompts at 80×24 and 120×24: six frames in one inventory-
checked test. Each snapshot retains every cell symbol and style runs. To update
only those TUI snapshots deliberately (never in CI):

```sh
ARDUR_UPDATE_TUI_GOLDENS=1 cargo test -p ardur-cli --lib styled_goldens
```

The frozen M0 transcript fixture is independent and must not be regenerated.
