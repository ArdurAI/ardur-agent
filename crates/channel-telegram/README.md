# ardur-channel-telegram

The Telegram channel adapter for ardur — a [`MessagingGateway`] backend
alongside the Slack, Matrix, and Discord adapters.

Built on [`teloxide`](https://github.com/teloxide/teloxide) (pinned to **0.17**)
with the `rustls` TLS backend (default `native-tls` dropped).

## What Phase 1 does

- **Bot-token auth** via a Bot-API token; `new` validates it with a `get_me`
  call (which also yields the bot's own user id for echo prevention).
- **Inbound**: a repl-style long-poll dispatcher forwards each text message
  through the gateway's `receive()`, gated by a chat allowlist and with the
  bot's own messages dropped (echo prevention).
- **Outbound**: `send_message` posts plaintext via the Bot API, returning the
  Telegram message id as the receipt's `provider_message_id`.

Inline keyboards, media, forum-topic threads, and webhooks are later phases.

## Configuration

Build a `TelegramConfig` with the builder or from the environment:

| Env var                  | Required | Default       | Meaning                                          |
| ------------------------ | -------- | ------------- | ------------------------------------------------ |
| `TELEGRAM_BOT_TOKEN`     | yes      | —             | the bot's `<id>:<secret>` token (held as secret) |
| `TELEGRAM_ALLOWED_CHATS` | no       | _(all chats)_ | comma-separated chat-id allowlist (signed `i64`) |

```rust
use ardur_channel_telegram::{TelegramChannel, TelegramConfig};

let config = TelegramConfig::from_env()?;
let channel = TelegramChannel::new(config).await?;
channel.start(); // begin the long-poll dispatcher
```

Telegram chat ids are **signed**: negative for groups/supergroups, positive for
private chats. Use [@userinfobot](https://t.me/userinfobot) or the Bot API's
`getUpdates` to discover a chat id.

## A note on long-polling

`start` runs one long-poll loop; only **one** process may poll a given bot token
at a time (Telegram returns a `409 Conflict` otherwise). `start` is idempotent —
a second call is a no-op. The dispatcher does **not** install a Ctrl-C handler
(the `ctrlc_handler` feature is off) so it does not race the embedding server's
graceful shutdown.

## Tests

- Unit tests (`cargo test -p ardur-channel-telegram`) cover config parsing and
  the allowlist — no network.
- The live send (`tests/integration.rs`) is **skipped unless
  `TELEGRAM_INTEGRATION_TEST=1`**; see that file's header for the env it needs.

[`MessagingGateway`]: https://docs.rs/ardur-messaging-gateway
