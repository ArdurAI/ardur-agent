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

No environment variables are read; configuration is entirely via these flags.

## Endpoints

All endpoints are `GET` — there are no write routes.

| Method | Path | Returns |
| --- | --- | --- |
| `GET` | `/` | HTML dashboard (server-rendered, auto-refreshing via HTMX). |
| `GET` | `/trust` | HTML **Trust Center**: receipt-chain integrity banner + explorer, cost ledger, capability wallet, policy-decision log, and injection-event feed (the last two need `--security-events`). |
| `GET` | `/healthz` | `200 ok` readiness check. |
| `GET` | `/api/sessions` | Session list: id, journal mtime, message/entry counts, last activity, last settled cost. |
| `GET` | `/api/sessions/{id}/journal` | Journal entries for a session. Defaults to the last 100; `?limit=&offset=` paginate. With no `offset`, the page is the tail. |
| `GET` | `/api/receipts` | The most recent 50 receipts, summarized (cost, provider, tool-call summary). |
| `GET` | `/api/receipts/{id}` | One receipt in full: decoded body + compact JWS. `404` if unknown. |
| `GET` | `/api/costs` | Aggregate cost: total + today/7d/30d windows, by provider, by day, top-10 sessions. |
| `GET` | `/api/memory/recent` | Last 20 memory records (when `--qdrant-url` is set; otherwise `{"enabled": false}`). |
| `GET` | `/api/trust/wallet` | Active (non-expired) capability grants from the configured cap-token claims. |
| `GET` | `/api/trust/chain` | Receipt-chain overview: total, per-link `parent_hash` validity, first broken index, newest 100 links. |
| `GET` | `/api/trust/events` | Redacted security-event view (policy denials + injection blocks): per-gate counts + newest 100 of each stream. `{"enabled": false}` when `--security-events` is unset. |
| `GET` | `/api/trust/receipts/verify` | Whole-chain hash-linkage verification result. |
| `GET` | `/api/trust/policy/debug` | Trace one Cedar decision: `?principal=&action=&resource=&attributes=` → allow/deny + matched policy ids (`503` when no bundle is configured). |

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
- **No auth by default.** Intended for a trusted local or private network. The
  optional `--basic-auth user:pass` adds a single-credential HTTP Basic gate —
  a light speed bump, **not** real authentication (no TLS termination, no user
  store, no rate limiting). Put it behind a reverse proxy / network ACL if it is
  reachable from anywhere untrusted.
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
