# ardur-admin

A standalone, **read-only** observability dashboard for an `ardur-server`
deployment (§13.X).

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
| `--bearer-tokens <token[,token...]>` | *(unset)* | Optional Bearer gate on every endpoint — accepts `Authorization: Bearer <token>` matching any configured token. Preferred over `--basic-auth` for anything reachable beyond loopback; also settable via `ARDUR_ADMIN_BEARER_TOKENS` so the token need not appear in shell history. |

`--basic-auth` and `--bearer-tokens` may be configured together — a request
satisfying *either* is authorized. The only other environment variable read is
`ARDUR_ADMIN_BIND` (see `--bind-addr` below); all other configuration is via
flags.

## Endpoints

All endpoints are `GET` — there are no write routes.

| Method | Path | Returns |
| --- | --- | --- |
| `GET` | `/` | HTML dashboard (server-rendered, auto-refreshing via HTMX). |
| `GET` | `/healthz` | `200 ok` readiness check. |
| `GET` | `/api/sessions` | Session list: id, journal mtime, message/entry counts, last activity, last settled cost. |
| `GET` | `/api/sessions/{id}/journal` | Journal entries for a session. Defaults to the last 100; `?limit=&offset=` paginate. With no `offset`, the page is the tail. |
| `GET` | `/api/receipts` | The most recent 50 receipts, summarized (cost, provider, tool-call summary). |
| `GET` | `/api/receipts/{id}` | One receipt in full: decoded body + compact JWS. `404` if unknown. |
| `GET` | `/api/costs` | Aggregate cost: total + today/7d/30d windows, by provider, by day, top-10 sessions. |
| `GET` | `/api/memory/recent` | Last 20 memory records (when `--qdrant-url` is set; otherwise `{"enabled": false}`). |
| `GET` | `/api/trust/wallet` | Active (non-expired) verified capability grants — the Trust Center capability wallet. |
| `GET` | `/api/trust/receipts/verify` | Re-derives and checks the receipt chain's parent-hash linkage; reports the first broken link, if any. |
| `GET` | `/api/trust/policy/debug?principal=&action=&resource=&attributes=` | Explains a hypothetical Cedar decision (Allow/Deny/Indeterminate + matched policy ids) against the loaded policy bundle. `503` if no bundle is configured. |

### The "provider" dimension

The persisted receipt body carries **no explicit provider field**. The closest
available grouping key is the receipt **verb** (`verb.object.state.vN`, e.g.
`llm.completion.minted.v1`), so `ardur-admin` surfaces the verb as the
"provider" dimension in the receipts feed and the cost-by-provider breakdown.

## Security model

- **Read-only.** Every filesystem and Qdrant access is a read. There is no
  route that writes, signs, or mutates a journal, receipt, or memory record, and
  the binary never opens any artifact for write. Receipt JWS payloads are
  decoded but **not** signature-verified (the admin-ui holds no keys).
- **No auth by default.** Intended for a trusted local or private network.
  `--bearer-tokens` adds a fail-closed, constant-time, length-bounded Bearer
  gate — the same check `ardur-server` uses for its own admin routes
  (`crates/server/src/routes.rs`'s `authorize_admin`) — and is the preferred
  mechanism for anything reachable beyond loopback. `--basic-auth user:pass`
  remains available as a lighter single-credential gate. Neither adds TLS
  termination or rate limiting; put the dashboard behind a reverse proxy /
  network ACL if it is reachable from anywhere untrusted.
- **No secrets exposed.** The dashboard surfaces costs, message counts, tool
  names, and receipt metadata. Journal *message contents* are returned by
  `/api/sessions/{id}/journal`; treat the endpoint accordingly.

## Implementation notes

- HTML is rendered with [`maud`](https://maud.lambda.xyz/) — compile-time HTML
  from a Rust macro, no template files and no runtime engine. Chosen over askama
  (needs a templates dir + build step) and tera (a runtime engine) as the
  lightest server-rendering option for a read-only dashboard.
- Receipt parsing reuses `ardur-receipt`'s `ReceiptBody`; journal parsing reuses
  `ardur-session-journals`' `JournalEntry`; optional memory reuses
  `ardur-memory-qdrant`'s record type — all read-only. None of those crates are
  modified.
