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

## Assumed server contract

> **Status:** as of this writing `ardur-server` does **not** expose a public
> chat endpoint — its HTTP surface is the Slack webhook (`POST /slack/events`),
> `GET /healthz`, and the optional MCP routes. Rather than block on that, the
> runner targets a small, documented contract the server can grow into. When
> the endpoint lands, the harness works unchanged. The path is overridable with
> `--chat-path` if the server names it differently.

```text
POST <server-url>/chat
Content-Type: application/json

{
  "message": "<the scenario prompt>",
  "session_id": "<optional; reused across a multi-turn scenario>"
}

200 OK
{
  "reply": "<assistant text>",        // required — what the matchers grade
  "tokens": 42,                        // optional — graded by max_tokens
  "cost_usd": 0.0007,                  // optional — graded by cost_under
  "tools_called": ["web_search"]       // optional — graded by tool_called
}
```

A non-2xx response, transport error, timeout, or malformed JSON marks that
scenario **errored** (not failed) — a flaky server doesn't abort the whole run.

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
pass/fail/error scoring (via a `wiremock` stand-in for `/chat`), the three
output renderers, and the CLI subcommands end-to-end (`assert_cmd`).

See [`scenarios/README.md`](scenarios/README.md) for the bundled scenarios.
