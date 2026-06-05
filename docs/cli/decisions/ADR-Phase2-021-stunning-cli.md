# ADR-Phase2-021 — Visually stunning Ardur CLI (hybrid rich-REPL + ratatui TUI)

- **Status:** Proposed
- **Date:** 2026-06-04
- **Section:** §2.X (CLI / front-end)
- **Supersedes / relates to:** continues the `ADR-Phase2-NNN` sequence (next free
  number after `ADR-Phase2-020`). Companion design docs:
  [`../cli-stunning-design.md`](../cli-stunning-design.md),
  [`../cli-competitive-survey.md`](../cli-competitive-survey.md).

> **Filing note.** The §2.X lane spec named `architect/decisions/` +
> `architect/design/` as output paths. In this repository `architect/` is
> gitignored (`.gitignore` line 36) and is a separate local vault with no remote —
> files there cannot be included in a PR to `dev`, and downstream coding lanes check
> out `dev`. Per the spec's explicit "PR → dev" requirement and its
> "`docs/cli/**` if needed" allowance, the survey, design, and this ADR are filed
> under `docs/cli/` so they are tracked, reviewable, and self-contained for the
> coders who implement them. The ADR number (`021`) continues the vault's
> `ADR-Phase2` sequence so it can be cross-filed into the vault's decision index
> verbatim if desired.

---

## Context

The user wants the `ardur` CLI to be **visually stunning** — explicitly "better than
Hermes Agent + OpenClaw + Claude Code combined," such that a non-technical person is
*amazed* by it. The current CLI surface is bare.

The competitive survey ([`../cli-competitive-survey.md`](../cli-competitive-survey.md))
benchmarked the three named tools plus OpenAI Codex CLI (the most polished Rust
full-TUI coding agent) and found a clear gap: **no existing tool combines the
document-quality rendering of the coding agents (Claude Code, Codex) with the
glanceable status/observability surface of the gateway agents (Hermes, OpenClaw) in
a single keyboard-first product.** Two further capabilities are unique to Ardur's
substrate and shown by no competitor: a **live signed-receipt-chain view** and a
**live memory-snippet view**.

Constraints shaping the decision:

- Ardur is a Rust workspace (edition 2024, MSRV 1.85, `#![forbid(unsafe_code)]`),
  so front-end crates must be mature Rust libraries.
- The terminal is the only target for v1 (no web frontend).
- Must degrade cleanly across truecolor → 256-color → 16-color → `NO_COLOR`, and be
  scriptable (`--json`/`--plain`).
- Accessibility is a first-class requirement (color-blind-safe themes; never convey
  state by color alone).

## Decision

Ship a **hybrid, two-front-end CLI from one binary, over one shared rendering core**:

1. **`ardur` (default) — a rich line-based REPL.** Streams tokens; renders markdown
   inline; syntax-highlights code; draws tool calls in rounded boxes; closes each
   turn with a dim, context-pressure-colored cost line; shows tasteful typing dots
   and a single-line truthful spinner.

2. **`ardur tui` — a full ratatui application.** Brand bar (always-on status) + a
   scrollable markdown chat pane + a right-side 3-pane stack (**tool-calls feed**,
   **memory snippets**, **live receipt-chain tail**) + an input box with a
   slash-command palette popover + a context-sensitive status bar. Keyboard-first,
   no mouse.

Both front-ends call the **same** `render_markdown()` / syntax-highlight / tool-box /
theme code. The TUI is a frame loop and pane layout *around* that core, never a
reimplementation.

### Technology choices

