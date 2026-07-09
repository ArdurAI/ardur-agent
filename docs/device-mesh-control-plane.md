# Device mesh and companion app control plane

`ardur nodes` is the local device-mesh control-plane prototype for paired desktop/mobile/browser companions.

State lives in `~/.ardur/device-mesh.json` and tracks:

- device identity id
- platform
- scoped capability grants
- trust tier
- pairing/approval/revocation timestamps
- last-seen heartbeat
- pairing token expiry
- mesh sessions
- route receipts
- emergency stop state

Basic flow:

```sh
ardur nodes pair macbook-pro --platform macos --cap tool.browser.open --trust-tier personal
ardur nodes approve macbook-pro
ardur nodes heartbeat macbook-pro
ardur nodes route-tool macbook-pro browser.open --capability tool.browser.open --receipt /tmp/route.json
ardur nodes status
ardur nodes revoke macbook-pro
```

Fail-closed routing rules:

- emergency stop blocks every route
- unapproved devices cannot receive routes
- revoked devices cannot receive routes
- expired pairing tokens are denied
- missing capabilities are denied
- stale devices are denied unless `--offline-ok` is explicitly set, in which case an `offline-fallback` receipt is written

Emergency stop:

```sh
ardur nodes emergency-stop --enable
ardur nodes emergency-stop --disable
```

The prototype intentionally keeps companion network ingress out of scope. A future mobile/desktop companion can consume this persisted model and receipts without widening authority before policy, transport, and app-store hardening are ready.
