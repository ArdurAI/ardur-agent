# ardur-channel-matrix

The Matrix channel adapter for ardur — a second [`MessagingGateway`] backend
alongside the Slack adapter. Matrix is an open, federated, Rust-native protocol,
which makes it a natural fit for self-hosted ardur deployments.

Built on the official [`matrix-sdk`](https://github.com/matrix-org/matrix-rust-sdk)
(pinned to **0.18**), with the `e2e-encryption` and `bundled-sqlite` features.

## What Phase 1 does

- **Bot-style auth** via an access token (preferred over password login for
  bots), restored into a persistent **sqlite** state + crypto store.
- **Inbound**: a sync loop forwards each `m.room.message` text event through the
  gateway's `receive()`, gated by a room allowlist and with the bot's own
  messages dropped (echo prevention).
- **Outbound**: `send_message` posts plaintext via `room.send`, returning the
  homeserver event id as the receipt's `provider_message_id`.
- **Auto-join**: room invites addressed to the bot are accepted automatically
  when `auto_join_invites` is set (the default), subject to the allowlist.

Direct messages, threaded replies, and attachment upload are later phases.

## Configuration

Build a `MatrixConfig` with the builder or from the environment:

| Env var                    | Required | Default                | Meaning                                  |
| -------------------------- | -------- | ---------------------- | ---------------------------------------- |
| `MATRIX_HOMESERVER_URL`    | yes      | —                      | e.g. `https://matrix.org`                |
| `MATRIX_USER_ID`           | yes      | —                      | e.g. `@ardur-bot:matrix.org`             |
| `MATRIX_ACCESS_TOKEN`      | yes      | —                      | bot access token (held as a secret)      |
| `MATRIX_DEVICE_ID`         | no       | `ARDUR_BOT`            | stable device id (see E2EE below)        |
| `MATRIX_STATE_DIR`         | no       | `~/.ardur/matrix-state`| sqlite state + crypto store              |
| `MATRIX_AUTO_JOIN_INVITES` | no       | `true`                 | accept room invites automatically        |
| `MATRIX_ALLOWED_ROOMS`     | no       | _(all rooms)_          | comma-separated room-id allowlist        |

```rust
use ardur_channel_matrix::{MatrixChannel, MatrixConfig};

let config = MatrixConfig::from_env()?;
let channel = MatrixChannel::new(config).await?;
channel.start_sync(); // begin draining inbound traffic
```

## E2EE caveat

The `e2e-encryption` feature is **on**. For messages in encrypted rooms,
decryption "just works" *once the bot's device keys are present in the crypto
store* — but on a brand-new device the bot has no keys yet, so the first
encrypted messages it sees may be undecryptable until other devices share the
room keys.

For production:

1. Set a **stable `MATRIX_DEVICE_ID`** and a persistent `MATRIX_STATE_DIR` so the
   crypto store (and its keys) survive restarts. A fresh device id on every boot
   re-derives keys and loses decryptability.
2. On first run, **verify the bot's device** from a trusted session
   (Element → the bot user → verify session). Until verified, other clients may
   withhold keys and show "unable to decrypt" for the bot.
3. Keep the `MATRIX_STATE_DIR` volume durable and private (it holds the crypto
   store; treat it like a secret).

If E2EE is not needed for your deployment, the adapter still operates in
unencrypted rooms with no extra setup.

## Tests

- Unit tests (`cargo test -p ardur-channel-matrix`) cover config parsing and the
  allowlist — no network.
- The live integration round-trip (`tests/integration.rs`) is **skipped unless
  `MATRIX_INTEGRATION_TEST=1`**; see that file's header for the env it needs and
  a Conduit-based setup.

[`MessagingGateway`]: https://docs.rs/ardur-messaging-gateway
