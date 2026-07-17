# Ardur CLI — Stunning Design Specification

> **Status:** design-only proposal for the §2.X stunning-CLI lane. No code.
> **Companion docs:** [`cli-competitive-survey.md`](./cli-competitive-survey.md) ·
> [`decisions/ADR-Phase2-021-stunning-cli.md`](./decisions/ADR-Phase2-021-stunning-cli.md)
>
> **Mandate.** The `ardur` CLI must be *visually stunning* — better than Hermes
> Agent, OpenClaw, and Claude Code combined. A non-technical person should be
> **amazed** by it. This document is concrete enough that a downstream coding lane
> can implement it without further questions: every screen is mocked in ASCII,
> every color is specified with hex + ANSI-256 + 16-color fallback, every keybinding
> and slash command is enumerated.
>
> **Design thesis (one line).** *Stunning = restraint executed perfectly.* No
> gradient noise, no flashing, no marketing animation. The "wow" comes from
> impeccable typography, a coherent palette, buttery-smooth rendering, and showing
> the user things no other agent CLI shows them — the live receipt chain and memory.

---

## Contents

- [§A. Form factor (hybrid)](#a-form-factor-hybrid)
- [§B. Visual identity](#b-visual-identity)
- [§C. Markdown rendering](#c-markdown-rendering)
- [§D. Animations](#d-animations)
- [§E. Themes](#e-themes)
- [§F. Slash commands](#f-slash-commands)
- [§G. Keyboard shortcuts (TUI)](#g-keyboard-shortcuts-tui)
- [§H. Branding moments](#h-branding-moments)
- [§I. What we explicitly do NOT do](#i-what-we-explicitly-do-not-do)
- [§J. Comparison table](#j-comparison-table)
- [Implementation roadmap (Phase 1/2/3)](#implementation-roadmap)

---

## §A. Form factor (hybrid)

Ardur ships **two** front-ends from one binary, sharing one rendering core:

| | `ardur` (default) | `ardur tui` |
|---|---|---|
| Shape | rich **line-based REPL** | full **ratatui** app |
| Scrollback | terminal-native (real scrollback) | app-managed panes |
| When | quick chat, scripts, CI, low-power terms | deep sessions, observability |
| Mouse | none | none (keyboard-first) |
| Renderer | shared markdown+syntax core | shared markdown+syntax core |

The two are **not** forks: both call the same `render_markdown()`, the same
syntax-highlight pass, the same theme, the same tool-box drawing. `ardur tui` adds
panes and a frame loop around that core; it never reimplements rendering.

### A.1 `ardur` — the rich REPL (default)

Streams tokens as they arrive. Markdown rendered **inline** as each block
completes. Code blocks syntax-highlighted. Tool calls drawn in **rounded boxes**.
A dim cost line closes each turn. Subtle typing dots while waiting for the first
token.

**Full mocked turn** (theme `dawn`, 96-col terminal):

```
ardur · session-07 · claude-opus-4-7                            432 tok · $0.04
────────────────────────────────────────────────────────────────────────────────
›  read the auth guard and tell me if it leaks the token on the error path

   · · ·                                          ← typing dots, 4 Hz, until token 1

   I read the guard. Here's what it does and the one issue I found.

   ## Findings

   The happy path is fine. The **error path** returns the raw token in the body:

   ┌─ tool · file.read ───────────────────────────── crates/auth/src/guard.rs ─┐
   │  18  pub fn guard(req: &Request) -> Result<Claims, AuthError> {            │
   │  19      let tok = bearer(req)?;                                           │
   │  24      Err(AuthError::Invalid { token: tok })   // ← leaks raw token     │
   │  …  (42 lines · ⌃O to expand)                                             │
   └─────────────────────────────────────────────────────── 12 ms · ok ───────┘

   1. Line 24 puts `tok` into the error — it reaches logs verbatim.
   2. Fix: replace with a redacted fingerprint.

   ```rust
   Err(AuthError::Invalid { token_fp: fingerprint(&tok) })
   ```

   ─ 432 tokens · $0.0041 · 1.8 s · opus-4-7 ──────────────────────────────────
›
```

Anatomy:

- **Top brand/status strip** (one line + rule): `ardur · session · model` left,
  running `tokens · $cost` right. In the REPL this is reprinted at the *top of each
  turn* (not a fixed overlay — that needs the TUI). Dim by default.
- **User prompt:** `›` glyph in the **primary** color, the typed text in the normal
  foreground.
- **Typing dots:** `· · ·` pulsing at 4 Hz in **dim** until the first token lands,
  then erased.
- **Tool box:** rounded corners (`┌ ┐ └ ┘`), title bar `tool · <name>` left and an
  optional context label (file path) right; footer right-aligns `<duration> · <status>`.
  Status `ok` in **success**, `err` in **error**. Body is the tool's rendered output;
  long bodies truncate with a `⌃O to expand` hint (REPL expands by reprinting).
- **Cost line:** a dim rule + `tokens · $cost · wall-time · model`, closing the turn.

**Low-capability fallback:** if `NO_COLOR` is set or the terminal is not a TTY, the
same turn degrades cleanly — boxes become ASCII (`+--+`/`|`), colors drop to the
16-color set or none, dots become a static `...`, and `--plain` strips boxes
entirely to a flat indented transcript (for piping/CI).

### A.2 `ardur tui` — the full TUI

A four-region ratatui layout. ASCII wireframe at 120×34:

```
┌ ardur · session-07 · claude-opus-4-7 · 432 tok · $0.04 · ◷ 1.8s · ● oncall ─────────────────────┐ ← brand bar
├──────────────────────────────────────────────────────────┬───────────────────────────────────────┤
│ CHAT                                                   ▲  │ TOOL CALLS                          ▲ │
│                                                        │  │  file.read  guard.rs      12ms  ok    │ ← top-right pane
│ › read the auth guard and check the error path         │  │  shell.run  cargo test    1.2s  ok    │
│                                                        ░  │  file.write guard.rs       4ms  ok    │
│ I read the guard. The error path returns the raw       ░  ├───────────────────────────────────────┤
│ token. Here's the offending line:                      ░  │ MEMORY                              ▲ │
│                                                        ░  │  ▸ auth tokens must be fingerprinted   │ ← middle-right pane
│ ┌─ tool · file.read ── guard.rs ─┐                      │  │  ▸ guard.rs reviewed 2026-05-29        │
│ │ 24  Err(Invalid{ token: tok }) │                      │  │  ▸ §11.14 receipt chain is canonical   │
│ └──────────────── 12 ms · ok ────┘                      ▼  ├───────────────────────────────────────┤
│                                                           │ RECEIPTS                            ▲ │
│ The fix is a redacted fingerprint:                        │  #41 ✓ tool.file.read   sig ok        │ ← bottom-right pane
│ ```rust                                                   │  #42 ✓ tool.shell.run   sig ok        │
│ Err(Invalid{ token_fp: fingerprint(&tok) })               │  #43 ✓ chat.reply       sig ok ◀ head │
│ ```                                                    ▼  │  chain head a3f9… verified            │
├──────────────────────────────────────────────────────────┴───────────────────────────────────────┤
│ › fix it and run the tests▏                                                                        │ ← input box
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ ⌃K palette · ⇥ focus · ⇧⇥ mode:plan · PgUp/PgDn scroll · ⌃T theme · ⌃C cancel · ⌃D exit   ◷ 38ms ● │ ← status bar
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Regions:

1. **Brand bar (top, 1 row):** `ardur · session · model · tokens · $cost · ◷ latency
   · ● oncall`. The `●` is an oncall/health dot (success/warn/error color). Always
   visible — this is the always-on status surface Claude Code lacks.
2. **Chat pane (left, fills):** scrollable, markdown-rendered, syntax-highlighted —
   identical rendering to the REPL. A slim scrollbar (`▲ ░ ▼`) on the right edge.
   Smooth scroll on overflow.
3. **Right stack (3 panes):**
   - **Tool calls feed** (top): every tool invocation, newest at bottom: `<name>
     <arg-summary> <duration> <status>`. Selecting one (focus + `↑/↓`) expands it in
     the chat pane.
   - **Memory** (middle): recent memory records as `▸ snippet` lines — the
     memory layer surfaced, not hidden behind a command.
   - **Receipts** (bottom): the **signed receipt-chain tail** — `#<seq> <verify-mark>
     <kind> sig <ok|BAD>`, head marked `◀ head`, with `chain head <hash> verified`.
     **No competitor shows this.** It is Ardur's provenance differentiator, made live.
4. **Input box (bottom, grows to 5 rows):** the prompt with a block cursor `▏`.
   `/` opens the slash palette as a popover *above* this box (see §F).
5. **Status bar (bottom, 1 row):** active keybindings + latency `◷` + health `●`.

**Focus model.** `Tab` cycles focus Chat → Tool → Memory → Receipts → Input → Chat.
The focused pane gets a **primary-color** border; others stay **dim**. Only the
focused pane responds to `↑/↓`/`PgUp/PgDn`.

**Narrow-terminal reflow.** Below 100 cols the right stack collapses; the three
panes become a single cycling pane toggled by `Ctrl+]` (Tool ▸ Memory ▸ Receipts),
label shown in its title bar. Below 72 cols, `ardur tui` prints a one-line notice
and suggests the REPL.

---

## §B. Visual identity

### B.1 ASCII logo

Three variants were drafted; **Variant 2 ("the bar mark")** is selected — it reads
at small sizes, survives 80-col wrapping, and the underscore evokes a terminal
caret.

**Variant 1 — block (rejected: too tall, noisy at small sizes):**

```
 █████╗ ██████╗ ██████╗ ██╗   ██╗██████╗
██╔══██╗██╔══██╗██╔══██╗██║   ██║██╔══██╗
███████║██████╔╝██║  ██║██║   ██║██████╔╝
██╔══██║██╔══██╗██║  ██║██║   ██║██╔══██╗
██║  ██║██║  ██║██████╔╝╚██████╔╝██║  ██║
╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝  ╚═════╝ ╚═╝  ╚═╝
```

**Variant 2 — the bar mark (SELECTED):**

```
        _
   __ _ _ __ __| |_   _ _ __
  / _` | '__/ _` | | | | '__|
 | (_| | | | (_| | |_| | |
  \__,_|_|  \__,_|\__,_|_|
        the agent that keeps the receipts
```

**Variant 3 — minimal monogram (kept as the compact/favicon mark for tight spaces):**

```
  ╭───╮
  │ a·│   ardur
  ╰───╯
```

Rules: the logo is rendered in the **primary** color, the tagline in **dim**. It
appears **only** on the splash (first launch) and `ardur --version`/`/about`. It is
*never* reprinted mid-session — restraint.

### B.2 Color palette

Each role specifies **hex** (truecolor terminals), **nearest ANSI-256** (256-color
terminals), and a **16-color fallback** (color-poor terminals). The two shipped
themes (`dawn`, `night`) define both; `terminal` overrides nothing (§E).

**`dawn` (light, warm — default):**

| Role | Use | Hex | ANSI-256 | 16-color |
|---|---|---|---|---|
| `primary` | logo, `›` prompt, focused border, links | `#B5651D` (warm ochre) | 173 | yellow |
| `accent` | headings, selection, slash-palette highlight | `#1F6F8B` (teal) | 30 | cyan |
| `fg` | body text | `#2E2A24` | 235 | default |
| `dim` | cost line, secondary text, unfocused borders | `#8A8275` | 244 | bright-black |
| `success` | `ok` status, verified receipts, health dot | `#2E7D32` | 28 | green |
| `warn` | 50–85% context, retries | `#B8860B` | 136 | yellow |
| `error` | `err` status, BAD sig, ≥85% context | `#C0392B` | 160 | red |
| `bg` | base background | `#FBF7F0` | 230 | default |

**`night` (dark, cool):**

| Role | Hex | ANSI-256 | 16-color |
|---|---|---|---|
| `primary` | `#E0A458` (amber) | 179 | yellow |
| `accent` | `#5FB3C9` (sky) | 74 | cyan |
| `fg` | `#E6E1D8` | 254 | default |
| `dim` | `#7A7264` | 242 | bright-black |
| `success` | `#7CC47F` | 114 | green |
| `warn` | `#E2B544` | 179 | yellow |
| `error` | `#E26D5C` | 173 | red |
| `bg` | `#16140F` | 234 | default |

**Palette laws:**

1. **Never** rely on color alone to convey state — pair every color with a glyph or
   word (`ok`/`err`, `✓`/`✗`, `●`). This is the accessibility floor and makes the
   16-color and `NO_COLOR` fallbacks legible.
2. **Context-pressure ramp** (Hermes' best idea, refined): the cost line's token
   fraction colors `dim` <50% → `warn` 50–85% → `error` ≥85%. The *only* place
   warn/error appear without an explicit failure, so they read as a real signal.
3. **Max 6 roles on screen at once.** A 7th color is noise.

### B.3 Typography (terminal monospace)

The terminal owns the font; we own *weight and style* semantics:

| Style | Means | Where |
|---|---|---|
| **bold** | structural emphasis | h1/h2 headings, `›` prompt glyph, focused border |
| *italic* | quoted/aside/inline-emphasis | blockquotes, `*italic*` md, tool arg summaries |
| underline | live link target (OSC-8 fallback) | links when OSC-8 unsupported |
| dim | secondary/metadata | cost line, timestamps, unfocused panes, taglines |
| reverse | active selection | slash-palette current row, selected tool-feed row |

No blink, ever. Bold is used sparingly — if everything is bold, nothing is.

---

## §C. Markdown rendering

The shared renderer. Parse with a CommonMark parser (`comrak` or `pulldown-cmark`),
then render to styled terminal spans (custom renderer; *not* a pre-baked
HTML-to-ANSI lib, so we control every glyph). Supported, by element:

**Headings (h1–h3 only — terminal real estate):**

```
 ▌ Heading 1                      ← accent, bold, left bar ▌, blank line above
 ── Heading 2 ──────────────────  ← accent, bold, hairline rule to margin
 Heading 3                        ← accent, bold, no rule
```

h4+ degrade to h3 styling (bold accent, no rule) — we never render deeper.

**Lists:**

```
 •  bullet item                   ← • in primary, 2-space hang indent
    ◦  nested bullet              ← ◦ at depth 2, ▪ at depth 3+
 1. numbered item                 ← number in primary
 2. second
```

**Code blocks (syntax-highlighted, min langs rust/python/ts/sh/json):**

````
 ┌ rust ───────────────────────────────────────────────┐
 │ pub fn fingerprint(tok: &str) -> String {            │   ← syntect highlight
 │     let h = blake3::hash(tok.as_bytes());            │      lang label in dim
 │     h.to_hex()[..12].to_string()                     │      on the top border
 │ }                                                    │
 └──────────────────────────────────────────────────────┘
````

Code-block frame is **dim** (recedes); the *code* carries the color via syntax
highlight. Unknown languages: no highlight, plain `fg`, label `text`. The box is
the same rounded style as tool boxes but with a language label instead of `tool ·`.

**Inline:** `` `code` `` → `accent` on a faint background cell; **bold** → bold `fg`;
*italic* → italic `fg`.

**Links:**

```
 the spec (https://ardur.dev/spec)        ← default: text + dim URL in parens
 the spec                                  ← OSC-8 terminals: text underlined, clickable
```

Detect OSC-8 support (terminfo / known-term allowlist); when present, emit OSC-8 and
drop the parenthetical. Otherwise the `text (URL)` form (never hide the URL).

**Tables (lightweight, box-drawn, auto-width to content, capped at term width):**

```
 ┌──────────┬──────────┬────────┐
 │ Provider │ Streaming│ Tools  │
 ├──────────┼──────────┼────────┤
 │ anthropic│ yes      │ yes    │
 │ ollama   │ yes      │ partial│
 └──────────┴──────────┴────────┘
```

Header row in **accent bold**; rule in **dim**. Columns wider than the terminal
ellipsize the longest cell with `…` (never horizontal scroll in the REPL).

**Blockquotes:**

```
 ▏ quoted text, italic, dim       ← left bar ▏ in dim, text italic
```

---

## §D. Animations

Real and purposeful, never gratuitous. All animations honor a `--no-anim` flag and
auto-disable on non-TTY / `NO_COLOR`.

| Animation | Spec | Where |
|---|---|---|
| **Typing dots** | `· · ·`, three dots pulsing in opacity (dim→fg→dim) at **4 Hz**, single line, erased on first token | REPL + TUI, while awaiting token 1 |
| **Tool spinner** | braille cycle `⠁⠂⠄⡀⢀⠠⠐⠈`, **~8 Hz slow rotation**, in `primary`, prefixing the running tool's title | beside an in-flight tool box |
| **Smooth scroll** | when the chat pane overflows, scroll **one cell per frame** (≤60 fps) to the new bottom rather than jumping | TUI chat pane |
| **Receipt tick** | a new receipt row fades in (dim→fg over ~200 ms) and the head marker `◀ head` slides down one row | TUI receipts pane |
| **Theme cross-fade** | on `Ctrl+T`, redraw once — **no** color tween (tweening truecolor is noise); instant swap is cleaner | TUI |

**Spinner verb (REPL, optional, off by default for non-tech "amazement" mode on):**
a single dim verb beside the spinner — `thinking…`, `reading…`, `running tests…` —
chosen from the *actual current activity*, not random whimsy. (We deliberately do
**not** copy Claude Code's random verbs; a *truthful* verb is more impressive to a
non-technical user — it looks like the machine is narrating what it does.)

**Explicitly banned:** flashing colors, rainbow/gradient text, marquee/scrolling
banners, progress bars that lie (fake percentages), bouncing ASCII art, sound. The
restraint *is* the polish.

---

## §E. Themes

Three ship by default; custom themes load from config.

| Theme | Mood | Notes |
|---|---|---|
| `dawn` | light, warm | **default**. Ochre/teal on cream. Full palette in §B.2. |
| `night` | dark, cool | Amber/sky on near-black. Full palette in §B.2. |
| `terminal` | zero override | uses the terminal's own 16-color palette verbatim — `primary`=yellow, `accent`=cyan, etc. For users with a curated terminal theme who want Ardur to blend in. |

**Plus** two **daltonized** variants (`dawn-cb`, `night-cb`) — color-blind-safe
ramps (the one accessibility idea worth copying wholesale from Claude Code). These
swap the success/warn/error hues for a blue↔orange-safe set and *always* keep the
glyph pairing from palette-law #1.

**Selection precedence** (first match wins):

1. `--theme <name>` flag
2. `$ARDUR_THEME` env var
3. `theme = "<name>"` in `~/.config/ardur/config.toml`
4. auto-detect terminal background (OSC 11 query; light→`dawn`, dark→`night`)
5. fallback `dawn`

**Custom theme file** — `~/.config/ardur/themes/<name>.toml`:

```toml
# ~/.config/ardur/themes/solar.toml
name    = "solar"
base    = "night"          # inherit unspecified roles from a built-in
[colors]
primary = "#cb4b16"        # hex; ANSI-256 + 16-color auto-derived by nearest-match
accent  = "#268bd2"
success = "#859900"
warn    = "#b58900"
error   = "#dc322f"
```

Switch live with `/theme <name>` (REPL + TUI) or `Ctrl+T` (TUI cycles
dawn→night→terminal→back). The active theme persists to `config.toml` on change.

---

## §F. Slash commands

Identical command set in REPL and TUI. In the REPL, typing `/` and pressing `Tab`
completes; in the TUI, `/` opens a **palette popover above the input** (and `Ctrl+K`
opens it directly). Palette mock (TUI):

```
        ┌ / commands ─────────────────────────────────────┐
        │ ▌ /model      switch provider / model           │ ← reverse = selected
        │   /theme      change theme                      │
        │   /sessions   list & switch sessions            │
        │   /memory     recent memory records             │
        │   /receipts   recent receipt chain              │
        │   /cost       this session's cost               │
        │   /copy reply copy last reply to clipboard      │
        │   …  type to filter · ↑↓ select · ⏎ run · esc   │
        └─────────────────────────────────────────────────┘
 › /mo▏
```

Fuzzy-filtered as you type (`/mo` → `/model`). Full set:

| Command | Action |
|---|---|
| `/help` | command reference (this list, rendered) |
| `/clear` | clear scrollback (REPL) / chat pane (TUI); keeps session |
| `/exit` | quit (alias `/quit`) |
| `/model <name>` | switch provider/model live (e.g. `claude-opus-4-7`, `ollama:llama3`) |
| `/sessions` | list sessions; `/sessions <id>` switches |
| `/history` | scroll/search the current session transcript |
| `/memory recent` | show recent memory records (also the TUI middle pane) |
| `/receipts` | show recent receipts + verify chain (also the TUI bottom pane) |
| `/tools list` | list registered tools + capabilities |
| `/cost` | current session token + dollar cost |
| `/theme <name>` | switch theme; bare `/theme` opens the picker |
| `/skill <name>` | invoke a `SKILL.md` skill |
| `/copy reply` | copy last reply to clipboard (via `arboard`) |
| `/save <path>` | save transcript to a file (markdown) |
| `/about` | logo + version + build |

**Palette also surfaces dynamic entries:** registered skills appear as
`/skill <name>` rows; available models appear under `/model`. Unknown `/foo` →
a dim `unknown command — /help` line, never an error dialog.

---

## §G. Keyboard shortcuts (TUI)

Keyboard-first, vim-friendly, no mouse. Shown live in the status bar (context-
sensitive — only the bindings valid for the focused pane).

| Key | Action |
|---|---|
| `Ctrl-C` | cancel the in-flight turn (does **not** exit) |
| `Ctrl-D` | exit (on empty input) |
| `Esc` | close palette / cancel completion / stop streaming |
| `Ctrl-K` | open slash-command palette |
| `Ctrl-L` | clear screen / repaint |
| `Tab` | cycle focus: Chat → Tool → Memory → Receipts → Input |
| `Shift-Tab` | cycle **mode**: chat → plan → auto-run (mirrors the survey's best idea) |
| `↑ / ↓` | input history (Input focus) / row select (pane focus) |
| `PgUp / PgDn` | scroll the focused pane |
| `Ctrl-T` | cycle theme (dawn → night → terminal) |
| `Ctrl-]` | cycle the collapsed right pane (narrow terminals) |
| `Ctrl-G` | open the input in `$EDITOR` for a long prompt |
| `Enter` | send |
| `Alt-Enter` / `Shift-Enter` | newline in input |

`Shift-Tab` mode cycling deserves emphasis: it is the single highest-leverage
control in the survey (Claude Code's `Shift+Tab`). **chat** = normal, **plan** =
agent proposes before acting, **auto-run** = tools run without per-call confirm. The
current mode shows in the status bar (`⇧⇥ mode:plan`).

---

## §H. Branding moments

Restraint: the brand appears at exactly three moments, never more.

**1. Splash (first launch only — gated on a `~/.config/ardur/.welcomed` marker):**

```


            _
       __ _ _ __ __| |_   _ _ __
      / _` | '__/ _` | | | | '__|
     | (_| | | | (_| | |_| | |
      \__,_|_|  \__,_|\__,_|_|

        the agent that keeps the receipts

        every action signed · every memory provable

        press any key to start  ▏


```

Logo in `primary`, tagline + sub-tagline in `dim`, centered. One keypress dismisses
it forever (writes the marker). `ardur --no-splash` skips it; `/about` reprints it.

**2. Status-bar mark (always, subtle):** the leading `ardur ·` in the brand bar /
top strip, in `primary` — a quiet persistent signature, never more than the word.

**3. `ardur --version` / `/about`:** logo + version + build hash + active provider.

**Emoji policy:** **off by default.** Enabled only if (a) the terminal reports
emoji-capable *and* (b) the active theme sets `emoji = true`. Even then, emoji are
confined to optional decoration (never load-bearing — the glyph/word pairing from
palette-law #1 always carries the meaning). The braille spinner, box glyphs, and `●`
health dot are **not** emoji and are always on (they degrade to ASCII on
`--ascii`/poor terminals).

---

## §I. What we explicitly do NOT do

Scope fences that keep v1 tight and stunning rather than sprawling and fragile:

- **No mouse support in the TUI.** Keyboard-first, like vim. (Click-to-focus and
  scroll-wheel are explicitly out; revisit post-v1 only if demanded.)
- **No clickable links beyond OSC-8.** We emit OSC-8 *only* where the terminal
  advertises support; everywhere else, `text (URL)`. No bespoke link protocols.
- **No images in the terminal.** No Sixel, no Kitty/iTerm image protocols — too
  fragile across terminals, and a stunning *text* experience is the whole bet.
- **No web frontend.** Terminal-native only for v1. (`ardur tui` is the "rich UI.")
- **No random whimsy.** No random spinner verbs, no easter-egg animations — truthful
  status only (§D).
- **No always-streaming sound / TTS / voice** in v1 (the gateways have it; we cut it
  to stay focused on *visual* excellence).
- **No more than 3 default themes + 2 daltonized variants.** Theming is extensible
  via config, but we ship a small, perfect set rather than a 32-theme picker.

If a downstream lane *wants* one of these, it is a **new** ADR, not a quiet addition.

---

## §J. Comparison table

How `ardur` (as designed here) stacks against the field. ★ = does it well; ◑ =
partial; ✗ = absent; **bold** = Ardur's unique or category-leading capability.

| Capability | Ardur (designed) | Hermes | OpenClaw | Claude Code | Codex |
|---|---|---|---|---|---|
| Rich inline markdown | ★ | ◑ (de-marked) | ✗/? | ★ | ★ |
| Syntax-highlighted code | ★ (syntect) | ◑ blocks | ? | ★ | ★ (syntect) |
| Tool calls in rounded boxes | **★** | ◑ feed | ◑ cards | ◑ blocks | ◑ inline |
| Always-on cost/context bar | ★ | ★ | ◑ | ✗ (pull) | ◑ footer |
| Context-pressure color ramp | ★ | ★ | ? | ✗ | ✗ |
| Full paned TUI | ★ | ◑ `--tui` | ★ | ✗ | ★ |
| Rich REPL **and** TUI (one binary) | **★** | ◑ | ◑ | ✗ (REPL only) | ✗ (TUI only) |
| **Live receipt-chain pane** | **★ unique** | ✗ | ✗ | ✗ | ✗ |
| **Live memory-snippet pane** | **★ unique** | ◑ memory | ◑ memory | ✗ | ◑ `/memories` |
| Daltonized themes | ★ | ✗ | ◑ contrast | ★ | ◑ |
| `terminal`-native theme | ★ | ◑ skins | ★ autodetect | ◑ | ★ |
| OSC-8 links | ★ | ? | ★ | ◑ | ? |
| `--json`/`--plain`/`NO_COLOR` | ★ | ◑ | ★ | ◑ | ◑ |
| Slash-command palette popover | ★ | ◑ list | ◑ | ◑ | ◑ |
| `Shift-Tab` mode cycling | ★ | ✗ | ◑ levels | ★ | ◑ |
| Tasteful single-line spinner | ★ truthful | ◑ kaomoji | ◑ | ★ random | ◑ |
| Mouse | ✗ (by design) | — | ◑ | ✗ | ✗ |
| In-terminal images | ✗ (by design) | ✗ | ◑? | ✗ | ✗ |

**Where Ardur wins outright:** the **live receipt-chain pane** and **memory pane**
(nobody else has verifiable receipts to show), and being the only tool that ships a
**polished rich REPL *and* a full TUI from one binary** with a *shared* renderer.
**Where Ardur matches the best:** rendering quality (Claude Code / Codex) and
status-bar discipline (Hermes / OpenClaw) — combined, which none of them do.

---

## Implementation roadmap

Three phases, each independently shippable. Downstream coding lanes pick up a phase;
each phase builds on the shared rendering core so work is additive, not throwaway.

### Phase 1 — REPL polish (the shared core)

*Goal: `ardur` (default) clears Claude Code's rendering bar.* Ships the rendering
core every later phase reuses.

- Markdown renderer (headings h1–3, lists, inline `code`/**bold**/*italic*,
  blockquotes, tables, links with OSC-8 detection) — §C.
- Syntax highlighting via `syntect` (rust/python/ts/sh/json minimum) — §C.
- Tool-call **rounded boxes** with title/footer/status/duration — §A.1.
- Dim **cost line** per turn with the context-pressure color ramp — §A.1, §B.2.
- Themes `dawn`/`night`/`terminal` via `$ARDUR_THEME` + `--theme`; palette with
  truecolor→256→16 fallback and `NO_COLOR`/`--plain`/`--ascii` degradation — §B, §E.
- Typing dots + truthful single-line spinner — §D.
- Top brand/status strip per turn; `--no-splash`/first-run splash — §A.1, §H.

**Exit criteria:** a markdown-heavy reply with code, a table, and a tool box renders
correctly in truecolor, 256-color, 16-color, and `NO_COLOR`; theme switch via env
var works; `--plain` produces clean pipeable output.

### Phase 2 — `ardur tui` MVP

*Goal: the paned shell exists and is usable.* Reuses Phase-1 rendering verbatim.

- ratatui app scaffold: brand bar + chat pane + input box + status bar (the right
  stack stubbed/empty) — §A.2.
- Chat pane renders via the Phase-1 core; basic scroll (PgUp/PgDn) — §A.2.
- Input box with multi-line (`Alt/Shift-Enter`), history (`↑/↓`) — §G.
- Slash-command **palette popover** (`/` + `Ctrl-K`), fuzzy filter — §F.
- Core keybindings: `Ctrl-C/D`, `Esc`, `Ctrl-K/L`, `Tab` (focus), `Enter` — §G.
- Live theme switch `Ctrl-T` + `/theme` — §E.

**Exit criteria:** a full chat session is comfortable entirely inside `ardur tui`;
palette opens/filters/runs; focus + scroll work; theme cycles live.

### Phase 3 — full TUI (the differentiators)

*Goal: the panes nobody else has, plus the animation polish.*

- Right stack wired to real data: **tool-calls feed**, **memory-snippets pane**, and
  the **live receipt-chain tail** with verify marks — §A.2.
- `Tab` focus across all 5 regions; selecting a tool-feed row expands it in chat;
  narrow-terminal pane collapse (`Ctrl-]`) — §A.2.
- Animations: smooth scroll, receipt tick fade-in, tool spinner in panes — §D.
- `Shift-Tab` mode cycling (chat/plan/auto-run) with status-bar indicator — §G.
- Branding: first-launch splash, persistent status-bar mark, `/about` — §H.
- Daltonized themes `dawn-cb`/`night-cb`; custom-theme TOML loading — §E.
- Remaining slash commands (`/sessions`, `/receipts`, `/copy`, `/save`, `/skill`) —
  §F; clipboard via `arboard`.

**Exit criteria:** the receipt-chain and memory panes update live during a real
session; every keybinding and slash command in §F/§G works; the splash + tagline
land on first launch; a non-technical observer says "wow."

---

*End of design specification.*
