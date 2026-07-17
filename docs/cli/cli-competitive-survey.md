# Ardur CLI — Competitive Survey

> Input for the *stunning CLI* design lane (§2.X). Surveys the three terminal
> agent CLIs the project benchmarks against — **Hermes Agent**, **OpenClaw**, and
> **Claude Code** — plus **OpenAI Codex CLI** as a fourth reference point because
> it is the most polished pure-coding TUI in the field. The goal is to extract
> reusable *design patterns*, not to copy any one of them.
>
> **Confidence note.** Repo existence and headline metrics were verified directly
> against the public GitHub API on 2026-06-04. Fine-grained visual details (exact
> glyphs, animation frames, status-bar layout) for Hermes and OpenClaw come from
> their own docs and are marked **[reported]** where they could not be confirmed
> against a live session. Claude Code details are firsthand (v2.1.163 running on
> this machine). Codex CLI details are from its published docs.

---

## 0. At-a-glance

| | Hermes Agent | OpenClaw | Claude Code | Codex CLI |
|---|---|---|---|---|
| Repo | `NousResearch/hermes-agent` | `openclaw/openclaw` | `anthropics/claude-code` | `openai/codex` |
| Stars (2026-06-04) | ~181k | ~377k | ~130k | ~89k |
| Created | 2025-07 | 2025-11 | 2025-02 | 2025-04 |
| Language | Python + TS | TypeScript | TS (Node) | Rust |
| Primary form | line REPL (+`--tui`) | TUI/control-plane | **line REPL** | **full TUI** |
| What it is | self-improving multi-channel agent | multi-channel assistant gateway | in-repo coding agent | in-repo coding agent |
| Markdown render | partial / *de-marked-down* | unverified | **rich, inline** | **rich + diffs** |
| Syntax highlight | code blocks only | unverified | **yes** | **yes (syntect)** |
| Cost/token line | **rich status bar** | `/usage` | `/cost` + statusline | `/status` footer |
| Themes | "skins" + personas | light/dark autodetect | named themes + daltonized | **32 syntect themes** |
| Mouse | n/a (REPL) | partial | no | no |
| License | MIT | MIT | proprietary | Apache-2.0 |

**The one-paragraph read.** Two of the four (Hermes, OpenClaw) are *multi-channel
assistant gateways* — their CLI is one surface among many (Telegram, Slack,
WhatsApp…), so their terminal UI optimizes for a persistent status bar and a
streaming activity feed rather than document-quality rendering. The other two
(Claude Code, Codex) are *in-repo coding agents* — their terminal UI is the whole
product, so they invest in markdown, syntax-highlighted diffs, and theming.
**Ardur wants the rendering quality of the coding agents with the glanceable
status-bar/observability instinct of the gateways** — and a full TUI that neither
coding agent currently ships.

---

## 1. Hermes Agent

**`NousResearch/hermes-agent`** — *"The agent that grows with you."* ~181k★, Python
(84%) + TypeScript, MIT, created 2025-07-22. A self-improving autonomous agent: a
learning loop that distills reusable *skills* from experience, persistent memory,
cron automations, and a gateway that bridges the CLI with Telegram, Discord, Slack,
WhatsApp, Signal, and email. Runs against hosted providers or fully local via
Ollama (needs ≥64K context — the 4K Ollama default breaks it). It is explicitly
**not** an IDE copilot; it is a long-running, server-resident agent.

**Form factor.** Two entry points: `hermes` (default **line-based REPL** with a
persistent fixed input prompt) and `hermes --tui` (a separate modern TUI with modal
overlays). The default is a REPL, not a paned TUI.

**Rendering.** Deliberately *de-marked-down*: it strips `**bold**`/`*italic*`
wrappers and noisy fences from final replies so output reads as clean terminal
prose, but **preserves code blocks and lists**. ANSI color throughout. This is an
interesting anti-pattern worth noting: they decided heavy markdown *hurts* terminal
readability and stripped it. Ardur's bet is the opposite — that *well-rendered*
markdown beats both raw markup and stripped prose.

