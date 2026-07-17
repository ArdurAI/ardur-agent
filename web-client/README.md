# Ardur web client PWA

A thin static PWA for ARD-460's first non-terminal consumer surface.

## Scope

- Streams `POST /chat` requests with `{ stream: true }` and parses SSE `data:` events.
- Keeps the chat bearer token in browser memory only; it is not written to local storage.
- Registers `sw.js` for installability and push-notification approval hooks.
- Supports approval deep links via `?approval_id=<id>` and calls:
  - `POST /approvals/<id>/approve`
  - `POST /approvals/<id>/reject`

Those approval endpoints are hooks for the approval-gate epic. If the server has
not mounted them yet, the UI reports the HTTP failure without hiding it.

## Local smoke

Serve from this directory with any static server, for example:

```sh
python3 -m http.server 4173
```

Then open <http://127.0.0.1:4173/> and point the server URL at an Ardur server
that exposes `/chat`.
