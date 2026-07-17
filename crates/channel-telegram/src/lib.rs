//! ardur-channel-telegram — a Telegram backend behind the §4.0
//! [`MessagingGateway`] contract (alongside [`ardur-slack-adapter`],
//! [`ardur-channel-matrix`], and [`ardur-channel-discord`]).
//!
//! Plan family: §4.Y (Discord + Telegram channel adapters Phase 1).
//!
//! This crate wraps the [`teloxide`] Telegram Bot-API client.
//!
//! # Phase 1 (this crate)
//!
//! - [`TelegramChannel`] — implements [`MessagingGateway`]. Construct with
//!   [`TelegramChannel::new`] (builds the bot and validates the token via
//!   `get_me`), then call [`start`](TelegramChannel::start) once to begin the
//!   long-poll dispatcher.
//! - **Outbound**: [`MessagingGateway::send_message`] posts plaintext via
//!   `send_message` and returns a [`MessageReceipt`] whose `provider_message_id`
//!   is the Telegram message id. [`TelegramChannel::send_text`] is the native
//!   send that preserves the full [`TelegramError`] taxonomy.
//! - **Inbound**: a repl-style single-endpoint dispatcher forwards each text
//!   message onto an internal queue, gating on the chat allowlist and dropping
//!   the bot's own messages (echo prevention).
//!   [`MessagingGateway::receive`] pops the next one.
//! - [`TelegramConfig`] — built via [`TelegramConfig::builder`] or
//!   [`TelegramConfig::from_env`] (`TELEGRAM_BOT_TOKEN`,
//!   `TELEGRAM_ALLOWED_CHATS`).
//! - [`TelegramError`] — the typed failure surface;
//!   [`into_gateway_error`](TelegramError::into_gateway_error) lowers it onto
//!   [`GatewayError`] at the trait boundary.
//!
//! # Adapt-points vs. the §4.Y task brief
//!
//! - The brief named `teloxide` 0.13; the current release is **0.17**, which
//!   this crate pins (the same kind of version bump the Matrix adapter took from
//!   the brief's guessed `matrix-sdk` 0.8 up to 0.18). In 0.13+ `Message::from`
//!   became a public field (the old `.from()` method is deprecated), which this
//!   adapter uses.
//! - teloxide's default features pull `native-tls` (system OpenSSL); this crate
//!   drops defaults and selects `rustls` to keep the TLS stack pure-Rust, and
//!   omits `ctrlc_handler` so the embedded dispatcher does not install a SIGINT
//!   handler that would race the server's own graceful shutdown.
//!
//! [`ardur-slack-adapter`]: https://docs.rs/ardur-slack-adapter
//! [`ardur-channel-matrix`]: https://docs.rs/ardur-channel-matrix
//! [`ardur-channel-discord`]: https://docs.rs/ardur-channel-discord
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessageReceipt`]: ardur_messaging_gateway::MessageReceipt
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message
//! [`MessagingGateway::receive`]: ardur_messaging_gateway::MessagingGateway::receive
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod channel;
mod config;
mod error;

pub use channel::TelegramChannel;
pub use config::{TelegramConfig, TelegramConfigBuilder, parse_allowed_chats};
pub use error::TelegramError;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use config::{ENV_ALLOWED_CHATS, ENV_BOT_TOKEN};

    /// A fake variable source backing `TelegramConfig::from_source` — so the
    /// `from_env` logic is unit-tested without mutating process-global env (which
    /// is `unsafe` under edition 2024, and this crate is `#![forbid(unsafe_code)]`).
    struct FakeEnv(HashMap<&'static str, &'static str>);

    impl FakeEnv {
        /// Start from the one required Telegram credential.
        fn required() -> Self {
            Self(HashMap::from([(ENV_BOT_TOKEN, "123456:ABC-DEF_token")]))
        }

        fn with(mut self, key: &'static str, value: &'static str) -> Self {
            self.0.insert(key, value);
            self
        }

        fn resolve(&self) -> Result<TelegramConfig, TelegramError> {
            TelegramConfig::from_source(|k| self.0.get(k).map(|v| (*v).to_owned()))
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let config = FakeEnv::required().resolve().expect("required var set");
        assert!(
            config.allowed_chat_ids.is_empty(),
            "an unset allowlist means all chats"
        );
        // The allowlist gate is open when empty.
        assert!(config.chat_allowed(-100_123));
    }

    #[test]
    fn config_from_env_reads_allowlist() {
        let config = FakeEnv::required()
            .with(ENV_ALLOWED_CHATS, "-1001234567890, 42 ,-99")
            .resolve()
            .expect("required var set");
        assert_eq!(config.allowed_chat_ids, vec![-1_001_234_567_890, 42, -99]);
        assert!(config.chat_allowed(42));
        assert!(config.chat_allowed(-1_001_234_567_890));
        assert!(!config.chat_allowed(7));
    }

    #[test]
    fn config_from_env_requires_token() {
        let err =
            TelegramConfig::from_source(|_| None).expect_err("missing required var must error");
        assert!(
            matches!(&err, TelegramError::MissingEnvVar(v) if v == ENV_BOT_TOKEN),
            "the missing required var is the bot token, got: {err}"
        );
    }

    #[test]
    fn allowlist_parsing_handles_signed_ids() {
        // Comma-split with whitespace trimmed and blanks dropped; ids may be
        // negative (groups/supergroups).
        let parsed = parse_allowed_chats(Some(" -1 ,2,  ,-3 ")).expect("signed ids parse");
        assert_eq!(parsed, vec![-1, 2, -3]);
        // None (and an all-blank value) parse to an empty list = "all chats".
        assert!(parse_allowed_chats(None).expect("none is empty").is_empty());
        assert!(
            parse_allowed_chats(Some("  , ,"))
                .expect("blanks are empty")
                .is_empty()
        );
        // A non-numeric entry is a named error.
        let err = parse_allowed_chats(Some("1,xyz")).expect_err("xyz is not an i64");
        assert!(matches!(err, TelegramError::InvalidChatId(v) if v == "xyz"));
    }

    #[test]
    fn builder_requires_non_empty_token() {
        let err = TelegramConfig::builder("")
            .build()
            .expect_err("empty token is rejected");
        assert!(matches!(err, TelegramError::MissingField(f) if f == "bot_token"));

        let config = TelegramConfig::builder("123:abc")
            .allowed_chat_ids(vec![-5])
            .build()
            .expect("token present");
        assert!(config.chat_allowed(-5));
        assert!(!config.chat_allowed(-6));
    }
}
