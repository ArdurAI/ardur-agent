# ardur-admin

An observability + operator-console dashboard for an `ardur-server`
deployment (§13.X). Every artifact access is read-only, with one narrow,
explicit exception: the Approvals surface proxies decisions to
`ardur-server`'s own admin-bearer-gated write API (see "Approvals" below).

`ardur-server` persists three things to its data directory that are useful to an
operator after the fact:

- **session journals** — `<data>/journals/sessions/<id>/journal.jsonl`, one
  serialized `JournalEntry` per line;
- a hash-chained **receipt log** — `<data>/receipts/chain.jsonl`, one compact
  JWS per line;
- when the durable memory backend is selected, a **Qdrant** collection.

`ardur-admin` reads those artifacts *directly* and serves them over a small HTTP
dashboard on its own port. It is deliberately decoupled from `ardur-server`: a
separate binary, a separate port, no shared boot config, and **no write path**.
Run it alongside `ardur-server` to inspect what's happening.

## Running

```sh
ardur-admin \
  --journal-dir   /var/lib/ardur/journals \
  --receipt-store /var/lib/ardur/receipts \
  --port 8090
```

If `ardur-server` is configured with data dir `/var/lib/ardur`, then
`--journal-dir` is `<data>/journals` and `--receipt-store` is `<data>/receipts`.
(`--receipt-store` may also point straight at a `chain.jsonl` file.)

### Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--journal-dir <path>` | *(required)* | Directory containing `sessions/<id>/journal.jsonl`. |
| `--receipt-store <path>` | *(required)* | Directory holding `chain.jsonl` (or the file itself). |
| `--qdrant-url <url>` | *(unset)* | Optional Qdrant gRPC URL; enables `/api/memory/recent`. |
| `--qdrant-collection <name>` | `ardur_memory` | Collection to scroll when `--qdrant-url` is set. |
| `--port <num>` | `8090` | Dashboard port (distinct from ardur-server's typical `8080`). |
| `--basic-auth <user:pass>` | *(unset)* | Optional HTTP Basic gate on every endpoint. |
| `--bearer-tokens <token[,token...]>` | *(unset)* | Optional Bearer gate on every endpoint — accepts `Authorization: Bearer <token>` matching any configured token. Preferred over `--basic-auth` for anything reachable beyond loopback; also settable via `ARDUR_ADMIN_BEARER_TOKENS` so the token need not appear in shell history. **Note:** a plain browser has no way to attach a bearer token to its first page load (unlike Basic, which browsers prompt for natively) — use `--basic-auth`, or a reverse proxy that injects the header, if the dashboard's HTML UI needs to work in a browser. |
| `--policy-bundle <path>` | *(unset)* | Path to the same Cedar `.cedar` policy file `ardur-server` enforces. Enables the Trust Center's policy debugger. Read-only — evaluates hypothetical queries, never enforces. |
| `--server-url <url>` | *(unset)* | Base URL of the `ardur-server` instance to proxy approval decisions to. Enables the Approvals surface when set together with `--server-admin-token`. |
| `--server-admin-token <token>` | *(unset)* | The admin bearer token `ardur-server` was started with, forwarded on every proxied approvals call. Also settable via `ARDUR_ADMIN_SERVER_TOKEN`. |

`--basic-auth` and `--bearer-tokens` may be configured together — a request
satisfying *either* is authorized. `--server-url` and `--server-admin-token`
must be set together (enforced at the flag-parsing level); enabling the
approvals proxy additionally requires `--basic-auth` or `--bearer-tokens` on
admin-ui itself — refused otherwise, since the proxy is write-capable. The
only other environment variable read is `ARDUR_ADMIN_BIND` (see `--bind-addr`
below); all other configuration is via flags.

## Endpoints

Every endpoint is `GET` except the three `/api/operator/approvals*` routes,
which are the dashboard's one deliberate write-capable exception (see
"Approvals" below).

