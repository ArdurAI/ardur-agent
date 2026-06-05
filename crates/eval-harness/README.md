# ardur-eval — a Tau-Bench-style evaluation harness

`ardur-eval` is a **standalone CLI** that grades a running `ardur-server`
against a directory of declarative, YAML-authored scenarios. It is *not* part
of the server: it POSTs prompts to a server URL over HTTP and checks the replies
against matchers, then reports the results as JSON, JUnit XML, or Markdown.

```text
┌────────────┐   POST /chat    ┌──────────────┐
│ ardur-eval │ ───────────────▶│ ardur-server │
│  (this)    │ ◀───────────────│ (under test) │
└────────────┘   { reply, … }  └──────────────┘
      │
      ├─ load scenarios/*.yaml
      ├─ grade each reply against its matchers
      └─ render json | junit | markdown
```

## Server contract

The runner targets `ardur-server`'s synchronous chat surface, `POST /chat`
(§4.0b). The path is overridable with `--chat-path` if the server names it
differently.

```text
POST <server-url>/chat
Content-Type: application/json

{
  "message": "<the scenario prompt>",
  "session_id": "<optional uuid; minted by the server, reused across a multi-turn scenario>"
}

200 OK
{
  "session_id": "<uuid the turn ran under>",
  "reply": "<assistant text>",            // what the matchers grade
  "tokens": { "input": 120, "output": 30 }, // summed → graded by max_tokens
  "cost_usd": 0.0007,                      // graded by cost_under
  "tools_called": ["web_search"],          // graded by tool_called
  "receipt_id": "<uuid>"
}
```

**Status mapping.** The runner distinguishes the two error families the server
reports:

- **`400`** — the server rejected the request body (empty `message`,
  unsupported `stream: true`, …). This is a scenario **failure**
  (`bad_request: …`), not an error — the request was malformed, not the server.
- **`502`** — the runtime rejected or failed the turn (cost gate denied,
  injection blocked, provider error). Surfaces as an **error** (`runtime: …`).
- Any other non-2xx, transport error, timeout, or malformed JSON also marks the
  scenario **errored** — a flaky server doesn't abort the whole run.

Multi-turn scenarios omit `session_id` on the first turn (the server mints one),
then thread that id through every `follow_ups` turn so the agent retains
context. The matchers grade the **final** reply.

## Installing / building

```sh
cargo build -p ardur-eval        # produces target/debug/ardur-eval
cargo run   -p ardur-eval -- --help
```

## Usage

```sh
# Run every scenario in a directory against a live server.
ardur-eval run --scenarios crates/eval-harness/scenarios \
               --server-url http://localhost:8080 \
               --output markdown        # json | junit | markdown (default markdown)

# Point at a differently-named endpoint.
ardur-eval run --scenarios ./scenarios --server-url http://localhost:8080 \
               --chat-path /v1/chat

# List the scenarios discovered in a directory.
ardur-eval list --scenarios crates/eval-harness/scenarios

# Scaffold a new scenario file (writes <dir>/<id>.yaml).
ardur-eval new --id my_new_case --scenarios crates/eval-harness/scenarios
```

`run` exits **non-zero** when any scenario failed or errored, so it drops
straight into a CI gate. Use `--output junit` to feed a CI test reporter.

### Live vs. mock

The crate's own tests are **mock-backed**: they drive the runner against a
`wiremock` stand-in for `/chat`, so `cargo test -p ardur-eval` needs no server
and no model credentials (this is what CI runs).

The bundled scenarios under [`scenarios/`](scenarios/) are written so they
*can* run against a live server, but most assert model behaviour and so need a
real backend to pass. The [`scenarios/live/`](scenarios/live/) subdirectory
holds three deliberately-minimal live smokes. `load_dir` is non-recursive, so a
plain `--scenarios crates/eval-harness/scenarios` run does **not** pick them up
— point `--scenarios` at the `live/` directory explicitly:

```sh
# Mock/offline: exercises the harness only (CI-safe; no server needed).
cargo test -p ardur-eval

# Live: run the three smokes against a running ardur-server.
ardur-eval run --scenarios crates/eval-harness/scenarios/live \
               --server-url http://localhost:8080
```

There is also a **gated integration test** (`tests/live_chat.rs`) that runs the
`live/` scenarios end-to-end. It is skipped unless **both** env vars are set, so
it stays inert in CI:

```sh
ARDUR_LIVE_CHAT_TEST=1 ARDUR_LIVE_CHAT_URL=http://localhost:8080 \
  cargo test -p ardur-eval --test live_chat -- --nocapture
```

## Writing a scenario

A scenario is one self-contained YAML file. The only required fields are `id`
and `prompt`; everything else has a sensible default.

```yaml
id: factual_recall
description: Agent should answer a basic factual question correctly.
prompt: "What is the capital of France? Answer in one word."
expected:
  contains: ["Paris"]                  # every substring must appear
  not_contains: ["London", "Berlin"]   # none of these may appear
  regex: "(?i)\\bparis\\b"             # reply must match this regex
  tool_called: web_search              # this tool must have been invoked
  cost_under: 0.01                     # turn cost (USD) must be below this
max_tokens: 100                        # 0/absent ⇒ unbounded; else graded if reported
max_turns: 1                           # informational turn budget
timeout_secs: 30                       # per-scenario HTTP wall-clock budget
follow_ups:                            # optional extra turns on the same session
  - "And what country is it in?"
```

### Matchers

| Matcher | Type | Passes when |
|---|---|---|
| `contains` | list of strings | **every** substring appears in the reply |
| `not_contains` | list of strings | **no** substring appears in the reply |
| `regex` | string | the reply matches the (Rust `regex`) pattern |
| `tool_called` | string | the server-reported `tools_called` includes it |
| `cost_under` | float | the reported `cost_usd` is strictly below it |

An empty `expected` block trivially passes — useful for a smoke case that only
asserts the server answered at all. For a multi-turn scenario, the matchers
grade the **final** reply; `follow_ups` are sent after `prompt` on a shared
`session_id`.

## Output formats

- **`json`** — full detail with a `summary` block; machine-readable.
- **`junit`** — JUnit XML (`<testsuite>`/`<testcase>`); CI-friendly.
- **`markdown`** — a human summary table with pass/fail/error counts.

## Testing this crate

```sh
cargo test -p ardur-eval
```

The tests cover scenario parsing/round-trip, every matcher, the runner's
pass/fail/error scoring against the real `/chat` response shape (via a
`wiremock` stand-in — including the nested `tokens` object, `502 → Error` and
`400 → Fail` mapping, and multi-turn session-id reuse), the three output
renderers, and the CLI subcommands end-to-end (`assert_cmd`). The gated
`live_chat` test (above) is the only one that touches a real server.

See [`scenarios/README.md`](scenarios/README.md) for the bundled scenarios.
