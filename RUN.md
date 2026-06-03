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
- An Anthropic API key with access to the model you intend to run.
- Docker + Docker Compose (local dev) or any Docker host (production).

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
    --env-file .env \
    ardur-server:latest
```

**Do NOT expose port 3000 directly to the public internet.** Always run
behind a TLS-terminating proxy — nginx, Caddy, Traefik, or a Cloudflare
Tunnel — so Slack's signed requests arrive over HTTPS and the signature
verification basestring includes a real `Host`. The container itself
listens on plain HTTP at `$ARDUR_BIND_ADDR` (default `0.0.0.0:3000`).

## Persistent state

`/var/lib/ardur/` is the data directory (configurable via `ARDUR_DATA_DIR`).
It contains:

| Path | Purpose |
|---|---|
| `memory/` | bi-temporal memory store (per-session + global) |
| `journals/` | append-only session journals (replay source of truth) |
| `receipts/` | signed receipt chain (JWS-ES256) |
| `keys/` | issuer keys — **`keys/issuer.pem` is the root of trust for the receipt chain. Back this up. Losing it invalidates every prior receipt.** |

Back the whole volume up regularly. The receipt chain is content-addressed
and append-only; a corrupted or missing journal entry breaks replay.

## Monitoring

- `GET /healthz` — returns `200 OK` once the runtime is initialized.
- Structured logs are emitted to stdout (JSON when `ARDUR_LOG_FORMAT=json`).
- Set `RUST_LOG=info,ardur=debug` for verbose ardur-internal tracing.

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

- **ARD-14** — Cedar derive from cap-token claims. **DONE** (landed in PR
  #42).
- **ARD-17** — Orphan-receipt durability: the two-phase commit between
  journal append and receipt sign is still single-phase. A crash in the
  window can leave an orphan receipt.
- **ARD-19** — Runtime ↔ memory wiring is still partial; some recall paths
  bypass the bi-temporal store.
- **ARD-48** — Injection-defense not yet wired into the FusedRuntime
  pipeline; the standalone crate exists but does not gate provider calls.
- **ARD-21** — Dependabot triage queue is unmanaged; pin reviews land
  ad-hoc.

Until ARD-17, ARD-19, and ARD-48 land, this is **dev fidelity**, not
**production**. Use it in a private Slack channel first.