| Method | Path | Returns |
| --- | --- | --- |
| `GET` | `/` | HTML dashboard (server-rendered, auto-refreshing via HTMX). |
| `GET` | `/healthz` | `200 ok` readiness check. |
| `GET` | `/api/sessions` | Session list: id, journal mtime, message/entry counts, last activity, last settled cost. |
| `GET` | `/api/sessions/{id}/journal` | Journal entries for a session (redacted — see "Security model"). Defaults to the last 100; `?limit=&offset=` paginate. With no `offset`, the page is the tail. |
| `GET` | `/api/receipts` | The most recent 50 receipts, summarized (cost, provider, tool-call summary). |
| `GET` | `/api/receipts/{id}` | One receipt in full: decoded body + compact JWS. `404` if unknown. |
| `GET` | `/api/costs` | Aggregate cost: total + today/7d/30d windows, by provider, by day, top-10 sessions. |
| `GET` | `/api/memory/recent` | Last 20 memory records (when `--qdrant-url` is set; otherwise `{"enabled": false}`). |
| `GET` | `/api/trust/wallet` | Active (non-expired) verified capability grants — the Trust Center capability wallet. |
| `GET` | `/api/trust/receipts/verify` | Re-derives and checks the receipt chain's parent-hash linkage; reports the first broken link, if any. |
| `GET` | `/api/trust/policy/debug?principal=&action=&resource=&attributes=` | Explains a hypothetical Cedar decision (Allow/Deny/Indeterminate + matched policy ids) against the loaded policy bundle. `503` if no bundle is configured. |
| `GET` | `/operator/approvals` | HTML fragment listing approval cards with Approve/Reject actions (the dashboard's Approvals section loads this). `503`-equivalent inline error if the proxy isn't configured or `ardur-server` is unreachable. |
| `GET` | `/api/operator/approvals` | The same list as raw JSON, proxied from `ardur-server`'s `/approvals`. `503` if the proxy isn't configured. |
| `POST` | `/api/operator/approvals/{id}/approve` | Proxies to `ardur-server`'s `/approvals/{id}/approve`. `400` malformed id, `404` unknown, `409` already decided, `502` if `ardur-server` is unreachable or rejects the configured token, `503` if the proxy isn't configured. |
| `POST` | `/api/operator/approvals/{id}/reject` | Proxies to `ardur-server`'s `/approvals/{id}/reject`, with an optional JSON body `{"reason": "..."}`. Same status codes as `approve`. |

## Approvals

The dashboard's one write-capable feature. `ardur-server` mounts its own
admin-bearer-gated decide API directly on its approvals store
(`GET /approvals`, `POST /approvals/{id}/approve`,
`POST /approvals/{id}/reject`) — `ardur-admin` never reads or writes that
store itself. Instead, when `--server-url` + `--server-admin-token` are
configured, it proxies decisions to `ardur-server`'s API over HTTP,
forwarding the admin token. `ardur-server` remains the sole writer of its own
state.

The dashboard's Approvals section loads its card list lazily
(`hx-trigger="load, approvalsChanged from:body"`) rather than on the same
5-second timer as the rest of the dashboard: fetching the list is a network
call to another process, and a slow or unreachable `ardur-server` on a fixed
poll would stall that one section repeatedly rather than failing once. An
Approve/Reject click fires the same event via the decide response's
`HX-Trigger: approvalsChanged` header, so the list refreshes right after a
decision without needing its own poll.

### The "provider" dimension

The persisted receipt body carries **no explicit provider field**. The closest
available grouping key is the receipt **verb** (`verb.object.state.vN`, e.g.
`llm.completion.minted.v1`), so `ardur-admin` surfaces the verb as the
"provider" dimension in the receipts feed and the cost-by-provider breakdown.

## Security model

- **Read-only against its own artifacts, with one explicit exception.** Every
  filesystem and Qdrant access is a read; the binary never opens a journal,
  receipt, or memory artifact for write. The one deliberate exception is the
  Approvals proxy: when configured, it forwards approve/reject decisions to
  `ardur-server`'s own admin-bearer-gated `/approvals` API — `ardur-admin`
  itself still never writes to any store. Receipt JWS payloads are decoded but
  **not** signature-verified (the admin-ui holds no keys).
- **No auth by default.** Intended for a trusted local or private network.
  `--bearer-tokens` adds a fail-closed, constant-time, length-bounded Bearer
  gate — the same check `ardur-server` uses for its own admin routes
  (`crates/server/src/routes.rs`'s `authorize_admin`) — and is the preferred
  mechanism for anything reachable beyond loopback. `--basic-auth user:pass`
  remains available as a lighter single-credential gate, and is currently the
  only one a plain browser can use to load the HTML dashboard (see the
  `--bearer-tokens` flag note above). Neither adds TLS termination or rate
  limiting; put the dashboard behind a reverse proxy / network ACL if it is
  reachable from anywhere untrusted. **Enabling the Approvals proxy
  (`--server-url`) requires one of these to be configured on admin-ui
  itself** — refused at startup otherwise, since the proxy is write-capable
  and there is no `--unsafe-bind`-style override for it.
- **Redacted message content.** `/api/sessions/{id}/journal` returns journal
  message text (`UserMessage`/`AssistantMessage` content, `Checkpoint`
  summaries, `Invalidation` reasons) with secret-shaped substrings —
  API keys, bearer tokens, AWS-style access keys, PEM private key blocks,
  `password=`/`secret:`-style natural-language leakage — replaced with
  `<REDACTED>` (`ardur-session-journals`' `redact` module, shared with
  `ardur cli`'s own `redact` command). This is pattern-based, not a content
  classifier: it catches secret-*shaped* text, not every sensitive thing a
  user might type. Treat the endpoint's output as still-sensitive
  conversational content.

## Implementation notes

- HTML is rendered with [`maud`](https://maud.lambda.xyz/) — compile-time HTML
  from a Rust macro, no template files and no runtime engine. Chosen over askama
  (needs a templates dir + build step) and tera (a runtime engine) as the
  lightest server-rendering option for a read-only dashboard.
- Receipt parsing reuses `ardur-receipt`'s `ReceiptBody`; journal parsing reuses
  `ardur-session-journals`' `JournalEntry`; optional memory reuses
  `ardur-memory-qdrant`'s record type — all read-only. None of those crates are
  modified.
