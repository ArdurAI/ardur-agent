//! ardur-channel-discord — a Discord backend behind the §4.0
//! [`MessagingGateway`] contract (alongside [`ardur-slack-adapter`] and
//! [`ardur-channel-matrix`]).
//!
//! Plan family: §4.Y (Discord + Telegram channel adapters Phase 1).
//!
//! This crate wraps the [`serenity`] Discord gateway + HTTP client.
//!
//! # Phase 1 (this crate)
//!
//! - [`DiscordChannel`] — implements [`MessagingGateway`]. Construct with
//!   [`DiscordChannel::new`] (builds the serenity client), then call
//!   [`start`](DiscordChannel::start) once to connect the gateway and begin
//!   draining inbound traffic.
//! - **Outbound**: [`MessagingGateway::send_message`] posts plaintext via
//!   `ChannelId::say` and returns a [`MessageReceipt`] whose
//!   `provider_message_id` is the Discord message id.
//!   [`DiscordChannel::send_text`] is the native send that preserves the full
//!   [`DiscordError`] taxonomy.
//! - **Inbound**: the gateway `message` event handler forwards each text message
//!   onto an internal queue, gating on the channel allowlist and dropping the
//!   bot's own messages (echo prevention). [`MessagingGateway::receive`] pops the
//!   next one.
//! - [`DiscordConfig`] — built via [`DiscordConfig::builder`] or
//!   [`DiscordConfig::from_env`] (`DISCORD_BOT_TOKEN`, `DISCORD_APPLICATION_ID`,
//!   `DISCORD_ALLOWED_CHANNELS`).
//! - [`DiscordError`] — the typed failure surface;
//!   [`into_gateway_error`](DiscordError::into_gateway_error) lowers it onto
//!   [`GatewayError`] at the trait boundary.
//!
//! # Echo prevention
//!
//! A Discord bot's user id equals its application id, so the adapter drops any
//! inbound message authored by [`DiscordConfig::application_id`] — no extra
//! round-trip to discover the bot's own id is needed.
//!
//! # Adapt-points vs. the §4.Y task brief
//!
//! - The brief named `serenity` 0.12 **plus `poise` 0.6**. `poise` is an
//!   application-command (slash-command) framework layered on serenity; a plain
//!   message-forwarding bot uses serenity's [`EventHandler`] directly — exactly
//!   as the Matrix adapter wraps `matrix-sdk` directly rather than a higher-level
//!   bot framework. Phase 1 therefore omits `poise`; it would be dead weight.
//!   Slash commands (the thing `poise` buys) are a later phase.
//! - The privileged `MESSAGE_CONTENT` intent (in [`DEFAULT_INTENTS`]) must also
//!   be toggled on for the bot in the Discord developer portal, or inbound
//!   `content` arrives empty. See this crate's README.
//!
//! [`ardur-slack-adapter`]: https://docs.rs/ardur-slack-adapter
//! [`ardur-channel-matrix`]: https://docs.rs/ardur-channel-matrix
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessageReceipt`]: ardur_messaging_gateway::MessageReceipt
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message
//! [`MessagingGateway::receive`]: ardur_messaging_gateway::MessagingGateway::receive
//! [`EventHandler`]: serenity::all::EventHandler
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod channel;
mod config;
mod error;

pub use channel::DiscordChannel;
pub use config::{DEFAULT_INTENTS, DiscordConfig, DiscordConfigBuilder, parse_allowed_channels};
pub use error::DiscordError;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use config::{ENV_ALLOWED_CHANNELS, ENV_APPLICATION_ID, ENV_BOT_TOKEN};

    /// A fake variable source backing `DiscordConfig::from_source` — so the
    /// `from_env` logic is unit-tested without mutating process-global env (which
    /// is `unsafe` under edition 2024, and this crate is `#![forbid(unsafe_code)]`).
    struct FakeEnv(HashMap<&'static str, &'static str>);

    impl FakeEnv {
        /// Start from the two required Discord credentials.
        fn required() -> Self {
            Self(HashMap::from([
                (ENV_BOT_TOKEN, "MTaBcDeF.bot.token"),
                (ENV_APPLICATION_ID, "123456789012345678"),
            ]))
        }

        fn with(mut self, key: &'static str, value: &'static str) -> Self {
            self.0.insert(key, value);
            self
        }

        fn resolve(&self) -> Result<DiscordConfig, DiscordError> {
            DiscordConfig::from_source(|k| self.0.get(k).map(|v| (*v).to_owned()))
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let config = FakeEnv::required().resolve().expect("required vars set");
        assert_eq!(config.application_id, 123_456_789_012_345_678);
        assert!(
            config.allowed_channel_ids.is_empty(),
            "an unset allowlist means all channels"
        );
        assert_eq!(
            config.intents, DEFAULT_INTENTS,
            "intents default to the guild+dm+content set"
        );
        // The allowlist gate is open when empty.
        assert!(config.channel_allowed(42));
    }

    #[test]
    fn config_from_env_reads_allowlist() {
        let config = FakeEnv::required()
            .with(ENV_ALLOWED_CHANNELS, "111, 222 ,333")
            .resolve()
            .expect("required vars set");
        assert_eq!(config.allowed_channel_ids, vec![111, 222, 333]);
        assert!(config.channel_allowed(222));
        assert!(!config.channel_allowed(999));
    }

    #[test]
    fn config_from_env_requires_token_first() {
        let err =
            DiscordConfig::from_source(|_| None).expect_err("missing required vars must error");
        assert!(
            matches!(&err, DiscordError::MissingEnvVar(v) if v == ENV_BOT_TOKEN),
            "the first missing required var is the bot token, got: {err}"
        );
    }

    #[test]
    fn config_from_env_rejects_non_numeric_application_id() {
        let err = FakeEnv::required()
            .with(ENV_APPLICATION_ID, "not-a-number")
            .resolve()
            .expect_err("a non-numeric application id is rejected");
        assert!(matches!(err, DiscordError::InvalidApplicationId(v) if v == "not-a-number"));
    }

    #[test]
    fn allowlist_parsing_rejects_non_numeric() {
        // Comma-split with whitespace trimmed and blanks dropped.
        let parsed = parse_allowed_channels(Some(" 1 ,2,  ,3 ")).expect("numeric ids parse");
        assert_eq!(parsed, vec![1, 2, 3]);
        // None (and an all-blank value) parse to an empty list = "all channels".
        assert!(
            parse_allowed_channels(None)
                .expect("none is empty")
                .is_empty()
        );
        assert!(
            parse_allowed_channels(Some("  , ,"))
                .expect("blanks are empty")
                .is_empty()
        );
        // A non-numeric entry is a named error.
        let err = parse_allowed_channels(Some("12,abc")).expect_err("abc is not a u64");
        assert!(matches!(err, DiscordError::InvalidChannelId(v) if v == "abc"));
    }

    #[test]
    fn builder_requires_non_empty_fields() {
        // The builder rejects an empty required field with a named error.
        let err = DiscordConfig::builder("", 123)
            .build()
            .expect_err("empty token is rejected");
        assert!(matches!(err, DiscordError::MissingField(f) if f == "bot_token"));

        let err = DiscordConfig::builder("token", 0)
            .build()
            .expect_err("zero application id is rejected");
        assert!(matches!(err, DiscordError::MissingField(f) if f == "application_id"));

        // A fully-specified builder produces the configured values.
        let config = DiscordConfig::builder("token", 999)
            .allowed_channel_ids(vec![7])
            .build()
            .expect("all required fields present");
        assert_eq!(config.application_id, 999);
        assert!(config.channel_allowed(7));
        assert!(!config.channel_allowed(8));
    }
}