| Concern | Choice | Rationale |
|---|---|---|
| TUI framework | **`ratatui`** | the de-facto Rust TUI lib; immediate-mode, pane layout, widely used. |
| Terminal backend | **`crossterm`** | cross-platform, ratatui's default backend, pure-Rust (no ncurses). |
| Markdown parse | **`comrak`** *or* `pulldown-cmark` | CommonMark; we render to *our own* styled spans, not HTML-to-ANSI, for full glyph control. (`termimad` evaluated and rejected — see Alternatives.) |
| Syntax highlight | **`syntect`** | proven for exactly this (Codex CLI ships 32 syntect themes); Sublime-grammar based; mature. |
| Clipboard | **`arboard`** | cross-platform clipboard for `/copy reply`. |
| Input editing | custom over `ratatui` + `crossterm` (consider `tui-textarea`) | a multi-line input with history + the palette popover; `tui-textarea` is a candidate but the palette/fuzzy logic is bespoke. |
| Themes | TOML under `~/.config/ardur/` | 3 built-ins (`dawn`/`night`/`terminal`) + 2 daltonized + custom files; truecolor→256→16 auto-derived. |

### Scope fences (explicit non-goals for v1)

No mouse support; no clickable links beyond OSC-8; no in-terminal images
(Sixel/Kitty/iTerm); no web frontend; no voice/TTS; no random whimsy (truthful
status only); at most 3 default themes + 2 daltonized variants. Any of these
returning is a **new ADR**, not a quiet addition.

## Consequences

**Positive:**

- One shared renderer ⇒ the REPL and TUI never drift; Phase-1 work is reused
  verbatim by Phases 2–3 (additive, not throwaway).
- Ardur occupies the empty quadrant: rendering quality *and* observability *and* both
  form factors — a combination no surveyed tool offers.
- The receipt-chain and memory panes turn Ardur's cryptographic-provenance and
  memory substrate into a *visible, demoable* differentiator.
- Mature, well-trodden crate choices (ratatui/crossterm/syntect) ⇒ low technical risk
  and a precedent (Codex) proving the syntect route.

**Negative / trade-offs:**

- A full TUI is real engineering surface (frame loop, focus model, reflow, narrow-
  terminal handling) — mitigated by phasing (the TUI is Phases 2–3; Phase 1 ships
  value alone).
- Terminal-only ⇒ no mouse, no images; some users will want them. Accepted to keep
  v1 focused and stunning rather than broad and fragile.
- `syntect` carries a Sublime-grammar/onig dependency footprint; acceptable given the
  rendering payoff and prior art.
- Cross-terminal color/glyph degradation is ongoing test surface (truecolor/256/16/
  `NO_COLOR`, OSC-8 detection) — addressed by the Phase-1 exit criteria.

## Alternatives considered

- **REPL only (no TUI).** Matches Claude Code. Rejected: forgoes the panes that make
  the receipt/memory differentiators visible, and the user explicitly asked for a
  TUI-class experience.
- **TUI only (no rich REPL).** Matches Codex. Rejected: the default, low-friction,
  scriptable surface most users touch first is the REPL; it must be excellent on its
  own and work where a full TUI can't (CI, pipes, low-power terminals).
- **`termimad` for markdown.** A capable terminal-markdown crate. Rejected as the
  *primary* renderer because we want per-glyph control of headings/boxes/tables to
  match this design exactly and to share styling with the tool boxes; a CommonMark
  parser + our own span renderer gives that control. (`termimad` may still inform the
  implementation.)
- **`tree-sitter-highlight` instead of `syntect`.** Faster/incremental, but more
  per-language wiring and less batteries-included theming; `syntect`'s Sublime
  grammars + theme model fit a batch-render-per-block use case better and have direct
  prior art (Codex). Revisit only if highlight latency becomes a measured problem.
- **A 32-theme picker (à la Codex).** Rejected for v1 in favor of a small, perfect
  set (3 + 2 daltonized) plus user TOML — restraint is the design thesis.

## References

- [`../cli-competitive-survey.md`](../cli-competitive-survey.md) — Hermes / OpenClaw /
  Claude Code / Codex survey.
- [`../cli-stunning-design.md`](../cli-stunning-design.md) — full design (§A–§J) +
  Phase 1/2/3 roadmap.
- Crates: `ratatui`, `crossterm`, `syntect`, `comrak`, `arboard`, `tui-textarea`.
- Prior art: Codex CLI syntect theming (`openai/codex` PR #11447).
