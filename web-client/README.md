# Ardur web client PWA

A thin static PWA for ARD-460's first non-terminal consumer surface.

## Scope

- Streams `POST /chat` with `{ stream: true }` and renders only fused
  `type: "content"` SSE frames into the transcript. Tool-call deltas, stage
  events, receipts, and finish frames are ignored (they are not assistant
  prose). In-band `{ "type": "error" }` events surface as chat errors.
- Keeps bearer tokens in browser memory only; they are not written to local
  storage.
- Registers `sw.js` for installability and push-notification approval hooks.
- Supports approval deep links via `?approval_id=<id>` and calls:
  - `POST /approvals/<id>/approve` (admin bearer)
  - `POST /approvals/<id>/reject` (admin bearer)

Those decide endpoints are mounted on `ardur-server` behind
`ARDUR_ADMIN_BEARER_TOKENS`. The optional admin-token field falls back to the
chat token when left empty.

## Cross-origin (required for this static server)

The PWA is served from a different origin than `ardur-server` (default
`http://127.0.0.1:4173` vs `http://127.0.0.1:3000`). Browsers will not send
`Authorization` across origins unless the server reflects the PWA origin.

Set an exact allowlist on the server (wildcard `*` is refused at config load):

```sh
export ARDUR_CORS_ORIGINS='http://127.0.0.1:4173'
```

Empty `ARDUR_CORS_ORIGINS` (the default) emits no CORS headers.

## Local smoke

```sh
# Terminal 1 — HTTP-only server with PWA CORS + chat bearer.
# `ARDUR_PROVIDER=ollama` is the no-Anthropic-key path; it talks to a live
# Ollama daemon (default http://127.0.0.1:11434) and the configured model
# must exist. Without that daemon, `/chat` returns a provider error rather
# than a streamed reply.
ARDUR_PROVIDER=ollama \
ARDUR_DEV_PERMISSIVE_POLICY=true \
ARDUR_CHAT_BEARER_TOKENS='dev-chat-token' \
ARDUR_ADMIN_BEARER_TOKENS='dev-admin-token' \
ARDUR_CORS_ORIGINS='http://127.0.0.1:4173' \
cargo run -p ardur-server --bin ardur-server

# Terminal 2 — static PWA
cd web-client && python3 -m http.server 4173
```

Open <http://127.0.0.1:4173/>, paste the chat bearer, and send a message. The
parser suite is `node --test web-client/sse.test.js`.
