# ardur-channel-discord

The Discord channel adapter for ardur — a [`MessagingGateway`] backend alongside
the Slack, Matrix, and Telegram adapters.

Built on [`serenity`](https://github.com/serenity-rs/serenity) (pinned to
**0.12**) with its default `rustls` TLS backend.

## What Phase 1 does

- **Bot-token auth** via a gateway token, restored into a serenity client.
- **Inbound**: the gateway `message` event handler forwards each text message
  through the gateway's `receive()`, gated by a channel allowlist and with the
  bot's own messages dropped (echo prevention).
- **Outbound**: `send_message` posts plaintext via `ChannelId::say`, returning
  the Discord message id as the receipt's `provider_message_id`.

Direct messages, slash commands, threads, and attachment upload are later phases.

## Configuration

Build a `DiscordConfig` with the builder or from the environment:

| Env var                     | Required | Default          | Meaning                                       |
| --------------------------- | -------- | ---------------- | --------------------------------------------- |
| `DISCORD_BOT_TOKEN`         | yes      | —                | the bot's gateway token (held as a secret)    |
| `DISCORD_APPLICATION_ID`    | yes      | —                | the bot's application id (== its user id)     |
| `DISCORD_ALLOWED_CHANNELS`  | no       | _(all channels)_ | comma-separated channel-id allowlist          |

```rust
use ardur_channel_discord::{DiscordChannel, DiscordConfig};

let config = DiscordConfig::from_env()?;
let channel = DiscordChannel::new(config).await?;
channel.start().await; // connect the gateway and begin draining inbound traffic
```

## Privileged intent

The adapter subscribes to `GUILD_MESSAGES`, `DIRECT_MESSAGES`, and the
**privileged** `MESSAGE_CONTENT` intent. The latter must also be enabled for the
bot in the Discord developer portal (Bot → Privileged Gateway Intents → Message
Content Intent), or inbound message `content` arrives **empty** and the adapter
forwards nothing.

## Why no `poise`

The §4.Y brief named `serenity` + `poise`. `poise` is an application-command
(slash-command) framework layered on serenity; a plain message-forwarding bot
uses serenity's `EventHandler` directly — the same way the Matrix adapter wraps
`matrix-sdk` directly. Phase 1 omits `poise`; slash commands are a later phase.

## Tests

- Unit tests (`cargo test -p ardur-channel-discord`) cover config parsing and the
  allowlist — no network.
- The live send (`tests/integration.rs`) is **skipped unless
  `DISCORD_INTEGRATION_TEST=1`**; see that file's header for the env it needs.

[`MessagingGateway`]: https://docs.rs/ardur-messaging-gateway