**Status bar — the standout feature.** A persistent bar color-coded by context-window
pressure (green <50%, yellow 50–80%, orange 80–95%, red ≥95%) showing model · token
count · a visual fill bar · estimated **$ cost** · compression count · active
background tasks · elapsed time. Reported format **[reported]**:

```
⚕ claude-sonnet-4 │ 12.4K/200K │ [██████░░░░] 6% │ $0.06 │ 15m
```

**Tool-call display.** An animated feed with per-call icons and durations **[reported]**:

```
┊ 🔍 web_search (1.2s)
┊ 💻 terminal `gh pr list` (0.3s)
```

**Slash commands [reported].** `/help` `/model` `/tools` `/skills browse` `/new`
`/reset` `/compress` `/usage` `/background <prompt>` `/skin` `/voice on` `/voice tts`
`/reasoning high` `/title` `/status` `/sessions` `/busy queue|steer|interrupt`
`/personality`.

**Keyboard.** `Enter` send · `Alt+Enter`/`Ctrl+J`/`Shift+Enter` newline · `Ctrl+G`
open input in `$EDITOR` · `Ctrl+C` interrupt · `Ctrl+Z` background · `Tab` accept
autocomplete · `Ctrl+B` voice record · `Ctrl+V` paste text/images.

**Themes & personas.** Two distinct concepts: CLI **"skins"** (`/skin`) and built-in
**personalities** (persona/tone — `helpful, concise, technical, teacher, kawaii,
catgirl, pirate, shakespeare, noir, philosopher…`). The kawaii/catgirl default lean
is a strong, polarizing branding choice.

**Animations [reported].** A thinking indicator cycling kaomoji frames
(`◜ (｡•́︿•̀｡) pondering…` → `✧٩( ˊᗜˋ )و✧ got it!`). Paste shows
`[pasted: 47 lines, 1,842 chars — press Enter to send]`.

**Typical session [reconstructed]:**

```
⚕ claude-sonnet-4 │ 12.4K/200K │ [██████░░░░] 6% │ $0.06 │ 15m
› summarize the open PRs and message me on telegram
  ◜ (｡•́︿•̀｡) pondering...
  ┊ 🔍 web_search (1.2s)
  ┊ 💻 terminal `gh pr list` (0.3s)
  Three PRs are open. #84 docs sweep is ready to merge...
```

**Strengths.** Best-in-class glanceable status bar; persistent memory + skill
self-improvement; genuinely multi-channel; first-class local-model story (zero API
cost); hibernating serverless backends.

**Weaknesses.** Heavy/stateful — needs a server and ≥64K context, not a quick
scratch tool; very high open-issue count signals churn; de-markdown'd output and
kawaii-default branding read as unprofessional in serious settings; not aimed at
in-repo coding.

**What Ardur takes:** the **context-pressure-colored status bar** and the
**per-tool-call duration feed**. **What Ardur rejects:** stripping markdown, and
cutesy-by-default branding.

---

## 2. OpenClaw

**`openclaw/openclaw`** — *"Your own personal AI assistant. Any OS. Any Platform. The
lobster way. 🦞"* ~377k★, TypeScript, MIT, created 2025-11-24. A local-first
assistant *gateway/control-plane*: connects an agent to ~14 messaging channels
(WhatsApp, Telegram, Slack, Discord, Signal, iMessage, IRC, Teams, Matrix, LINE…)
plus companion desktop/mobile apps, voice wake/talk, a "Live Canvas" visualization,
and cron. Provider-agnostic (OpenAI/Codex and others). Architecturally a sibling of
Hermes — gateway + multi-channel + CLI/TUI surface — not a Codex-style coding agent.

