# Running ardur-server

This is the operator runbook for `ardur-server`, the HTTP service that
accepts Slack events, drives the fused-runtime agent loop, and replies in
the originating Slack channel.

> **Status: dev fidelity, not production.** Several durability and defence
> gaps are still open (see [Known gaps](#known-gaps)). Run this in a private
> Slack channel before exposing it to anyone else.

## Prerequisites

- A Slack workspace where you can install a bot app.
- A Slack bot app with at minimum the `chat:write` scope.
- The Slack **signing secret** and **bot token** for that app.
- Credentials for the model backend you intend to run (see
  [Selecting a provider](#selecting-a-provider)) — an Anthropic API key by
  default.
- Docker + Docker Compose (local dev) or any Docker host (production).

## Selecting a provider

Both `ardur-server` and the `ardur` CLI pick their model backend at boot from
the `ARDUR_PROVIDER` environment variable. The value is case-insensitive; an
unset or empty value defaults to `anthropic`. An **unrecognized** value aborts
the process at boot with a message listing the supported values — a typo never
silently downgrades to a different backend. The selected provider is logged at
startup (`using provider provider=<id>`).

| `ARDUR_PROVIDER` | Backend | Required / notable env |
|---|---|---|
| `anthropic` (default) | Anthropic Messages API | `ANTHROPIC_API_KEY` |
| `openrouter` | OpenRouter HTTP gateway | `OPENROUTER_API_KEY` |
| `openai-compat` (alias `openai`) | Generic OpenAI-compatible Chat Completions endpoint | `OPENAI_COMPAT_API_KEY` preferred; `OPENAI_API_KEY` fallback for OpenAI proper; `OPENAI_COMPAT_BASE_URL` default `https://api.openai.com/v1`; `OPENAI_COMPAT_TIMEOUT_SECS` |
| `ollama` | Ollama local daemon **or** hosted cloud | `OLLAMA_BASE_URL` (default `http://localhost:11434`); `OLLAMA_API_KEY` (cloud only — its presence auto-defaults the base URL to `https://ollama.com`) |
| `codex` | OpenAI Codex CLI (ChatGPT subscription) | `CODEX_BINARY` (default: `codex` on `PATH`), `CODEX_DEFAULT_MODEL`, `CODEX_SANDBOX_MODE` (`read-only` \| `workspace-write` \| `danger-full-access`), `CODEX_WORKING_DIR` |
| `claude-cli` (alias `claude-subscription`) | Claude Code CLI (Anthropic subscription) | `CLAUDE_CLI_BINARY` (default: `claude` on `PATH`), `CLAUDE_CLI_DEFAULT_MODEL`, `CLAUDE_CLI_PERMISSION_MODE` (`default` \| `acceptEdits` \| `auto` \| `bypassPermissions` \| `dontAsk` \| `plan`), `CLAUDE_CLI_WORKING_DIR`, `CLAUDE_CLI_ALLOWED_TOOLS`. Run `claude login` once; spends the **Agent SDK Credit pool** ($20–$200/mo, plan-dependent), not unbounded |

One-liners (CLI shown; the server reads the same variables from its `.env`):

```sh
# Anthropic (default) — equivalent to leaving ARDUR_PROVIDER unset
ARDUR_PROVIDER=anthropic ANTHROPIC_API_KEY=sk-ant-... ardur chat

# OpenRouter
ARDUR_PROVIDER=openrouter OPENROUTER_API_KEY=sk-or-... ardur chat

# OpenAI-compatible endpoint — defaults to https://api.openai.com/v1
ARDUR_PROVIDER=openai-compat OPENAI_COMPAT_API_KEY=sk-... ardur chat
# OpenAI proper can use the standard OpenAI key name as a fallback
ARDUR_PROVIDER=openai OPENAI_API_KEY=sk-... ardur chat

# Ollama — local daemon (no key)
ARDUR_PROVIDER=ollama ardur chat
# Ollama — hosted cloud (a key auto-targets https://ollama.com)
ARDUR_PROVIDER=ollama OLLAMA_API_KEY=... ardur chat

# Codex — uses your logged-in ChatGPT subscription via the codex CLI
ARDUR_PROVIDER=codex ardur chat

# Claude CLI — uses your logged-in Anthropic subscription via the claude CLI
ARDUR_PROVIDER=claude-cli ardur chat
```

The Anthropic, OpenRouter, and OpenAI-compatible backends fail at boot if their
API key is missing (the CLI then falls back to a network-free stub and prints an
offline notice; the server aborts). `OPENAI_COMPAT_BASE_URL` must use HTTPS
unless it targets loopback HTTP for local tests. The Ollama, Codex, and
Claude-CLI backends need no credentials to wire — they fail later, per-turn, if
the daemon/binary is unreachable or the CLI is not logged in.

## Selecting a memory backend

`ardur-server` picks its bi-temporal memory substrate at boot from the
`ARDUR_MEMORY` environment variable. An unset or empty value defaults to
`in_memory`.

| `ARDUR_MEMORY` | Backend | Required / notable env |
|---|---|---|
| `in_memory` (default) | In-process §7.0 Phase 1 store | none — **lost on restart** |
| `qdrant` | Durable, Qdrant-backed §7.0 Phase 2 store | `QDRANT_URL` (**required**); `QDRANT_API_KEY` (cloud only), `QDRANT_COLLECTION` (default `ardur_memory`), `QDRANT_VECTOR_DIM` (default `384`), `EMBED_MODEL` (default `bge-small-en-v1.5`) |
| `hybrid` | §7.0c dense+sparse retriever over the durable store | same as `qdrant` (`QDRANT_URL` **required**) plus a BM25 lexical index persisted under `<ARDUR_DATA_DIR>/memory/bm25` |

The default `in_memory` store is fast but volatile: every fact is gone when the
process restarts. The `qdrant` backend upserts each bi-temporal record as a
Qdrant point so memory survives a restart (or a pod reschedule). When
`ARDUR_MEMORY=qdrant`, `QDRANT_URL` is **required** — a missing URL aborts at
config-load (the same conditional shape as `ANTHROPIC_API_KEY` under the
Anthropic provider) rather than failing silently at first use.

The `hybrid` backend (§7.0c) boots the same durable Qdrant store **and** a
file-backed BM25 lexical index plus the embedder, wrapping them in a
`HybridMemoryRetriever` that adds fused dense+sparse recall behind the same
`MemoryRuntime` seam. It shares every `QDRANT_*` / `EMBED_MODEL` knob with
`qdrant` (and likewise **requires** `QDRANT_URL`); the BM25 half persists under
`<ARDUR_DATA_DIR>/memory/bm25` so the lexical index survives restarts too. The
embedder is downloaded and disk-cached on first boot.

```sh
# Durable memory against a local Qdrant (gRPC on 6334).
docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant
ARDUR_MEMORY=qdrant QDRANT_URL=http://localhost:6334 ardur-server

# Or fused dense+sparse recall over that same Qdrant (§7.0c).
ARDUR_MEMORY=hybrid QDRANT_URL=http://localhost:6334 ardur-server
```

The collection (Cosine distance, the configured dim) and its payload indexes
(`subject`, `channel_id`, `session_id`) are created automatically on first boot
if absent.

### Embeddings + hybrid retrieval

The Qdrant store embeds each record's searchable text (its `predicate object`,
falling back to the payload text) through a local [`fastembed`](crates/embeddings)
model — no API key, the model is downloaded once and cached on disk. `EMBED_MODEL`
selects it (`bge-small-en-v1.5` default / `gte-base-en-v1.5` / `all-minilm-l6-v2`);
the collection's vector dim is realigned to the model automatically, so
`QDRANT_VECTOR_DIM` only matters for a store left without an embedder (which keeps
the legacy placeholder vector — bi-temporal reads still work, only vector *search*
is meaningless).

`HybridMemoryRetriever` (in `ardur-memory-qdrant`) layers **dense** vector search
and **sparse** BM25 lexical search over the same store and fuses them with
reciprocal-rank fusion — see that crate's `README.md`. As of EPIC-TRUST the
turn path calls `MemoryRuntime::search_scoped` after cap-token verification and
Cedar authorization, then injects matching memory cards into the provider
request. The hybrid backend implements that seam with dense+sparse recall and a
subject/workspace filter; the in-process backend provides a deterministic
lexical fallback for local/offline runs.

### CLI memory explorer

Interactive `ardur chat` sessions expose a scoped memory explorer. The commands
operate only on the verified session holder's memory view and show provenance,
confidence, validity, TTL, and receipt ids when present:

```text
/memory list          # list current memory cards for this holder
/memory list --json   # export cards as JSON
/memory show <id>     # show one card with full payload/provenance
/memory forget <id>   # append a receipt-linked tombstone for the card
```

Writes made by the fused turn path are authorized through `MemoryControlPlane`,
so the verified cap-token must include `memory.write` before the turn can create
memory side effects. Successful writes are receipt-chained (`source_receipt_id`
is set to the turn receipt). `forget` is append-only: the original card remains
in history, and a tombstone/invalidation row carries the receipt linkage forward.
Direct `/memory show <id>` only returns live cards; after a successful forget,
show returns not-found rather than disclosing the historical payload.

Programmatic/operator memory mutations should use `ardur_memory::MemoryControlPlane`
rather than calling a backend directly. The control plane enforces cap-token
claims (`memory.read` for list/show, `memory.write` for record/forget), evaluates
Cedar (`Action::"MemoryList"`, `Action::"MemoryShow"`, `Action::"MemoryRecord"`,
`Action::"MemoryForget"`), rejects cross-workspace subjects, and rejects memory
writes that are not linked to a receipt.

Useful memory verification commands:

```sh
cargo test -p ardur-memory --test authorized_operations
cargo test -p ardur-cli --test memory_commands
cargo test -p ardur-fused-runtime --test memory_recall
cargo test -p ardur-fused-runtime --test receipt_atomic_commit

# Full hybrid retriever tests with real Qdrant and deterministic mock embeddings.
docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant
QDRANT_INTEGRATION_TEST=1 QDRANT_URL=http://localhost:6334 \
  cargo test -p ardur-memory-qdrant --test hybrid_integration

# Full chat → hybrid memory store → Qdrant/BM25 recall → memory display pipeline.
QDRANT_INTEGRATION_TEST=1 QDRANT_URL=http://localhost:6334 \
  cargo test -p ardur-e2e-tests --test scenario_hybrid_memory_full_pipeline
```

## Slack app setup

1. Create a Slack app at https://api.slack.com/apps → **From scratch**.
2. **OAuth & Permissions** → add bot scope `chat:write`. Install to your
   workspace. Copy the **Bot User OAuth Token** (`xoxb-…`) into
   `SLACK_BOT_TOKEN`.
3. **Basic Information** → copy the **Signing Secret** into
   `SLACK_SIGNING_SECRET`, and the **App ID** (`A…`) into `SLACK_APP_ID`.
4. **Event Subscriptions** → enable, set the request URL to
   `https://your-host/slack/events`, and subscribe to bot events
   `message.channels` and `message.im`.
5. Invite the bot to the channel(s) you want it to listen on
   (`/invite @ardur`).

## Matrix channel (optional second channel)

ardur can run a Matrix bot alongside Slack. Matrix is an open, federated,
Rust-native protocol — a good fit for self-hosted deployments. The adapter
(`crates/channel-matrix`, built on `matrix-sdk` 0.18) is **off by default**;
enable it with `ARDUR_CHANNEL_MATRIX=true`.

1. **Provision a bot account** on your homeserver (e.g. matrix.org or a
   self-hosted Synapse/Conduit). Register a user such as `@ardur-bot:your.hs`.
2. **Mint an access token** for the bot (access-token auth is preferred over a
   password for bots). Either from a client (Element → Settings → Help & About →
   Access Token) or via the login API:
   ```sh
   curl -XPOST https://your.hs/_matrix/client/v3/login \
     -d '{"type":"m.login.password","identifier":{"type":"m.id.user","user":"ardur-bot"},"password":"…"}'
   # copy the "access_token" (and "device_id") from the response
   ```
3. **Configure the env** (see `.env.example`):
   ```sh
   ARDUR_CHANNEL_MATRIX=true
   MATRIX_HOMESERVER_URL=https://your.hs
   MATRIX_USER_ID=@ardur-bot:your.hs
   MATRIX_ACCESS_TOKEN=syt_…
   MATRIX_DEVICE_ID=ARDUR_BOT            # use the device_id from step 2 for E2EE
   MATRIX_STATE_DIR=/var/lib/ardur/matrix-state
   MATRIX_AUTO_JOIN_INVITES=false
   MATRIX_ALLOWED_ROOMS=!abc:your.hs     # required: comma-separated room allowlist
   ```
   When `ARDUR_CHANNEL_MATRIX=true`, the three `MATRIX_*` credentials are
   required at startup (the boot fails fast if any is missing).
4. **Opt into rooms.** Set `MATRIX_ALLOWED_ROOMS` to a comma-separated list of
   room ids (`!abc:your.hs,!def:your.hs`) before enabling the channel. The bot
   ignores all messages outside that allowlist. `MATRIX_AUTO_JOIN_INVITES=false`
   is the safe default; if you temporarily set it to `true`, auto-join still only
   joins rooms already present in `MATRIX_ALLOWED_ROOMS`.
5. **End-to-end encryption.** The adapter is built with E2EE on. For encrypted
   rooms, give the bot a **stable `MATRIX_DEVICE_ID`** and a durable
   `MATRIX_STATE_DIR` (it holds the sqlite crypto store — treat it as a secret),
   and **verify the bot's device** from a trusted session on first run.
   Otherwise messages in encrypted rooms may be undecryptable until keys are
   shared. See `crates/channel-matrix/README.md` for the full caveat.

Inbound Matrix messages run through the same fused turn pipeline as Slack, and
replies are posted back into the originating room.

## Discord channel (optional)

ardur can run a Discord bot alongside Slack. The adapter
(`crates/channel-discord`, built on `serenity` 0.12) is **off by default**;
enable it with `ARDUR_CHANNEL_DISCORD=true`.

1. **Create an application + bot** at <https://discord.com/developers/applications>.
   Copy the **Application ID** (General Information) and the **Bot Token**
   (Bot → Reset Token).
2. **Enable the privileged Message Content intent** (Bot → Privileged Gateway
   Intents → Message Content Intent). Without it, inbound message `content`
   arrives empty and the bot sees nothing to answer.
3. **Configure the env** (see `.env.example`):
   ```sh
   ARDUR_CHANNEL_DISCORD=true
   DISCORD_BOT_TOKEN=…
   DISCORD_APPLICATION_ID=123456789012345678
   DISCORD_ALLOWED_CHANNELS=        # optional: restrict to specific channel ids
   ```
   When `ARDUR_CHANNEL_DISCORD=true`, `DISCORD_BOT_TOKEN` and
   `DISCORD_APPLICATION_ID` are required at startup (the boot fails fast if
   either is missing).
4. **Invite the bot** to your server with the `bot` scope and the *Send
   Messages* / *Read Message History* permissions, then talk to it in any
   channel it can see. To restrict it, set `DISCORD_ALLOWED_CHANNELS` to a
   comma-separated list of channel ids; messages elsewhere are dropped.

The bot drops its own messages (its user id equals its application id), so it
never answers itself.

## Telegram channel (optional)

ardur can run a Telegram bot alongside Slack. The adapter
(`crates/channel-telegram`, built on `teloxide` 0.17) is **off by default**;
enable it with `ARDUR_CHANNEL_TELEGRAM=true`.

1. **Create a bot** by talking to [@BotFather](https://t.me/BotFather)
   (`/newbot`). Copy the `<id>:<secret>` token it gives you.
2. **Configure the env** (see `.env.example`):
   ```sh
   ARDUR_CHANNEL_TELEGRAM=true
   TELEGRAM_BOT_TOKEN=123456:ABC-DEF…
   TELEGRAM_ALLOWED_CHATS=          # optional: restrict to specific chat ids
   ```
   When `ARDUR_CHANNEL_TELEGRAM=true`, `TELEGRAM_BOT_TOKEN` is required at
   startup. Only **one** process may long-poll a given bot token at a time
   (Telegram returns `409 Conflict` otherwise).
3. **Start a chat** with the bot or add it to a group. Telegram chat ids are
   signed (negative for groups/supergroups, positive for private chats); use
   [@userinfobot](https://t.me/userinfobot) to find one. To restrict the bot,
   set `TELEGRAM_ALLOWED_CHATS` to a comma-separated list of chat ids.

Inbound Discord and Telegram messages run through the same fused turn pipeline
as Slack, and replies are posted back into the originating channel/chat.

## Local development

```sh
cp .env.example .env
# edit .env with your tokens
docker compose up --build
# in a second terminal:
ngrok http 3000
# paste the https://….ngrok-free.app URL into the Slack app's
# Event Subscriptions request URL field, append /slack/events
```

Slack will hit `/slack/events` with a `url_verification` challenge first;
the adapter responds with the matching `challenge` payload and Slack marks
the URL verified. Subsequent `event_callback` payloads then flow through to
the fused runtime.

## Production

```sh
docker run -d \
    --name ardur-server \
    --restart=unless-stopped \
    -p 3000:3000 \
    -v ardur-data:/var/lib/ardur \
    -e ARDUR_BIND_ADDR=0.0.0.0:3000 \
    --env-file .env \
    ardur-server:latest
```

**Do NOT expose port 3000 directly to the public internet.** Always run
behind a TLS-terminating proxy — nginx, Caddy, Traefik, or a Cloudflare
Tunnel — so Slack's signed requests arrive over HTTPS and the signature
verification basestring includes a real `Host`. The container itself
listens on plain HTTP at `$ARDUR_BIND_ADDR` (default `127.0.0.1:3000`; set
`ARDUR_BIND_ADDR=0.0.0.0:3000` inside containers behind a private Docker network
or reverse proxy).

## Persistent state

`/var/lib/ardur/` is the data directory (configurable via `ARDUR_DATA_DIR`).
It contains:

| Path | Purpose |
|---|---|
| `memory/` | bi-temporal memory store (per-session + global) — used only by the default `in_memory` backend; with `ARDUR_MEMORY=qdrant` the durable store lives in Qdrant, not on this volume |
| `journals/` | append-only session journals (replay source of truth) |
| `receipts/` | signed receipt chain (JWS-ES256) |
| `keys/` | issuer keys — **`keys/issuer.pem` is the root of trust for the receipt chain. Back this up. Losing it invalidates every prior receipt.** |

Back the whole volume up regularly. The receipt chain is content-addressed
and append-only; a corrupted or missing journal entry breaks replay.

## MCP (Model Context Protocol)

ardur speaks MCP both ways, built on the official [`rmcp`](https://crates.io/crates/rmcp)
Rust SDK: it **serves** its local tools to any MCP client, and can **consume**
tools from remote MCP servers.

The surface is off by default. Enable it with:

```bash
ARDUR_MCP_ENABLED=true
ARDUR_MCP_BEARER_TOKENS=token-one,token-two   # required when enabled
ARDUR_MCP_PATH_PREFIX=/mcp                     # default
# client side: consume remote servers (name=url,…)
# ARDUR_MCP_REMOTE_SERVERS=weather=https://mcp.example.com/mcp/weather
```

**Server.** When enabled, the Streamable-HTTP transport mounts at
`<prefix>/{server_name}` (e.g. `POST /mcp/ardur`), handling the MCP
`GET`/`POST`/`DELETE` methods. Direct MCP currently exposes only
capability-free tools because the direct MCP path has bearer auth but does not yet
derive a fused-runtime cap-token/Cedar context:

| Tool | Purpose |
|---|---|
| `echo` | returns its input arguments unchanged (round-trip check) |
| `health_check` | reports uptime, selected provider, and memory backend |

Capability-bearing tools such as `voice.transcribe` remain available to fused
runtime turns, where `ToolInvoke` cap-token/Cedar checks and signed tool receipts
are enforced, but they are filtered out of direct MCP `tools/list` and rejected
from direct MCP `tools/call` until MCP gets the same scoped invocation context.

**Auth.** Every MCP request must carry `Authorization: Bearer $ARDUR_MCP_TOKEN`
where the token matches one entry in `ARDUR_MCP_BEARER_TOKENS` (constant-time
compare); anything else is `401`. The bearer allowlist is the security boundary —
the transport's default loopback-only DNS-rebinding guard is lifted so remote
clients can connect, so **front the endpoint with your own TLS/ingress.**

Quick check against a running server:

```bash
curl -sS http://localhost:3000/mcp/ardur \
  -H "Authorization: Bearer ${ARDUR_MCP_TOKEN}" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
# omit the Authorization header → 401
```

**Client.** `ARDUR_MCP_REMOTE_SERVERS` (`name=url,…`) is connected at boot: each
server's tools join the runtime's tool registry alongside the local ones, so the
model can call them in a turn (see **Tool use** below). A remote server that
fails to connect or list is logged and skipped — one dead remote does not take
the agent down.

## Tool use

When a provider's completion comes back requesting tool calls, the runtime
**invokes** the tools and loops the results back to the model until it produces a
final answer (§6.0). Each round runs the full pipeline — cost-gate admission,
injection-defense scanning of the tool output, and a signed receipt that records
the calls — so tool use is governed and audited like the rest of a turn.

The tools available in a turn are the local ones (`echo`, `health_check`),
optional `voice.transcribe` (see **Voice transcription** below), any filesystem
**skills** (see **Skills** below), plus any from `ARDUR_MCP_REMOTE_SERVERS`. Two
safeguards bound the loop:

```bash
ARDUR_TOOL_MAX_ITERATIONS=5    # provider rounds that may request tools (default 5)
ARDUR_TOOL_TIMEOUT_SECS=30     # per-tool-call deadline (default 30)
```

A turn that keeps requesting tools past the iteration ceiling aborts with a
`tool-call loop exhausted` error; a tool that overruns its deadline aborts with a
`tool timed out` error (releasing the cost reservation in both cases). An unknown
tool name, or a tool output that trips the injection filter, likewise aborts the
turn before it can affect the conversation.

Provider support (Phase 1): **anthropic** (Messages API `tool_use`),
**openrouter** (OpenAI-compatible `tools`/`tool_calls`), and
**openai-compat** (OpenAI-compatible `tools`/`tool_calls`). The `codex` and
`claude` CLI providers orchestrate their own tools internally, so the runtime
loop does not drive tools through them.

## Platform integrations

EPIC-PLATFORM adds dedicated platform crates for browser, terminal, and web
operations. They are designed to be registered as tools through the same runtime
path described above, so a model-requested call is admitted only after cap-token
verification, Cedar `Action::ToolInvoke`, cost-gate admission, injection-defense
output scanning, and signed runtime receipt creation. The tool implementations
also carry local fail-closed checks for direct use in tests and offline harnesses:
an empty cap-token is denied, and a direct `ToolContext` with
`ARDUR_CEDAR_DECISION=deny` is refused.

### Browser automation (`ardur-browser`)

Tools: `browser.navigate`, `browser.click`, `browser.type`,
`browser.screenshot`, and `browser.extract`.

- Configure a `BrowserPolicy` with `SiteAction::new("example.com", "click")`
  style site/action allowlist entries. Empty allowlists deny external sites.
- `ConfirmationLevel::ExternalConsequences` requires a `confirmed: true`
  argument for sensitive write actions such as clicking, typing, form-fill, form
  submit, and downloads. `ConfirmationLevel::EveryAction` requires confirmation
  for read-only actions too.
- Every browser action appends a browser-action receipt with `{id, parent_id,
  action, target, timestamp_ms}` and includes it in `ToolOutput.receipt_data`, so
  browser actions can be chained into the signed runtime receipt.

### Terminal backends (`ardur-terminal`)

Backends: local shell, Docker exec, SSH remote, and Modal/cloud sandbox.

- `TerminalPolicy::allow_commands(["printf", "python"])` allowlists commands by
  first shell token; the default policy is deny-all. Each backend checks policy
  before execution.
- Local execution runs `/bin/sh -c <command>` with timeout and bounded stdout /
  stderr capture.
- Docker execution uses the [`bollard`](https://crates.io/crates/bollard) Docker
  daemon API (`create_exec` + `start_exec`) against an existing container. For
  live tests or operations, start Docker Desktop / the Docker daemon first and
  pass a running container id/name.
- SSH execution requires a configured host-key fingerprint and uses strict host
  key checking for live command execution. The backend carries a `russh` client
  config so the implementation can move fully native without changing callers.
- Modal/cloud execution posts `{ "command": ... }` to a configured HTTPS sandbox
  endpoint and requires `MODAL_TOKEN_ID` for live use. Offline tests use mock
  backends when cloud credentials are absent.
- `terminal.exec` receipts include backend name, action, command digest, and
  timestamp; runtime receipts still sign the enclosing tool-call digest.

### Web capabilities (`ardur-web`)

Tools: `web.fetch`, `web.parse`, `web.screenshot`, and `web.form_fill`.

- `web.fetch` enforces HTTPS for external URLs. HTTP is allowed only for loopback
  development URLs when `WebPolicy::dev_loopback()` is used.
- `WebPolicy::with_allowlist(["example.com"])` narrows eligible hosts; each
  fetch/screenshot/form-fill validates the URL before network or browser work.
- `web.parse` extracts titles, selector text, links, and form metadata from HTML
  without network access.
- `web.form_fill` denies `submit: true` unless the caller supplies
  `confirmed: true`; field-fill previews without submit remain allowed under the
  URL policy.
- Each web operation returns receipt metadata under `ToolOutput.receipt_data`.

## Voice transcription (Whisper)

EPIC-TOOLS adds a concrete `TranscriptionProvider`: `WhisperApiTranscriptionProvider`
in `ardur-media-audio`. When the server assembles its tool registry it attempts
to register `voice.transcribe`; missing Whisper credentials are a graceful
degradation (the server still boots, logs that the tool is disabled, and the
tool is simply absent from the registry).

Environment:

```bash
OPENAI_WHISPER_API_KEY=sk-...        # preferred for Whisper only
# or OPENAI_API_KEY=sk-...           # fallback
OPENAI_WHISPER_BASE_URL=https://api.openai.com/v1   # optional; loopback HTTP allowed for tests
OPENAI_WHISPER_MODEL=whisper-1       # optional default
```

`voice.transcribe` takes base64 audio bytes and a declared format (`mp3`, `wav`,
`ogg`/`opus`, `flac`, `m4a`, or `webm`) and returns transcript text, language,
provider/model ids, and the provider-level receipt hash. The runtime still gates
the tool call before invocation with the normal cap-token allowlist and Cedar
`Action::"ToolInvoke"` check, then records the invocation in the signed turn
receipt. The provider itself also validates the audio request, refuses empty or
oversized inline audio before any network call, enforces the declared duration
ceiling, validates the Whisper base URL (HTTPS except loopback HTTP for tests),
and hash-chains provider operation receipts.

Example tool arguments (when a model asks to call it through the fused runtime):

```json
{
  "audio_base64": "UklGRiQAAABXQVZF...",
  "format": "wav",
  "duration_seconds_upper_bound": 30,
  "language_hint": "en",
  "mission_id": "mission.voice-note"
}
```

## ACP (Agent Communication Protocol)

EPIC-TOOLS wires ACP two ways:

- `ardur-acp::StdioAcpTransport` carries newline-delimited JSON-RPC 2.0 ACP
  frames over any stdio-like async reader/writer (child-process stdin/stdout,
  or `tokio::io::duplex` in tests).
- `POST /acp` accepts one ACP JSON-RPC request over HTTP. It uses the same bearer
  gate as `/chat`, then dispatches only explicitly supported methods. Currently
  `initialize` and `session/prompt` are accepted; unsupported methods return a
  JSON-RPC `-32601` error before provider work, journaling, or receipt writes.
  Accepted requests are submitted through the fused runtime so the request is
  cap-token verified, Cedar-authorized, cost-gated, journaled, and
  receipt-chained before the JSON-RPC response is returned.

Quick HTTP check against a running server:

```bash
curl -sS http://localhost:3000/acp \
  -H "Authorization: Bearer $ARDUR_CHAT_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}'
```

Successful responses are ACP JSON-RPC responses whose `result` includes
`accepted: true`, the method, the fused-runtime reply, and the signed
`receipt_id`. Missing/invalid bearer tokens return `401`; malformed ACP messages
return `400`; unsupported ACP methods return JSON-RPC `-32601`; runtime failures
are returned as JSON-RPC errors with `502`.

## Automation task-flow DAGs

`ardur-automation::DefaultTaskFlowOrchestrator` is now an in-memory production
default rather than a `NotImplemented` placeholder. It validates non-empty
descriptions, cap-token allowlists for every declared dispatch, DAG
version/depth/fanout, and fail-closed control-flow constraints. Empty sequence or
parallel controls, invalid `AnyN`, Cedar invariants, and conditional branches are
rejected until a real Cedar predicate evaluator is wired. It records a
`TaskRuntimeState`, deterministically traverses supported sequence/parallel/retry
nodes, and records both success and timeout failure paths. Operator verification
overrides require a `task.override` allowlist entry and an existing
verification-failed step. It still does not dispatch external
tools/providers/webhooks; it provides the real DAG execution/state seam that
runtime workers can depend on while effectful dispatch is added in later phases.

## OpenClaw hook compatibility

`ardur-hooks-openclaw-compat` now exposes `OpenClawHookRegistryExt` for
registering OpenClaw-format hook configs directly into
`ardur_lifecycle_hooks::HookRegistry`. The compatibility layer translates
OpenClaw/codex events into Ardur canonical hook events, preserves source order as
hook priority, and adapts runner results into lifecycle decisions:

- `pre_tool_use` / permission-style events run as pre-submit hooks and can veto
  with a human-readable reason.
- post/finalize-style events run as post-receipt observational hooks.
- the default runner is safe no-op; tests and embeddings can inject explicit
  runners (for example a recording runner, or a future subprocess runner).

This keeps OpenClaw config parsing and ordering compatible without silently
executing shell commands just because a compatibility config exists.

## Streaming

Every provider exposes a streaming surface on the `Provider` trait
(`Provider::stream` → a `ProviderStream` of `StreamEvent`s), and each backend
advertises whether it implements it via `supports_streaming()`:

| Provider | `supports_streaming()` | Transport |
|---|---|---|
| `anthropic` | yes | SSE (§3.1b) |
| `ollama` | yes | NDJSON (§3.4b) |
| `openrouter` | yes | SSE (§3.2b) |
| `openai-compat` | yes | SSE |
| `codex` | no | CLI orchestrates its own output |
| `claude-cli` | no | planned |

This is currently a **provider-library** capability: the fused turn pipeline
consumes the non-streaming completion and posts the full reply once it is
ready, so there is no operator knob and you will not see token-by-token
"typing" land in Slack/Matrix/Discord/Telegram yet. The streaming path is in
place at the provider boundary so the turn loop can adopt it later without a
provider rewrite.

## Skills (SKILL.md)

A **skill** is a folder holding a `SKILL.md`: YAML frontmatter (`name`,
`description`, optional `metadata`) followed by a Markdown body of instructions
(§8.X). Point `ARDUR_SKILLS_DIRS` at one or more directories of skill folders and
each discovered skill registers as a tool — its `name` becomes the tool id, its
`description` the tool description, and invoking it returns the body:

```bash
ARDUR_SKILLS_DIRS=./examples/skills,/etc/ardur/skills
```

Each listed directory is a *collection* of `<name>/SKILL.md` skill folders:

```
examples/skills/
  git-commit-message/
    SKILL.md
    conventions.md        # referenced from the body as @./conventions.md
  code-review/
    SKILL.md
```

**Progressive disclosure.** A body may reference sibling files with `@./file.md`
markers. By default they pass through un-inlined (the model only pays for the
body); a caller inlines specific ones on demand with the tool's `expand`
argument, e.g. `{"expand": ["conventions.md"]}`.

**Validation.** `name` and `description` are required — a `SKILL.md` missing
either is skipped with a warning. Unknown frontmatter fields are ignored, so a
newer skill schema still loads. A skill whose `name` collides with an
already-registered tool is skipped (first registration wins). Nine example
skills ship under `examples/skills/`.

## HTTP endpoints

`ardur-server` exposes a small HTTP surface over the fused runtime:

| Method & path                   | Purpose                                                        |
| ------------------------------- | -------------------------------------------------------------- |
| `POST /slack/events`            | Slack Events-API webhook (HMAC-verified; replies to channel).  |
| `POST /chat`                    | Generic synchronous chat — run one turn, get the reply back.   |
| `POST /acp`                     | ACP JSON-RPC ingress; bearer-gated and receipt-chained through the fused runtime. |
| `GET  /healthz`                 | Liveness probe with build metadata.                            |
| `GET  /openapi.json`            | OpenAPI 3.0 document for the mounted HTTP surface.              |
| `GET  /openapi/clients/rust`    | Generated Rust client source.                                  |
| `GET  /openapi/clients/python`  | Generated Python client source.                                |
| `…/mcp` (optional)              | Bearer-gated MCP surface (see [MCP](#mcp-model-context-protocol)). |

### `POST /chat`

Run a single turn through the full pipeline (cap-token → cedar → cost-gate →
injection-defense → provider → receipt → journal → memory) and get the
consolidated result back in the **same** request — unlike `/slack/events`, the
reply is returned to the caller rather than posted to a channel. This is the
endpoint [`ardur-eval`](#companion-tools) targets and a generic surface for
embedding the agent.

Request body:

```jsonc
{
  "message": "What is the capital of France?",  // required, non-empty
  "session_id": "018f5e1a-...-000000000abc",     // optional UUID; minted if absent
  "stream": false                                  // optional; see note below
}
```

Response body (`200 OK`):

```json
{
  "session_id": "018f5e1a-0000-7000-8000-000000000abc",
  "reply": "The capital of France is Paris.",
  "tokens": { "input": 120, "output": 30 },
  "cost_usd": 0.42,
  "tools_called": ["echo"],
  "receipt_id": "5f8e1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b"
}
```

- `session_id` round-trips when provided (thread follow-up turns onto the same
  session); a fresh time-ordered UUID is minted when it is omitted.
- `tokens` / `cost_usd` are the turn's billed usage (`cost_usd` is the receipt's
  cents rendered as dollars).
- `tools_called` lists the tools the model invoked over the turn, in receipt
  order — empty when no tool ran.
- `receipt_id` is the id of the signed receipt minted for the turn, joinable
  against the receipt log (and the `ardur-admin` dashboard).

Example:

```sh
curl -sS http://localhost:3000/chat \
  -H "Authorization: Bearer ${ARDUR_CHAT_TOKEN}" \
  -H 'content-type: application/json' \
  -d '{"message":"hello ardur"}'
```

Status codes:

- `400` — malformed JSON body, a missing/empty `message`, or `stream: true`.
- `502` — the runtime rejected or failed the turn (cost-gate denied, injection
  blocked, provider error, …); the body carries `{"error": "<reason>"}`.
- `200` — success, with the body above.

**Streaming.** `stream: true` (an SSE `text/event-stream` response of
`Provider::stream` events) is **not yet implemented** — a `true` value is
rejected with `400` rather than silently answered with a consolidated body. It
is a planned P1.5 follow-up.

### OpenAPI and generated clients

Fetch the OpenAPI spec and generated client sources from a running server:

```sh
curl -sS http://localhost:3000/openapi.json | python3 -m json.tool
curl -sS http://localhost:3000/openapi/clients/rust -o ardur_client.rs
curl -sS http://localhost:3000/openapi/clients/python -o ardur_client.py
```

The Rust and Python client templates include `healthz()` and `chat(message)`
helpers. `chat` attaches `Authorization: Bearer <token>` when a token is
configured; `/healthz` is unauthenticated.

## Monitoring

- `GET /healthz` — returns `200 OK` once the runtime is initialized.
- Structured logs are emitted to stdout (JSON when `ARDUR_LOG_FORMAT=json`).
- Set `RUST_LOG=info,ardur=debug` for verbose ardur-internal tracing.

## Observability (OpenTelemetry GenAI)

Every provider call is wrapped in a `provider.send` tracing span carrying the
[OpenTelemetry GenAI semantic-convention](https://opentelemetry.io/docs/specs/semconv/gen-ai/)
attributes — `gen_ai.system`, `gen_ai.request.model`,
`gen_ai.usage.input_tokens` / `output_tokens`, `gen_ai.response.finish_reasons`,
`error.type`, and friends. Export them to any OTLP-native backend (Langfuse,
Arize Phoenix, Arize, Jaeger, Grafana Tempo, …) for token-usage dashboards,
latency tracing, and per-call drill-down — for free, across **every** provider
(anthropic / openrouter / openai-compat / ollama / codex / claude-cli).

Disabled by default. To enable, set:

```bash
ARDUR_OTEL_ENABLED=true
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317   # OTLP/gRPC (default)
OTEL_SERVICE_NAME=ardur-agent                       # optional; defaults to ardur-agent
```

`OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME` are the standard OTel
variables — point them at your collector and the spans flow there. When OTel is
disabled the same spans still emit to the console subscriber; only the OTLP
export is gated. Both `ardur-server` and the `ardur` CLI honor these variables
and flush buffered spans on graceful shutdown.

Point it at a backend in seconds:

- **Jaeger** — `docker run -p 4317:4317 -p 16686:16686 jaegertracing/all-in-one`,
  then browse <http://localhost:16686>.
- **Arize Phoenix** — `docker run -p 6006:6006 -p 4317:4317 arizephoenix/phoenix`,
  then browse <http://localhost:6006>.
- **Langfuse** — run an OTLP collector that forwards to Langfuse's OTLP ingest
  endpoint, or point `OTEL_EXPORTER_OTLP_ENDPOINT` at a Langfuse-compatible
  collector.

## Companion tools

Two standalone binaries ship alongside `ardur-server` for evaluation and
after-the-fact observability. Both are optional and decoupled from the server
process.

- **`ardur-admin`** (`crates/admin-ui`) — a **read-only** observability
  dashboard over a deployment's on-disk artifacts. It reads the server's
  journals, the hash-chained receipt log, and (optionally) the durable Qdrant
  memory collection *directly* and serves them over a small HTTP UI on its own
  port. No write path, no shared config — configured purely by CLI flags:

  ```sh
  ardur-admin \
    --journal-dir /var/lib/ardur/journals \
    --receipt-store /var/lib/ardur/receipts \
    --qdrant-url http://localhost:6334 \   # optional; enables the memory view
    --port 8090 \                          # default 8090
    --basic-auth user:pass                 # optional light gate
  ```

  It is strictly read-only, but it surfaces receipt and journal contents —
  treat its port like the data directory and keep it on a trusted/private
  network behind your own auth.

  Trust Center APIs are exposed under `/api/trust/*` on the same read-only
  server:

  ```sh
  # Capability Wallet: active verified grants, tool allowlists, expiry, budget,
  # and a read-only revoke-button affordance for operators.
  curl -sS http://localhost:8090/api/trust/wallet

  # Receipt Explorer: verify the on-disk parent-hash chain.
  curl -sS http://localhost:8090/api/trust/receipts/verify

  # Policy Debugger: explain Cedar allow/deny/indeterminate with matched policy ids.
  curl -sG http://localhost:8090/api/trust/policy/debug \
    --data-urlencode 'principal=User::"alice"' \
    --data-urlencode 'action=Action::"Submit"' \
    --data-urlencode 'resource=Session::"s1"'
  ```

  `ardur-admin` remains read-only: wallet revocation is shown as an operator
  affordance, while actual cap-token revocation still flows through the runtime
  deny-list API.

- **`ardur-eval`** (`crates/eval-harness`) — a CLI that POSTs scenario files
  to a server chat endpoint and grades the results, emitting `json`, `junit`,
  or `markdown`:

  ```sh
  ardur-eval run  --scenarios ./scenarios --server-url http://localhost:3000 --output markdown
  ardur-eval list --scenarios ./scenarios
  ardur-eval new  --id my-scenario --scenarios ./scenarios
  ```

  It targets the `POST <server-url>/chat` JSON contract that `ardur-server`
  now exposes (see [HTTP endpoints](#http-endpoints)). The path is overridable
  with `--chat-path` if you front the server with a different route.

## Closed learning loop

`ardur-automation::learning` implements the closed self-improvement loop. A
scheduled job supplies already-verified cap-token claims and a Cedar policy
bundle, reads the past N sessions for one workspace, normalizes/merges duplicate
patterns, and writes structured playbook proposals. The job requires:

- cap-token tool grant `learning.dream`;
- Cedar allow for action `Action::"LearningDream"` on the workspace resource;
- human approval before any proposal can transition from
  `PendingHumanApproval` to `Approved`.

The generated proposals are receipt-style hash chained (`parent_hash` points to
`SHA256(previous proposal)`), so a later approval/audit can prove ordering.
Useful checks while developing the loop:

```sh
cargo test -p ardur-automation learning
cargo test -p ardur-fused-runtime --test receipt_atomic_commit
cargo test -p ardur-fused-runtime --test memory_recall
```

## Cost ceilings

`ARDUR_COST_BUDGET_CENTS=10000` caps a single session at $100 of provider
spend. The cost-gate enforces this server-side and returns a structured
error to the channel before the next provider call when the ceiling is hit.

## Troubleshooting

**Events not arriving.**
- Verify the Slack app's Event Subscriptions URL ends in `/slack/events`.
- Check container logs for `403 InvalidSignature` — usually a stale
  `SLACK_SIGNING_SECRET` or a proxy that's rewriting the request body.
- Confirm the bot is invited to the channel (`/invite @ardur`).

**Responses not coming back.**
- Check `ANTHROPIC_API_KEY` is set and reachable from the container.
- Confirm the bot has `chat:write` on the channel.
- Check logs for `cost-gate: budget exhausted` — the session hit
  `ARDUR_COST_BUDGET_CENTS`.

**`/healthz` returns 503.**
- The data directory is unwritable. Check the volume mount and the
  `nonroot` user's permissions on the host-side volume.

## Known gaps

Open tickets that an operator should know about before depending on this
deployment for anything sensitive:

- **ARD-21** — Dependabot triage queue is unmanaged; pin reviews land
  ad-hoc.

EPIC-TRUST closes the ARD-17/ARD-19 trust-substrate gaps: receipt and journal
commit are atomic from the runtime's perspective, boot verifies persisted chain
integrity, orphan reconciliation remains available, and scoped hybrid memory
recall is wired into the turn path after cap-token and Cedar enforcement. This
is still **dev fidelity**, not **production**; use it in a private channel first.
