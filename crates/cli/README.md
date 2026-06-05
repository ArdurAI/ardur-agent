# ardur-cli — the `ardur` binary

A capability-secure, cost-metered chat REPL over the Ardur substrate. `ardur chat`
streams a model's reply, renders it inline as styled Markdown, draws tool calls in
rounded boxes, and closes each turn with a dim cost line.

```
ardur chat                 # full substrate (real LLM, fused pipeline, ~/.ardur state)
ardur chat --echo          # legacy in-memory echo runtime — no provider, no cost
ardur version              # print the version
```

With no `ANTHROPIC_API_KEY` set, `ardur chat` runs offline against a network-free
stub and prints an offline notice; the turn still flows through the full fused
runtime (cap-token → Cedar → cost gate → signed receipt → journal).

## The stunning rendering core (§2.X, Phase 1)

Replies render through a shared core (`ADR-Phase2-021`, `docs/cli/cli-stunning-design.md`):

```
── Findings ────────────────────────────────────────────────────────────────

• Line 24 leaks tok into the error
• Fix: a redacted fingerprint

┌ rust ────────────────────────────────────────────────────────────────────┐
│ Err(AuthError::Invalid { token_fp: fingerprint(&tok) })                   │
└──────────────────────────────────────────────────────────────────────────┘

┌───────────┬───────────┬─────────┐
│ Provider  │ Streaming │ Tools   │
├───────────┼───────────┼─────────┤
│ anthropic │ yes       │ yes     │
│ ollama    │ yes       │ partial │
└───────────┴───────────┴─────────┘

▏ redact secrets before they reach logs

┌─ tool · file.read ───────────────────────────────────────────────────────┐
│ { "path": "crates/auth/src/guard.rs" }                                    │
└──────────────────────────────────────────────────────────────────────────┘
───────────────── 432 tokens in · 187 out · $0.0041 · 1.8s ─────────────────
```

- **Markdown** — headings (h1 bold·underline·accent; h2/h3 bold·dim-accent), lists
  (`•`/`◦`/`▪` by depth), **bold**/*italic*/`code`, block quotes (`▏`), tables with
  box borders, and links as `text (url)` (or OSC-8 hyperlinks where supported).
- **Syntax highlighting** — fenced code blocks via `syntect` (rust, python, ts, sh,
  json, yaml, toml, and every other bundled grammar) inside a dim frame.
- **Tool boxes** — each tool call drawn in a rounded box, width-adapted to the
  terminal (capped at 80 columns).
- **Cost line** — dim, with a context-pressure colour ramp (dim < 50% → warn → error).
- **Typing dots** — `·`/`··`/`···` pulsing at 4 Hz while awaiting the first token.

### Themes

Three ship by default (design §B.2 / §E):

| Theme | Mood | Notes |
|---|---|---|
| `night` | dark, cool (amber/sky) | **default** |
| `dawn` | light, warm (ochre/teal) | |
| `terminal` | zero override | maps roles onto your terminal's own 16 ANSI colours |

Select with `ARDUR_THEME=dawn|night|terminal`, or switch live with `/theme <name>`.
`dawn`/`night` emit ANSI-256; `terminal` emits ANSI-16. Setting `NO_COLOR` (any
value), piping stdout, or passing `--plain` drops all colour to clean, escape-free
output — the box/bullet/word structure still carries the meaning.

## Slash commands (Phase 1)

| Command | Action |
|---|---|
| `/help` | command reference |
| `/clear` | clear the screen |
| `/theme <name>` | switch theme live (`dawn` · `night` · `terminal`) |
| `/cost` | this session's running token + dollar cost |
| `/budget` | remaining session budget |
| `/quit`, `/exit` | leave |

`/sessions`, `/memory`, `/receipts`, `/skill`, `/copy`, and `/save` are deferred to
Phases 2–3 (the `ardur tui`).

## Flags

| Flag | Effect |
|---|---|
| `--echo` | legacy in-memory echo runtime (no provider, no cost, no state) |
| `--no-stream` | render each turn from one `complete()` call through the full pipeline |
| `--plain` | escape-free output + full Markdown layout (implies `--no-stream`) — for pipes/CI |
| `--budget-cents <C>` | per-session spend ceiling in US cents (default 1000 = $10) |
| `--config <PATH>` | config file (default `~/.ardur/config.toml`) |

## Environment variables

| Variable | Purpose |
|---|---|
| `ARDUR_THEME` | `dawn` \| `night` \| `terminal` (default `night`) |
| `NO_COLOR` | present (any value) ⇒ no colour/styling |
| `ARDUR_FORCE_OSC8` | `1`/`true` ⇒ force OSC-8 hyperlinks (else auto-detected from `TERM*`) |
| `ARDUR_CLI_BUDGET_CENTS` | per-session budget ceiling (overridden by `--budget-cents`) |
| `ARDUR_PROVIDER` | model backend selector — see the top-level `RUN.md` |
| `ANTHROPIC_API_KEY` | enables real Anthropic calls; absent ⇒ offline stub |

State (keys, receipt chain, journals) persists under `~/.ardur/`; the first-launch
splash marker lives at `~/.config/ardur/state.toml`.

## First launch

The brand splash shows once, on the first interactive launch, then a marker
(`~/.config/ardur/state.toml`) flips so it never shows again.