**Form factor.** Several subcommands: `openclaw onboard` (guided wizard),
`openclaw gateway` (daemon), `openclaw message send`, and `openclaw tui` (aliases
`chat`, `terminal`) for the interactive surface. The TUI is a streaming chat log
**plus an observability/control plane** connected to the Gateway over WebSocket —
more dashboard-like than a plain REPL. It surfaces active channels, request/response
logs, model-usage tracking, and memory state.

**Rendering.** **Unverified** for markdown/syntax-highlighting. What *is* documented
is strong TTY hygiene: ANSI color + progress indicators only in a real TTY,
**OSC-8 hyperlinks** (clickable, with plain-URL fallback), **OSC 9;4** progress for
long-running commands, and `--json`/`--plain`/`--no-color` to disable styling for
scripting. v2026.3.8 added **light/dark background auto-detection** computing WCAG
2.1 contrast ratios on the fly **[reported]**.

**Tool-call display.** Rendered as **"tool execution cards"** inline in the chat
stream **[reported]** — card visual detail unverified.

**Cost/token.** Model-usage tracking surfaced in the TUI and via `/usage`; exact
per-message `$` rendering unverified.

**Slash commands [reported].** `/status` `/trace` `/config` `/debug` `/new` `/reset`
`/compact` `/think` `/verbose` `/usage` `/restart` `/activation` `/goal`
`/auth [provider]`. Session tools: `sessions_list` `sessions_history` `sessions_send`.

**Keyboard [reported].** `Ctrl+N` new session · `Ctrl+S` switch session · `Ctrl+T`
thinking level · `Ctrl+A` switch agent · `Ctrl+C` cancel · `Ctrl+D` exit.

**Branding.** The lobster (🦞) / "the lobster way." Streaming + TTY spinners. No
confirmed ASCII splash.

**Typical session [reconstructed, layout unverified]:**

```
$ openclaw tui
🦞 OpenClaw — connected to gateway (ws://localhost)
> draft a standup update from today's commits
  [tool] git.log ▸ running…  ✓ (0.4s)
  Here's your standup:
  • Landed PR #84 (docs sweep)
```

**Strengths.** Widest channel coverage of any tool here; local-first with a real
daemon; **excellent TTY rendering hygiene** (OSC-8 links, OSC 9;4 progress, contrast
auto-detection — the most terminal-correct of the four); script-friendly
(`--json`/`--plain`).

**Weaknesses.** UI specifics (markdown, tool cards, cost) thinly documented in
primary sources — hard to assess polish; Node-24/daemon footprint is heavy; much
secondary writeup is uneven SEO content; not a code-editing agent.

