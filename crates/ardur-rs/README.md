# Paired-home scenarios

Run a single YAML file or a directory of YAML files through the paired home:

```sh
ardur-rs test run scenario.yaml --json report.json --junit report.xml --transcripts transcripts
ardur-rs test run scenarios --transcripts transcripts
```

The home owns execution and the saved bot settings. The client only dispatches,
waits, and grades text. Tests and turns run serially. A suite is sorted by file
name and fully validated before any request is sent. Report files must be new;
existing reports and scenario files are never replaced.

```yaml
id: remember-word
description: Keep context across two turns.
target:
  kind: bot
  bot: fixture-bot
prompt: Remember the word teal.
follow_ups:
  - Reply with that word alone.
max_turns: 2
timeout_secs: 60
expected:
  exact: teal
```

`exact` compares the whole reply, including case and whitespace. `contains` and
`not_contains` compare case-sensitive substrings. `regex` uses Rust regular
expressions. All populated matchers must hold; only the final turn is graded.
An empty matcher block checks that an authoritative answer was obtained.
`max_turns` bounds the number of scripted turns. It does not change the home's
model settings. `timeout_secs` is the whole scenario deadline.

For room work, use an explicit destination:

```yaml
id: compare-options
target:
  kind: room
  room_id: fixture-room
prompt: Compare the two options.
expected:
  contains: [comparison]
```

The client resolves the room thread, or uses an explicit `thread_id` in the
target. It waits for every admitted run in the returned order and joins replies
with a newline. A greeting receipt without an admitted run is unavailable for
execution grading. Bot follow-ups stay on the same resolved bot and must retain
the thread. Every answer must match its run, task where available, and thread.

Results distinguish these states:

- **Pass:** the answer was obtained and every assertion is supported and holds.
- **Fail:** a supported assertion does not hold.
- **Unavailable:** required answer or evidence could not be obtained.

Tool, cost, and token assertions are unavailable until authoritative home
records support them. Reply text, tool-result blocks, estimates, and empty
defaults do not prove tool use or cost. If text fails while other evidence is
missing, the result stays failed and includes the missing-evidence reasons.
JSON reports count unavailable cases separately. JUnit maps them to skipped
cases, with a reason. They still produce a nonzero command exit.

Exit codes are `0` for all pass, `1` for an assertion failure, `2` for unavailable
evidence or input/output errors, and `130` for interruption. A deadline or
interrupt stops waiting without cancelling home work.

Each scenario gets a new JSON-lines transcript, even without a report option.
Entries are redacted before serialization and synced before the next remote
step. They record request nonces, turn numbers, admissions, task/run ids, replies,
and verdicts. Reports are refreshed after each finished case. Unix output files
are created with private permissions. Pairing storage retains its existing
platform restrictions.

After interruption, read the last admission and resume its exact run:

```sh
ardur-rs wait --run saved-run-id
```

The transcript reader keeps all complete lines and ignores an incomplete final
line left by a crash. A request with no received admission retains its nonce;
the original scenario supplies the exact text for an identical replay through
the device sender. Redacted text must never be replayed as the original input.
Rerunning a scenario normally starts new work; it is not an automatic resume.

The historical evaluation binary still supports its server transport. The
paired client uses the new home runner and has no local execution dependency.