**What Ardur takes:** **OSC-8 hyperlinks, OSC 9;4 progress, background-color
auto-detection, and `--json`/`--plain` rendering modes** — the terminal-correctness
discipline. **What Ardur rejects:** the daemon-first, dashboard-heavy shape for the
*default* experience (Ardur's default is a clean REPL; the dashboard is `ardur tui`).

---

## 3. Claude Code

**`anthropics/claude-code`** — Anthropic's agentic coding tool. ~130k★, Node, created
2025-02-22. Firsthand: **v2.1.163** running on this machine. An in-repo coding agent;
the terminal *is* the product.

**Form factor.** **Line-based REPL** — no paned TUI. Output streams into the scrollback
as rendered blocks; the input is a single growing prompt at the bottom. This is the
exact form factor Ardur's *default* mode targets, so it is the closest direct
comparand for `ardur` (the non-`tui` binary).

**Rendering — the bar to clear.** Inline markdown rendered well: ATX headings, bullet
and numbered lists, inline `code`/**bold**/*italic*, tables, and
**syntax-highlighted fenced code blocks**. Tool calls render as labeled, expandable
blocks (a header line + collapsible body) — e.g. a `Bash` call shows the command,
then the output indented under it; large outputs collapse with a "ctrl+o to expand"
affordance. Diffs render with red/green gutters.

**Cost/token.** `/cost` prints session token + dollar totals; a configurable
**status line** (`/statusline`, a user script) can pin model/branch/cost at the
bottom.

**Slash commands (firsthand, representative).** `/help` `/clear` `/compact` `/model`
`/config` `/theme` `/statusline` `/cost` `/init` `/review` `/resume` `/memory`
`/agents` `/mcp` `/status` `/vim` `/export` `/bug` `/doctor` `/login` `/logout`
`/permissions` `/hooks`. Plus user/plugin skills surfaced as `/<skill-name>`.

**Keyboard (firsthand).** `Ctrl+C` interrupt turn · `Ctrl+D` exit · `Esc` interrupt
streaming · **`Shift+Tab` cycle permission modes** (normal → auto-accept → plan) ·
`Ctrl+R` toggle verbose/expand · `↑/↓` input history · `/vim` for modal editing ·
`Ctrl+L`-style clear via `/clear`.

**Themes.** Named themes selectable via `/theme` / `/config`: dark, light, and
**daltonized** (color-blind-safe) dark/light variants — a genuinely thoughtful
accessibility touch Ardur should copy.

**Animations (firsthand).** A single-glyph spinner (the `✻` sparkle) paired with a
cycling whimsical verb ("Pondering…", "Schlepping…", "Finagling…") and an elapsed
timer + token counter while waiting. Tasteful, not noisy — one line, no color churn.

**Branding (firsthand).** A boxed welcome splash on launch ("Welcome to Claude Code")
with the cwd and a couple of starter tips; the `✻` mark recurs as the spinner.

**Typical session [firsthand]:**

```
✻ Welcome to Claude Code
  ~/code/project  ·  shift+tab to plan

> refactor the auth guard to return Result

● I'll update the guard. Reading the file first…

  ⎿ Read auth.rs (42 lines)

● Here's the change:
  ```rust
  pub fn guard(req: &Request) -> Result<Claims, AuthError> { … }
  ```
  ✻ Finagling… (4s · ↑ 1.2k tokens)
```

**Strengths.** Best markdown + code rendering of the four; tasteful one-line spinner;
daltonized themes; `Shift+Tab` mode cycling is a brilliant low-friction control;
expandable tool blocks keep scrollback clean.

**Weaknesses.** **No full TUI** — no persistent panes for tool history / memory /
receipts (everything is linear scrollback); no always-on status bar by default (cost
is pull-based via `/cost`); proprietary.

**What Ardur takes:** the **inline markdown + syntax-highlight quality bar**,
**daltonized themes**, the **one-line tasteful spinner**, **`Shift+Tab` mode
cycling**, and **expandable tool blocks**. **What Ardur adds on top:** the persistent
panes Claude Code lacks (the entire point of `ardur tui`).

---

## 4. OpenAI Codex CLI (reference)

**`openai/codex`** — *"Lightweight coding agent that runs in your terminal."* ~89k★,
**Rust**, Apache-2.0, created 2025-04-13. Included because it is the most polished
*full-TUI coding agent* in the field and the closest analogue to `ardur tui`'s
ambition — and because it is also Rust, so its crate choices are directly relevant.

**Form factor.** Interactive **paned terminal UI** (Rust), with a branded splash
screen. This is the closest existing thing to what `ardur tui` wants to be.

**Rendering.** Syntax-highlighted fenced code blocks and **file diffs** in the TUI.
`/theme` opens a **live-preview theme picker backed by `syntect` with 32 bundled
themes** — strong evidence that `syntect` is the right syntax-highlight crate for a
Rust TUI (Ardur's ADR adopts it).

**Tool/command display.** Shows a plan, then **approve/reject command executions
inline**; `/permissions` adjusts the approval policy mid-session; `/approve` retries
a denied action; `/diff` shows exact file changes including untracked files.

**Cost/token.** `/status` shows model, approval policy, writable roots, and **current
token usage**; `/statusline` configures footer items.

**Slash commands (published, extensive).** `/model` `/fast` `/personality` `/plan`
`/goal` `/clear` `/new` `/permissions` `/approve` `/review` `/agent` `/fork`
`/side`(`/btw`) `/resume` `/status` `/diff` `/mcp` `/theme` `/keymap` `/statusline`
`/copy` `/raw` `/compact` `/mention` `/skills` `/plugins` `/apps` `/hooks`
`/memories` `/vim` `/ide` `/init` `/ps` `/stop` `/feedback` `/logout`.

**Keyboard.** `Ctrl+L` clear · `Ctrl+O` copy last output · `Ctrl+R` search prompt
history · `/keymap` remap · `/vim` mode.

**Strengths.** Mature Rust single-binary; real syntax-highlighted diffs + approval
gating; deep config surface (themes, keymap, MCP, plugins, hooks); 32 syntect themes.

**Weaknesses.** OpenAI-account-gated; coding-only (not multi-channel); approval
prompts add friction in fast loops.

**What Ardur takes:** **`syntect`-backed theming with live preview**, the **rich
diff renderer**, the **breadth of the slash-command surface as a menu of ideas**, and
proof that a **Rust + ratatui-class TUI** is the right technical bet.

---

## 5. Synthesis — the gap Ardur fills

Plotting the four on two axes makes the opening obvious:

```
                 rich rendering / document-quality
                              ▲
                Codex CLI ●    │    ● Claude Code
              (full TUI,       │     (REPL only,
               diffs, 32       │      great md,
               themes)         │      no panes)
                              │
   ◄──────────────────────────┼──────────────────────────►
   coding-agent shape         │         gateway / multi-channel shape
                              │
                              │    ● Hermes  ● OpenClaw
                              │     (great status bar / TTY hygiene,
                              │      weak or stripped rendering)
                              ▼
                 glanceable status / observability
```

- **Top-left (Codex):** full TUI + great rendering, but coding-only and no
  observability instinct.
- **Top-right (Claude Code):** great rendering, but *REPL only* — no panes.
- **Bottom (Hermes/OpenClaw):** great status bars and terminal-correctness, but weak
  document rendering.

**No single tool occupies the top-center-plus-observability quadrant: rich rendering
*and* a glanceable status/observability surface *and* a real TUI.** That is exactly
where Ardur aims:

1. **Default `ardur` (rich REPL):** clear Claude Code's rendering bar — inline
   markdown, syntax highlight, tasteful one-line spinner — and add what Claude Code
   lacks by default: a persistent, **context-pressure-colored cost/status line**
   (Hermes' best idea) and **tool calls in rounded boxes** with durations.
2. **`ardur tui` (full TUI):** the paned shape Codex proves works, but wired to
   Ardur's distinctive substrate — a **right-side stack of tool-feed / memory /
   receipt-chain panes** that no competitor shows, because no competitor *has*
   verifiable receipts and a memory layer to surface.

**The three things Ardur can uniquely own:**

- **Receipt-chain tail pane** — a live view of the signed receipt chain. None of the
  four has this; it is Ardur's cryptographic-provenance differentiator made visible.
- **Memory-snippet pane** — recent memory records surfaced alongside the chat, not
  buried behind a `/memory` command.
- **Rendering + observability together** — the document quality of the coding agents
  with the glanceable status discipline of the gateways, in one keyboard-first TUI.

The rest of the design (`cli-stunning-design.md`) specifies exactly how.

---

## Appendix — sources

- Hermes: `github.com/NousResearch/hermes-agent`,
  `github.com/NousResearch/hermes-agent` user-guide CLI docs, `docs.ollama.com`
  Hermes integration.
- OpenClaw: `github.com/openclaw/openclaw`, `docs.openclaw.ai/cli` (+ `/tui`, `/agent`),
  `terminaltrove.com/openclaw`.
- Claude Code: firsthand, v2.1.163; `github.com/anthropics/claude-code`.
- Codex CLI: `github.com/openai/codex`, `developers.openai.com/codex/cli`
  (+ slash-commands), Codex PR #11447 (syntect 32-theme picker).
- Star counts / dates: public GitHub API, retrieved 2026-06-04.
